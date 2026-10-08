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
