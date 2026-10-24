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

//! Tests for the `enif_*` resource family — the sibling child of `resources.rs`.
//!
//! Path-included from `resources.rs` (`#[path = "resources_tests.rs"]`) exactly like the
//! crate's other per-family test children (`port_tests.rs`, `process_tests.rs`,
//! `mapiter_tests.rs`), so the production file stays under the ~1000-line target.

use super::*;

/// A minimal [`NifHost`] for the retention test: a bump heap plus an atom table.
struct RetainHost {
    heap: beam_alloc::Heap,
    atoms: beam_term::atom::AtomTable,
}

impl NifHost for RetainHost {
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

impl RetainHost {
    /// A host with a 4096-word bump heap and an empty atom table.
    fn new() -> RetainHost {
        RetainHost {
            heap: beam_alloc::Heap::new(4096).expect("a small test heap is allocatable"),
            atoms: beam_term::atom::AtomTable::new(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers for the resource-term release tests (`release_resource_terms`)
// ---------------------------------------------------------------------------------------------

/// Open a resource type called `name` with `dtor`, on the registry the env reaches.
///
/// Each test uses its own name: types are keyed by name and never freed (the C's global
/// `resource_type_list`), and tests in one binary share this process-wide registry.
fn open_type(env: &NifEnv, name: &str, dtor: ErlNifResourceDtor) -> *mut ErlNifResourceType {
    use std::ffi::CString;

    let c_name = CString::new(name).expect("no interior NUL");
    let mut tried = ErlNifResourceFlags::from_bits(0);
    // SAFETY: `env` is live; `c_name` is NUL-terminated; `tried` is a live local.
    let ty = unsafe {
        enif_open_resource_type(
            env.as_ptr(),
            core::ptr::null(),
            c_name.as_ptr(),
            Some(dtor),
            ErlNifResourceFlags::CREATE,
            &mut tried,
        )
    };
    assert!(!ty.is_null(), "the type must open");
    ty
}

/// `enif_alloc_resource(ty, size)` as a live payload pointer.
///
/// # Safety
///
/// `ty` must be a type from [`open_type`], and the caller must release the returned pointer (its one
/// reference) or leave it to a minted term's reference.
unsafe fn alloc_object(ty: *mut ErlNifResourceType, size: libc::size_t) -> *mut c_void {
    // SAFETY: forwarded from the caller's contract.
    let obj = unsafe { enif_alloc_resource(ty, size) };
    assert!(!obj.is_null(), "the allocation must succeed");
    obj
}

/// `enif_make_resource(env, obj)` as a term (`enif_make_resource` returns a NIF term).
///
/// # Safety
///
/// `env` must be live and `obj` a live payload pointer (see [`alloc_object`]).
unsafe fn make_resource_term(env: &NifEnv, obj: *mut c_void) -> Eterm {
    // SAFETY: forwarded from the caller's contract.
    t(unsafe { enif_make_resource(env.as_ptr(), obj) })
}

/// `enif_get_resource(env, term, ty, &mut objp)` as the C's `1`/`0` return code.
///
/// # Safety
///
/// `env` must be live and `ty` a type from [`open_type`] (or null).
unsafe fn get_resource(env: &NifEnv, term: Eterm, ty: *mut ErlNifResourceType) -> (i32, *mut c_void) {
    let mut objp: *mut c_void = core::ptr::null_mut();
    // SAFETY: `env` is live; `term` is a term; `ty` comes from `open_type`; `objp` is a live local.
    let matched = unsafe { enif_get_resource(env.as_ptr(), n(term), ty, &mut objp) };
    (matched, objp)
}
/// A **copy** of an internal-reference block: the same four words at a different heap address —
/// exactly what `beam_message::copy_struct` (the send path, ETS) produces for one.
///
/// # Safety
///
/// `term` must be a live M1 internal-reference thing ([`prelude::is_internal_ref`]) whose block has
/// exactly [`prelude::INTERNAL_REF_WORDS`] readable words.
unsafe fn copy_reference(env: &NifEnv, term: Eterm) -> Eterm {
    assert!(
        unsafe { prelude::is_internal_ref(term) },
        "the term must be an internal reference"
    );
    // SAFETY: `alloc` hands out exactly `INTERNAL_REF_WORDS` exclusively-owned, writable words from
    // the host heap; every one of them is written below, before the pointer escapes.
    let copy = unsafe { alloc(env, prelude::INTERNAL_REF_WORDS) }
        .expect("the test heap can fit a four-word reference");
    // SAFETY: `term` is a validated M1 reference, so its block holds exactly `INTERNAL_REF_WORDS`
    // readable words; `copy` addresses exactly that many writable words, and the two blocks do not
    // overlap (different allocations).
    unsafe {
        core::ptr::copy_nonoverlapping(prelude::boxed_val(term), copy, prelude::INTERNAL_REF_WORDS);
    }
    prelude::make_boxed(copy as *const Eterm)
}

/// **The A.3 divergence this file fixes.** `alloc → enif_make_resource → enif_release_resource →
/// enif_get_resource` must still resolve to the live payload. The minted term takes its own
/// reference (exactly the C magic ref's reference), so releasing the allocator's reference must
/// **not** free the block. Oracle: `{1, 42}`; a refcount-zero-free runtime: `{0, -1}`.
#[test]
fn a_resource_term_keeps_the_object_alive_after_release() {
    use std::ffi::CString;

    let mut host = RetainHost {
        heap: beam_alloc::Heap::new(4096).expect("a small test heap is allocatable"),
        atoms: beam_term::atom::AtomTable::new(),
    };
    let env = NifEnv::new(&mut host);
    let name = CString::new("retain_rt").expect("no interior NUL");
    let mut tried = ErlNifResourceFlags::from_bits(0);
    // SAFETY: `env` is live; `name` is NUL-terminated; `tried` is a live local.
    let ty = unsafe {
        enif_open_resource_type(
            env.as_ptr(),
            core::ptr::null(),
            name.as_ptr(),
            None,
            ErlNifResourceFlags::CREATE,
            &mut tried,
        )
    };
    assert!(!ty.is_null(), "the type must open");

    // SAFETY: `ty` came from the open call above.
    let obj = unsafe { enif_alloc_resource(ty, 4) };
    assert!(!obj.is_null(), "the allocation must succeed");
    // SAFETY: `obj` is a live 4-byte payload; the first byte is in bounds.
    unsafe { *(obj as *mut u8) = 42 };

    // SAFETY: `env` is live; `obj` is a live payload pointer.
    let term = unsafe { enif_make_resource(env.as_ptr(), obj) };
    // Drop the single allocator reference; the term's own reference must keep the block alive.
    // SAFETY: `obj` still has an outstanding reference (the term's).
    unsafe { enif_release_resource(obj) };

    let mut got: *mut c_void = core::ptr::null_mut();
    // SAFETY: `env` is live; `term` is the resource term just minted; `got` is a live local.
    let matched = unsafe { enif_get_resource(env.as_ptr(), term, ty, &mut got) };
    assert_eq!(
        matched, 1,
        "a released-but-term-wrapped resource term must keep resolving"
    );
    assert_eq!(got, obj, "it must resolve to the same payload pointer");
    // SAFETY: `got == obj` is the live 4-byte payload written above.
    assert_eq!(
        unsafe { *(got as *const u8) },
        42,
        "and the payload is intact"
    );

    // Leave the process-wide registry as it was. A term left behind here would keep a binding whose
    // term block dies with this test's host arena — the very "reused address / dead mint" case the
    // registry keys and evicts by identity — so this test cleans up after itself. (The count is not
    // asserted: this registry is shared with every test in the binary.)
    let minted_word = term.raw();
    assert!(
        release_resource_terms(|candidate| candidate.raw() != minted_word) >= 1,
        "this test's own resource term is released"
    );
}
/// An object that is **never** exported as a term is still destroyed on its last release, and its
/// destructor runs (the documented synchronous timing).
#[test]
fn an_object_with_no_term_is_destroyed_on_release() {
    use std::ffi::CString;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    static DTOR_RUNS: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn counting_dtor(_: *mut ErlNifEnv, _: *mut c_void) {
        DTOR_RUNS.fetch_add(1, AtomicOrdering::Relaxed);
    }

    let mut host = RetainHost {
        heap: beam_alloc::Heap::new(4096).expect("a small test heap is allocatable"),
        atoms: beam_term::atom::AtomTable::new(),
    };
    let env = NifEnv::new(&mut host);
    let name = CString::new("no_term_rt").expect("no interior NUL");
    let mut tried = ErlNifResourceFlags::from_bits(0);
    // SAFETY: `env` is live; `name` is NUL-terminated; `tried` is a live local.
    let ty = unsafe {
        enif_open_resource_type(
            env.as_ptr(),
            core::ptr::null(),
            name.as_ptr(),
            Some(counting_dtor),
            ErlNifResourceFlags::CREATE,
            &mut tried,
        )
    };
    assert!(!ty.is_null());

    let before = DTOR_RUNS.load(AtomicOrdering::Relaxed);
    // SAFETY: `ty` came from the open call above.
    let obj = unsafe { enif_alloc_resource(ty, 1) };
    assert!(!obj.is_null());
    // SAFETY: the destructor must run synchronously on the last release of an unexported object.
    unsafe { enif_release_resource(obj) };
    assert_eq!(
        DTOR_RUNS.load(AtomicOrdering::Relaxed) - before,
        1,
        "the destructor runs exactly once on the release that reaches zero"
    );
}

/// The destructor runs with a **live environment built from the type's host** — the analogue of the
/// C's `pre_nif_noproc(&msg_env, type->owner, NULL)` (`erl_nif.c:3016-3018`). A destructor that drives
/// the env's host (here: interns an atom) therefore works, and it is handed the type's env, **not**
/// the env of the call that released the last reference.
#[test]
fn the_destructor_receives_a_live_environment_from_the_types_host() {
    use std::ffi::CString;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};

    static ENV_REACHED_HOST: AtomicBool = AtomicBool::new(false);
    static PAYLOAD_SEEN: AtomicBool = AtomicBool::new(false);
    static DTOR_ENV: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn probing_dtor(env: *mut ErlNifEnv, obj: *mut c_void) {
        PAYLOAD_SEEN.store(!obj.is_null(), AtomicOrdering::Relaxed);
        // SAFETY: `env` is the live environment `destroy` built for this call — its pointer came from
        // `NifEnv::as_ptr` on a `NifEnv` that outlives the destructor call.
        if let Some(e) = unsafe { NifEnv::from_opaque(env) } {
            DTOR_ENV.store(env as usize, AtomicOrdering::Relaxed);
            ENV_REACHED_HOST.store(
                e.host().intern_atom("dtor_env_atom").is_some(),
                AtomicOrdering::Relaxed,
            );
        }
    }

    let mut host = RetainHost {
        heap: beam_alloc::Heap::new(4096).expect("a small test heap is allocatable"),
        atoms: beam_term::atom::AtomTable::new(),
    };
    let env = NifEnv::new(&mut host);
    let call_env = env.as_ptr();
    let name = CString::new("dtor_env_rt").expect("no interior NUL");
    let mut tried = ErlNifResourceFlags::from_bits(0);
    // SAFETY: `env` is live; `name` is NUL-terminated; `tried` is a live local.
    let ty = unsafe {
        enif_open_resource_type(
            env.as_ptr(),
            core::ptr::null(),
            name.as_ptr(),
            Some(probing_dtor),
            ErlNifResourceFlags::CREATE,
            &mut tried,
        )
    };
    assert!(!ty.is_null(), "the type must open");

    // SAFETY: `ty` came from the open call above.
    let obj = unsafe { enif_alloc_resource(ty, 4) };
    assert!(!obj.is_null(), "the allocation must succeed");
    // The object is never exported as a term, so this release reaches zero and runs the destructor.
    // SAFETY: `obj` is a live payload pointer with its one outstanding reference.
    unsafe { enif_release_resource(obj) };

    assert!(
        PAYLOAD_SEEN.load(AtomicOrdering::Relaxed),
        "the destructor is handed the payload"
    );
    assert!(
        ENV_REACHED_HOST.load(AtomicOrdering::Relaxed),
        "the destructor's environment must reach the type's host"
    );
    assert_ne!(
        DTOR_ENV.load(AtomicOrdering::Relaxed),
        call_env as usize,
        "the destructor is handed the type's own env, not the releasing call's"
    );

    // SAFETY: `env` is still a live env.
    let atom = env.host().intern_atom("after_dtor").expect("interns");
    assert!(prelude::is_atom(atom), "the call env is untouched");
}

/// The destructor's **count and ordering**, which match the oracle exactly (only the timing differs):
/// it does not run while a `keep` is outstanding, runs exactly once on the release that reaches zero,
/// and runs in release order across objects.
#[test]
fn a_keep_defers_the_destructor_until_the_last_release() {
    use std::ffi::CString;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    static COUNT: AtomicUsize = AtomicUsize::new(0);
    static ORDER: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn tagging_dtor(_: *mut ErlNifEnv, obj: *mut c_void) {
        // SAFETY: the fixture-shaped contract: `obj` is a live payload whose first byte is readable.
        let byte = unsafe { *(obj as *const u8) } as usize;
        // Append the tag, so the final value encodes the order the destructors ran in.
        let _ = ORDER.fetch_update(
            AtomicOrdering::Relaxed,
            AtomicOrdering::Relaxed,
            |prev| Some((prev << 8) | byte),
        );
        COUNT.fetch_add(1, AtomicOrdering::Relaxed);
    }

    let mut host = RetainHost {
        heap: beam_alloc::Heap::new(4096).expect("a small test heap is allocatable"),
        atoms: beam_term::atom::AtomTable::new(),
    };
    let env = NifEnv::new(&mut host);
    let name = CString::new("keep_rt").expect("no interior NUL");
    let mut tried = ErlNifResourceFlags::from_bits(0);
    // SAFETY: `env` is live; `name` is NUL-terminated; `tried` is a live local.
    let ty = unsafe {
        enif_open_resource_type(
            env.as_ptr(),
            core::ptr::null(),
            name.as_ptr(),
            Some(tagging_dtor),
            ErlNifResourceFlags::CREATE,
            &mut tried,
        )
    };
    assert!(!ty.is_null(), "the type must open");

    let base = COUNT.load(AtomicOrdering::Relaxed);
    // SAFETY: `ty` is a live type; each `obj` is released exactly as often as it is kept/allocated.
    unsafe {
        let a = enif_alloc_resource(ty, 1);
        let b = enif_alloc_resource(ty, 1);
        assert!(!a.is_null() && !b.is_null());
        *(a as *mut u8) = 0x11;
        *(b as *mut u8) = 0x22;

        enif_keep_resource(a); // a: 2
        enif_release_resource(a); // a: 1 — a live keep, so no destructor
        assert_eq!(
            COUNT.load(AtomicOrdering::Relaxed) - base,
            0,
            "a keep is outstanding, so nothing is destroyed"
        );
        enif_release_resource(a); // a: 0 — destructor #1 (tag 0x11)
        enif_release_resource(b); // b: 0 — destructor #2 (tag 0x22)
    }
    assert_eq!(
        COUNT.load(AtomicOrdering::Relaxed) - base,
        2,
        "exactly one destructor run per object reaching zero"
    );
    assert_eq!(
        ORDER.load(AtomicOrdering::Relaxed),
        0x1122,
        "the destructors run in release order"
    );
}

#[test]
fn header_layout_is_the_documented_block_shape() {
    assert_eq!(RESOURCE_HEADER_SIZE, 32);
    assert_eq!(core::mem::align_of::<ResourceHeader>(), 16);
}

#[test]
fn header_and_payload_pointers_round_trip() {
    // SAFETY: a live 32-byte block; the payload pointer is derived from it and converted back.
    unsafe {
        let block = libc::malloc(RESOURCE_HEADER_SIZE + 8).cast::<ResourceHeader>();
        assert!(!block.is_null());
        let payload = payload_ptr(block).cast::<c_void>();
        assert_eq!(header_ptr(payload), block);
        libc::free(block.cast::<c_void>());
    }
}

#[test]
fn callback_members_switch_reads_from_the_top() {
    use crate::common::ErlDrvMonitor;
    use crate::nif::types::ErlNifPid;

    unsafe extern "C" fn noop_dtor(_: *mut ErlNifEnv, _: *mut c_void) {}
    unsafe extern "C" fn noop_stop(
        _: *mut ErlNifEnv,
        _: *mut c_void,
        _: libc::c_int,
        _: libc::c_int,
    ) {
    }
    unsafe extern "C" fn noop_down(
        _: *mut ErlNifEnv,
        _: *mut c_void,
        _: *mut ErlNifPid,
        _: *mut ErlDrvMonitor,
    ) {
    }
    unsafe extern "C" fn noop_dyncall(_: *mut ErlNifEnv, _: *mut c_void, _: *mut c_void) {}

    let init = ErlNifResourceTypeInit {
        dtor: Some(noop_dtor),
        stop: Some(noop_stop),
        down: Some(noop_down),
        members: 4,
        dyncall: Some(noop_dyncall),
    };
    let (d, s, w, c) = callbacks(&init, 1);
    assert!(d.is_some() && s.is_none() && w.is_none() && c.is_none());
    let (d, s, w, c) = callbacks(&init, 3);
    assert!(d.is_some() && s.is_some() && w.is_some() && c.is_none());
    let (d, s, w, c) = callbacks(&init, 4);
    assert!(d.is_some() && s.is_some() && w.is_some() && c.is_some());
    let (d, s, w, c) = callbacks(&init, 0);
    assert!(d.is_none() && s.is_none() && w.is_none() && c.is_none());
}

// ---------------------------------------------------------------------------------------------
// The resource-term release seam — the caller-owned "the term died" signal
//
// `release_resource_terms(predicate)` drops the one reference each *unreachable* resource term holds
// (the reference `enif_make_resource` took, the C magic ref's), removing the term→object binding and
// running the destructor when that was the last reference. The predicate is the caller's soundness
// assertion — a false "unreachable" is a use-after-free — so every predicate below is deliberately
// narrowed to *this* test's own term words (`candidate.raw() != word`), which keeps the other tests'
// terms in this shared process reachable when the whole binary runs in parallel.
//
// Oracle semantics this enables (measured on OTP 29.1.1 / erts-17.1 with
// `tests/fixtures/nif_resource_release.c`): `enif_alloc_resource` + `enif_make_resource` +
// `enif_release_resource` + a forced `erlang:garbage_collect/0` ⇒ the destructor count is `1`.
// ---------------------------------------------------------------------------------------------

/// A destructor that records nothing — for the tests whose assertions are about the *term* and the
/// reference count, not about the callback.
unsafe extern "C" fn silent_dtor(_: *mut ErlNifEnv, _: *mut c_void) {}

/// The destructor fires exactly once per term death: **0** before the prune, **+1** when the term is
/// released, and still **+1** when the same term is pruned again (no double fire).
#[test]
fn a_pruned_term_drops_the_terms_reference_and_runs_the_destructor_once() {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    static RUNS: AtomicUsize = AtomicUsize::new(0);
    static DTOR_OBJ: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn counting_dtor(_: *mut ErlNifEnv, obj: *mut c_void) {
        DTOR_OBJ.store(obj as usize, AtomicOrdering::Relaxed);
        RUNS.fetch_add(1, AtomicOrdering::Relaxed);
    }

    let mut host = RetainHost::new();
    let env = NifEnv::new(&mut host);
    let ty = open_type(&env, "prune_count_rt", counting_dtor);
    let base = RUNS.load(AtomicOrdering::Relaxed);

    // SAFETY: `ty` came from `open_type`; the object is released exactly once below (after the term
    // took its own reference).
    let obj = unsafe { alloc_object(ty, 4) };
    // SAFETY: `obj` is a live 4-byte payload; the first byte is in bounds.
    unsafe { *(obj as *mut u8) = 7 };
    // SAFETY: `env` is live; `obj` is a live payload pointer.
    let term = unsafe { make_resource_term(&env, obj) };
    // Drop the allocator's reference: the term's own reference is all that is left.
    // SAFETY: `obj` still has the term's outstanding reference.
    unsafe { enif_release_resource(obj) };
    assert_eq!(
        RUNS.load(AtomicOrdering::Relaxed) - base,
        0,
        "the resource term keeps the object alive, so no destructor has run"
    );

    let word = term.raw();
    assert_eq!(
        release_resource_terms(|candidate| candidate.raw() != word),
        1,
        "the prune releases exactly the one term the predicate called unreachable"
    );
    assert_eq!(
        RUNS.load(AtomicOrdering::Relaxed) - base,
        1,
        "the term was the last reference, so its destructor ran exactly once"
    );
    assert_eq!(
        DTOR_OBJ.load(AtomicOrdering::Relaxed),
        obj as usize,
        "the destructor was handed this object's payload"
    );

    // Idempotent by term identity: the binding is gone, so a second and a third prune release nothing
    // and the destructor does not fire again.
    assert_eq!(release_resource_terms(|candidate| candidate.raw() != word), 0);
    assert_eq!(release_resource_terms(|candidate| candidate.raw() != word), 0);
    assert_eq!(
        RUNS.load(AtomicOrdering::Relaxed) - base,
        1,
        "a repeated prune must not run the destructor a second time"
    );
}

/// A pruned term no longer resolves: the term→object binding is removed, so `enif_get_resource`
/// answers `0` (and leaves `*objp` untouched) afterwards.
#[test]
fn a_pruned_term_no_longer_resolves_through_enif_get_resource() {
    let mut host = RetainHost::new();
    let env = NifEnv::new(&mut host);
    let ty = open_type(&env, "prune_binding_rt", silent_dtor);

    // SAFETY: `ty` came from `open_type`; the term holds the only remaining reference.
    let obj = unsafe { alloc_object(ty, 1) };
    // SAFETY: `env` is live; `obj` is a live payload pointer.
    let term = unsafe { make_resource_term(&env, obj) };
    // SAFETY: the term keeps the object alive from here on.
    unsafe { enif_release_resource(obj) };

    // Before the prune the term designates the object (the oracle's behaviour).
    // SAFETY: `env` is live; `ty` is the object's type.
    let (matched, got) = unsafe { get_resource(&env, term, ty) };
    assert_eq!((matched, got), (1, obj), "the term resolves before the prune");

    let word = term.raw();
    assert_eq!(release_resource_terms(|candidate| candidate.raw() != word), 1);

    // A sentinel that is *not* a resource payload pointer (it addresses a local byte), so "left
    // untouched" is distinguishable from "written".
    let mut placeholder: u8 = 0;
    let sentinel: *mut c_void = (&mut placeholder as *mut u8).cast();
    let mut objp: *mut c_void = sentinel;
    // SAFETY: `env` is live; `term` is the term just pruned; `ty` is a live type; `objp` is a live
    // local.
    let after = unsafe { enif_get_resource(env.as_ptr(), n(term), ty, &mut objp) };
    assert_eq!(after, 0, "a pruned term no longer designates anything");
    assert_eq!(
        objp, sentinel,
        "*objp is left untouched, as for any non-resource term"
    );
}

/// A resource with a live user reference **survives** the prune: its count drops by exactly one, no
/// destructor runs, the payload stays readable, and the later release runs the destructor once.
#[test]
fn a_resource_with_a_live_user_reference_survives_the_prune() {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    static RUNS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn counting_dtor(_: *mut ErlNifEnv, _: *mut c_void) {
        RUNS.fetch_add(1, AtomicOrdering::Relaxed);
    }

    let mut host = RetainHost::new();
    let env = NifEnv::new(&mut host);
    let ty = open_type(&env, "prune_live_user_ref_rt", counting_dtor);
    let base = RUNS.load(AtomicOrdering::Relaxed);

    // alloc (1) + keep (2) + term (3).
    // SAFETY: `ty` came from `open_type`; every reference taken here is released below exactly once.
    let obj = unsafe { alloc_object(ty, 4) };
    // SAFETY: `obj` is a live 4-byte payload; the first byte is in bounds.
    unsafe { *(obj as *mut u8) = 0x5a };
    // SAFETY: `obj` is a live payload pointer with an outstanding reference.
    unsafe { enif_keep_resource(obj) };
    // SAFETY: `env` is live; `obj` is a live payload pointer.
    let term = unsafe { make_resource_term(&env, obj) };

    // Prune the term: only its reference goes, so the object (alloc + keep ⇒ 2) survives untouched.
    let word = term.raw();
    assert_eq!(release_resource_terms(|candidate| candidate.raw() != word), 1);
    assert_eq!(
        RUNS.load(AtomicOrdering::Relaxed) - base,
        0,
        "a live user reference keeps the object — and its destructor — alive"
    );
    // SAFETY: `obj` is still alive (two references), so reading its payload and header is sound.
    unsafe {
        assert_eq!(*(obj as *const u8), 0x5a, "the payload is intact");
        assert_eq!(
            enif_sizeof_resource(obj),
            4,
            "the block is still allocated with its recorded payload size"
        );
        // The allocator's reference: 2 -> 1 (the keep).
        enif_release_resource(obj);
    }
    assert_eq!(
        RUNS.load(AtomicOrdering::Relaxed) - base,
        0,
        "the `keep` is still outstanding, so nothing is destroyed"
    );
    // SAFETY: the keep's reference is the last one: 1 -> 0, which runs the destructor.
    unsafe { enif_release_resource(obj) };
    assert_eq!(
        RUNS.load(AtomicOrdering::Relaxed) - base,
        1,
        "the later release runs the destructor exactly once"
    );
}

/// A term the predicate calls **reachable** is left completely alone: it still resolves and no
/// destructor runs. (The only difference from a release is the predicate's answer.)
#[test]
fn a_reachable_term_is_left_alone() {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    static RUNS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn counting_dtor(_: *mut ErlNifEnv, _: *mut c_void) {
        RUNS.fetch_add(1, AtomicOrdering::Relaxed);
    }

    let mut host = RetainHost::new();
    let env = NifEnv::new(&mut host);
    let ty = open_type(&env, "prune_reachable_rt", counting_dtor);
    let base = RUNS.load(AtomicOrdering::Relaxed);

    // SAFETY: `ty` came from `open_type`; the term holds the only remaining reference.
    let obj = unsafe { alloc_object(ty, 2) };
    // SAFETY: `env` is live; `obj` is a live payload pointer.
    let term = unsafe { make_resource_term(&env, obj) };
    // SAFETY: the term keeps the object alive from here on.
    unsafe { enif_release_resource(obj) };

    // Everything reachable: the prune is a no-op over the whole process.
    assert_eq!(
        release_resource_terms(|_| true),
        0,
        "no term is called unreachable, so nothing is released"
    );
    assert_eq!(
        RUNS.load(AtomicOrdering::Relaxed) - base,
        0,
        "and no destructor runs"
    );
    // SAFETY: `env` is live; `ty` is the object's type.
    let (matched, got) = unsafe { get_resource(&env, term, ty) };
    assert_eq!((matched, got), (1, obj), "the term still resolves");

    // The same term, now called unreachable, is released — proving the predicate is the whole
    // difference.
    let word = term.raw();
    assert_eq!(release_resource_terms(|candidate| candidate.raw() != word), 1);
    assert_eq!(RUNS.load(AtomicOrdering::Relaxed) - base, 1);
}

/// Two terms for **one** object: releasing the first leaves the object alive (and the second term
/// resolving), releasing the second runs the destructor once.
#[test]
fn two_terms_for_one_object_release_one_at_a_time() {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    static RUNS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn counting_dtor(_: *mut ErlNifEnv, _: *mut c_void) {
        RUNS.fetch_add(1, AtomicOrdering::Relaxed);
    }

    let mut host = RetainHost::new();
    let env = NifEnv::new(&mut host);
    let ty = open_type(&env, "prune_two_terms_rt", counting_dtor);
    let base = RUNS.load(AtomicOrdering::Relaxed);

    // alloc (1) + two terms (2, 3).
    // SAFETY: `ty` came from `open_type`; the single allocator reference is released once below.
    let obj = unsafe { alloc_object(ty, 4) };
    // SAFETY: `obj` is a live 4-byte payload; the first byte is in bounds.
    unsafe { *(obj as *mut u8) = 0x21 };
    // SAFETY: `env` is live; `obj` is a live payload pointer.
    let first = unsafe { make_resource_term(&env, obj) };
    // SAFETY: as above — a second, distinct term for the same object (the oracle mints a fresh magic
    // ref per call).
    let second = unsafe { make_resource_term(&env, obj) };
    // SAFETY: `obj` still has the two terms' references.
    unsafe { enif_release_resource(obj) };

    let first_word = first.raw();
    assert_eq!(release_resource_terms(|candidate| candidate.raw() != first_word), 1);
    assert_eq!(
        RUNS.load(AtomicOrdering::Relaxed) - base,
        0,
        "the second term still holds the object"
    );
    // SAFETY: `env` is live; `second` is still a live resource term; `ty` is the object's type.
    let (matched, got) = unsafe { get_resource(&env, second, ty) };
    assert_eq!((matched, got), (1, obj), "the second term keeps resolving");
    // SAFETY: the object is alive (the second term's reference).
    assert_eq!(unsafe { *(obj as *const u8) }, 0x21, "the payload is intact");

    let second_word = second.raw();
    assert_eq!(release_resource_terms(|candidate| candidate.raw() != second_word), 1);
    assert_eq!(
        RUNS.load(AtomicOrdering::Relaxed) - base,
        1,
        "the second release reaches zero and runs the destructor exactly once"
    );
}

/// `resource_terms()` reports the live set — both minting routes included — and shrinks after the
/// prune.
#[test]
fn resource_terms_reports_the_live_set_and_shrinks_after_the_prune() {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    static RUNS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn counting_dtor(_: *mut ErlNifEnv, _: *mut c_void) {
        RUNS.fetch_add(1, AtomicOrdering::Relaxed);
    }

    let mut host = RetainHost::new();
    let env = NifEnv::new(&mut host);
    let ty = open_type(&env, "prune_snapshot_rt", counting_dtor);
    let base = RUNS.load(AtomicOrdering::Relaxed);

    let before = resource_terms();
    // SAFETY: `ty` came from `open_type`; the allocator's reference is released below.
    let obj = unsafe { alloc_object(ty, 4) };
    // SAFETY: `env` is live; `obj` is a live payload pointer.
    let term = unsafe { make_resource_term(&env, obj) };
    let bytes = [9u8, 8, 7, 6];
    // SAFETY: `env` is live; `obj` is a live payload pointer; `bytes` is a readable 4-byte buffer.
    let term_bin = t(unsafe {
        enif_make_resource_binary(env.as_ptr(), obj, bytes.as_ptr().cast(), bytes.len())
    });
    let live = resource_terms();
    assert!(
        live.contains(&term),
        "the `enif_make_resource` term is in the live set"
    );
    assert!(
        live.contains(&term_bin),
        "and so is the `enif_make_resource_binary` term"
    );
    // No length delta is asserted: this registry is process-wide and a *leaked* term from an earlier
    // test (a term nobody pruned) can share a raw word with a fresh one when a freed host arena is
    // reallocated at the same address — the binding is then simply replaced. Membership plus the
    // released count below are the concurrency-proof observables.
    assert!(live.len() >= before.len());
    let words = [term.raw(), term_bin.raw()];
    assert_eq!(
        release_resource_terms(|candidate| !words.contains(&candidate.raw())),
        2,
        "both terms are released in one pass"
    );
    let after = resource_terms();
    assert!(!after.contains(&term), "the released terms leave the set");
    assert!(!after.contains(&term_bin));
    assert_eq!(
        RUNS.load(AtomicOrdering::Relaxed) - base,
        0,
        "the allocator's reference is still outstanding, so nothing is destroyed yet"
    );
    // SAFETY: the allocator's reference is the last one (three references — the allocator's and the
    // two terms' — minus the two the prune just dropped): the destructor runs here, once.
    unsafe { enif_release_resource(obj) };
    assert_eq!(RUNS.load(AtomicOrdering::Relaxed) - base, 1);
}

/// The seam is reachable at both documented public paths — `nif::enif::resources::*` (where it is
/// implemented) and `nif::enif::*` (the family re-export) — because the next `beam_rt` round
/// implements the caller side against them.
#[test]
fn the_release_seam_is_publicly_reachable() {
    unsafe extern "C" fn noop_dtor(_: *mut ErlNifEnv, _: *mut c_void) {}

    // Every live term is called reachable here, so nothing is released (and the shared registry of
    // this test binary is left untouched).
    assert_eq!(crate::nif::enif::resources::release_resource_terms(|_| true), 0);
    assert_eq!(crate::nif::enif::release_resource_terms(|_| true), 0);

    // A term minted by this test is visible through both paths. Two consecutive snapshots are
    // deliberately NOT compared for set equality: the registry is process-wide, so a concurrently
    // running test can mint or prune a term between the two calls. Membership of a term this test
    // owns is the concurrency-proof observable.
    let mut host = RetainHost::new();
    let env = NifEnv::new(&mut host);
    let ty = open_type(&env, "seam_reachable_rt", noop_dtor);
    // SAFETY: `ty` came from `open_type`; the allocator's reference is released below.
    let obj = unsafe { alloc_object(ty, 1) };
    // SAFETY: `env` is live; `obj` is a live payload pointer.
    let term = unsafe { make_resource_term(&env, obj) };
    // SAFETY: the minted term holds the reference that keeps the object alive from here on.
    unsafe { enif_release_resource(obj) };
    assert!(
        crate::nif::enif::resources::resource_terms().contains(&term),
        "the implemented path reports the live term"
    );
    assert!(
        crate::nif::enif::resource_terms().contains(&term),
        "so does the family re-export"
    );
    // Leave the process-wide registry as it was: drop this term's own reference (everything else in
    // the snapshot is called reachable, so only this binding is taken).
    assert_eq!(
        release_resource_terms(|candidate| candidate.raw() != term.raw()),
        1,
        "only this test's term is released"
    );
}

/// Concurrent prunes of the same term release it **exactly once**: the binding removal under the
/// registry lock hands it to one thread only.
#[test]
fn concurrent_prunes_release_a_term_exactly_once() {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    static RUNS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn counting_dtor(_: *mut ErlNifEnv, _: *mut c_void) {
        RUNS.fetch_add(1, AtomicOrdering::Relaxed);
    }

    let mut host = RetainHost::new();
    let word = {
        let env = NifEnv::new(&mut host);
        let ty = open_type(&env, "prune_concurrent_rt", counting_dtor);
        // SAFETY: `ty` came from `open_type`; the term holds the only remaining reference.
        let obj = unsafe { alloc_object(ty, 1) };
        // SAFETY: `env` is live; `obj` is a live payload pointer.
        let term = unsafe { make_resource_term(&env, obj) };
        // SAFETY: the term keeps the object alive from here on.
        unsafe { enif_release_resource(obj) };
        term.raw()
    }; // the env is dropped here, so `host` is no longer mutably borrowed by this thread

    let base = RUNS.load(AtomicOrdering::Relaxed);
    let threads: Vec<_> = (0..4)
        .map(|_| {
            std::thread::spawn(move || release_resource_terms(|candidate| candidate.raw() != word))
        })
        .collect();
    let released: usize = threads
        .into_iter()
        .map(|handle| handle.join().expect("a pruning thread must not panic"))
        .sum();
    assert_eq!(released, 1, "exactly one thread takes the term binding");
    assert_eq!(
        RUNS.load(AtomicOrdering::Relaxed) - base,
        1,
        "so the destructor runs exactly once"
    );
}

// ---------------------------------------------------------------------------------------------
// Term IDENTITY, not the heap word: a copied resource term designates the same object
//
// A resource term is an internal reference whose identity is the `id` in its block's last word, so a
// `copy_struct` copy (the send path, ETS) is a *different heap address with the same identity* and
// designates the same resource. The C's magic refs are pointer-identified through `ErtsMRefThing.mb`,
// which is copy-stable, so the oracle answers `1` for a copy — measured with
// `tests/fixtures/nif_resource.c`'s `roundtrip/0` shape plus a `copy_struct` copy of the term.
// ---------------------------------------------------------------------------------------------

/// The registry key of a term (the private helper under test) — the one place binding and lookup
/// derive identity from.
fn key_of(term: Eterm) -> TermKey {
    term_key(term)
}

/// A copy of a minted resource term resolves to the **same object**, and both copies share the one
/// binding the mint created (so a single prune pass releases exactly one reference).
#[test]
fn a_copied_resource_term_resolves_to_the_same_object() {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    static RUNS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn counting_dtor(_: *mut ErlNifEnv, _: *mut c_void) {
        RUNS.fetch_add(1, AtomicOrdering::Relaxed);
    }

    let mut host = RetainHost::new();
    let env = NifEnv::new(&mut host);
    let ty = open_type(&env, "copied_term_rt", counting_dtor);
    let base = RUNS.load(AtomicOrdering::Relaxed);

    // alloc (1) + the minted term (2); the allocator's reference is dropped below, so the term holds
    // the only one from then on.
    // SAFETY: `ty` came from `open_type`.
    let obj = unsafe { alloc_object(ty, 4) };
    // SAFETY: `obj` is a live 4-byte payload; the first byte is in bounds.
    unsafe { *(obj as *mut u8) = 0x33 };
    // SAFETY: `env` is live; `obj` is a live payload pointer.
    let minted = unsafe { make_resource_term(&env, obj) };
    // SAFETY: the term keeps the object alive from here on.
    unsafe { enif_release_resource(obj) };

    // The copy: four identical words at a different address — what `copy_struct` produces.
    // SAFETY: `minted` is the internal reference `mint_resource_ref` just built.
    let copy = unsafe { copy_reference(&env, minted) };
    assert_ne!(
        copy.raw(),
        minted.raw(),
        "the copy lives at a different heap address"
    );
    assert_eq!(
        key_of(copy),
        key_of(minted),
        "but it carries the same reference identity"
    );
    // The minted term resolves (the C's behaviour, unchanged).
    // SAFETY: `env` is live; `ty` is the object's type.
    let (matched, got) = unsafe { get_resource(&env, minted, ty) };
    assert_eq!((matched, got), (1, obj), "the minted term resolves");
    // **The fix:** the copy resolves too. Before the identity key this answered `(0, null)` — the
    // registry was keyed by the term WORD, so the copy's distinct address missed — while the oracle
    // answers `1`.
    // SAFETY: `env` is live; `ty` is the object's type.
    let (copied_matched, copied_got) = unsafe { get_resource(&env, copy, ty) };
    assert_eq!(
        (copied_matched, copied_got),
        (1, obj),
        "a copy_struct copy of a resource term resolves to the same object"
    );
    assert_eq!(got, copied_got, "both idioms name one and the same payload");

    // One binding per MINT: a copy shares the minted term's binding instead of adding one, so
    // `resource_terms()` still reports the minted word (the caller's reachability input) and one
    // prune pass drops exactly one reference.
    assert_eq!(
        resource_terms().iter().filter(|t| **t == minted).count(),
        1,
        "the minted term is in the live set exactly once"
    );

    let minted_word = minted.raw();
    assert_eq!(
        release_resource_terms(|candidate| candidate.raw() != minted_word),
        1,
        "one binding for this mint ⇒ one released reference"
    );
    assert_eq!(
        RUNS.load(AtomicOrdering::Relaxed) - base,
        1,
        "the term held the last reference, so its destructor ran once"
    );
    // After the prune neither term designates anything — the copy included, because it resolved
    // through the same binding.
    // SAFETY: `env` is live; both terms are still readable words (their host arena is alive).
    let after_minted = unsafe { get_resource(&env, minted, ty) };
    let after_copy = unsafe { get_resource(&env, copy, ty) };
    assert_eq!(after_minted.0, 0, "the pruned term no longer resolves");
    assert_eq!(after_copy.0, 0, "nor does its copy");
}

/// A reference identity that was **never minted** is not a resource, and neither is any non-reference
/// term: `enif_get_resource` answers `0` and leaves `*objp` untouched (the C's contract).
#[test]
fn an_unminted_reference_identity_is_not_a_resource() {
    let mut host = RetainHost::new();
    let env = NifEnv::new(&mut host);
    let ty = open_type(&env, "unminted_ref_rt", silent_dtor);

    // An ordinary internal reference (`erlang:make_ref/0`'s shape) whose id no `enif_make_resource`
    // ever minted.
    // SAFETY: `alloc` hands out exactly `INTERNAL_REF_WORDS` exclusively-owned words, which
    // `write_internal_ref` fully initialises with an id that fits a small integer.
    let unminted = unsafe { alloc(&env, prelude::INTERNAL_REF_WORDS) }
        .expect("the test heap can fit a four-word reference");
    // SAFETY: `unminted` addresses the four words just reserved; the id fits the small-integer slot.
    let unminted = unsafe { prelude::write_internal_ref(unminted, 999_999) }
        .expect("999_999 fits the reference id slot");
    assert_eq!(
        key_of(unminted),
        TermKey::Ref(999_999),
        "it is keyed by its reference identity like any other M1 reference"
    );

    // A not-a-resource sentinel that addresses a local byte, so "left untouched" is distinguishable
    // from "written".
    let mut placeholder: u8 = 0;
    let sentinel: *mut c_void = (&mut placeholder as *mut u8).cast();
    let atom = env.host().intern_atom("not_a_resource").expect("interns");
    for term in [unminted, prelude::make_small(7), atom, NIL] {
        let mut objp: *mut c_void = sentinel;
        // SAFETY: `env` is live; `term` is a readable term; `ty` is a live type; `objp` is a live
        // local.
        let matched = unsafe { enif_get_resource(env.as_ptr(), n(term), ty, &mut objp) };
        assert_eq!(
            matched, 0,
            "a term that is not one of this crate's resource terms never resolves"
        );
        assert_eq!(objp, sentinel, "*objp is left untouched");
    }
}

/// The C's **type check**: a valid resource term of another type answers `0` (and leaves `*objp`
/// untouched), for a copy as well as for the minted term.
#[test]
fn a_resource_term_of_another_type_does_not_resolve() {
    let mut host = RetainHost::new();
    let env = NifEnv::new(&mut host);
    let ty = open_type(&env, "type_check_rt", silent_dtor);
    let other = open_type(&env, "type_check_other_rt", silent_dtor);
    assert_ne!(ty, other, "two distinct types");

    // SAFETY: `ty` came from `open_type`; the term holds the only remaining reference.
    let obj = unsafe { alloc_object(ty, 2) };
    // SAFETY: `env` is live; `obj` is a live payload pointer.
    let minted = unsafe { make_resource_term(&env, obj) };
    // SAFETY: the term keeps the object alive from here on.
    unsafe { enif_release_resource(obj) };
    // SAFETY: `minted` is the internal reference `mint_resource_ref` just built.
    let copy = unsafe { copy_reference(&env, minted) };

    let mut placeholder: u8 = 0;
    let sentinel: *mut c_void = (&mut placeholder as *mut u8).cast();
    for term in [minted, copy] {
        let mut objp: *mut c_void = sentinel;
        // SAFETY: `env` is live; `term` is a resource term; `other` is a live type; `objp` is a live
        // local.
        let matched = unsafe { enif_get_resource(env.as_ptr(), n(term), other, &mut objp) };
        assert_eq!(matched, 0, "the resource's type is not `other`");
        assert_eq!(objp, sentinel, "*objp is left untouched");
        // The same term against its own type does resolve — the type is what made the difference.
        // SAFETY: `env` is live; `ty` is the object's real type.
        assert_eq!(
            unsafe { get_resource(&env, term, ty) },
            (1, obj),
            "the same term resolves against its own type"
        );
    }

    // Leave the process-wide registry as it was.
    let minted_word = minted.raw();
    assert_eq!(release_resource_terms(|candidate| candidate.raw() != minted_word), 1);
}

// ---------------------------------------------------------------------------------------------
// `resource_term(host, obj)` — a resource term for a bare `void*` handle
// (`resources_term.rs`; the C's `prepare_select_msg`, `erl_check_io.c:699-767`)
//
// Every prune below is narrowed to this test's own term words (`candidate.raw() != word`), so the
// other tests' live terms in this shared process stay reachable under parallel execution; and every
// assertion is a membership/identity statement rather than a length delta on the process-wide
// registry, for the same reason.
// ---------------------------------------------------------------------------------------------

/// A handle that is null, and a pointer this crate never allocated, both answer `None` — and neither
/// even allocates a reference block, so no term is fabricated and no object is resurrected.
#[test]
fn resource_term_refuses_a_handle_that_is_not_ours() {
    let mut host = RetainHost::new();
    let before = host.heap.used();

    // SAFETY: a null handle is explicitly allowed by `resource_term`'s contract.
    assert_eq!(
        resource_term(&mut host, core::ptr::null_mut()),
        None,
        "a null handle names no object"
    );
    // A pointer this crate never allocated. It is never dereferenced: the registry's live-object set
    // is the only gate, exactly as for `enif_get_resource`.
    let foreign = 0x1000 as *mut c_void;
    assert_eq!(
        resource_term(&mut host, foreign),
        None,
        "a pointer this crate did not allocate is not a resource"
    );
    assert_eq!(
        host.heap.used(),
        before,
        "both refusals happen before the reference block is allocated — nothing was minted"
    );
}

/// The minted term is a full resource term: `enif_get_resource` resolves it to the very pointer that
/// was allocated, and it holds **its own** reference (releasing the allocator's leaves it alive and
/// the destructor un-run until the term dies).
#[test]
fn resource_term_mints_a_resolving_term_that_holds_its_own_reference() {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    static RUNS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn counting_dtor(_: *mut ErlNifEnv, _: *mut c_void) {
        RUNS.fetch_add(1, AtomicOrdering::Relaxed);
    }

    let mut host = RetainHost::new();
    let (ty, obj) = {
        let env = NifEnv::new(&mut host);
        let ty = open_type(&env, "select_term_rt", counting_dtor);
        // SAFETY: `ty` came from `open_type`; every reference taken here is released below.
        let obj = unsafe { alloc_object(ty, 4) };
        // SAFETY: `obj` is a live 4-byte payload; the first byte is in bounds.
        unsafe { *(obj as *mut u8) = 0x2a };
        (ty, obj)
    };
    let base = RUNS.load(AtomicOrdering::Relaxed);

    // The host seam call: a bare `void*` in, a resource term out (no env, no raw pointer to an env).
    let term = resource_term(&mut host, obj).expect("a live object mints a term");

    {
        let env = NifEnv::new(&mut host);
        // SAFETY: `env` is live; `term` is the term just minted; `ty` is the object's own type.
        assert_eq!(
            unsafe { get_resource(&env, term, ty) },
            (1, obj),
            "the minted term resolves to the very payload pointer that was allocated"
        );
    }

    // Release the allocator's reference: the mint's own reference keeps the block (and the payload)
    // alive, exactly as for `enif_make_resource` (module docs, "It takes a reference").
    // SAFETY: `obj` still has the term's outstanding reference.
    unsafe { enif_release_resource(obj) };
    assert_eq!(
        RUNS.load(AtomicOrdering::Relaxed) - base,
        0,
        "the term's reference keeps the object alive, so no destructor has run"
    );
    {
        let env = NifEnv::new(&mut host);
        // SAFETY: `env` is live; `ty` is the object's own type; the block is still allocated.
        assert_eq!(
            unsafe { get_resource(&env, term, ty) },
            (1, obj),
            "and it still resolves after the allocator's release"
        );
    }

    // The term's reference is dropped by the release seam (the caller-owned reachability signal).
    let word = term.raw();
    assert_eq!(
        release_resource_terms(|candidate| candidate.raw() != word),
        1,
        "the prune releases exactly this term"
    );
    assert_eq!(
        RUNS.load(AtomicOrdering::Relaxed) - base,
        1,
        "the term was the last reference, so the destructor ran exactly once"
    );
    {
        let env = NifEnv::new(&mut host);
        // SAFETY: `env` is live; `term` is a readable word on the still-live host heap.
        assert_eq!(
            unsafe { get_resource(&env, term, ty) },
            (0, core::ptr::null_mut()),
            "the binding is gone, so the term no longer designates anything"
        );
    }
}

/// Two mints of ONE object answer two **distinct** terms that both resolve to it — the C's fresh
/// magic reference per call (measured on the oracle for `enif_make_resource`).
#[test]
fn two_mints_of_one_object_answer_two_distinct_terms() {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    static RUNS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn counting_dtor(_: *mut ErlNifEnv, _: *mut c_void) {
        RUNS.fetch_add(1, AtomicOrdering::Relaxed);
    }

    let mut host = RetainHost::new();
    let (ty, obj) = {
        let env = NifEnv::new(&mut host);
        let ty = open_type(&env, "select_term_twice_rt", counting_dtor);
        // SAFETY: `ty` came from `open_type`; every reference taken here is released below.
        let obj = unsafe { alloc_object(ty, 2) };
        (ty, obj)
    };
    let base = RUNS.load(AtomicOrdering::Relaxed);

    let first = resource_term(&mut host, obj).expect("the first mint succeeds");
    let second = resource_term(&mut host, obj).expect("the second mint succeeds");
    assert_ne!(
        first.raw(),
        second.raw(),
        "each mint is a fresh internal-reference block"
    );
    // SAFETY: both are internal-reference blocks `write_resource_ref` just built, so their id words
    // are readable.
    let (first_id, second_id) = unsafe {
        (
            prelude::internal_ref_id(first).expect("a reference id"),
            prelude::internal_ref_id(second).expect("a reference id"),
        )
    };
    assert_ne!(first_id, second_id, "each mint draws a fresh reference id");

    {
        let env = NifEnv::new(&mut host);
        for term in [first, second] {
            // SAFETY: `env` is live; `term` is one of the two mints; `ty` is the object's own type.
            assert_eq!(
                unsafe { get_resource(&env, term, ty) },
                (1, obj),
                "both terms designate the same object"
            );
        }
    }

    // Three references: the allocator's and the two terms'.
    let words = [first.raw(), second.raw()];
    assert_eq!(
        release_resource_terms(|candidate| !words.contains(&candidate.raw())),
        2,
        "both mints are released in one pass"
    );
    assert_eq!(
        RUNS.load(AtomicOrdering::Relaxed) - base,
        0,
        "the allocator's reference still holds the object"
    );
    // SAFETY: the allocator's reference is the last outstanding one, so this release destroys it.
    unsafe { enif_release_resource(obj) };
    assert_eq!(RUNS.load(AtomicOrdering::Relaxed) - base, 1);
}

/// Two consecutive `enif_make_resource` calls mint **distinct, strictly increasing** reference ids —
/// the C's fresh magic reference per call, and the monotonicity every local reference producer shares.
#[test]
fn two_consecutive_mints_draw_distinct_strictly_increasing_ids() {
    let mut host = RetainHost::new();
    let env = NifEnv::new(&mut host);
    let ty = open_type(&env, "increasing_ids_rt", silent_dtor);
    // SAFETY: `ty` came from `open_type`. The two minted terms keep their references (this test asserts
    // ids, not lifetimes, so leaving them outstanding is only a bounded leak); the allocator's own
    // reference is released at the end.
    let obj = unsafe { alloc_object(ty, 1) };
    // SAFETY: `env` is live; `obj` is a live payload pointer.
    let first = unsafe { make_resource_term(&env, obj) };
    // SAFETY: as above.
    let second = unsafe { make_resource_term(&env, obj) };

    // SAFETY: both are internal-reference blocks `write_resource_ref` just built, so their id words
    // are readable.
    let (first_id, second_id) = unsafe {
        (
            prelude::internal_ref_id(first).expect("a reference id"),
            prelude::internal_ref_id(second).expect("a reference id"),
        )
    };
    assert_ne!(
        first.raw(),
        second.raw(),
        "each mint is a fresh internal-reference block"
    );
    assert_ne!(first_id, second_id, "each mint draws a fresh reference id");
    // Strictly increasing, not merely distinct: the shared source is one monotone counter, so the
    // later mint's id is always the greater one even when another thread draws in between.
    assert!(
        second_id > first_id,
        "the second mint's id ({second_id}) must exceed the first's ({first_id})"
    );

    // SAFETY: the two minted terms still hold references, so the object outlives this release; this
    // drops the allocator's own reference.
    unsafe { enif_release_resource(obj) };
}

/// Mint one resource term and prove that **no other draw** of the shared counter happened across the
/// mint, so the minted id can be compared with its neighbouring draws *exactly*.
///
/// Returns `(minted_id, before1, before2, after)`, where `before1`/`before2` are draws of
/// [`beam_common::next_ref_id`] taken back to back immediately before the mint and `after` is the draw
/// taken immediately after it. When `after == before1 + 3` the window is **tight** — three draws happened between `before1`
/// and `after` and the mint accounts for the middle one — which is what makes `minted_id ==
/// before2 + 1` a proof of the id's *source* rather than a mere monotonicity observation.
///
/// `cargo test` runs this binary's unit tests on parallel threads and several other resource tests mint
/// too, so the window is retried (bounded) until it comes out tight. Each attempt mints a term whose
/// reference is left outstanding — a bounded leak of a few words, and the test heap (4096 words) holds
/// far more attempts than the retry budget needs in practice.
fn mint_in_tight_window(env: &NifEnv, obj: *mut c_void) -> (u64, u64, u64, u64) {
    // Bounded so a pathological scheduler cannot spin here (see the heap-budget note above): 256
    // attempts reserve at most 256 × `RESOURCE_REF_WORDS` words.
    for _ in 0..256 {
        let before1 = beam_common::next_ref_id();
        let before2 = beam_common::next_ref_id();
        // SAFETY: `env` is live and `obj` a live payload pointer (the caller's contract).
        let minted = unsafe { make_resource_term(env, obj) };
        let after = beam_common::next_ref_id();
        // SAFETY: `minted` is the internal-reference block the mint just built.
        let id = unsafe { prelude::internal_ref_id(minted) }.expect("a reference id");
        if after == before1 + 3 {
            return (id, before1, before2, after);
        }
    }
    panic!(
        "no interference-free draw window in 256 attempts — another thread drew from the shared \
         reference-id counter on every attempt"
    );
}

/// **The id's source, pinned exactly.** A minted resource id is the *next draw of the one shared local
/// reference-id space* ([`beam_common::next_ref_id`]) — the space `erlang:make_ref/0`, the NIF
/// `make_ref`, monitors, aliases, timers, `seq_trace` references and `beam_process`'s process table
/// draw from — and not a private counter's.
///
/// The proof is a tight draw window: `before2 == before1 + 1` and `after == before1 + 3` show that
/// nothing else drew between `before1` and `after`, so `minted == before2 + 1` says the mint consumed
/// exactly the next id of that one space. A private, independently seeded counter could not land on
/// `before2 + 1` in a tight window — which is the aliasing hazard this closes: with one space, a dead
/// resource term can never carry a *live* plain reference's id.
#[test]
fn a_minted_id_is_the_next_draw_of_the_shared_reference_id_space() {
    let mut host = RetainHost::new();
    let env = NifEnv::new(&mut host);
    let ty = open_type(&env, "shared_id_space_rt", silent_dtor);
    // SAFETY: `ty` came from `open_type`; the minted terms' references are left outstanding (a bounded
    // leak — this test asserts ids), and the allocator's own reference is released at the end.
    let obj = unsafe { alloc_object(ty, 1) };

    let (minted_id, before1, before2, after) = mint_in_tight_window(&env, obj);

    assert_eq!(before2, before1 + 1, "the two probe draws are contiguous");
    assert_eq!(
        after,
        before1 + 3,
        "the window is tight: the mint drew exactly one id"
    );
    assert_eq!(
        minted_id,
        before2 + 1,
        "the minted id is the very next draw of the shared reference-id space"
    );
    // The id is inside the block's id slot, read back the same way the writer bounded it (a negative
    // or out-of-range id is refused by `internal_ref_id` and by `narrow_ref_id` alike).
    assert!(
        narrow_ref_id(minted_id).is_some(),
        "the minted id fits the block's small-integer id slot"
    );

    // SAFETY: the minted term holds a reference, so the object outlives this release; this drops the
    // allocator's own reference.
    unsafe { enif_release_resource(obj) };
}

/// The checked narrowing that feeds the block's `make_small(id)` word: it accepts exactly the
/// small-integer id slot and **refuses** (never truncates, wraps or substitutes) everything past it.
///
/// The bound is `beam_term::MAX_SMALL` — read from `beam_term`, not transcribed — so the accepted set
/// coincides with what `beam_term`'s own checked encoder accepts, which is asserted here directly.
#[test]
fn narrow_ref_id_accepts_exactly_the_blocks_id_slot() {
    let max = beam_term::MAX_SMALL;
    assert_eq!(max, (1i64 << 59) - 1, "the 60-bit small-integer bound");

    for id in [0, 1, 2, max - 1, max] {
        assert_eq!(
            narrow_ref_id(id as u64),
            Some(id),
            "{id} is inside the block's id slot"
        );
        // Cross-check: `beam_term`'s own checked small-int encoder agrees it is representable.
        assert!(
            prelude::try_make_small(id).is_ok(),
            "beam_term accepts {id} as a small integer too"
        );
    }

    // One past the bound, the top of the `i64` range, and the whole upper half of the `u64` space
    // (where `i64::try_from` refuses first) are all typed refusals — never a wrapped id, which would
    // alias another reference.
    for id in [
        (max as u64) + 1,
        (max as u64) + 2,
        i64::MAX as u64,
        i64::MAX as u64 + 1,
        u64::MAX,
    ] {
        assert_eq!(narrow_ref_id(id), None, "{id} is past the id slot");
    }
    assert!(
        prelude::try_make_small(max + 1).is_err(),
        "beam_term refuses one past the bound as well — the two agree"
    );
}

/// The minting seam is reachable at both documented public paths —
/// `nif::enif::resources::resource_term` (where it is implemented) and `nif::enif::resource_term`
/// (the family re-export) — with one and the same signature, because the runtime implements its
/// caller side against them.
#[test]
fn the_minting_seam_is_publicly_reachable() {
    type Seam = fn(&mut dyn NifHost, *mut c_void) -> Option<Eterm>;
    let here: Seam = resource_term;
    let family: Seam = crate::nif::enif::resources::resource_term;
    let re_export: Seam = crate::nif::enif::resource_term;
    let _ = (here, family, re_export);
}
