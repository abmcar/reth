//! Opt-in timing for the common `Handler::run_exec_loop` boundary.

use std::{cell::Cell, time::Instant};

/// Monotonic counters for completed or failed `run_exec_loop` calls.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunExecLoopMetrics {
    /// Number of `run_exec_loop` calls observed while collection was enabled.
    pub call_count: u64,
    /// Inclusive wall time spent inside `run_exec_loop`.
    pub wall_ns: u64,
}

impl RunExecLoopMetrics {
    /// Returns the checked monotonic delta from `before` to `self`.
    pub fn checked_delta(self, before: Self) -> Option<Self> {
        Some(Self {
            call_count: self.call_count.checked_sub(before.call_count)?,
            wall_ns: self.wall_ns.checked_sub(before.wall_ns)?,
        })
    }
}

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    static METRICS: Cell<RunExecLoopMetrics> = const { Cell::new(RunExecLoopMetrics {
        call_count: 0,
        wall_ns: 0,
    }) };
}

/// Enables collection on the current thread and clears prior counters.
pub fn enable() {
    METRICS.set(RunExecLoopMetrics::default());
    ENABLED.set(true);
}

/// Disables collection on the current thread.
pub fn disable() {
    ENABLED.set(false);
}

/// Returns whether collection is enabled on the current thread.
pub fn is_enabled() -> bool {
    ENABLED.get()
}

/// Returns the current monotonic snapshot.
pub fn snapshot() -> RunExecLoopMetrics {
    METRICS.get()
}

pub(crate) fn scope() -> Option<RunExecLoopScope> {
    ENABLED.get().then(|| RunExecLoopScope(Instant::now()))
}

pub(crate) struct RunExecLoopScope(Instant);

impl Drop for RunExecLoopScope {
    fn drop(&mut self) {
        let elapsed = u64::try_from(self.0.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let metrics = METRICS.get();
        METRICS.set(RunExecLoopMetrics {
            call_count: metrics.call_count.saturating_add(1),
            wall_ns: metrics.wall_ns.saturating_add(elapsed),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_are_monotonic_and_checked() {
        enable();
        let before = snapshot();
        drop(scope());
        let delta = snapshot().checked_delta(before).expect("monotonic delta");
        disable();

        assert_eq!(delta.call_count, 1);
    }
}
