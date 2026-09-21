//! Transactional admission.
//!
//! Admitting a request means inserting its operation and the resource changes
//! it implies in one transaction, so a client that receives an operation id
//! knows the work is durable. The controller picks it up independently of the
//! HTTP connection.
//!
//! Deduplication is `UNIQUE (project_id, idempotency_key)` in the schema, not
//! a check-then-insert in this code. Two concurrent identical requests race,
//! one loses on the constraint, and the loser reads the winner's row — which
//! is the only arrangement that is correct without a lock.

use sandbox_protocol::images::ImageAllowlist;
use sandbox_protocol::{
    DIGEST_VERSION, Id, IdempotencyKey, OperationId, ProjectId, RequestDigest, SandboxId,
    TokenKeyId,
};
use sqlx::Row as _;
use uuid::Uuid;

use crate::{Store, StoreError};

/// Requested sandbox size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resources {
    /// Virtual CPUs.
    pub vcpu: i32,
    /// Memory in MiB.
    pub memory_mib: i64,
    /// Writable disk in MiB.
    pub disk_mib: i64,
}

/// A validated create request, ready to admit.
#[derive(Debug, Clone)]
pub struct CreateSandbox {
    /// The owning project, resolved from the credential.
    pub project_id: ProjectId,
    /// Which credential admitted it. Recorded for audit.
    pub key_id: TokenKeyId,
    /// The caller's key for this logical mutation.
    pub idempotency_key: IdempotencyKey,
    /// Digest of the normalized request.
    pub request_digest: RequestDigest,
    /// Requested immutable image. Membership is checked after retry resolution.
    pub image_digest: String,
    /// Optional display name. Never a lookup key.
    pub name: Option<String>,
    /// Requested size.
    pub resources: Resources,
    /// The validated payload, stored for reconciliation and audit.
    pub payload: serde_json::Value,
}

/// What admission decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// New work is not permitted by the current operator image policy.
    ImageDenied,
    /// A new operation was inserted.
    Admitted {
        /// The sandbox this created.
        sandbox_id: SandboxId,
        /// The operation to poll.
        operation_id: OperationId,
    },
    /// This key was already used for an identical request.
    ///
    /// Returned instead of running anything again, even if the sandbox has
    /// since moved on.
    Existing {
        /// The sandbox the original request created.
        sandbox_id: SandboxId,
        /// The original operation.
        operation_id: OperationId,
        /// Its status now.
        status: String,
    },
    /// This key was used for a different request.
    ///
    /// A `409`. The key is not admitted, and nothing runs.
    DigestConflict {
        /// The operation the key already belongs to.
        operation_id: OperationId,
    },
}

/// PostgreSQL's unique-violation code.
const UNIQUE_VIOLATION: &str = "23505";

impl Store {
    /// Admit a create request, or resolve it against an earlier one.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Query`] if the transaction fails for any reason
    /// other than losing the deduplication race, which is handled here.
    pub async fn admit_create_sandbox(
        &self,
        request: &CreateSandbox,
        images: &ImageAllowlist,
    ) -> Result<Admission, StoreError> {
        // A concurrent identical request may win the unique constraint while
        // this one is mid-transaction. That is not an error: re-read and
        // return what the winner admitted.
        match self.try_admit_create_sandbox(request, images).await {
            Err(StoreError::Query(error)) if is_unique_violation(&error) => self
                .resolve_existing(request)
                .await?
                .ok_or_else(|| StoreError::Corrupt("duplicate key with no row".to_owned())),
            other => other,
        }
    }

    async fn try_admit_create_sandbox(
        &self,
        request: &CreateSandbox,
        images: &ImageAllowlist,
    ) -> Result<Admission, StoreError> {
        let mut tx = self.pool().begin().await.map_err(StoreError::Query)?;

        // Look first, so an ordinary retry costs one query rather than a
        // failed insert and a rollback.
        if let Some(existing) = existing_operation(&mut tx, request).await? {
            tx.commit().await.map_err(StoreError::Query)?;
            return Ok(existing);
        }

        // Policy changes cannot erase a caller's existing retry handle. New
        // work must pass this check before any resource or operation is written.
        if !images.allows(&request.image_digest) {
            return Ok(Admission::ImageDenied);
        }

        let sandbox_id = SandboxId::generate();
        let operation_id = OperationId::generate();

        sqlx::query(
            r"
            INSERT INTO sandboxes
                (id, project_id, name, image_digest, resources, desired_state, observed_state)
            VALUES ($1, $2, $3, $4, $5, 'running', 'creating')
            ",
        )
        .bind(sandbox_id.uuid())
        .bind(request.project_id.uuid())
        .bind(request.name.as_deref())
        .bind(&request.image_digest)
        .bind(serde_json::json!({
            "vcpu": request.resources.vcpu,
            "memory_mib": request.resources.memory_mib,
            "disk_mib": request.resources.disk_mib,
        }))
        .execute(&mut *tx)
        .await
        .map_err(StoreError::Query)?;

        sqlx::query(
            r"
            INSERT INTO operations
                (id, project_id, sandbox_id, kind, initiator_kind, initiator_key_id,
                 idempotency_key, request_digest, digest_version, payload, status)
            VALUES ($1, $2, $3, 'create', 'project', $4, $5, $6, $7, $8, 'queued')
            ",
        )
        .bind(operation_id.uuid())
        .bind(request.project_id.uuid())
        .bind(sandbox_id.uuid())
        .bind(request.key_id.as_str())
        .bind(request.idempotency_key.as_str())
        .bind(request.request_digest.as_bytes().as_slice())
        .bind(DIGEST_VERSION)
        .bind(&request.payload)
        .execute(&mut *tx)
        .await
        .map_err(StoreError::Query)?;

        // The sandbox points at the transition that owns it, so a second
        // lifecycle request can be rejected rather than interleaved.
        sqlx::query("UPDATE sandboxes SET active_transition_operation_id = $1 WHERE id = $2")
            .bind(operation_id.uuid())
            .bind(sandbox_id.uuid())
            .execute(&mut *tx)
            .await
            .map_err(StoreError::Query)?;

        tx.commit().await.map_err(StoreError::Query)?;

        Ok(Admission::Admitted {
            sandbox_id,
            operation_id,
        })
    }

    async fn resolve_existing(
        &self,
        request: &CreateSandbox,
    ) -> Result<Option<Admission>, StoreError> {
        let mut tx = self.pool().begin().await.map_err(StoreError::Query)?;
        let found = existing_operation(&mut tx, request).await?;
        tx.commit().await.map_err(StoreError::Query)?;
        Ok(found)
    }
}

/// Resolve a key that may already exist, comparing digests.
async fn existing_operation(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    request: &CreateSandbox,
) -> Result<Option<Admission>, StoreError> {
    let row = sqlx::query(
        r"
        SELECT id, sandbox_id, status, request_digest
          FROM operations
         WHERE project_id = $1 AND idempotency_key = $2
        ",
    )
    .bind(request.project_id.uuid())
    .bind(request.idempotency_key.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(StoreError::Query)?;

    let Some(row) = row else { return Ok(None) };

    let operation_id =
        OperationId::from_uuid(row.try_get::<Uuid, _>("id").map_err(StoreError::Query)?);
    let stored: Vec<u8> = row.try_get("request_digest").map_err(StoreError::Query)?;
    let stored: [u8; 32] = stored
        .try_into()
        .map_err(|_| StoreError::Corrupt("stored request digest is not 32 bytes".to_owned()))?;

    // Content is compared before today's lifecycle state, so an identical
    // retry resolves the same way whatever has happened since.
    if RequestDigest::from_bytes(stored) != request.request_digest {
        return Ok(Some(Admission::DigestConflict { operation_id }));
    }

    Ok(Some(Admission::Existing {
        sandbox_id: SandboxId::from_uuid(
            row.try_get::<Uuid, _>("sandbox_id")
                .map_err(StoreError::Query)?,
        ),
        operation_id,
        status: row.try_get("status").map_err(StoreError::Query)?,
    }))
}

fn is_unique_violation(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::code)
        .is_some_and(|code| code == UNIQUE_VIOLATION)
}
