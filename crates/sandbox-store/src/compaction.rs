//! Atomic removal of expired request/response bodies, preserving retry and
//! reconciliation evidence. This does not delete receipts or reclaim VM journals.
use crate::{
    Store,
    output::{self, OutputError},
    output_cleanup,
};
use sandbox_protocol::{
    DIGEST_VERSION, Id, OperationId, RequestDigest, SandboxId, command::CommandInput,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Row, postgres::PgRow};
use time::OffsetDateTime;

/// Stored only by the compactor after validating the original command and
/// applicable final receipts. Never accepted from an API or supervisor caller.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CommandSummary {
    version: u8,
    pub(crate) digest: [u8; 32],
    pub(crate) deadline_unix_ms: i64,
    pub(crate) output_limit: u64,
}
impl CommandSummary {
    fn from_input(input: &CommandInput, id: OperationId) -> Result<Self, OutputError> {
        input.validate().map_err(|_| OutputError::Corrupt)?;
        Ok(Self {
            version: 1,
            digest: input
                .for_operation(id)
                .digest()
                .map_err(|_| OutputError::Corrupt)?,
            deadline_unix_ms: input.deadline_unix_ms,
            output_limit: input.output_limit,
        })
    }
}

pub(crate) fn command_summary(row: &PgRow) -> Result<CommandSummary, OutputError> {
    // Upgrade fixtures and old rows read before migration have no summary;
    // they must still validate the complete original payload as before.
    let compacted = match row.try_get::<Option<OffsetDateTime>, _>("payload_compacted_at") {
        Err(sqlx::Error::ColumnNotFound(_)) => None,
        other => other?,
    };
    if compacted.is_some() {
        let value: Value = row
            .try_get::<Option<Value>, _>("command_summary")?
            .ok_or(OutputError::Corrupt)?;
        let summary: CommandSummary =
            serde_json::from_value(value).map_err(|_| OutputError::Corrupt)?;
        if summary.version != 1
            || summary.deadline_unix_ms <= 0
            || summary.output_limit > sandbox_protocol::command::MAX_OUTPUT
            || row.try_get::<Value, _>("payload")? != json!({})
        {
            return Err(OutputError::Corrupt);
        }
        Ok(summary)
    } else {
        let input: CommandInput =
            serde_json::from_value(row.try_get("payload")?).map_err(|_| OutputError::Corrupt)?;
        CommandSummary::from_input(&input, OperationId::from_uuid(row.try_get("id")?))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compaction {
    Idle,
    Completed(OperationId),
    /// Invalid stored evidence is preserved and deferred for operator review.
    Deferred(OperationId),
}

fn validate_request(row: &PgRow) -> Result<(), OutputError> {
    if row.try_get::<i32, _>("digest_version")? != DIGEST_VERSION {
        return Err(OutputError::Corrupt);
    }
    let sandbox = SandboxId::from_uuid(row.try_get("sandbox_id")?);
    let payload: Value = row.try_get("payload")?;
    let kind: String = row.try_get("kind")?;
    let initiator: String = row.try_get("initiator_kind")?;
    let (method, target, body) = match (kind.as_str(), initiator.as_str()) {
        ("create", "project" | "admin") => ("POST", "/v1/sandboxes".into(), payload),
        ("execute", "project" | "admin") => {
            ("POST", format!("/v1/sandboxes/{sandbox}/execute"), payload)
        }
        ("cancel", "project") => (
            "POST",
            format!(
                "/v1/operations/{}/cancel",
                OperationId::from_uuid(row.try_get("target_operation_id")?)
            ),
            payload,
        ),
        ("destroy", "project" | "admin") => (
            "POST",
            format!("/v1/sandboxes/{sandbox}/destroy"),
            json!({"correlation_id":payload.get("correlation_id").ok_or(OutputError::Corrupt)?}),
        ),
        ("destroy", "service") => ("SERVICE", "/allocation-cleanup".into(), payload),
        _ => return Err(OutputError::Corrupt),
    };
    let digest =
        RequestDigest::compute(method, &target, &body).map_err(|_| OutputError::Corrupt)?;
    if row.try_get::<Vec<u8>, _>("request_digest")?.as_slice() != digest.as_bytes() {
        return Err(OutputError::Corrupt);
    }
    Ok(())
}

impl Store {
    /// Compact one eligible operation while holding its row lock. All effects
    /// are in PostgreSQL; cancellation rolls back and a lost commit can be
    /// reconciled by the persisted timestamp. Corrupt rows cannot starve peers.
    pub async fn compact_expired_response(&self) -> Result<Compaction, OutputError> {
        let mut tx = self.pool().begin().await?;
        let row=sqlx::query("SELECT o.* FROM operations o
            WHERE o.payload_compacted_at IS NULL AND o.response_expires_at<=clock_timestamp()
            AND o.status IN ('succeeded','failed','cancelled') AND o.kind IN ('create','execute','destroy','cancel')
            AND o.completed_at IS NOT NULL AND o.lease_expires_at IS NULL AND o.next_retry_at IS NULL
            AND (o.payload_compaction_next_at IS NULL OR o.payload_compaction_next_at<=clock_timestamp())
            AND (o.output_status IN ('none','pending') OR
                (o.output_status='expired' AND EXISTS(SELECT 1 FROM output_cleanup c WHERE c.operation_id=o.id AND c.completed_at IS NOT NULL)))
            AND NOT EXISTS(SELECT 1 FROM sandboxes s WHERE s.active_transition_operation_id=o.id)
            ORDER BY o.response_expires_at,o.id FOR UPDATE OF o SKIP LOCKED LIMIT 1")
            .fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            return Ok(Compaction::Idle);
        };
        let id = OperationId::from_uuid(row.try_get("id")?);
        let output_status: String = row.try_get("output_status")?;
        let validation: Result<Option<CommandSummary>, OutputError> = async {
            validate_request(&row)?;
            let summary = if row.try_get::<String, _>("kind")? == "execute" {
                Some(command_summary(&row)?)
            } else {
                None
            };
            if output_status == "pending" {
                output::evidence(&mut tx, &row).await?;
            }
            if output_status == "expired" {
                output_cleanup::validate_completed(&mut tx, &row).await?;
            }
            Ok(summary)
        }
        .await;
        let summary = match validation {
            Ok(summary) => summary,
            Err(OutputError::Corrupt | OutputError::BadEvidence) => {
                sqlx::query("UPDATE operations SET payload_compaction_next_at=clock_timestamp()+interval '1 hour' WHERE id=$1")
                    .bind(id.uuid()).execute(&mut *tx).await?;
                tx.commit().await?;
                return Ok(Compaction::Deferred(id));
            }
            Err(e) => return Err(e),
        };
        let n=sqlx::query("UPDATE operations SET payload='{}',result=NULL,error=NULL,command_summary=$2,
            payload_compacted_at=clock_timestamp(),payload_compaction_next_at=NULL,
            output_status=CASE WHEN output_status='pending' THEN 'expired' ELSE output_status END,
            output_expires_at=CASE WHEN output_status='pending' THEN response_expires_at ELSE output_expires_at END,
            output_claim_revision=output_claim_revision+CASE WHEN output_status='pending' THEN 1 ELSE 0 END,
            output_lease_expires_at=NULL,output_next_retry_at=NULL
            WHERE id=$1 AND payload_compacted_at IS NULL AND response_expires_at<=clock_timestamp()
            AND status IN ('succeeded','failed','cancelled') AND lease_expires_at IS NULL AND next_retry_at IS NULL")
            .bind(id.uuid()).bind(summary.map(|v|json!(v))).execute(&mut *tx).await?.rows_affected();
        if n != 1 {
            return Err(OutputError::LostClaim);
        }
        tx.commit().await?;
        Ok(Compaction::Completed(id))
    }
}
