//! Checksum and digest BIFs for `erlang`: `crc32/1,2`, `crc32_combine/3`,
//! `adler32/1,2`, `adler32_combine/3`, `md5/1`, `md5_init/0`, `md5_update/2` and
//! `md5_final/1`.
//!
//! Ground truth (read-only): the entries in
//! `../../erts/emulator/beam/bif.tab` and `../../erts/emulator/beam/erl_md5.c`
//! (the `erts_md5_*` context plus the `crc32`/`adler32` wrappers over `zlib`).
//!
//! # Roster
//!
//! `erlang:crc32/1`, `erlang:crc32/2`, `erlang:crc32_combine/3`,
//! `erlang:adler32/1`, `erlang:adler32/2`, `erlang:adler32_combine/3`,
//! `erlang:md5/1`, `erlang:md5_init/0`, `erlang:md5_update/2` and
//! `erlang:md5_final/1`.
//!
//! The digest implementations land here in a follow-up change; this module
//! currently declares only its `register` stub so the module tree and the
//! `with_core_bifs` wiring are in place.

/// Registers this module's BIFs (none yet).
pub(crate) fn register(reg: &mut crate::registry::BifRegistry) {
    let _ = reg;
}
