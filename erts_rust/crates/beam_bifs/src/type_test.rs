//! The `is_*` type-test (guard) BIFs (`is_atom/1`, `is_tuple/1`, `is_number/1`, …),
//! implemented in a follow-up step.

use crate::BifRegistry;

/// Registers this module's BIFs (implemented in a follow-up step).
pub(crate) fn register(_reg: &mut BifRegistry) {}
