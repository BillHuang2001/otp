//! Arithmetic BIFs for `erlang` (`+`, `-`, `*`, `/`, `div`, `rem`, `abs` and the
//! unary `-`/`+`), implemented in a follow-up step.

use crate::BifRegistry;

/// Registers this module's BIFs (implemented in a follow-up step).
pub(crate) fn register(_reg: &mut BifRegistry) {}
