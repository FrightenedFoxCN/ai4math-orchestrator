//! Domain-agnostic knowledge vault ontology: four strata (including external
//! `LITERATURE`), source provenance kinds, reproducibility classes for idle
//! verification scheduling, facet schemas, and domain-qualified epistemic tags.

use crate::{EpistemicLevel, ParseEnumError, ScientificTrust, Trust, WikiStratum, wire_enum};
use serde::{Deserialize, Serialize};
use std::fmt;

wire_enum!(
    /// Four-stratum knowledge vault classification extending `WikiStratum` with
    /// an isolated `Literature` stratum for external paper cards.
    KnowledgeStratum {
        Core => "core",
        Sediment => "sediment",
        Graveyard => "graveyard",
        Literature => "literature",
    }
);

impl From<WikiStratum> for KnowledgeStratum {
    fn from(s: WikiStratum) -> Self {
        match s {
            WikiStratum::Core => Self::Core,
            WikiStratum::Sediment => Self::Sediment,
            WikiStratum::Graveyard => Self::Graveyard,
        }
    }
}

impl TryFrom<KnowledgeStratum> for WikiStratum {
    type Error = ParseEnumError;

    fn try_from(s: KnowledgeStratum) -> Result<Self, Self::Error> {
        match s {
            KnowledgeStratum::Core => Ok(Self::Core),
            KnowledgeStratum::Sediment => Ok(Self::Sediment),
            KnowledgeStratum::Graveyard => Ok(Self::Graveyard),
            KnowledgeStratum::Literature => Err(ParseEnumError {
                kind: "WikiStratum",
                value: s.as_str().to_owned(),
            }),
        }
    }
}

wire_enum!(
    /// Epistemic provenance of a knowledge vault entry.
    KnowledgeSourceKind {
        ProjectVerified => "project_verified",
        ProjectArchived => "project_archived",
        ProjectRefuted => "project_refuted",
        LiteratureExtracted => "literature_extracted",
        HumanCurated => "human_curated",
    }
);

wire_enum!(
    /// Whether an external/literature claim can be scheduled for automated
    /// idle-time formalization or computational replication.
    ReproducibilityClass {
        FormalizableInItp => "formalizable_in_itp",
        ReplicableByProtocol => "replicable_by_protocol",
        RequiresWetLab => "requires_wet_lab",
        BackgroundOnly => "background_only",
    }
);

impl ReproducibilityClass {
    /// True when the broker can autonomously formalize or computationally
    /// replicate this claim during idle capacity windows.
    pub const fn is_automatable(self) -> bool {
        matches!(self, Self::FormalizableInItp | Self::ReplicableByProtocol)
    }
}

wire_enum!(
    /// Epistemic stance of an extracted literature assertion relative to its paper.
    ExtractionStance {
        DirectProof => "direct_proof",
        DirectMeasurement => "direct_measurement",
        DirectComputation => "direct_computation",
        SecondaryCitation => "secondary_citation",
        AuthorSpeculation => "author_speculation",
    }
);

wire_enum!(
    /// Registry integrity state of an external bibliographic source.
    CitationStatus {
        Active => "active",
        Corrected => "corrected",
        Retracted => "retracted",
        Preprint => "preprint",
        Unverified => "unverified",
    }
);

wire_enum!(
    /// Unified provenance & scientific graph edge types (strict superset of
    /// existing ITP Wiki edge types).
    KnowledgeEdgeKind {
        DependsOn => "depends_on",
        UsedBy => "used_by",
        Refutes => "refutes",
        Generalizes => "generalizes",
        SupersededBy => "superseded_by",
        BornFromClaim => "born_from_claim",
        BridgeSource => "bridge_source",
        ExtractedFrom => "extracted_from",
        Replicates => "replicates",
        Contradicts => "contradicts",
        CalibratesProtocol => "calibrates_protocol",
        CitesSecondary => "cites_secondary",
    }
);

/// Declaration of one domain-structured inverted-index facet key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FacetDescriptor {
    pub key: &'static str,
    pub required_for_core: bool,
    pub description: &'static str,
}

/// Domain-specific knowledge indexing profile attached to a `DomainPack`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DomainKnowledgeProfile {
    pub domain_id: &'static str,
    pub allowed_facets: &'static [FacetDescriptor],
    pub default_reproducibility: ReproducibilityClass,
}

impl DomainKnowledgeProfile {
    pub fn allows_facet(&self, key: &str) -> bool {
        self.allowed_facets.iter().any(|f| f.key == key)
            || LITERATURE_COMMON_FACETS.iter().any(|f| f.key == key)
    }
}

/// Cross-domain bibliographic facets allowed on `LITERATURE` entries in any domain.
pub const LITERATURE_COMMON_FACETS: &[FacetDescriptor] = &[
    FacetDescriptor {
        key: "doi",
        required_for_core: false,
        description: "Canonical DOI identifier",
    },
    FacetDescriptor {
        key: "arxiv_id",
        required_for_core: false,
        description: "arXiv identifier (with optional version)",
    },
    FacetDescriptor {
        key: "pmid",
        required_for_core: false,
        description: "PubMed identifier",
    },
    FacetDescriptor {
        key: "pub_year",
        required_for_core: false,
        description: "Publication year",
    },
    FacetDescriptor {
        key: "citation_status",
        required_for_core: false,
        description: "Registry status (active/corrected/retracted/preprint/unverified)",
    },
    FacetDescriptor {
        key: "card_kind",
        required_for_core: false,
        description: "Typed literature extraction card kind",
    },
    FacetDescriptor {
        key: "claim_stance",
        required_for_core: false,
        description: "Author stance (direct_proof/direct_measurement/secondary_citation/...)",
    },
    FacetDescriptor {
        key: "reproducibility_class",
        required_for_core: false,
        description: "Automation eligibility for idle replication/formalization",
    },
];

/// Built-in knowledge profile for the ITP (Lean 4 / Agda / Rocq) domain pack.
pub const ITP_KNOWLEDGE_FACETS: &[FacetDescriptor] = &[
    FacetDescriptor {
        key: "conclusion_head",
        required_for_core: false,
        description: "Leading conclusion symbol in a formal declaration",
    },
    FacetDescriptor {
        key: "hypothesis_head",
        required_for_core: false,
        description: "Leading hypothesis symbol in a formal declaration",
    },
    FacetDescriptor {
        key: "typeclass",
        required_for_core: false,
        description: "Typeclass constraint in a formal signature",
    },
    FacetDescriptor {
        key: "const",
        required_for_core: false,
        description: "Referenced constant or definition symbol",
    },
];

pub const ITP_KNOWLEDGE_PROFILE: DomainKnowledgeProfile = DomainKnowledgeProfile {
    domain_id: "itp",
    allowed_facets: ITP_KNOWLEDGE_FACETS,
    default_reproducibility: ReproducibilityClass::FormalizableInItp,
};

/// Domain-qualified epistemic level tag bridging `Trust` (`L0..L5` for `itp`)
/// and `ScientificTrust` (`E0..E5` for empirical/computational domains).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EpistemicTag {
    pub domain_id: String,
    pub code: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidEpistemicTag {
    pub domain_id: String,
    pub code: String,
    pub detail: &'static str,
}

impl fmt::Display for InvalidEpistemicTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid epistemic tag ({:?}, {:?}): {}",
            self.domain_id, self.code, self.detail
        )
    }
}

impl std::error::Error for InvalidEpistemicTag {}

impl EpistemicTag {
    pub fn from_itp(trust: Trust) -> Self {
        Self {
            domain_id: "itp".to_owned(),
            code: trust.as_str().to_owned(),
        }
    }

    pub fn from_scientific(
        domain_id: impl Into<String>,
        trust: ScientificTrust,
    ) -> Result<Self, InvalidEpistemicTag> {
        let domain_id = domain_id.into();
        let tag = Self {
            domain_id,
            code: trust.as_str().to_owned(),
        };
        tag.validate()?;
        Ok(tag)
    }

    pub fn parse(domain_id: &str, code: &str) -> Result<Self, InvalidEpistemicTag> {
        let tag = Self {
            domain_id: domain_id.to_owned(),
            code: code.to_owned(),
        };
        tag.validate()?;
        Ok(tag)
    }

    pub fn validate(&self) -> Result<(), InvalidEpistemicTag> {
        let valid_domain = !self.domain_id.is_empty()
            && self.domain_id.len() <= 64
            && self
                .domain_id
                .starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && self
                .domain_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'));
        if !valid_domain {
            return Err(InvalidEpistemicTag {
                domain_id: self.domain_id.clone(),
                code: self.code.clone(),
                detail: "domain_id must be a safe identifier",
            });
        }
        if self.domain_id == "itp" {
            self.code
                .parse::<Trust>()
                .map_err(|_| InvalidEpistemicTag {
                    domain_id: self.domain_id.clone(),
                    code: self.code.clone(),
                    detail: "itp domain requires Trust levels L0..L5",
                })?;
        } else {
            self.code
                .parse::<ScientificTrust>()
                .map_err(|_| InvalidEpistemicTag {
                    domain_id: self.domain_id.clone(),
                    code: self.code.clone(),
                    detail: "non-itp domains require ScientificTrust levels E0..E5",
                })?;
        }
        Ok(())
    }

    pub fn as_itp(&self) -> Option<Trust> {
        (self.domain_id == "itp")
            .then(|| self.code.parse::<Trust>().ok())
            .flatten()
    }

    pub fn as_scientific(&self) -> Option<ScientificTrust> {
        (self.domain_id != "itp")
            .then(|| self.code.parse::<ScientificTrust>().ok())
            .flatten()
    }

    pub fn positive_rank(&self) -> Option<u8> {
        if let Some(t) = self.as_itp() {
            t.positive_rank()
        } else if let Some(s) = self.as_scientific() {
            s.positive_rank()
        } else {
            None
        }
    }

    pub fn refutation_rank(&self) -> Option<u8> {
        if let Some(t) = self.as_itp() {
            t.refutation_rank()
        } else if let Some(s) = self.as_scientific() {
            s.refutation_rank()
        } else {
            None
        }
    }

    pub fn is_hard_refutation(&self) -> bool {
        self.as_itp()
            .is_some_and(EpistemicLevel::is_hard_refutation)
            || self
                .as_scientific()
                .is_some_and(EpistemicLevel::is_hard_refutation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epistemic_tag_enforces_domain_level_separation() {
        let itp = EpistemicTag::from_itp(Trust::L5);
        assert!(itp.validate().is_ok());
        assert_eq!(itp.positive_rank(), Some(5));

        let chem = EpistemicTag::from_scientific("compchem", ScientificTrust::E4a).unwrap();
        assert_eq!(chem.positive_rank(), Some(3));

        // Cross-domain level mismatch is rejected.
        assert!(EpistemicTag::parse("itp", "E4a").is_err());
        assert!(EpistemicTag::parse("compchem", "L5").is_err());
        assert!(EpistemicTag::parse("", "E0").is_err());
    }

    #[test]
    fn stratum_conversions_and_reproducibility_flags_work() {
        assert_eq!(
            KnowledgeStratum::from(WikiStratum::Core),
            KnowledgeStratum::Core
        );
        assert!(WikiStratum::try_from(KnowledgeStratum::Literature).is_err());
        assert!(ReproducibilityClass::FormalizableInItp.is_automatable());
        assert!(ReproducibilityClass::ReplicableByProtocol.is_automatable());
        assert!(!ReproducibilityClass::RequiresWetLab.is_automatable());
        assert!(!ReproducibilityClass::BackgroundOnly.is_automatable());
        assert!(ITP_KNOWLEDGE_PROFILE.allows_facet("conclusion_head"));
        assert!(ITP_KNOWLEDGE_PROFILE.allows_facet("doi"));
        assert!(!ITP_KNOWLEDGE_PROFILE.allows_facet("unknown_facet"));
    }
}
