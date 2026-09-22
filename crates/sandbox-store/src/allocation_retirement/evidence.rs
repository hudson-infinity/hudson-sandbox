use super::*;

pub(super) async fn release(
    db: &mut PgConnection,
    a: &PgRow,
    p: &Permit,
    allow_simulated: bool,
    epoch: i64,
) -> Result<(Value, bool), Error> {
    if a.try_get::<String, _>("status")? != "released"
        || a.try_get::<Option<OffsetDateTime>, _>("released_at")?
            .is_none()
    {
        return Err(Error::Ineligible);
    }
    let release: Value = a.try_get("release_evidence")?;
    let operation: OperationId = release["operation_id"]
        .as_str()
        .ok_or(Error::Evidence)?
        .parse()
        .map_err(|_| Error::Evidence)?;
    let row=sqlx::query("SELECT * FROM operations WHERE id=$1 AND project_id=$2 AND sandbox_id=$3 AND kind IN ('create','destroy')")
        .bind(operation.uuid()).bind(p.project.uuid()).bind(p.sandbox.uuid()).fetch_optional(&mut *db).await?.ok_or(Error::Evidence)?;
    let create_valid: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM operations WHERE id=$1 AND project_id=$2 AND sandbox_id=$3 AND kind='create' AND status IN ('succeeded','failed','cancelled') AND completed_at IS NOT NULL AND lease_expires_at IS NULL AND next_retry_at IS NULL)")
        .bind(p.create_operation.uuid()).bind(p.project.uuid()).bind(p.sandbox.uuid()).fetch_one(&mut *db).await?;
    if !create_valid
        || (row.try_get::<String, _>("kind")? == "create" && operation != p.create_operation)
        || !matches!(
            row.try_get::<String, _>("status")?.as_str(),
            "succeeded" | "failed" | "cancelled"
        )
        || row
            .try_get::<Option<OffsetDateTime>, _>("completed_at")?
            .is_none()
        || row
            .try_get::<Option<OffsetDateTime>, _>("lease_expires_at")?
            .is_some()
        || row
            .try_get::<Option<OffsetDateTime>, _>("next_retry_at")?
            .is_some()
        || !row
            .try_get::<Value, _>("attempt_receipts")?
            .as_array()
            .ok_or(Error::Evidence)?
            .contains(&release)
    {
        return Err(Error::Evidence);
    }
    let simulated = match release["phase"].as_str() {
        Some("rejected_before_dispatch") => {
            if operation != p.create_operation
                || release["dispatch_intent_absent"] != true
                || row.try_get::<i32, _>("attempt_count")? != 0
                || row.try_get::<String, _>("status")? != "failed"
                || row.try_get::<Option<String>, _>("phase")?.as_deref()
                    != Some("rejected_before_dispatch")
            {
                return Err(Error::Evidence);
            }
            false
        }
        Some("allocation_released") => {
            for (key, expected) in [
                ("host_id", p.host.to_string()),
                ("project_id", p.project.to_string()),
                ("sandbox_id", p.sandbox.to_string()),
                ("allocation_id", p.allocation.to_string()),
            ] {
                if release[key].as_str() != Some(expected.as_str()) {
                    return Err(Error::Evidence);
                }
            }
            if row.try_get::<i32, _>("attempt_count")? <= 0
                || release
                    .get("reporting_epoch")
                    .is_some_and(|v| v.as_i64().is_none_or(|v| v < p.original_epoch || v > epoch))
                || release["claim_revision"]
                    .as_i64()
                    .is_some_and(|v| v > row.get::<i64, _>("claim_revision"))
                || release["generation"].as_i64() != Some(p.generation)
                || release["supervisor_epoch"].as_i64() != Some(p.original_epoch)
                || release["observed_unix_ms"].as_i64().is_none_or(|v| v <= 0)
                || release["claim_revision"].as_i64().is_none_or(|v| v <= 0)
            {
                return Err(Error::Evidence);
            }
            release["simulated"].as_bool().ok_or(Error::Evidence)?
        }
        _ => return Err(Error::Evidence),
    };
    if simulated && !allow_simulated {
        return Err(Error::Evidence);
    }
    Ok((release, simulated))
}

pub(super) async fn consumers(db: &mut PgConnection, p: &Permit) -> Result<(), Error> {
    // Unfinished lifecycle work is conservatively scoped to the sandbox because
    // the original schema does not pin create/destroy rows to allocations.
    let pending:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM allocations WHERE id=$1 AND maintenance_lease_until>clock_timestamp()) OR EXISTS(SELECT 1 FROM operations o WHERE (o.execution_allocation_id=$1 OR o.file_allocation_id=$1 OR (o.sandbox_id=$2 AND o.kind IN ('create','destroy')) OR (o.kind='cancel' AND EXISTS(SELECT 1 FROM operations target WHERE target.id=o.target_operation_id AND target.execution_allocation_id=$1))) AND (o.status NOT IN ('succeeded','failed','cancelled') OR o.completed_at IS NULL OR o.lease_expires_at IS NOT NULL OR o.next_retry_at IS NOT NULL OR o.output_lease_expires_at IS NOT NULL OR o.output_next_retry_at IS NOT NULL)) OR EXISTS(SELECT 1 FROM sandboxes WHERE id=$2 AND current_allocation_id=$1) OR EXISTS(SELECT 1 FROM allocation_history WHERE allocation_id=$1 AND lease_expires_at>clock_timestamp()) OR EXISTS(SELECT 1 FROM released_allocation_history WHERE allocation_id=$1 AND lease_expires_at>clock_timestamp())")
        .bind(p.allocation.uuid()).bind(p.sandbox.uuid()).fetch_one(db).await?;
    if pending {
        return Err(Error::Ineligible);
    }
    Ok(())
}

pub(super) async fn domain(
    db: &mut PgConnection,
    a: &PgRow,
    domain: Domain,
    allow_simulated: bool,
) -> Result<(DomainClosure, bool), Error> {
    let allocation: uuid::Uuid = a.try_get("id")?;
    let (name, query) = match domain {
        Domain::Commands => (
            "commands",
            "SELECT * FROM operations WHERE execution_allocation_id=$1 AND ($2::uuid IS NULL OR id>$2) ORDER BY id LIMIT 32",
        ),
        Domain::Files => (
            "files",
            "SELECT * FROM operations WHERE file_allocation_id=$1 AND ($2::uuid IS NULL OR id>$2) ORDER BY id LIMIT 32",
        ),
    };
    // Keyset batches bound memory without excluding long-lived allocations.
    let mut last = None;
    loop {
        let rows = sqlx::query(query)
            .bind(allocation)
            .bind(last)
            .fetch_all(&mut *db)
            .await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            if crate::history::evidence::eligible(db, &row, a, domain, allow_simulated)
                .await?
                .is_none()
            {
                return Err(Error::Ineligible);
            }
            last = Some(row.try_get::<uuid::Uuid, _>("id")?);
        }
    }
    let proofs=sqlx::query("SELECT h.completed_through,h.completion FROM allocation_history h JOIN completed_live_allocation_history v USING(allocation_id,domain) WHERE h.allocation_id=$1 AND h.domain=$2 UNION ALL SELECT h.completed_through,h.completion FROM released_allocation_history h JOIN completed_released_allocation_history v USING(allocation_id,domain) WHERE h.allocation_id=$1 AND h.domain=$2")
        .bind(allocation).bind(name).fetch_all(&mut *db).await?;
    let mut verified = None;
    let mut simulated = false;
    for proof in proofs {
        let through: uuid::Uuid = proof.try_get("completed_through")?;
        verified = Some(verified.map_or(through, |old: uuid::Uuid| old.max(through)));
        simulated |= proof.try_get::<Value, _>("completion")?["simulated"]
            .as_bool()
            .ok_or(Error::Evidence)?;
    }
    if simulated && !allow_simulated {
        return Err(Error::Evidence);
    }
    let reserved=sqlx::query_scalar::<_,uuid::Uuid>("SELECT reserved_through FROM allocation_history WHERE allocation_id=$1 AND domain=$2 UNION ALL SELECT reserved_through FROM released_allocation_history WHERE allocation_id=$1 AND domain=$2")
        .bind(allocation).bind(name).fetch_all(&mut *db).await?;
    if last != verified || reserved.iter().any(|r| verified.is_none_or(|v| *r > v)) {
        return Err(Error::Ineligible);
    }
    Ok((
        match last {
            Some(id) => DomainClosure::Retired {
                through: OperationId::from_uuid(id),
            },
            None => DomainClosure::Empty {},
        },
        simulated,
    ))
}
