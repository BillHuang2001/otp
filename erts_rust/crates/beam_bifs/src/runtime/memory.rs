//! The `erlang:memory/0,1` accounting pair (the [`MemoryHook`](crate::context::MemoryHook) seam).

use crate::registry::BifRegistry;

/// Registers this family's BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    let _ = reg;
}
