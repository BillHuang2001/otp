//! **Port lifecycle, failures & attributes** family of the driver host layer (part of the
//! `driver_*` / `erl_drv_*` C ABI, workspace contract **A.4**).
//!
//! This file owns the slot list (and, in a later workstream, the implementations) for the
//! functions that close a port, set its busy/control state, register/remove a statically linked
//! driver and create/lock ports. It contributes no slots yet: [`populate`] is a no-op today, so
//! every one of them stays `None` in the frozen `ErlDrvApi` table — a driver that reaches one gets
//! the C's failure behaviour, never a crash.
//!
//! # Slots owned by this file
//!
//! * busy-message-queue window — `erl_drv_busy_msgq_limits`
//! * failures — `driver_failure_eof`, `driver_failure_atom`, `driver_failure_posix`,
//!   `driver_failure`, `driver_exit`
//! * port attributes/flags — `set_busy_port`, `set_port_control_flags`, `get_port_flags`
//! * driver registry — `add_driver_entry`, `remove_driver_entry`, `driver_lock_driver`
//! * port creation & async — `driver_create_port`, `driver_async_port_key`
//! * asynchronous init — `erl_drv_init_ack`, `erl_drv_set_os_pid`
//!
//! # Modelling notes (for the implementing workstream)
//!
//! Every port-carrying member here delegates to the
//! [`DriverHost`](crate::driver::host::DriverHost) seam: `busy_msgq_limits`, `failure`
//! (the single hook behind the whole `driver_failure*` family, taking a
//! [`PortFailure`](crate::driver::host::PortFailure) reason), `port_exit`, `set_busy_port`,
//! `set_control_flags`, `get_port_flags`, `init_ack`, `set_os_pid` and `async_port_key`.
//! `add_driver_entry`/`remove_driver_entry` are process-global driver-registry operations and
//! `driver_create_port` builds a new port, so those three need their own runtime service rather
//! than a per-port host hook.

use crate::driver::api::ErlDrvApi;

/// Populate this family's slots of the frozen [`ErlDrvApi`].
///
/// No-op stub: the port-lifecycle family is not implemented yet, so every slot it owns stays
/// `None`. Wiring a slot is a local edit to this file (the `populate` chain in [`super`] already
/// calls this).
pub(super) const fn populate(api: ErlDrvApi) -> ErlDrvApi {
    api
}
