//! Project rows and their API token metadata.

use sandbox_protocol::{Id, ProjectId, TokenHash, TokenKeyId};
use sqlx::Row as _;
use time::OffsetDateTime;

use crate::{Store, StoreError};

/// A project's lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectStatus {
    /// Ordinary operation.
    Active,
    /// Access blocked; the project and its data still exist.
    Suspended,
    /// Being torn down. No new work is admitted.
    Deleting,
}

impl ProjectStatus {
    fn from_column(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "suspended" => Some(Self::Suspended),
            "deleting" => Some(Self::Deleting),
            _ => None,
        }
    }
}

/// One token's stored metadata. Never the secret — only its hash.
#[derive(Debug, Clone)]
pub struct TokenRecord {
    /// The project the token belongs to.
    pub project_id: ProjectId,
    /// That project's current status, read in the same query so a suspended
    /// project cannot be missed by checking the token alone.
    pub project_status: ProjectStatus,
    /// Which credential this is.
    pub key_id: TokenKeyId,
    /// SHA-256 of the secret.
    pub hash: TokenHash,
    /// When the token stops being valid, if ever.
    pub expires_at: Option<OffsetDateTime>,
    /// When the token was revoked, if it was.
    pub revoked_at: Option<OffsetDateTime>,
}

impl Store {
    /// Find the token metadata for a key identifier.
    ///
    /// Returns `Ok(None)` when no such key exists. The caller must still
    /// verify the secret: this lookup proves nothing on its own, and the key
    /// identifier is not a credential.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Query`] if the query fails, and
    /// [`StoreError::Corrupt`] if a stored row cannot be interpreted — a
    /// hash of the wrong length or an unknown status is a bug or tampering,
    /// never something to treat as "no match".
    pub async fn token_by_key_id(
        &self,
        key_id: &TokenKeyId,
    ) -> Result<Option<TokenRecord>, StoreError> {
        // api_tokens is a bounded array on the project, so the token is found
        // by expanding it. At most two active tokens per project keeps this
        // small; if that ever changes, this becomes its own table.
        let row = sqlx::query(
            r"
            SELECT p.id            AS project_id,
                   p.status        AS project_status,
                   t.value->>'key_id'      AS key_id,
                   decode(t.value->>'hash', 'hex') AS hash,
                   (t.value->>'expires_at')::timestamptz AS expires_at,
                   (t.value->>'revoked_at')::timestamptz AS revoked_at
              FROM projects p
              CROSS JOIN LATERAL jsonb_array_elements(p.api_tokens) AS t(value)
             WHERE t.value->>'key_id' = $1
            ",
        )
        .bind(key_id.as_str())
        .fetch_optional(self.pool())
        .await
        .map_err(StoreError::Query)?;

        let Some(row) = row else { return Ok(None) };

        token_record(&row).map(Some)
    }
}

pub(crate) fn token_record(row: &sqlx::postgres::PgRow) -> Result<TokenRecord, StoreError> {
    let status: String = row.try_get("project_status").map_err(StoreError::Query)?;
    let project_status = ProjectStatus::from_column(&status)
        .ok_or_else(|| StoreError::Corrupt(format!("unknown project status {status:?}")))?;

    let hash: Vec<u8> = row.try_get("hash").map_err(StoreError::Query)?;
    let hash: [u8; 32] = hash
        .try_into()
        .map_err(|_| StoreError::Corrupt("stored token hash is not 32 bytes".to_owned()))?;

    let stored_key_id: String = row.try_get("key_id").map_err(StoreError::Query)?;

    Ok(TokenRecord {
        project_id: ProjectId::from_uuid(row.try_get("project_id").map_err(StoreError::Query)?),
        project_status,
        key_id: TokenKeyId::from_stored(stored_key_id),
        hash: TokenHash::from_bytes(hash),
        expires_at: row.try_get("expires_at").map_err(StoreError::Query)?,
        revoked_at: row.try_get("revoked_at").map_err(StoreError::Query)?,
    })
}
