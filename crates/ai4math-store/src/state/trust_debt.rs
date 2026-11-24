use super::{
    ReplayedClaim, StateStore, Trust, graph_audit::compare_graph, graph_check::check_replay,
    graph_replay::replay_graph_in_snapshot,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;

const MAX_FINDINGS: usize = 128;

/// Advisory trust-level findings at one consistent ledger head, not proof authority.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct TrustDebtScan {
    pub schema: &'static str,
    pub advisory_only: bool,
    pub ledger_event_seq: i64,
    pub counts: TrustDebtCounts,
    /// Exact number of findings, not unique Claims; includes omitted rows.
    pub finding_count: usize,
    pub truncated: bool,
    /// First 128 findings, ordered by Claim ID then direct, prior_tainted, effective_ceiling.
    pub findings: Vec<TrustDebtFinding>,
}

/// Exact counts including findings beyond the returned prefix.
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct TrustDebtCounts {
    pub direct_oracle: usize,
    pub direct_cross_engine: usize,
    pub prior_tainted_oracle: usize,
    pub prior_tainted_cross_engine: usize,
    pub effective_ceiling_oracle: usize,
    pub effective_ceiling_cross_engine: usize,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct TrustDebtFinding {
    pub claim_id: String,
    pub engine: String,
    pub critical: bool,
    pub stale: bool,
    pub effective_stale: bool,
    pub kind: TrustDebtKind,
    pub role: TrustDebtRole,
    pub local_trust: Trust,
    pub effective_trust: Trust,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prior_trust: Option<Trust>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustDebtKind {
    /// L4a.
    Oracle,
    /// L4b.
    CrossEngine,
}

/// Independent observations of trust levels, not attributions to dependencies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustDebtRole {
    /// Local trust, including on locally stale Claims.
    Direct,
    /// Prior trust on a locally stale Claim.
    PriorTainted,
    /// Effective trust, which may equal local trust.
    EffectiveCeiling,
}

impl StateStore {
    /// Replay, independently check semantics, and compare live graph fields at one read
    /// snapshot. Refuse inconsistent, malformed, or over-budget graphs without returning
    /// partial findings. Does not check evidence, receipts, or the legacy oracle_debt flag.
    pub fn scan_trust_debt(&self) -> Result<TrustDebtScan> {
        let tx = self.db.unchecked_transaction()?;
        let replay = replay_graph_in_snapshot(&tx, None)
            .context("trust debt scan refused: ledger graph replay failed")?;
        let semantic = check_replay(&replay)
            .context("trust debt scan refused: could not check ledger graph semantics")?;
        ensure!(
            semantic.consistent,
            "trust debt scan refused: ledger graph semantic check failed ({} mismatches)",
            semantic.mismatch_count
        );
        let audit = compare_graph(&tx, &replay).context(
            "trust debt scan refused: could not compare the live graph to ledger replay",
        )?;
        ensure!(
            audit.consistent,
            "trust debt scan refused: live graph differs from ledger replay ({} mismatches)",
            audit.mismatch_count
        );

        let mut scan = TrustDebtScan {
            schema: "ai4math.t2.trust-debt-scan.v1",
            advisory_only: true,
            ledger_event_seq: replay.ledger_head_event_seq,
            counts: TrustDebtCounts::default(),
            finding_count: 0,
            truncated: false,
            findings: Vec::new(),
        };
        // Replay supplies Claim ID order; the array fixes the order among overlapping roles.
        for claim in &replay.claims {
            for (role, trust) in [
                (TrustDebtRole::Direct, Some(claim.local_trust)),
                (
                    TrustDebtRole::PriorTainted,
                    claim.prior_trust.filter(|_| claim.stale),
                ),
                (TrustDebtRole::EffectiveCeiling, Some(claim.effective_trust)),
            ] {
                let kind = match trust {
                    Some(Trust::L4a) => TrustDebtKind::Oracle,
                    Some(Trust::L4b) => TrustDebtKind::CrossEngine,
                    _ => continue,
                };
                scan.record(claim, role, kind);
            }
        }
        tx.commit()?;
        Ok(scan)
    }
}

impl TrustDebtScan {
    fn record(&mut self, claim: &ReplayedClaim, role: TrustDebtRole, kind: TrustDebtKind) {
        use TrustDebtKind::{CrossEngine, Oracle};
        use TrustDebtRole::{Direct, EffectiveCeiling, PriorTainted};

        let count = match (role, kind) {
            (Direct, Oracle) => &mut self.counts.direct_oracle,
            (Direct, CrossEngine) => &mut self.counts.direct_cross_engine,
            (PriorTainted, Oracle) => &mut self.counts.prior_tainted_oracle,
            (PriorTainted, CrossEngine) => &mut self.counts.prior_tainted_cross_engine,
            (EffectiveCeiling, Oracle) => &mut self.counts.effective_ceiling_oracle,
            (EffectiveCeiling, CrossEngine) => &mut self.counts.effective_ceiling_cross_engine,
        };
        *count += 1;
        self.finding_count += 1;
        if self.findings.len() == MAX_FINDINGS {
            self.truncated = true;
            return;
        }
        self.findings.push(TrustDebtFinding {
            claim_id: claim.claim_id.clone(),
            engine: claim.engine.clone(),
            critical: claim.critical,
            stale: claim.stale,
            effective_stale: claim.effective_stale,
            kind,
            role,
            local_trust: claim.local_trust,
            effective_trust: claim.effective_trust,
            prior_trust: claim.prior_trust,
        });
    }
}
