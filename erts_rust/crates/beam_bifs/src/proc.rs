//! Process BIFs: `erlang:self/0` and `erlang:send/2` (`"!"/2`), implemented in a
//! follow-up step.

use crate::BifRegistry;

/// Registers this module's BIFs (implemented in a follow-up step).
pub(crate) fn register(_reg: &mut BifRegistry) {}
