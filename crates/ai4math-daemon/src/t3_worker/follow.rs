//! Resident selection is advisory. The guarded CLI still validates the exact approval
//! and reviewed main under its own Git gate, and alone performs any writes.
use super::{Config, MAX_STEPS, Stop, next_revision};
use crate::runtime::managed_wiki::{direct_main, is_forward_main};
use ai4math_store::{StateStore, T3SweepState, T3SweepStatus};
use std::{collections::BTreeSet, path::Path, thread, time::Duration};

const MAX_TRANSITIONS: usize = 32;

type Outcome<T> = std::result::Result<T, Stop>;

pub(super) fn run(config: &Config, root: &Path) -> Stop {
    let Some(repo) = config.bare_repo.to_str() else {
        return Stop::Unavailable;
    };
    let mut follow = Follow::default();
    let interval = Duration::from_millis(config.interval_ms);
    loop {
        if let Err(stop) = follow.tick(config, root, repo) {
            return stop;
        }
        // Idle and paused runs still observe main and newly applied plans, without spinning.
        thread::sleep(interval);
    }
}

#[derive(Default)]
struct Follow {
    head: Option<String>,
    last_attempted: Option<String>,
    disabled: BTreeSet<String>,
    steps: usize,
    transitions: usize,
}

impl Follow {
    fn tick(&mut self, config: &Config, root: &Path, repo: &str) -> Outcome<()> {
        config.check_paths().map_err(|_| Stop::Unavailable)?;
        let state = config
            .open_bound_state(root)
            .map_err(|_| Stop::Unavailable)?;
        let head = self.observe_head(repo)?;
        if paused(&state)? {
            return Ok(());
        }
        let candidate = self.next_candidate(&state, &head)?;
        drop(state);
        let Some((hash, before, expected)) = candidate else {
            return Ok(());
        };

        // Re-verify the private paths, binding and journals after selection. Release the
        // read connection and resample direct main immediately before any launch.
        config.check_paths().map_err(|_| Stop::Unavailable)?;
        let state = config
            .open_bound_state(root)
            .map_err(|_| Stop::Unavailable)?;
        let pending = paused(&state)?;
        drop(state);
        if pending || self.observe_head(repo)? != head {
            // Even a verified forward move must rescan approvals against the new head.
            return Ok(());
        }

        self.steps += 1;
        if let Err(reason) = config
            .step(root, &hash)
            .and_then(|()| config.readback(root, &hash, &before, expected))
        {
            self.disable(hash.clone(), reason);
        }
        self.last_attempted = Some(hash);
        if self.steps == MAX_STEPS {
            return Err(Stop::StepLimit);
        }
        Ok(())
    }

    fn observe_head(&mut self, repo: &str) -> Outcome<String> {
        let head = direct_main(repo).ok_or(Stop::HeadChanged)?;
        self.accept_head(&head, |previous, current| {
            is_forward_main(repo, previous, current)
        })?;
        Ok(head)
    }

    fn accept_head(
        &mut self,
        head: &str,
        is_forward: impl FnOnce(&str, &str) -> bool,
    ) -> Outcome<()> {
        if let Some(previous) = self.head.as_deref() {
            if previous == head {
                return Ok(());
            }
            if !is_forward(previous, head) {
                return Err(Stop::NonForwardMain);
            }
            if self.transitions == MAX_TRANSITIONS {
                return Err(Stop::HeadTransitionLimit);
            }
            self.transitions += 1;
        }
        self.head = Some(head.to_owned());
        // An older head is never visited again in this run. Keep global budgets and
        // latches, but start lexicographic rotation afresh for this head.
        self.last_attempted = None;
        Ok(())
    }

    fn next_candidate(
        &mut self,
        state: &StateStore,
        head: &str,
    ) -> Outcome<Option<(String, T3SweepStatus, i64)>> {
        // The store supplies only canonical hashes for this exact head, in byte order.
        let hashes = state
            .applied_t3_hashes_for_head(head)
            .map_err(|_| Stop::Unavailable)?;
        let start = rotation_start(&hashes, self.last_attempted.as_deref());
        for hash in hashes[start..].iter().chain(&hashes[..start]) {
            if self.disabled.contains(hash) {
                continue;
            }
            let before = match state.t3_sweep_status(hash) {
                Ok(before) if before.reviewed_commit == head => before,
                _ => {
                    self.disable(hash.clone(), Stop::Unavailable);
                    continue;
                }
            };
            if before.status == T3SweepState::Complete {
                continue;
            }
            if let Some(expected) = next_revision(&before) {
                return Ok(Some((hash.clone(), before, expected)));
            }
            self.disable(hash.clone(), Stop::NoProgress);
        }
        Ok(None)
    }

    fn disable(&mut self, hash: String, reason: Stop) {
        self.disabled.insert(hash);
        reason.log_disabled();
    }
}

fn paused(state: &StateStore) -> Outcome<bool> {
    Ok(state
        .promotion_intent()
        .map_err(|_| Stop::Unavailable)?
        .is_some()
        || state
            .pivot_intent()
            .map_err(|_| Stop::Unavailable)?
            .is_some())
}

fn rotation_start(hashes: &[String], last_attempted: Option<&str>) -> usize {
    last_attempted.map_or(0, |last| {
        hashes.partition_point(|hash| hash.as_str() <= last)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_resumes_after_previous_hash_even_if_catalog_changed() {
        let hashes = ["a", "d", "e"].map(str::to_owned);
        for (last, expected) in [
            (None, ["a", "d", "e"]),
            (Some("a"), ["d", "e", "a"]),
            (Some("c"), ["d", "e", "a"]),
            (Some("e"), ["a", "d", "e"]),
        ] {
            let start = rotation_start(&hashes, last);
            let rotated = hashes[start..]
                .iter()
                .chain(&hashes[..start])
                .map(String::as_str)
                .collect::<Vec<_>>();
            assert_eq!(rotated, expected);
        }
    }

    #[test]
    fn transitions_are_forward_only_and_keep_run_wide_limits() -> Outcome<()> {
        let mut follow = Follow::default();
        let unexpected_check = |_: &str, _: &str| panic!("no transition to check");
        follow.accept_head("0", unexpected_check)?;
        follow.steps = 17;
        follow.disabled.insert("failed".into());
        follow.last_attempted = Some("last".into());
        follow.accept_head("0", unexpected_check)?;
        assert_eq!(follow.last_attempted.as_deref(), Some("last"));
        for i in 1..=MAX_TRANSITIONS {
            follow.accept_head(&i.to_string(), |previous, current| {
                assert_eq!(previous, (i - 1).to_string());
                assert_eq!(current, i.to_string());
                true
            })?;
            assert_eq!(follow.last_attempted, None);
            assert_eq!(follow.transitions, i);
        }
        assert_eq!(follow.steps, 17);
        assert!(follow.disabled.contains("failed"));
        assert_eq!(
            follow.accept_head("0", |_, _| false),
            Err(Stop::NonForwardMain),
        );
        assert_eq!(
            follow.accept_head("next", |_, _| true),
            Err(Stop::HeadTransitionLimit),
        );
        assert_eq!(follow.head, Some(MAX_TRANSITIONS.to_string()));
        Ok(())
    }
}
