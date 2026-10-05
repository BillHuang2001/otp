//! Term construction/inspection BIFs (`element`, `setelement`, `tuple_size`,
//! `tuple_to_list`, `list_to_tuple`, `length`, `hd`, `tl`, `size`), implemented in
//! a follow-up step.

use crate::BifRegistry;

/// Registers this module's BIFs (implemented in a follow-up step).
pub(crate) fn register(_reg: &mut BifRegistry) {}
