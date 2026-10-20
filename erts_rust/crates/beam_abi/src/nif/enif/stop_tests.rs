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

//! Tests for the **pending-stop table** (`super`) and the `enif_select` STOP-verdict dispatch that
//! feeds it (`super::super::select`) — path-included next to the other resource children.
//!
//! These pin *this crate's* seam, not a C ABI behaviour, so they need no fixture `.so` and no oracle:
//! the verdict values and the reference accounting they assert are the oracle's, measured with a real
//! C NIF (`beam_bifs`'s `tests/fixtures/a3_resource_stop.c`) against OTP 29.1.1 / erts-17.1 and
//! tabulated in `stop.rs`'s and `select.rs`'s module docs.
//!
//! # Test isolation (the table and the registry are process-wide, the tests run in parallel)
//!
//! * [`serial`] holds one lock for the whole module, because every test here parks and drains entries
//!   in the *one* process-wide pending-stop table.
//! * each test opens its own resource **type** under a unique name (types are keyed by name and never
//!   freed), and
//! * each test uses its own event handles, so two tests can never steal each other's parked stop by
//!   the `(obj, event)` key even if the lock were ever dropped.

use super::*;

use std::ffi::CString;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Mutex, MutexGuard};

use libc::{c_int, c_void};

use crate::common::{
    ErlNifSelectFlags, ERL_NIF_SELECT_FAILED, ERL_NIF_SELECT_READ_CANCELLED,
    ERL_NIF_SELECT_STOP_CALLED, ERL_NIF_SELECT_STOP_SCHEDULED,
};
use crate::nif::enif::resources::{
    enif_alloc_resource, enif_open_resource_type_x, enif_release_resource,
};
use crate::nif::enif::select::enif_select;
use crate::nif::host::{NifEnv, NifHost};
use crate::nif::term::nif_from_eterm;
use crate::nif::types::{
    ErlNifEnv, ErlNifEvent, ErlNifResourceDtor, ErlNifResourceFlags, ErlNifResourceStop,
    ErlNifResourceType, ErlNifResourceTypeInit,
};
use beam_term::prelude::{make_atom, Eterm, NIL};

// ---------------------------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------------------------

/// Serialises this module's tests over the one process-wide pending-stop table.
static TABLE_LOCK: Mutex<()> = Mutex::new(());

/// Take the module lock, recovering a poisoned one (a test that panicked with a parked entry must not
/// take the rest of the module down).
fn serial() -> MutexGuard<'static, ()> {
    TABLE_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Drain anything a previously failed test left parked, so a count assertion starts from a known
/// empty table. The leftover callbacks are this module's own (they only bump statics), so running
/// them here is harmless.
fn flush_leftovers() {
    let _ = run_pending_stops();
    assert_eq!(
        pending_stop_count(),
        0,
        "the pending-stop table starts empty for every test"
    );
}

/// A host that answers one `enif_select` verdict, set per call.
struct VerdictHost {
    heap: beam_alloc::Heap,
    atoms: beam_term::atom::AtomTable,
    /// What the `select` hook answers (`None` = unsupported).
    code: Option<c_int>,
}

impl VerdictHost {
    fn new() -> VerdictHost {
        VerdictHost {
            heap: beam_alloc::Heap::new(4096).expect("a small test heap is allocatable"),
            atoms: beam_term::atom::AtomTable::new(),
            code: None,
        }
    }
}

impl NifHost for VerdictHost {
    fn heap(&mut self) -> &mut beam_alloc::Heap {
        &mut self.heap
    }

    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        Some(make_atom(self.atoms.intern(name)))
    }

    fn atom_name(&mut self, atom: Eterm) -> Option<String> {
        if !beam_term::prelude::is_atom(atom) {
            return None;
        }
        self.atoms
            .name(beam_term::prelude::atom_val(atom))
            .map(str::to_string)
    }

    fn select(
        &mut self,
        _event: ErlNifEvent,
        _mode: ErlNifSelectFlags,
        _obj: *mut c_void,
        _pid: Option<Eterm>,
        _reference: Eterm,
    ) -> Option<c_int> {
        self.code
    }
}

/// Open a resource type carrying `dtor` **and** `stop` (`members = 3`: `dtor`, `stop`, `down`, the
/// three `enif_open_resource_type_x` reads). The plain `enif_open_resource_type` takes only a `dtor`,
/// so the `_x` form is the one that can install a `stop` callback.
fn open_stop_type(
    host: &mut VerdictHost,
    name: &str,
    dtor: ErlNifResourceDtor,
    stop: ErlNifResourceStop,
) -> *mut ErlNifResourceType {
    let c_name = CString::new(name).expect("no interior NUL");
    let init = ErlNifResourceTypeInit {
        dtor: Some(dtor),
        stop: Some(stop),
        down: None,
        members: 3,
        dyncall: None,
    };
    let mut tried = ErlNifResourceFlags::from_bits(0);
    let mut env = NifEnv::new(host);
    // SAFETY: `env` is live; `c_name` is NUL-terminated; `init` is a live local of the frozen shape;
    // `tried` is a live local. The type's environment source is this host, which outlives the test.
    let ty = unsafe {
        enif_open_resource_type_x(
            env.as_ptr(),
            c_name.as_ptr(),
            &init,
            ErlNifResourceFlags::CREATE,
            &mut tried,
        )
    };
    assert!(!ty.is_null(), "the type must open");
    ty
}

/// `enif_select(env, event, flags, obj, NULL, nil)` with the host answering `code` — the C's "the
/// calling process is the recipient" shape.
fn select_call(
    host: &mut VerdictHost,
    code: c_int,
    event: ErlNifEvent,
    flags: ErlNifSelectFlags,
    obj: *mut c_void,
) -> c_int {
    host.code = Some(code);
    let mut env = NifEnv::new(host);
    // SAFETY: `env` is live, `obj` is a live payload pointer (or a deliberately foreign handle in the
    // refusal tests), and `pid` is null (the calling process).
    unsafe {
        enif_select(
            env.as_ptr(),
            event,
            flags,
            obj,
            core::ptr::null(),
            nif_from_eterm(NIL),
        )
    }
}

/// `enif_alloc_resource(ty, size)` with one reference (the allocator's), asserting success.
///
/// # Safety
///
/// `ty` must be a live type; the caller must account for the returned pointer's one reference.
unsafe fn alloc_object(ty: *mut ErlNifResourceType, size: usize) -> *mut c_void {
    // SAFETY: forwarded from the caller's contract.
    let obj = unsafe { enif_alloc_resource(ty, size) };
    assert!(!obj.is_null(), "the allocation must succeed");
    obj
}

// ---------------------------------------------------------------------------------------------
// The verdict dispatch
// ---------------------------------------------------------------------------------------------

/// `STOP_CALLED` alone (the C's fast track for an fd with no event state, `erl_check_io.c:1278-1285`):
/// the callback runs **inside** `enif_select`, with `is_direct_call = true` and the request's own event
/// handle, and the host's code comes back unchanged. Nothing is parked, and the reference the caller
/// passed in is still the caller's (`CALL_STOP`) — one release here destroys the object.
#[test]
fn a_plain_stop_called_runs_the_callback_synchronously() {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    static LAST_DIRECT: AtomicIsize = AtomicIsize::new(-1);
    static LAST_EVENT: AtomicIsize = AtomicIsize::new(-1);
    static DTORS: AtomicUsize = AtomicUsize::new(0);

    /// The type's `stop`: records the two operands that tell a synchronous call from a deferred one.
    unsafe extern "C" fn stop_cb(
        _env: *mut ErlNifEnv,
        _obj: *mut c_void,
        event: ErlNifEvent,
        is_direct_call: c_int,
    ) {
        CALLS.fetch_add(1, AtomicOrdering::SeqCst);
        LAST_DIRECT.store(is_direct_call as isize, AtomicOrdering::SeqCst);
        LAST_EVENT.store(event as isize, AtomicOrdering::SeqCst);
    }
    /// The type's destructor: only the run count matters here.
    unsafe extern "C" fn dtor_cb(_env: *mut ErlNifEnv, _obj: *mut c_void) {
        DTORS.fetch_add(1, AtomicOrdering::SeqCst);
    }

    let _serial = serial();
    flush_leftovers();
    let mut host = VerdictHost::new();
    let ty = open_stop_type(&mut host, "stop_plain_called", dtor_cb, stop_cb);
    // SAFETY: `ty` was just opened; the payload is one word (this test's own size class).
    let obj = unsafe { alloc_object(ty, 8) };

    let code = select_call(
        &mut host,
        ERL_NIF_SELECT_STOP_CALLED,
        21,
        ErlNifSelectFlags::STOP,
        obj,
    );
    assert_eq!(
        code, ERL_NIF_SELECT_STOP_CALLED,
        "the host's STOP code is answered unchanged"
    );
    assert_eq!(CALLS.load(AtomicOrdering::SeqCst), 1, "the callback ran");
    assert_eq!(
        LAST_DIRECT.load(AtomicOrdering::SeqCst),
        1,
        "is_direct_call = 1: the scheduler-side call"
    );
    assert_eq!(
        LAST_EVENT.load(AtomicOrdering::SeqCst),
        21,
        "the callback is handed the event handle the STOP was issued on"
    );
    assert_eq!(
        pending_stop_count(),
        0,
        "a synchronous verdict parks nothing"
    );
    assert_eq!(DTORS.load(AtomicOrdering::SeqCst), 0, "the object is alive");

    // The fast track is the C's `CALL_STOP`: the caller still owns its reference, so this one release
    // is the last one.
    // SAFETY: `obj` is a live payload with an outstanding reference (the allocator's).
    unsafe { enif_release_resource(obj) };
    assert_eq!(
        DTORS.load(AtomicOrdering::SeqCst),
        1,
        "the caller's single release destroyed the object"
    );
}

/// `STOP_CALLED | READ_CANCELLED` (17 — arming READ then STOP on an epoll-able fd, the C's
/// `CALL_STOP_AND_RELEASE` arm): still synchronous, `is_direct_call = true`, code unchanged. The
/// reference the runtime released there is the **host's** armed-interest reference, not this crate's,
/// so one release of the allocator's reference still destroys the object.
#[test]
fn a_stop_called_with_a_cancelled_read_is_still_synchronous() {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    static LAST_DIRECT: AtomicIsize = AtomicIsize::new(-1);
    static LAST_EVENT: AtomicIsize = AtomicIsize::new(-1);
    static DTORS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn stop_cb(
        _env: *mut ErlNifEnv,
        _obj: *mut c_void,
        event: ErlNifEvent,
        is_direct_call: c_int,
    ) {
        CALLS.fetch_add(1, AtomicOrdering::SeqCst);
        LAST_DIRECT.store(is_direct_call as isize, AtomicOrdering::SeqCst);
        LAST_EVENT.store(event as isize, AtomicOrdering::SeqCst);
    }
    unsafe extern "C" fn dtor_cb(_env: *mut ErlNifEnv, _obj: *mut c_void) {
        DTORS.fetch_add(1, AtomicOrdering::SeqCst);
    }

    let _serial = serial();
    flush_leftovers();
    let mut host = VerdictHost::new();
    let ty = open_stop_type(&mut host, "stop_called_cancelled_read", dtor_cb, stop_cb);
    // SAFETY: `ty` was just opened; 24 bytes is this test's own size class.
    let obj = unsafe { alloc_object(ty, 24) };

    let verdict = ERL_NIF_SELECT_STOP_CALLED | ERL_NIF_SELECT_READ_CANCELLED;
    let code = select_call(&mut host, verdict, 22, ErlNifSelectFlags::STOP, obj);
    assert_eq!(code, verdict, "the composed code is answered unchanged");
    assert_eq!(CALLS.load(AtomicOrdering::SeqCst), 1, "the callback ran");
    assert_eq!(
        LAST_DIRECT.load(AtomicOrdering::SeqCst),
        1,
        "the CANCELLED bit does not make the call deferred"
    );
    assert_eq!(LAST_EVENT.load(AtomicOrdering::SeqCst), 22);
    assert_eq!(pending_stop_count(), 0, "nothing was parked");

    // SAFETY: `obj` is a live payload with an outstanding reference (the allocator's).
    unsafe { enif_release_resource(obj) };
    assert_eq!(DTORS.load(AtomicOrdering::SeqCst), 1);
}

/// `STOP_SCHEDULED | READ_CANCELLED` (18 — the C's `wake_poller` arm): nothing runs at `enif_select`
/// time, exactly one entry is parked, and the drain runs the callback with `is_direct_call = false` and
/// the parked event handle. The drain then gives its keep back, so the allocator's reference is still
/// the object's only one — one release destroys it — and a second drain finds an empty lot.
#[test]
fn a_scheduled_stop_runs_from_the_drain_with_no_direct_call() {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    static LAST_DIRECT: AtomicIsize = AtomicIsize::new(-1);
    static LAST_EVENT: AtomicIsize = AtomicIsize::new(-1);
    static DTORS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn stop_cb(
        _env: *mut ErlNifEnv,
        _obj: *mut c_void,
        event: ErlNifEvent,
        is_direct_call: c_int,
    ) {
        CALLS.fetch_add(1, AtomicOrdering::SeqCst);
        LAST_DIRECT.store(is_direct_call as isize, AtomicOrdering::SeqCst);
        LAST_EVENT.store(event as isize, AtomicOrdering::SeqCst);
    }
    unsafe extern "C" fn dtor_cb(_env: *mut ErlNifEnv, _obj: *mut c_void) {
        DTORS.fetch_add(1, AtomicOrdering::SeqCst);
    }

    let _serial = serial();
    flush_leftovers();
    let mut host = VerdictHost::new();
    let ty = open_stop_type(&mut host, "stop_scheduled", dtor_cb, stop_cb);
    // SAFETY: `ty` was just opened; 32 bytes is this test's own size class.
    let obj = unsafe { alloc_object(ty, 32) };

    let verdict = ERL_NIF_SELECT_STOP_SCHEDULED | ERL_NIF_SELECT_READ_CANCELLED;
    let code = select_call(&mut host, verdict, 23, ErlNifSelectFlags::STOP, obj);
    assert_eq!(code, verdict, "the host's code is answered unchanged");
    assert_eq!(
        CALLS.load(AtomicOrdering::SeqCst),
        0,
        "a deferred verdict does not call the callback from enif_select"
    );
    assert_eq!(pending_stop_count(), 1, "exactly one stop is parked");
    assert_eq!(
        DTORS.load(AtomicOrdering::SeqCst),
        0,
        "the parked keep holds the object alive"
    );

    assert_eq!(run_pending_stops(), 1, "one drain ran one callback");
    assert_eq!(CALLS.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(
        LAST_DIRECT.load(AtomicOrdering::SeqCst),
        0,
        "is_direct_call = 0: the poller-side call (erl_check_io.c:2241)"
    );
    assert_eq!(
        LAST_EVENT.load(AtomicOrdering::SeqCst),
        23,
        "the parked event handle is the one the STOP was issued on"
    );
    assert_eq!(pending_stop_count(), 0, "the lot is empty");
    assert_eq!(run_pending_stops(), 0, "a second drain has nothing to run");
    assert_eq!(
        CALLS.load(AtomicOrdering::SeqCst),
        1,
        "and no second callback fired"
    );
    assert_eq!(
        DTORS.load(AtomicOrdering::SeqCst),
        0,
        "the drain released only its own keep; the allocator's reference stands"
    );
    assert!(
        resource_type_has_stop(obj),
        "the object is still a live registered resource"
    );

    // SAFETY: `obj` is a live payload whose remaining reference is the allocator's.
    unsafe { enif_release_resource(obj) };
    assert_eq!(
        DTORS.load(AtomicOrdering::SeqCst),
        1,
        "the object is destroyed exactly once"
    );
    assert!(
        !resource_type_has_stop(obj),
        "a destroyed object is no longer registered"
    );
}

/// Every non-STOP verdict — a plain `0`, the `*_CANCELLED` bits, a negative refusal — calls no
/// callback, parks nothing, and is answered unchanged.
#[test]
fn no_other_verdict_calls_the_callback_or_parks_a_stop() {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    static DTORS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn stop_cb(
        _env: *mut ErlNifEnv,
        _obj: *mut c_void,
        _event: ErlNifEvent,
        _is_direct_call: c_int,
    ) {
        CALLS.fetch_add(1, AtomicOrdering::SeqCst);
    }
    unsafe extern "C" fn dtor_cb(_env: *mut ErlNifEnv, _obj: *mut c_void) {
        DTORS.fetch_add(1, AtomicOrdering::SeqCst);
    }

    let _serial = serial();
    flush_leftovers();
    let mut host = VerdictHost::new();
    let ty = open_stop_type(&mut host, "stop_other_verdicts", dtor_cb, stop_cb);
    // SAFETY: `ty` was just opened; 40 bytes is this test's own size class.
    let obj = unsafe { alloc_object(ty, 40) };

    // (answer, the request that produced it). A negative answer is one of the C's `INT_MIN | bit`
    // refusals — never a STOP verdict, whatever its low bits happen to be.
    let cases: [(c_int, ErlNifSelectFlags); 4] = [
        (0, ErlNifSelectFlags::READ),
        (ERL_NIF_SELECT_READ_CANCELLED, ErlNifSelectFlags::CANCEL),
        (c_int::MIN | ERL_NIF_SELECT_FAILED, ErlNifSelectFlags::READ),
        (-1, ErlNifSelectFlags::STOP),
    ];
    for (index, (answer, flags)) in cases.iter().enumerate() {
        let event = 30 + index as ErlNifEvent;
        let code = select_call(&mut host, *answer, event, *flags, obj);
        assert_eq!(code, *answer, "the host's answer is passed through unchanged");
        assert_eq!(
            CALLS.load(AtomicOrdering::SeqCst),
            0,
            "no callback for answer {answer}"
        );
        assert_eq!(pending_stop_count(), 0, "and nothing parked for {answer}");
    }
    assert_eq!(
        DTORS.load(AtomicOrdering::SeqCst),
        0,
        "the object outlived every verdict"
    );

    // SAFETY: `obj` is a live payload with an outstanding reference (the allocator's).
    unsafe { enif_release_resource(obj) };
    assert_eq!(DTORS.load(AtomicOrdering::SeqCst), 1);
}

/// A second STOP for the same `(obj, event)` **steals** the parked entry instead of double-arming it
/// (the C's `steal_pending_stop_nif`, `erl_check_io.c:1334`): still one entry, still exactly one keep
/// reference, and the callback runs exactly once when the lot is drained. A leak would keep the object
/// alive past the final release; a double release would destroy it early — both are asserted against.
#[test]
fn a_second_stop_for_the_same_pair_steals_the_parked_entry() {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    static LAST_DIRECT: AtomicIsize = AtomicIsize::new(-1);
    static DTORS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn stop_cb(
        _env: *mut ErlNifEnv,
        _obj: *mut c_void,
        _event: ErlNifEvent,
        is_direct_call: c_int,
    ) {
        CALLS.fetch_add(1, AtomicOrdering::SeqCst);
        LAST_DIRECT.store(is_direct_call as isize, AtomicOrdering::SeqCst);
    }
    unsafe extern "C" fn dtor_cb(_env: *mut ErlNifEnv, _obj: *mut c_void) {
        DTORS.fetch_add(1, AtomicOrdering::SeqCst);
    }

    let _serial = serial();
    flush_leftovers();
    let mut host = VerdictHost::new();
    let ty = open_stop_type(&mut host, "stop_steal", dtor_cb, stop_cb);
    // SAFETY: `ty` was just opened; 48 bytes is this test's own size class.
    let obj = unsafe { alloc_object(ty, 48) };

    let verdict = ERL_NIF_SELECT_STOP_SCHEDULED;
    let event = 41;
    let first = select_call(&mut host, verdict, event, ErlNifSelectFlags::STOP, obj);
    assert_eq!(first, verdict);
    assert_eq!(pending_stop_count(), 1, "the first STOP parked one entry");

    let second = select_call(&mut host, verdict, event, ErlNifSelectFlags::STOP, obj);
    assert_eq!(second, verdict);
    assert_eq!(
        pending_stop_count(),
        1,
        "a second STOP for the same (obj, event) steals, it does not double-arm"
    );
    assert_eq!(
        DTORS.load(AtomicOrdering::SeqCst),
        0,
        "the single held keep is still a reference: the steal neither over-released nor destroyed"
    );

    assert_eq!(run_pending_stops(), 1, "one entry, one drain");
    assert_eq!(
        CALLS.load(AtomicOrdering::SeqCst),
        1,
        "the callback ran exactly once, not twice"
    );
    assert_eq!(LAST_DIRECT.load(AtomicOrdering::SeqCst), 0);
    assert_eq!(pending_stop_count(), 0);
    assert_eq!(
        DTORS.load(AtomicOrdering::SeqCst),
        0,
        "the drain balanced its keep, so only the allocator's reference remains"
    );

    // SAFETY: `obj` is a live payload whose remaining reference is the allocator's — a leaked keep
    // would leave the object alive here, and the assertion below would fail.
    unsafe { enif_release_resource(obj) };
    assert_eq!(
        DTORS.load(AtomicOrdering::SeqCst),
        1,
        "no leaked reference keeps the object alive"
    );
    assert!(
        !resource_type_has_stop(obj),
        "the object is destroyed, so it is no longer registered"
    );
}

/// The balanced pin the oracle measured: **8 objects → 8 destructors** across all four verdicts. Each
/// object is driven through one verdict in turn, with its own payload tag; after the drain and one
/// release of each allocator reference, every object's destructor has run **exactly once** (its own
/// tag), and no object is left registered — no leak and no double destroy anywhere.
#[test]
fn eight_objects_across_every_verdict_are_each_destroyed_exactly_once() {
    static STOPS: AtomicUsize = AtomicUsize::new(0);
    static DTORS: AtomicUsize = AtomicUsize::new(0);
    static SEEN: Mutex<Vec<usize>> = Mutex::new(Vec::new());

    unsafe extern "C" fn stop_cb(
        _env: *mut ErlNifEnv,
        _obj: *mut c_void,
        _event: ErlNifEvent,
        _is_direct_call: c_int,
    ) {
        STOPS.fetch_add(1, AtomicOrdering::SeqCst);
    }
    /// The type's destructor: records the payload tag of the object it destroys, so "each object
    /// exactly once" is pinned per object and not only in aggregate.
    unsafe extern "C" fn dtor_cb(_env: *mut ErlNifEnv, obj: *mut c_void) {
        DTORS.fetch_add(1, AtomicOrdering::SeqCst);
        // SAFETY: `obj` is the live payload of the object whose count just reached zero, and the test
        // wrote one word into every payload before releasing it.
        let tag = unsafe { core::ptr::read(obj as *const usize) };
        SEEN.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(tag);
    }

    let _serial = serial();
    flush_leftovers();
    let mut host = VerdictHost::new();
    let ty = open_stop_type(&mut host, "stop_balanced", dtor_cb, stop_cb);

    // The four verdict families the oracle measured, spread over eight objects.
    let verdicts: [(c_int, ErlNifSelectFlags); 8] = [
        (ERL_NIF_SELECT_STOP_CALLED, ErlNifSelectFlags::STOP),
        (
            ERL_NIF_SELECT_STOP_CALLED | ERL_NIF_SELECT_READ_CANCELLED,
            ErlNifSelectFlags::STOP,
        ),
        (
            ERL_NIF_SELECT_STOP_SCHEDULED | ERL_NIF_SELECT_READ_CANCELLED,
            ErlNifSelectFlags::STOP,
        ),
        (ERL_NIF_SELECT_STOP_CALLED, ErlNifSelectFlags::STOP),
        (ERL_NIF_SELECT_STOP_SCHEDULED, ErlNifSelectFlags::STOP),
        (0, ErlNifSelectFlags::READ),
        (ERL_NIF_SELECT_READ_CANCELLED, ErlNifSelectFlags::CANCEL),
        (-1, ErlNifSelectFlags::STOP),
    ];
    // Derived from the table, so the expectations can never disagree with what is driven.
    let synchronous = verdicts
        .iter()
        .filter(|(code, _)| *code >= 0 && code & ERL_NIF_SELECT_STOP_CALLED != 0)
        .count();
    let deferred = verdicts
        .iter()
        .filter(|(code, _)| *code >= 0 && code & ERL_NIF_SELECT_STOP_SCHEDULED != 0)
        .count();
    assert_eq!(synchronous + deferred, 5, "the four families, as tabulated");

    let mut objs: Vec<*mut c_void> = Vec::new();
    for (tag, (code, flags)) in verdicts.iter().enumerate() {
        // SAFETY: `ty` is live; 64 bytes is this test's own size class.
        let obj = unsafe { alloc_object(ty, 64) };
        // SAFETY: the payload is 64 bytes, so its first word is writable and the destructor reads it.
        unsafe { core::ptr::write(obj as *mut usize, tag) };
        objs.push(obj);
        let event = 60 + tag as ErlNifEvent;
        let answered = select_call(&mut host, *code, event, *flags, obj);
        assert_eq!(answered, *code, "the host's answer is unchanged for tag {tag}");
    }

    assert_eq!(
        STOPS.load(AtomicOrdering::SeqCst),
        synchronous,
        "the synchronous verdicts ran their callbacks inside enif_select"
    );
    assert_eq!(
        pending_stop_count(),
        deferred,
        "the deferred verdicts are parked"
    );
    assert_eq!(
        DTORS.load(AtomicOrdering::SeqCst),
        0,
        "every object still holds its allocator reference"
    );

    assert_eq!(
        run_pending_stops(),
        deferred,
        "the drain ran every parked stop"
    );
    assert_eq!(
        STOPS.load(AtomicOrdering::SeqCst),
        synchronous + deferred,
        "one callback per object, no more"
    );
    assert_eq!(pending_stop_count(), 0);

    for obj in &objs {
        // SAFETY: each object holds exactly its allocator's reference at this point.
        unsafe { enif_release_resource(*obj) };
    }
    assert_eq!(
        DTORS.load(AtomicOrdering::SeqCst),
        8,
        "8 objects, 8 destructors — the oracle's balanced transcript"
    );
    let mut seen = SEEN
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    seen.sort_unstable();
    assert_eq!(
        seen,
        (0..8).collect::<Vec<usize>>(),
        "each object's own destructor ran, exactly once"
    );
    for obj in &objs {
        assert!(
            !resource_type_has_stop(*obj),
            "a destroyed object is no longer registered, so no reference leaked"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The seam's own refusals
// ---------------------------------------------------------------------------------------------

/// The helpers refuse a null handle, a pointer this crate never allocated (without dereferencing it),
/// and a negative event — answering `false`, taking no reference and parking nothing.
#[test]
fn the_helpers_refuse_a_null_or_foreign_handle_and_a_negative_event() {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    static DTORS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn stop_cb(
        _env: *mut ErlNifEnv,
        _obj: *mut c_void,
        _event: ErlNifEvent,
        _is_direct_call: c_int,
    ) {
        CALLS.fetch_add(1, AtomicOrdering::SeqCst);
    }
    unsafe extern "C" fn dtor_cb(_env: *mut ErlNifEnv, _obj: *mut c_void) {
        DTORS.fetch_add(1, AtomicOrdering::SeqCst);
    }

    let _serial = serial();
    flush_leftovers();
    let mut host = VerdictHost::new();
    let ty = open_stop_type(&mut host, "stop_refusals", dtor_cb, stop_cb);
    // SAFETY: `ty` was just opened; 72 bytes is this test's own size class.
    let obj = unsafe { alloc_object(ty, 72) };

    // A handle this crate never allocated — the `0x1000` stand-in the select tests use. Neither helper
    // may dereference it.
    let foreign = 0x1000 as *mut c_void;
    assert!(!resource_type_has_stop(core::ptr::null_mut()));
    assert!(!resource_type_has_stop(foreign));
    assert!(!call_resource_stop(core::ptr::null_mut(), 1, true));
    assert!(!call_resource_stop(foreign, 1, true));
    assert!(!record_pending_stop(core::ptr::null_mut(), 1));
    assert!(!record_pending_stop(foreign, 1));
    assert_eq!(pending_stop_count(), 0, "nothing was parked");
    assert_eq!(CALLS.load(AtomicOrdering::SeqCst), 0, "no callback ran");

    // A negative event is not a selectable handle: the STOP it names cannot be serviced.
    assert!(resource_type_has_stop(obj), "the object itself is sound");
    assert!(
        !record_pending_stop(obj, -1),
        "a negative event is refused, not parked"
    );
    assert_eq!(pending_stop_count(), 0, "and nothing was parked for it");

    // The drain of an empty lot runs nothing, so the object is untouched and still alive.
    assert_eq!(run_pending_stops(), 0);
    assert_eq!(DTORS.load(AtomicOrdering::SeqCst), 0);

    // SAFETY: `obj` is a live payload with an outstanding reference (the allocator's).
    unsafe { enif_release_resource(obj) };
    assert_eq!(DTORS.load(AtomicOrdering::SeqCst), 1);

    // A destroyed object is refused by both helpers: no second callback, no resurrected entry.
    assert!(!resource_type_has_stop(obj));
    assert!(!call_resource_stop(obj, 1, true));
    assert!(!record_pending_stop(obj, 1));
    assert_eq!(CALLS.load(AtomicOrdering::SeqCst), 0);
    assert_eq!(pending_stop_count(), 0);
    assert_eq!(
        DTORS.load(AtomicOrdering::SeqCst),
        1,
        "no second destructor ran"
    );
}

/// `call_resource_stop` runs on a **real** object outside the select path too, with the operand it is
/// given — the helper's own contract, pinned without a host verdict in between.
#[test]
fn the_helper_runs_a_real_stops_callback_with_the_given_operands() {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    static LAST_DIRECT: AtomicIsize = AtomicIsize::new(-1);
    static LAST_EVENT: AtomicIsize = AtomicIsize::new(-1);
    static LAST_TAG: AtomicIsize = AtomicIsize::new(-1);

    unsafe extern "C" fn stop_cb(
        _env: *mut ErlNifEnv,
        obj: *mut c_void,
        event: ErlNifEvent,
        is_direct_call: c_int,
    ) {
        CALLS.fetch_add(1, AtomicOrdering::SeqCst);
        LAST_DIRECT.store(is_direct_call as isize, AtomicOrdering::SeqCst);
        LAST_EVENT.store(event as isize, AtomicOrdering::SeqCst);
        // SAFETY: the payload's first word was written by the test.
        let tag = unsafe { core::ptr::read(obj as *const usize) };
        LAST_TAG.store(tag as isize, AtomicOrdering::SeqCst);
    }
    unsafe extern "C" fn dtor_cb(_env: *mut ErlNifEnv, _obj: *mut c_void) {}

    let _serial = serial();
    flush_leftovers();
    let mut host = VerdictHost::new();
    let ty = open_stop_type(&mut host, "stop_helper_direct", dtor_cb, stop_cb);
    // SAFETY: `ty` was just opened; 80 bytes is this test's own size class.
    let obj = unsafe { alloc_object(ty, 80) };
    // SAFETY: the payload is 80 bytes, so its first word is writable.
    unsafe { core::ptr::write(obj as *mut usize, 7) };

    assert!(
        call_resource_stop(obj, 77, false),
        "a live object of a type with a stop runs its callback"
    );
    assert_eq!(CALLS.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(LAST_DIRECT.load(AtomicOrdering::SeqCst), 0);
    assert_eq!(LAST_EVENT.load(AtomicOrdering::SeqCst), 77);
    assert_eq!(
        LAST_TAG.load(AtomicOrdering::SeqCst),
        7,
        "the callback is handed the object's own payload"
    );

    assert!(call_resource_stop(obj, 78, true));
    assert_eq!(CALLS.load(AtomicOrdering::SeqCst), 2);
    assert_eq!(LAST_DIRECT.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(LAST_EVENT.load(AtomicOrdering::SeqCst), 78);
    assert_eq!(
        pending_stop_count(),
        0,
        "a direct call parks nothing, whatever its is_direct_call"
    );

    // SAFETY: `obj` is a live payload with an outstanding reference (the allocator's).
    unsafe { enif_release_resource(obj) };
}
