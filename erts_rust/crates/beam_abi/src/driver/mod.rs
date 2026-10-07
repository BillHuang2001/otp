//! Driver C ABI — the `ErlDrv*` types and the `driver_*`/`erl_drv_*` function table.
//!
//! Reproduces the layout of `erts/emulator/beam/erl_driver.h`. See the crate `CONTEXT.md` for the
//! frozen layout table.
//!
//! - [`types`] — the `#[repr(C)]` `ErlDrv*` structs/enums/flags, `ErlDrvEntry`, and constants.
//! - [`api`] — `ErlDrvApi`, the frozen `driver_*`/`erl_drv_*` function-pointer table.
//! - [`host`] — the driver host seam: `DriverHost` (runtime services) + `DriverEnv` (the per-port
//!   environment behind the opaque `*mut ErlDrvPort_`) + the term-data array decoder.

pub mod api;
pub mod host;
pub mod types;
