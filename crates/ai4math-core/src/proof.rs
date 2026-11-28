//! Render a proof expression into an operator-supplied trailing slot. This is a
//! deliberately restricted source profile, not a prover parser or sandbox.

use crate::preflight::{Token, TokenKind, lex, rocq_ident, rocq_qualified_dot, rocq_term};
use crate::{Engine, PreFlightViolation, scan_source, validate_source_path};
use serde::{Deserialize, Serialize};

const MAX_BODY_BYTES: usize = 256 * 1024;
const MAX_RENDERED_BYTES: usize = 2 * 1024 * 1024;
const MAX_SLOT_INDENT: usize = 64;
const SLOT: &str = "{{PROOF_BODY}}";

/// The broker supplies these bytes from the trusted base, not from a worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenTarget {
    pub engine: Engine,
    pub target_id: String,
    pub file_path: String,
    pub prefix: String,
    pub suffix: String,
}

/// A worker can submit only the RHS proof expression for one frozen target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProofBodySubmission {
    pub target_id: String,
    pub proof_body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProofBodyError {
    TargetMismatch,
    UnsupportedEngine(Engine),
    InvalidTemplate(&'static str),
    InvalidBody {
        line: Option<usize>,
        detail: &'static str,
    },
    PreFlight(PreFlightViolation),
}

impl std::fmt::Display for ProofBodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TargetMismatch => f.write_str("submission does not name the frozen target"),
            Self::UnsupportedEngine(engine) => write!(f, "{engine} proof slots are not supported"),
            Self::InvalidTemplate(detail) => write!(f, "invalid frozen proof slot: {detail}"),
            Self::InvalidBody { line, detail } => {
                f.write_str("invalid proof body")?;
                if let Some(line) = line {
                    write!(f, " at line {line}")?;
                }
                write!(f, ": {detail}")
            }
            Self::PreFlight(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ProofBodyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::PreFlight(e) => Some(e),
            _ => None,
        }
    }
}

fn ident(name: &str) -> bool {
    name.starts_with(|c: char| c == '_' || c.is_ascii_alphabetic())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'\''))
        && name != "_"
}

fn indent(source: &str, t: &Token<'_>) -> Option<usize> {
    let prefix = source[..t.start].rsplit('\n').next().unwrap_or("");
    prefix.bytes().all(|b| b == b' ').then_some(prefix.len())
}

fn end(token: &Token<'_>) -> usize {
    token.start + token.text.len()
}

fn opening(t: &str) -> Option<&'static str> {
    match t {
        "(" => Some(")"),
        "[" => Some("]"),
        "{" => Some("}"),
        "⟨" => Some("⟩"),
        _ => None,
    }
}

fn step_delimiters(stack: &mut Vec<&'static str>, t: &Token<'_>) -> bool {
    if let Some(close) = opening(t.text) {
        stack.push(close);
        true
    } else if matches!(t.text, ")" | "]" | "}" | "⟩") {
        stack.pop() == Some(t.text)
    } else {
        true
    }
}

fn closed_slice(tokens: &[Token<'_>]) -> bool {
    let mut stack = Vec::new();
    tokens.iter().all(|t| step_delimiters(&mut stack, t)) && stack.is_empty()
}

fn template_slot<'a>(
    target: &'a FrozenTarget,
    module: &str,
) -> Result<(usize, &'a str), ProofBodyError> {
    use ProofBodyError::InvalidTemplate as Bad;
    let prefix = &target.prefix;
    let suffix = &target.suffix;
    if prefix.contains(SLOT)
        || !suffix
            .bytes()
            .all(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
    {
        return Err(Bad(
            "expected one trailing slot with only whitespace after it",
        ));
    }
    let tokens = lex(target.engine, prefix).map_err(|e| Bad(e.detail))?;
    if !closed_slice(&tokens) {
        return Err(Bad("unbalanced delimiters in trusted prefix"));
    }
    let last = tokens.last().ok_or(Bad("missing trusted declaration"))?;
    let gap = &prefix[end(last)..];
    if !gap
        .bytes()
        .all(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
    {
        return Err(Bad(
            "only whitespace may separate the assignment and the slot",
        ));
    }
    let line_start = tokens.partition_point(|t| t.line < last.line);
    let line = &tokens[line_start..];
    let lead = indent(prefix, &line[0]).ok_or(Bad("expected assignment on its own source line"))?;
    if lead > MAX_SLOT_INDENT {
        return Err(Bad("trusted slot exceeds the indentation limit"));
    }
    let id = target.target_id.as_str();
    match target.engine {
        Engine::Lean4 => {
            if id.starts_with("_root_.") || !id.split('.').all(ident) {
                return Err(Bad("expected a conventional Lean theorem name"));
            }
            // Phase one has no namespace parser. At root, a dotted declaration
            // name may be written explicitly and must match the exact target ID.
            if lead != 0
                || line[0].text != "theorem"
                || tokens[..line_start].iter().any(|t| t.text == "namespace")
                || last.text != ":="
            {
                return Err(Bad("expected a root-level theorem header ending in :="));
            }
            let name = line.get(1).ok_or(Bad("missing theorem name"))?;
            if !prefix[name.start..].starts_with(id) {
                return Err(Bad("the theorem name does not match the target ID"));
            }
            let name_end = name.start + id.len();
            let header_start = line.partition_point(|t| t.start < name_end);
            if end(&line[header_start - 1]) != name_end {
                return Err(Bad("the theorem name does not match the target ID"));
            }
            let header = &line[header_start..line.len() - 1];
            if !header
                .first()
                .is_some_and(|t| matches!(t.text, ":" | "(" | "{" | "["))
            {
                return Err(Bad("expected a theorem header with an explicit type"));
            }
            let mut stack = Vec::new();
            let mut typed = false;
            for (i, t) in header.iter().enumerate() {
                if stack.is_empty() {
                    if t.text == ":=" {
                        return Err(Bad("the terminal assignment must begin the proof"));
                    }
                    typed |= t.text == ":" && i + 1 < header.len();
                }
                if !step_delimiters(&mut stack, t) {
                    return Err(Bad("unbalanced delimiters in theorem header"));
                }
            }
            if !typed || !stack.is_empty() {
                return Err(Bad(
                    "expected a balanced theorem header with an explicit type",
                ));
            }
        }
        Engine::Agda => {
            if !ident(id)
                || line.len() < 2
                || line[0].text != id
                || last.text != "="
                || !line[1..line.len() - 1].iter().all(|t| ident(t.text))
            {
                return Err(Bad(
                    "expected a trailing Agda equation with only simple pattern names",
                ));
            }
            let mut prior_lines = tokens[..line_start].chunk_by(|a, b| a.line == b.line);
            let module_header = prior_lines.next();
            if !module_header.is_some_and(|l| {
                indent(prefix, &l[0]) == Some(0)
                    && l.len() == 3
                    && l[0].text == "module"
                    && l[1].text == module
                    && l[2].text == "where"
            }) {
                return Err(Bad(
                    "expected the root Agda module header to match the file name",
                ));
            }
            let mut signatures = 0;
            for earlier in prior_lines {
                if earlier[0].text == "module" || earlier[0].text == "mutual" {
                    return Err(Bad(
                        "nested Agda modules and mutual blocks are not supported in a frozen slot",
                    ));
                }
                if indent(prefix, &earlier[0]) == Some(lead)
                    && earlier[0].text == id
                    && earlier.get(1).is_some_and(|t| t.text == ":")
                    && earlier.len() >= 3
                {
                    signatures += 1;
                }
            }
            if signatures != 1 {
                return Err(Bad(
                    "expected one matching, explicit Agda type signature at the equation's indentation",
                ));
            }
        }
        Engine::Rocq => return Err(ProofBodyError::UnsupportedEngine(target.engine)),
    }
    Ok((lead, gap))
}

fn rocq_template_slot(target: &FrozenTarget) -> Result<(), ProofBodyError> {
    use ProofBodyError::InvalidTemplate as Bad;
    let whitespace = |s: &str| s.bytes().all(|b| b.is_ascii_whitespace());
    if !rocq_ident(&target.target_id)
        || target.prefix.contains(SLOT)
        || target.suffix.contains(SLOT)
    {
        return Err(Bad(
            "expected a single root Rocq theorem name and exactly one slot",
        ));
    }
    let tokens = lex(Engine::Rocq, &target.prefix).map_err(|e| Bad(e.detail))?;
    let [.., dot, proof, proof_dot, exact, open] = tokens.as_slice() else {
        return Err(Bad("missing the frozen Rocq theorem and proof wrapper"));
    };
    if [dot.text, proof.text, proof_dot.text, exact.text, open.text]
        != [".", "Proof", ".", "exact", "("]
        || !whitespace(&target.prefix[end(open)..])
        || rocq_qualified_dot(&tokens, tokens.len() - 5)
        || rocq_qualified_dot(&tokens, tokens.len() - 3)
    {
        return Err(Bad(
            "Rocq slot must follow the frozen Proof. exact ( wrapper",
        ));
    }
    let header_end = tokens.len() - 5;
    let header_start = (0..header_end)
        .rfind(|&i| tokens[i].text == "." && !rocq_qualified_dot(&tokens, i))
        .map_or(0, |i| i + 1);
    let header = &tokens[header_start..header_end];
    if !header.first().is_some_and(|t| t.text == "Theorem")
        || !header.get(1).is_some_and(|t| t.text == target.target_id)
    {
        return Err(Bad(
            "expected a final Rocq Theorem with the exact target ID",
        ));
    }
    let suffix = lex(Engine::Rocq, &target.suffix).map_err(|e| Bad(e.detail))?;
    let mut after = 0;
    if !suffix.iter().map(|t| t.text).eq([")", ".", "Qed", "."])
        || !suffix.iter().all(|t| {
            let gap = &target.suffix[after..t.start];
            after = end(t);
            whitespace(gap)
        })
        || !whitespace(&target.suffix[after..])
    {
        return Err(Bad(
            "expected only the frozen ). Qed. suffix after the Rocq slot",
        ));
    }
    Ok(())
}

/// Extract only the validated one-line Lean theorem header, not its body,
/// preceding modules/comments, or writer context. This is a syntactic review
/// input; independent semantic approval requires the dependency context too.
pub fn lean_frozen_header(target: &FrozenTarget) -> Result<&str, ProofBodyError> {
    use ProofBodyError::InvalidTemplate as Bad;
    if target.engine != Engine::Lean4 {
        return Err(ProofBodyError::UnsupportedEngine(target.engine));
    }
    if target.prefix.len().saturating_add(target.suffix.len()) > MAX_RENDERED_BYTES {
        return Err(Bad("trusted prefix exceeds the source size limit"));
    }
    let (_, gap) = template_slot(target, "")?;
    let assignment = target.prefix.len() - gap.len() - ":=".len();
    let header = target.prefix[..assignment]
        .rsplit('\n')
        .next()
        .unwrap_or_default()
        .trim_end();
    if header.contains("/-")
        || header.contains("--")
        || header.chars().any(char::is_control)
        || header.len() > 16 * 1024
    {
        return Err(Bad(
            "reviewable theorem headers must be single-line, bounded, and comment-free",
        ));
    }
    Ok(header)
}

fn body_subset(engine: Engine, body: &str) -> Result<(), ProofBodyError> {
    let bad = |line, detail| ProofBodyError::InvalidBody { line, detail };
    if body.len() > MAX_BODY_BYTES {
        return Err(bad(None, "body exceeds the Phase 1 size limit"));
    }
    if body.contains(SLOT) || body.contains('\t') {
        return Err(bad(
            None,
            "slot markers and tabs are not supported in proof bodies",
        ));
    }
    let tokens = lex(engine, body).map_err(|e| bad(Some(e.line), e.detail))?;
    if tokens.is_empty() {
        return Err(bad(None, "expected a proof expression"));
    }
    if engine == Engine::Rocq {
        return rocq_term(&tokens).map_err(|e| bad(Some(e.line), e.detail));
    }
    let mut stack = Vec::new();
    for (i, t) in tokens.iter().enumerate() {
        let supported = match t.kind {
            TokenKind::Word => match engine {
                Engine::Lean4 => !matches!(
                    t.text,
                    "abbrev"
                        | "attribute"
                        | "class"
                        | "def"
                        | "deriving"
                        | "end"
                        | "example"
                        | "export"
                        | "import"
                        | "include"
                        | "inductive"
                        | "instance"
                        | "lemma"
                        | "local"
                        | "mutual"
                        | "namespace"
                        | "omit"
                        | "opaque"
                        | "open"
                        | "prelude"
                        | "private"
                        | "protected"
                        | "scoped"
                        | "section"
                        | "structure"
                        | "theorem"
                        | "universe"
                        | "universes"
                        | "variable"
                        | "where"
                ),
                Engine::Agda => !matches!(
                    t.text,
                    "abstract"
                        | "constructor"
                        | "data"
                        | "field"
                        | "hiding"
                        | "import"
                        | "inductive"
                        | "infix"
                        | "infixl"
                        | "infixr"
                        | "instance"
                        | "module"
                        | "mutual"
                        | "open"
                        | "pattern"
                        | "postulate"
                        | "private"
                        | "public"
                        | "record"
                        | "renaming"
                        | "rewrite"
                        | "syntax"
                        | "using"
                        | "variable"
                        | "where"
                        | "with"
                ),
                Engine::Rocq => false,
            },
            TokenKind::Symbol => {
                matches!(
                    t.text,
                    "(" | ")"
                        | "["
                        | "]"
                        | "{"
                        | "}"
                        | "⟨"
                        | "⟩"
                        | ":"
                        | ":="
                        | ","
                        | "."
                        | ";"
                        | "="
                        | "|"
                        | "&"
                        | "!"
                        | "?"
                        | "*"
                        | "+"
                        | "-"
                        | "/"
                        | "^"
                        | "%"
                        | "<"
                        | ">"
                        | "~"
                        | "$"
                        | "∀"
                        | "∃"
                        | "∧"
                        | "∨"
                        | "¬"
                        | "→"
                        | "←"
                        | "↔"
                        | "⇒"
                        | "≠"
                        | "≤"
                        | "≥"
                        | "∈"
                        | "∉"
                        | "⊆"
                        | "∪"
                        | "∩"
                        | "×"
                        | "λ"
                        | "·"
                        | "▸"
                        | "∘"
                        | "≡"
                        | "∎"
                ) || (engine == Engine::Lean4
                    && t.text == "@"
                    && tokens.get(i + 1).is_none_or(|next| next.text != "["))
            }
            TokenKind::Literal | TokenKind::Pragma | TokenKind::DocComment => false,
        };
        if !supported {
            return Err(bad(
                Some(t.line),
                "unsupported syntax in a proof expression",
            ));
        }
        if !step_delimiters(&mut stack, t) {
            return Err(bad(Some(t.line), "unbalanced delimiters"));
        }
    }
    if !stack.is_empty() {
        return Err(bad(None, "unbalanced delimiters"));
    }
    Ok(())
}

/// Render one parenthesized proof expression, leaving both trusted spans intact.
/// Both the frozen layout and the candidate source must pass static checks; the
/// operator must still run the result only in the broker's OS sandbox and apply
/// the engine's independent kernel admission policy.
///
/// Rocq requires the final root declaration to be
/// `Theorem id : type. Proof. exact ({{PROOF_BODY}}). Qed.` (binders before
/// the colon are allowed). The frozen prefix and suffix own the parentheses;
/// the submission is a Gallina term, never a tactic script or a declaration.
pub fn render_proof_body(
    target: &FrozenTarget,
    submission: &ProofBodySubmission,
) -> Result<String, ProofBodyError> {
    render_proof_body_with_policy(target, submission, false, false)
}

/// Formal-sketch rendering admits only the explicit `sorry`/`admit` proof-hole
/// constructs. Every other static deny rule remains in force.
pub fn render_proof_body_allow_sorry(
    target: &FrozenTarget,
    submission: &ProofBodySubmission,
) -> Result<String, ProofBodyError> {
    render_proof_body_with_policy(target, submission, true, false)
}

/// Oracle-backed rendering admits only the documented compiler-trust constructs.
/// It never admits proof holes or arbitrary axioms and does not confer kernel trust.
pub fn render_proof_body_allow_oracle(
    target: &FrozenTarget,
    submission: &ProofBodySubmission,
) -> Result<String, ProofBodyError> {
    render_proof_body_with_policy(target, submission, false, true)
}

fn render_proof_body_with_policy(
    target: &FrozenTarget,
    submission: &ProofBodySubmission,
    allow_sorry: bool,
    allow_oracle: bool,
) -> Result<String, ProofBodyError> {
    if target.target_id != submission.target_id {
        return Err(ProofBodyError::TargetMismatch);
    }
    validate_source_path(target.engine, &target.file_path).map_err(ProofBodyError::PreFlight)?;
    let (module, _) = target
        .file_path
        .split_once('.')
        .ok_or(ProofBodyError::InvalidTemplate("missing file extension"))?;
    if !module.starts_with(|c: char| c.is_ascii_uppercase())
        || !module
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(ProofBodyError::InvalidTemplate(
            "expected a conventional root-level module path",
        ));
    }
    let frozen_len = target.prefix.len().saturating_add(target.suffix.len());
    if frozen_len >= MAX_RENDERED_BYTES {
        return Err(ProofBodyError::InvalidTemplate(
            "frozen source exceeds the Phase 1 size limit",
        ));
    }
    let body = &submission.proof_body;
    if target.engine == Engine::Rocq {
        rocq_template_slot(target)?;
        body_subset(target.engine, body)?;
        let rendered_len = frozen_len + body.len() + 2;
        if rendered_len > MAX_RENDERED_BYTES {
            return Err(ProofBodyError::InvalidBody {
                line: None,
                detail: "rendered source exceeds the Phase 1 size limit",
            });
        }
        // The trusted spans own the parentheses. Separating their tokens from
        // the body also prevents a body edge from forming a comment opener.
        let source = format!("{}\n{body}\n{}", target.prefix, target.suffix);
        return checked_source_policy(target, source, allow_sorry, allow_oracle);
    }
    let (lead, gap) = template_slot(target, module)?;
    body_subset(target.engine, body)?;
    let common_indent = body
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start_matches(' ').len())
        .min()
        .unwrap_or(0);
    let padding = " ".repeat(lead + 2);
    let before_open = if gap.contains('\n') {
        padding.as_str()
    } else {
        " "
    };
    let normalized = body
        .lines()
        .map(|line| (!line.trim().is_empty()).then(|| &line[common_indent..]));
    let body_len: usize = normalized
        .clone()
        .map(|line| 1 + line.map_or(0, |line| padding.len() + line.len()))
        .sum();
    let rendered_len = frozen_len + before_open.len() + 2 + body_len + padding.len() + 1;
    if rendered_len > MAX_RENDERED_BYTES {
        return Err(ProofBodyError::InvalidBody {
            line: None,
            detail: "rendered source exceeds the Phase 1 size limit",
        });
    }
    let mut source = String::with_capacity(rendered_len);
    source.push_str(&target.prefix);
    source.push_str(before_open);
    source.push_str("(\n");
    for line in normalized {
        if let Some(line) = line {
            source.push_str(&padding);
            source.push_str(line);
        }
        source.push('\n');
    }
    source.push_str(&padding);
    source.push(')');
    source.push_str(&target.suffix);
    checked_source_policy(target, source, allow_sorry, allow_oracle)
}

fn checked_source_policy(
    target: &FrozenTarget,
    source: String,
    allow_sorry: bool,
    allow_oracle: bool,
) -> Result<String, ProofBodyError> {
    let report = scan_source(target.engine, &target.file_path, &source);
    if let Some(violation) = report.violations.into_iter().find(|v| {
        v.severity == crate::PreFlightSeverity::Deny
            && !(target.engine == Engine::Lean4
                && v.rule_id == "LEAN_TRUST"
                && ((allow_sorry
                    && matches!(v.matched_construct.as_deref(), Some("sorry" | "admit")))
                    || (allow_oracle
                        && matches!(
                            v.matched_construct.as_deref(),
                            Some("native_decide" | "ofReduceBool" | "trustCompiler")
                        ))))
    }) {
        return Err(ProofBodyError::PreFlight(violation));
    }
    Ok(source)
}
