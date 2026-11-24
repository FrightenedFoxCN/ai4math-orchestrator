use crate::wire_enum;
use std::cmp::Ordering;

wire_enum!(
    /// Evidence for a claim; proof and refutation are separate branches.
    Trust { L0 => "L0", L1 => "L1", L2a => "L2a", L2b => "L2b", L3 => "L3", L4a => "L4a", L4b => "L4b", L5 => "L5" }
);

impl Trust {
    pub const fn proof_rank(self) -> Option<u8> {
        match self {
            Self::L0 => Some(0),
            Self::L1 => Some(1),
            Self::L3 => Some(2),
            Self::L4a => Some(3),
            Self::L4b => Some(4),
            Self::L5 => Some(5),
            Self::L2a | Self::L2b => None,
        }
    }

    pub const fn is_hard_refutation(self) -> bool {
        matches!(self, Self::L2b)
    }
}

impl Default for Trust {
    fn default() -> Self {
        Self::L0
    }
}

impl PartialOrd for Trust {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match (self.proof_rank(), other.proof_rank()) {
            (Some(a), Some(b)) => Some(a.cmp(&b)),
            _ if self == other => Some(Ordering::Equal),
            (Some(rank), None) if rank <= 1 => Some(Ordering::Less),
            (None, Some(rank)) if rank <= 1 => Some(Ordering::Greater),
            _ => match (self, other) {
                (Self::L2a, Self::L2b) => Some(Ordering::Less),
                (Self::L2b, Self::L2a) => Some(Ordering::Greater),
                _ => None,
            },
        }
    }
}

/// AND: the local proof is capped by every dependency; refutations never
/// count as proof evidence. The caller handles branch status and staleness.
pub fn aggregate_plan(
    local: Trust,
    dependencies: impl IntoIterator<Item = Trust>,
) -> Option<Trust> {
    let mut best = local;
    let mut rank = best.proof_rank()?;
    for dependency in dependencies {
        let next = dependency.proof_rank()?;
        if next < rank {
            best = dependency;
            rank = next;
        }
    }
    Some(best)
}

/// OR: choose the strongest surviving proof route, never a refutation.
pub fn best_proof_plan(plans: impl IntoIterator<Item = Trust>) -> Option<Trust> {
    plans
        .into_iter()
        .filter_map(|trust| trust.proof_rank().map(|rank| (rank, trust)))
        .max_by_key(|(rank, _)| *rank)
        .map(|(_, trust)| trust)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Trust::*;

    #[test]
    fn proof_and_refutation_are_not_a_total_order() {
        assert_eq!(L2b.partial_cmp(&L3), None);
        assert_eq!(L5.partial_cmp(&L2a), None);
        assert!(L2b > L2a && L2a > L1);
        assert!(L5 > L4b && L4b > L4a && L4a > L3);
    }

    #[test]
    fn and_or_aggregation_carries_debt_but_not_refutation() {
        assert_eq!(aggregate_plan(L5, []), Some(L5));
        assert_eq!(aggregate_plan(L5, [L4a, L5]), Some(L4a));
        assert_eq!(aggregate_plan(L5, [L4a, L2b]), None);
        assert_eq!(aggregate_plan(L5, [L4a, L2a]), None);
        assert_eq!(best_proof_plan([L2b, L4b, L4a]), Some(L4b));
        assert_eq!(best_proof_plan([L2b, L2a]), None);
    }
}
