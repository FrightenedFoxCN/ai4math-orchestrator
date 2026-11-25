//! Durable binding of already-verified Owner approvals to the existing Pivot journal.
use super::*;

/// Metadata supplied by the trusted coordinator after checking the approval.
/// No signature or trust anchor crosses into the Store. Digests are lowercase
/// hex SHA-256; `repo` must match the canonical path bound to this ledger.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OwnerArchiveRequest {
    pub request_id: String,
    pub message_sha256: String,
    pub public_key_fingerprint: String,
    pub repo: String,
    pub base_commit: String,
    pub adr_id: String,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OwnerArchiveRequestStatus {
    Prepared,
    Applied,
    Abandoned,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OwnerArchiveRequestRecord {
    pub request: OwnerArchiveRequest,
    pub status: OwnerArchiveRequestStatus,
    pub intent: PivotIntent,
    pub result: Option<PivotResult>,
}

impl StateStore {
    /// Consume a verified request and prepare its exact Pivot in one transaction.
    /// Any replay fails, even after the intent has been abandoned or applied.
    pub fn prepare_owner_archive_pivot(
        &mut self,
        request: &OwnerArchiveRequest,
        intent: &PivotIntent,
    ) -> Result<()> {
        self.prepare_owner_archive_pivot_with_check(request, intent, || Ok(()))
    }

    /// As above, with the coordinator's final freshness check executed inside
    /// the writer transaction, just before commit. The callback must not write
    /// to Git or SQLite; an error rolls back both request and intent.
    pub fn prepare_owner_archive_pivot_with_check(
        &mut self,
        request: &OwnerArchiveRequest,
        intent: &PivotIntent,
        check: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        self.write(|tx| {
            validate(request, intent)?;
            ensure!(
                !tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM owner_archive_requests WHERE request_id=?1)",
                    [&request.request_id],
                    |r| r.get::<_, bool>(0),
                )?,
                "Owner archive request ID was already consumed"
            );
            prepare_pivot_in(tx, intent)?;
            ensure!(
                tx.execute(
                    "INSERT INTO owner_archive_requests(request_id,message_sha256,public_key_fingerprint,adr_id,plan_json,new_commit)
                    SELECT ?1,?2,?3,adr_id,plan_json,new_commit FROM pivot_resume_intent WHERE singleton=1",
                    params![request.request_id, request.message_sha256, request.public_key_fingerprint],
                )? == 1,
                "could not bind the prepared Owner archive intent"
            );
            check()
        })
    }

    /// Read the binding and exact intent/result from one SQLite snapshot. A
    /// pre-migration ledger has no requests; only a writable open migrates it.
    pub fn owner_archive_request(
        &self,
        request_id: &str,
    ) -> Result<Option<OwnerArchiveRequestRecord>> {
        let tx = self.db.unchecked_transaction()?;
        let migrated = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM state_migrations WHERE version=6)",
            [],
            |r| r.get::<_, bool>(0),
        )?;
        let record = if migrated {
            read(&tx, request_id, false)?
        } else {
            None
        };
        tx.commit()?;
        Ok(record)
    }
}

fn validate(request: &OwnerArchiveRequest, intent: &PivotIntent) -> Result<()> {
    ensure!(
        (1..=128).contains(&request.request_id.len())
            && request
                .request_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            && request.message_sha256.len() == 64
            && oid(&request.message_sha256)
            && request.public_key_fingerprint.len() == 64
            && oid(&request.public_key_fingerprint),
        "invalid Owner archive approval metadata"
    );
    ensure!(
        intent.plan.action == PivotAction::ArchiveBreaker,
        "Owner archive approval requires ARCHIVE_BREAKER"
    );
    ensure!(
        Path::new(&request.repo).is_absolute()
            && !request.repo.chars().any(char::is_control)
            && oid(&request.base_commit)
            && request.repo == intent.plan.repo
            && request.base_commit == intent.plan.base_commit
            && request.adr_id == intent.adr_id,
        "Owner archive approval does not match the Pivot repository, base, and ADR"
    );
    Ok(())
}

pub(super) fn ensure_unused_adr(db: &Connection, adr_id: &str) -> Result<()> {
    ensure!(
        !db.query_row(
            "SELECT EXISTS(SELECT 1 FROM owner_archive_requests WHERE adr_id=?1)
                 OR EXISTS(SELECT 1 FROM recorded_pivots WHERE adr_id=?1)",
            [adr_id],
            |r| r.get::<_, bool>(0),
        )?,
        "Pivot ADR was already reserved or recorded"
    );
    Ok(())
}

// Use a single statement so a concurrent land/abandon cannot split the
// binding, singleton, and result observations across different snapshots.
fn read(db: &Connection, id: &str, by_adr: bool) -> Result<Option<OwnerArchiveRequestRecord>> {
    let key = if by_adr { "o.adr_id" } else { "o.request_id" };
    db.query_row(
        &format!("SELECT o.request_id,o.message_sha256,o.public_key_fingerprint,o.adr_id,o.plan_json,o.new_commit,o.status,
                  p.plan_json,p.new_commit,g.plan_json,g.git_commit,g.event_seq,g.reviewed_through
           FROM owner_archive_requests o
           LEFT JOIN pivot_resume_intent p ON p.singleton=1 AND p.adr_id=o.adr_id
           LEFT JOIN recorded_pivots g ON g.adr_id=o.adr_id
           WHERE {key}=?1"),
        [id],
        |r| Ok((
            r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?,
            r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?, r.get::<_, String>(6)?,
            (r.get::<_, Option<String>>(7)?, r.get::<_, Option<String>>(8)?),
            (r.get::<_, Option<String>>(9)?, r.get::<_, Option<String>>(10)?, r.get::<_, Option<i64>>(11)?, r.get::<_, Option<i64>>(12)?),
        )),
    ).optional()?.map(|(request_id, message_sha256, public_key_fingerprint, adr_id, plan_json, new_commit, status, pending, recorded)| {
        let plan: PivotPlan = serde_json::from_str(&plan_json)?;
        ensure!(
            plan.adr_id()? == adr_id && oid(&new_commit) && new_commit != plan.base_commit,
            "durable Owner archive intent is invalid"
        );
        let request = OwnerArchiveRequest {
            request_id,
            message_sha256,
            public_key_fingerprint,
            repo: plan.repo.clone(),
            base_commit: plan.base_commit.clone(),
            adr_id: adr_id.clone(),
        };
        let intent = PivotIntent { adr_id, plan, new_commit };
        validate(&request, &intent)?;
        let (status, result) = match (status.as_str(), pending, recorded) {
            ("PREPARED", (Some(plan), Some(commit)), (None, None, None, None))
                if plan == plan_json && commit == intent.new_commit => (OwnerArchiveRequestStatus::Prepared, None),
            ("APPLIED", (None, None), (Some(plan), Some(git_commit), Some(event_seq), Some(reviewed_through)))
                if plan == plan_json && git_commit == intent.new_commit && reviewed_through == intent.plan.reviewed_through => (
                    OwnerArchiveRequestStatus::Applied,
                    Some(PivotResult {
                        adr_id: intent.adr_id.clone(), git_commit, event_seq, reviewed_through,
                        breaker_state: intent.plan.action.target_state(), probes_started: false, lesson_id: None,
                    }),
                ),
            ("ABANDONED", (None, None), (None, None, None, None)) => (OwnerArchiveRequestStatus::Abandoned, None),
            _ => anyhow::bail!("Owner archive request disagrees with its Pivot journal"),
        };
        Ok(OwnerArchiveRequestRecord { request, status, intent, result })
    }).transpose()
}

pub(super) fn prepared_for(db: &Connection, intent: &PivotIntent) -> Result<Option<String>> {
    read(db, &intent.adr_id, true)?
        .map(|record| {
            ensure!(
                record.status == OwnerArchiveRequestStatus::Prepared && record.intent == *intent,
                "Owner archive request does not match the prepared intent"
            );
            Ok(record.request.request_id)
        })
        .transpose()
}

pub(super) fn transition(
    db: &Connection,
    request_id: &str,
    next: OwnerArchiveRequestStatus,
) -> Result<()> {
    let status = match next {
        OwnerArchiveRequestStatus::Applied => "APPLIED",
        OwnerArchiveRequestStatus::Abandoned => "ABANDONED",
        OwnerArchiveRequestStatus::Prepared => {
            anyhow::bail!("invalid Owner archive status transition")
        }
    };
    ensure!(
        db.execute(
            "UPDATE owner_archive_requests SET status=?2 WHERE request_id=?1 AND status='PREPARED'",
            params![request_id, status],
        )? == 1,
        "Owner archive request is no longer prepared"
    );
    Ok(())
}

#[cfg(test)]
mod tests;
