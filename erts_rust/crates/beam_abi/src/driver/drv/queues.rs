//! **Port queues** family of the driver host layer (part of the `driver_*` / `erl_drv_*` C ABI,
//! workspace contract **A.4**).
//!
//! This file owns the slot list (and, in a later workstream, the implementations) for the
//! functions that read and write a port's data queue. It contributes no slots yet: [`populate`] is
//! a no-op today, so every one of them stays `None` in the frozen `ErlDrvApi` table — a driver that
//! reaches one gets the C's failure behaviour, never a crash.
//!
//! # Slots owned by this file
//!
//! * `driver_vec_to_buf`
//! * `driver_enq`, `driver_pushq`, `driver_deq`, `driver_sizeq`
//! * `driver_enq_bin`, `driver_pushq_bin`
//! * `driver_peekqv`, `driver_peekq`
//! * `driver_enqv`, `driver_pushqv`
//!
//! # Modelling notes (for the implementing workstream)
//!
//! Every one of these operates on a port's queue and so delegates to the corresponding
//! [`DriverHost`](crate::driver::host::DriverHost) seam hooks — `enq`/`pushq`/`deq`/`sizeq`/
//! `peekq` — which the runtime supplies over its real port registry. The `*_bin`/`*v` variants
//! differ only in how the driver presents the bytes (an `ErlDrvBinary` + window, or an `ErlIOVec`
//! with a `skip`); the seam hands the host a plain byte slice, so those variants gather/linearize
//! before calling it.

use crate::driver::api::ErlDrvApi;

/// Populate this family's slots of the frozen [`ErlDrvApi`].
///
/// No-op stub: the port-queue family is not implemented yet, so every slot it owns stays `None`.
/// Wiring a slot is a local edit to this file (the `populate` chain in [`super`] already calls
/// this).
pub(super) const fn populate(api: ErlDrvApi) -> ErlDrvApi {
    api
}
