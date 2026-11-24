//! Static admission checks for a single source file. These lexical checks are
//! not a parser, a workspace authorization check, or an execution sandbox.

use crate::{DispatchEngine, Engine, wire_enum};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

wire_enum!(PreFlightSeverity { Deny => "DENY", Warn => "WARN" });

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreFlightViolation {
    pub rule_id: String,
    pub severity: PreFlightSeverity,
    pub file_path: String,
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_construct: Option<String>,
    pub detail: String,
}

impl std::fmt::Display for PreFlightViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} [{}] {}", self.severity, self.rule_id, self.file_path)?;
        if let Some(line) = self.line {
            write!(f, ":{line}")?;
        }
        write!(f, ": {}", self.detail)?;
        if let Some(construct) = &self.matched_construct {
            write!(f, " ({construct:?})")?;
        }
        Ok(())
    }
}
impl std::error::Error for PreFlightViolation {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreFlightScanResult {
    pub passed: bool,
    pub engine: DispatchEngine,
    pub patch_sha256: String,
    #[serde(default)]
    pub authorized_scope_checked: bool,
    pub violations: Vec<PreFlightViolation>,
}

fn deny(
    path: &str,
    rule: &str,
    line: Option<usize>,
    construct: Option<&str>,
    detail: &str,
) -> PreFlightViolation {
    PreFlightViolation {
        rule_id: rule.into(),
        severity: PreFlightSeverity::Deny,
        file_path: path.into(),
        line,
        matched_construct: construct.map(str::to_owned),
        detail: detail.into(),
    }
}

/// Check the lexical spelling of an engine-specific, workspace-relative source
/// path. The caller must independently authorize and resolve the path, including
/// checking file type and symlink policy without a check/use race.
pub fn validate_source_path(engine: Engine, file_path: &str) -> Result<(), PreFlightViolation> {
    let path_error = |detail| deny(file_path, "SOURCE_PATH", None, None, detail);
    let (parent, file) = file_path.rsplit_once('/').unwrap_or(("", file_path));
    let (stem, extension) = file.rsplit_once('.').unwrap_or((file, ""));
    let safe_component = |s: &str| {
        s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    };
    if file_path.starts_with('/')
        || !safe_component(stem)
        || (!parent.is_empty() && !parent.split('/').all(safe_component))
    {
        return Err(path_error(
            "expected a canonical workspace-relative source path",
        ));
    }
    let mut parts = parent.split('/').peekable();
    while let Some(part) = parts.next() {
        if part.eq_ignore_ascii_case("docs")
            && parts.peek().is_some_and(|s| s.eq_ignore_ascii_case("adr"))
        {
            return Err(path_error("protected decision record directory"));
        }
    }
    if matches!(
        file.to_ascii_lowercase().as_str(),
        "lakefile.lean" | "lakefile.toml" | "lean-toolchain" | "_coqproject" | "makefile"
    ) || extension.eq_ignore_ascii_case("agda-lib")
    {
        return Err(path_error("protected build configuration"));
    }
    if !matches!(
        (engine, extension),
        (Engine::Lean4, "lean") | (Engine::Agda, "agda") | (Engine::Rocq, "v")
    ) {
        return Err(path_error("source extension does not match the engine"));
    }
    if engine == Engine::Rocq
        && (!parent.is_empty()
            || !stem.starts_with(|c: char| c.is_ascii_uppercase())
            || !stem.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
    {
        return Err(path_error(
            "Rocq requires a conventional root-level Module.v path",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenKind {
    Word,
    Symbol,
    Literal,
    Pragma,
    DocComment,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Token<'a> {
    pub text: &'a str,
    pub kind: TokenKind,
    pub start: usize,
    pub line: usize,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct LexError {
    pub line: usize,
    pub detail: &'static str,
}

/// Tokenize the selected lexical subset; the compiler remains authoritative.
/// Positions are byte offsets and one-based lines in the original text.
pub(crate) fn lex(engine: Engine, source: &str) -> Result<Vec<Token<'_>>, LexError> {
    let (line_comment, block_open, block_close) = match engine {
        Engine::Lean4 => (Some("--"), "/-", "-/"),
        Engine::Agda => (Some("--"), "{-", "-}"),
        Engine::Rocq => (None, "(*", "*)"),
    };
    // Reject control characters even in comments. As a lexical rule, CR is only
    // accepted as part of CRLF; dangling CR is not silently treated as space.
    let mut line = 1;
    for (i, c) in source.char_indices() {
        match c {
            '\n' => line += 1,
            '\t' => (),
            '\r' if source.as_bytes().get(i + 1) == Some(&b'\n') => (),
            c if c.is_control()
                || (c.is_whitespace() && c != ' ')
                || matches!(c, '\u{ad}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}') =>
            {
                return Err(LexError {
                    line,
                    detail: "unsupported control, spacing, or formatting character",
                });
            }
            _ => (),
        }
    }

    let mut tokens = Vec::new();
    let mut pos = 0;
    let mut line = 1;
    while pos < source.len() {
        let rest = &source[pos..];
        let c = rest.chars().next().expect("nonempty remainder");
        let start = pos;
        let token_line = line;
        let kind;
        if c.is_ascii_whitespace() {
            pos += 1;
            line += usize::from(c == '\n');
            continue;
        } else if line_comment.is_some_and(|comment| rest.starts_with(comment)) {
            pos += rest.find('\n').unwrap_or(rest.len());
            continue;
        } else if engine == Engine::Agda && rest.starts_with("{-#") {
            let Some(end) = rest[3..].find("#-}") else {
                return Err(LexError {
                    line,
                    detail: "unterminated Agda pragma",
                });
            };
            pos += 3 + end + 3;
            kind = TokenKind::Pragma;
        } else if rest.starts_with(block_open) {
            let doc =
                engine == Engine::Lean4 && (rest.starts_with("/-!") || rest.starts_with("/--"));
            pos += block_open.len();
            let mut depth: usize = 1;
            while depth > 0 && pos < source.len() {
                let rest = &source[pos..];
                if rest.starts_with(block_open) {
                    depth += 1;
                    pos += block_open.len();
                } else if rest.starts_with(block_close) {
                    depth -= 1;
                    pos += block_close.len();
                } else {
                    let c = rest.chars().next().expect("nonempty remainder");
                    // Rocq can recognize quoted strings inside comments, including
                    // comment delimiters inside those strings. Exclude this syntax
                    // rather than risk a different view of uncommented source.
                    if engine == Engine::Rocq && c == '"' {
                        return Err(LexError {
                            line: line + source[start..pos].bytes().filter(|&b| b == b'\n').count(),
                            detail: "quoted text inside Rocq comments is outside the supported profile",
                        });
                    }
                    pos += c.len_utf8();
                }
            }
            if depth != 0 {
                return Err(LexError {
                    line,
                    detail: "unterminated nested block comment",
                });
            }
            kind = TokenKind::DocComment;
            if !doc {
                line += source[start..pos].bytes().filter(|&ch| ch == b'\n').count();
                continue;
            }
        } else if rest.starts_with(block_close) {
            return Err(LexError {
                line,
                detail: "unmatched block comment closer",
            });
        } else if engine == Engine::Lean4 && matches!(c, '«' | '»') {
            return Err(LexError {
                line,
                detail: "quoted Lean identifiers are not supported",
            });
        } else if c == '"' || (c == '\'' && engine != Engine::Rocq) {
            if c == '"'
                && engine == Engine::Lean4
                && start > 0
                && matches!(source.as_bytes()[start - 1], b'!' | b'#')
            {
                return Err(LexError {
                    line,
                    detail: "interpolated and raw Lean strings are not supported",
                });
            }
            pos += c.len_utf8();
            let mut closed = false;
            while pos < source.len() {
                let ch = source[pos..].chars().next().expect("nonempty remainder");
                pos += ch.len_utf8();
                if ch == '\\' && engine != Engine::Rocq {
                    if let Some(escaped) = source[pos..]
                        .chars()
                        .next()
                        .filter(|&x| x != '\n' && x != '\r')
                    {
                        pos += escaped.len_utf8();
                    } else {
                        break;
                    }
                } else if ch == c {
                    if engine == Engine::Rocq && source[pos..].starts_with('"') {
                        pos += 1;
                    } else {
                        closed = true;
                        break;
                    }
                } else if ch == '\n' || ch == '\r' {
                    break;
                }
            }
            if !closed {
                return Err(LexError {
                    line,
                    detail: "unterminated or unsupported string or character literal",
                });
            }
            kind = TokenKind::Literal;
        } else if word_char(c) {
            pos += c.len_utf8();
            while let Some(ch) = source[pos..]
                .chars()
                .next()
                .filter(|&ch| word_char(ch) || ch == '\'')
            {
                pos += ch.len_utf8();
            }
            kind = TokenKind::Word;
        } else {
            pos += if rest.starts_with(":=") {
                2
            } else {
                c.len_utf8()
            };
            kind = TokenKind::Symbol;
        }
        let text = &source[start..pos];
        line += text.bytes().filter(|&ch| ch == b'\n').count();
        tokens.push(Token {
            text,
            kind,
            start,
            line: token_line,
        });
    }
    Ok(tokens)
}

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// A Rocq identifier in this profile has no Unicode or quoted identifier syntax.
pub(crate) fn rocq_ident(text: &str) -> bool {
    text != "_"
        && text.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'\''))
}

/// Qualification is the only use of a period permitted inside a term. Insist
/// on direct adjacency, leaving whitespace-delimited periods for vernacular.
pub(crate) fn rocq_qualified_dot(tokens: &[Token<'_>], i: usize) -> bool {
    i > 0
        && i + 1 < tokens.len()
        && tokens[i].text == "."
        && rocq_ident(tokens[i - 1].text)
        && rocq_ident(tokens[i + 1].text)
        && tokens[i - 1].start + tokens[i - 1].text.len() == tokens[i].start
        && tokens[i].start + 1 == tokens[i + 1].start
}

fn rocq_delimiter(stack: &mut Vec<&'static str>, text: &str) -> bool {
    match text {
        "(" => stack.push(")"),
        "[" => stack.push("]"),
        "{" => stack.push("}"),
        ")" | "]" | "}" => return stack.pop() == Some(text),
        _ => (),
    }
    true
}

/// Lexical Gallina subset, not a definition of Gallina's expression grammar.
/// Imported syntax/libraries must come from the broker's trusted load path.
pub(crate) fn rocq_term(tokens: &[Token<'_>]) -> Result<(), LexError> {
    let err = |line, detail| LexError { line, detail };
    if tokens.is_empty() {
        return Err(err(1, "expected a nonempty Rocq term"));
    }
    let mut stack = Vec::new();
    for (i, t) in tokens.iter().enumerate() {
        let supported = match t.kind {
            TokenKind::Word => {
                (rocq_ident(t.text) || t.text == "_" || t.text.bytes().all(|b| b.is_ascii_digit()))
                    && !matches!(
                        t.text,
                        "ltac"
                            | "ltac1"
                            | "ltac2"
                            | "by"
                            | "by_tactic"
                            | "eval"
                            | "Eval"
                            | "native_compute"
                            | "vm_compute"
                            | "unsafe"
                            | "admit"
                            | "Admitted"
                    )
                    && !t.text.ends_with("_no_check")
            }
            TokenKind::Symbol => match t.text {
                "." => rocq_qualified_dot(tokens, i),
                // Standard conjunction/disjunction, not a standalone escape.
                "\\" => {
                    tokens
                        .get(i + 1)
                        .is_some_and(|next| next.text == "/" && t.start + 1 == next.start)
                        || i > 0 && tokens[i - 1].text == "/" && tokens[i - 1].start + 1 == t.start
                }
                "(" | ")" | "[" | "]" | "{" | "}" | ":" | ":=" | "," | ";" | "=" | "|" | "@"
                | "*" | "+" | "-" | "/" | "<" | ">" | "~" | "&" | "%" | "^" => true,
                _ => false,
            },
            _ => false,
        };
        if !supported {
            return Err(err(t.line, "unsupported token in Rocq term-only syntax"));
        }
        if !rocq_delimiter(&mut stack, t.text) {
            return Err(err(t.line, "unbalanced delimiters in Rocq term"));
        }
    }
    if !stack.is_empty() {
        return Err(err(
            tokens.last().unwrap().line,
            "unbalanced delimiters in Rocq term",
        ));
    }
    Ok(())
}

fn rocq_top_level(tokens: &[Token<'_>], symbol: &str) -> Option<usize> {
    let mut stack = Vec::new();
    for (i, t) in tokens.iter().enumerate() {
        if stack.is_empty() && t.text == symbol {
            return Some(i);
        }
        if !rocq_delimiter(&mut stack, t.text) {
            return None;
        }
    }
    None
}

fn rocq_qualid_end(tokens: &[Token<'_>], start: usize) -> Option<usize> {
    if !tokens.get(start).is_some_and(|t| rocq_ident(t.text)) {
        return None;
    }
    let mut i = start + 1;
    while rocq_qualified_dot(tokens, i) {
        i += 2;
    }
    Some(i)
}

fn rocq_require(tokens: &[Token<'_>]) -> bool {
    let mut i = 0;
    if tokens[0].text == "From" {
        let Some(end) = rocq_qualid_end(tokens, 1) else {
            return false;
        };
        i = end;
    }
    if tokens.get(i).map(|t| t.text) != Some("Require") {
        return false;
    }
    i += 1;
    if matches!(tokens.get(i).map(|t| t.text), Some("Import" | "Export")) {
        i += 1;
    }
    if i == tokens.len() {
        return false;
    }
    while i < tokens.len() {
        let Some(end) = rocq_qualid_end(tokens, i) else {
            return false;
        };
        i = end;
    }
    true
}

/// Source admission is a closed vernacular profile for BOTH the candidate and
/// every committed dependency. Commands are bounded by a complete sentence;
/// unknown commands, attributes, command modifiers and arbitrary tactics cannot
/// borrow a permitted keyword. Import resolution and assertion auditing belong
/// to the broker; source-supplied diagnostics are never audit evidence.
fn rocq_source(source: &str, tokens: &[Token<'_>]) -> Result<(), LexError> {
    #[derive(PartialEq, Eq)]
    enum State {
        Declaration,
        Proof,
        Exact,
        Qed,
    }
    let err = |line, detail| LexError { line, detail };
    let mut state = State::Declaration;
    let mut start = 0;
    for (i, dot) in tokens.iter().enumerate() {
        if dot.text != "." || rocq_qualified_dot(tokens, i) {
            continue;
        }
        if !source
            .as_bytes()
            .get(dot.start + 1)
            .is_none_or(u8::is_ascii_whitespace)
        {
            return Err(err(
                dot.line,
                "a Rocq sentence period must be followed by whitespace or EOF",
            ));
        }
        let s = &tokens[start..i];
        let head = s
            .first()
            .ok_or_else(|| err(dot.line, "unexpected empty Rocq sentence"))?;
        match state {
            State::Declaration => match head.text {
                "Require" | "From" if rocq_require(s) => (),
                "Definition" | "Theorem" | "Lemma" | "Corollary" | "Proposition" | "Fact"
                | "Remark" => {
                    if !s.get(1).is_some_and(|name| rocq_ident(name.text)) {
                        return Err(err(
                            head.line,
                            "expected a single root Rocq declaration name",
                        ));
                    }
                    let decl = &s[2..];
                    if head.text == "Definition" {
                        let assignment = rocq_top_level(decl, ":=").ok_or_else(|| {
                            err(
                                head.line,
                                "Rocq Definition requires a Gallina body after :=",
                            )
                        })?;
                        let header = &decl[..assignment];
                        if rocq_top_level(header, ":")
                            .is_some_and(|colon| colon + 1 == header.len())
                        {
                            return Err(err(
                                head.line,
                                "expected a nonempty Rocq Definition type after :",
                            ));
                        }
                        if !header.is_empty() {
                            rocq_term(header)?;
                        }
                        if assignment + 1 == decl.len() {
                            return Err(err(head.line, "expected a nonempty Rocq Definition body"));
                        }
                        rocq_term(&decl[assignment + 1..])?;
                    } else {
                        let explicit_type =
                            rocq_top_level(decl, ":").is_some_and(|colon| colon + 1 < decl.len());
                        if !explicit_type || rocq_top_level(decl, ":=").is_some() {
                            return Err(err(
                                head.line,
                                "Rocq theorem requires an explicit type and separate proof",
                            ));
                        }
                        rocq_term(decl)?;
                        state = State::Proof;
                    }
                }
                _ => return Err(err(head.line, "unsupported Rocq vernacular sentence")),
            },
            State::Proof if s.len() == 1 && head.text == "Proof" => state = State::Exact,
            State::Exact
                if s.len() >= 4
                    && head.text == "exact"
                    && s[1].text == "("
                    && s.last().unwrap().text == ")" =>
            {
                rocq_term(&s[2..s.len() - 1])?;
                state = State::Qed;
            }
            State::Qed if s.len() == 1 && head.text == "Qed" => state = State::Declaration,
            _ => {
                return Err(err(
                    head.line,
                    "Rocq proofs must be exactly Proof. exact (term). Qed.",
                ));
            }
        }
        start = i + 1;
    }
    if start != tokens.len() || state != State::Declaration {
        return Err(err(
            tokens.get(start).or(tokens.last()).map_or(1, |t| t.line),
            "incomplete Rocq sentence or proof",
        ));
    }
    Ok(())
}

fn rule_for(
    engine: Engine,
    tokens: &[Token<'_>],
    i: usize,
) -> Option<(&'static str, &'static str)> {
    let t = &tokens[i];
    let next = |by| tokens.get(i + by).map(|t: &Token<'_>| t.text);
    match engine {
        Engine::Lean4 => match (t.kind, t.text) {
            (
                TokenKind::Word,
                "axiom" | "axioms" | "sorry" | "admit" | "sorryAx" | "native_decide"
                | "ofReduceBool" | "trustCompiler",
            ) => Some(("LEAN_TRUST", "proof bypass or unapproved axiom")),
            (
                TokenKind::Word,
                "unsafe" | "partial" | "extern" | "implemented_by" | "run_cmd" | "run_elab"
                | "run_meta" | "run_tac" | "by_elab" | "initialize" | "builtin_initialize",
            ) => Some((
                "LEAN_EXEC",
                "unsupported execution or safety-sensitive construct",
            )),
            (TokenKind::Word, "IO" | "System" | "BaseIO" | "EIO") if next(1) == Some(".") => {
                Some(("LEAN_EXEC", "host interface namespace"))
            }
            (TokenKind::Symbol, "#") if matches!(next(1), Some("eval" | "reduce" | "compile")) => {
                Some(("LEAN_EXEC", "unsupported compiler directive"))
            }
            (TokenKind::Word, "eval") if next(1) == Some("%") => {
                Some(("LEAN_EXEC", "inline evaluation is outside Phase 1"))
            }
            (
                TokenKind::Word,
                "elab" | "elab_rules" | "macro" | "macro_rules" | "syntax" | "notation"
                | "declare_syntax_cat" | "infix" | "infixl" | "infixr" | "prefix" | "postfix",
            ) => Some((
                "LEAN_SYNTAX",
                "syntax and elaborator extensions are outside Phase 1",
            )),
            (TokenKind::Word, "set_option" | "register_option" | "register_builtin_option") => {
                Some(("LEAN_OPTION", "inline option mutation is outside Phase 1"))
            }
            _ => None,
        },
        Engine::Agda => match (t.kind, t.text) {
            (TokenKind::Pragma, _) => Some((
                "AGDA_PRAGMA",
                "inline pragmas are outside Phase 1; the broker sets locked options",
            )),
            (TokenKind::Word, "postulate" | "primTrustMe") | (TokenKind::Symbol, "?") => {
                Some(("AGDA_TRUST", "unapproved axiom or proof hole"))
            }
            (TokenKind::Symbol, "{") if next(1) == Some("!") => {
                Some(("AGDA_TRUST", "interaction hole"))
            }
            (
                TokenKind::Word,
                "execTC" | "macro" | "tactic" | "quote" | "quoteTerm" | "unquoteDecl"
                | "unquoteDef" | "unquote",
            ) => Some(("AGDA_REFLECTION", "reflection extension is outside Phase 1")),
            _ => None,
        },
        Engine::Rocq => match (t.kind, t.text) {
            (
                TokenKind::Word,
                "Admitted" | "admit" | "Axiom" | "Axioms" | "Parameter" | "Parameters"
                | "Conjecture" | "Conjectures" | "Hypothesis" | "Hypotheses",
            ) => Some(("ROCQ_TRUST", "unapproved assumption or proof bypass")),
            (TokenKind::Word, "Redirect" | "Extraction" | "Load") => {
                Some(("ROCQ_EXEC", "unsupported compiler directive"))
            }
            (TokenKind::Word, "Declare") if next(1) == Some("ML") && next(2) == Some("Module") => {
                Some(("ROCQ_EXEC", "plugin loading is outside Phase 1"))
            }
            (TokenKind::Word, "Set" | "Unset")
                if matches!(
                    next(1),
                    Some("Guard" | "Positivity" | "Universe" | "Typing")
                ) =>
            {
                Some(("ROCQ_OPTION", "unsupported typing or safety option"))
            }
            _ => None,
        },
    }
}

/// Scan an exact UTF-8 source snapshot. The digest binds the exact source bytes
/// (not the path or a diff). Passing does not imply syntax validity, scope
/// authorization, isolation, or kernel acceptance.
///
/// The root-level Rocq profile permits only `[From path] Require [Import|Export]
/// name... .`, Gallina definitions with `:=`, and named theorem-family
/// declarations with an explicit type and `Proof. exact (term). Qed.`. Terms use
/// ASCII names, contiguous qualified names, natural numerals, and a restricted
/// punctuation set; string literals, tactic quotations and custom syntax are
/// excluded. Import resolution must be restricted to audited dependencies and
/// the trusted toolchain by the caller.
pub fn scan_source(engine: Engine, file_path: &str, source: &str) -> PreFlightScanResult {
    let mut violations = Vec::new();
    if let Err(violation) = validate_source_path(engine, file_path) {
        violations.push(violation);
    }
    match lex(engine, source) {
        Ok(tokens) if engine == Engine::Rocq => {
            if let Some((t, (rule, detail))) = tokens
                .iter()
                .enumerate()
                .find_map(|(i, t)| rule_for(engine, &tokens, i).map(|diagnostic| (t, diagnostic)))
            {
                violations.push(deny(file_path, rule, Some(t.line), Some(t.text), detail));
            } else if let Err(e) = rocq_source(source, &tokens) {
                violations.push(deny(
                    file_path,
                    "ROCQ_PROFILE",
                    Some(e.line),
                    None,
                    e.detail,
                ));
            }
        }
        Ok(tokens) => {
            for (i, t) in tokens.iter().enumerate() {
                if let Some((rule, detail)) = rule_for(engine, &tokens, i) {
                    // Avoid embedding an unbounded pragma or other source span in diagnostics.
                    let construct = if t.kind == TokenKind::Pragma {
                        "{-# … #-}"
                    } else {
                        t.text
                    };
                    violations.push(deny(file_path, rule, Some(t.line), Some(construct), detail));
                }
            }
        }
        Err(e) => violations.push(deny(file_path, "SOURCE_LEX", Some(e.line), None, e.detail)),
    }
    PreFlightScanResult {
        passed: !violations
            .iter()
            .any(|v| v.severity == PreFlightSeverity::Deny),
        engine: engine.into(),
        patch_sha256: format!("{:x}", Sha256::digest(source.as_bytes())),
        authorized_scope_checked: false,
        violations,
    }
}
