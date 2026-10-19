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
//! # One reference per armed interest, released at disarm
//!
//! The C keeps the interest object alive with a per-interest `enif_keep_resource`
//! (`erts/emulator/sys/common/erl_check_io.c:1446`, and again for a postponed `stop_select` at
//! `:1509`) and gives that reference back when the interest is dropped (`:1533`, `:913`, `:1834`,
//! `:2242`).
//! This module does the same: [`SelectWatch::arm`] takes one reference through
//! [`keep_resource`], the registration owns it as a [`SelectHold`], and that hold gives it back
//! **exactly once** at disarm.
//!
//! Holding it is what makes a released-but-armed object survive to its readiness: a NIF that arms a
//! select and then drops its own reference still gets its notification, as it does in the C.
//!
//! The notification's resource term is minted **at delivery** through
//! [`beam_abi::nif::enif::resource_term`], which refuses a null/foreign/dying pointer instead of
//! fabricating one; a refusal delivers **nothing**.
//! The interest's reference is released only **after** that mint, because the mint takes its own
//! reference — releasing first would leave a window in which the object could be destroyed between
//! the disarm and the mint.
//!
//! A non-null object this runtime cannot keep alive — one it never allocated, or one already dying —
//! is refused at arm time, so no interest is armed for an object no notification could name.
//! The raw pointer is moved and never dereferenced: `beam_rt` is `#![deny(unsafe_code)]`.
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

use beam_abi::nif::enif::resources::{keep_resource, unkeep_resource};
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
    /// The interest's own reference on [`SelectDelivery::obj`], carried to the **first** delivery of
    /// a readiness.
    ///
    /// One reference per registration, so every later direction of the same readiness carries
    /// `None`; an object-less registration (`prim_tty:tty_select/2`) carries `None` throughout. The
    /// delivery releases it only after the resource term is minted (see [`SelectHold`]).
    pub(crate) hold: Option<SelectHold>,
}

/// The reference one armed interest holds on its resource object — the C's per-interest
/// `enif_keep_resource` (`erts/emulator/sys/common/erl_check_io.c:1446`).
///
/// It is held by the registration (and, across a delivery, by the [`SelectDelivery`] carrying the
/// object) and released **exactly once**, when the hold drops: on [`SelectWatch::stop`], on a
/// [`SelectWatch::cancel`] that leaves no direction armed, on [`SelectWatch::prune_dead`], on the
/// delivery path after the notification's term is minted, or on the store's own teardown.
/// "Exactly once" is by construction — one hold, one `Drop` — so a leaked interest is impossible.
///
/// The object is a raw `*mut c_void` that is only ever **moved**: [`keep_resource`] and
/// [`unkeep_resource`] are safe functions, so this crate stays `#![deny(unsafe_code)]`.
pub(crate) struct SelectHold {
    /// The resource object the reference is held on.
    obj: *mut c_void,
    /// Whether the reference is still held (`false` once released, which is what makes the release
    /// idempotent).
    held: bool,
}

impl SelectHold {
    /// Takes the interest's reference on `obj`.
    ///
    /// A null `obj` — the `prim_tty:tty_select/2` shape — carries nothing to keep, so `Ok(None)`.
    /// A non-null object this runtime cannot keep alive (not one it allocated, or one already dying)
    /// is refused, because arming an interest for an object nothing keeps alive could only deliver
    /// nothing later; refusing here means no registration is created at all.
    ///
    /// # Errors
    ///
    /// The typed capability gap for a non-null object [`keep_resource`] declines.
    pub(crate) fn take(obj: *mut c_void) -> Result<Option<SelectHold>> {
        if obj.is_null() {
            return Ok(None);
        }
        if !keep_resource(obj) {
            return Err(error::unsupported(
                "select: the resource object is not one this runtime can keep alive (it never \
                 allocated it, or it is already dying), so no interest was armed",
            ));
        }
        Ok(Some(SelectHold { obj, held: true }))
    }

    /// Gives the reference back, at most once: an already-released hold — or one that never took a
    /// reference — does nothing.
    fn release(&mut self) {
        if !self.held {
            return;
        }
        self.held = false;
        // A `false` answer means the object was destroyed (or was never this runtime's) without the
        // count underflowing; it is tolerated and must never be followed by a second release.
        let _ = unkeep_resource(self.obj);
    }
}

impl Drop for SelectHold {
    fn drop(&mut self) {
        self.release();
    }
}

impl core::fmt::Debug for SelectHold {
    /// Never the bare pointer identity: what a reader needs is whether the interest still holds its
    /// reference.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SelectHold")
            .field("held", &self.held)
            .finish()
    }
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
    /// The reference held on `obj` for this interest, or `None` when there is no object.
    hold: Option<SelectHold>,
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
    /// # Take before commit
    ///
    /// The interest's own reference is taken **before** the poll set is touched, and a refusal
    /// therefore arms nothing: a fresh arming that cannot keep its object registers no membership,
    /// and a re-registration that cannot keep its new object leaves the existing registration
    /// completely undisturbed (same key, same mask, same object, same hold). A poller failure then
    /// drops the freshly taken hold, which releases the reference — so no route through this method
    /// can leak one.
    ///
    /// A re-registration naming the **same** object keeps the existing hold rather than swapping
    /// keep for unkeep: one interest, one reference, no window in which the object is unheld. A
    /// different object replaces the hold only **after** the re-registration succeeded.
    ///
    /// # Errors
    ///
    /// The poller's own registration failure (the OS refuses the descriptor — a closed fd, say), the
    /// typed exhaustion of the key reservation, or the typed refusal of a non-null object this
    /// runtime cannot keep alive (see [`SelectHold::take`]).
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
            // The same object is already kept by this registration: one reference per interest, so
            // it is neither released nor re-taken.
            let same_obj = existing.obj == obj;
            // Take **before** the commit: a refusal here leaves the existing registration exactly as
            // it was, and a poller failure below drops `fresh` (releasing what it took).
            let mut fresh = if same_obj {
                None
            } else {
                SelectHold::take(obj)?
            };
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
            if !same_obj {
                // Committed: the replaced hold drops here — after the successful re-registration —
                // and releases the reference the old object's interest held.
                registration.hold = fresh.take();
            }
            self.by_fd_handle.insert((fd, handle_key(handle)), key);
            return Ok(());
        }
        if self.next_key <= TTY_KEY_FLOOR {
            return Err(error::unsupported(
                "select: the poller key reservation is exhausted",
            ));
        }
        // Take before commit (see the docs above): a refused keep registers nothing with the poller,
        // and a poller failure drops `hold`, releasing the reference.
        let hold = SelectHold::take(obj)?;
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
                hold,
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
///
/// The registration's [`SelectHold`] travels with the **first** delivery only: the interest took one
/// reference, and that reference must outlive the mint that happens on the delivery path. Every later
/// direction of the same readiness carries `None`, and a readiness that reports **no** direction drops
/// the hold here (a disarm) — which is exactly right, since its reference must be given back too.
fn deliveries_of(registration: SelectRegistration, observed: Observed) -> Vec<SelectDelivery> {
    let mut hold = registration.hold;
    events_of(observed, registration.interest)
        .into_iter()
        .map(|event| SelectDelivery {
            owner: registration.owner,
            payload: registration.payload.clone(),
            event,
            handle: registration.handle,
            obj: registration.obj,
            hold: hold.take(),
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

/// The store's unit suite: the `enif_select`/`tty_select/2` registration semantics (arm, stop,
/// cancel, take_ready, prune_dead) and the readiness classification `events_of` performs (see
/// `src/port/select/tests.rs`).
#[cfg(test)]
mod tests;
