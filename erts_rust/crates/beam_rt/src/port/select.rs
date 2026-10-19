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
//!   observed direction, which is [`SelectEvent`]. The C keeps **one state per direction** and
//!   clears each as it queues that direction's message (`:2126-2151`), then sends them in the
//!   `input` → `output` → `error` order (`:2168-2175`), so one readiness can carry several events:
//!   [`events_of`] is that report.
//! * **The select is one-shot** — `erts_poll_wait`'s `ERTS_EV_TYPE_NIF` arm sets
//!   `state->driver.nif->in.pid = NIL` and `state->events &= ~revents` (`:2135-2151`), so a
//!   descriptor is reported for readiness exactly once per `enif_select` call.
//!   [`SelectWatch::take_ready`] **de-registers** once it has reported every direction of a
//!   readiness. For `prim_tty:reader/1` (`lib/kernel/src/prim_tty.erl:440-452`), which calls
//!   `tty_select/2` **once** for its whole lifetime, a level-triggered re-report would make an
//!   always-readable descriptor (stdin on `/dev/null`, where the differential harness runs every
//!   case) wake the reader on every poll round for ever.
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
///
/// It is `Clone` because one registration can deliver more than one notification (one per observed
/// direction, see [`events_of`]), and every one of them carries the same payload.
#[derive(Clone, Debug)]
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
}

/// The directions one poll round observed — the C's three separate per-direction slots
/// (`state->driver.nif->{in,out,err}`, `erts/emulator/sys/common/erl_check_io.c:1447-1463`).
///
/// It exists because [`beam_io::Ready`] is opaque (getters only, no constructor), while the
/// classification below is exactly the part worth exercising for every combination of observed and
/// requested directions.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Observed {
    /// The round reported readability.
    pub(crate) readable: bool,
    /// The round reported writability.
    pub(crate) writable: bool,
    /// The round reported an error condition.
    pub(crate) error: bool,
}

impl Observed {
    /// The directions `ready` observed.
    fn of(ready: Ready) -> Observed {
        Observed {
            readable: ready.readable(),
            writable: ready.writable(),
            error: ready.is_error(),
        }
    }

    /// Whether the round observed any direction at all.
    fn any(self) -> bool {
        self.readable || self.writable || self.error
    }
}

/// The events one readiness report carries: **one per observed *and requested* direction**, in the
/// C's `input` → `output` → `error` order.
///
/// The C keeps a separate recipient per direction (`state->driver.nif->{in,out,err}`,
/// `erts/emulator/sys/common/erl_check_io.c:1447-1463`), takes each one whose direction the round
/// observed (`:2126-2151`) and queues the messages in this order (`:2168-2175`), so a readiness
/// carrying several directions really does produce several notifications — a reset socket reported
/// as readable *and* errored delivers `ready_input` and then `ready_error`.
///
/// A direction the round observed but the registration did **not** ask for reports nothing, exactly
/// as the C's `is_not_nil(...)` tests leave that direction's slot alone.
///
/// A readiness that observed **no** direction at all — a bare hang-up, which
/// `beam_io::Poller::poll` reports as `read_closed` rather than as readability — falls back to the
/// direction the registration asked for: an EOF on a read select *is* the readable event the C
/// reports, which is what makes a closed pipe wake its reader instead of hanging for ever.
fn events_of(observed: Observed, interest: Interest) -> Vec<SelectEvent> {
    let mut events = Vec::with_capacity(3);
    if observed.readable && interest.is_readable() {
        events.push(SelectEvent::ReadyInput);
    }
    if observed.writable && interest.is_writable() {
        events.push(SelectEvent::ReadyOutput);
    }
    if observed.error && interest.is_error() {
        events.push(SelectEvent::ReadyError);
    }
    if events.is_empty() && !observed.any() {
        // The EOF fallback (see the docs above).
        events.push(if interest.is_readable() {
            SelectEvent::ReadyInput
        } else if interest.is_writable() {
            SelectEvent::ReadyOutput
        } else {
            SelectEvent::ReadyError
        });
    }
    events
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

/// The directions `interest` held, as the [`Cancelled`] bits a stop/cancel reports — one
/// `ERL_NIF_SELECT_*_CANCELLED` bit per direction really armed (`erl_check_io.c:1470-1482`).
fn cancelled_of(interest: Interest) -> Cancelled {
    Cancelled {
        readable: interest.is_readable(),
        writable: interest.is_writable(),
        error: interest.is_error(),
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

    /// De-registers the registration on `fd`, answering the directions it had armed — the C's
    /// `ERL_NIF_SELECT_STOP` (`erl_check_io.c:1272-1284`).
    ///
    /// The C decides a `STOP` before the mask is decoded, rewriting the mode to
    /// `ERL_DRV_READ | ERL_DRV_WRITE | ERL_DRV_USE` (`:1274-1276`) and running its "off" path, which
    /// ORs one `ERL_NIF_SELECT_*_CANCELLED` bit per direction it really cleared (`:1470-1482`), so
    /// the answer is the armed interest of the registration the stop removed. An empty
    /// [`Cancelled`] is a descriptor that was never armed — the C's own fast track
    /// (`:1276-1280`).
    ///
    /// `handle` narrows the lookup to the registration that call armed; the C addresses a
    /// registration by its **descriptor** alone, so a handle that names no registration on a
    /// descriptor that *is* armed still stops it.
    ///
    /// # Errors
    ///
    /// The poller's own de-registration failure.
    pub(crate) fn stop(
        &mut self,
        poller: &mut Poller,
        fd: RawFd,
        handle: Eterm,
    ) -> Result<Cancelled> {
        let key = self
            .by_fd_handle
            .get(&(fd, handle_key(handle)))
            .copied()
            .or_else(|| self.by_fd.get(&fd).copied());
        let Some(key) = key else {
            return Ok(Cancelled::default());
        };
        let Some(registration) = self.remove(poller, key)? else {
            return Ok(Cancelled::default());
        };
        Ok(cancelled_of(registration.interest))
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
    /// One registration contributes **one delivery per observed-and-requested direction**, in the
    /// C's `input` → `output` → `error` order (see [`events_of`]), and is de-registered **once**,
    /// after all of them: an errored descriptor keeps reporting, so leaving the error direction
    /// armed across rounds would spin the poll round for ever.
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
            if !self.registrations.contains_key(&key) {
                continue;
            }
            let observed = Observed::of(event.ready());
            let Some(registration) = self.remove(poller, key)? else {
                continue;
            };
            delivered.extend(deliveries_of(registration, observed));
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

/// The notifications one ready registration produces — **one per event** [`events_of`] reports, all
/// carrying the registration's own owner, payload, handle and object.
fn deliveries_of(registration: SelectRegistration, observed: Observed) -> Vec<SelectDelivery> {
    events_of(observed, registration.interest)
        .into_iter()
        .map(|event| SelectDelivery {
            owner: registration.owner,
            payload: registration.payload.clone(),
            event,
            handle: registration.handle,
            obj: registration.obj,
        })
        .collect()
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
    use super::*;
    use beam_io::Ready;
    use beam_term::make_small;
    use std::collections::HashSet;

    /// A poller and a watch with one `prim_tty:tty_select/2`-shaped registration (a read interest
    /// on [`TTY_IFD`], a plain payload, no resource object) armed, plus the terms it carries.
    fn armed() -> (Poller, SelectWatch, ProcessId, Eterm, Eterm) {
        let mut poller = Poller::new().expect("a poller");
        let mut watch = SelectWatch::new();
        let owner = ProcessId::new(7, 0);
        let handle = make_small(11);
        let reference = make_small(22);
        watch
            .arm(
                &mut poller,
                TTY_IFD,
                Interest::READABLE,
                owner,
                SelectPayload::Plain { reference },
                handle,
                core::ptr::null_mut(),
            )
            .expect("fd 0 is registrable in a test process");
        (poller, watch, owner, handle, reference)
    }

    /// The poll round's event for `key`, reporting **no** observed direction — what an EOF-only
    /// readiness looks like (see [`SelectEvent::of`]'s fallback).
    ///
    /// The token is read out of the watch's own registration: a poller token is opaque, so the
    /// only way a test can name a real one is to take the one the registration was minted with
    /// (the store keys by `key`, so any minted token will do for a foreign key).
    fn polled(watch: &SelectWatch, key: u64) -> Event {
        let token = watch
            .registrations
            .get(&key)
            .or_else(|| watch.registrations.values().next())
            .map(|registration| registration.token)
            .expect("a poll round needs a token, so at least one registration must be armed");
        Event::new(token, key, Ready::default())
    }

    #[test]
    fn a_fresh_watch_is_not_armed_and_owns_nothing() {
        let watch = SelectWatch::new();
        assert!(!watch.is_armed());
        assert!(!watch.owns(0));
        assert!(!watch.owns(TTY_KEY_TOP));
    }

    #[test]
    fn arming_reserves_a_key_and_owns_it() {
        let (_poller, watch, ..) = armed();
        assert!(watch.is_armed());
        assert!(watch.owns(TTY_KEY_TOP), "the first key is the top of the range");
        // The reservation stays outside the port-number space and below the signal sentinel.
        assert!(TTY_KEY_TOP < super::super::watch::SIGNAL_KEY);
        assert!(TTY_KEY_FLOOR > 1 << 59, "keys never wrap into the port-number space");
    }

    /// The full `prim_tty` shape: a read interest on fd 0 delivers one plain `ready_input` report
    /// carrying the handle and the caller's reference, and disarms itself.
    #[test]
    fn a_readiness_report_is_delivered_exactly_once() {
        let (mut poller, mut watch, owner, handle, reference) = armed();
        let events = [polled(&watch, TTY_KEY_TOP)];
        let delivered = watch.take_ready(&mut poller, &events).expect("a delivery");
        assert_eq!(delivered.len(), 1);
        let delivery = &delivered[0];
        assert_eq!(delivery.owner, owner);
        assert_eq!(delivery.handle, handle);
        assert_eq!(delivery.event, SelectEvent::ReadyInput);
        match delivery.payload {
            SelectPayload::Plain { reference: delivered } => assert_eq!(delivered, reference),
            SelectPayload::Custom { .. } => panic!("a plain arming must not deliver a custom message"),
        }
        assert!(delivery.obj.is_null(), "a `tty_select/2` arming carries no resource object");
        // One-shot: the interest is disarmed, so a second round reports nothing.
        assert!(!watch.is_armed());
        assert!(watch
            .take_ready(&mut poller, &events)
            .expect("nothing armed")
            .is_empty());
    }

    #[test]
    fn a_key_the_watch_does_not_own_is_ignored() {
        let (mut poller, mut watch, ..) = armed();
        let foreign = [polled(&watch, 42)];
        assert!(watch
            .take_ready(&mut poller, &foreign)
            .expect("a foreign key is skipped")
            .is_empty());
        assert!(watch.is_armed(), "the armed registration is untouched");
    }

    /// A second arming on one descriptor **widens** the interest mask and replaces the payload,
    /// owner, handle and object — `mio` allows one membership per descriptor, so there is no second
    /// registration to create.
    #[test]
    fn a_second_arming_widens_the_interest_and_replaces_the_payload() {
        let (mut poller, mut watch, owner, _handle, _reference) = armed();
        let handle = make_small(33);
        let message = make_small(44);
        watch
            .arm(
                &mut poller,
                TTY_IFD,
                Interest::WRITABLE,
                owner,
                SelectPayload::Custom { message },
                handle,
                core::ptr::null_mut(),
            )
            .expect("a re-registration");
        assert_eq!(watch.registrations.len(), 1, "still one registration");
        assert_eq!(watch.by_fd.len(), 1, "still one descriptor index entry");
        assert_eq!(watch.by_fd.get(&TTY_IFD).copied(), Some(TTY_KEY_TOP), "the key is kept");
        let registration = &watch.registrations[&TTY_KEY_TOP];
        assert_eq!(registration.interest, Interest::BOTH, "the mask is widened");
        assert_eq!(registration.handle, handle, "the handle is replaced");

        let events = [polled(&watch, TTY_KEY_TOP)];
        let delivered = watch.take_ready(&mut poller, &events).expect("a delivery");
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].handle, handle);
        match delivered[0].payload {
            SelectPayload::Custom { message: delivered } => assert_eq!(delivered, message),
            SelectPayload::Plain { .. } => panic!("the widened arming's payload must replace the old one"),
        }
    }

    /// `ERL_NIF_SELECT_STOP` answers the directions the stopped registration had armed, and
    /// de-registers both the descriptor and the `(fd, handle)` index — a handle that names no
    /// registration on an armed descriptor still stops it (the C addresses a registration by its
    /// descriptor).
    #[test]
    fn stop_de_registers_the_descriptor() {
        let (mut poller, mut watch, _owner, handle, _reference) = armed();
        let stale = make_small(99);
        let stopped = watch.stop(&mut poller, TTY_IFD, handle).expect("a stop");
        assert!(
            stopped.readable && !stopped.writable && !stopped.error,
            "the armed read interest is the one the stop reports: {stopped:?}"
        );
        assert!(!watch.is_armed());
        assert_eq!(watch.by_fd.get(&TTY_IFD), None);
        assert_eq!(watch.by_fd_handle.get(&(TTY_IFD, handle_key(handle))), None);
        assert!(
            watch
                .stop(&mut poller, TTY_IFD, handle)
                .expect("nothing armed")
                .is_empty(),
            "a second STOP reports that there was nothing to stop"
        );

        // A handle the store does not know stops the descriptor all the same.
        let (mut poller, mut watch, ..) = armed();
        assert!(
            !watch
                .stop(&mut poller, TTY_IFD, stale)
                .expect("a stop")
                .is_empty()
        );
        assert!(!watch.is_armed());
    }

    /// `ERL_NIF_SELECT_CANCEL` reports the directions really armed and clears exactly those — the
    /// descriptor is de-registered once no direction is left.
    #[test]
    fn cancel_reports_the_directions_really_armed() {
        let (mut poller, mut watch, owner, handle, reference) = armed();
        // A read interest is all that is armed, so a write cancel clears nothing.
        assert!(watch
            .cancel(&mut poller, TTY_IFD, handle, Interest::WRITABLE)
            .expect("a cancel")
            .is_empty());
        assert!(watch.is_armed(), "nothing was cancelled, so nothing was disarmed");

        // Widen to both directions, then clear the read half: the write half stays armed.
        watch
            .arm(
                &mut poller,
                TTY_IFD,
                Interest::WRITABLE,
                owner,
                SelectPayload::Plain { reference },
                handle,
                core::ptr::null_mut(),
            )
            .expect("a re-registration");
        let cancelled = watch
            .cancel(&mut poller, TTY_IFD, handle, Interest::READABLE)
            .expect("a cancel");
        assert!(cancelled.readable && !cancelled.writable && !cancelled.error);
        assert!(watch.is_armed());
        assert_eq!(watch.registrations[&TTY_KEY_TOP].interest, Interest::WRITABLE);

        // Clearing the remaining direction de-registers the descriptor.
        let cancelled = watch
            .cancel(&mut poller, TTY_IFD, handle, Interest::WRITABLE)
            .expect("a cancel");
        assert!(!cancelled.readable && cancelled.writable && !cancelled.error);
        assert!(!watch.is_armed());
        assert!(
            watch
                .cancel(&mut poller, TTY_IFD, handle, Interest::READABLE)
                .expect("nothing armed")
                .is_empty(),
            "a cancel on an unarmed descriptor cleared nothing"
        );
    }

    #[test]
    fn a_dead_owner_is_pruned_and_a_live_one_is_kept() {
        let (mut poller, mut watch, owner, ..) = armed();
        let live: HashSet<ProcessId> = HashSet::new();
        watch
            .prune_dead(&mut poller, |pid| live.contains(&pid))
            .expect("a prune");
        assert!(!watch.is_armed(), "{owner:?} is gone, so the select is disarmed");
        assert_eq!(watch.by_fd.get(&TTY_IFD), None);

        let (mut poller, mut watch, owner, ..) = armed();
        let mut live = HashSet::new();
        live.insert(owner);
        watch
            .prune_dead(&mut poller, |pid| live.contains(&pid))
            .expect("a prune");
        assert!(watch.is_armed(), "a live owner keeps its registration");
    }

    /// Every observed direction the registration asked for is reported, in the C's `input` →
    /// `output` → `error` order, and a direction it did **not** ask for is not
    /// (`erl_check_io.c:2126-2175`).
    #[test]
    fn each_observed_and_requested_direction_is_reported_in_the_c_order() {
        let readable: Observed = Observed {
            readable: true,
            writable: false,
            error: false,
        };
        let writable = Observed {
            readable: false,
            writable: true,
            error: false,
        };
        let errored = Observed {
            readable: false,
            writable: false,
            error: true,
        };
        let combinations: [(Observed, Vec<SelectEvent>); 7] = [
            (readable, vec![SelectEvent::ReadyInput]),
            (writable, vec![SelectEvent::ReadyOutput]),
            (errored, vec![SelectEvent::ReadyError]),
            (
                Observed {
                    readable: true,
                    writable: true,
                    error: false,
                },
                vec![SelectEvent::ReadyInput, SelectEvent::ReadyOutput],
            ),
            // The measured `READ | ERROR` shape: a reset socket is both readable and errored.
            (
                Observed {
                    readable: true,
                    writable: false,
                    error: true,
                },
                vec![SelectEvent::ReadyInput, SelectEvent::ReadyError],
            ),
            (
                Observed {
                    readable: false,
                    writable: true,
                    error: true,
                },
                vec![SelectEvent::ReadyOutput, SelectEvent::ReadyError],
            ),
            (
                Observed {
                    readable: true,
                    writable: true,
                    error: true,
                },
                vec![
                    SelectEvent::ReadyInput,
                    SelectEvent::ReadyOutput,
                    SelectEvent::ReadyError,
                ],
            ),
        ];
        for (observed, expected) in combinations {
            assert_eq!(
                events_of(observed, Interest::BOTH.add(Interest::ERROR)),
                expected,
                "a registration asking for everything reports every observed direction, in order"
            );
            // Each direction on its own reports only itself.
            assert_eq!(events_of(observed, Interest::READABLE), {
                let mut filtered = expected.clone();
                filtered.retain(|event| *event == SelectEvent::ReadyInput);
                // A readiness that observed *something* but nothing the mask asked for reports
                // nothing at all (`:2144-2151` leaves that direction's slot alone).
                filtered
            });
            assert_eq!(
                events_of(observed, Interest::ERROR),
                expected
                    .iter()
                    .copied()
                    .filter(|event| *event == SelectEvent::ReadyError)
                    .collect::<Vec<_>>()
            );
        }
    }

    /// A readiness that observed no direction at all — a bare hang-up — still reports the direction
    /// the registration asked for, which is what wakes a reader whose peer closed.
    #[test]
    fn a_bare_hang_up_falls_back_to_the_requested_direction() {
        assert_eq!(
            events_of(Observed::default(), Interest::READABLE),
            vec![SelectEvent::ReadyInput]
        );
        assert_eq!(
            events_of(Observed::default(), Interest::WRITABLE),
            vec![SelectEvent::ReadyOutput]
        );
        assert_eq!(
            events_of(Observed::default(), Interest::ERROR),
            vec![SelectEvent::ReadyError]
        );
        assert_eq!(
            events_of(Observed::default(), Interest::BOTH),
            vec![SelectEvent::ReadyInput],
            "a combined mask reports the readable direction first"
        );
    }

    /// One registration that observed two requested directions delivers **two** notifications,
    /// both carrying its own payload — the C's one message per direction.
    #[test]
    fn one_readiness_delivers_one_notification_per_direction() {
        let (_poller, watch, owner, handle, reference) = armed();
        let registration = SelectRegistration {
            token: watch.registrations[&TTY_KEY_TOP].token,
            fd: TTY_IFD,
            interest: Interest::READABLE.add(Interest::ERROR),
            owner,
            payload: SelectPayload::Plain { reference },
            handle,
            obj: core::ptr::null_mut(),
        };
        let delivered = deliveries_of(
            registration,
            Observed {
                readable: true,
                writable: false,
                error: true,
            },
        );
        assert_eq!(delivered.len(), 2, "the readable and the errored direction");
        assert_eq!(delivered[0].event, SelectEvent::ReadyInput);
        assert_eq!(delivered[1].event, SelectEvent::ReadyError);
        for delivery in &delivered {
            assert_eq!(delivery.owner, owner);
            assert_eq!(delivery.handle, handle);
            match delivery.payload {
                SelectPayload::Plain { reference: carried } => assert_eq!(carried, reference),
                SelectPayload::Custom { .. } => panic!("a plain arming must not deliver a custom message"),
            }
        }
    }
}
