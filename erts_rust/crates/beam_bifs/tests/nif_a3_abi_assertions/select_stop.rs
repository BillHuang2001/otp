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

//! §3 — the `enif_select` **STOP verdicts** and the resource type's `stop` callback.
//!
//! The oracle transcript behind this module is `../HANDOFF_A3_NIF_HOST.md` §3 and its replay is
//! `../nif_a3_oracle_replay.rs`: STOP on an epoll-able fd (a pipe) answers
//! `17 = STOP_CALLED | READ_CANCELLED` and runs the callback **synchronously** with
//! `is_direct_call = 1`; STOP on an fd the kernel poll refuses (`/dev/null`, a regular file, a
//! directory) answers `18 = STOP_SCHEDULED | READ_CANCELLED` and runs it later, on the poller, with
//! `is_direct_call = 0`. The host owns the poller, so the *code* is the host's; what this module pins
//! is this crate's dispatch of that code and its reference accounting.
//!
//! | the host's answer | asserted here |
//! |---|---|
//! | `1` (`STOP_CALLED`) | one call, synchronous, `is_direct_call = true` |
//! | `17` (`STOP_CALLED \| READ_CANCELLED`) | the same — the CANCELLED bit does not defer it |
//! | `18` (`STOP_SCHEDULED \| READ_CANCELLED`) | nothing at return; exactly one drain runs it once with `is_direct_call = false`, a second drain runs nothing |
//! | `16` (`READ_CANCELLED`), `0`, a negative code | never |
//!
//! The code comes back **unchanged** in every case (the verdict is the host's), and each case checks
//! the reference count end to end: after the test releases its own reference the destructor ran
//! exactly once — for the deferred case only *after* the drain, which is what proves the parking lot's
//! own `enif_keep_resource` was taken (`erl_check_io.c:1509`) and released (`:2242`).
//!
//! **Never** drive a resource type opened **without** a `stop` callback through `enif_select`: the
//! oracle calls a NULL function pointer there and takes the VM down (SIGSEGV, 3/3 measured runs, the
//! fixture's `probe_stop_nostop_type/0`). The stop-less shape is a deliberate typed no-op in this
//! crate; only `resource_type_has_stop` is queried, and `enif_select` is never issued on it.
//!
//! The pending-stop table is **process-wide**, so [`STOP_LOCK`] serialises every case here and the
//! assertions read this test's own objects' counters rather than the global `pending_stop_count()`.

use std::cell::RefCell;
use std::ffi::CString;
use std::rc::Rc;
use std::sync::{Mutex, MutexGuard};

use libc::{c_int, c_void};

use beam_abi::common::{
    ERL_NIF_SELECT_READ_CANCELLED, ERL_NIF_SELECT_STOP_CALLED, ERL_NIF_SELECT_STOP_SCHEDULED,
};
use beam_abi::nif::enif::resources::{
    enif_alloc_resource, enif_init_resource_type, enif_release_resource, resource_type_has_stop,
};
use beam_abi::nif::enif::select::enif_select;
use beam_abi::nif::enif::stop::run_pending_stops;
use beam_abi::nif::types::{
    ErlNifEnv, ErlNifEvent, ErlNifResourceFlags, ErlNifResourceType, ErlNifResourceTypeInit,
};
use beam_abi::{nif_from_eterm, ErlNifSelectFlags, NifEnv, NifHost};
use beam_alloc::Heap;
use beam_term::atom::AtomTable;
use beam_term::prelude::{atom_val, is_atom, make_atom, Eterm, NIL};

/// Serialises the stop-verdict cases over the one process-wide pending-stop table.
///
/// A `Mutex` because the table is process-wide while the test binary runs its tests in parallel; the
/// assertions themselves read this test's own objects' counters, never `pending_stop_count()`.
static STOP_LOCK: Mutex<()> = Mutex::new(());

/// Every `stop` callback invocation, in call order: `(object address, event, is_direct_call)`.
static STOP_LOG: Mutex<Vec<(usize, ErlNifEvent, bool)>> = Mutex::new(Vec::new());

/// Every destructor invocation, in call order: the object address.
static DTOR_LOG: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// The `stop` log, tolerating poison (a panicking test must fail on its own assertions).
fn stop_log() -> MutexGuard<'static, Vec<(usize, ErlNifEvent, bool)>> {
    STOP_LOG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The destructor log, tolerating poison.
fn dtor_log() -> MutexGuard<'static, Vec<usize>> {
    DTOR_LOG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Every `stop` call recorded for `obj`: `(event, is_direct_call)`.
fn stop_calls(obj: *mut c_void) -> Vec<(ErlNifEvent, bool)> {
    stop_log()
        .iter()
        .filter(|(seen, _, _)| *seen == obj as usize)
        .map(|(_, event, direct)| (*event, *direct))
        .collect()
}

/// How many destructor runs were recorded for `obj`.
fn dtor_calls(obj: *mut c_void) -> usize {
    dtor_log()
        .iter()
        .filter(|seen| **seen == obj as usize)
        .count()
}

/// The witness resource type's `stop` callback — the C's
/// `void (*stop)(ErlNifEnv*, void* obj, ErlNifEvent event, int is_direct_call)`.
///
/// # Safety
///
/// `_env` must be null or a live env, `_obj` null or a live resource payload — the C contract of the
/// callback. The body only records values and dereferences no pointer.
unsafe extern "C" fn witness_stop(
    _env: *mut ErlNifEnv,
    obj: *mut c_void,
    event: ErlNifEvent,
    is_direct_call: c_int,
) {
    stop_log().push((obj as usize, event, is_direct_call != 0));
}

/// The witness resource type's destructor — the C's `void (*dtor)(ErlNifEnv*, void*)`.
///
/// # Safety
///
/// As [`witness_stop`]: `_env` null or live, `_obj` null or a dying payload. Nothing is dereferenced.
unsafe extern "C" fn witness_dtor(_env: *mut ErlNifEnv, obj: *mut c_void) {
    dtor_log().push(obj as usize);
}

/// A [`NifHost`] that answers a **scripted** `enif_select` verdict and records every select mode it
/// was asked for, so a case can prove the host really was consulted.
struct WitnessHost {
    heap: Heap,
    atoms: AtomTable,
    /// The code the `select` hook answers (`None` = the trait default, "unsupported").
    select_code: Option<c_int>,
    /// Every select mode this host was given, in call order.
    modes: Rc<RefCell<Vec<c_int>>>,
}

impl WitnessHost {
    fn new(select_code: c_int) -> WitnessHost {
        WitnessHost {
            heap: Heap::new(4096).expect("a test heap is allocatable"),
            atoms: AtomTable::new(),
            select_code: Some(select_code),
            modes: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Whether the host was asked to select on `ERL_NIF_SELECT_STOP` at least once.
    fn was_asked_to_stop(&self) -> bool {
        self.modes
            .borrow()
            .iter()
            .any(|bits| bits & ErlNifSelectFlags::STOP.bits() != 0)
    }
}

impl NifHost for WitnessHost {
    fn heap(&mut self) -> &mut Heap {
        &mut self.heap
    }

    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        Some(make_atom(self.atoms.intern(name)))
    }

    fn atom_name(&mut self, atom: Eterm) -> Option<String> {
        if !is_atom(atom) {
            return None;
        }
        self.atoms.name(atom_val(atom)).map(str::to_string)
    }

    fn select(
        &mut self,
        _event: ErlNifEvent,
        mode: ErlNifSelectFlags,
        _obj: *mut c_void,
        _pid: Option<Eterm>,
        _reference: Eterm,
    ) -> Option<c_int> {
        self.modes.borrow_mut().push(mode.bits());
        self.select_code
    }
}

/// Build one resource type over `host`: `dtor` + `stop` installed, `down` absent, `members = 3`
/// (the three callbacks `erl_nif.c:2747-2756` reads), opened with `CREATE | TAKEOVER` under a name
/// unique to the case (types are keyed by name and never freed).
///
/// The host must outlive every resource of the type: the type captures its pointer and dereferences
/// it to build the callback's environment.
fn open_stop_type(host: &mut WitnessHost, name: &str) -> *mut ErlNifResourceType {
    let c_name = CString::new(name).expect("no interior NUL");
    let init = ErlNifResourceTypeInit {
        dtor: Some(witness_dtor),
        stop: Some(witness_stop),
        down: None,
        members: 3,
        dyncall: None,
    };
    let flags = ErlNifResourceFlags::from_bits(
        ErlNifResourceFlags::CREATE.bits() | ErlNifResourceFlags::TAKEOVER.bits(),
    );
    let mut tried = ErlNifResourceFlags::from_bits(0);
    let mut env = NifEnv::new(host);
    // SAFETY: `env` is live, `c_name` is NUL-terminated, `init` is a live local of the frozen shape
    // and `tried` is a live local. `host` outlives the type (the caller keeps it alive).
    let ty = unsafe { enif_init_resource_type(env.as_ptr(), c_name.as_ptr(), &init, flags, &mut tried) };
    assert!(!ty.is_null(), "the witness resource type {name} opens");
    ty
}

/// What one stop-verdict case observed.
struct Observed {
    /// The code `enif_select` answered.
    returned: c_int,
    /// How many `stop` calls had been recorded when `enif_select` returned.
    calls_at_return: usize,
    /// `stop` calls recorded when `enif_select` returned (0 or 1).
    dtors_at_return: usize,
    /// Destructor runs after the test released its own reference (before any drain).
    dtors_after_release: usize,
    /// How many entries the first drain ran (`None` when the case does not drain).
    drained: Option<usize>,
    /// How many entries a second drain ran (`None` when the case does not drain).
    drained_again: Option<usize>,
    /// Every `stop` call this object ever got: `(event, is_direct_call)`.
    calls: Vec<(ErlNifEvent, bool)>,
    /// Destructor runs for this object at the end of the case.
    dtors_final: usize,
    /// Whether the host was asked to select on `ERL_NIF_SELECT_STOP`.
    asked_select: bool,
}

/// Drive one verdict through `enif_select` on a fresh object, releasing the test's own reference and
/// (when `deferred`) draining the pending-stop table **after** that release — which is the order that
/// proves the parked entry's own `enif_keep_resource` is what kept the object alive.
fn run_stop_case(
    host: &mut WitnessHost,
    verdict: c_int,
    event: ErlNifEvent,
    type_name: &str,
    deferred: bool,
) -> Observed {
    // The case's host answers this case's scripted code; checking the two agree keeps a future case
    // from silently driving a verdict nobody installed a host for.
    assert_eq!(
        host.select_code,
        Some(verdict),
        "the host for {type_name} answers the verdict under test"
    );
    stop_log().clear();
    dtor_log().clear();
    let ty = open_stop_type(host, type_name);
    // SAFETY: `ty` was just opened; the payload is 8 bytes this test owns and never reads.
    let obj = unsafe { enif_alloc_resource(ty, 8) };
    assert!(!obj.is_null(), "the witness resource object allocates");

    let returned = {
        let mut env = NifEnv::new(host);
        // SAFETY: `env` is live, `obj` is a live payload this test still references, `pid` is null
        // (the calling process) and the reference term is the empty list (irrelevant to STOP).
        unsafe {
            enif_select(
                env.as_ptr(),
                event,
                ErlNifSelectFlags::STOP,
                obj,
                core::ptr::null(),
                nif_from_eterm(NIL),
            )
        }
    };
    let calls_at_return = stop_calls(obj).len();
    let dtors_at_return = dtor_calls(obj);

    // SAFETY: `obj` is a live payload whose remaining reference (if any) is the allocator's own.
    unsafe { enif_release_resource(obj) };
    let dtors_after_release = dtor_calls(obj);

    let drained = deferred.then(run_pending_stops);
    let drained_again = deferred.then(run_pending_stops);

    Observed {
        returned,
        calls_at_return,
        dtors_at_return,
        dtors_after_release,
        drained,
        drained_again,
        calls: stop_calls(obj),
        dtors_final: dtor_calls(obj),
        asked_select: host.was_asked_to_stop(),
    }
}

/// The oracle's `enif_select` STOP verdict matrix, one fresh object per case.
///
/// Each case's host lives in a `Box` for the whole test, because the resource type captures the
/// host's address and dereferences it to build the `stop`/`dtor` callback's environment.
///
/// A resource type opened **without** a `stop` callback is deliberately never passed to `enif_select`
/// here: the oracle calls a NULL function pointer and takes the VM down (SIGSEGV, 3/3 measured runs —
/// `a3_resource_stop:probe_stop_nostop_type/0`), while this crate's dispatch is a typed no-op
/// (`resource_type_has_stop` is the query). Only that query is asserted.
#[test]
fn enif_select_stop_verdicts_run_the_right_callback() {
    let _serial = STOP_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    // One host per case, boxed: the types keep the pointer for the life of this test.
    let mut hosts: Vec<Box<WitnessHost>> = Vec::new();
    let codes: [c_int; 6] = [
        // 1 — STOP_CALLED alone: the C's fast track for an fd with no event state.
        ERL_NIF_SELECT_STOP_CALLED,
        // 17 — STOP_CALLED | READ_CANCELLED: the fixture's epoll-able pipe arm.
        ERL_NIF_SELECT_STOP_CALLED | ERL_NIF_SELECT_READ_CANCELLED,
        // 18 — STOP_SCHEDULED | READ_CANCELLED: the /dev/null arm, deferred to the poller.
        ERL_NIF_SELECT_STOP_SCHEDULED | ERL_NIF_SELECT_READ_CANCELLED,
        // 16 — READ_CANCELLED only: a cancelled interest, not a stop.
        ERL_NIF_SELECT_READ_CANCELLED,
        // 0 — nothing to do.
        0,
        // -1 — the C's INT_MIN | bit refusals are negative too.
        -1,
    ];
    for code in codes {
        hosts.push(Box::new(WitnessHost::new(code)));
    }

    // ---- 1 = STOP_CALLED: the callback runs inside enif_select, with is_direct_call = 1.
    let observed = run_stop_case(
        &mut hosts[0],
        codes[0],
        31,
        "a3_stop_called",
        false,
    );
    assert_eq!(
        observed.returned, codes[0],
        "the host's code is answered unchanged"
    );
    assert!(observed.asked_select, "the host really was asked to STOP");
    assert_eq!(
        observed.calls,
        vec![(31, true)],
        "STOP_CALLED runs the callback exactly once, synchronously (no drain took place) and with \
         is_direct_call = 1 (`erl_check_io.c:1278-1285`, `:1531`)"
    );
    assert_eq!(
        observed.calls_at_return, 1,
        "the callback had already run when enif_select returned"
    );
    assert_eq!(observed.dtors_at_return, 0, "the object is alive");
    assert_eq!(
        observed.dtors_after_release, 1,
        "the fast track is the C's CALL_STOP: the caller still owns its reference, so the single \
         release destroys the object"
    );
    assert_eq!(observed.dtors_final, 1, "the destructor runs exactly once");

    // ---- 17 = STOP_CALLED | READ_CANCELLED: still synchronous, still direct (the pipe arm).
    let observed = run_stop_case(
        &mut hosts[1],
        codes[1],
        32,
        "a3_stop_called_cancelled_read",
        false,
    );
    assert_eq!(
        observed.returned, codes[1],
        "the composed code is answered unchanged"
    );
    assert_eq!(
        observed.calls,
        vec![(32, true)],
        "the CANCELLED bit does not make the call deferred: the callback runs once, directly"
    );
    assert_eq!(observed.calls_at_return, 1, "and it ran inside enif_select");
    assert_eq!(
        observed.dtors_after_release, 1,
        "the reference released at :1502 is the runtime's armed interest, not the caller's"
    );
    assert_eq!(observed.dtors_final, 1);

    // ---- 18 = STOP_SCHEDULED | READ_CANCELLED: nothing at return, exactly one drain runs it.
    let observed = run_stop_case(
        &mut hosts[2],
        codes[2],
        33,
        "a3_stop_scheduled",
        true,
    );
    assert_eq!(
        observed.returned, codes[2],
        "the host's code is answered unchanged"
    );
    assert!(observed.asked_select, "the host really was asked to STOP");
    assert_eq!(
        observed.calls_at_return, 0,
        "a deferred verdict calls nothing from enif_select"
    );
    assert_eq!(
        observed.dtors_at_return, 0,
        "nothing ran, so nothing was destroyed"
    );
    assert_eq!(
        observed.dtors_after_release, 0,
        "the parked stop's own enif_keep_resource (`erl_check_io.c:1509`) holds the object alive \
         even after the test released its reference"
    );
    assert_eq!(
        observed.drained,
        Some(1),
        "the first drain runs the one parked stop"
    );
    assert_eq!(
        observed.calls,
        vec![(33, false)],
        "the drain runs the callback once with the parked event handle and is_direct_call = 0 \
         (`:2241`)"
    );
    assert_eq!(
        observed.drained_again,
        Some(0),
        "a second drain has nothing left to run"
    );
    assert_eq!(
        observed.dtors_final, 1,
        "the drain released only its own keep, so the object is destroyed exactly once"
    );

    // ---- 16 / 0 / -1: nothing runs, nothing is parked, and the code comes back unchanged.
    for (index, venue) in [(3usize, 34 as ErlNifEvent), (4, 35), (5, 36)] {
        let observed = run_stop_case(
            &mut hosts[index],
            codes[index],
            venue,
            &format!("a3_no_stop_{}", codes[index].unsigned_abs()),
            false,
        );
        assert_eq!(
            observed.returned, codes[index],
            "the host's refusal/no-op code {} is answered unchanged",
            codes[index]
        );
        assert!(
            observed.asked_select,
            "the host was consulted even for code {}",
            codes[index]
        );
        assert_eq!(
            observed.calls,
            Vec::new(),
            "code {} is not a STOP verdict: no callback, no parked entry",
            codes[index]
        );
        assert_eq!(
            observed.dtors_after_release, 1,
            "nothing was kept, so the caller's one release destroys the object (code {})",
            codes[index]
        );
        assert_eq!(observed.dtors_final, 1);
    }

    // ---- The stop-less shape: a typed no-op, and deliberately NOT driven through `enif_select`.
    // The logs are cleared first: the object below may reuse an address a destroyed object had, and
    // both counters are keyed by address.
    stop_log().clear();
    dtor_log().clear();
    let ty = {
        let init = ErlNifResourceTypeInit {
            dtor: Some(witness_dtor),
            stop: None,
            down: None,
            members: 3,
            dyncall: None,
        };
        let c_name = CString::new("a3_stop_without_a_callback").expect("no interior NUL");
        let flags = ErlNifResourceFlags::from_bits(
            ErlNifResourceFlags::CREATE.bits() | ErlNifResourceFlags::TAKEOVER.bits(),
        );
        let mut tried = ErlNifResourceFlags::from_bits(0);
        let mut env = NifEnv::new(&mut *hosts[0]);
        // SAFETY: live env, NUL-terminated name, live `init`/`tried` locals.
        unsafe { enif_init_resource_type(env.as_ptr(), c_name.as_ptr(), &init, flags, &mut tried) }
    };
    assert!(!ty.is_null(), "the stop-less type opens");
    // SAFETY: `ty` is the type just opened; the payload is 8 bytes this test owns.
    let stopless = unsafe { enif_alloc_resource(ty, 8) };
    assert!(!stopless.is_null());
    assert!(
        !resource_type_has_stop(stopless),
        "a type opened without a `stop` callback is reported as such — which is exactly why this \
         crate makes the STOP dispatch a typed no-op instead of the oracle's NULL call"
    );
    // SAFETY: `stopless` is a live payload with the allocator's reference.
    unsafe { enif_release_resource(stopless) };
    assert_eq!(
        dtor_calls(stopless),
        1,
        "the stop-less type still has its destructor"
    );
}
