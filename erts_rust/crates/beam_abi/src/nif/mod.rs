//! NIF C ABI — `ERL_NIF_TERM` + the `ErlNif*` types and the `enif_*` function table.
//!
//! Reproduces the layout of `erts/emulator/beam/erl_nif.h` and the function surface of
//! `erts/emulator/beam/erl_nif_api_funcs.h`. See the crate `CONTEXT.md` for the frozen layout
//! table and the ABI version targeted.
//!
//! - [`term`] — `ERL_NIF_TERM` and the safe `Eterm <=> ERL_NIF_TERM` transmute seam.
//! - [`types`] — the `#[repr(C)]` `ErlNif*` structs/enums/flags and function-type aliases.
//! - [`api`] — `ErlNifApi`, the frozen `enif_*` function-pointer table (176 entries on LP64).
//! - [`host`] — the host seam: [`host::NifHost`] (the runtime services behind the `enif_*`
//!   implementations) and [`host::NifEnv`] (the concrete env behind the opaque `*mut ErlNifEnv`).
//! - [`enif`] — the `enif_*` host functions (the Rust side of the NIF C ABI) and the populated
//!   [`enif::enif_api_table`] they are published through.
//! - [`loader`] — the dynamic NIF loader: `dlopen`/`dlsym`, the `ErlNifEntry` load-time contract,
//!   and a callable [`loader::NifLibrary`].

pub mod api;
pub mod enif;
pub mod host;
pub mod loader;
pub mod term;
pub mod types;
