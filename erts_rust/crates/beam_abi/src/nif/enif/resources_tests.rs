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
fn open_type(env: &mut NifEnv, name: &str, dtor: ErlNifResourceDtor) -> *mut ErlNifResourceType {
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
unsafe fn make_resource_term(env: &mut NifEnv, obj: *mut c_void) -> Eterm {
    // SAFETY: forwarded from the caller's contract.
    t(unsafe { enif_make_resource(env.as_ptr(), obj) })
}

/// `enif_get_resource(env, term, ty, &mut objp)` as the C's `1`/`0` return code.
///
/// # Safety
///
/// `env` must be live and `ty` a type from [`open_type`] (or null).
unsafe fn get_resource(env: &mut NifEnv, term: Eterm, ty: *mut ErlNifResourceType) -> (i32, *mut c_void) {
    let mut objp: *mut c_void = core::ptr::null_mut();
    // SAFETY: `env` is live; `term` is a term; `ty` comes from `open_type`; `objp` is a live local.
    let matched = unsafe { enif_get_resource(env.as_ptr(), n(term), ty, &mut objp) };
    (matched, objp)
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
    let mut env = NifEnv::new(&mut host);
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
    let mut env = NifEnv::new(&mut host);
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
    let mut env = NifEnv::new(&mut host);
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
    let mut env = NifEnv::new(&mut host);
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
    let mut env = NifEnv::new(&mut host);
    let ty = open_type(&mut env, "prune_count_rt", counting_dtor);
    let base = RUNS.load(AtomicOrdering::Relaxed);

    // SAFETY: `ty` came from `open_type`; the object is released exactly once below (after the term
    // took its own reference).
    let obj = unsafe { alloc_object(ty, 4) };
    // SAFETY: `obj` is a live 4-byte payload; the first byte is in bounds.
    unsafe { *(obj as *mut u8) = 7 };
    // SAFETY: `env` is live; `obj` is a live payload pointer.
    let term = unsafe { make_resource_term(&mut env, obj) };
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
    let mut env = NifEnv::new(&mut host);
    let ty = open_type(&mut env, "prune_binding_rt", silent_dtor);

    // SAFETY: `ty` came from `open_type`; the term holds the only remaining reference.
    let obj = unsafe { alloc_object(ty, 1) };
    // SAFETY: `env` is live; `obj` is a live payload pointer.
    let term = unsafe { make_resource_term(&mut env, obj) };
    // SAFETY: the term keeps the object alive from here on.
    unsafe { enif_release_resource(obj) };

    // Before the prune the term designates the object (the oracle's behaviour).
    // SAFETY: `env` is live; `ty` is the object's type.
    let (matched, got) = unsafe { get_resource(&mut env, term, ty) };
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
    let mut env = NifEnv::new(&mut host);
    let ty = open_type(&mut env, "prune_live_user_ref_rt", counting_dtor);
    let base = RUNS.load(AtomicOrdering::Relaxed);

    // alloc (1) + keep (2) + term (3).
    // SAFETY: `ty` came from `open_type`; every reference taken here is released below exactly once.
    let obj = unsafe { alloc_object(ty, 4) };
    // SAFETY: `obj` is a live 4-byte payload; the first byte is in bounds.
    unsafe { *(obj as *mut u8) = 0x5a };
    // SAFETY: `obj` is a live payload pointer with an outstanding reference.
    unsafe { enif_keep_resource(obj) };
    // SAFETY: `env` is live; `obj` is a live payload pointer.
    let term = unsafe { make_resource_term(&mut env, obj) };

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
    let mut env = NifEnv::new(&mut host);
    let ty = open_type(&mut env, "prune_reachable_rt", counting_dtor);
    let base = RUNS.load(AtomicOrdering::Relaxed);

    // SAFETY: `ty` came from `open_type`; the term holds the only remaining reference.
    let obj = unsafe { alloc_object(ty, 2) };
    // SAFETY: `env` is live; `obj` is a live payload pointer.
    let term = unsafe { make_resource_term(&mut env, obj) };
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
    let (matched, got) = unsafe { get_resource(&mut env, term, ty) };
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
    let mut env = NifEnv::new(&mut host);
    let ty = open_type(&mut env, "prune_two_terms_rt", counting_dtor);
    let base = RUNS.load(AtomicOrdering::Relaxed);

    // alloc (1) + two terms (2, 3).
    // SAFETY: `ty` came from `open_type`; the single allocator reference is released once below.
    let obj = unsafe { alloc_object(ty, 4) };
    // SAFETY: `obj` is a live 4-byte payload; the first byte is in bounds.
    unsafe { *(obj as *mut u8) = 0x21 };
    // SAFETY: `env` is live; `obj` is a live payload pointer.
    let first = unsafe { make_resource_term(&mut env, obj) };
    // SAFETY: as above — a second, distinct term for the same object (the oracle mints a fresh magic
    // ref per call).
    let second = unsafe { make_resource_term(&mut env, obj) };
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
    let (matched, got) = unsafe { get_resource(&mut env, second, ty) };
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
    let mut env = NifEnv::new(&mut host);
    let ty = open_type(&mut env, "prune_snapshot_rt", counting_dtor);
    let base = RUNS.load(AtomicOrdering::Relaxed);

    let before = resource_terms();
    // SAFETY: `ty` came from `open_type`; the allocator's reference is released below.
    let obj = unsafe { alloc_object(ty, 4) };
    // SAFETY: `env` is live; `obj` is a live payload pointer.
    let term = unsafe { make_resource_term(&mut env, obj) };
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
    // Every live term is called reachable here, so nothing is released (and the shared registry of
    // this test binary is left untouched).
    assert_eq!(crate::nif::enif::resources::release_resource_terms(|_| true), 0);
    assert_eq!(crate::nif::enif::release_resource_terms(|_| true), 0);
    assert_eq!(
        crate::nif::enif::resources::resource_terms(),
        crate::nif::enif::resource_terms(),
        "both paths designate the same live set"
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
        let mut env = NifEnv::new(&mut host);
        let ty = open_type(&mut env, "prune_concurrent_rt", counting_dtor);
        // SAFETY: `ty` came from `open_type`; the term holds the only remaining reference.
        let obj = unsafe { alloc_object(ty, 1) };
        // SAFETY: `env` is live; `obj` is a live payload pointer.
        let term = unsafe { make_resource_term(&mut env, obj) };
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
