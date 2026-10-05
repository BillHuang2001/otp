//! Raise helpers (`erlang:error/1,2`, `exit/1`, `throw/1`) and the
//! `beam_common::ExitReason` ↔ reason-term mapping, implemented in a follow-up step.

use crate::BifRegistry;

/// Registers this module's BIFs (implemented in a follow-up step).
pub(crate) fn register(_reg: &mut BifRegistry) {}
