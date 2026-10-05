//! The allocator strategy selector (compatibility contract A.6, configuration
//! fidelity).
//!
//! Mirrors the legacy `ErtsAlcStrat_t` of `erts/emulator/beam/erl_alloc_util.h` so
//! the runtime option surface (`+MMas`, ...) can name every strategy the C VM
//! accepts. Only the **segregated first-fit** family is implemented in Milestone 1;
//! every variant currently selects that behaviour. The strategy plugin seam that
//! would let `goodfit`/`bestfit`/`afit` differ is reserved — see the crate
//! `CONTEXT.md`, Design Decisions.

/// The allocation strategy an [`crate::allocator::AllocatorConfig`] selects.
///
/// All variants are accepted for configuration fidelity, but Milestone 1 implements
/// only the segregated first-fit behaviour that [`Strategy::FirstFit`] names.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Strategy {
    /// Segregated first-fit over the per-size-class free lists (the M1 behaviour).
    #[default]
    FirstFit,
    /// Good-fit over a variable-block carrier (reserved; selects first-fit in M1).
    GoodFit,
    /// Best-fit over a variable-block carrier (reserved; selects first-fit in M1).
    BestFit,
    /// Address-order first-fit (reserved; selects first-fit in M1).
    AFirstFit,
}
