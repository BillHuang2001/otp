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

//! The **pending-stop table** — the deferred half of the `enif_select` STOP verdicts.
//!
//! When a host answers `ERL_NIF_SELECT_STOP_SCHEDULED`, the C has decided it is *not* safe to close
//! the handle from the calling thread, so it parks the stop and lets the poller thread run it
//! (`erts/emulator/sys/common/erl_check_io.c:1485-1517` and `:2229-2245`). This module owns the same
//! parking lot for this crate: recording an entry takes one `enif_keep_resource` reference
//! (`:1509`), [`run_pending_stops`] runs each parked callback on the caller's thread and then gives
//! the reference back (`:2241-2242`).
//!
//! # Why the table lives here
//!
//! [`super::select`] owns *argument handling and the verdict dispatch* and must stay the small,
//! readable layer it is; the parking lot, its keep/release accounting and its steal rule are a store,
//! so they live in their own file and are re-exported through [`super::enif`].
//!
//! # What the oracle does (measured)
//!
//! The recorded facts below come from a real C NIF (`beam_bifs`'s `tests/fixtures/a3_resource_stop.c`)
//! run against OTP 29.1.1 / erts-17.1:
//!
//! | shape | oracle |
//! |---|---|
//! | arm READ then STOP, epoll-able fd (a pipe) | `17` = `STOP_CALLED \| READ_CANCELLED`, callback **synchronous**, `is_direct_call = 1` |
//! | arm READ then STOP, an fd the kernel poll refuses (`/dev/null`, a regular file, a directory) | `18` = `STOP_SCHEDULED \| READ_CANCELLED`, callback **later, on the poller thread**, `is_direct_call = 0` |
//! | STOP twice | the callback runs a **second** time (the first stop cleared the fd's event state, so the second takes the unknown-fd fast track) |
//! | a READ on an fd whose stop is still pending | `2` = `STOP_SCHEDULED` and nothing runs yet — `steal_pending_stop_nif` (`:1318-1337`) steals the pending stop and unconditionally reports it |
//!
//! **`STOP_SCHEDULED` is unreachable on Linux/epoll for an epoll-able fd.** `wake_poller`, the only
//! thing that defers a stop, is set by the select/poll **fallback** driver (`erl_poll.c:761-877`), so
//! a pipe or socket STOP is always synchronous there. A host whose select store de-registers
//! immediately (this workspace's runtime does) therefore always answers `STOP_CALLED`, and this table
//! stays empty; the deferred arm exists for a host that really defers, and what is pinned here is the
//! **dispatch**, driven through a host that answers each verdict in turn.
//!
//! # Keying, stealing and the reference count
//!
//! Entries are keyed by `(obj, event)` — the C's event state is keyed by **file descriptor**, one stop
//! per fd — and the table keeps **insertion order**, so a drain is deterministic (a `HashMap` would
//! not be).
//!
//! Exactly **one** keep reference is held per pending entry:
//!
//! * [`record_pending_stop`] takes the new keep **first**, then releases the one a displaced entry
//!   held, so the count never dips through zero even when the entry is stolen by a second STOP for the
//!   same `(obj, event)` (the C's `steal_pending_stop_nif`, `:1334`: the parked stop is taken over, not
//!   armed twice).
//! * [`run_pending_stops`] releases the keep **after** the callback ran, unconditionally — the
//!   reference is this table's, and it goes back whether or not the callback's type still had a
//!   `stop` by then. A destroy in between is impossible while the keep stands, so the release lands.
//!
//! A double-arm can therefore neither leak a reference nor release one twice: the count is one per
//! entry, and the entry is the only holder.
//!
//! # Refuse, never fabricate
//!
//! [`record_pending_stop`] answers `false` — and records nothing, taking no reference — for a null
//! handle, a negative event, an object this crate did not allocate (or one already freed), a type
//! opened without a `stop` callback, and an object that is already dying. The verdict itself is still
//! the host's to answer; this crate simply has no callback to park, and it says so instead of parking
//! an entry that could never run.
//!
//! # Threading
//!
//! The table is process-wide behind a `Mutex` (recovered rather than re-panicked on poisoning, like
//! the resource registry). Offsets are stored as `usize` rather than raw pointers so the table is
//! `Send` without an `unsafe impl`, exactly as [`super::resources`]'s live-object set does.
//!
//! # The deviation, stated once
//!
//! The C runs a parked stop on the **poller thread** (`:2241`). Here [`run_pending_stops`] runs it on
//! whichever thread calls the drain — a host with a real reactor calls it from its own poll loop, which
//! is where the C's poller thread sits, and the callback still receives `is_direct_call = false`. Only
//! the thread's identity can differ, never the operand.

use std::sync::{Mutex, OnceLock};

use libc::c_void;

use crate::nif::types::ErlNifEvent;

use super::resources::{call_resource_stop, keep_resource, resource_type_has_stop, unkeep_resource};

/// One parked stop: the object (as an address, so the table is `Send` without an `unsafe impl`) and
/// the event handle the STOP was issued on.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct PendingStop {
    obj: usize,
    event: ErlNifEvent,
}

/// The parking lot, in insertion order.
#[derive(Default)]
struct PendingStops {
    entries: Vec<PendingStop>,
}

impl PendingStops {
    /// Park `entry`, answering the entry it displaced — the C's "stealing" of an already-pending stop
    /// for the same `(obj, event)` (`erl_check_io.c:1334`). A displaced entry's keep reference must be
    /// released by the caller, **outside** this lock.
    fn park(&mut self, entry: PendingStop) -> Option<PendingStop> {
        match self
            .entries
            .iter_mut()
            .find(|held| held.obj == entry.obj && held.event == entry.event)
        {
            Some(held) => Some(core::mem::replace(held, entry)),
            None => {
                self.entries.push(entry);
                None
            }
        }
    }
}

/// The pending-stop table singleton (initialised on first use).
fn table() -> &'static Mutex<PendingStops> {
    static TABLE: OnceLock<Mutex<PendingStops>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(PendingStops::default()))
}

/// Run `f` against the table under its lock.
///
/// A poisoned lock means an earlier holder panicked. The guard is recovered rather than re-panicked:
/// this store is reached from across the C ABI, where unwinding is not an option.
fn with_table<R>(f: impl FnOnce(&mut PendingStops) -> R) -> R {
    let mut guard = match table().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    f(&mut guard)
}

/// Park a stop for `(obj, event)` — what an `ERL_NIF_SELECT_STOP_SCHEDULED` verdict does
/// (`erl_check_io.c:1509`), so that [`run_pending_stops`] can run the callback later.
///
/// `true` means an entry is parked and **one** reference on `obj` is now held by this table. `false`
/// means nothing was parked and no reference was taken (module docs, "Refuse, never fabricate").
///
/// A second call for the same `(obj, event)` **steals** instead of double-arming: the entry is
/// replaced, the displaced entry's reference is released, and exactly one reference remains held for
/// the one entry. A steal of the entry a live keep stands for can never destroy the object (the new
/// keep is taken before the old one goes).
pub fn record_pending_stop(obj: *mut c_void, event: ErlNifEvent) -> bool {
    if obj.is_null() || event < 0 {
        return false;
    }
    // Only a type with a `stop` callback can be parked: an entry that could never run would be a
    // fabricated success.
    if !resource_type_has_stop(obj) {
        return false;
    }
    // The C's per-pending-stop `enif_keep_resource` (`:1509`). A dying or foreign object answers
    // `false` and is not parked (there would be nothing left to run the callback on).
    if !keep_resource(obj) {
        return false;
    }
    let entry = PendingStop {
        obj: obj as usize,
        event,
    };
    let displaced = with_table(|t| t.park(entry));
    if let Some(previous) = displaced {
        // Outside the lock: the release that reaches zero runs the type's destructor, which may call
        // straight back into this crate.
        unkeep_resource(previous.obj as *mut c_void);
    }
    true
}

/// Drain the parking lot: run every parked stop with the C's poller-thread operand
/// (`call_resource_stop(obj, event, false)`, `erl_check_io.c:2241`) and then release the reference
/// each entry held (`:2242`).
///
/// Answers how many entries were drained (each one ran its callback attempt and gave its reference
/// back), so a caller can tell "nothing was pending" from "N stops ran". The callbacks run **outside**
/// the table lock, in parking order, and may call straight back into this crate — including parking
/// another stop, which the next drain picks up.
pub fn run_pending_stops() -> usize {
    let drained = with_table(|t| core::mem::take(&mut t.entries));
    let mut ran = 0;
    for entry in drained {
        let obj = entry.obj as *mut c_void;
        // The keep this entry holds is what makes the raw handle valid here.
        call_resource_stop(obj, entry.event, false);
        unkeep_resource(obj);
        ran += 1;
    }
    ran
}

/// How many stops are parked right now — the observation a caller (or a test) needs to tell "a stop
/// was parked and has not run yet" from "there is nothing to drain". Process-wide, like the table.
pub fn pending_stop_count() -> usize {
    with_table(|t| t.entries.len())
}

#[cfg(test)]
#[path = "stop_tests.rs"]
mod tests;
