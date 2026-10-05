#![doc = "Layout-frozen NIF (`enif_*`) and driver (`driver_*`/`erl_drv_*`) C ABI declarations."]
//!
//! `beam_abi` owns the **frozen foreign-function C ABIs** the Rust ERTS exposes to native code:
//! the NIF C ABI (workspace compatibility contract **A.3**) and the driver C ABI (**A.4**). It
//! reproduces the exact struct layouts, enum widths, function-pointer signatures and constants of
//! the legacy headers (`erts/emulator/beam/erl_nif.h`, `erl_nif_api_funcs.h`, `erl_drv_nif.h`,
//! `erl_driver.h`) so that a NIF/driver compiled against the *unmodified* OTP headers links against
//! Rust ERTS and behaves identically. This is the **link-time frozen-header seam** of Part II §C.
//!
//! The crate **declares types only** — no `enif_*`/`erl_drv_*` function bodies live here (those
//! belong to the NIF/driver subsystem in a later pass). What is frozen now is the shape of the
//! boundary: the types, the layouts, the term transmute, and the `ErlNifApi`/`ErlDrvApi` tables the
//! runtime must implement and export.
//!
//! Layouts are pinned by compile-time assertions in [`layout`]; a mismatch is a compile error.
//!
//! See `CONTEXT.md` for the ABI version targeted (`ERL_NIF_MAJOR_VERSION` 2 / minor 18,
//! `ERL_DRV_EXTENDED_MAJOR_VERSION` 3 / minor 3), the full frozen layout table, and the design
//! decisions (flag newtypes, opaque handles, the term transmute seam).

pub mod common;
pub mod driver;
pub mod layout;
pub mod nif;
