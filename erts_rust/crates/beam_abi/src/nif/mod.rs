//! NIF C ABI — `ERL_NIF_TERM` + the `ErlNif*` types and the `enif_*` function table.
//!
//! Reproduces the layout of `erts/emulator/beam/erl_nif.h` and the function surface of
//! `erts/emulator/beam/erl_nif_api_funcs.h`. See the crate `CONTEXT.md` for the frozen layout
//! table and the ABI version targeted.
//!
//! - [`term`] — `ERL_NIF_TERM` and the safe `Eterm <=> ERL_NIF_TERM` transmute seam.
//! - [`types`] — the `#[repr(C)]` `ErlNif*` structs/enums/flags and function-type aliases.
//! - [`api`] — `ErlNifApi`, the frozen `enif_*` function-pointer table (181 entries on LP64).

pub mod api;
pub mod term;
pub mod types;
