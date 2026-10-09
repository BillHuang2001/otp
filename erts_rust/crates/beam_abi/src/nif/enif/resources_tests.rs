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
