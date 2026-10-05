//! JIT tier seam and tiering design.
//!
//! The engine ships three execution tiers, but only the **interpreter** is
//! implemented in Milestone-1:
//!
//! * `interpreter` — the direct-threaded loop (`crate::interpreter`). Always
//!   present; the vertical slice runs on it.
//! * `jit-cranelift` — the optional hot tier (Cranelift codegen), gated behind the
//!   **`jit-cranelift`** Cargo feature (default **OFF**). Deliberately off the
//!   critical path: with the feature disabled the crate carries no Cranelift
//!   dependency at all and the build never depends on Cranelift codegen. JIT
//!   compilation is *measurement-gated* (Part II §F) with a bounded-latency,
//!   cheap-deopt design.
//! * `jit-beamasm` — a Rust port of BeamAsm with block register allocation, gated
//!   behind the **`jit-beamasm`** feature. A documented stub in M1.
//!
//! [`available_tiers`] reports the tier names compiled into this build; the actual
//! backends are the manager's deliverable and are not wired up in M1.

/// A tier that can execute the typed IR.
pub trait JitBackend {
    /// The tier's stable name (e.g. `"interpreter"`), as returned by
    /// [`available_tiers`].
    fn name(&self) -> &'static str;

    /// Whether this tier is usable in the current build.
    fn available(&self) -> bool;
}

/// The execution tiers compiled into this build.
///
/// Without a JIT feature this is exactly `["interpreter"]`; enabling `jit-beamasm`
/// or `jit-cranelift` adds the corresponding (still-stubbed) tier name. The
/// interpreter is always first.
pub fn available_tiers() -> &'static [&'static str] {
    TIERS
}

#[cfg(feature = "jit-cranelift")]
const TIERS: &[&str] = &["interpreter", "jit-cranelift"];

#[cfg(all(not(feature = "jit-cranelift"), feature = "jit-beamasm"))]
const TIERS: &[&str] = &["interpreter", "jit-beamasm"];

#[cfg(all(not(feature = "jit-cranelift"), not(feature = "jit-beamasm")))]
const TIERS: &[&str] = &["interpreter"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpreter_is_always_available() {
        let tiers = available_tiers();
        assert_eq!(tiers.first().copied(), Some("interpreter"));
        assert!(!tiers.is_empty());
    }

    #[test]
    fn cranelift_tier_tracks_the_feature() {
        let has = available_tiers().contains(&"jit-cranelift");
        assert_eq!(has, cfg!(feature = "jit-cranelift"));
    }
}
