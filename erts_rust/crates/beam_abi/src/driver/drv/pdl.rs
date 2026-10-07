//! **Port data lock, monitors & binary refcounts** family of the driver host layer (part of the
//! `driver_*` / `erl_drv_*` C ABI, workspace contract **A.4**).
//!
//! This file owns the slot list (and, in a later workstream, the implementations) for the
//! per-port data lock (PDL), the process-monitor family, the (deprecated) wall-clock accessor and
//! the binary refcount trio. It contributes no slots yet: [`populate`] is a no-op today, so every
//! one of them stays `None` in the frozen `ErlDrvApi` table — a driver that reaches one gets the
//! C's failure behaviour, never a crash.
//!
//! # Slots owned by this file
//!
//! * port data lock — `driver_pdl_create`, `driver_pdl_lock`, `driver_pdl_unlock`,
//!   `driver_pdl_get_refc`, `driver_pdl_inc_refc`, `driver_pdl_dec_refc`
//! * monitors — `driver_monitor_process`, `driver_demonitor_process`,
//!   `driver_get_monitored_process`, `driver_compare_monitors`
//! * binary refcounts — `driver_binary_get_refc`, `driver_binary_inc_refc`,
//!   `driver_binary_dec_refc`
//! * deprecated — `driver_get_now` (`ERL_DRV_DEPRECATED_FUNC`)
//!
//! # Modelling notes (for the implementing workstream)
//!
//! The PDL and the monitor family delegate to the
//! [`DriverHost`](crate::driver::host::DriverHost) seam (`pdl_*`, `monitor_process`/
//! `demonitor_process`/`get_monitored_process`, `now`). `driver_compare_monitors` compares two
//! `ErlDrvMonitor` payloads and needs **no** host state.
//!
//! The **binary refcount trio stays deliberately `None`** and should not be wired: the frozen
//! [`ErlDrvBinary`](crate::driver::types::ErlDrvBinary) layout is exactly `{ orig_size, orig_bytes }`
//! (16 bytes) — there is no refcount word, so there is nothing to report or adjust, and the
//! emulator's private `Binary` header is not part of this ABI.

use crate::driver::api::ErlDrvApi;

/// Populate this family's slots of the frozen [`ErlDrvApi`].
///
/// No-op stub: the PDL/monitor family is not implemented yet, so every slot it owns stays `None`.
/// Wiring a slot is a local edit to this file (the `populate` chain in [`super`] already calls
/// this).
pub(super) const fn populate(api: ErlDrvApi) -> ErlDrvApi {
    api
}
