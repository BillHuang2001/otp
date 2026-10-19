// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

//! The runtime half of `enif_select` (contract A.3): the mode decode, the recipient resolution and
//! the C's return codes.
//!
//! The *mechanism* — one poller, one registration per descriptor, the one-shot readiness report and
//! the notification's payload shapes — is the port layer's
//! ([`crate::port::select`] and [`PortRegistry::arm_nif_select`](crate::port::PortRegistry::arm_nif_select)).
//! What is left, and what this module owns, is exactly the C's argument handling in
//! `erts/emulator/sys/common/erl_check_io.c`:
//!
//! * **the recipient** — `const Eterm recipient = pid ? pid->pid : env->proc->common.id;`
//!   (`:1256`). The ABI layer has already dereferenced the `ErlNifPid*` and turned a null pointer
//!   into `None`, so [`recipient`] is that one expression, with the host's own pid term standing in
//!   for `env->proc->common.id`.
//! * **the interest mask** — `ERL_DRV_READ` ⇒ `ERTS_POLL_EV_IN` (`:1294`), `ERL_DRV_WRITE` ⇒
//!   `ERTS_POLL_EV_OUT` (`:1297`), `ERL_NIF_SELECT_ERROR` ⇒ `ERTS_POLL_EV_ERR` (`:1300`), combined
//!   with `|=`. [`interest_of`] is that mapping onto [`beam_io::Interest`].
//! * **the control modifiers** — `STOP` (`:1272-1290`) and `CANCEL` (`:1292`, the
//!   `ERL_NIF_SELECT_*_CANCELLED` bits at `:1470-1482`) are *not* directions: they are decided
//!   before the mask is read and never register anything.
//! * **the codes** — `INT_MIN | ERL_NIF_SELECT_INVALID_EVENT` (`:1264-1268`), `0` for an accepted
//!   registration (`:1464`) and for a mask naming no direction (`:1467`), `INT_MIN |
//!   ERL_NIF_SELECT_FAILED` when the poll set refuses the descriptor (`:1433`), the OR-ed
//!   `*_CANCELLED` bits for a `CANCEL` (`:1470-1482`), and `ERL_NIF_SELECT_STOP_CALLED` **plus** those
//!   same `*_CANCELLED` bits for a `STOP` (`:1274`, `:1470-1482`, `:1497-1510`).
//!
//! # `ERL_NIF_SELECT_NOTSUP` is never emitted here, and why
//!
//! The C answers `INT_MIN | ERL_NIF_SELECT_NOTSUP` (`:1305`) for an error-direction select on a
//! platform whose poll backend has no error event. [`beam_io::Interest::ERROR`] is a
//! **caller-intent** bit on every backend — `epoll`/`kqueue` report an error condition
//! unconditionally in every round, which is precisely what [`beam_io::Ready::is_error`] surfaces —
//! so this poller serves the request everywhere, exactly as the C's epoll build does (the C's own
//! `#if` guards `ERTS_POLL_EV_ERR` behind `ERTS_POLL_USE_EPOLL`).
//!
//! # The registration's handle, and the stop callback
//!
//! A NIF select's `(fd, handle)` index key is the **descriptor** ([`handle_of`], a small integer
//! term): the C addresses a `STOP`/`CANCEL` by descriptor alone (`:1272-1284`), so one registration
//! per descriptor is the whole address space, and the plain payload's second element is the
//! *minted* resource term ([`crate::port::select::SelectPayload`]) rather than this key — never
//! rendered.
//!
//! `ERL_NIF_SELECT_STOP` answers `ERL_NIF_SELECT_STOP_CALLED` **plus the `*_CANCELLED` bit of every
//! direction it cleared**: the C decides the stop before the mask is decoded, rewrites the mode to
//! `ERL_DRV_READ | ERL_DRV_WRITE | ERL_DRV_USE` (`:1274-1276`) and runs its "off" path, which ORs
//! those bits (`:1470-1482`) and then adds `ERL_NIF_SELECT_STOP_CALLED` (`:1497-1510`). A stop on a
//! descriptor that was never armed is `ERL_NIF_SELECT_STOP_CALLED` alone — the C's own fast track
//! (`call_stop = CALL_STOP; ret = ERL_NIF_SELECT_STOP_CALLED;`, `:1276-1280`). **Residual:** the
//! resource type's `stop` callback has no seam on this side — it is the resource registry's
//! (`beam_abi`), reached from the `enif_*` entry point rather than from a host hook — so no `stop`
//! callback is invoked and `ERL_NIF_SELECT_STOP_SCHEDULED` (`:1500`) is never answered.

use core::ffi::{c_int, c_void};
use std::cell::RefMut;
use std::os::fd::RawFd;
use std::rc::Rc;

use beam_abi::common::{
    ErlNifSelectFlags, ERL_NIF_SELECT_ERROR_CANCELLED, ERL_NIF_SELECT_FAILED,
    ERL_NIF_SELECT_INVALID_EVENT, ERL_NIF_SELECT_READ_CANCELLED, ERL_NIF_SELECT_STOP_CALLED,
    ERL_NIF_SELECT_WRITE_CANCELLED,
};
use beam_abi::nif::types::ErlNifEvent;
use beam_io::Interest;
use beam_process::{ProcessId, ProcessTable};
use beam_term::prelude::make_small;
use beam_term::Eterm;

use crate::nif::RuntimeNifServices;
use crate::port::select::{Cancelled, SelectPayload};
use crate::port::PortRegistry;

/// Register, cancel or stop the poll interest the request names — the runtime half of
/// `enif_select`.
///
/// `self_pid` is the calling host's own pid term (the C's `env->proc->common.id`, `:1256`) and
/// `pid` the C's `const ErlNifPid*` after the ABI dereferenced it (`None` for a null pointer).
/// `reference` is the caller's reference operand — or, with `ERL_NIF_SELECT_CUSTOM_MSG`, the
/// message term itself (`:716-731`).
///
/// Answers the C's return code, or `None` when this provider cannot service the request at all
/// (the port registry is already borrowed, or the recipient names no process), which
/// [`beam_abi`]'s `enif_select` turns into its own `-1` — its documented "not serviceable".
pub(crate) fn select(
    services: &mut RuntimeNifServices<'_>,
    self_pid: Eterm,
    event: ErlNifEvent,
    mode: ErlNifSelectFlags,
    obj: *mut c_void,
    pid: Option<Eterm>,
    reference: Eterm,
) -> Option<c_int> {
    // The invalid-event path, decided exactly where the C decides it — before any fd state is
    // touched (`grow_drv_ev_state` refuses a negative descriptor, `:1264-1268`).
    if event < 0 {
        return Some(c_int::MIN | ERL_NIF_SELECT_INVALID_EVENT);
    }
    let fd: RawFd = event;
    // `:1256` — the recipient, resolved before anything can be registered for a process that does
    // not exist. `enif_select`'s recipient is stored and delivered later, so this is the only
    // resolution point.
    let owner = recipient(services, pid, self_pid)?;
    let directions = interest_of(mode);

    // `STOP` (`:1272-1290`): delete the descriptor's registration and answer the C's
    // "stop callback called". It wins over every other bit, as in the C (`mode` is rewritten to
    // `READ | WRITE | USE` there and the mask is never decoded).
    if mode.intersects(ErlNifSelectFlags::STOP) {
        let stopped = {
            let mut registry = borrow_ports(services)?;
            registry.stop_nif_select(fd, handle_of(fd))
        };
        return Some(match stopped {
            // The C rewrites the mode to `ERL_DRV_READ | ERL_DRV_WRITE | ERL_DRV_USE` (`:1274`)
            // and runs its "off" path, which ORs one `*_CANCELLED` bit per direction it really
            // cleared (`:1470-1482`) *before* adding `ERL_NIF_SELECT_STOP_CALLED` (`:1497-1510`),
            // so a stopped read interest answers `STOP_CALLED | READ_CANCELLED`.
            Ok(cancelled) => ERL_NIF_SELECT_STOP_CALLED | cancel_code(cancelled),
            Err(_) => c_int::MIN | ERL_NIF_SELECT_FAILED,
        });
    }

    // `CANCEL` (`:1292`, the `*_CANCELLED` bits at `:1470-1482`): clear the named directions and
    // report which of them really were armed. A mask naming no direction cancels nothing and is
    // the C's own `ret = 0`.
    if mode.intersects(ErlNifSelectFlags::CANCEL) {
        let cancelled = match directions {
            None => Cancelled::default(),
            Some(interest) => {
                let cancelled = {
                    let mut registry = borrow_ports(services)?;
                    registry.cancel_nif_select(fd, handle_of(fd), interest)
                };
                match cancelled {
                    Ok(cancelled) => cancelled,
                    Err(_) => return Some(c_int::MIN | ERL_NIF_SELECT_FAILED),
                }
            }
        };
        return Some(cancel_code(cancelled));
    }

    // A mask that names no direction after the modifiers registers nothing — the C's `ret = 0`
    // (`:1467`, the "off" arm's answer when no interest bit was set).
    let Some(interest) = directions else {
        return Some(0);
    };

    // The payload, **stabilised**: the registration outlives the NIF call that armed it, so both
    // shapes are copied onto the runtime's never-reset arena (`Runtime::stabilize_term`) before the
    // port layer stores them.
    let payload = match services.runtime.stabilize_term(reference) {
        Ok(term) if mode.intersects(ErlNifSelectFlags::CUSTOM_MSG) => SelectPayload::Custom {
            message: term,
        },
        Ok(term) => SelectPayload::Plain { reference: term },
        // The reference/message term cannot be re-homed: the C has no code for it, and a
        // registration whose payload would dangle is not one this runtime may make.
        Err(_) => return Some(c_int::MIN | ERL_NIF_SELECT_FAILED),
    };

    // The arm (`:1447-1464`): `0` on success. A descriptor the poll set refuses (a closed one, the
    // C's `ERTS_POLL_EV_NVAL`) is `INT_MIN | ERL_NIF_SELECT_FAILED` (`:1433`); the key
    // reservation's exhaustion is the same answer. `obj` is **moved**, never dereferenced — that is
    // what `#![deny(unsafe_code)]` requires of this crate, and the mint happens at delivery.
    let armed = {
        let mut registry = borrow_ports(services)?;
        registry.arm_nif_select(owner, fd, interest, payload, handle_of(fd), obj)
    };
    Some(match armed {
        Ok(()) => 0,
        Err(_) => c_int::MIN | ERL_NIF_SELECT_FAILED,
    })
}

/// The C's `recipient` (`:1256`): `pid` when the caller named one, else the calling process.
///
/// A `pid` term that is not an internal pid names no process this runtime can deliver to, and the
/// C stores whatever it is handed and drops the notification later; there is no return code for
/// it. `None` is this provider's "cannot service" — the ABI's `-1` — rather than a registration
/// whose notification could never be delivered.
///
/// The calling-process case additionally asks the process table: the C's `env->proc` is live by
/// construction (the NIF is running on it), while this host's `self_pid` is a term that could name
/// an already-terminated process on a nested call, and a registration for a dead owner is one the
/// port layer would prune at its next round.
fn recipient(
    services: &RuntimeNifServices<'_>,
    pid: Option<Eterm>,
    self_pid: Eterm,
) -> Option<ProcessId> {
    match pid {
        Some(term) => ProcessId::from_eterm(term).ok(),
        None => {
            let owner = ProcessId::from_eterm(self_pid).ok()?;
            is_live(services, owner).then_some(owner)
        }
    }
}

/// Whether `pid` names a live process, resolved exactly as
/// [`NifHostServices::is_process_alive`](crate::nif::NifHostServices) resolves it.
fn is_live(services: &RuntimeNifServices<'_>, pid: ProcessId) -> bool {
    let table: &ProcessTable = services.processes();
    table.is_process_alive(pid)
}

/// The poller interest `mode` asks for, or `None` when it names no direction (`:1294-1307`).
///
/// `ERL_DRV_READ`/`ERL_DRV_WRITE` are `#define`s of `ERL_NIF_SELECT_READ`/`_WRITE`
/// (`erts/emulator/beam/erl_driver.h:61-63`), so one decode serves both spellings.
pub(crate) fn interest_of(mode: ErlNifSelectFlags) -> Option<Interest> {
    let base = match (
        mode.intersects(ErlNifSelectFlags::READ),
        mode.intersects(ErlNifSelectFlags::WRITE),
    ) {
        (true, true) => Some(Interest::BOTH),
        (true, false) => Some(Interest::READABLE),
        (false, true) => Some(Interest::WRITABLE),
        (false, false) => None,
    };
    match (base, mode.intersects(ErlNifSelectFlags::ERROR)) {
        (Some(interest), true) => Some(interest.add(Interest::ERROR)),
        (Some(interest), false) => Some(interest),
        (None, true) => Some(Interest::ERROR),
        (None, false) => None,
    }
}

/// The C's answer to a `CANCEL` (`:1470-1482`): `0` plus a bit per direction that really was
/// armed.
fn cancel_code(cancelled: Cancelled) -> c_int {
    let mut code = 0;
    if cancelled.readable {
        code |= ERL_NIF_SELECT_READ_CANCELLED;
    }
    if cancelled.writable {
        code |= ERL_NIF_SELECT_WRITE_CANCELLED;
    }
    if cancelled.error {
        code |= ERL_NIF_SELECT_ERROR_CANCELLED;
    }
    code
}

/// The `(fd, handle)` index key of a NIF select — the descriptor itself (see the module docs).
fn handle_of(fd: RawFd) -> Eterm {
    make_small(fd as i64)
}

/// The port registry, borrowed mutably — a nested borrow is the typed "cannot service" (`None`),
/// never a panic.
fn borrow_ports<'r>(
    services: &'r mut RuntimeNifServices<'_>,
) -> Option<RefMut<'r, PortRegistry>> {
    // The `Rc` is cloned so the borrow is of the registry itself, and the clone is leaked into the
    // returned `RefMut`'s scope by being dropped only after it — `RefMut` borrows the `RefCell`
    // inside it, so the clone must outlive the guard. Handing back a guard over a temporary is
    // exactly what `RefCell` forbids, so the registry `Rc` is stored in the runtime: the guard
    // borrows `services.runtime.ports` for the caller's scope, and the runtime outlives the call.
    let ports: &'r Rc<std::cell::RefCell<PortRegistry>> = &services.runtime.ports;
    ports.try_borrow_mut().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every direction bit maps onto exactly its own interest, and the combinations union — the C's
    /// `ctl_events |=` (`:1294-1307`).
    #[test]
    fn the_mode_decode_covers_every_combination() {
        let read = ErlNifSelectFlags::READ;
        let write = ErlNifSelectFlags::WRITE;
        let error = ErlNifSelectFlags::ERROR;
        assert_eq!(interest_of(read), Some(Interest::READABLE));
        assert_eq!(interest_of(write), Some(Interest::WRITABLE));
        assert_eq!(interest_of(error), Some(Interest::ERROR));
        assert_eq!(
            interest_of(ErlNifSelectFlags::from_bits(read.bits() | write.bits())),
            Some(Interest::BOTH)
        );
        assert_eq!(
            interest_of(ErlNifSelectFlags::from_bits(read.bits() | error.bits())),
            Some(Interest::READABLE.add(Interest::ERROR))
        );
        assert_eq!(
            interest_of(ErlNifSelectFlags::from_bits(write.bits() | error.bits())),
            Some(Interest::WRITABLE.add(Interest::ERROR))
        );
        assert_eq!(
            interest_of(ErlNifSelectFlags::from_bits(
                read.bits() | write.bits() | error.bits()
            )),
            Some(Interest::BOTH.add(Interest::ERROR))
        );
    }

    /// The control modifiers are **not** directions: a modifier-only mask registers nothing (the
    /// C's "off" arm, `:1467`), which is what makes `STOP`/`CANCEL` answer `0`-family codes
    /// instead of arming an empty interest.
    #[test]
    fn the_control_modifiers_name_no_direction() {
        for modifier in [
            ErlNifSelectFlags::STOP,
            ErlNifSelectFlags::CANCEL,
            ErlNifSelectFlags::CUSTOM_MSG,
            ErlNifSelectFlags::from_bits(
                ErlNifSelectFlags::STOP.bits() | ErlNifSelectFlags::CANCEL.bits(),
            ),
        ] {
            assert_eq!(interest_of(modifier), None);
        }
        // `CUSTOM_MSG` combined with a direction is still that direction: it replaces the
        // notification, not the registration.
        assert_eq!(
            interest_of(ErlNifSelectFlags::from_bits(
                ErlNifSelectFlags::CUSTOM_MSG.bits() | ErlNifSelectFlags::READ.bits()
            )),
            Some(Interest::READABLE)
        );
    }

    /// The `CANCEL` answer is `0` plus one bit per direction that really was armed (`:1470-1482`).
    #[test]
    fn the_cancel_code_reports_each_armed_direction() {
        assert_eq!(cancel_code(Cancelled::default()), 0);
        assert_eq!(
            cancel_code(Cancelled {
                readable: true,
                writable: false,
                error: false
            }),
            ERL_NIF_SELECT_READ_CANCELLED
        );
        assert_eq!(
            cancel_code(Cancelled {
                readable: false,
                writable: true,
                error: false
            }),
            ERL_NIF_SELECT_WRITE_CANCELLED
        );
        assert_eq!(
            cancel_code(Cancelled {
                readable: false,
                writable: false,
                error: true
            }),
            ERL_NIF_SELECT_ERROR_CANCELLED
        );
        assert_eq!(
            cancel_code(Cancelled {
                readable: true,
                writable: true,
                error: true
            }),
            ERL_NIF_SELECT_READ_CANCELLED
                | ERL_NIF_SELECT_WRITE_CANCELLED
                | ERL_NIF_SELECT_ERROR_CANCELLED
        );
    }

    /// The registration key is the descriptor, so one registration per descriptor is the whole
    /// address space the `(fd, handle)` index can name — the C's own rule (`:1272-1284`).
    #[test]
    fn the_handle_is_the_descriptor() {
        assert_eq!(handle_of(0), make_small(0));
        assert_eq!(handle_of(7), make_small(7));
        assert_ne!(handle_of(7), handle_of(8));
    }
}
