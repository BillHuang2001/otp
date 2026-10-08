//! The `erlang`-module runtime-coupled BIFs (process control, timers, code loading, ...).

use crate::registry::BifRegistry;

/// Registers this family's BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    let _ = reg;
}
