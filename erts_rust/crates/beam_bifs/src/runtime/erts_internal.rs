//! The `erts_internal` runtime-coupled BIFs (the resource-object stores, ...).

use crate::registry::BifRegistry;

/// Registers this family's BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    let _ = reg;
}
