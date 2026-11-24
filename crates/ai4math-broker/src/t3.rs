//! Deliberately narrow attribution for native Lean T3 rechecks.
use crate::{AXIOMS, lean_axiom_report, sandbox::ChildExit};
use ai4math_core::FrozenTarget;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;

#[derive(Clone, Copy, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
struct Position {
    line: usize,
    column: usize,
}

impl Position {
    fn at_end_of(prefix: &str) -> Self {
        Self {
            line: prefix.bytes().filter(|b| *b == b'\n').count() + 1,
            column: prefix
                .rsplit('\n')
                .next()
                .unwrap_or_default()
                .chars()
                .count(),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Diagnostic<'a> {
    file_name: &'a str,
    severity: Severity,
    pos: Position,
    end_pos: Option<Position>,
    #[serde(default)]
    is_silent: bool,
    data: String,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Severity {
    Error,
    Warning,
    Information,
}

fn diagnostics(output: &str) -> Result<Vec<Diagnostic<'_>>> {
    // stdout and stderr share a bounded capture. Every line must be a Lean
    // message; unstructured loader/sandbox errors or changed formats fail closed.
    ensure!(
        !output.contains('\u{fffd}'),
        "inexact Lean diagnostic capture"
    );
    output
        .lines()
        .map(|line| serde_json::from_str(line).context("unrecognized Lean JSON diagnostic"))
        .collect()
}

fn axiom_pos(source: &str) -> Position {
    // The broker appends a newline and the audit command after the rendered source.
    Position {
        line: Position::at_end_of(source).line + 1,
        column: 0,
    }
}

fn is_audit(diagnostic: &Diagnostic<'_>, file_path: &str, position: Position) -> bool {
    diagnostic.severity == Severity::Information
        && !diagnostic.is_silent
        && diagnostic.file_name == file_path
        && diagnostic.pos == position
}

pub(super) fn axiom_report(output: &str, target: &FrozenTarget, source: &str) -> Result<String> {
    axiom_report_at(output, &target.file_path, source)
}

pub(super) fn axiom_report_at(output: &str, file_path: &str, source: &str) -> Result<String> {
    let diagnostics = diagnostics(output)?;
    ensure!(
        diagnostics.iter().all(|d| d.severity != Severity::Error),
        "Lean reported an error despite a successful exit"
    );
    let position = axiom_pos(source);
    let mut audits = diagnostics
        .iter()
        .filter(|d| is_audit(d, file_path, position));
    let report = audits.next().context("missing Lean audit diagnostic")?;
    ensure!(audits.next().is_none(), "ambiguous Lean audit diagnostic");
    Ok(report.data.clone())
}

fn is_proof_diagnostic(data: &str) -> bool {
    // Positive allowlist: locations alone do not distinguish errors raised by
    // imports, resource exhaustion, file IO, or elaborator/plugin failures.
    // These known core diagnostics describe a type or definitional equality
    // mismatch. Unknown identifiers, tactics, and messages remain unavailable.
    let type_mismatch = (data.starts_with("Type mismatch\n")
        || data.starts_with("Application type mismatch:"))
        && data.contains("\nhas type\n")
        && (data.contains("\nbut is expected to have type\n")
            || data.contains("\nbut is expected to have type:\n"));
    let rfl_mismatch = data.starts_with("Tactic `rfl` failed: The left-hand side\n")
        && data.contains("\nis not definitionally equal to the right-hand side\n");
    type_mismatch || rfl_mismatch
}

/// Classify only a known core mismatch in an exact downstream file. Other
/// compiler exits, loader/sandbox errors, partial captures, or mixed errors
/// remain unclassified infrastructure/candidate errors for the operator.
pub(super) fn is_downstream_regression(exit: &ChildExit, file_path: &str, source: &str) -> bool {
    if exit.status.code() != Some(1) {
        return false;
    }
    let Ok(parsed) = diagnostics(&exit.diagnostics) else {
        return false;
    };
    let last = Position::at_end_of(source);
    let errors = parsed
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect::<Vec<_>>();
    !errors.is_empty()
        && errors.iter().all(|d| {
            !d.is_silent
                && d.file_name == file_path
                && d.pos.line > 0
                && d.pos <= last
                && d.end_pos.is_some_and(|end| d.pos < end && end <= last)
                && is_proof_diagnostic(&d.data)
        })
}

#[derive(Debug)]
pub(super) struct TargetCompilerRejection(pub(super) String);

impl std::fmt::Display for TargetCompilerRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TargetCompilerRejection {}

pub(super) fn target_rejection(
    exit: &ChildExit,
    target: &FrozenTarget,
    source: &str,
) -> Option<TargetCompilerRejection> {
    if exit.status.code() != Some(1) {
        return None;
    }
    // Exclude errors in frozen imports/headers, appended audits, and diagnostics
    // spanning outside the inserted parenthesized expression.
    let inserted = source
        .strip_prefix(&target.prefix)?
        .strip_suffix(&target.suffix)?;
    let open = inserted.find('(')?;
    let start = Position::at_end_of(&source[..target.prefix.len() + open]);
    let end = Position::at_end_of(&source[..source.len() - target.suffix.len()]);
    let mut errors = Vec::new();
    let mut audit_seen = false;
    let audit_position = axiom_pos(source);
    for diagnostic in diagnostics(&exit.diagnostics).ok()? {
        if is_audit(&diagnostic, &target.file_path, audit_position) && !audit_seen {
            // Lean may introduce sorryAx during recovery from a genuine error.
            // An independent unexpected axiom is still a policy ambiguity.
            let axioms = lean_axiom_report(&diagnostic.data, &target.target_id).ok()?;
            if axioms
                .iter()
                .any(|a| a != "sorryAx" && !AXIOMS.contains(&a.as_str()))
            {
                return None;
            }
            audit_seen = true;
        } else if diagnostic.severity == Severity::Error
            && !diagnostic.is_silent
            && diagnostic.file_name == target.file_path
            && diagnostic.pos >= start
            && diagnostic
                .end_pos
                .is_some_and(|p| diagnostic.pos < p && p <= end)
            && is_proof_diagnostic(&diagnostic.data)
        {
            errors.push(format!(
                "{}:{}:{}: {}",
                diagnostic.file_name, diagnostic.pos.line, diagnostic.pos.column, diagnostic.data
            ));
        } else {
            return None;
        }
    }
    (audit_seen && !errors.is_empty()).then(|| TargetCompilerRejection(errors.join("\n")))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use ai4math_core::{Engine, ProofBodySubmission, render_proof_body};
    use serde_json::json;
    use std::os::unix::process::ExitStatusExt;

    fn fixture() -> (FrozenTarget, String) {
        let target = FrozenTarget {
            engine: Engine::Lean4,
            target_id: "target".into(),
            file_path: "Target.lean".into(),
            prefix: "import Lean\ntheorem target : 0 = 1 := ".into(),
            suffix: "\n".into(),
        };
        let source = render_proof_body(
            &target,
            &ProofBodySubmission {
                target_id: "target".into(),
                proof_body: "by rfl".into(),
            },
        )
        .unwrap();
        (target, source)
    }

    #[test]
    fn audit_must_come_from_the_appended_command() {
        let (target, source) = fixture();
        let mut audit = json!({
            "fileName": "Target.lean",
            "severity": "information",
            "pos": {"line": 6, "column": 0},
            "endPos": {"line": 6, "column": 6},
            "data": "'target' does not depend on any axioms",
        });
        let good_log = audit.to_string();
        assert_eq!(
            axiom_report(&good_log, &target, &source).unwrap(),
            "'target' does not depend on any axioms"
        );
        assert!(axiom_report(&format!("{good_log}\n{good_log}"), &target, &source).is_err());
        audit["pos"]["line"] = json!(3);
        assert!(axiom_report(&audit.to_string(), &target, &source).is_err());
        audit["severity"] = json!("error");
        assert!(axiom_report(&format!("{good_log}\n{audit}"), &target, &source).is_err());
    }

    #[test]
    fn downstream_regression_requires_structured_attributable_non_resource_failure() {
        let mut diagnostic = json!({"fileName":"Downstream.lean","severity":"error",
            "pos":{"line":1,"column":5},"endPos":{"line":1,"column":8},
            "data":"Type mismatch\n  term\nhas type\n  True\nbut is expected to have type\n  False"});
        let assess = |log: String, code: i32| {
            is_downstream_regression(
                &ChildExit {
                    status: std::process::ExitStatus::from_raw(code),
                    diagnostics: log,
                },
                "Downstream.lean",
                "theorem broken : False := True.intro\n",
            )
        };
        let strict = diagnostic.to_string();
        assert!(assess(strict.clone(), 1 << 8));
        for status in [9, 71 << 8, 0] {
            assert!(!assess(strict.clone(), status));
        }
        assert!(!assess(
            format!("{strict}\nsandbox-exec: permission denied\n"),
            1 << 8
        ));
        diagnostic["data"] = json!("maximum heartbeats exceeded");
        assert!(!assess(format!("{strict}\n{diagnostic}\n"), 1 << 8));
        diagnostic = serde_json::from_str(&strict).unwrap();
        diagnostic["fileName"] = json!("Other.lean");
        assert!(!assess(diagnostic.to_string(), 1 << 8));
    }

    #[test]
    fn only_attributable_lean_mismatches_can_reject() {
        let (target, source) = fixture();
        let error = json!({
            "fileName": "Target.lean",
            "severity": "error",
            "pos": {"line": 3, "column": 5},
            "endPos": {"line": 3, "column": 8},
            "data": "Tactic `rfl` failed: The left-hand side\n  0\nis not definitionally equal to the right-hand side\n  1\n\n⊢ 0 = 1",
        });
        let audit = json!({
            "fileName": "Target.lean",
            "severity": "information",
            "pos": {"line": 6, "column": 0},
            "endPos": {"line": 6, "column": 6},
            "data": "'target' depends on axioms: [sorryAx]",
        });
        let assess = |diagnostics, raw| {
            target_rejection(
                &ChildExit {
                    status: std::process::ExitStatus::from_raw(raw),
                    diagnostics,
                },
                &target,
                &source,
            )
        };
        let good_log = format!("{error}\n{audit}\n");
        assert!(assess(good_log.clone(), 1 << 8).is_some());
        let mut mismatch = error.clone();
        mismatch["data"] = json!(
            "Type mismatch\n  True.intro\nhas type\n  True\nbut is expected to have type\n  0 = 1"
        );
        assert!(assess(format!("{mismatch}\n{audit}\n"), 1 << 8).is_some());
        assert!(assess(good_log.clone(), 9).is_none()); // signal, not exit 1
        assert!(assess(good_log.clone(), 42 << 8).is_none());
        assert!(assess(error.to_string(), 1 << 8).is_none()); // incomplete compile
        assert!(assess(audit.to_string(), 1 << 8).is_none()); // no target error
        assert!(assess(format!("{good_log}{audit}\n"), 1 << 8).is_none());
        assert!(
            assess(
                format!("{good_log}sandbox-exec: permission denied\n"),
                1 << 8
            )
            .is_none()
        );
        for (field, replacement) in [
            ("fileName", json!("Dependency.lean")),
            ("pos", json!({"line": 1, "column": 0})),
            ("endPos", json!({"line": 6, "column": 1})),
            ("endPos", json!(null)),
            ("isSilent", json!(true)),
            ("severity", json!("warning")),
            ("data", json!("unknown module prefix 'Support'")),
            ("data", json!("Unknown identifier 'current'")),
            ("data", json!("maximum heartbeats exceeded")),
            ("data", json!("permission denied (error code: 1)")),
        ] {
            let mut ambiguous = error.clone();
            ambiguous[field] = replacement;
            assert!(
                assess(format!("{ambiguous}\n{audit}\n"), 1 << 8).is_none(),
                "{ambiguous}"
            );
            assert!(
                assess(format!("{good_log}{ambiguous}\n"), 1 << 8).is_none(),
                "mixed: {ambiguous}"
            );
        }
        let mut disallowed_axiom = audit;
        disallowed_axiom["data"] = json!("'target' depends on axioms: [unexpectedAxiom, sorryAx]");
        assert!(assess(format!("{error}\n{disallowed_axiom}\n"), 1 << 8).is_none());
    }
}
