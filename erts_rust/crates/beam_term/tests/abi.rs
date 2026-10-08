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

//! NIF-ABI pins (`tests/abi.rs`).
//!
//! `Eterm` is a `#[repr(transparent)]` newtype over `usize`, so it is layout- and
//! pass-identical to one machine word and therefore to a NIF's `ERL_NIF_TERM`
//! (workspace `CONTEXT.md` §2 / Part I §A.3). `beam_abi` will `transmute` between
//! them, so this file pins the guarantee: same size, same alignment, `Copy`, a
//! transparent raw round-trip and a `NIL` default. It consumes the crate only via
//! its public API (`beam_term::prelude::*`).

use beam_term::prelude::*;

// Compile-time proof of the one-word ABI (fails to compile if either changes).
const _: () = assert!(std::mem::size_of::<Eterm>() == std::mem::size_of::<usize>());
const _: () = assert!(std::mem::align_of::<Eterm>() == std::mem::align_of::<usize>());
const _: () = assert!(std::mem::size_of::<[Eterm; 4]>() == std::mem::size_of::<[usize; 4]>());

fn assert_copy<T: Copy>() {}
fn assert_clone<T: Clone>() {}
fn assert_default<T: Default>() {}
fn assert_send_sync<T: Send + Sync>() {}
fn assert_eq_hash<T: Eq + core::hash::Hash>() {}
fn assert_debug<T: std::fmt::Debug>() {}

#[test]
fn eterm_is_exactly_one_word() {
    assert_eq!(std::mem::size_of::<Eterm>(), std::mem::size_of::<usize>());
    assert_eq!(std::mem::align_of::<Eterm>(), std::mem::align_of::<usize>());
    assert_eq!(
        std::mem::size_of::<[Eterm; 4]>(),
        std::mem::size_of::<[usize; 4]>()
    );
    assert_eq!(std::mem::size_of::<Eterm>(), std::mem::size_of::<*const ()>());
}

#[test]
fn eterm_implements_the_word_traits() {
    assert_copy::<Eterm>();
    assert_clone::<Eterm>();
    assert_default::<Eterm>();
    assert_send_sync::<Eterm>();
    assert_eq_hash::<Eterm>();
    assert_debug::<Eterm>();
}

#[test]
fn repr_transparent_raw_round_trip() {
    for raw in [0usize, 1, 0xF, 0x3B, 0x40, 0x1000, usize::MAX] {
        let t = Eterm::from_raw(raw);
        assert_eq!(t.raw(), raw);
        // Round-trip through the public API is the identity.
        assert_eq!(Eterm::from_raw(t.raw()), t);
    }
}

#[test]
fn transmute_identity_with_usize() {
    // Equal size/alignment make this transmute well-formed; the round-trip shows
    // the transparent layout carries the identical bytes either way.
    for raw in [0usize, 0xF, 0x3B, 0xDEAD_BEEF, usize::MAX] {
        // SAFETY: `Eterm` is `#[repr(transparent)]` over `usize` and the two types
        // have identical size and alignment (asserted at compile time above), so
        // the bit patterns are interchangeable.
        let t: Eterm = unsafe { std::mem::transmute::<usize, Eterm>(raw) };
        assert_eq!(t.raw(), raw);
        // SAFETY: as above, the reverse direction.
        let back: usize = unsafe { std::mem::transmute::<Eterm, usize>(t) };
        assert_eq!(back, raw);
    }
}

#[test]
fn default_is_nil() {
    assert_eq!(Eterm::default(), NIL);
    assert_eq!(Eterm::default(), Eterm::nil());
    assert_eq!(Eterm::default().raw(), 0x3B);
    // `Copy` semantics: a copy is independent and equal.
    let a = Eterm::default();
    let b = a;
    assert_eq!(a, b);
}
