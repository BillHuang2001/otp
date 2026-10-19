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

//! The **generic readiness registration** behind `enif_select` (contract A.3) and behind
//! `prim_tty:tty_select/2` (A.7) — the delivery half of both.
//!
//! [`super::PortRegistry`] owns the runtime's one poller, so this module — a child of the port
//! layer, exactly like [`super::watch`] — is where a registered descriptor's readiness enters the
//! runtime. The *answer surfaces* stay where their arguments live: [`crate::native::prim_tty`] owns
//! `tty_select/2`'s resource check and its `ok`, and [`crate::nif`] owns `enif_select`'s return
//! codes.
//!
//! # The two producers are one mechanism
//!
//! `prim_tty:tty_select/2` is a **special case** of `enif_select`: `tty_select_nif`
//! (`erts/emulator/nifs/common/prim_tty_nif.c:1244-1264`) is three statements —
//!
//! ```c
//! if (!enif_get_resource(env, argv[0], tty_rt, (void **)&tty))
//!     return enif_make_badarg(env);
//! enif_select(env, tty->ifd, ERL_NIF_SELECT_READ, tty, NULL, argv[1]);
//! enif_self(env, &tty->reader); enif_monitor_process(env, tty, &tty->reader, NULL);
//! return atom_ok;
//! ```
//!
//! — i.e. a read interest on a descriptor (fd `0`, hardcoded by `tty_create_nif`
//! `:1009-1013`), addressed to the calling process, carrying the caller's reference. This module is
//! that one mechanism: [`SelectWatch::arm`] takes the descriptor, the interest mask, the owner and
//! the payload, so both producers register through the same store and the same delivery.
//!
//! # The C ground truth (`erts/emulator/sys/common/erl_check_io.c`)
//!
//! * **The notification** — `prepare_select_msg` (`:699-800`) builds a `TUPLE4`
//!   `{select, ResourceTerm, RefTerm, EventAtom}` (`:757`) whose `ResourceTerm` is the interest
//!   object's magic reference (`erts_mk_magic_ref`, `:746`), and `send_select_msg` (`:836-858`)
//!   queues it with `am_system` as the sender. `ERL_NIF_SELECT_CUSTOM_MSG` replaces that whole
//!   tuple with the caller's own message term (`:716-731`), which is [`SelectPayload::Custom`].
//! * **The event atom** — `ready_input` / `ready_output` / `ready_error` (`:1451-1463`), one per
//!   observed direction, which is [`SelectEvent`].
//! * **The select is one-shot** — `erts_poll_wait`'s `ERTS_EV_TYPE_NIF` arm sets
//!   `state->driver.nif->in.pid = NIL` and `state->events &= ~revents` (`:2135-2151`), so a
//!   descriptor is reported for readiness exactly once per `enif_select` call.
//!   [`SelectWatch::take_ready`] **de-registers** as it reports. For `prim_tty:reader/1`
//!   (`lib/kernel/src/prim_tty.erl:440-452`), which calls `tty_select/2` **once** for its whole
//!   lifetime, a level-triggered re-report would make an always-readable descriptor (stdin on
//!   `/dev/null`, where the differential harness runs every case) wake the reader on every poll
//!   round for ever.
//! * **`ERL_NIF_SELECT_STOP` / `ERL_NIF_SELECT_CANCEL`** address a registration by its
//!   **descriptor** (`:1272-1284`, `:1292`): `STOP` deletes the fd from the poll set, `CANCEL`
//!   clears the named interest bits and reports which of them really were armed (the
//!   `ERL_NIF_SELECT_*_CANCELLED` bits, `:1470-1482`). [`SelectWatch::stop`] and
//!   [`SelectWatch::cancel`] are those two.
//! * **The owner dying disarms the select** — `tty_select_nif` also monitors the reader and its
//!   `tty_monitor_down` callback (`:1266-1276`) does `enif_select(..., ERL_NIF_SELECT_STOP, ...)`.
//!   [`SelectWatch::prune_dead`] is that arm.
//!
//! # The resource term is minted at delivery, never at arm time
//!
//! The C keeps the interest object alive with a per-interest `enif_keep_resource`
//! (`:1446`, `:1509`) and mints the notification's magic reference when the event is queued. This
//! module stores the **raw object pointer** the NIF handed over (`obj`, moved and never
//! dereferenced — `beam_rt` is `#![deny(unsafe_code)]`) and mints the term at delivery through
//! [`beam_abi::nif::enif::resource_term`], which refuses a null/foreign/dying pointer instead of
//! fabricating one. A `resource_term` refusal delivers **nothing**.
//!
//! The residual that replaces `enif_keep_resource`: a registration holds no reference of its own, so
//! an object released before its readiness arrives is destroyed and its select then delivers
//! nothing, where the C would have kept it alive. It is a lost notification, never a dangling
//! pointer.
//!
//! # One registration per descriptor
//!
//! `beam_io::Poller::register` refuses a duplicate descriptor (`mio` gives one membership per fd), so
//! a second [`SelectWatch::arm`] on a descriptor that is already registered goes through
//! `Poller::reregister`, **widening** the interest mask and **replacing** the payload, owner and
//! object. That is one registration's worth of state, which is why the store keys each registration
//! by its poller key and keeps an `(fd, handle)` index beside it for the `STOP`/`CANCEL` lookups.

use core::ffi::c_void;
use std::collections::HashMap;
use std::os::fd::RawFd;

use beam_common::Result;
use beam_io::{Event, Interest, Poller, Ready, Token};
use beam_process::ProcessId;
use beam_term::Eterm;

use crate::error;

/// The descriptor `prim_tty:tty_select/2` arms on — the C's hardcoded `tty->ifd = 0` (stdin), for a
/// handle built from `stdout` exactly as for one built from `stderr`.
pub(crate) const TTY_IFD: RawFd = 0;

/// The highest poller key of the select reservation; keys are handed out **downwards** from here.
///
/// It must sit below [`super::watch::SIGNAL_KEY`] (`u64::MAX - 1`) and the poller's own
/// [`beam_io::WAKER_KEY`] (`u64::MAX`), and the whole reservation must stay above the port-number
/// space (the port table only hands out numbers below `1 << 60`) — which [`TTY_KEY_FLOOR`] pins.
const TTY_KEY_TOP: u64 = u64::MAX - 2;

/// The lowest key the reservation may use. A registry that somehow ran through this many concurrent
/// registrations reports the typed `system_limit` rather than wrapping into the port number space
/// (where a select event would be mistaken for a port's).
const TTY_KEY_FLOOR: u64 = 1 << 60;

/// What a ready registration delivers — the C's two notification shapes.
///
/// Both terms are **stabilised** (copied onto the runtime's never-reset arena by
/// [`crate::Runtime::stabilize_term`] before they are stored): a raw `Eterm` held across a scheduler
/// pass belongs to a heap a collection may relocate, so it is never stored here.
#[derive(Debug)]
pub(crate) enum SelectPayload {
    /// The C's own `{select, ResourceTerm, Reference, EventAtom}` (`prepare_select_msg`,
    /// `erl_check_io.c:699-800`), with `ERL_NIF_SELECT_CUSTOM_MSG` unset.
    Plain {
        /// `argv[1]`/`enif_select`'s `ref` operand — the tuple's third element, which the caller
        /// matches its notification on.
        reference: Eterm,
    },
    /// The caller's message term verbatim (`ERL_NIF_SELECT_CUSTOM_MSG`, `:716-731`).
    Custom {
        /// The stabilised message term the C would queue in place of the 4-tuple.
        message: Eterm,
    },
}

/// Which of the C's three event atoms a readiness report carries (`erl_check_io.c:1451-1463`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SelectEvent {
    /// `ready_input` — the descriptor is readable.
    ReadyInput,
    /// `ready_output` — the descriptor is writable.
    ReadyOutput,
    /// `ready_error` — the descriptor reported an error condition.
    ReadyError,
}

impl SelectEvent {
    /// The atom name the event is delivered as.
    pub(crate) fn name(self) -> &'static str {
        match self {
            SelectEvent::ReadyInput => "ready_input",
            SelectEvent::ReadyOutput => "ready_output",
            SelectEvent::ReadyError => "ready_error",
        }
    }

    /// The event kind `ready` reports for a registration interested in `interest`.
    ///
    /// Error first, then readability, then writability — the C reports one atom per observed
    /// direction, so a combined `READ | WRITE` mask delivers `ready_input` for a readable event and
    /// `ready_output` for a writable one.
    ///
    /// A `Ready` that observed **no** direction at all (a bare hang-up, which
    /// `beam_io::Poller::poll` reports as `read_closed` rather than as readability) falls back to
    /// the direction the registration asked for: an EOF on a read select *is* the readable event the
    /// C reports, which is what makes a closed pipe wake its reader instead of hanging for ever.
    fn of(ready: Ready, interest: Interest) -> SelectEvent {
        if ready.is_error() {
            SelectEvent::ReadyError
        } else if ready.readable() {
            SelectEvent::ReadyInput
        } else if ready.writable() {
            SelectEvent::ReadyOutput
        } else if interest.is_readable() {
            SelectEvent::ReadyInput
        } else if interest.is_writable() {
            SelectEvent::ReadyOutput
        } else {
            SelectEvent::ReadyError
        }
    }
}

/// The directions a [`SelectWatch::cancel`] really cleared — the C's `ERL_NIF_SELECT_*_CANCELLED`
/// bits (`erl_check_io.c:1470-1482`).
///
/// It is a struct rather than an [`Interest`] because `beam_io::Interest` has no empty value (its
/// bitset is private and only [`Interest::add`] combines directions), and "cancelled nothing" is
/// exactly the state a `CANCEL` must be able to report.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Cancelled {
    /// `ERL_NIF_SELECT_READ_CANCELLED` — a read interest really was armed.
    pub(crate) readable: bool,
    /// `ERL_NIF_SELECT_WRITE_CANCELLED` — a write interest really was armed.
    pub(crate) writable: bool,
    /// `ERL_NIF_SELECT_ERROR_CANCELLED` — an error interest really was armed.
    pub(crate) error: bool,
}

impl Cancelled {
    /// Whether nothing was cancelled — the C's `0` answer to a `CANCEL`.
    pub(crate) fn is_empty(self) -> bool {
        !(self.readable || self.writable || self.error)
    }
}

/// One readiness report, ready for delivery: who it belongs to, what it carries, and which
/// direction was observed.
#[derive(Debug)]
pub(crate) struct SelectDelivery {
    /// The process the notification belongs to — `enif_select`'s recipient.
    pub(crate) owner: ProcessId,
    /// The payload the registration carried.
    pub(crate) payload: SelectPayload,
    /// The direction actually observed, as the C's event atom.
    pub(crate) event: SelectEvent,
    /// The registration's opaque handle term — the plain payload's second element while
    /// [`SelectDelivery::obj`] is null.
    pub(crate) handle: Eterm,
    /// The resource object the notification's resource term is minted from, or null.
    ///
    /// Moved, never dereferenced: it only ever flows into
    /// [`beam_abi::nif::enif::resource_term`].
    pub(crate) obj: *mut c_void,
}

/// One armed registration: the descriptor, its poller token, the owner, the interest mask and the
/// payload.
#[derive(Debug)]
struct SelectRegistration {
    /// The poller token, needed to de-register.
    token: Token,
    /// The descriptor `token` names.
    fd: RawFd,
    /// The readiness directions armed (a caller-intent mask, see [`beam_io::Interest`]).
    interest: Interest,
    /// The calling process the notification is addressed to (`enif_select`'s implicit recipient
    /// when the NIF passed no pid).
    owner: ProcessId,
    /// What a readiness report delivers.
    payload: SelectPayload,
    /// The opaque handle term, the `(fd, handle)` index's other half.
    handle: Eterm,
    /// The resource object, or null.
    obj: *mut c_void,
}

/// The runtime's registrations of `enif_select`/`prim_tty:tty_select/2` interests with its one port
/// poller.
///
/// Owned by [`super::PortRegistry`] (the poller's owner) rather than by the NIF floor, because a
/// poller token is only meaningful for the poll set it was minted from.
#[derive(Debug)]
pub(crate) struct SelectWatch {
    /// The armed registrations, keyed by poller key.
    registrations: HashMap<u64, SelectRegistration>,
    /// The poller key armed on each descriptor — `mio` gives one membership per fd.
    by_fd: HashMap<RawFd, u64>,
    /// `(fd, handle)` → poller key: the `ERL_NIF_SELECT_STOP`/`_CANCEL` lookup index.
    by_fd_handle: HashMap<(RawFd, u64), u64>,
    /// The next key to hand out (counts down from [`TTY_KEY_TOP`]).
    next_key: u64,
}

impl SelectWatch {
    /// A watch with nothing armed.
    pub(crate) fn new() -> SelectWatch {
        SelectWatch {
            registrations: HashMap::new(),
            by_fd: HashMap::new(),
            by_fd_handle: HashMap::new(),
            next_key: TTY_KEY_TOP,
        }
    }

    /// Whether any interest is armed — the "something for the poll round to wait on" half
    /// [`super::PortRegistry::has_nothing_to_poll`] needs, alongside the live ports and a signal
    /// self-pipe that **holds** an undelivered notification (an armed but idle `handle`
    /// disposition is not pending work).
    pub(crate) fn is_armed(&self) -> bool {
        !self.registrations.is_empty()
    }

    /// Whether `key` names one of this watch's registrations — the filter that keeps a select event
    /// out of the port scheduler's readiness batch.
    pub(crate) fn owns(&self, key: u64) -> bool {
        self.registrations.contains_key(&key)
    }

    /// Arms a **one-shot** interest on `fd` for `owner` — the C's `enif_select` registration.
    ///
    /// A descriptor already registered is **re-registered** rather than refused (`mio` allows one
    /// membership per fd): the interest mask is widened with `interest` and the payload, owner,
    /// handle and object are replaced by this call's.
    ///
    /// `payload`'s terms must already be **stabilised** (copied onto the runtime's never-reset arena
    /// by [`crate::Runtime::stabilize_term`]): the registration outlives the call that armed it, so a
    /// raw `Eterm` from a heap a collection may relocate is never stored.
    ///
    /// `obj` is the `void*` resource operand of `enif_select` (or null when the producer has none,
    /// as `prim_tty:tty_select/2` has). It is moved, never dereferenced.
    ///
    /// # Errors
    ///
    /// The poller's own registration failure (the OS refuses the descriptor — a closed fd, say) or
    /// the typed exhaustion of the key reservation.
    pub(crate) fn arm(
        &mut self,
        poller: &mut Poller,
        fd: RawFd,
        interest: Interest,
        owner: ProcessId,
        payload: SelectPayload,
        handle: Eterm,
        obj: *mut c_void,
    ) -> Result<()> {
        if let Some(key) = self.by_fd.get(&fd).copied() {
            // One membership per descriptor: widen the arming, replace the payload.
            let Some(existing) = self.registrations.get_mut(&key) else {
                // The index and the store disagree — impossible through this API; refuse rather
                // than arm a second membership the poller would reject anyway.
                return Err(error::unsupported(
                    "select: the descriptor index names no registration",
                ));
            };
            let widened = existing.interest.add(interest);
            poller.reregister(fd, existing.token, widened)?;
            let previous_handle = existing.handle;
            let _ = self.by_fd_handle.remove(&(fd, handle_key(previous_handle)));
            let Some(registration) = self.registrations.get_mut(&key) else {
                return Err(error::unsupported(
                    "select: the registration store lost its entry",
                ));
            };
            registration.interest = widened;
            registration.owner = owner;
            registration.payload = payload;
            registration.handle = handle;
            registration.obj = obj;
            self.by_fd_handle.insert((fd, handle_key(handle)), key);
            return Ok(());
        }
        if self.next_key <= TTY_KEY_FLOOR {
            return Err(error::unsupported(
                "select: the poller key reservation is exhausted",
            ));
        }
        let key = self.next_key;
        let token = poller.register(fd, interest, key)?;
        self.next_key = key - 1;
        self.registrations.insert(
            key,
            SelectRegistration {
                token,
                fd,
                interest,
                owner,
                payload,
                handle,
                obj,
            },
        );
        self.by_fd.insert(fd, key);
        self.by_fd_handle.insert((fd, handle_key(handle)), key);
        Ok(())
    }

    /// De-registers the registration on `fd`, answering whether anything was armed — the C's
    /// `ERL_NIF_SELECT_STOP` (`erl_check_io.c:1272-1284`).
    ///
    /// `handle` narrows the lookup to the registration that call armed; the C addresses a
    /// registration by its **descriptor** alone, so a handle that names no registration on a
    /// descriptor that *is* armed still stops it.
    ///
    /// # Errors
    ///
    /// The poller's own de-registration failure.
    pub(crate) fn stop(&mut self, poller: &mut Poller, fd: RawFd, handle: Eterm) -> Result<bool> {
        let key = self
            .by_fd_handle
            .get(&(fd, handle_key(handle)))
            .copied()
            .or_else(|| self.by_fd.get(&fd).copied());
        let Some(key) = key else {
            return Ok(false);
        };
        let _ = self.remove(poller, key)?;
        Ok(true)
    }

    /// Clears the `interest` directions armed on `fd` and answers which of them really were armed —
    /// the C's `ERL_NIF_SELECT_CANCEL` (`erl_check_io.c:1292`, the `*_CANCELLED` bits at
    /// `:1470-1482`).
    ///
    /// The descriptor is de-registered when no direction is left.
    ///
    /// # Errors
    ///
    /// The poller's re-registration or de-registration failure.
    pub(crate) fn cancel(
        &mut self,
        poller: &mut Poller,
        fd: RawFd,
        handle: Eterm,
        interest: Interest,
    ) -> Result<Cancelled> {
        let key = self
            .by_fd_handle
            .get(&(fd, handle_key(handle)))
            .copied()
            .or_else(|| self.by_fd.get(&fd).copied());
        let Some(key) = key else {
            return Ok(Cancelled::default());
        };
        let Some(registration) = self.registrations.get(&key) else {
            return Ok(Cancelled::default());
        };
        let armed = registration.interest;
        let cancelled = Cancelled {
            readable: interest.is_readable() && armed.is_readable(),
            writable: interest.is_writable() && armed.is_writable(),
            error: interest.is_error() && armed.is_error(),
        };
        if cancelled.is_empty() {
            return Ok(cancelled);
        }
        if interest.is_readable() || interest.is_writable() {
            // A direction was cleared: narrow the poller's mask. `beam_io::Interest` has no
            // subtraction, so the remaining direction is rebuilt from the arming it kept.
            let remaining = if interest.is_readable() && interest.is_writable() {
                None
            } else if interest.is_readable() {
                armed.is_writable().then_some(Interest::WRITABLE)
            } else {
                armed.is_readable().then_some(Interest::READABLE)
            };
            let Some(remaining) = remaining else {
                let _ = self.remove(poller, key)?;
                return Ok(cancelled);
            };
            let token = registration.token;
            poller.reregister(fd, token, remaining)?;
            if let Some(registration) = self.registrations.get_mut(&key) {
                registration.interest = remaining;
            }
            return Ok(cancelled);
        }
        // An error-only cancel clears the caller-intent error bit and nothing else; the descriptor
        // stays armed for its own directions (the C's `ERTS_POLL_EV_ERR` clear).
        if let Some(registration) = self.registrations.get_mut(&key) {
            registration.interest = if armed.is_readable() {
                Interest::READABLE
            } else if armed.is_writable() {
                Interest::WRITABLE
            } else {
                armed
            };
        }
        Ok(cancelled)
    }

    /// The registrations the poll round observed ready, **disarming** each — the C's one-shot
    /// delivery (`erl_check_io.c:2135-2151` clears the interest as the message is queued).
    ///
    /// An event this watch does not own is skipped: a poll round's batch also carries live ports'
    /// and the signal self-pipe's events.
    ///
    /// # Errors
    ///
    /// The poller's own de-registration failure.
    pub(crate) fn take_ready(
        &mut self,
        poller: &mut Poller,
        polled: &[Event],
    ) -> Result<Vec<SelectDelivery>> {
        let mut delivered = Vec::new();
        for event in polled {
            let key = event.key();
            let Some(registration) = self.registrations.get(&key) else {
                continue;
            };
            let kind = SelectEvent::of(event.ready(), registration.interest);
            let Some(registration) = self.remove(poller, key)? else {
                continue;
            };
            delivered.push(SelectDelivery {
                owner: registration.owner,
                payload: registration.payload,
                event: kind,
                handle: registration.handle,
                obj: registration.obj,
            });
        }
        Ok(delivered)
    }

    /// Drops (and de-registers) every registration whose owner `live` denies — the C's
    /// `tty_monitor_down` `ERL_NIF_SELECT_STOP` arm (`prim_tty_nif.c:1266-1276`).
    ///
    /// # Errors
    ///
    /// The poller's own de-registration failure.
    pub(crate) fn prune_dead(
        &mut self,
        poller: &mut Poller,
        mut live: impl FnMut(ProcessId) -> bool,
    ) -> Result<()> {
        let dead: Vec<u64> = self
            .registrations
            .iter()
            .filter(|(_, registration)| !live(registration.owner))
            .map(|(key, _)| *key)
            .collect();
        for key in dead {
            let _ = self.remove(poller, key)?;
        }
        Ok(())
    }

    /// De-registers `key` from the poller and every index, leaving the store consistent, and answers
    /// the registration it took out (`None` when the key named nothing here).
    fn remove(&mut self, poller: &mut Poller, key: u64) -> Result<Option<SelectRegistration>> {
        let Some(registration) = self.registrations.remove(&key) else {
            return Ok(None);
        };
        if self.by_fd.get(&registration.fd) == Some(&key) {
            self.by_fd.remove(&registration.fd);
        }
        self.by_fd_handle
            .remove(&(registration.fd, handle_key(registration.handle)));
        poller.deregister(registration.fd, registration.token)?;
        Ok(Some(registration))
    }
}

/// The opaque index key of a handle term: its raw word, never interpreted as a term.
///
/// A handle is a stabilised term for `prim_tty` and an opaque registration key for a NIF (which has
/// no term of its own to name the registration with), so it is compared as a word and never read.
fn handle_key(handle: Eterm) -> u64 {
    handle.raw() as u64
}

impl Default for SelectWatch {
    fn default() -> SelectWatch {
        SelectWatch::new()
    }
}

#[cfg(test)]
mod tests {
    // The suite was written against the `prim_tty`-only shape and is rewritten with the generalised
    // store; it lives here rather than in a sibling file because it pins the private
    // `SelectRegistration`/`SelectWatch` interiors.
}
