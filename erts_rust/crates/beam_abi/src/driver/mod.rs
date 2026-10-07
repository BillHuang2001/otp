//! Driver C ABI — the `ErlDrv*` types and the `driver_*`/`erl_drv_*` function table.
//!
//! Reproduces the layout of `erts/emulator/beam/erl_driver.h`. See the crate `CONTEXT.md` for the
//! frozen layout table.
//!
//! - [`types`] — the `#[repr(C)]` `ErlDrv*` structs/enums/flags, `ErlDrvEntry`, and constants.
//! - [`api`] — `ErlDrvApi`, the frozen `driver_*`/`erl_drv_*` function-pointer table.
//! - [`host`] — the driver host seam: `DriverHost` (the runtime services behind the `erl_drv_*`
//!   implementations) and `DriverEnv` (the concrete env behind the opaque `ErlDrvPort` a runtime
//!   passes to driver callbacks).
//! - [`drv`] — the implemented `driver_*`/`erl_drv_*` host functions and the populated
//!   `driver_api_table()` they are published through.
//! - [`loader`] — the dynamic driver loader: `dlopen`/`dlsym`, the `ErlDrvEntry` load contract,
//!   and `DriverLibrary`.

pub mod api;
pub mod drv;
pub mod host;
pub mod loader;
pub mod types;
