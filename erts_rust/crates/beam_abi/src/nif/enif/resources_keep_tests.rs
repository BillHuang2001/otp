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

//! Tests for the explicit **keep/unkeep seam** (`super::keep`) — the sibling child of `resources.rs`
//! (path-included next to `resources_tests.rs` and `resources_monitors_tests.rs`).
//!
//! These pin *this crate's* seam, not a C ABI behaviour, so they need no fixture `.so` and no oracle:
//! the C's `enif_keep_resource`/`enif_release_resource` semantics they mirror are cited in
//! `resources_keep.rs`'s module docs (`erts/emulator/beam/erl_nif.c:3164-3185`,
//! `erts/emulator/sys/common/erl_check_io.c:1446`/`:1509`/`:1533`).
//!
//! # Test isolation (the registry is process-wide, the tests run in parallel)
//!
//! Two rules keep these tests independent:
//!
//! * each test opens its own resource **type** under a unique name (types are keyed by name and never
//!   freed), and
//! * each test's payload is a **different size**, so no two tests can ever be handed the same freed
//!   `malloc` chunk. That matters for the assertions on a *stale* pointer (an unkeep after the object
//!   was destroyed): the liveness set is keyed by payload address, so the only way a stale pointer
//!   could name a live object is an address reuse — the residual `resources_keep.rs` documents — and a
//!   distinct size class per test rules it out.

use super::*;

use std::ffi::CString;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

// ---------------------------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------------------------

/// A minimal [`NifHost`] for the keep/unkeep tests: a bump heap plus an atom table.
struct KeepHost {
    heap: beam_alloc::Heap,
    atoms: beam_term::atom::AtomTable,
}

impl KeepHost {
    /// A host with a 4096-word bump heap and an empty atom table.
    fn new() -> KeepHost {
        KeepHost {
            heap: beam_alloc::Heap::new(4096).expect("a small test heap is allocatable"),
            atoms: beam_term::atom::AtomTable::new(),
        }
    }
}

impl NifHost for KeepHost {
    fn heap(&mut self) -> &mut beam_alloc::Heap {
        &mut self.heap
    }

    fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
        Some(prelude::make_atom(self.atoms.intern(name)))
    }

    fn atom_name(&mut self, atom: Eterm) -> Option<String> {
        if !prelude::is_atom(atom) {
            return None;
        }
        self.atoms.name(prelude::atom_val(atom)).map(str::to_string)
    }
}

/// Open a resource type called `name` with `dtor` — each test uses its own name (types are keyed by
/// name and never freed, and the registry is process-wide).
fn open_type(
    env: &NifEnv,
    name: &str,
    dtor: Option<ErlNifResourceDtor>,
) -> *mut ErlNifResourceType {
    let c_name = CString::new(name).expect("no interior NUL");
    let mut tried = ErlNifResourceFlags::from_bits(0);
    // SAFETY: `env` is live; `c_name` is NUL-terminated; `tried` is a live local.
    let ty = unsafe {
        enif_open_resource_type(
            env.as_ptr(),
            core::ptr::null(),
            c_name.as_ptr(),
            dtor,
            ErlNifResourceFlags::CREATE,
            &mut tried,
        )
    };
    assert!(!ty.is_null(), "the type must open");
    ty
}

/// `enif_alloc_resource(ty, size)` as a live payload pointer (one reference, the allocator's).
///
/// # Safety
///
/// `ty` must be a type from [`open_type`], and the caller must account for the returned pointer's one
/// reference (release it, or keep it, or let a minted term's reference outlive it).
unsafe fn alloc_object(ty: *mut ErlNifResourceType, size: usize) -> *mut c_void {
    // SAFETY: forwarded from the caller's contract.
    let obj = unsafe { enif_alloc_resource(ty, size) };
    assert!(!obj.is_null(), "the allocation must succeed");
    obj
}

/// Write `value` into the first word of `obj`'s payload — the value the destructor cross-checks.
///
/// # Safety
///
/// `obj` must be a live payload pointer of at least `size_of::<usize>()` bytes.
unsafe fn write_payload(obj: *mut c_void, value: usize) {
    // SAFETY: the caller guarantees the payload is at least one word, and `usize` has no alignment
    // requirement stricter than the block's 16-byte payload alignment.
    unsafe { core::ptr::write(obj as *mut usize, value) };
}

/// Whether `obj` is currently a **live** payload pointer this crate allocated — the registry's own
/// membership answer (`Registry::object_type`), i.e. exactly the gate `keep_resource`/`unkeep_resource`
/// consult. Never dereferences `obj`.
fn is_registered(obj: *mut c_void) -> bool {
    with_registry(|r| r.object_type(obj)).is_some()
}

/// `enif_make_resource(env, obj)` as a term (the mint takes its own reference).
///
/// # Safety
///
/// `env` must be live and `obj` a live payload pointer (see [`alloc_object`]).
unsafe fn make_resource_term(env: &NifEnv, obj: *mut c_void) -> Eterm {
    // SAFETY: forwarded from the caller's contract.
    t(unsafe { enif_make_resource(env.as_ptr(), obj) })
}

/// `enif_get_resource(env, term, ty, &mut objp)` as the C's `1`/`0` return code plus `*objp`.
///
/// # Safety
///
/// `env` must be live and `ty` a type from [`open_type`] (or null).
unsafe fn get_resource(
    env: &NifEnv,
    term: Eterm,
    ty: *mut ErlNifResourceType,
) -> (i32, *mut c_void) {
    let mut objp: *mut c_void = core::ptr::null_mut();
    // SAFETY: `env` is live; `term` is a term; `ty` comes from `open_type`; `objp` is a live local.
    let matched = unsafe { enif_get_resource(env.as_ptr(), n(term), ty, &mut objp) };
    (matched, objp)
}

// ---------------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------------

/// A keep outlives the allocator's own reference, and the unkeep that follows runs the destructor
/// **exactly once** with the object's payload — the shape the select route relies on (an armed
/// interest keeps the object; the stop/cancel releases it).
#[test]
fn a_keep_holds_the_object_past_the_owners_release() {
    static RUNS: AtomicUsize = AtomicUsize::new(0);
    static PAYLOAD: AtomicUsize = AtomicUsize::new(0);
    /// The type's destructor: counts its runs and records the payload it was handed, so the count
    /// *and* the operand are pinned.
    unsafe extern "C" fn counting_dtor(_: *mut ErlNifEnv, obj: *mut c_void) {
        RUNS.fetch_add(1, AtomicOrdering::SeqCst);
        // SAFETY: `obj` is the live payload of the object whose count just reached zero; the test wrote
        // one word into it before releasing.
        let word = unsafe { core::ptr::read(obj as *const usize) };
        PAYLOAD.store(word, AtomicOrdering::SeqCst);
    }

    let mut host = KeepHost::new();
    let env = NifEnv::new(&mut host);
    let ty = open_type(&env, "keep_holds", Some(counting_dtor));
    // SAFETY: `ty` just opened; the payload is one word (this test's own size class).
    let obj = unsafe { alloc_object(ty, 8) };
    // SAFETY: the payload is eight bytes, as allocated.
    unsafe { write_payload(obj, 42) };

    assert!(keep_resource(obj), "the keep takes a reference");
    // The allocator's own reference goes; only the keep's remains.
    // SAFETY: `obj` is a live payload with an outstanding reference (the allocator's).
    unsafe { enif_release_resource(obj) };
    assert_eq!(
        RUNS.load(AtomicOrdering::SeqCst),
        0,
        "the destructor must not run while a keep stands"
    );
    assert!(
        is_registered(obj),
        "the object stays registered as a live payload while the keep stands"
    );

    assert!(unkeep_resource(obj), "the unkeep drops the last reference");
    assert_eq!(
        RUNS.load(AtomicOrdering::SeqCst),
        1,
        "the destructor runs exactly once"
    );
    assert_eq!(
        PAYLOAD.load(AtomicOrdering::SeqCst),
        42,
        "the destructor is handed the object's payload"
    );
    assert!(
        !is_registered(obj),
        "a destroyed object is unregistered, so no stale pointer resolves"
    );
}

/// Both helpers refuse a null handle instead of dereferencing it (the C would crash).
#[test]
fn a_null_handle_is_refused() {
    assert!(
        !keep_resource(core::ptr::null_mut()),
        "a keep of null takes no reference"
    );
    assert!(
        !unkeep_resource(core::ptr::null_mut()),
        "an unkeep of null drops no reference"
    );
}

/// A pointer this crate never allocated is refused without being dereferenced — including a dangling
/// address (which a dereference would fault on) and a readable stack address that is simply not one of
/// ours (the gate is the registry's live-object set, not "is the address readable").
#[test]
fn an_unknown_pointer_is_refused_without_dereferencing_it() {
    let bogus = core::ptr::dangling_mut::<c_void>();
    assert!(!keep_resource(bogus), "a dangling address is not one of ours");
    assert!(!unkeep_resource(bogus), "and it holds no reference to drop");

    let mut local: u64 = 0xdead_beef;
    let stack = &mut local as *mut u64 as *mut c_void;
    assert!(!keep_resource(stack), "a stack address is not a payload");
    assert!(!unkeep_resource(stack), "and it holds no reference to drop");
    assert_eq!(local, 0xdead_beef, "the refusal never touched the memory");

    // A pointer *inside* a real block is still not the payload pointer the registry knows, so it is
    // refused too — the object keeps its single reference and is released normally afterwards.
    let mut host = KeepHost::new();
    let env = NifEnv::new(&mut host);
    let ty = open_type(&env, "keep_unknown", None);
    // SAFETY: `ty` just opened; 64 bytes is this test's own size class.
    let obj = unsafe { alloc_object(ty, 64) };
    // SAFETY: `obj` points at a 64-byte payload, so eight bytes in is still inside the block.
    let interior = unsafe { (obj as *mut u8).add(8) as *mut c_void };
    assert!(
        !keep_resource(interior),
        "an interior pointer is not the payload the registry registered"
    );
    assert!(!unkeep_resource(interior), "and it holds no reference to drop");
    assert!(is_registered(obj), "the real object is untouched");
    assert!(
        keep_resource(obj),
        "the payload pointer itself is still recognised"
    );
    // SAFETY: `obj` holds the allocator's reference plus the one just kept.
    unsafe { enif_release_resource(obj) };
    assert!(unkeep_resource(obj), "the kept reference is dropped");
    assert!(!is_registered(obj), "and the object is destroyed");
}

/// A **dying** object (count already zero, destructor already due) is not resurrected: the keep
/// answers `false` and the count stays at zero, and the unkeep refuses to decrement it further.
///
/// The window is driven deterministically through the crate-private header access instead of with a
/// release/destructor race: after the decrement the object is still registered with a count of zero,
/// which is exactly the state `erts/emulator/sys/common/erl_check_io.c:1261` asserts against with
/// `ASSERT(!erts_dbg_is_resource_dying(resource))` before it takes its per-interest reference.
#[test]
fn a_dying_object_is_not_resurrected() {
    let mut host = KeepHost::new();
    let env = NifEnv::new(&mut host);
    let ty = open_type(&env, "keep_dying", None);
    // SAFETY: `ty` just opened; 200 bytes is this test's own size class.
    let obj = unsafe { alloc_object(ty, 200) };

    // Drive the block's count to zero *without* running the destructor: the C's dying window.
    // SAFETY: `obj` is a live payload pointer, so its header is the 32 bytes before it in the same
    // block; the count is one (the allocator's) and this test is its only owner.
    let header = unsafe { header_ptr(obj) };
    // SAFETY: `header` points at that live block.
    let previous = unsafe { (*header).refcount.fetch_sub(1, Ordering::AcqRel) };
    assert_eq!(previous, 1, "the allocation starts at one reference");
    assert!(
        is_registered(obj),
        "no destroy ran, so the block is still registered"
    );

    assert!(
        !keep_resource(obj),
        "a dying object must not be resurrected by a keep"
    );
    // SAFETY: `header` is still a live block (nothing destroyed it).
    assert_eq!(
        unsafe { (*header).refcount.load(Ordering::SeqCst) },
        0,
        "the refused keep left the count at zero"
    );
    assert!(
        !unkeep_resource(obj),
        "an unkeep of a zero count drops nothing"
    );
    // SAFETY: as above.
    assert_eq!(
        unsafe { (*header).refcount.load(Ordering::SeqCst) },
        0,
        "the refused unkeep did not underflow the count"
    );

    // Give the block its reference back and release it, so the object is destroyed and nothing leaks.
    // SAFETY: `obj` is a live payload pointer whose block is still allocated.
    unsafe { retain_object(obj) };
    // SAFETY: `obj` now holds exactly one reference again — the one just restored.
    unsafe { enif_release_resource(obj) };
    assert!(!is_registered(obj), "the block was destroyed and unregistered");
}

/// An unkeep **after** the object was destroyed answers `false` and runs no second destructor — the
/// registry unregisters the payload before the block is freed, so the stale handle cannot name it.
#[test]
fn an_unkeep_after_destruction_answers_false_and_runs_no_second_destructor() {
    static RUNS: AtomicUsize = AtomicUsize::new(0);
    /// The type's destructor: only the run count matters here.
    unsafe extern "C" fn counting_dtor(_: *mut ErlNifEnv, _: *mut c_void) {
        RUNS.fetch_add(1, AtomicOrdering::SeqCst);
    }

    let mut host = KeepHost::new();
    let env = NifEnv::new(&mut host);
    let ty = open_type(&env, "keep_after_destroy", Some(counting_dtor));
    // SAFETY: `ty` just opened; 512 bytes is this test's own size class, so no other test can be handed
    // this freed chunk.
    let obj = unsafe { alloc_object(ty, 512) };

    assert!(
        unkeep_resource(obj),
        "the allocator's only reference is dropped"
    );
    assert_eq!(
        RUNS.load(AtomicOrdering::SeqCst),
        1,
        "that release was the last one, so the destructor ran"
    );
    assert!(!is_registered(obj), "the block is gone from the registry");

    assert!(
        !unkeep_resource(obj),
        "an unkeep of a destroyed object drops nothing"
    );
    assert!(
        !keep_resource(obj),
        "and a keep cannot resurrect a destroyed object"
    );
    assert_eq!(
        RUNS.load(AtomicOrdering::SeqCst),
        1,
        "no second destructor ran"
    );
}

/// A keep/unkeep round trip leaves the object resolvable through the resource **term** that also holds
/// it: releasing the allocator's reference and then the keep's leaves the term's reference standing, so
/// `enif_get_resource` keeps answering `1` with the same payload and no destructor has run.
#[test]
fn a_keep_and_unkeep_round_trip_leaves_term_resolution_intact() {
    static RUNS: AtomicUsize = AtomicUsize::new(0);
    /// The type's destructor: only the run count matters here.
    unsafe extern "C" fn counting_dtor(_: *mut ErlNifEnv, _: *mut c_void) {
        RUNS.fetch_add(1, AtomicOrdering::SeqCst);
    }

    let mut host = KeepHost::new();
    let env = NifEnv::new(&mut host);
    let ty = open_type(&env, "keep_roundtrip", Some(counting_dtor));
    // SAFETY: `ty` just opened; 1000 bytes is this test's own size class.
    let obj = unsafe { alloc_object(ty, 1000) };
    // SAFETY: `obj` is a live payload pointer and `env` is live; the mint takes its own reference.
    let term = unsafe { make_resource_term(&env, obj) };

    assert!(keep_resource(obj), "the keep takes a reference");
    // Drop the allocator's reference: the term's and the keep's references remain.
    // SAFETY: `obj` is a live payload with an outstanding reference (the allocator's).
    unsafe { enif_release_resource(obj) };
    assert_eq!(
        RUNS.load(AtomicOrdering::SeqCst),
        0,
        "two references still stand"
    );
    // SAFETY: `env` is live; `term` is the term just minted; `ty` is the object's type.
    let (matched, objp) = unsafe { get_resource(&env, term, ty) };
    assert_eq!(matched, 1, "the term resolves before the unkeep");
    assert_eq!(objp, obj, "to the very payload it was minted for");

    assert!(unkeep_resource(obj), "the keep's reference is dropped");
    assert_eq!(
        RUNS.load(AtomicOrdering::SeqCst),
        0,
        "the term's own reference still holds the object"
    );
    // SAFETY: `env` is live; `term` is still minted; `ty` is the object's type.
    let (matched, objp) = unsafe { get_resource(&env, term, ty) };
    assert_eq!(
        matched, 1,
        "the term still resolves after the keep/unkeep round trip"
    );
    assert_eq!(objp, obj, "to the same payload");

    // Drop the term's reference deliberately (this caller owns it) so the test leaves nothing behind;
    // destroying the object forgets its term bindings, so no stale resolution survives.
    // SAFETY: `obj` is a live payload whose last reference is the term's.
    unsafe { enif_release_resource(obj) };
    assert_eq!(
        RUNS.load(AtomicOrdering::SeqCst),
        1,
        "the term's reference was the last one"
    );
    assert!(!is_registered(obj), "the object is destroyed");
}
