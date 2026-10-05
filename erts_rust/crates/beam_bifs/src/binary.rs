//! The `binary` module BIFs (`binary:at/2`, `part/2,3`, `copy/1,2`,
//! `list_to_bin/1`, `bin_to_list/1,2,3`, `decode_unsigned/1,2`,
//! `encode_unsigned/1,2`, `first/1`, `last/1`, `split/2,3`, `concat/2`,
//! `match/2,3`, `longest_common_prefix/1`).
//!
//! Ground truth (read-only): `../../erts/emulator/beam/erl_bif_binary.c` and the
//! `bif.tab` roster (`bif binary:at/2`, `ubif binary:part/3`, …).
//!
//! Registers its BIFs through [`register`]; the shared heap-bitstring helpers live
//! in [`crate::bits`].

use crate::registry::BifRegistry;

/// Registers the `binary` module BIFs.
pub(crate) fn register(_reg: &mut BifRegistry) {}
