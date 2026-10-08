//! The remaining runtime-coupled families (the ETS C interior, `re`, `erl_ddll`, distribution, ...).

use crate::registry::BifRegistry;

/// Registers this family's BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    let _ = reg;
}
