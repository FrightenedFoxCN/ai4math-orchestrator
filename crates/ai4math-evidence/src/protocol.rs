//! IO-free anti-drift protocol specification, typed slot compilation,
//! deterministic physical/numerical sanity gates, blinded control verification,
//! pre-registration contracts, and protocol-smell drift detection.
//!
//! Generalizes the typed-DSL pattern in `oracle.rs` (rejecting raw untrusted
//! solver scripts in favor of schema-validated slot compilation) to arbitrary
//! computational science workflows (e.g. computational chemistry and biology).

use crate::ordered_source_set_sha256;
use ai4math_core::{EpistemicLevel, ScientificTrust};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_PROTOCOL_SPEC_BYTES: usize = 128 * 1024;
pub const MAX_SUBMISSION_BYTES: usize = 64 * 1024;
pub const MAX_CONTROL_PACK_BYTES: usize = 256 * 1024;
pub const MAX_PREREG_BYTES: usize = 32 * 1024;
pub const MAX_RENDERED_PROTOCOL_BYTES: usize = 512 * 1024;
pub const MAX_SLOTS: usize = 32;
pub const MAX_PARAMS: usize = 64;
pub const MAX_TEMPLATES: usize = 16;
pub const MAX_PINNED_INPUTS: usize = 64;
pub const MAX_GATES: usize = 64;
pub const MAX_CONTROLS_PER_POLARITY: usize = 32;
pub const MAX_DIAGNOSTICS: usize = 128;
pub const MAX_DEBT_CODES: usize = 32;
pub const MAX_TRIALS_LIMIT: u32 = 100_000;

fn sha256_json(domain: &str, value: impl Serialize) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(domain.as_bytes());
    hash.update([0u8]);
    hash.update(serde_json::to_vec(&value)?);
    Ok(format!("{:x}", hash.finalize()))
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_safe_id(value: &str, max_len: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_len
        && value.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

fn is_safe_relative_path(path: &str) -> bool {
    if path.is_empty() || path.len() > 256 || path.starts_with('/') || path.ends_with('/') {
        return false;
    }
    path.split('/').all(|segment| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
            && segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    })
}

fn has_forbidden_control_or_template_chars(s: &str, allow_newlines: bool) -> bool {
    if s.contains("{{") || s.contains("}}") {
        return true;
    }
    s.chars().any(|c| {
        if allow_newlines && c == '\n' {
            return false;
        }
        c.is_control()
            || matches!(
                c,
                '\u{ad}'
                    | '\u{200b}'..='\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2060}'..='\u{206f}'
                    | '\u{feff}'
            )
    })
}

fn canonical_f64(value: f64) -> Result<f64> {
    ensure!(value.is_finite(), "non-finite floating-point value");
    Ok(if value == 0.0 { 0.0 } else { value })
}

/// Immutable protocol-level parameter values controlled solely by the frozen
/// `ProtocolSpec`, never writable by candidate workers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ParamValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

impl ParamValue {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Bool(_) | Self::Int(_) => Ok(()),
            Self::Float(v) => {
                canonical_f64(*v)?;
                Ok(())
            }
            Self::Text(s) => {
                ensure!(
                    s.len() <= 4096 && !has_forbidden_control_or_template_chars(s, false),
                    "invalid frozen protocol text parameter"
                );
                Ok(())
            }
        }
    }

    fn render(&self) -> Result<String> {
        Ok(match self {
            Self::Bool(b) => b.to_string(),
            Self::Int(i) => i.to_string(),
            Self::Float(v) => format!("{}", canonical_f64(*v)?),
            Self::Text(s) => s.clone(),
        })
    }
}

/// Strict single-line character profile enforced on text candidate slots to
/// prevent line/command/parameter injection into rendered scientific input files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextProfile {
    /// `[A-Za-z_][A-Za-z0-9_.-]*`
    Identifier,
    /// Printable single-line ASCII (`0x20..=0x7E`) with no `{{` / `}}`.
    SafeAscii,
    /// Strict single-line SMILES alphabet (no whitespace, newlines, quotes, or shell chars).
    Smiles,
    /// Strict single-line IUPAC amino-acid / nucleotide sequence (`[A-Za-z*]+`).
    Fasta,
}

impl TextProfile {
    fn validate_value(self, value: &str, max_len: usize) -> Result<()> {
        ensure!(
            !value.is_empty() && value.len() <= max_len,
            "slot text must be non-empty and within max_len"
        );
        ensure!(
            !has_forbidden_control_or_template_chars(value, false),
            "slot text contains forbidden control or template-delimiter characters"
        );
        match self {
            Self::Identifier => {
                ensure!(
                    is_safe_id(value, max_len),
                    "slot text is not a valid identifier"
                );
            }
            Self::SafeAscii => {
                ensure!(
                    value.bytes().all(|b| (0x20..=0x7e).contains(&b)),
                    "slot text must contain only printable single-line ASCII"
                );
            }
            Self::Smiles => {
                ensure!(
                    value.bytes().all(|b| {
                        b.is_ascii_alphanumeric()
                            || matches!(
                                b,
                                b'@' | b'+'
                                    | b'-'
                                    | b'['
                                    | b']'
                                    | b'('
                                    | b')'
                                    | b'='
                                    | b'#'
                                    | b'$'
                                    | b':'
                                    | b'/'
                                    | b'\\'
                                    | b'.'
                                    | b'%'
                                    | b'*'
                            )
                    }),
                    "slot text contains characters outside the strict SMILES alphabet"
                );
            }
            Self::Fasta => {
                ensure!(
                    value.bytes().all(|b| b.is_ascii_alphabetic() || b == b'*'),
                    "slot text contains characters outside the strict single-line FASTA alphabet"
                );
            }
        }
        Ok(())
    }
}

/// Allowed type and bounds of one variable slot exposed to candidate workers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SlotKind {
    Text {
        max_len: usize,
        profile: TextProfile,
    },
    Int {
        min: i64,
        max: i64,
    },
    Float {
        min: f64,
        max: f64,
    },
    Bool,
    Enum {
        allowed: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotSpec {
    pub name: String,
    #[serde(flatten)]
    pub kind: SlotKind,
}

impl SlotSpec {
    fn validate(&self) -> Result<()> {
        ensure!(
            is_safe_id(&self.name, 64),
            "invalid slot name {:?}",
            self.name
        );
        match &self.kind {
            SlotKind::Text { max_len, .. } => {
                ensure!(
                    (1..=16_384).contains(max_len),
                    "text slot max_len must be in 1..=16384"
                );
            }
            SlotKind::Int { min, max } => {
                ensure!(min <= max, "int slot min must not exceed max");
            }
            SlotKind::Float { min, max } => {
                let lo = canonical_f64(*min)?;
                let hi = canonical_f64(*max)?;
                ensure!(lo <= hi, "float slot min must not exceed max");
            }
            SlotKind::Bool => {}
            SlotKind::Enum { allowed } => {
                ensure!(
                    !allowed.is_empty() && allowed.len() <= 64,
                    "enum slot must declare 1..=64 allowed values"
                );
                let mut seen = BTreeSet::new();
                for item in allowed {
                    TextProfile::SafeAscii.validate_value(item, 128)?;
                    ensure!(seen.insert(item.as_str()), "duplicate enum slot value");
                }
            }
        }
        Ok(())
    }

    fn validate_value(&self, value: &SlotValue) -> Result<String> {
        match (&self.kind, value) {
            (SlotKind::Text { max_len, profile }, SlotValue::Text(s)) => {
                profile.validate_value(s, *max_len)?;
                Ok(s.clone())
            }
            (SlotKind::Int { min, max }, SlotValue::Int(v)) => {
                ensure!(
                    (*min..=*max).contains(v),
                    "int slot {:?} value {} outside [{}..={}]",
                    self.name,
                    v,
                    min,
                    max
                );
                Ok(v.to_string())
            }
            (SlotKind::Float { min, max }, SlotValue::Float(v)) => {
                let v = canonical_f64(*v)?;
                ensure!(
                    v >= *min && v <= *max,
                    "float slot {:?} value {} outside [{}..={}]",
                    self.name,
                    v,
                    min,
                    max
                );
                Ok(format!("{v}"))
            }
            (SlotKind::Bool, SlotValue::Bool(b)) => Ok(b.to_string()),
            (SlotKind::Enum { allowed }, SlotValue::Text(s)) => {
                ensure!(
                    allowed.iter().any(|item| item == s),
                    "enum slot {:?} value {:?} is not in allowlist",
                    self.name,
                    s
                );
                Ok(s.clone())
            }
            _ => bail!("slot {:?} received a value of the wrong type", self.name),
        }
    }
}

/// Typed value submitted by a worker to fill one declared slot in a frozen protocol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum SlotValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

/// Frozen input-file template compiled by the broker using single-pass substitution
/// of `{{param:<name>}}` and `{{slot:<name>}}` placeholders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactTemplate {
    pub path: String,
    pub template: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TemplateChunk<'a> {
    Literal(&'a str),
    Param(&'a str),
    Slot(&'a str),
}

fn parse_template(template: &str) -> Result<Vec<TemplateChunk<'_>>> {
    ensure!(
        template.len() <= MAX_RENDERED_PROTOCOL_BYTES,
        "artifact template exceeds byte cap"
    );
    ensure!(
        !template
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')),
        "artifact template contains forbidden control characters"
    );
    let mut chunks = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        let literal = &rest[..open];
        ensure!(
            !literal.contains("}}"),
            "unmatched closing template delimiter `}}`"
        );
        if !literal.is_empty() {
            chunks.push(TemplateChunk::Literal(literal));
        }
        let after_open = &rest[open + 2..];
        let Some(close) = after_open.find("}}") else {
            bail!("unterminated template placeholder `{{`");
        };
        let body = &after_open[..close];
        ensure!(
            !body.contains("{{") && !body.contains("}}"),
            "nested template placeholder is not permitted"
        );
        if let Some(name) = body.strip_prefix("param:") {
            ensure!(is_safe_id(name, 64), "invalid param placeholder {:?}", body);
            chunks.push(TemplateChunk::Param(name));
        } else if let Some(name) = body.strip_prefix("slot:") {
            ensure!(is_safe_id(name, 64), "invalid slot placeholder {:?}", body);
            chunks.push(TemplateChunk::Slot(name));
        } else {
            bail!(
                "unsupported template placeholder {:?}; expected param:<name> or slot:<name>",
                body
            );
        }
        rest = &after_open[close + 2..];
    }
    ensure!(
        !rest.contains("}}"),
        "unmatched closing template delimiter `}}`"
    );
    if !rest.is_empty() {
        chunks.push(TemplateChunk::Literal(rest));
    }
    Ok(chunks)
}

/// Deterministic physical/numerical necessary condition or target acceptance rule
/// evaluated exclusively by the broker over parsed diagnostics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum MetricRule {
    RequiredPresent { metric: String },
    EqualsBool { metric: String, expected: bool },
    EqualsInt { metric: String, expected: i64 },
    MinFloat { metric: String, min: f64 },
    MaxFloat { metric: String, max: f64 },
    RangeFloat { metric: String, min: f64, max: f64 },
}

impl MetricRule {
    pub fn metric(&self) -> &str {
        match self {
            Self::RequiredPresent { metric }
            | Self::EqualsBool { metric, .. }
            | Self::EqualsInt { metric, .. }
            | Self::MinFloat { metric, .. }
            | Self::MaxFloat { metric, .. }
            | Self::RangeFloat { metric, .. } => metric,
        }
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            is_safe_id(self.metric(), 128),
            "invalid diagnostic metric name {:?}",
            self.metric()
        );
        match self {
            Self::MinFloat { min, .. } => {
                canonical_f64(*min)?;
            }
            Self::MaxFloat { max, .. } => {
                canonical_f64(*max)?;
            }
            Self::RangeFloat { min, max, .. } => {
                let lo = canonical_f64(*min)?;
                let hi = canonical_f64(*max)?;
                ensure!(lo <= hi, "RangeFloat min must not exceed max");
            }
            Self::RequiredPresent { .. } | Self::EqualsBool { .. } | Self::EqualsInt { .. } => {}
        }
        Ok(())
    }

    fn evaluate(&self, diagnostics: &ParsedDiagnostics) -> std::result::Result<(), String> {
        let Some(value) = diagnostics.metrics.get(self.metric()) else {
            return Err(format!("missing required metric {:?}", self.metric()));
        };
        match (self, value) {
            (Self::RequiredPresent { .. }, _) => Ok(()),
            (Self::EqualsBool { expected, .. }, DiagnosticValue::Bool(actual)) => {
                if actual == expected {
                    Ok(())
                } else {
                    Err(format!("expected {expected}, observed {actual}"))
                }
            }
            (Self::EqualsInt { expected, .. }, DiagnosticValue::Int(actual)) => {
                if actual == expected {
                    Ok(())
                } else {
                    Err(format!("expected {expected}, observed {actual}"))
                }
            }
            (Self::MinFloat { min, .. }, v) => {
                let actual = v
                    .as_finite_f64()
                    .ok_or_else(|| "expected finite numeric metric".to_owned())?;
                if actual >= *min {
                    Ok(())
                } else {
                    Err(format!("observed {actual} < min {min}"))
                }
            }
            (Self::MaxFloat { max, .. }, v) => {
                let actual = v
                    .as_finite_f64()
                    .ok_or_else(|| "expected finite numeric metric".to_owned())?;
                if actual <= *max {
                    Ok(())
                } else {
                    Err(format!("observed {actual} > max {max}"))
                }
            }
            (Self::RangeFloat { min, max, .. }, v) => {
                let actual = v
                    .as_finite_f64()
                    .ok_or_else(|| "expected finite numeric metric".to_owned())?;
                if actual >= *min && actual <= *max {
                    Ok(())
                } else {
                    Err(format!("observed {actual} outside [{min}, {max}]"))
                }
            }
            _ => Err(format!(
                "metric {:?} has incompatible value type",
                self.metric()
            )),
        }
    }
}

/// Named gate binding a human-readable physical/statistical rationale to a `MetricRule`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedMetricRule {
    pub rule_id: String,
    pub rule: MetricRule,
}

impl NamedMetricRule {
    fn validate(&self) -> Result<()> {
        ensure!(
            is_safe_id(&self.rule_id, 64),
            "invalid gate rule_id {:?}",
            self.rule_id
        );
        self.rule.validate()
    }
}

/// Multi-verifier / orthogonal corroboration policy enforced by the broker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrthogonalPolicy {
    pub min_verifiers: u8,
    #[serde(default)]
    pub require_distinct_backends: bool,
    /// Maximum allowed spread (`max - min`) across verifiers for named numeric metrics.
    #[serde(default)]
    pub metric_tolerances: BTreeMap<String, f64>,
}

impl Default for OrthogonalPolicy {
    fn default() -> Self {
        Self {
            min_verifiers: 1,
            require_distinct_backends: false,
            metric_tolerances: BTreeMap::new(),
        }
    }
}

impl OrthogonalPolicy {
    fn validate(&self) -> Result<()> {
        ensure!(
            (1..=4).contains(&self.min_verifiers),
            "min_verifiers must be in 1..=4"
        );
        ensure!(
            self.metric_tolerances.len() <= MAX_GATES,
            "too many metric tolerances"
        );
        for (metric, tol) in &self.metric_tolerances {
            ensure!(
                is_safe_id(metric, 128),
                "invalid tolerance metric {:?}",
                metric
            );
            let tol = canonical_f64(*tol)?;
            ensure!(tol >= 0.0, "metric tolerance must be non-negative");
        }
        Ok(())
    }
}

/// Commitment to hidden positive/negative controls evaluated exclusively in the
/// broker clean-room so workers cannot inspect or overfit to control identities.
///
/// Merkle commitment order (acyclic):
/// `BlindedControlPack -> control_pack_sha256 -> ProtocolSpec -> protocol_sha256`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlindedControlPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_pack_sha256: Option<String>,
    #[serde(default)]
    pub min_positive_controls: u8,
    #[serde(default)]
    pub min_negative_controls: u8,
}

impl BlindedControlPolicy {
    pub fn requires_controls(&self) -> bool {
        self.min_positive_controls > 0
            || self.min_negative_controls > 0
            || self.control_pack_sha256.is_some()
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            usize::from(self.min_positive_controls) <= MAX_CONTROLS_PER_POLARITY
                && usize::from(self.min_negative_controls) <= MAX_CONTROLS_PER_POLARITY,
            "control count exceeds cap"
        );
        if let Some(hash) = &self.control_pack_sha256 {
            ensure!(
                is_sha256_hex(hash),
                "control_pack_sha256 must be a lowercase 64-hex SHA-256 digest"
            );
        } else if self.min_positive_controls > 0 || self.min_negative_controls > 0 {
            bail!("control_pack_sha256 is required whenever minimum controls are configured");
        }
        Ok(())
    }
}

/// Frozen, content-addressed verification protocol specification.
///
/// Subagents cannot alter `pinned_inputs`, `frozen_parameters`, `artifact_templates`,
/// `sanity_gates`, `acceptance_criteria`, `orthogonal_policy`, or `control_policy`;
/// they may only submit typed values matching `slots`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolSpec {
    pub version: u8,
    pub protocol_id: String,
    pub domain_id: String,
    pub backend_id: String,
    pub epistemic_ceiling: ScientificTrust,
    #[serde(default)]
    pub default_debts: Vec<String>,
    #[serde(default)]
    pub pinned_inputs: BTreeMap<String, String>,
    #[serde(default)]
    pub frozen_parameters: BTreeMap<String, ParamValue>,
    pub slots: Vec<SlotSpec>,
    pub artifact_templates: Vec<ArtifactTemplate>,
    pub sanity_gates: Vec<NamedMetricRule>,
    pub acceptance_criteria: Vec<NamedMetricRule>,
    #[serde(default)]
    pub orthogonal_policy: OrthogonalPolicy,
    #[serde(default)]
    pub control_policy: BlindedControlPolicy,
    #[serde(default)]
    pub preregistration_required: bool,
}

impl ProtocolSpec {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_PROTOCOL_SPEC_BYTES,
            "protocol spec exceeds byte cap"
        );
        let spec: Self =
            serde_json::from_slice(bytes).context("expected a typed JSON ProtocolSpec")?;
        spec.validate()?;
        Ok(spec)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "unsupported ProtocolSpec version");
        ensure!(is_safe_id(&self.protocol_id, 128), "invalid protocol_id");
        ensure!(is_safe_id(&self.domain_id, 64), "invalid domain_id");
        ensure!(is_safe_id(&self.backend_id, 64), "invalid backend_id");
        ensure!(
            self.epistemic_ceiling
                .positive_rank()
                .is_some_and(|rank| rank >= 1),
            "protocol epistemic_ceiling must be a positive epistemic level >= E1"
        );
        ensure!(
            self.default_debts.len() <= MAX_DEBT_CODES,
            "too many default_debts"
        );
        let mut seen_debts = BTreeSet::new();
        for debt in &self.default_debts {
            ensure!(is_safe_id(debt, 64), "invalid debt code {:?}", debt);
            ensure!(seen_debts.insert(debt.as_str()), "duplicate debt code");
        }
        ensure!(
            self.pinned_inputs.len() <= MAX_PINNED_INPUTS,
            "too many pinned_inputs"
        );
        for (name, digest) in &self.pinned_inputs {
            ensure!(
                is_safe_id(name, 128),
                "invalid pinned input name {:?}",
                name
            );
            ensure!(
                is_sha256_hex(digest),
                "pinned input {:?} must have a lowercase 64-hex SHA-256 digest",
                name
            );
        }
        ensure!(
            self.frozen_parameters.len() <= MAX_PARAMS,
            "too many frozen_parameters"
        );
        for (name, val) in &self.frozen_parameters {
            ensure!(
                is_safe_id(name, 64),
                "invalid frozen parameter name {:?}",
                name
            );
            val.validate()?;
        }
        ensure!(
            !self.slots.is_empty() && self.slots.len() <= MAX_SLOTS,
            "protocol must declare 1..=32 candidate slots"
        );
        let mut slot_names = BTreeSet::new();
        for slot in &self.slots {
            slot.validate()?;
            ensure!(
                slot_names.insert(slot.name.as_str()),
                "duplicate slot name {:?}",
                slot.name
            );
        }
        ensure!(
            !self.artifact_templates.is_empty() && self.artifact_templates.len() <= MAX_TEMPLATES,
            "protocol must declare 1..=16 artifact templates"
        );
        let mut folded_paths = BTreeSet::new();
        for tpl in &self.artifact_templates {
            ensure!(
                is_safe_relative_path(&tpl.path),
                "unsafe artifact template path {:?}",
                tpl.path
            );
            ensure!(
                folded_paths.insert(tpl.path.to_ascii_lowercase()),
                "case-insensitive collision on artifact template path {:?}",
                tpl.path
            );
            for chunk in parse_template(&tpl.template)? {
                match chunk {
                    TemplateChunk::Literal(_) => {}
                    TemplateChunk::Param(name) => {
                        ensure!(
                            self.frozen_parameters.contains_key(name),
                            "template references undeclared frozen parameter {:?}",
                            name
                        );
                    }
                    TemplateChunk::Slot(name) => {
                        ensure!(
                            slot_names.contains(name),
                            "template references undeclared candidate slot {:?}",
                            name
                        );
                    }
                }
            }
        }
        ensure!(
            !self.sanity_gates.is_empty() && self.sanity_gates.len() <= MAX_GATES,
            "protocol must declare 1..=64 mandatory sanity gates"
        );
        let mut seen_sanity = BTreeSet::new();
        for gate in &self.sanity_gates {
            gate.validate()?;
            ensure!(
                seen_sanity.insert(gate.rule_id.as_str()),
                "duplicate sanity gate rule_id {:?}",
                gate.rule_id
            );
        }
        ensure!(
            !self.acceptance_criteria.is_empty() && self.acceptance_criteria.len() <= MAX_GATES,
            "protocol must declare 1..=64 acceptance criteria"
        );
        let mut seen_criteria = BTreeSet::new();
        for criterion in &self.acceptance_criteria {
            criterion.validate()?;
            ensure!(
                seen_criteria.insert(criterion.rule_id.as_str()),
                "duplicate acceptance criterion rule_id {:?}",
                criterion.rule_id
            );
        }
        self.orthogonal_policy.validate()?;
        if self.epistemic_ceiling == ScientificTrust::E4b {
            ensure!(
                self.orthogonal_policy.min_verifiers >= 2,
                "E4b multi-fidelity protocols require orthogonal_policy.min_verifiers >= 2"
            );
        }
        self.control_policy.validate()?;
        Ok(())
    }

    /// Canonical SHA-256 fingerprint of this frozen protocol specification.
    pub fn protocol_sha256(&self) -> Result<String> {
        self.validate()?;
        sha256_json("ai4math:protocol-spec:v1", self)
    }

    /// Canonical SHA-256 of pinned immutable inputs declared by this protocol.
    pub fn pinned_inputs_sha256(&self) -> Result<String> {
        self.validate()?;
        sha256_json("ai4math:protocol-pinned-inputs:v1", &self.pinned_inputs)
    }

    /// Validate a worker's typed submission and deterministically compile all
    /// frozen artifact templates in a single pass with zero re-interpolation.
    pub fn compile_submission(
        &self,
        submission: &CandidateSubmission,
    ) -> Result<CompiledProtocolRun> {
        let protocol_sha256 = self.protocol_sha256()?;
        submission.validate_against(self, &protocol_sha256)
    }
}

/// Untrusted typed candidate submission supplied by a worker subagent.
///
/// The worker can only supply values for the exact slots declared by the frozen
/// `ProtocolSpec`, bound cryptographically to `protocol_sha256`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateSubmission {
    pub version: u8,
    pub protocol_sha256: String,
    pub candidate_id: String,
    pub slot_values: BTreeMap<String, SlotValue>,
}

impl CandidateSubmission {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_SUBMISSION_BYTES,
            "candidate submission exceeds byte cap"
        );
        serde_json::from_slice(bytes).context("expected a typed JSON CandidateSubmission")
    }

    fn validate_against(
        &self,
        protocol: &ProtocolSpec,
        expected_protocol_sha256: &str,
    ) -> Result<CompiledProtocolRun> {
        ensure!(self.version == 1, "unsupported CandidateSubmission version");
        ensure!(
            self.protocol_sha256 == expected_protocol_sha256,
            "candidate submission protocol_sha256 does not match frozen protocol"
        );
        ensure!(
            is_safe_id(&self.candidate_id, 128),
            "invalid candidate_id {:?}",
            self.candidate_id
        );
        ensure!(
            self.slot_values.len() == protocol.slots.len(),
            "submission must provide exact declared slot set (expected {}, got {})",
            protocol.slots.len(),
            self.slot_values.len()
        );

        let mut rendered_slots = BTreeMap::new();
        for slot in &protocol.slots {
            let Some(val) = self.slot_values.get(&slot.name) else {
                bail!("missing required candidate slot {:?}", slot.name);
            };
            let rendered = slot.validate_value(val)?;
            ensure!(
                !rendered.contains("{{") && !rendered.contains("}}"),
                "rendered slot value contains template delimiters"
            );
            rendered_slots.insert(slot.name.as_str(), rendered);
        }

        let mut rendered_params = BTreeMap::new();
        for (k, v) in &protocol.frozen_parameters {
            rendered_params.insert(k.as_str(), v.render()?);
        }

        let mut artifacts = BTreeMap::new();
        let mut total_bytes: usize = 0;
        for tpl in &protocol.artifact_templates {
            let chunks = parse_template(&tpl.template)?;
            let mut out = String::with_capacity(tpl.template.len());
            for chunk in chunks {
                let piece = match chunk {
                    TemplateChunk::Literal(s) => s,
                    TemplateChunk::Param(name) => rendered_params
                        .get(name)
                        .map(String::as_str)
                        .context("missing validated param")?,
                    TemplateChunk::Slot(name) => rendered_slots
                        .get(name)
                        .map(String::as_str)
                        .context("missing validated slot")?,
                };
                total_bytes = total_bytes
                    .checked_add(piece.len())
                    .context("rendered artifact byte count overflow")?;
                ensure!(
                    total_bytes <= MAX_RENDERED_PROTOCOL_BYTES,
                    "rendered protocol artifacts exceed byte budget"
                );
                out.push_str(piece);
            }
            artifacts.insert(tpl.path.clone(), out);
        }

        let ordered_pairs: Vec<(&str, &str)> = artifacts
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_str()))
            .collect();
        let rendered_artifacts_sha256 = ordered_source_set_sha256(&ordered_pairs);
        let candidate_sha256 = sha256_json("ai4math:protocol-candidate:v1", self)?;

        Ok(CompiledProtocolRun {
            protocol_id: protocol.protocol_id.clone(),
            protocol_sha256: expected_protocol_sha256.to_owned(),
            candidate_id: self.candidate_id.clone(),
            candidate_sha256,
            rendered_artifacts_sha256,
            artifacts,
        })
    }
}

/// Deterministic broker-compiled input bundle ready for clean-room execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledProtocolRun {
    protocol_id: String,
    protocol_sha256: String,
    candidate_id: String,
    candidate_sha256: String,
    rendered_artifacts_sha256: String,
    artifacts: BTreeMap<String, String>,
}

impl CompiledProtocolRun {
    pub fn protocol_id(&self) -> &str {
        &self.protocol_id
    }

    pub fn protocol_sha256(&self) -> &str {
        &self.protocol_sha256
    }

    pub fn candidate_id(&self) -> &str {
        &self.candidate_id
    }

    pub fn candidate_sha256(&self) -> &str {
        &self.candidate_sha256
    }

    pub fn rendered_artifacts_sha256(&self) -> &str {
        &self.rendered_artifacts_sha256
    }

    pub fn artifacts(&self) -> &BTreeMap<String, String> {
        &self.artifacts
    }
}

/// One hidden positive or negative control case kept strictly inside the broker
/// clean-room and never disclosed to candidate workers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlindedControlCase {
    pub control_id: String,
    pub slot_values: BTreeMap<String, SlotValue>,
}

/// Broker-exclusive blinded control set committed to by `ProtocolSpec.control_policy`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlindedControlPack {
    pub version: u8,
    pub protocol_id: String,
    #[serde(default)]
    pub positive_controls: Vec<BlindedControlCase>,
    #[serde(default)]
    pub negative_controls: Vec<BlindedControlCase>,
}

impl BlindedControlPack {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_CONTROL_PACK_BYTES,
            "blinded control pack exceeds byte cap"
        );
        let pack: Self =
            serde_json::from_slice(bytes).context("expected a typed JSON BlindedControlPack")?;
        pack.validate_shape()?;
        Ok(pack)
    }

    fn validate_shape(&self) -> Result<()> {
        ensure!(self.version == 1, "unsupported BlindedControlPack version");
        ensure!(
            is_safe_id(&self.protocol_id, 128),
            "invalid control pack protocol_id"
        );
        ensure!(
            self.positive_controls.len() <= MAX_CONTROLS_PER_POLARITY
                && self.negative_controls.len() <= MAX_CONTROLS_PER_POLARITY,
            "control pack exceeds maximum controls per polarity"
        );
        let mut seen = BTreeSet::new();
        for case in self
            .positive_controls
            .iter()
            .chain(self.negative_controls.iter())
        {
            ensure!(
                is_safe_id(&case.control_id, 128),
                "invalid blinded control_id {:?}",
                case.control_id
            );
            ensure!(
                seen.insert(case.control_id.as_str()),
                "duplicate blinded control_id {:?}",
                case.control_id
            );
        }
        Ok(())
    }

    pub fn fingerprint(&self) -> Result<String> {
        self.validate_shape()?;
        sha256_json("ai4math:blinded-control-pack:v1", self)
    }

    /// Verify cryptographic commitment against `protocol` and compile all hidden
    /// positive and negative controls through the exact same frozen template pipeline.
    pub fn compile_for(&self, protocol: &ProtocolSpec) -> Result<CompiledControlSet> {
        self.validate_shape()?;
        let protocol_sha256 = protocol.protocol_sha256()?;
        ensure!(
            self.protocol_id == protocol.protocol_id,
            "blinded control pack is bound to protocol_id {:?}, expected {:?}",
            self.protocol_id,
            protocol.protocol_id
        );
        let expected_pack_hash = protocol
            .control_policy
            .control_pack_sha256
            .as_deref()
            .context("protocol does not declare a control_pack_sha256 commitment")?;
        let actual_pack_hash = self.fingerprint()?;
        ensure!(
            actual_pack_hash == expected_pack_hash,
            "blinded control pack digest mismatch against frozen protocol commitment"
        );
        ensure!(
            self.positive_controls.len()
                >= usize::from(protocol.control_policy.min_positive_controls),
            "blinded control pack has fewer positive controls than protocol requires"
        );
        ensure!(
            self.negative_controls.len()
                >= usize::from(protocol.control_policy.min_negative_controls),
            "blinded control pack has fewer negative controls than protocol requires"
        );

        let compile_case = |case: &BlindedControlCase| -> Result<CompiledProtocolRun> {
            let sub = CandidateSubmission {
                version: 1,
                protocol_sha256: protocol_sha256.clone(),
                candidate_id: case.control_id.clone(),
                slot_values: case.slot_values.clone(),
            };
            protocol.compile_submission(&sub)
        };

        let mut positive = Vec::with_capacity(self.positive_controls.len());
        for case in &self.positive_controls {
            positive.push(compile_case(case)?);
        }
        let mut negative = Vec::with_capacity(self.negative_controls.len());
        for case in &self.negative_controls {
            negative.push(compile_case(case)?);
        }
        Ok(CompiledControlSet {
            control_pack_sha256: actual_pack_hash,
            positive,
            negative,
        })
    }
}

/// Compiled hidden controls ready for broker-internal clean-room execution.
#[derive(Debug, Clone)]
pub struct CompiledControlSet {
    pub control_pack_sha256: String,
    pub positive: Vec<CompiledProtocolRun>,
    pub negative: Vec<CompiledProtocolRun>,
}

/// Redacted, worker-safe blinded control separation summary embedded in receipts.
/// Intentionally omits hidden control IDs, SMILES/sequences, and slot values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlindedControlSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_pack_sha256: Option<String>,
    pub positive_total: u32,
    pub positive_passed: u32,
    pub negative_total: u32,
    /// Number of negative controls that passed sanity gates AND were properly rejected
    /// by the protocol's acceptance criteria (true negatives).
    pub negative_properly_rejected: u32,
    pub all_controls_passed: bool,
}

impl BlindedControlSummary {
    pub fn not_configured() -> Self {
        Self {
            control_pack_sha256: None,
            positive_total: 0,
            positive_passed: 0,
            negative_total: 0,
            negative_properly_rejected: 0,
            all_controls_passed: true,
        }
    }
}

/// Typed diagnostic value parsed by a broker-owned deterministic parser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum DiagnosticValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

impl DiagnosticValue {
    pub fn as_finite_f64(&self) -> Option<f64> {
        match self {
            Self::Float(v) if v.is_finite() => Some(if *v == 0.0 { 0.0 } else { *v }),
            Self::Int(i) => Some(*i as f64),
            _ => None,
        }
    }

    fn validate(&self) -> Result<()> {
        match self {
            Self::Bool(_) | Self::Int(_) => Ok(()),
            Self::Float(v) => {
                canonical_f64(*v)?;
                Ok(())
            }
            Self::Text(s) => {
                ensure!(
                    s.len() <= 2048 && !has_forbidden_control_or_template_chars(s, false),
                    "invalid diagnostic text value"
                );
                Ok(())
            }
        }
    }
}

/// Structured diagnostics produced exclusively by a broker-owned parser.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParsedDiagnostics {
    pub metrics: BTreeMap<String, DiagnosticValue>,
}

impl ParsedDiagnostics {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.metrics.len() <= MAX_DIAGNOSTICS,
            "diagnostic metric count exceeds cap"
        );
        for (name, val) in &self.metrics {
            ensure!(
                is_safe_id(name, 128),
                "invalid diagnostic metric {:?}",
                name
            );
            val.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateFailure {
    pub rule_id: String,
    pub metric: String,
    pub reason: String,
}

/// Evaluate all mandatory sanity gates. Any missing/non-finite/violated metric fails closed.
pub fn evaluate_sanity_gates(
    protocol: &ProtocolSpec,
    diagnostics: &ParsedDiagnostics,
) -> Vec<GateFailure> {
    evaluate_rules(&protocol.sanity_gates, diagnostics)
}

/// Evaluate target acceptance criteria (callers must gate on `evaluate_sanity_gates` first).
pub fn evaluate_acceptance_criteria(
    protocol: &ProtocolSpec,
    diagnostics: &ParsedDiagnostics,
) -> Vec<GateFailure> {
    evaluate_rules(&protocol.acceptance_criteria, diagnostics)
}

fn evaluate_rules(rules: &[NamedMetricRule], diagnostics: &ParsedDiagnostics) -> Vec<GateFailure> {
    let mut failures = Vec::new();
    for named in rules {
        if let Err(reason) = named.rule.evaluate(diagnostics) {
            failures.push(GateFailure {
                rule_id: named.rule_id.clone(),
                metric: named.rule.metric().to_owned(),
                reason,
            });
        }
    }
    failures
}

/// Deterministic verdict sealed by the broker for a protocol run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolVerdict {
    Passed,
    Rejected,
    SanityGateFailed,
    ControlDriftDetected,
    Disagreement,
    VerifierError,
}

impl ProtocolVerdict {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Rejected => "rejected",
            Self::SanityGateFailed => "sanity_gate_failed",
            Self::ControlDriftDetected => "control_drift_detected",
            Self::Disagreement => "disagreement",
            Self::VerifierError => "verifier_error",
        }
    }
}

impl std::str::FromStr for ProtocolVerdict {
    type Err = ai4math_core::ParseEnumError;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "passed" => Ok(Self::Passed),
            "rejected" => Ok(Self::Rejected),
            "sanity_gate_failed" => Ok(Self::SanityGateFailed),
            "control_drift_detected" => Ok(Self::ControlDriftDetected),
            "disagreement" => Ok(Self::Disagreement),
            "verifier_error" => Ok(Self::VerifierError),
            _ => Err(ai4math_core::ParseEnumError {
                kind: "ProtocolVerdict",
                value: value.to_owned(),
            }),
        }
    }
}

/// Per-verifier execution report captured inside a sealed protocol receipt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifierReport {
    pub backend_id: String,
    pub verifier_sha256: String,
    pub elapsed_seconds: f64,
    pub sanity_passed: bool,
    pub acceptance_passed: bool,
    pub sanity_failures: Vec<GateFailure>,
    pub acceptance_failures: Vec<GateFailure>,
    pub diagnostics: ParsedDiagnostics,
}

/// Pure orthogonal agreement check across N >= 2 independent verifiers.
/// Returns a list of concrete disagreement explanations (empty = consensus).
pub fn evaluate_orthogonal_agreement(
    policy: &OrthogonalPolicy,
    reports: &[VerifierReport],
) -> Vec<String> {
    let mut conflicts = Vec::new();
    if reports.len() < usize::from(policy.min_verifiers) {
        conflicts.push(format!(
            "insufficient verifiers: got {}, require {}",
            reports.len(),
            policy.min_verifiers
        ));
        return conflicts;
    }
    if policy.require_distinct_backends && reports.len() > 1 {
        let distinct_backends: BTreeSet<&str> =
            reports.iter().map(|r| r.backend_id.as_str()).collect();
        let distinct_digests: BTreeSet<&str> =
            reports.iter().map(|r| r.verifier_sha256.as_str()).collect();
        if distinct_backends.len() < reports.len() || distinct_digests.len() < reports.len() {
            conflicts.push(
                "orthogonal policy requires distinct backend identities and verifier digests"
                    .to_owned(),
            );
        }
    }
    let any_sanity_pass = reports.iter().any(|r| r.sanity_passed);
    let any_sanity_fail = reports.iter().any(|r| !r.sanity_passed);
    if any_sanity_pass && any_sanity_fail {
        conflicts
            .push("verifiers disagreed on mandatory physical/numerical sanity gates".to_owned());
    }
    if reports.iter().all(|r| r.sanity_passed) {
        let any_accept = reports.iter().any(|r| r.acceptance_passed);
        let any_reject = reports.iter().any(|r| !r.acceptance_passed);
        if any_accept && any_reject {
            conflicts.push("verifiers disagreed on frozen acceptance criteria verdict".to_owned());
        }
        for (metric, max_delta) in &policy.metric_tolerances {
            let mut vals = Vec::with_capacity(reports.len());
            let mut missing = false;
            for r in reports {
                match r
                    .diagnostics
                    .metrics
                    .get(metric)
                    .and_then(DiagnosticValue::as_finite_f64)
                {
                    Some(v) => vals.push(v),
                    None => {
                        missing = true;
                        break;
                    }
                }
            }
            if missing {
                conflicts.push(format!(
                    "metric {metric:?} missing or non-numeric across orthogonal verifiers"
                ));
                continue;
            }
            let min_v = vals.iter().copied().fold(f64::INFINITY, f64::min);
            let max_v = vals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            let spread = max_v - min_v;
            if !spread.is_finite() || spread > *max_delta {
                conflicts.push(format!(
                    "metric {metric:?} inter-verifier spread {spread:.6} exceeds tolerance {max_delta:.6}"
                ));
            }
        }
    }
    conflicts
}

/// Multiple-testing adjustment rule for pre-registered hypothesis families.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CorrectionMethod {
    None,
    Bonferroni,
}

impl CorrectionMethod {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Bonferroni => "bonferroni",
        }
    }
}

impl std::str::FromStr for CorrectionMethod {
    type Err = ai4math_core::ParseEnumError;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "none" => Ok(Self::None),
            "bonferroni" => Ok(Self::Bonferroni),
            _ => Err(ai4math_core::ParseEnumError {
                kind: "CorrectionMethod",
                value: value.to_owned(),
            }),
        }
    }
}

/// Immutable pre-registration commitment binding a hypothesis family to a frozen
/// protocol, trial budget, primary endpoint, and multiplicity correction rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreRegistrationSpec {
    pub version: u8,
    pub prereg_id: String,
    pub hypothesis_family_id: String,
    pub protocol_sha256: String,
    pub primary_metric: String,
    pub max_trials: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub significance_alpha: Option<f64>,
    pub correction: CorrectionMethod,
    pub stopping_rule: String,
}

impl PreRegistrationSpec {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_PREREG_BYTES,
            "pre-registration spec exceeds byte cap"
        );
        let spec: Self =
            serde_json::from_slice(bytes).context("expected a typed JSON PreRegistrationSpec")?;
        spec.validate()?;
        Ok(spec)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "unsupported PreRegistrationSpec version");
        ensure!(is_safe_id(&self.prereg_id, 128), "invalid prereg_id");
        ensure!(
            is_safe_id(&self.hypothesis_family_id, 128),
            "invalid hypothesis_family_id"
        );
        ensure!(
            is_sha256_hex(&self.protocol_sha256),
            "pre-registration protocol_sha256 must be a 64-hex SHA-256 digest"
        );
        ensure!(
            is_safe_id(&self.primary_metric, 128),
            "invalid primary_metric"
        );
        ensure!(
            (1..=MAX_TRIALS_LIMIT).contains(&self.max_trials),
            "max_trials must be in 1..={MAX_TRIALS_LIMIT}"
        );
        if let Some(alpha) = self.significance_alpha {
            let alpha = canonical_f64(alpha)?;
            ensure!(
                alpha > 0.0 && alpha < 1.0,
                "significance_alpha must lie strictly within (0, 1)"
            );
        }
        ensure!(
            !self.stopping_rule.trim().is_empty()
                && self.stopping_rule.len() <= 1024
                && !has_forbidden_control_or_template_chars(&self.stopping_rule, false),
            "invalid stopping_rule"
        );
        Ok(())
    }

    pub fn fingerprint(&self) -> Result<String> {
        self.validate()?;
        sha256_json("ai4math:protocol-preregistration:v1", self)
    }
}

/// Policy thresholds for deterministic Protocol-Smell / Goodhart drift detection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolSmellPolicy {
    pub min_trials_for_sanity_smell: u32,
    pub max_sanity_failure_rate: f64,
    pub min_trials_for_disagreement_smell: u32,
    pub max_disagreement_rate: f64,
    pub min_surrogate_paired_trials: u32,
    pub min_surrogate_physical_pass_rate: f64,
}

impl Default for ProtocolSmellPolicy {
    fn default() -> Self {
        Self {
            min_trials_for_sanity_smell: 3,
            max_sanity_failure_rate: 0.50,
            min_trials_for_disagreement_smell: 2,
            max_disagreement_rate: 0.40,
            min_surrogate_paired_trials: 4,
            min_surrogate_physical_pass_rate: 0.25,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProtocolSmellSignal {
    ControlDrift {
        control_drift_trials: u32,
    },
    SystematicSanityFailure {
        sanity_failed_trials: u32,
        total_trials: u32,
        failure_rate: f64,
    },
    FrequentDisagreement {
        disagreement_trials: u32,
        total_trials: u32,
        disagreement_rate: f64,
    },
    SurrogateGoodhartCollapse {
        surrogate_passed_trials: u32,
        physical_passed_trials: u32,
        physical_conversion_rate: f64,
    },
}

/// Minimal immutable trial sample used to compute audit summaries & drift signals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrialSample {
    pub verdict: ProtocolVerdict,
    /// Optional indicator of whether a fast surrogate (`E1`) had passed this candidate
    /// before it was escalated to this physical protocol.
    pub surrogate_passed: Option<bool>,
}

/// Honest, append-only statistical and drift audit over all recorded trials for a
/// pre-registered hypothesis family (prevents silent p-hacking / cherry-picking).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrialAuditSummary {
    pub prereg_id: String,
    pub protocol_sha256: String,
    pub max_trials: u32,
    pub trials_consumed: u32,
    pub passed_trials: u32,
    pub rejected_trials: u32,
    pub sanity_failed_trials: u32,
    pub control_drift_trials: u32,
    pub disagreement_trials: u32,
    pub verifier_error_trials: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nominal_alpha: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_alpha: Option<f64>,
    pub selection_bias_warning: bool,
    pub smell_signals: Vec<ProtocolSmellSignal>,
}

/// Compute honest cumulative trial accounting, multiplicity-adjusted alpha, and
/// deterministic protocol-smell / Goodhart collapse alerts.
pub fn compute_trial_audit(
    prereg: &PreRegistrationSpec,
    samples: &[TrialSample],
    policy: &ProtocolSmellPolicy,
) -> TrialAuditSummary {
    let trials_consumed = u32::try_from(samples.len()).unwrap_or(u32::MAX);
    let mut passed_trials = 0u32;
    let mut rejected_trials = 0u32;
    let mut sanity_failed_trials = 0u32;
    let mut control_drift_trials = 0u32;
    let mut disagreement_trials = 0u32;
    let mut verifier_error_trials = 0u32;
    let mut surrogate_passed_trials = 0u32;
    let mut surrogate_and_physical_passed = 0u32;

    for sample in samples {
        match sample.verdict {
            ProtocolVerdict::Passed => passed_trials += 1,
            ProtocolVerdict::Rejected => rejected_trials += 1,
            ProtocolVerdict::SanityGateFailed => sanity_failed_trials += 1,
            ProtocolVerdict::ControlDriftDetected => control_drift_trials += 1,
            ProtocolVerdict::Disagreement => disagreement_trials += 1,
            ProtocolVerdict::VerifierError => verifier_error_trials += 1,
        }
        if sample.surrogate_passed == Some(true) {
            surrogate_passed_trials += 1;
            if sample.verdict == ProtocolVerdict::Passed {
                surrogate_and_physical_passed += 1;
            }
        }
    }

    let effective_alpha = prereg
        .significance_alpha
        .map(|alpha| match prereg.correction {
            CorrectionMethod::None => alpha,
            CorrectionMethod::Bonferroni => alpha / f64::from(trials_consumed.max(1)),
        });

    let selection_bias_warning =
        trials_consumed > 1 && passed_trials > 0 && passed_trials < trials_consumed;

    let mut smell_signals = Vec::new();
    if control_drift_trials > 0 {
        smell_signals.push(ProtocolSmellSignal::ControlDrift {
            control_drift_trials,
        });
    }
    if trials_consumed >= policy.min_trials_for_sanity_smell && trials_consumed > 0 {
        let rate = f64::from(sanity_failed_trials) / f64::from(trials_consumed);
        if rate >= policy.max_sanity_failure_rate {
            smell_signals.push(ProtocolSmellSignal::SystematicSanityFailure {
                sanity_failed_trials,
                total_trials: trials_consumed,
                failure_rate: rate,
            });
        }
    }
    if trials_consumed >= policy.min_trials_for_disagreement_smell && trials_consumed > 0 {
        let rate = f64::from(disagreement_trials) / f64::from(trials_consumed);
        if rate >= policy.max_disagreement_rate {
            smell_signals.push(ProtocolSmellSignal::FrequentDisagreement {
                disagreement_trials,
                total_trials: trials_consumed,
                disagreement_rate: rate,
            });
        }
    }
    if surrogate_passed_trials >= policy.min_surrogate_paired_trials {
        let rate = f64::from(surrogate_and_physical_passed) / f64::from(surrogate_passed_trials);
        if rate < policy.min_surrogate_physical_pass_rate {
            smell_signals.push(ProtocolSmellSignal::SurrogateGoodhartCollapse {
                surrogate_passed_trials,
                physical_passed_trials: surrogate_and_physical_passed,
                physical_conversion_rate: rate,
            });
        }
    }

    TrialAuditSummary {
        prereg_id: prereg.prereg_id.clone(),
        protocol_sha256: prereg.protocol_sha256.clone(),
        max_trials: prereg.max_trials,
        trials_consumed,
        passed_trials,
        rejected_trials,
        sanity_failed_trials,
        control_drift_trials,
        disagreement_trials,
        verifier_error_trials,
        nominal_alpha: prereg.significance_alpha,
        effective_alpha,
        selection_bias_warning,
        smell_signals,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_compchem_protocol() -> ProtocolSpec {
        let mut pinned_inputs = BTreeMap::new();
        pinned_inputs.insert("receptor_pdb".into(), "a".repeat(64));
        pinned_inputs.insert("force_field".into(), "b".repeat(64));

        let mut frozen_parameters = BTreeMap::new();
        frozen_parameters.insert("temp_k".into(), ParamValue::Float(300.0));
        frozen_parameters.insert("production_restraints".into(), ParamValue::Bool(false));

        ProtocolSpec {
            version: 1,
            protocol_id: "compchem_fep_v1".into(),
            domain_id: "compchem".into(),
            backend_id: "openmm_fep".into(),
            epistemic_ceiling: ScientificTrust::E4a,
            default_debts: vec!["classical_ff_only".into(), "fixed_protonation".into()],
            pinned_inputs,
            frozen_parameters,
            slots: vec![
                SlotSpec {
                    name: "ligand_smiles".into(),
                    kind: SlotKind::Text {
                        max_len: 256,
                        profile: TextProfile::Smiles,
                    },
                },
                SlotSpec {
                    name: "net_charge".into(),
                    kind: SlotKind::Int { min: -4, max: 4 },
                },
            ],
            artifact_templates: vec![ArtifactTemplate {
                path: "run/fep.cfg".into(),
                template: "temp={{param:temp_k}}\nrestraints={{param:production_restraints}}\nsmiles={{slot:ligand_smiles}}\ncharge={{slot:net_charge}}\n".into(),
            }],
            sanity_gates: vec![
                NamedMetricRule {
                    rule_id: "no_lincs_warnings".into(),
                    rule: MetricRule::EqualsInt {
                        metric: "lincs_warnings".into(),
                        expected: 0,
                    },
                },
                NamedMetricRule {
                    rule_id: "no_prod_restraints".into(),
                    rule: MetricRule::EqualsBool {
                        metric: "production_restraints_active".into(),
                        expected: false,
                    },
                },
                NamedMetricRule {
                    rule_id: "fep_overlap".into(),
                    rule: MetricRule::RangeFloat {
                        metric: "min_window_overlap".into(),
                        min: 0.03,
                        max: 1.0,
                    },
                },
            ],
            acceptance_criteria: vec![
                NamedMetricRule {
                    rule_id: "binding_ddg".into(),
                    rule: MetricRule::MaxFloat {
                        metric: "ddg_kcal_mol".into(),
                        max: -1.5,
                    },
                },
                NamedMetricRule {
                    rule_id: "uncertainty_cap".into(),
                    rule: MetricRule::MaxFloat {
                        metric: "ddg_stderr_kcal_mol".into(),
                        max: 0.35,
                    },
                },
            ],
            orthogonal_policy: OrthogonalPolicy::default(),
            control_policy: BlindedControlPolicy::default(),
            preregistration_required: true,
        }
    }

    #[test]
    fn compiles_typed_submission_and_blocks_drift_and_injection() -> Result<()> {
        let protocol = sample_compchem_protocol();
        let hash = protocol.protocol_sha256()?;

        let mut good_slots = BTreeMap::new();
        good_slots.insert(
            "ligand_smiles".into(),
            SlotValue::Text("CC(=O)Oc1ccccc1C(=O)O".into()),
        );
        good_slots.insert("net_charge".into(), SlotValue::Int(0));
        let good = CandidateSubmission {
            version: 1,
            protocol_sha256: hash.clone(),
            candidate_id: "aspirin_01".into(),
            slot_values: good_slots,
        };
        let compiled = protocol.compile_submission(&good)?;
        let cfg = compiled.artifacts().get("run/fep.cfg").unwrap();
        assert_eq!(
            cfg,
            "temp=300\nrestraints=false\nsmiles=CC(=O)Oc1ccccc1C(=O)O\ncharge=0\n"
        );

        // 1. Parameter injection via extra undeclared slot is rejected.
        let mut extra = good.clone();
        extra
            .slot_values
            .insert("production_restraints".into(), SlotValue::Bool(true));
        assert!(protocol.compile_submission(&extra).is_err());

        // 2. Newline injection inside SMILES to sneak restraints into config is rejected.
        let mut injected = good.clone();
        injected.slot_values.insert(
            "ligand_smiles".into(),
            SlotValue::Text("CCO\nrestraints=true".into()),
        );
        assert!(protocol.compile_submission(&injected).is_err());

        // 3. Template placeholder injection is rejected.
        let mut placeholder = good.clone();
        placeholder.slot_values.insert(
            "ligand_smiles".into(),
            SlotValue::Text("{{param:temp_k}}".into()),
        );
        assert!(protocol.compile_submission(&placeholder).is_err());

        // 4. Cross-protocol replay with wrong protocol_sha256 is rejected.
        let mut wrong_hash = good.clone();
        wrong_hash.protocol_sha256 = "0".repeat(64);
        assert!(protocol.compile_submission(&wrong_hash).is_err());

        // 5. Out-of-bounds integer slot is rejected.
        let mut bad_charge = good;
        bad_charge
            .slot_values
            .insert("net_charge".into(), SlotValue::Int(99));
        assert!(protocol.compile_submission(&bad_charge).is_err());
        Ok(())
    }

    #[test]
    fn sanity_gates_fail_closed_on_missing_or_nan_or_unphysical_metrics() -> Result<()> {
        let protocol = sample_compchem_protocol();
        let mut diag = ParsedDiagnostics::default();
        diag.metrics
            .insert("lincs_warnings".into(), DiagnosticValue::Int(0));
        diag.metrics.insert(
            "production_restraints_active".into(),
            DiagnosticValue::Bool(false),
        );
        diag.metrics
            .insert("min_window_overlap".into(), DiagnosticValue::Float(0.12));
        diag.metrics
            .insert("ddg_kcal_mol".into(), DiagnosticValue::Float(-2.4));
        diag.metrics
            .insert("ddg_stderr_kcal_mol".into(), DiagnosticValue::Float(0.2));
        diag.validate()?;
        assert!(evaluate_sanity_gates(&protocol, &diag).is_empty());
        assert!(evaluate_acceptance_criteria(&protocol, &diag).is_empty());

        // Missing overlap metric fails closed.
        let mut missing = diag.clone();
        missing.metrics.remove("min_window_overlap");
        assert_eq!(evaluate_sanity_gates(&protocol, &missing).len(), 1);

        // NaN fails validation and sanity evaluation.
        let mut nan_diag = diag.clone();
        nan_diag.metrics.insert(
            "min_window_overlap".into(),
            DiagnosticValue::Float(f64::NAN),
        );
        assert!(nan_diag.validate().is_err());
        assert_eq!(evaluate_sanity_gates(&protocol, &nan_diag).len(), 1);

        // Secret production restraint trips sanity gate even with great ddG.
        let mut restrained = diag;
        restrained.metrics.insert(
            "production_restraints_active".into(),
            DiagnosticValue::Bool(true),
        );
        assert_eq!(evaluate_sanity_gates(&protocol, &restrained).len(), 1);
        Ok(())
    }

    #[test]
    fn e4b_requires_orthogonal_verifiers_and_detects_disagreement() -> Result<()> {
        let mut protocol = sample_compchem_protocol();
        protocol.epistemic_ceiling = ScientificTrust::E4b;
        assert!(protocol.validate().is_err());

        protocol.orthogonal_policy = OrthogonalPolicy {
            min_verifiers: 2,
            require_distinct_backends: true,
            metric_tolerances: BTreeMap::from([("ddg_kcal_mol".into(), 0.5)]),
        };
        protocol.validate()?;

        let mut d1 = ParsedDiagnostics::default();
        d1.metrics
            .insert("ddg_kcal_mol".into(), DiagnosticValue::Float(-2.0));
        let mut d2 = ParsedDiagnostics::default();
        d2.metrics
            .insert("ddg_kcal_mol".into(), DiagnosticValue::Float(-3.2));

        let r1 = VerifierReport {
            backend_id: "amber_fep".into(),
            verifier_sha256: "1".repeat(64),
            elapsed_seconds: 1.0,
            sanity_passed: true,
            acceptance_passed: true,
            sanity_failures: vec![],
            acceptance_failures: vec![],
            diagnostics: d1,
        };
        let r2 = VerifierReport {
            backend_id: "charmm_fep".into(),
            verifier_sha256: "2".repeat(64),
            elapsed_seconds: 1.2,
            sanity_passed: true,
            acceptance_passed: true,
            sanity_failures: vec![],
            acceptance_failures: vec![],
            diagnostics: d2,
        };
        let conflicts = evaluate_orthogonal_agreement(&protocol.orthogonal_policy, &[r1, r2]);
        assert!(conflicts.iter().any(|c| c.contains("exceeds tolerance")));
        Ok(())
    }

    #[test]
    fn blinded_control_pack_verifies_merkle_commitment_and_rejects_tampering() -> Result<()> {
        let mut protocol = sample_compchem_protocol();
        let pack = BlindedControlPack {
            version: 1,
            protocol_id: protocol.protocol_id.clone(),
            positive_controls: vec![BlindedControlCase {
                control_id: "pos_known_binder".into(),
                slot_values: BTreeMap::from([
                    ("ligand_smiles".into(), SlotValue::Text("c1ccccc1O".into())),
                    ("net_charge".into(), SlotValue::Int(0)),
                ]),
            }],
            negative_controls: vec![BlindedControlCase {
                control_id: "neg_decoy_alkane".into(),
                slot_values: BTreeMap::from([
                    ("ligand_smiles".into(), SlotValue::Text("CCCCCC".into())),
                    ("net_charge".into(), SlotValue::Int(0)),
                ]),
            }],
        };
        let pack_hash = pack.fingerprint()?;
        protocol.control_policy = BlindedControlPolicy {
            control_pack_sha256: Some(pack_hash.clone()),
            min_positive_controls: 1,
            min_negative_controls: 1,
        };
        let compiled = pack.compile_for(&protocol)?;
        assert_eq!(compiled.control_pack_sha256, pack_hash);
        assert_eq!(compiled.positive.len(), 1);
        assert_eq!(compiled.negative.len(), 1);

        // Tampering with a hidden negative control invalidates the commitment.
        let mut tampered = pack;
        tampered.negative_controls[0]
            .slot_values
            .insert("ligand_smiles".into(), SlotValue::Text("c1ccccc1O".into()));
        assert!(tampered.compile_for(&protocol).is_err());
        Ok(())
    }

    #[test]
    fn trial_audit_computes_bonferroni_alpha_selection_bias_and_goodhart_smells() -> Result<()> {
        let protocol = sample_compchem_protocol();
        let prereg = PreRegistrationSpec {
            version: 1,
            prereg_id: "PREREG-001".into(),
            hypothesis_family_id: "kinase_series_a".into(),
            protocol_sha256: protocol.protocol_sha256()?,
            primary_metric: "ddg_kcal_mol".into(),
            max_trials: 100,
            significance_alpha: Some(0.05),
            correction: CorrectionMethod::Bonferroni,
            stopping_rule: "stop after 5 validated hits or 100 trials".into(),
        };
        let samples = vec![
            TrialSample {
                verdict: ProtocolVerdict::SanityGateFailed,
                surrogate_passed: Some(true),
            },
            TrialSample {
                verdict: ProtocolVerdict::SanityGateFailed,
                surrogate_passed: Some(true),
            },
            TrialSample {
                verdict: ProtocolVerdict::Rejected,
                surrogate_passed: Some(true),
            },
            TrialSample {
                verdict: ProtocolVerdict::Rejected,
                surrogate_passed: Some(true),
            },
            TrialSample {
                verdict: ProtocolVerdict::Passed,
                surrogate_passed: Some(true),
            },
        ];
        let audit = compute_trial_audit(&prereg, &samples, &ProtocolSmellPolicy::default());
        assert_eq!(audit.trials_consumed, 5);
        assert_eq!(audit.passed_trials, 1);
        assert!((audit.effective_alpha.unwrap() - 0.01).abs() < 1e-12);
        assert!(audit.selection_bias_warning);
        assert!(audit.smell_signals.iter().any(|s| matches!(
            s,
            ProtocolSmellSignal::SurrogateGoodhartCollapse {
                surrogate_passed_trials: 5,
                physical_passed_trials: 1,
                ..
            }
        )));
        Ok(())
    }
}
