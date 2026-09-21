//! Offline operator provisioning. A credential file is durable before this insert is attempted.
use crate::{Store, StoreError};
use sandbox_protocol::{Id, ProjectId, TokenHash, TokenKeyId};
use time::OffsetDateTime;

/// All values are retained by the operator so a lost commit acknowledgement can be reconciled.
#[derive(Debug)]
pub struct ProjectProvision {
    pub project: ProjectId,
    pub name: String,
    pub key: TokenKeyId,
    pub hash: TokenHash,
    pub created_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
}
impl Store {
    /// Insert once, or confirm an exactly matching active project. Never rotate, unsuspend,
    /// overwrite quotas, or resurrect a revoked credential as a side effect of retrying.
    pub async fn provision_project(&self, request: &ProjectProvision) -> Result<bool, StoreError> {
        if request.name.trim().is_empty()
            || request.name.len() > 200
            || request.name.chars().any(char::is_control)
            || request.expires_at <= request.created_at
        {
            return Err(StoreError::Corrupt("invalid provisioning metadata".into()));
        }
        let mut tx = self.pool().begin().await.map_err(StoreError::Query)?;
        let limits =
            serde_json::json!({"sandboxes":25,"vcpu":100,"memory_mib":204800,"disk_mib":1638400});
        // Normalize timestamps to UTC so retries from another DB session timezone compare exactly.
        let tokens: serde_json::Value = sqlx::query_scalar("SELECT jsonb_build_array(jsonb_build_object('key_id',$1::text,'hash',$2::text,'created_at',to_char($3::timestamptz AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'),'expires_at',to_char($4::timestamptz AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')))")
            .bind(request.key.as_str()).bind(hex::encode(request.hash.as_bytes())).bind(request.created_at).bind(request.expires_at)
            .fetch_one(&mut *tx).await.map_err(StoreError::Query)?;
        sqlx::query("INSERT INTO projects(id,name,status,limits,api_tokens) VALUES($1,$2,'active',$3,$4) ON CONFLICT(id) DO NOTHING")
            .bind(request.project.uuid()).bind(&request.name).bind(&limits).bind(&tokens)
            .execute(&mut *tx).await.map_err(StoreError::Query)?;
        let matches: bool = sqlx::query_scalar("SELECT name=$2 AND status='active' AND limits=$3 AND api_tokens=$4 FROM projects WHERE id=$1 FOR UPDATE")
            .bind(request.project.uuid()).bind(&request.name).bind(limits).bind(tokens)
            .fetch_one(&mut *tx).await.map_err(StoreError::Query)?;
        tx.commit().await.map_err(StoreError::Query)?;
        Ok(matches)
    }
}
