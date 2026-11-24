//! Bounded native Lean premise reconstruction. This route never issues a promotion receipt.
use super::{
    Broker, Engine, Lane, Pressure, Toolchain,
    breaker_probe::{
        build_committed_sources, command_timeout, identifier, reject_installation_shadows,
    },
    dependencies_digest, digest, path_for, sandbox, stage_source, validate_modules,
};
use ai4math_core::{FrozenTarget, ProofBodySubmission, render_proof_body};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    time::{Duration, Instant},
};

pub const MAX_HAMMER_PREMISES_BYTES: usize = 4096;
pub const MAX_HAMMER_DEPENDENCIES: usize = 8;
const MAX_PREMISES: usize = 8;
const TOTAL_TIMEOUT: Duration = Duration::from_secs(45);
const CHECKER_FILE: &str = "Ai4mathSealedLeanHammer.lean";
const CHECKER: &str = include_str!("lean_hammer_audit.lean");
const SEAL: &str = "ai4math:sealed-lean-hammer-audit:v1 ";

/// A list of exact Lean declaration names, never terms, selectors or tactic text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HammerPremises(Vec<String>);

impl HammerPremises {
    /// Accept only a bounded v1 JSON object with 1–8 distinct dotted ASCII names.
    /// Segments start with an ASCII letter or underscore, then use ASCII letters,
    /// digits, underscores or apostrophes. Do not include a leading `_root_.`.
    /// JSON escapes are deliberately not accepted, even when they decode to ASCII.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_HAMMER_PREMISES_BYTES,
            "Lean hammer premises JSON exceeds its byte limit"
        );
        ensure!(
            !bytes.contains(&b'\\'),
            "Lean hammer JSON escapes are not allowed"
        );
        ensure!(
            bytes.iter().copied().find(|b| !b.is_ascii_whitespace()) == Some(b'{'),
            "expected a Lean hammer premises JSON object"
        );
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            version: u8,
            premises: Vec<String>,
        }
        let Wire { version, premises } =
            serde_json::from_slice(bytes).context("invalid Lean hammer premises JSON")?;
        ensure!(version == 1, "expected Lean hammer premises version 1");
        ensure!(
            (1..=MAX_PREMISES).contains(&premises.len()),
            "expected 1–8 Lean hammer premises"
        );
        let mut seen = HashSet::new();
        for name in &premises {
            ensure!(
                name.contains('.')
                    && !name.starts_with("_root_.")
                    && name.split('.').all(identifier),
                "expected an exact dotted ASCII Lean declaration name: {name:?}"
            );
            ensure!(seen.insert(name), "duplicate Lean hammer premise: {name:?}");
        }
        Ok(Self(premises))
    }

    fn proof_body(&self) -> String {
        // `only` removes default lemmas and locals, but does not on its own
        // disable the core tactic's constructor/introduction fallbacks.
        let hints = self
            .0
            .iter()
            .map(|name| format!("_root_.{name}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "by\n  solve_by_elim (maxDepth := 4) -constructor -intro -symm -exfalso -suggestions only [{hints}]"
        )
    }
}

/// Advisory output only: submit the body to the usual verification and promotion
/// flow if desired. Hashes bind the rendered source and ordered dependency bytes;
/// they are not receipts or evidence of provenance for the caller's commit label.
#[derive(Debug, Serialize)]
#[must_use]
pub struct HammerCandidate {
    pub proof_body: String,
    pub base_commit: String,
    pub file_path: String,
    pub target_id: String,
    pub source_sha256: String,
    pub dependencies_sha256: String,
    pub observed_axioms: Vec<String>,
}

fn audit_report(log: &str) -> Result<Vec<String>> {
    let json = log
        .strip_prefix(SEAL)
        .and_then(|line| line.strip_suffix('\n'))
        .filter(|line| !line.contains('\n'))
        .context("expected exact sealed Lean hammer audit")?;
    let axioms: Vec<String> =
        serde_json::from_str(json).context("invalid sealed Lean hammer axiom list")?;
    ensure!(
        axioms.iter().all(|a| super::AXIOMS.contains(&a.as_str()))
            && axioms.windows(2).all(|w| w[0] < w[1]),
        "non-L5 or ambiguous Lean hammer axioms: {axioms:?}"
    );
    Ok(axioms)
}

impl Broker {
    /// Try one fixed native Lean reconstruction tactic against a frozen theorem.
    /// The caller supplies the frozen target and its exact committed predecessor
    /// sources in build order. This never runs an ATP, pool search or arbitrary
    /// caller tactics, and cannot return a T1 receipt.
    pub fn reconstruct_lean_hammer(
        &self,
        tool: &Toolchain,
        target: &FrozenTarget,
        premises: &HammerPremises,
        base_commit: &str,
        dependencies: &[(&str, &str)],
    ) -> Result<HammerCandidate> {
        let deadline = Instant::now() + TOTAL_TIMEOUT;
        ensure!(
            tool.engine == Engine::Lean4 && target.engine == Engine::Lean4,
            "Lean hammer requires a native Lean toolchain and target"
        );
        ensure!(
            matches!(base_commit.len(), 40 | 64)
                && base_commit.bytes().all(|c| c.is_ascii_hexdigit()),
            "expected a full Git base commit hash"
        );
        ensure!(
            dependencies.len() <= MAX_HAMMER_DEPENDENCIES,
            "too many Lean hammer dependencies"
        );
        ensure!(
            !premises.0.contains(&target.target_id),
            "a Lean hammer premise cannot be the target itself"
        );

        let submission = ProofBodySubmission {
            target_id: target.target_id.clone(),
            proof_body: premises.proof_body(),
        };
        let source =
            render_proof_body(target, &submission).context("render frozen Lean hammer proof")?;
        let module = path_for(Engine::Lean4, &target.file_path)?;
        let mut sources = dependencies.to_vec();
        sources.push((&target.file_path, &source));
        validate_modules(Engine::Lean4, &sources, Some(CHECKER_FILE))?;
        reject_installation_shadows(tool, &sources)?;

        // Start the shared budget before admission, not after each child. No
        // candidate build or checker command may run for longer than 30s.
        let _lease = self.resources.acquire(Lane::Heavy, Pressure::Red)?;
        command_timeout(deadline)?;
        let build = tempfile::tempdir()?;
        let build_path = build.path().canonicalize()?;
        build_committed_sources(tool, &build_path, &sources, deadline, None)
            .context("try isolated native Lean hammer reconstruction")?;
        let objects = build_path.join("objects");
        let audit = tempfile::tempdir()?;
        let writable = audit.path().join("scratch");
        fs::create_dir(&writable)?;
        stage_source(
            Engine::Lean4,
            audit.path(),
            &writable,
            CHECKER_FILE,
            CHECKER,
        )?;
        let mut args = vec![
            "--run".into(),
            CHECKER_FILE.into(),
            objects
                .to_str()
                .context("non-UTF-8 Lean hammer object path")?
                .into(),
            module.into(),
            target.target_id.clone(),
        ];
        args.extend(premises.0.iter().cloned());
        // Only this static checker sees the objects as data, with no LEAN_PATH
        // at startup. Build stderr/stdout cannot stand in for the axiom audit.
        let log = sandbox::run_with_data(
            sandbox::Invocation {
                executable: &tool.executable,
                args: &args,
                room: audit.path(),
                writable: &writable,
                reads: &tool.reads,
                env: &[],
                timeout: command_timeout(deadline)?,
            },
            &[objects],
            &[],
        )
        .context("audit native Lean hammer reconstruction")?;
        let observed_axioms = audit_report(&log)?;
        let source_sha256 = digest(source.as_bytes());
        let dependencies_sha256 = dependencies_digest(dependencies);
        audit.close()?;
        build.close()?;
        command_timeout(deadline)?;

        Ok(HammerCandidate {
            proof_body: submission.proof_body,
            base_commit: base_commit.into(),
            file_path: target.file_path.clone(),
            target_id: target.target_id.clone(),
            source_sha256,
            dependencies_sha256,
            observed_axioms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_a_bounded_exact_premise_list() -> Result<()> {
        let valid = br#"{"version":1,"premises":["Foo.bar'","Other_Name.x2"]}"#;
        assert_eq!(
            HammerPremises::parse(valid)?.proof_body(),
            "by\n  solve_by_elim (maxDepth := 4) -constructor -intro -symm -exfalso -suggestions only [_root_.Foo.bar', _root_.Other_Name.x2]"
        );
        assert!(
            HammerPremises::parse(
                br#"{"version":1,"premises":["A.b","A.c","A.d","A.e","A.f","A.g","A.h","A.i"]}"#,
            )
            .is_ok()
        );
        let mut at_limit = valid.to_vec();
        at_limit.resize(MAX_HAMMER_PREMISES_BYTES, b' ');
        assert!(HammerPremises::parse(&at_limit).is_ok());
        at_limit.push(b' ');
        assert!(HammerPremises::parse(&at_limit).is_err());
        for invalid in [
            r#"{"version":2,"premises":["A.b"]}"#,
            r#"[1, ["A.b"]]"#,
            r#"{"version":1,"version":1,"premises":["A.b"]}"#,
            r#"{"version":1,"premises":["A.b"],"premises":["A.c"]}"#,
            r#"{"version":1.0,"premises":["A.b"]}"#,
            r#"{"version":1,"premises":[]}"#,
            r#"{"version":1,"premises":["A.b","A.b"]}"#,
            r#"{"version":1,"premises":["A.b","A.c","A.d","A.e","A.f","A.g","A.h","A.i","A.j"]}"#,
            r#"{"version":1,"premises":["A.b"],"maxDepth":99}"#,
            r#"{"version":1,"premises":["A.b"],"tactics":"exact A.b"}"#,
            r#"{"version":1,"premises":["A.\u0062"]}"#,
            r#"{"version":1,"premises":["A.b"]} {}"#,
            r#"{"version":1,"premises":["_root_.A.b"]}"#,
            r#"{"version":1,"premises":["b"]}"#,
            r#"{"version":1,"premises":["A..b"]}"#,
            r#"{"version":1,"premises":["A._"]}"#,
            r#"{"version":1,"premises":["A.3b"]}"#,
            r#"{"version":1,"premises":["A.β"]}"#,
            r#"{"version":1,"premises":["A.b] ; exact c"]}"#,
            r#"{"version":1,"premises":["A.«b»"]}"#,
        ] {
            assert!(
                HammerPremises::parse(invalid.as_bytes()).is_err(),
                "{invalid}"
            );
        }
        Ok(())
    }

    #[test]
    fn audit_output_requires_an_exact_allowlisted_seal() {
        assert!(audit_report(&format!("{SEAL}[]\n")).unwrap().is_empty());
        for invalid in [
            "",
            "[\"sorryAx\"]",
            "[\"propext\",\"propext\"]",
            "[]\nwarning",
        ] {
            assert!(audit_report(&format!("{SEAL}{invalid}\n")).is_err());
        }
        assert!(audit_report(&format!("source log\n{SEAL}[]\n")).is_err());
    }
}
