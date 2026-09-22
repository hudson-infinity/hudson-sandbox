use super::*;
use sandbox_protocol::{guest_model::Context, supervisor::Ownership, supervisor_files::FileRecord};
use serde_json::Value;
use sqlx::postgres::PgRow;
use time::OffsetDateTime;

pub(super) async fn eligible(
    db: &mut PgConnection,
    row: &PgRow,
    a: &PgRow,
    domain: Domain,
    allow_simulated: bool,
) -> Result<Option<Option<Context>>, Error> {
    if row.try_get::<String, _>("kind")?
        != match domain {
            Domain::Commands => "execute",
            Domain::Files => "file_write",
        }
    {
        return Err(Error::Evidence);
    }
    let status: String = row.try_get("status")?;
    if !matches!(status.as_str(), "succeeded" | "failed" | "cancelled")
        || row
            .try_get::<Option<OffsetDateTime>, _>("completed_at")?
            .is_none()
        || row
            .try_get::<Option<OffsetDateTime>, _>("lease_expires_at")?
            .is_some()
        || row
            .try_get::<Option<OffsetDateTime>, _>("next_retry_at")?
            .is_some()
    {
        return Ok(None);
    }
    let phase: Option<String> = row.try_get("phase")?;
    let attempts: i32 = row.try_get("attempt_count")?;
    match domain {
        Domain::Commands => {
            let summary = crate::compaction::command_summary(row).map_err(|_| Error::Evidence)?;
            let receipts: Value = row.try_get("attempt_receipts")?;
            let output: String = row.try_get("output_status")?;
            if attempts == 0 {
                if !matches!(
                    (status.as_str(), phase.as_deref()),
                    ("failed", Some("rejected_before_dispatch"))
                        | ("cancelled", Some("cancelled_before_dispatch"))
                ) {
                    return Err(Error::Evidence);
                }
                if !matches!(
                    phase.as_deref(),
                    Some("rejected_before_dispatch" | "cancelled_before_dispatch")
                ) || receipts != serde_json::json!([])
                    || output != "none"
                {
                    return Err(Error::Evidence);
                }
                return Ok(Some(None));
            }
            let owner = Ownership {
                host_id: HostId::from_uuid(a.try_get("host_id")?).to_string(),
                project_id: sandbox_protocol::ProjectId::from_uuid(a.try_get("project_id")?)
                    .to_string(),
                sandbox_id: sandbox_protocol::SandboxId::from_uuid(a.try_get("sandbox_id")?)
                    .to_string(),
                allocation_id: AllocationId::from_uuid(a.try_get("id")?).to_string(),
                operation_id: OperationId::from_uuid(row.try_get("id")?).to_string(),
                generation: a.try_get("generation")?,
                supervisor_epoch: a.try_get("supervisor_epoch")?,
                claim_revision: 0,
                claim_expires_unix_ms: 0,
            };
            crate::execute::validate_intent(row, &owner, &summary.digest)
                .map_err(|_| Error::Evidence)?;
            if matches!(
                phase.as_deref(),
                Some("not_started" | "cancelled_before_start")
            ) {
                if !matches!(
                    (status.as_str(), phase.as_deref()),
                    ("failed", Some("not_started")) | ("cancelled", Some("cancelled_before_start"))
                ) {
                    return Err(Error::Evidence);
                }
                let items = receipts
                    .as_array()
                    .filter(|v| v.len() == 2)
                    .ok_or(Error::Evidence)?;
                let observed = &items[1];
                if observed["phase"] != "command_fenced_before_start"
                    || observed["not_started"] != true
                    || observed.get("guest_receipt").is_some()
                    || observed["command_digest"] != hex::encode(summary.digest)
                    || output != "none"
                    || [
                        "host_id",
                        "project_id",
                        "sandbox_id",
                        "allocation_id",
                        "operation_id",
                        "generation",
                        "supervisor_epoch",
                    ]
                    .iter()
                    .any(|key| observed.get(*key) != items[0].get(*key))
                {
                    return Err(Error::Evidence);
                }
                if observed["simulated"].as_bool().ok_or(Error::Evidence)? && !allow_simulated {
                    return Err(Error::Evidence);
                }
                return Ok(Some(None));
            }
            let e = crate::output::evidence(db, row)
                .await
                .map_err(output_error)?;
            if e.simulated && !allow_simulated {
                return Err(Error::Evidence);
            }
            if output != "expired" {
                return Ok(None);
            }
            if row.try_get::<Option<Value>, _>("output_ticket")?.is_some() {
                // No operation lock is acquired. Completed cleanup is immutable.
                crate::output_cleanup::validate_completed(db, row)
                    .await
                    .map_err(output_error)?;
            } else if row
                .try_get::<Option<OffsetDateTime>, _>("payload_compacted_at")?
                .is_none()
            {
                return Err(Error::Evidence);
            }
            Ok(Some(Some(e.receipt.context)))
        }
        Domain::Files => {
            let file = sqlx::query("SELECT * FROM file_uploads WHERE operation_id=$1")
                .bind(row.try_get::<uuid::Uuid, _>("id")?)
                .fetch_one(&mut *db)
                .await?;
            if file
                .try_get::<Option<OffsetDateTime>, _>("source_retired_at")?
                .is_none()
            {
                return Ok(None);
            }
            let plan = crate::uploads::cleanup::validate_retired(db, row, &file)
                .await
                .map_err(|e| match e {
                    crate::dispatch::DispatchError::Query(e) => Error::Query(e),
                    _ => Error::Evidence,
                })?;
            let begun: bool = file.try_get("begin_requested")?;
            if !begun {
                if status != "failed"
                    || attempts != 0
                    || phase.as_deref() != Some("file_not_started")
                    || file.try_get::<Option<Value>, _>("record")?.is_some()
                {
                    return Err(Error::Evidence);
                }
                return Ok(Some(None));
            }
            let record: FileRecord =
                serde_json::from_value(file.try_get("record")?).map_err(|_| Error::Evidence)?;
            record.validate().map_err(|_| Error::Evidence)?;
            if record.digest != plan.upload.digest().map_err(|_| Error::Evidence)?
                || (!record.not_started && record.size != plan.upload.size)
            {
                return Err(Error::Evidence);
            }
            let raw = row
                .try_get::<Option<Value>, _>(if status == "succeeded" {
                    "result"
                } else {
                    "error"
                })?
                .ok_or(Error::Evidence)?;
            if raw["simulated"].as_bool().ok_or(Error::Evidence)? && !allow_simulated {
                return Err(Error::Evidence);
            }
            if (record.not_started && status != "failed")
                || (!record.not_started
                    && ((record.state == 3 && status != "succeeded")
                        || (record.state == 5 && status != "failed")))
            {
                return Err(Error::Evidence);
            }
            let expected = if record.not_started {
                Some("file_not_started")
            } else {
                match record.state {
                    3 => Some("file_committed"),
                    5 => Some("file_aborted"),
                    _ => None,
                }
            };
            if expected.is_none() || phase.as_deref() != expected {
                return Err(Error::Evidence);
            }
            if let Some(context) = &record.context
                && (context.allocation_id != plan.owner.scope.allocation_id
                    || context.generation != plan.owner.scope.generation)
            {
                return Err(Error::Evidence);
            }
            Ok(Some(record.context))
        }
    }
}

fn output_error(e: crate::output::OutputError) -> Error {
    match e {
        crate::output::OutputError::Query(e) => Error::Query(e),
        _ => Error::Evidence,
    }
}
