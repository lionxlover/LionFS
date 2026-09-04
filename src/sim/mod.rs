//! # The deterministic simulator (Phase 8, ②)
//!
//! FoundationDB's simulation discipline, applied to the LionFS 3.0
//! policy stack: **every policy object takes caller-supplied time,
//! so the whole stack can run on a simulated clock, bit-identically,
//! with faults injected at deterministic points.** The same seed
//! produces the same universe, every time, on every platform --
//! which is what makes "we crashed at op #k, for every k" a test
//! suite instead of a soak farm.
//!
//! [`SimClock`] is the clock; [`SimRng`] is the seeded entropy; the
//! [`crash`] module is the full-stack crash simulator over the Phase
//! 8 wiring (QoS gate → record-log router → GC loop → retention
//! daemon → telemetry bridge), with power cuts modeled as log-image
//! truncations at seeded byte offsets and replay convergence checked
//! as an invariant, not an observation.
//!
//! ## Why determinism is the whole point
//!
//! A crash-recovery bug that appears once per 10^6 random crashes is
//! invisible to CI and inevitable in production. Under a seeded,
//! exhaustive crash-point sweep, that same bug is a failing test at
//! op #k, reproducible with `--seed S --crash-at k`. The
//! combinatorics:
//!
//! $$N_{\text{universes}} = \underbrace{|\text{seeds}|}_{\text{workloads}}
//!   \times \underbrace{N_{\text{ops}}}_{\text{crash points}} \times
//!   \underbrace{|\text{trunc offsets}|}_{\text{tear points}}$$
//!
//! -- each axis is a loop index, not a probability.

/// The simulated clock: a plain u64 nanosecond counter the harness
/// advances. No wall-clock reads anywhere under `sim`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SimClock {
    now_ns: u64,
}

impl SimClock {
    #[must_use]
    pub fn new(start_ns: u64) -> Self {
        Self { now_ns: start_ns }
    }

    /// Advances by `ns` and returns the new time.
    pub fn advance(&mut self, ns: u64) -> u64 {
        self.now_ns = self.now_ns.saturating_add(ns);
        self.now_ns
    }

    /// Current time.
    #[must_use]
    pub fn now(&self) -> u64 {
        self.now_ns
    }
}

/// Seeded xorshift64* PRNG: stable across platforms (no
/// multiplication-order surprises in the low bits we use), fast,
/// and enough entropy for workload scripting. This is *not* a
/// CSPRNG -- it is a universe generator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimRng {
    state: u64,
}

impl SimRng {
    /// A universe from a seed (0 is remapped: the all-zero state is
    /// the xorshift fixed point).
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed },
        }
    }

    /// Next raw u64.
    pub fn next_u64(&mut self) -> u64 {
        // xorshift64* (Marsaglia / Vigna).
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `0..bound` (bound > 0), via rejection-free modulo
    /// (bias is negligible for the bounds the simulator uses).
    #[must_use]
    pub fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound.max(1)
    }

    /// The current state (determinism introspection: two sims at the
    /// same op count with equal states are in lockstep).
    #[must_use]
    pub fn state(&self) -> u64 {
        self.state
    }
}

pub mod crash;

pub use crash::{CrashMode, CrashSimulator, SimReport};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_advances_monotonically_and_saturates() {
        let mut clock = SimClock::new(1_000);
        assert_eq!(clock.advance(5), 1_005);
        assert_eq!(clock.now(), 1_005);
        assert_eq!(clock.advance(u64::MAX), u64::MAX);
    }

    #[test]
    fn rng_is_deterministic_per_seed() {
        let mut a = SimRng::new(42);
        let mut b = SimRng::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn rng_differs_across_seeds() {
        let mut a = SimRng::new(1);
        let mut b = SimRng::new(2);
        let differs = (0..8).any(|_| a.next_u64() != b.next_u64());
        assert!(differs);
    }

    #[test]
    fn zero_seed_is_remapped_not_stuck() {
        let mut rng = SimRng::new(0);
        let first = rng.next_u64();
        let second = rng.next_u64();
        assert_ne!(first, second);
        assert_ne!(first, 0);
    }

    #[test]
    fn below_respects_the_bound() {
        let mut rng = SimRng::new(7);
        for _ in 0..1000 {
            assert!(rng.below(10) < 10);
        }
    }
}
