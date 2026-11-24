use anyhow::{Context, Result, bail, ensure};
use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

// Only waiters count against the queue limit; the running lease is separate.
const DEFAULT_HEAVY_QUEUE_CAPACITY: usize = 8;
const DEFAULT_HEAVY_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Fast,
    Heavy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pressure {
    Green,
    Yellow,
    Red,
}
impl Pressure {
    pub fn from_used_percent(used: u64) -> Self {
        if used > 85 {
            Self::Red
        } else if used >= 70 {
            Self::Yellow
        } else {
            Self::Green
        }
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct BrokerLoad {
    pub fast_busy: usize,
    pub fast_capacity: usize,
    pub heavy_running: usize,
    pub heavy_queued: usize,
}

#[derive(Default)]
struct Counts {
    fast: usize,
    heavy: bool,
    heavy_pending: VecDeque<u64>,
    next_heavy_ticket: u64,
}

/// One per host broker. Fast work never blocks in a queue; heavy work has
/// priority over new fast leases while queued. Heavy work runs one at a time,
/// in enqueue order, with a bounded queue and a separate admission deadline.
pub(crate) struct Resources {
    counts: Mutex<Counts>,
    changed: Condvar,
    fast_cap: usize,
    heavy_queue_cap: usize,
    heavy_wait_timeout: Duration,
}
impl Default for Resources {
    fn default() -> Self {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        Self {
            counts: Mutex::new(Counts::default()),
            changed: Condvar::new(),
            fast_cap: cores.saturating_sub(1).clamp(1, 8),
            heavy_queue_cap: DEFAULT_HEAVY_QUEUE_CAPACITY,
            heavy_wait_timeout: DEFAULT_HEAVY_WAIT_TIMEOUT,
        }
    }
}

pub(crate) struct Lease<'a>(&'a Resources, Lane);
impl Drop for Lease<'_> {
    fn drop(&mut self) {
        let mut counts = self.0.counts.lock().unwrap_or_else(|e| e.into_inner());
        match self.1 {
            Lane::Fast => counts.fast -= 1,
            Lane::Heavy => counts.heavy = false,
        }
        self.0.changed.notify_all();
    }
}

impl Resources {
    pub fn snapshot(&self) -> Result<BrokerLoad> {
        let count = self
            .counts
            .lock()
            .map_err(|_| anyhow::anyhow!("lease lock poisoned"))?;
        Ok(BrokerLoad {
            fast_busy: count.fast,
            fast_capacity: self.fast_cap,
            heavy_running: usize::from(count.heavy),
            heavy_queued: count.heavy_pending.len(),
        })
    }

    pub fn acquire(&self, lane: Lane, pressure: Pressure) -> Result<Lease<'_>> {
        let mut state = self
            .counts
            .lock()
            .map_err(|_| anyhow::anyhow!("lease lock poisoned"))?;
        match lane {
            Lane::Fast => {
                let cap = match pressure {
                    Pressure::Green => self.fast_cap,
                    Pressure::Yellow => self.fast_cap / 2,
                    Pressure::Red => 0,
                };
                ensure!(
                    state.heavy_pending.is_empty() && state.fast < cap,
                    "fast lane unavailable: memory pressure or higher-priority work"
                );
                state.fast += 1;
            }
            Lane::Heavy => {
                if !state.heavy && state.heavy_pending.is_empty() {
                    state.heavy = true;
                    return Ok(Lease(self, lane));
                }

                ensure!(
                    state.heavy_pending.len() < self.heavy_queue_cap,
                    "heavy lane queue full (capacity {})",
                    self.heavy_queue_cap
                );
                let ticket = state.next_heavy_ticket;
                state.next_heavy_ticket = ticket
                    .checked_add(1)
                    .context("heavy lane tickets exhausted")?;
                let deadline = Instant::now() + self.heavy_wait_timeout;
                state.heavy_pending.push_back(ticket);

                loop {
                    // Check the same absolute deadline even if a release or a
                    // spurious wake just made the slot appear available.
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        self.withdraw_heavy_waiter(&mut state, ticket);
                        bail!(
                            "heavy lane queue wait timed out after {:?}",
                            self.heavy_wait_timeout
                        );
                    }
                    if !state.heavy && state.heavy_pending.front() == Some(&ticket) {
                        state.heavy_pending.pop_front();
                        state.heavy = true;
                        break;
                    }
                    state = match self.changed.wait_timeout(state, remaining) {
                        Ok((state, _)) => state,
                        Err(poisoned) => {
                            let (mut state, _) = poisoned.into_inner();
                            self.withdraw_heavy_waiter(&mut state, ticket);
                            bail!("lease lock poisoned");
                        }
                    };
                }
            }
        }
        Ok(Lease(self, lane))
    }

    fn withdraw_heavy_waiter(&self, state: &mut Counts, ticket: u64) {
        state.heavy_pending.retain(|&queued| queued != ticket);
        // An idle lane can proceed if its previous head withdrew.
        self.changed.notify_all();
    }
}

/// Treat free, speculative and inactive pages as reclaimable. This is an
/// admission heuristic, not an exact measurement of macOS memory pressure.
#[cfg(target_os = "macos")]
pub(crate) fn host_pressure() -> Result<Pressure> {
    let out = std::process::Command::new("/usr/bin/vm_stat")
        .env_clear()
        .output()?;
    ensure!(out.status.success(), "vm_stat failed");
    let stats = String::from_utf8(out.stdout)?;
    let mut total: u64 = 0;
    let mut free: u64 = 0;
    let mut matched = 0;
    for line in stats.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let reclaimable = match key {
            "Pages free" | "Pages inactive" | "Pages speculative" => true,
            "Pages active" | "Pages wired down" | "Pages occupied by compressor" => false,
            _ => continue,
        };
        let value: u64 = value
            .trim()
            .trim_end_matches('.')
            .parse()
            .context("invalid vm_stat page count")?;
        total = total.saturating_add(value);
        if reclaimable {
            free = free.saturating_add(value);
        }
        matched += 1;
    }
    ensure!(matched == 6 && total > 0, "incomplete vm_stat counters");
    Ok(Pressure::from_used_percent(
        total.saturating_sub(free).saturating_mul(100) / total,
    ))
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn host_pressure() -> Result<Pressure> {
    anyhow::bail!("fast lane memory admission is unavailable on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, thread};

    const TEST_GUARD: Duration = Duration::from_secs(5);

    fn assert_heavy_load(resources: &Resources, running: usize, queued: usize) {
        let load = resources.snapshot().unwrap();
        assert_eq!((load.heavy_running, load.heavy_queued), (running, queued));
    }

    fn wait_for_pending(resources: &Resources, queued: usize) {
        let deadline = Instant::now() + TEST_GUARD;
        while resources.snapshot().unwrap().heavy_queued != queued {
            assert!(Instant::now() < deadline, "expected {queued} heavy waiters");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn fast_lane_rejects_pressure_and_releases_capacity() {
        let resources = Resources {
            fast_cap: 1,
            ..Resources::default()
        };
        assert!(resources.acquire(Lane::Fast, Pressure::Red).is_err());
        assert!(resources.acquire(Lane::Fast, Pressure::Yellow).is_err());
        let fast = resources.acquire(Lane::Fast, Pressure::Green).unwrap();
        assert!(resources.acquire(Lane::Fast, Pressure::Green).is_err());
        assert!(resources.acquire(Lane::Heavy, Pressure::Red).is_ok());
        drop(fast);
        assert!(resources.acquire(Lane::Fast, Pressure::Green).is_ok());
    }

    #[test]
    fn heavy_lane_bounds_concurrent_waiters_and_hands_off_in_fifo_order() {
        let resources = Resources {
            heavy_queue_cap: 3,
            heavy_wait_timeout: TEST_GUARD * 2,
            ..Resources::default()
        };
        let resources = &resources;
        thread::scope(|scope| {
            let active = resources.acquire(Lane::Heavy, Pressure::Red).unwrap();
            let (started, observed) = mpsc::channel();
            let mut releases = Vec::new();
            for id in 0..resources.heavy_queue_cap {
                let started = started.clone();
                let (release, released) = mpsc::channel();
                releases.push(release);
                scope.spawn(move || {
                    let lease = resources.acquire(Lane::Heavy, Pressure::Red).unwrap();
                    started.send(id).unwrap();
                    released.recv_timeout(TEST_GUARD).unwrap();
                    drop(lease);
                });
                // Hold the running lease while registering each contender so
                // expected enqueue order does not depend on thread scheduling.
                wait_for_pending(resources, id + 1);
            }
            assert_heavy_load(resources, 1, resources.heavy_queue_cap);
            assert!(resources.acquire(Lane::Fast, Pressure::Green).is_err());
            let error = resources.acquire(Lane::Heavy, Pressure::Red).err().unwrap();
            assert!(error.to_string().contains("heavy lane queue full"));
            assert_heavy_load(resources, 1, resources.heavy_queue_cap);

            drop(active);
            for (id, release) in releases.into_iter().enumerate() {
                assert_eq!(observed.recv_timeout(TEST_GUARD).unwrap(), id);
                assert_heavy_load(resources, 1, resources.heavy_queue_cap - id - 1);
                release.send(()).unwrap();
            }
        });
        assert_heavy_load(resources, 0, 0);
    }

    #[test]
    fn heavy_lane_timeout_clears_pending_and_restores_admission() {
        let resources = Resources {
            heavy_queue_cap: 1,
            heavy_wait_timeout: Duration::from_millis(75),
            ..Resources::default()
        };
        let resources = &resources;
        thread::scope(|scope| {
            let active = resources.acquire(Lane::Heavy, Pressure::Red).unwrap();
            for _ in 0..2 {
                let (result, observed) = mpsc::channel();
                scope.spawn(move || {
                    let outcome = resources.acquire(Lane::Heavy, Pressure::Red).map(drop);
                    result.send(outcome).unwrap();
                });
                let error = observed.recv_timeout(TEST_GUARD).unwrap().unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("heavy lane queue wait timed out")
                );
                assert_heavy_load(resources, 1, 0);
                // A timed-out Heavy request must not keep Fast admission blocked.
                assert!(resources.acquire(Lane::Fast, Pressure::Green).is_ok());
            }
            drop(active);
        });
        assert_heavy_load(resources, 0, 0);
        assert!(resources.acquire(Lane::Heavy, Pressure::Red).is_ok());
        assert_heavy_load(resources, 0, 0);
    }
}
