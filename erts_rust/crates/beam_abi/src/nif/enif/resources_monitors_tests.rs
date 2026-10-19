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

//! Tests for the resource **monitor tree** and the `down`-callback delivery seam — the sibling child
//! of `resources.rs` (path-included next to `resources_tests.rs`).
//!
//! Every rule the C states about `erts_fire_nif_monitor` (`erts/emulator/beam/erl_nif.c:3058-3115`) is
//! pinned in-process here: the callback runs **once** per armed monitor with the object, the target pid
//! and the whole stored reference block the arm wrote; a second delivery is refused (the C unlinks the tree
//! entry before it calls, `:3091`); a monitor whose resource was destroyed never fires
//! (`run_resource_dtor` → `dtor_demonitor`, `:2974-3030`); and a `down` callback that calls back into
//! the crate — `enif_get_resource`, `enif_release_resource`, even a destructor — does not deadlock,
//! because the crate never holds its registry lock across the callback.
//!
//! # Test isolation (the registry is process-wide, the tests run in parallel)
//! Every object carries a **unique marker** in its payload and the recording callbacks report that
//! marker, not just the object address: `libc::malloc` reuses a freed block immediately, so a parallel
//! test can allocate a new object at the address a destroyed one had. Object *types* are unique per
//! test for the same reason (they are keyed by name and never freed).
//!
//! # The oracle half (a recorded measurement, re-measured when the toolchain is present)
//!
//! `fixtures/nif_resource_down.c` is a real C NIF whose resource type carries a `down` callback; it is
//! compiled at test time with the system `cc` against the system `erl_nif.h` and run under the system
//! `erl` (OTP 29.1.1 / erts-17.1), which is the oracle column this file pins. The recorded transcript
//! is reproduced by [`the_same_fixture_agrees_with_the_measured_oracle_transcript`].
//!
//! **The `.so` is never loaded into the Rust side.** This is a `src/` unit test, so its binary is the
//! crate's *lib test* target — `build.rs` emits `-Wl,--export-dynamic` for `tests/` targets only, so
//! the `enif_*` symbols are not in `.dynsym` and a fixture `dlopen(RTLD_NOW)` would fail on the first
//! unresolved import (`src/nif/CONTEXT.md` → *Symbol-export mechanism*). The Rust half of every
//! assertion below is therefore the **in-process** `down` callback, driven through the real
//! `enif_monitor_process`/`enif_demonitor_process`/`deliver_resource_down_id` entry points.
//!
//! Skip-vs-fail policy (the same as `tests/common/mod.rs`): without `erl`/`erlc` or a C compiler the
//! oracle test prints a loud `SKIP:` banner and returns; whenever the toolchain is present every
//! assertion is hard.

use super::*;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::Mutex;

use crate::nif::enif::process::{MONITOR_BYTES, REF_THING_BYTES};
use crate::nif::host::NifMonitorResult;
use crate::nif::types::{
    ErlNifMonitor, ErlNifPid, ErlNifResourceDown, ErlNifResourceFlags, ErlNifResourceType,
    ErlNifResourceTypeInit,
};

use beam_term::prelude::{is_small, make_atom, make_internal_pid, make_pid_data};

// ---------------------------------------------------------------------------------------------
// The arming host
// ---------------------------------------------------------------------------------------------

/// A [`NifHost`] with a bump heap, an atom table and the **three-way** monitor hook — the shape a
/// real host (`beam_rt`'s `NifHostImpl`) must have for an arm to succeed.
struct ArmingHost {
    heap: beam_alloc::Heap,
    atoms: beam_term::atom::AtomTable,
    /// What [`NifHost::monitor_process_result`] answers (`None` = decline, fall back to the legacy
    /// hook).
    result: Option<NifMonitorResult>,
}

impl ArmingHost {
    fn new() -> ArmingHost {
        ArmingHost {
            heap: beam_alloc::Heap::new(4096).expect("a small test heap is allocatable"),
            atoms: beam_term::atom::AtomTable::new(),
            result: None,
        }
    }

    /// Mint the internal reference an arming host hands over — built with the crate's canonical writer,
    /// so it is the frozen 4-word M1 block `[header, node, serial, id]` every producer in the tree
    /// mints (`beam_process::MonitorRef`, `beam_bifs`'s `make_ref_thing`, `beam_rt`) — and keep it on
    /// this host's heap, so the whole `REF_THING_BYTES` block stays readable for the whole test.
    ///
    /// The `id` is what makes two references distinct: `node`/`serial` are pinned to `make_small(0)`
    /// for every reference, so the identity is entirely in word 3 — which is exactly why the stored
    /// block has to be the whole 4 words.
    fn mint_ref(&mut self, id: usize) -> Eterm {
        // SAFETY: fresh, exclusively owned words from the test heap.
        let p = unsafe { self.heap.alloc_words(prelude::INTERNAL_REF_WORDS) }
            .expect("the test heap has room");
        // SAFETY: `p` addresses those words; `write_internal_ref` writes exactly `INTERNAL_REF_WORDS`
        // of them before the root escapes.
        unsafe { prelude::write_internal_ref(p, id as u64) }.expect("the id fits the M1 id slot")
    }

    /// `enif_monitor_process` with `reference` as the armed monitor's identity:
    /// `(the C's return code, holder)`.
    fn arm_with(&mut self, obj: *mut c_void, pid: Eterm, reference: Eterm) -> (c_int, ErlNifMonitor) {
        self.result = Some(NifMonitorResult::Armed(reference));
        let mut holder = ErlNifMonitor {
            data: [0u8; MONITOR_BYTES],
        };
        let nif_pid = ErlNifPid { pid: n(pid) };
        let code = {
            // The env lives only for this call: the type's host pointer is this same `host`, and the
            // delivery path below re-borrows it, so the two `&mut` borrows must not overlap.
            let mut env = NifEnv::new(self);
            // SAFETY: `env` is live; `obj` is a live payload pointer; `nif_pid`/`holder` are live
            // locals.
            unsafe { enif_monitor_process(env.as_ptr(), obj, &nif_pid, &mut holder) }
        };
        (code, holder)
    }

    /// Mint a fresh reference, arm with it and return the holder.
    fn arm(&mut self, obj: *mut c_void, pid: Eterm, id: usize) -> (c_int, ErlNifMonitor) {
        let reference = self.mint_ref(id);
        self.arm_with(obj, pid, reference)
    }
}

impl NifHost for ArmingHost {
    fn heap(&mut self) -> &mut beam_alloc::Heap {
        &mut self.heap
    }

    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        Some(make_atom(self.atoms.intern(name)))
    }

    fn atom_name(&mut self, atom: Eterm) -> Option<String> {
        if !prelude::is_atom(atom) {
            return None;
        }
        self.atoms.name(prelude::atom_val(atom)).map(str::to_string)
    }

    fn monitor_process_result(&mut self, _pid: Eterm) -> Option<NifMonitorResult> {
        self.result
    }
}

// ---------------------------------------------------------------------------------------------
// The recording callbacks
// ---------------------------------------------------------------------------------------------

/// One observation a `down` callback made: the object's unique marker, its address, the pid operand's
/// raw word and the whole stored reference block (`REF_THING_BYTES` bytes).
#[derive(Clone, PartialEq, Eq, Debug)]
struct Observed {
    marker: usize,
    obj: usize,
    pid: usize,
    monitor: [u8; REF_THING_BYTES],
}

/// Every `down` this test binary ran; filtered by the object's **marker** (never by its address, which
/// a freed block's successor can reuse).
static OBSERVED: Mutex<Vec<Observed>> = Mutex::new(Vec::new());

/// The marker of every object whose destructor ran.
static DTORS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

fn observations_for(marker: usize) -> Vec<Observed> {
    let seen = lock(&OBSERVED);
    seen.iter()
        .filter(|observed| observed.marker == marker)
        .cloned()
        .collect()
}

fn dtors_for(marker: usize) -> usize {
    lock(&DTORS).iter().filter(|&&m| m == marker).count()
}

/// Lock a test-side `Mutex`, recovering a poisoned guard (a failing assertion in another thread must
/// not turn into a panic cascade).
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Read an object's marker (the first payload word).
///
/// # Safety
///
/// `obj` must be one of this file's objects — a payload of at least `size_of::<usize>()` bytes that is
/// still allocated (true inside `down` and inside a destructor, which runs before the block is freed).
unsafe fn marker_of(obj: *mut c_void) -> usize {
    // SAFETY: forwarded from the caller's contract.
    unsafe { *(obj as *const usize) }
}

/// Record what the callback observed; the body calls nothing, so it cannot deadlock or recurse.
///
/// # Safety
///
/// The C `ErlNifResourceDown` ABI: `obj` is a resource payload, and `pid`/`monitor` are the operands
/// `erts_fire_nif_monitor` builds (`erl_nif.c:3105-3106`), each read only after a null test.
unsafe extern "C" fn recording_down(
    _env: *mut crate::nif::types::ErlNifEnv,
    obj: *mut c_void,
    pid: *mut ErlNifPid,
    monitor: *mut ErlNifMonitor,
) {
    let mut bytes = [0u8; REF_THING_BYTES];
    // SAFETY: `monitor` is null or a live holder the caller built (ABI contract).
    if let Some(monitor) = unsafe { monitor.as_ref() } {
        bytes.copy_from_slice(&monitor.data[..REF_THING_BYTES]);
    }
    // SAFETY: `pid` is null or points at a readable `ErlNifPid` (ABI contract).
    let pid_word = unsafe { pid.as_ref() }.map_or(0, |pid| pid.pid.raw());
    // SAFETY: `obj` is one of this file's marked objects.
    let marker = unsafe { marker_of(obj) };
    lock(&OBSERVED).push(Observed {
        marker,
        obj: obj as usize,
        pid: pid_word,
        monitor: bytes,
    });
}

/// A destructor that records the marker of the object it ran for; the body calls nothing else.
///
/// # Safety
///
/// The C `ErlNifResourceDtor` ABI; `obj` is this file's marked object and its block is still allocated
/// (the destructor runs before the free).
unsafe extern "C" fn recording_dtor(_env: *mut crate::nif::types::ErlNifEnv, obj: *mut c_void) {
    // SAFETY: see the `# Safety` section.
    let marker = unsafe { marker_of(obj) };
    lock(&DTORS).push(marker);
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The next unique object marker.
fn next_marker() -> usize {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    NEXT.fetch_add(1, AtomicOrdering::SeqCst)
}

/// Open a resource type called `name` with `down`, through `enif_init_resource_type` (`members = 3`,
/// i.e. `dtor`+`stop`+`down` initialised — the C's `init_members` switch, `erl_nif.c:2747-2756`).
///
/// Each test uses its own name: types are keyed by name and never freed (the C's global
/// `resource_type_list`), and every test in this binary shares the process-wide registry.
fn open_down_type(
    env: &mut NifEnv,
    name: &str,
    down: ErlNifResourceDown,
) -> *mut ErlNifResourceType {
    use std::ffi::CString;

    let c_name = CString::new(name).expect("no interior NUL");
    let init = ErlNifResourceTypeInit {
        dtor: Some(recording_dtor),
        stop: None,
        down: Some(down),
        members: 3,
        dyncall: None,
    };
    let mut tried = ErlNifResourceFlags::from_bits(0);
    // SAFETY: `env` is live; `c_name` is NUL-terminated; `init`/`tried` are live locals.
    let ty = unsafe {
        enif_init_resource_type(
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

/// An object of `ty`, carrying a fresh marker: `(obj, marker)` — the caller owns its one reference.
///
/// # Safety
///
/// `ty` must be a type from [`open_down_type`].
unsafe fn alloc_tagged(ty: *mut ErlNifResourceType) -> (*mut c_void, usize) {
    // SAFETY: forwarded from the caller's contract.
    let obj = unsafe { enif_alloc_resource(ty, core::mem::size_of::<usize>()) };
    assert!(!obj.is_null(), "the allocation must succeed");
    let marker = next_marker();
    // SAFETY: `obj` is a live payload of `size_of::<usize>()` bytes.
    unsafe { *(obj as *mut usize) = marker };
    (obj, marker)
}

/// Open a type for one test and allocate the test's first object: `(type, obj, marker)`.
fn type_and_object(
    host: &mut ArmingHost,
    name: &str,
    down: ErlNifResourceDown,
) -> (*mut ErlNifResourceType, *mut c_void, usize) {
    let ty = {
        let mut env = NifEnv::new(host);
        open_down_type(&mut env, name, down)
    };
    // SAFETY: `ty` came from the open call above.
    let (obj, marker) = unsafe { alloc_tagged(ty) };
    (ty, obj, marker)
}

/// The [`MonitorId`] a holder carries.
#[track_caller]
fn id_of(holder: &ErlNifMonitor) -> MonitorId {
    // SAFETY: `holder` is a live local.
    unsafe { MonitorId::from_holder(holder) }.expect("an armed holder carries a reference block")
}

/// A pid term to arm on (the arm only compares it against the C's undefined marker, so any live pid
/// will do).
fn pid_term() -> Eterm {
    make_internal_pid(make_pid_data(3, 7))
}

/// `enif_demonitor_process(env, obj, holder)` as the C's `0`/`1`.
fn demonitor(host: &mut ArmingHost, obj: *mut c_void, holder: &ErlNifMonitor) -> c_int {
    let mut env = NifEnv::new(host);
    // SAFETY: `env` is live; `holder` is a live local.
    unsafe { enif_demonitor_process(env.as_ptr(), obj, holder) }
}

// ---------------------------------------------------------------------------------------------
// 1. Delivery runs the callback once, with the C's three operands
// ---------------------------------------------------------------------------------------------

/// The C `erts_fire_nif_monitor` contract (`erl_nif.c:3105-3108`): one delivery runs `down` **once**
/// with the resource payload pointer, the target pid and the very reference block the arm stored in
/// the caller's holder (the whole `REF_THING_BYTES`-byte block).
#[test]
fn a_delivered_down_reports_the_object_the_pid_and_the_stored_monitor_bytes() {
    let mut host = ArmingHost::new();
    let (_ty, obj, marker) = type_and_object(&mut host, "down_deliver_rt", recording_down);
    let pid = pid_term();
    let (code, holder) = host.arm(obj, pid, 0x5eed_0001);
    assert_eq!(code, 0, "the arm must be established (the C's 0)");

    let id = id_of(&holder);
    assert!(
        deliver_resource_down_id(id, pid),
        "an armed monitor must deliver"
    );

    let seen = observations_for(marker);
    assert_eq!(seen.len(), 1, "the callback must run exactly once");
    assert_eq!(seen[0].obj, obj as usize, "the object operand is the payload");
    assert_eq!(
        seen[0].pid,
        n(pid).raw(),
        "the pid operand is the target pid the host passed"
    );
    assert_eq!(
        seen[0].monitor,
        id.bytes(),
        "the monitor operand carries the reference block the arm stored"
    );

    // SAFETY: the allocator's one reference is still outstanding.
    unsafe { enif_release_resource(obj) };
}

/// The term form of the same call: `deliver_resource_down(Armed(term), pid)` — the route a host takes
/// when it still holds the minted reference rather than the holder.
#[test]
fn the_reference_term_form_delivers_the_same_way() {
    let mut host = ArmingHost::new();
    let (_ty, obj, marker) = type_and_object(&mut host, "down_term_rt", recording_down);
    let pid = pid_term();
    let reference = host.mint_ref(0x5eed_0010);
    let (code, holder) = host.arm_with(obj, pid, reference);
    assert_eq!(code, 0);
    let id = id_of(&holder);
    assert_eq!(
        MonitorId::from_term(reference),
        Some(id),
        "the armed identity IS the minted reference's block"
    );

    assert!(
        deliver_resource_down(reference, pid),
        "the reference-term form must deliver"
    );
    let seen = observations_for(marker);
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].monitor,
        id.bytes(),
        "both forms address the same stored block"
    );
    assert!(
        !deliver_resource_down(reference, pid),
        "and only once, like the id form"
    );

    // SAFETY: the allocator's one reference is still outstanding.
    unsafe { enif_release_resource(obj) };
}

// ---------------------------------------------------------------------------------------------
// 2. A second delivery is refused
// ---------------------------------------------------------------------------------------------

/// The C deletes the monitor from the resource's tree **before** it calls `down`
/// (`erts_monitor_tree_delete`, `erl_nif.c:3091`), so a monitor can never fire twice; here the binding
/// is taken out under the registry lock, which leaves exactly one caller able to deliver.
#[test]
fn a_second_delivery_of_the_same_monitor_is_refused() {
    let mut host = ArmingHost::new();
    let (_ty, obj, marker) = type_and_object(&mut host, "down_twice_rt", recording_down);
    let pid = pid_term();
    let (code, holder) = host.arm(obj, pid, 0x5eed_0020);
    assert_eq!(code, 0);
    let id = id_of(&holder);

    assert!(deliver_resource_down_id(id, pid), "the first delivery runs");
    assert!(
        !deliver_resource_down_id(id, pid),
        "the second delivery must be refused"
    );
    assert_eq!(
        observations_for(marker).len(),
        1,
        "the callback must not run twice"
    );
    assert!(
        !deliver_resource_down_id(id, pid),
        "and it stays refused"
    );
    assert_eq!(observations_for(marker).len(), 1);

    // SAFETY: the allocator's one reference is still outstanding.
    unsafe { enif_release_resource(obj) };
}

// ---------------------------------------------------------------------------------------------
// 3. A monitor nothing knows about delivers nothing
// ---------------------------------------------------------------------------------------------

/// A monitor that was never armed — or one `enif_demonitor_process` already removed — has no binding,
/// so delivery answers `false` and runs no callback.
#[test]
fn an_unknown_or_already_demonitored_monitor_delivers_nothing() {
    let zeroed = ErlNifMonitor {
        data: [0u8; MONITOR_BYTES],
    };
    assert!(
        !deliver_resource_down_id(id_of(&zeroed), pid_term()),
        "a never-armed monitor must not deliver"
    );
    assert!(
        !deliver_resource_down(Eterm::from_raw(0), pid_term()),
        "a non-reference term must not deliver"
    );

    let mut host = ArmingHost::new();
    let (_ty, obj, marker) = type_and_object(&mut host, "down_unknown_rt", recording_down);
    let pid = pid_term();
    let (code, holder) = host.arm(obj, pid, 0x5eed_0030);
    assert_eq!(code, 0);
    let id = id_of(&holder);

    // `enif_demonitor_process` removes the binding without firing `down` (`erl_nif.c:4046-4048`).
    assert_eq!(
        demonitor(&mut host, obj, &holder),
        0,
        "the C's 0 = identified and removed"
    );
    assert!(
        !deliver_resource_down_id(id, pid),
        "a demonitored monitor must not deliver"
    );
    assert!(observations_for(marker).is_empty(), "and no callback ran");

    // SAFETY: the allocator's one reference is still outstanding.
    unsafe { enif_release_resource(obj) };
}

// ---------------------------------------------------------------------------------------------
// 4. `enif_demonitor_process` answers from our tree
// ---------------------------------------------------------------------------------------------

/// The C keys the lookup on the resource's own monitor tree (`erl_nif.c:4046`): `0` when it found and
/// removed the entry, `1` when it did not — so an armed monitor answers `0`, the same holder answers
/// `1` afterwards, and a monitor whose `down` already fired answers `1` (the fixture transcript's
/// `{demonitor_armed,0}` / `{demonitor_after_fire,1}` / `{demonitor_after_demonitor,1}`).
#[test]
fn the_demonitor_codes_are_the_cs_once_our_tree_is_consulted() {
    let mut host = ArmingHost::new();
    let (_ty, obj, marker) = type_and_object(&mut host, "down_demonitor_rt", recording_down);
    let pid = pid_term();
    let (code, holder) = host.arm(obj, pid, 0x5eed_0040);
    assert_eq!(code, 0);

    assert_eq!(
        demonitor(&mut host, obj, &holder),
        0,
        "an armed monitor is identified and removed"
    );
    assert_eq!(
        demonitor(&mut host, obj, &holder),
        1,
        "the second demonitor cannot find it"
    );

    // After a fired `down`: `1` too (the entry was unlinked before the call).
    let (code, holder) = host.arm(obj, pid, 0x5eed_0041);
    assert_eq!(code, 0);
    let id = id_of(&holder);
    assert!(deliver_resource_down_id(id, pid));
    assert_eq!(
        demonitor(&mut host, obj, &holder),
        1,
        "a fired monitor cannot be demonitored"
    );

    // `enif_compare_monitors` / `enif_make_monitor_term` still agree with the stored payload: the
    // rebuilt holder carries the same block the arm wrote.
    let mut rebuilt = ErlNifMonitor {
        data: [0u8; MONITOR_BYTES],
    };
    rebuilt.data[..REF_THING_BYTES].copy_from_slice(&id.bytes());
    // SAFETY: both holders are live locals.
    assert_eq!(
        unsafe { enif_compare_monitors(&holder, &rebuilt) },
        0,
        "the rebuilt holder compares equal to the stored one"
    );
    let term = {
        let mut env = NifEnv::new(&mut host);
        // SAFETY: `env` is live; `holder` is a live local.
        t(unsafe { enif_make_monitor_term(env.as_ptr(), &holder) })
    };
    assert_eq!(
        MonitorId::from_term(term),
        Some(id),
        "the monitor term names the same stored block"
    );
    assert!(
        !is_small(term),
        "the monitor term is a reference, not a small integer"
    );
    assert_eq!(observations_for(marker).len(), 1);

    // SAFETY: the allocator's one reference is still outstanding.
    unsafe { enif_release_resource(obj) };
}

// ---------------------------------------------------------------------------------------------
// 4b. Two monitors armed from different references stay distinct
// ---------------------------------------------------------------------------------------------

/// **The monitor identity is the WHOLE stored reference block, not the C's first 24 bytes.**
///
/// This tree's frozen internal reference is the 4-word M1 block `[header, node, serial, id]`
/// (`beam_term::boxed::INTERNAL_REF_WORDS`): `node` and `serial` are pinned to `make_small(0)` for
/// every reference in a runtime and the identity lives in **word 3**. A block that stopped at the C's
/// 3 words (`ERTS_REF_THING_SIZE`, `erl_term.h:990`) would therefore be byte-identical for every
/// monitor, so two distinct monitors would collapse into ONE tree entry — the first
/// `enif_demonitor_process` would cancel the second's arm and its `down` could never fire.
///
/// Pinned here, in both directions:
/// * two monitors minted from two **different** references have different `MonitorId` bytes (their
///   first three words are identical — that is exactly what made the narrow block collapse them),
///   demonitoring the first leaves the second armed, and the second's `down` still fires;
/// * the **same** reference armed twice still compares equal (`enif_compare_monitors` == 0) and
///   shares one tree entry — the legitimate "same monitor" case the widening must not break.
#[test]
fn two_monitors_minted_from_different_references_stay_distinct() {
    let mut host = ArmingHost::new();
    let (_ty, obj, marker) = type_and_object(&mut host, "down_two_refs_rt", recording_down);
    let pid = pid_term();

    // Two references that differ only in the id word (word 3).
    let first_ref = host.mint_ref(0x11);
    let second_ref = host.mint_ref(0x22);
    let (first_code, first) = host.arm_with(obj, pid, first_ref);
    let (second_code, second) = host.arm_with(obj, pid, second_ref);
    assert_eq!((first_code, second_code), (0, 0), "both arms establish");

    let first_id = id_of(&first);
    let second_id = id_of(&second);
    assert_eq!(
        MonitorId::from_term(first_ref),
        Some(first_id),
        "the identity of the first arm is its reference's block"
    );
    assert_eq!(MonitorId::from_term(second_ref), Some(second_id));
    assert_ne!(
        first_id, second_id,
        "two monitors minted from different references must have different identities"
    );
    assert_ne!(
        first_id.bytes(),
        second_id.bytes(),
        "and different stored bytes"
    );
    // The blocks differ in the id word only — so a 3-word (C-width) block would have been identical
    // for both and the two monitors would have been one entry.
    let block_words_but_the_id = 3 * core::mem::size_of::<Eterm>();
    assert_eq!(
        first_id.bytes()[..block_words_but_the_id],
        second_id.bytes()[..block_words_but_the_id],
        "words 0..2 (header, node, serial) are identical for every reference in this tree"
    );
    assert_ne!(
        first_id.bytes()[block_words_but_the_id..],
        second_id.bytes()[block_words_but_the_id..],
        "the identity lives in word 3, which only the widened block carries"
    );

    // The first demonitor must not take the second arm with it.
    assert_eq!(
        demonitor(&mut host, obj, &first),
        0,
        "the first monitor is identified and removed"
    );
    assert!(
        resource_monitors().iter().any(|(m, _)| *m == second_id),
        "the second monitor is still armed after the first was cancelled"
    );
    assert!(
        deliver_resource_down(second_ref, pid),
        "so the second monitor's `down` still fires"
    );
    let seen = observations_for(marker);
    assert_eq!(seen.len(), 1, "exactly one `down` ran");
    assert_eq!(
        seen[0].monitor,
        second_id.bytes(),
        "and for the second monitor's block"
    );
    assert!(
        !deliver_resource_down(first_ref, pid),
        "the cancelled monitor never fires"
    );

    // The legitimate "same monitor" case: one reference armed twice is ONE monitor.
    let same_ref = host.mint_ref(0x33);
    let (again_code, again_a) = host.arm_with(obj, pid, same_ref);
    let (twice_code, again_b) = host.arm_with(obj, pid, same_ref);
    assert_eq!((again_code, twice_code), (0, 0), "both arms establish");
    // SAFETY: both holders are initialised locals.
    assert_eq!(
        unsafe { enif_compare_monitors(&again_a, &again_b) },
        0,
        "the same reference armed twice is the same monitor"
    );
    assert_eq!(id_of(&again_a), id_of(&again_b), "and the same identity");
    assert_eq!(
        demonitor(&mut host, obj, &again_a),
        0,
        "one demonitor cancels it (a single tree entry)"
    );
    assert_eq!(
        demonitor(&mut host, obj, &again_b),
        1,
        "and nothing is left to cancel"
    );
    assert!(
        !deliver_resource_down(same_ref, pid),
        "a cancelled monitor cannot deliver"
    );

    // SAFETY: the allocator's one reference is still outstanding.
    unsafe { enif_release_resource(obj) };
}

// ---------------------------------------------------------------------------------------------
// 5. A destroyed resource cancels its monitors silently
// ---------------------------------------------------------------------------------------------

/// The C `run_resource_dtor` → `dtor_demonitor` path (`erl_nif.c:2940-2948`, `:2974-3030`): a dying
/// resource cancels every monitor in its tree and runs **no** `down`, so a monitor that outlived its
/// resource never fires. Measured on the oracle: arm, release-to-zero, kill the target ⇒ the `down`
/// count is still `0` (`{state_after_release_kill,{0,none,-1}}` in the fixture transcript).
#[test]
fn releasing_the_last_reference_cancels_an_armed_monitor_silently() {
    let mut host = ArmingHost::new();
    let (_ty, obj, marker) = type_and_object(&mut host, "down_cancel_rt", recording_down);
    let pid = pid_term();
    let (code, holder) = host.arm(obj, pid, 0x5eed_0050);
    assert_eq!(code, 0);
    let id = id_of(&holder);
    assert!(
        resource_monitors().iter().any(|(m, _)| *m == id),
        "the armed monitor is listed while the object is alive"
    );

    // SAFETY: the allocator's one reference; this is the release that reaches zero.
    unsafe { enif_release_resource(obj) };

    assert!(
        !resource_monitors().iter().any(|(m, _)| *m == id),
        "the tree no longer has it"
    );
    assert!(
        !deliver_resource_down_id(id, pid),
        "delivery after destruction must be refused"
    );
    assert!(
        observations_for(marker).is_empty(),
        "the destructor path runs no `down`"
    );
    assert_eq!(dtors_for(marker), 1, "the object was destroyed exactly once");
}

// ---------------------------------------------------------------------------------------------
// 6. `resource_monitors()` is a deterministic snapshot
// ---------------------------------------------------------------------------------------------

/// The snapshot is sorted by the monitor bytes (a `HashMap` order would not be), lists exactly the
/// armed monitors with their recorded pid words, and two calls agree.
#[test]
fn the_monitor_snapshot_is_sorted_and_lists_the_arms() {
    let mut host = ArmingHost::new();
    let (ty, first, first_marker) = type_and_object(&mut host, "down_snapshot_rt", recording_down);
    // SAFETY: `ty` is the type just opened; this is a second object of it.
    let (second, second_marker) = unsafe { alloc_tagged(ty) };

    let pid_a = make_internal_pid(make_pid_data(3, 11));
    let pid_b = make_internal_pid(make_pid_data(3, 12));
    let (code, holder_a) = host.arm(first, pid_a, 0x5eed_0060);
    assert_eq!(code, 0);
    let (code, holder_b) = host.arm(second, pid_b, 0x5eed_0061);
    assert_eq!(code, 0);
    let id_a = id_of(&holder_a);
    let id_b = id_of(&holder_b);
    assert_ne!(id_a, id_b, "two arms have two identities");

    let first_snapshot = resource_monitors();
    let second_snapshot = resource_monitors();
    assert_eq!(
        first_snapshot, second_snapshot,
        "two snapshots must agree element for element"
    );
    assert!(
        first_snapshot.windows(2).all(|w| w[0].0 <= w[1].0),
        "the snapshot is sorted by the monitor bytes"
    );
    let recorded = |id: MonitorId| {
        first_snapshot
            .iter()
            .find(|(m, _)| *m == id)
            .map(|(_, pid)| pid.raw())
    };
    assert_eq!(
        recorded(id_a),
        Some(n(pid_a).raw()),
        "the snapshot reports the recorded pid word"
    );
    assert_eq!(recorded(id_b), Some(n(pid_b).raw()));
    assert_ne!(first_marker, second_marker, "the markers are distinct");

    // SAFETY: each object still holds its allocator reference.
    unsafe {
        enif_release_resource(first);
        enif_release_resource(second);
    }
    assert!(
        !resource_monitors()
            .iter()
            .any(|(m, _)| *m == id_a || *m == id_b),
        "both arms are gone with their objects"
    );
}

// ---------------------------------------------------------------------------------------------
// 7. Re-entrancy: the callback may call back into the crate
// ---------------------------------------------------------------------------------------------

static REENTRY_RAN: AtomicUsize = AtomicUsize::new(0);
static REENTRY_MATCHED: AtomicUsize = AtomicUsize::new(0);
static REENTRY_GOT: AtomicUsize = AtomicUsize::new(0);
static REENTRY_RELEASED: AtomicUsize = AtomicUsize::new(0);
static REENTRY_TERM: AtomicUsize = AtomicUsize::new(0);
static REENTRY_TY: AtomicUsize = AtomicUsize::new(0);
static REENTRY_EXTRA: AtomicUsize = AtomicUsize::new(0);

/// A `down` that re-enters the crate exactly the way a real NIF would: it resolves a resource term
/// with `enif_get_resource` (which takes the registry lock), releases the monitored object's allocator
/// reference with `enif_release_resource`, and drops the **last** reference of a second object — whose
/// destructor takes the registry lock again. None of that may deadlock, which is only true because the
/// delivery releases the lock before it calls.
///
/// # Safety
///
/// The C `ErlNifResourceDown` ABI; `obj` is this file's marked object.
unsafe extern "C" fn reentrant_down(
    env: *mut crate::nif::types::ErlNifEnv,
    obj: *mut c_void,
    _pid: *mut ErlNifPid,
    _monitor: *mut ErlNifMonitor,
) {
    use std::sync::atomic::Ordering::SeqCst;

    REENTRY_RAN.fetch_add(1, SeqCst);
    let ty = REENTRY_TY.load(SeqCst) as *mut ErlNifResourceType;
    let term = ERL_NIF_TERM::from_raw(REENTRY_TERM.load(SeqCst));
    let mut got: *mut c_void = core::ptr::null_mut();
    // SAFETY: `env` is the env the delivery built; `term` is a resource term minted on the same host
    // heap; `ty` is a live type; `got` is a live local.
    let matched = unsafe { enif_get_resource(env, term, ty, &mut got) };
    REENTRY_MATCHED.store(matched as usize, SeqCst);
    REENTRY_GOT.store(got as usize, SeqCst);

    // SAFETY: the monitored object holds the allocator reference plus the delivery's protective one,
    // so this release cannot reach zero — which is exactly what the C's `brefc` buys.
    unsafe { enif_release_resource(obj) };
    REENTRY_RELEASED.fetch_add(1, SeqCst);

    let extra = REENTRY_EXTRA.swap(0, SeqCst) as *mut c_void;
    if !extra.is_null() {
        // SAFETY: this is `extra`'s last reference, so its destructor runs right here — inside the
        // callback, with no crate lock held. That is the deadlock probe for the `dtor` path.
        unsafe { enif_release_resource(extra) };
    }
}

/// A `down` callback is re-entrant: it may call `enif_get_resource` and `enif_release_resource`, and a
/// destructor may even run inside it, all without deadlocking. With the protective reference in place
/// a re-entrant release can never be the *last* one (that is the C's `brefc`, `erl_nif.c:3092`), so the
/// monitored object's own destructor runs on the outer release — which is asserted to be safe.
#[test]
fn a_down_callback_may_re_enter_the_crate_without_deadlocking() {
    use std::sync::atomic::Ordering::SeqCst;

    let mut host = ArmingHost::new();
    let (ty, obj, marker) = type_and_object(&mut host, "down_reentrant_rt", reentrant_down);
    // A second object of the same type whose last reference the callback will drop.
    // SAFETY: `ty` is the type just opened.
    let (extra, extra_marker) = unsafe { alloc_tagged(ty) };
    let pid = pid_term();

    // Mint the resource term the callback resolves, before the delivery: the term lives on the host
    // heap, which outlives the delivery's env.
    let resource_term = {
        let mut env = NifEnv::new(&mut host);
        // SAFETY: `env` is live; `obj` is a live payload pointer.
        t(unsafe { enif_make_resource(env.as_ptr(), obj) })
    };
    REENTRY_TERM.store(resource_term.raw(), SeqCst);
    REENTRY_TY.store(ty as usize, SeqCst);
    REENTRY_EXTRA.store(extra as usize, SeqCst);

    let (code, holder) = host.arm(obj, pid, 0x5eed_0070);
    assert_eq!(code, 0);
    assert_eq!(dtors_for(extra_marker), 0, "the extra object is alive");

    assert!(
        deliver_resource_down_id(id_of(&holder), pid),
        "the delivery must run the callback"
    );

    assert_eq!(REENTRY_RAN.load(SeqCst), 1, "the callback ran once");
    assert_eq!(REENTRY_MATCHED.load(SeqCst), 1, "enif_get_resource resolved");
    assert_eq!(
        REENTRY_GOT.load(SeqCst),
        obj as usize,
        "and it resolved to the monitored payload"
    );
    assert_eq!(REENTRY_RELEASED.load(SeqCst), 1, "the re-entrant release ran");
    assert_eq!(
        dtors_for(extra_marker),
        1,
        "the destructor of the second object ran inside the callback"
    );
    assert_eq!(
        dtors_for(marker),
        0,
        "the monitored object survived: the protective reference held it"
    );

    // The object is still alive through the resource term's reference, and the outer protective
    // release was safe; pruning the dead term is what finally destroys it.
    assert!(
        release_resource_terms(|_| false) >= 1,
        "the dead resource term was pruned"
    );
    assert_eq!(dtors_for(marker), 1, "the destructor ran once, at the prune");
    assert!(
        !deliver_resource_down_id(id_of(&holder), pid),
        "the monitor died with the object"
    );
    assert_eq!(
        REENTRY_RAN.load(SeqCst),
        1,
        "and the callback ran exactly once"
    );
}

// ---------------------------------------------------------------------------------------------
// 8. An object destroyed through the term seam takes its monitors with it
// ---------------------------------------------------------------------------------------------

/// The same `dtor_demonitor` rule as test 5, reached through the **term** release seam: an object kept
/// alive only by a resource term is destroyed when the caller declares that term dead
/// (`release_resource_terms`), and the monitor armed on it never fires.
#[test]
fn an_object_destroyed_through_the_term_seam_cancels_its_monitors() {
    let mut host = ArmingHost::new();
    let (_ty, obj, marker) = type_and_object(&mut host, "down_term_cancel_rt", recording_down);
    let pid = pid_term();
    let resource_term = {
        let mut env = NifEnv::new(&mut host);
        // SAFETY: `env` is live; `obj` is a live payload pointer.
        t(unsafe { enif_make_resource(env.as_ptr(), obj) })
    };
    let (code, holder) = host.arm(obj, pid, 0x5eed_0080);
    assert_eq!(code, 0);
    let id = id_of(&holder);

    // Drop the allocator's reference: the term's reference keeps the object alive.
    // SAFETY: `obj` still has the term's reference outstanding.
    unsafe { enif_release_resource(obj) };
    assert!(
        resource_monitors().iter().any(|(m, _)| *m == id),
        "the monitor is still armed while the term keeps the object alive"
    );

    // Declare the term dead: the object is destroyed, the monitor cancelled, no `down`.
    assert!(release_resource_terms(|_| false) >= 1);
    assert_eq!(dtors_for(marker), 1, "the object was destroyed");
    assert!(
        !resource_monitors().iter().any(|(m, _)| *m == id),
        "its monitor went with it"
    );
    assert!(!deliver_resource_down_id(id, pid), "and it cannot deliver");
    assert!(
        observations_for(marker).is_empty(),
        "no `down` runs on the destructor path"
    );
    assert!(!is_small(resource_term));
}

// ---------------------------------------------------------------------------------------------
// The oracle half — the same C fixture under the system Erlang
// ---------------------------------------------------------------------------------------------

/// The measured oracle transcript (OTP 29.1.1 / erts-17.1), produced by the fixture's own entry points
/// under the system `erl`: arm on a victim, kill it, and read `state/0` = `{Count, Pid, Mon}`.
///
/// Read it as: the `down` callback observes the **recorded** target pid (`same`), the **stored** 24
/// monitor bytes (`1`) and the armed object (`obj/0` = `1`), exactly once (`Count` = `1`); the fire
/// removes the tree entry, so the post-fire `demonitor/0` answers `1`; a demonitor **while armed**
/// answers `0`, and the killed target then fires nothing; and a resource released to zero runs no
/// `down` at all (`state_after_release_kill` = `{0,none,-1}`, `obj/0` = `-1`).
const ORACLE: [&str; 17] = [
    "{arm_first,0}",
    "{state_armed,{0,none,-1}}",
    "{state_after_fire,{1,same,1}}",
    "{obj_after_fire,1}",
    "{demonitor_after_fire,1}",
    "{arm_second,0}",
    "{state_second_armed,{0,none,-1}}",
    "{demonitor_armed,0}",
    "{state_after_demonitor,{0,none,-1}}",
    "{state_after_demonitor_kill,{0,none,-1}}",
    "{demonitor_after_demonitor,1}",
    "{arm_third,0}",
    "{state_third_armed,{0,none,-1}}",
    "{release_third,ok}",
    "{state_after_release_kill,{0,none,-1}}",
    "{obj_after_release_kill,-1}",
    "{demonitor_after_release,1}",
];

/// The fixture (a real C NIF whose resource type carries a `down` callback).
const FIXTURE_C: &str = include_str!("fixtures/nif_resource_down.c");
/// The NIF's module name — the `ERL_NIF_INIT` argument, which the loader cross-checks against the
/// calling module (`erl_nif.c:4841`).
const NIF_MODULE: &str = "nif_resource_down";

/// The Erlang stub module: one exported stub per NIF entry (`erlang:nif_error/1` until the library
/// loads), loaded from `-on_load`.
fn nif_stub_module(so_without_suffix: &str) -> String {
    let mut erl = format!("-module({NIF_MODULE}).\n");
    erl.push_str("-export([arm/1, state/0, obj/0, demonitor/0, release/0]).\n");
    erl.push_str("-on_load(init/0).\n\n");
    erl.push_str(&format!(
        "init() ->\n    erlang:load_nif(\"{so_without_suffix}\", 0).\n\n"
    ));
    erl.push_str("arm(_) -> erlang:nif_error(nif_not_loaded).\n");
    erl.push_str("state() -> erlang:nif_error(nif_not_loaded).\n");
    erl.push_str("obj() -> erlang:nif_error(nif_not_loaded).\n");
    erl.push_str("demonitor() -> erlang:nif_error(nif_not_loaded).\n");
    erl.push_str("release() -> erlang:nif_error(nif_not_loaded).\n");
    erl
}

/// The driver module: three arms, three kills, and every `demonitor/0` around them — the transcript
/// [`ORACLE`] pins.
fn oracle_driver_module() -> String {
    let mut erl = String::from("-module(nif_resource_down_report).\n-export([main/0]).\n\n");
    erl.push_str("victim() -> spawn(fun() -> receive stop -> ok end end).\n");
    erl.push_str("settle() -> timer:sleep(400).\n");
    erl.push_str("p(T) -> io:format(\"~w~n\", [T]).\n\n");
    erl.push_str("main() ->\n    P1 = victim(),\n");
    for (before, tag, call) in [
        (None, "arm_first", "arm(P1)"),
        (None, "state_armed", "state()"),
        (Some("exit(P1, kill)"), "state_after_fire", "state()"),
        (None, "obj_after_fire", "obj()"),
        (None, "demonitor_after_fire", "demonitor()"),
    ] {
        if let Some(action) = before {
            erl.push_str(&format!("    {action},\n    settle(),\n"));
        }
        erl.push_str(&format!("    p({{{tag}, {NIF_MODULE}:{call}}}),\n"));
    }
    erl.push_str("    P2 = victim(),\n");
    for (before, tag, call) in [
        (None, "arm_second", "arm(P2)"),
        (None, "state_second_armed", "state()"),
        (None, "demonitor_armed", "demonitor()"),
        (None, "state_after_demonitor", "state()"),
        (Some("exit(P2, kill)"), "state_after_demonitor_kill", "state()"),
        (None, "demonitor_after_demonitor", "demonitor()"),
    ] {
        if let Some(action) = before {
            erl.push_str(&format!("    {action},\n    settle(),\n"));
        }
        erl.push_str(&format!("    p({{{tag}, {NIF_MODULE}:{call}}}),\n"));
    }
    erl.push_str("    P3 = victim(),\n");
    for (before, tag, call) in [
        (None, "arm_third", "arm(P3)"),
        (None, "state_third_armed", "state()"),
        (None, "release_third", "release()"),
        (Some("exit(P3, kill)"), "state_after_release_kill", "state()"),
        (None, "obj_after_release_kill", "obj()"),
        (None, "demonitor_after_release", "demonitor()"),
    ] {
        if let Some(action) = before {
            erl.push_str(&format!("    {action},\n    settle(),\n"));
        }
        erl.push_str(&format!("    p({{{tag}, {NIF_MODULE}:{call}}}),\n"));
    }
    erl.push_str("    halt(0).\n");
    erl
}

/// Print a loud skip banner (never a silent pass).
fn skip(reason: &str) {
    println!("================================================================");
    println!("SKIP: {reason}");
    println!("================================================================");
}

/// Is `cmd` runnable? (a cheap probe; `erlc -v` answers the version and exits `0`, while `erlc -h`
/// exits `1`, so `-h` must not be used as the probe)
fn runnable(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// The C compiler to use: `$CC` when set, else `cc`.
fn cc() -> String {
    std::env::var("CC").unwrap_or_else(|_| "cc".to_string())
}

/// True when `dir` holds a readable `erl_nif.h`.
fn has_header(dir: &Path) -> bool {
    dir.join("erl_nif.h").is_file()
}

/// Locate the system OTP include directory, the same three-step resolution as
/// `tests/common/mod.rs` (that file is a read-only precedent — a `src/` child cannot import it):
/// `code:lib_dir(erts,include)` (accepted only if `erl_nif.h` is really there — the Nix-packaged
/// runtime answers a path that does not exist), then the `erts-*/include` sibling of the `erl` on
/// `PATH`, then `$ERL_TOP` and the standard installation roots.
fn erl_include_dir() -> Option<PathBuf> {
    fn from_erl_eval() -> Option<PathBuf> {
        let output = Command::new("erl")
            .args([
                "-noshell",
                "-eval",
                "io:format(\"~s\",[code:lib_dir(erts,include)])",
                "-s",
                "init",
                "stop",
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let dir = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
        has_header(&dir).then_some(dir)
    }

    fn from_path_erl() -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        let erl = std::env::split_paths(&path)
            .map(|dir| dir.join("erl"))
            .find(|candidate| candidate.is_file())?
            .canonicalize()
            .ok()?;
        let bin = erl.parent()?;
        let erts = bin.parent()?;
        if bin.file_name()? != "bin" || !erts.file_name()?.to_string_lossy().starts_with("erts-") {
            return None;
        }
        let include = erts.join("include");
        has_header(&include).then_some(include)
    }

    fn from_roots() -> Option<PathBuf> {
        let mut roots: Vec<PathBuf> = Vec::new();
        if let Some(top) = std::env::var_os("ERL_TOP") {
            roots.push(PathBuf::from(top));
        }
        for base in [
            "/usr/lib/erlang",
            "/usr/local/lib/erlang",
            "/usr/lib64/erlang",
            "/opt/erlang",
        ] {
            roots.push(PathBuf::from(base));
        }
        for root in roots {
            let direct = root.join("include");
            if has_header(&direct) {
                return Some(direct);
            }
            for base in [root.clone(), root.join("lib").join("erlang")] {
                let Ok(entries) = std::fs::read_dir(&base) else {
                    continue;
                };
                for entry in entries.flatten() {
                    if !entry.file_name().to_string_lossy().starts_with("erts-") {
                        continue;
                    }
                    let include = entry.path().join("include");
                    if has_header(&include) {
                        return Some(include);
                    }
                }
            }
        }
        None
    }

    from_erl_eval().or_else(from_path_erl).or_else(from_roots)
}

/// A fresh scratch directory for one compilation (unique per stem + process + call).
fn scratch_dir(stem: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
    std::env::temp_dir().join(format!("beam_abi-mon-{stem}-{}-{n}", std::process::id()))
}

/// Compile the fixture into a shared object in a scratch dir, or `None` after a loud skip.
fn compile_fixture() -> Option<PathBuf> {
    let Some(include) = erl_include_dir() else {
        skip("nif_resource_down: no OTP include directory holding erl_nif.h");
        return None;
    };
    if !runnable(&cc(), &["--version"]) {
        skip(&format!(
            "nif_resource_down: no C compiler ({} / $CC) available",
            cc()
        ));
        return None;
    }
    let dir = scratch_dir(NIF_MODULE);
    let src = dir.join(format!("{NIF_MODULE}.c"));
    let so = dir.join(format!("{NIF_MODULE}.so"));
    let write = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&src, FIXTURE_C));
    if let Err(err) = write {
        skip(&format!("nif_resource_down: cannot prepare {dir:?}: {err}"));
        return None;
    }
    let output = Command::new(cc())
        .arg("-shared")
        .arg("-fPIC")
        .arg(format!("-I{}", include.display()))
        .arg("-o")
        .arg(&so)
        .arg(&src)
        .output();
    let Ok(output) = output else {
        skip("nif_resource_down: could not run the C compiler");
        return None;
    };
    if !output.status.success() {
        skip(&format!(
            "nif_resource_down: cc failed (exit {:?}):\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ));
        return None;
    }
    Some(so)
}

/// Run the fixture under the **system** Erlang and collect its `~w` transcript, or `None` after a loud
/// skip.
fn oracle_transcript(so: &Path) -> Option<Vec<String>> {
    if !runnable("erl", &["-noshell", "-eval", "halt(0)."]) || !runnable("erlc", &["-v"]) {
        skip("the erl/erlc oracle toolchain is not available");
        return None;
    }
    let dir = so.parent()?.to_path_buf();
    // `erlang:load_nif/2` appends the shared-object suffix itself, so hand it the path WITHOUT `.so`.
    let stem = so.file_stem()?.to_str()?;
    let nif_path = dir.join(stem).to_str()?.to_string();

    let stub = dir.join(format!("{NIF_MODULE}.erl"));
    let driver = dir.join("nif_resource_down_report.erl");
    std::fs::write(&stub, nif_stub_module(&nif_path)).expect("the oracle module is writable");
    std::fs::write(&driver, oracle_driver_module()).expect("the driver is writable");

    for source in [&stub, &driver] {
        let compiled = Command::new("erlc")
            .arg("-o")
            .arg(&dir)
            .arg(source)
            .output()
            .expect("erlc is runnable (probed above)");
        assert!(
            compiled.status.success(),
            "erlc failed for {source:?}:\n{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
    }

    let out = Command::new("erl")
        .arg("-noshell")
        .arg("-pa")
        .arg(&dir)
        .arg("-s")
        .arg("nif_resource_down_report")
        .arg("main")
        .output()
        .expect("erl is runnable (probed above)");
    assert!(
        out.status.success(),
        "the oracle run failed (exit {:?}):\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    println!("oracle: {}", lines.join(" | "));
    Some(lines)
}

/// The oracle recomputes the recorded transcript when the toolchain is present; the Rust half is the
/// in-process `down` callback asserted above — no `.so` is loaded into this binary, because the lib
/// test target does not export the `enif_*` symbols (`build.rs`'s `--export-dynamic` covers `tests/`
/// only), so `dlopen(RTLD_NOW)` would fail on the first unresolved import.
#[test]
fn the_same_fixture_agrees_with_the_measured_oracle_transcript() {
    let Some(so) = compile_fixture() else { return };
    let Some(lines) = oracle_transcript(&so) else {
        return;
    };
    assert_eq!(
        lines,
        ORACLE.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        "the measured `down` transcript (fire once with the recorded pid and stored bytes, \
         the demonitor codes, and no `down` for a dying resource)"
    );
}
