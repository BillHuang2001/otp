//! `erlang` time and unique-integer BIFs: `monotonic_time/0,1`,
//! `system_time/0,1`, `time_offset/0,1`, `unique_integer/0,1`, `timestamp/0`.
//!
//! Ground truth (read-only): `../../erts/emulator/beam/erl_bif_time.c`, `bif.tab`.
//! `monotonic_time/1` and `system_time/1` take a unit atom (`second` `millisecond`
//! `microsecond` `nanosecond` `native`) and return the value in that unit.

use crate::registry::BifRegistry;

/// Registers the `erlang` time/unique-integer BIFs.
pub(crate) fn register(_reg: &mut BifRegistry) {}
