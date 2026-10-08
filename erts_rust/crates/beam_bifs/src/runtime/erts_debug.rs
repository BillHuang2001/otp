//! The `erts_debug`/`erl_debugger` runtime-coupled BIFs (the internal debugger tools).

use crate::registry::BifRegistry;

/// Registers this family's BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    let _ = reg;
}
