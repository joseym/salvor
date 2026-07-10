//! Per-call resource limits: the operator-facing [`ToolLimits`] configuration
//! and the host-side [`TrackedLimits`] resource limiter that enforces the
//! memory cap while recording denials for attribution.

use wasmtime::{ResourceLimiter, StoreLimits, StoreLimitsBuilder};

/// The default wall-time cap per call, in milliseconds.
pub const DEFAULT_WALL_TIME_MS: u64 = 5_000;

/// The default memory cap per call: 128 MiB. Generous enough for an 18 MB
/// componentized-Python guest with headroom; small enough that an allocation
/// bomb dies in milliseconds.
pub const DEFAULT_MEMORY_BYTES: u64 = 128 * 1024 * 1024;

/// How often the engine-wide epoch ticker advances. A call's deadline is its
/// wall-time budget divided into these ticks, so enforcement granularity is
/// one tick.
pub(crate) const EPOCH_TICK_MS: u64 = 10;

/// The per-call resource caps for one tool, as configured by the operator.
///
/// These are limits on **one sandboxed call**, disjoint from run budgets: run
/// budgets (`steps`, `tokens`, `cost`, wall time across the run) are enforced
/// by the runtime between events, and a call killed here is a failed tool
/// call fed back to the model, never a `BudgetExceeded` park.
///
/// Wall time is the enforced default cap. For a guest that holds no blocking
/// capability (and a deny-all WASI context grants none), one epoch deadline
/// is simultaneously the CPU limit and the wall-time limit, and its unit is
/// one operators already think in. Fuel is the optional deterministic
/// alternative; its unit is per-instruction and wildly guest-dependent (the
/// same word count costs ~450x more fuel in a Python guest than in Rust), so
/// it stays opt-in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolLimits {
    /// Wall-clock budget for one call, in milliseconds. Enforced by an epoch
    /// deadline; granularity is [`EPOCH_TICK_MS`].
    pub wall_time_ms: u64,
    /// Linear-memory byte cap for one call. A grow past this is denied.
    pub memory_bytes: u64,
    /// Optional deterministic fuel budget. `None` means unmetered (the engine
    /// still runs with fuel accounting on; the tank is just set to `u64::MAX`).
    pub fuel: Option<u64>,
}

impl Default for ToolLimits {
    fn default() -> Self {
        Self {
            wall_time_ms: DEFAULT_WALL_TIME_MS,
            memory_bytes: DEFAULT_MEMORY_BYTES,
            fuel: None,
        }
    }
}

impl ToolLimits {
    /// The epoch-deadline tick count for this wall-time budget: the budget in
    /// ticks, rounded up, plus one so a budget shorter than a single tick
    /// still gets a full tick rather than zero.
    pub(crate) fn epoch_deadline_ticks(self) -> u64 {
        self.wall_time_ms.div_ceil(EPOCH_TICK_MS) + 1
    }
}

/// A [`ResourceLimiter`] wrapping [`StoreLimits`] to record *which* denials
/// happened.
///
/// The wrapping exists for error attribution. When a grow is denied, most
/// guests' allocators abort, and the host observes only an opaque
/// `unreachable` trap; without this record, "memory cap exceeded" would
/// surface as a mystery crash. After a failed call the engine reads
/// [`memory_denied`](Self::memory_denied) /
/// [`table_denied`](Self::table_denied) back off the store to attribute the
/// trap.
///
/// The static counts handed to [`StoreLimitsBuilder`] are deliberately
/// generous: a component links several core modules (the guest, the WASI
/// adapter, bindgen shims), so tight instance/table/memory counts fail
/// instantiation outright. The byte cap is the real limit.
#[derive(Default)]
pub(crate) struct TrackedLimits {
    inner: StoreLimits,
    memory_denied: bool,
    table_denied: bool,
}

impl TrackedLimits {
    /// A limiter enforcing `memory_bytes` as the linear-memory cap.
    pub(crate) fn new(memory_bytes: u64) -> Self {
        // usize::try_from only narrows on 32-bit hosts, where a >4 GiB cap
        // saturates to the platform maximum anyway.
        let cap = usize::try_from(memory_bytes).unwrap_or(usize::MAX);
        Self {
            inner: StoreLimitsBuilder::new()
                .memory_size(cap)
                .memories(8)
                .tables(16)
                .instances(32)
                .build(),
            memory_denied: false,
            table_denied: false,
        }
    }

    /// Whether a memory grow was denied during the call.
    pub(crate) fn memory_denied(&self) -> bool {
        self.memory_denied
    }

    /// Whether a table grow was denied during the call.
    pub(crate) fn table_denied(&self) -> bool {
        self.table_denied
    }
}

impl ResourceLimiter for TrackedLimits {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let allowed = self.inner.memory_growing(current, desired, maximum)?;
        if !allowed {
            self.memory_denied = true;
        }
        Ok(allowed)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let allowed = self.inner.table_growing(current, desired, maximum)?;
        if !allowed {
            self.table_denied = true;
        }
        Ok(allowed)
    }

    fn instances(&self) -> usize {
        self.inner.instances()
    }

    fn tables(&self) -> usize {
        self.inner.tables()
    }

    fn memories(&self) -> usize {
        self.inner.memories()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_values() {
        let limits = ToolLimits::default();
        assert_eq!(limits.wall_time_ms, 5_000);
        assert_eq!(limits.memory_bytes, 128 * 1024 * 1024);
        assert_eq!(limits.fuel, None);
    }

    #[test]
    fn deadline_ticks_round_up_and_never_hit_zero() {
        let ticks = |wall_time_ms: u64| {
            ToolLimits {
                wall_time_ms,
                ..ToolLimits::default()
            }
            .epoch_deadline_ticks()
        };
        assert_eq!(ticks(0), 1);
        assert_eq!(ticks(1), 2);
        assert_eq!(ticks(200), 21);
    }

    #[test]
    fn denied_memory_grow_is_recorded() {
        let mut limits = TrackedLimits::new(1024 * 1024);
        // A grow within the cap is allowed and not recorded as a denial.
        let allowed = limits.memory_growing(0, 65_536, None).expect("no error");
        assert!(allowed);
        assert!(!limits.memory_denied());
        // A grow past the cap is denied and recorded.
        let allowed = limits
            .memory_growing(0, 8 * 1024 * 1024, None)
            .expect("no error");
        assert!(!allowed);
        assert!(limits.memory_denied());
    }
}
