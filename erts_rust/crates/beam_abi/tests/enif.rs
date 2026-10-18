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

//! Integration tests for the `enif_*` host functions (the NIF C ABI surface, contract A.3).
//!
//! The functions are `#[no_mangle] pub unsafe extern "C"`, so they are called directly from Rust
//! through `beam_abi::nif::enif` — the only difference from a real NIF library is the calling
//! convention, which the ABI already fixes.
//!
//! A minimal [`common::TestHost`] (a `beam_alloc::Heap` + `beam_term` `AtomTable`) supplies the host
//! seam. Every call passes a real live env (`NifEnv::as_ptr`) exactly as the C side would.

mod common;

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::process::Command;

use beam_abi::nif::enif::*;
use beam_abi::nif::host::NifEnv;
use beam_abi::nif::loader::load;
use beam_abi::nif::term::{eterm_from_nif, nif_from_eterm, ERL_NIF_TERM};
use beam_abi::nif::types::ErlNifBinary;
use beam_term::prelude::{self, *};

use common::{compile_so, skip};

/// Intern `name` in the host and return the atom term.
fn atom(host: &mut common::TestHost, name: &str) -> Eterm {
    host.atom(name)
}

#[test]
fn make_int_round_trips_through_get_int() {
    let mut host = common::TestHost::new();
    let mut env = NifEnv::new(&mut host);
    // SAFETY: `env` is live for the whole test; the out-pointer is a live local.
    unsafe {
        let term = enif_make_int(env.as_ptr(), 5);
        assert_eq!(eterm_from_nif(term), make_small(5));
        let mut out = 0i32;
        assert_eq!(enif_get_int(env.as_ptr(), term, &mut out), 1);
        assert_eq!(out, 5);
        // A negative value round-trips too.
        let neg = enif_make_int(env.as_ptr(), -7);
        let mut out_neg = 0i32;
        assert_eq!(enif_get_int(env.as_ptr(), neg, &mut out_neg), 1);
        assert_eq!(out_neg, -7);
    }
}

#[test]
fn get_int_rejects_out_of_range_and_non_integers() {
    let mut host = common::TestHost::new();
    // Intern the atom *before* the env borrows the host (`NifEnv` holds the host mutably).
    let zzz = atom(&mut host, "zzz");
    let mut env = NifEnv::new(&mut host);
    // SAFETY: `env` is live; out-pointers are live locals.
    unsafe {
        // A value wider than `int` (but a perfectly good small integer) must be rejected.
        let big = nif_from_eterm(make_small(1i64 << 40));
        let mut out = 123i32;
        assert_eq!(enif_get_int(env.as_ptr(), big, &mut out), 0);
        assert_eq!(out, 123, "a rejected conversion leaves the out-param untouched");
        // `long`/`ulong` accept it.
        let mut lout = 0i64;
        assert_eq!(enif_get_long(env.as_ptr(), big, &mut lout), 1);
        assert_eq!(lout, 1i64 << 40);
        // Non-integers are rejected.
        let an_atom = nif_from_eterm(zzz);
        assert_eq!(enif_get_int(env.as_ptr(), an_atom, &mut out), 0);
        // A negative value is not a `ulong`.
        let mut uout = 0u64;
        assert_eq!(enif_get_ulong(env.as_ptr(), nif_from_eterm(make_small(-1)), &mut uout), 0);
        // Null out-pointer: rejected, no panic.
        assert_eq!(enif_get_int(env.as_ptr(), nif_from_eterm(make_small(1)), std::ptr::null_mut()), 0);
    }
}

#[test]
fn double_round_trip_and_non_finite_is_badarg() {
    let mut host = common::TestHost::new();
    // Intern the atom *before* the env borrows the host.
    let badarg = atom(&mut host, "badarg");
    let mut env = NifEnv::new(&mut host);
    // SAFETY: `env` is live; out-pointers are live locals.
    unsafe {
        let term = enif_make_double(env.as_ptr(), 1.5);
        assert!(is_float(eterm_from_nif(term)));
        let mut out = 0.0f64;
        assert_eq!(enif_get_double(env.as_ptr(), term, &mut out), 1);
        assert_eq!(out, 1.5);
        // NaN cannot be represented as a heap float by the constructor: it raises `badarg`.
        let nan = enif_make_double(env.as_ptr(), f64::NAN);
        assert_eq!(eterm_from_nif(nan), badarg);
        assert!(env.pending_exception().is_some());
        env.clear();
        // A plain integer is not a float.
        assert_eq!(enif_get_double(env.as_ptr(), nif_from_eterm(make_small(1)), &mut out), 0);
    }
}

#[test]
fn atoms_make_get_length_round_trip() {
    let mut host = common::TestHost::new();
    // Intern the atom *before* the env borrows the host.
    let hello = atom(&mut host, "hello");
    let mut env = NifEnv::new(&mut host);
    let name = std::ffi::CString::new("hello").unwrap();
    // SAFETY: `env` is live; `name` is a valid NUL-terminated string; buffers are live.
    unsafe {
        let term = enif_make_atom(env.as_ptr(), name.as_ptr());
        assert_eq!(eterm_from_nif(term), hello);
        assert_eq!(enif_is_atom(env.as_ptr(), term), 1);

        let mut buf = [0i8; 16];
        let written = enif_get_atom(
            env.as_ptr(),
            term,
            buf.as_mut_ptr(),
            buf.len() as u32,
            beam_abi::nif::types::ErlNifCharEncoding::Utf8,
        );
        assert_eq!(written, 6, "the return is the byte count including the NUL");
        let bytes: Vec<u8> = buf[..5].iter().map(|&b| b as u8).collect();
        assert_eq!(&bytes, b"hello");
        assert_eq!(buf[5], 0, "the name is NUL-terminated");

        let mut len = 0u32;
        assert_eq!(
            enif_get_atom_length(
                env.as_ptr(),
                term,
                &mut len,
                beam_abi::nif::types::ErlNifCharEncoding::Utf8
            ),
            1
        );
        assert_eq!(len, 5);
        // A buffer one byte too short for name+NUL is refused.
        let mut tight = [0i8; 5];
        assert_eq!(
            enif_get_atom(
                env.as_ptr(),
                term,
                tight.as_mut_ptr(),
                5,
                beam_abi::nif::types::ErlNifCharEncoding::Utf8
            ),
            0
        );
        // A non-atom is refused.
        assert_eq!(
            enif_get_atom_length(
                env.as_ptr(),
                nif_from_eterm(make_small(1)),
                &mut len,
                beam_abi::nif::types::ErlNifCharEncoding::Utf8
            ),
            0
        );
    }
}

#[test]
fn atom_len_and_encoding_validation() {
    let mut host = common::TestHost::new();
    // Intern the atom *before* the env borrows the host.
    let abc = atom(&mut host, "abc");
    let mut env = NifEnv::new(&mut host);
    // SAFETY: `env` is live; the byte slices are live.
    unsafe {
        let bytes = b"abc";
        let term = enif_make_atom_len(env.as_ptr(), bytes.as_ptr().cast(), bytes.len());
        assert_eq!(eterm_from_nif(term), abc);
        // A Latin-1 name that is not valid UTF-8 is still representable in Latin-1.
        let latin = [0xE5u8]; // "å" in Latin-1
        let term = enif_make_new_atom_len(
            env.as_ptr(),
            latin.as_ptr().cast(),
            1,
            std::ptr::null_mut(),
            beam_abi::nif::types::ErlNifCharEncoding::Utf8,
        );
        assert_eq!(term, 0, "a Latin-1 byte is not valid UTF-8");
    }
}

#[test]
fn tuples_from_array_and_back() {
    let mut host = common::TestHost::new();
    let mut env = NifEnv::new(&mut host);
    let arr = [make_small(1), make_small(2), make_small(3)].map(nif_from_eterm);
    // SAFETY: `env` is live; `arr` is a live 3-element array; out-params are live locals.
    unsafe {
        let term = enif_make_tuple_from_array(env.as_ptr(), arr.as_ptr(), 3);
        assert_eq!(enif_is_tuple(env.as_ptr(), term), 1);
        let mut arity = 0i32;
        let mut elements: *const ERL_NIF_TERM = std::ptr::null();
        assert_eq!(
            enif_get_tuple(env.as_ptr(), term, &mut arity, &mut elements),
            1
        );
        assert_eq!(arity, 3);
        assert!(!elements.is_null());
        for (i, expected) in arr.iter().enumerate() {
            // SAFETY: the C sets the array to the tuple's first element word; it has `arity` terms.
            assert_eq!(eterm_from_nif(*elements.add(i)), eterm_from_nif(*expected));
        }
        // The empty tuple is the shared zero-arity literal.
        let empty = enif_make_tuple_from_array(env.as_ptr(), std::ptr::null(), 0);
        assert_eq!(eterm_from_nif(empty), tuple0());
        // A non-tuple is refused.
        assert_eq!(
            enif_get_tuple(
                env.as_ptr(),
                nif_from_eterm(make_small(1)),
                &mut arity,
                &mut elements
            ),
            0
        );
    }
}

#[test]
fn lists_cells_length_and_reverse() {
    let mut host = common::TestHost::new();
    let mut env = NifEnv::new(&mut host);
    let arr = [make_small(1), make_small(2), make_small(3)].map(nif_from_eterm);
    // SAFETY: `env` is live; `arr` is live; out-params are live locals.
    unsafe {
        let list = enif_make_list_from_array(env.as_ptr(), arr.as_ptr(), 3);
        assert_eq!(enif_is_list(env.as_ptr(), list), 1);
        assert_eq!(enif_is_empty_list(env.as_ptr(), list), 0);

        let mut head = ERL_NIF_TERM::default();
        let mut tail = ERL_NIF_TERM::default();
        assert_eq!(enif_get_list_cell(env.as_ptr(), list, &mut head, &mut tail), 1);
        assert_eq!(eterm_from_nif(head), make_small(1));
        // The tail is `[2,3]`, itself a proper list.
        assert_eq!(enif_is_list(env.as_ptr(), tail), 1);
        // `[]` has no cell.
        assert_eq!(
            enif_get_list_cell(
                env.as_ptr(),
                nif_from_eterm(NIL),
                &mut head,
                &mut tail
            ),
            0
        );

        let mut len = 0u32;
        assert_eq!(enif_get_list_length(env.as_ptr(), list, &mut len), 1);
        assert_eq!(len, 3);
        assert_eq!(enif_get_list_length(env.as_ptr(), nif_from_eterm(NIL), &mut len), 1);
        assert_eq!(len, 0);
        // An improper list has no length.
        let improper = enif_make_list_cell(env.as_ptr(), nif_from_eterm(make_small(1)), nif_from_eterm(make_small(2)));
        assert_eq!(enif_get_list_length(env.as_ptr(), improper, &mut len), 0);

        let mut reversed = ERL_NIF_TERM::default();
        assert_eq!(enif_make_reverse_list(env.as_ptr(), list, &mut reversed), 1);
        let mut rhead = ERL_NIF_TERM::default();
        let mut rtail = ERL_NIF_TERM::default();
        assert_eq!(enif_get_list_cell(env.as_ptr(), reversed, &mut rhead, &mut rtail), 1);
        assert_eq!(eterm_from_nif(rhead), make_small(3));
        // A non-list cannot be reversed.
        assert_eq!(
            enif_make_reverse_list(env.as_ptr(), nif_from_eterm(make_small(1)), &mut reversed),
            0
        );
        // Null out-pointer: refused, no panic.
        assert_eq!(enif_make_reverse_list(env.as_ptr(), list, std::ptr::null_mut()), 0);
    }
}

#[test]
fn strings_lists_and_lengths() {
    let mut host = common::TestHost::new();
    let mut env = NifEnv::new(&mut host);
    let text = std::ffi::CString::new("hi").unwrap();
    // SAFETY: `env` is live; `text` is NUL-terminated; buffers are live.
    unsafe {
        let list = enif_make_string(env.as_ptr(), text.as_ptr(), beam_abi::nif::types::ErlNifCharEncoding::Utf8);
        let mut len = 0u32;
        assert_eq!(
            enif_get_string_length(env.as_ptr(), list, &mut len, beam_abi::nif::types::ErlNifCharEncoding::Utf8),
            1
        );
        assert_eq!(len, 2);
        let mut buf = [0i8; 8];
        let written = enif_get_string(
            env.as_ptr(),
            list,
            buf.as_mut_ptr(),
            buf.len() as u32,
            beam_abi::nif::types::ErlNifCharEncoding::Utf8,
        );
        assert_eq!(written, 3);
        assert_eq!(&buf[..2].iter().map(|&b| b as u8).collect::<Vec<_>>(), b"hi");
        // A one-byte buffer cannot hold "hi"+NUL: the C returns a negative count.
        let mut tiny = [0i8; 2];
        let truncated = enif_get_string(
            env.as_ptr(),
            list,
            tiny.as_mut_ptr(),
            2,
            beam_abi::nif::types::ErlNifCharEncoding::Utf8,
        );
        assert!(truncated < 0, "truncation is reported as a negative count");
        // A non-list is refused.
        assert_eq!(
            enif_get_string_length(
                env.as_ptr(),
                nif_from_eterm(make_small(1)),
                &mut len,
                beam_abi::nif::types::ErlNifCharEncoding::Utf8
            ),
            0
        );
    }
}

#[test]
fn binaries_alloc_inspect_make_and_release() {
    let mut host = common::TestHost::new();
    let mut env = NifEnv::new(&mut host);
    // SAFETY: `env` is live; `bin` is a live struct; the malloc'd payload is written through `data`.
    unsafe {
        let mut bin = ErlNifBinary::default();
        assert_eq!(enif_alloc_binary(4, &mut bin), 1);
        assert_eq!(bin.size, 4);
        assert!(!bin.data.is_null());
        core::ptr::copy_nonoverlapping(b"abcd".as_ptr(), bin.data, 4);

        // Turn it into a heap binary (this consumes `bin`).
        let term = enif_make_binary(env.as_ptr(), &mut bin);
        assert!(is_binary(eterm_from_nif(term)));
        assert!(bin.data.is_null(), "the owned buffer was consumed");

        // Inspecting it yields a zero-copy view over the heap payload.
        let mut view = ErlNifBinary::default();
        assert_eq!(enif_inspect_binary(env.as_ptr(), term, &mut view), 1);
        assert_eq!(view.size, 4);
        let bytes = core::slice::from_raw_parts(view.data, 4);
        assert_eq!(bytes, b"abcd");
        // Releasing an inspected view must NOT free the heap payload.
        enif_release_binary(&mut view);
        let mut again = ErlNifBinary::default();
        assert_eq!(enif_inspect_binary(env.as_ptr(), term, &mut again), 1);
        assert_eq!(core::slice::from_raw_parts(again.data, 4), b"abcd");

        // A non-binary is refused, and a null out-struct never panics.
        assert_eq!(
            enif_inspect_binary(env.as_ptr(), nif_from_eterm(make_small(1)), &mut again),
            0
        );
        assert_eq!(enif_inspect_binary(env.as_ptr(), term, std::ptr::null_mut()), 0);
        assert_eq!(enif_alloc_binary(4, std::ptr::null_mut()), 0);

        // `enif_make_new_binary` hands back a writable payload.
        let mut reserved = ERL_NIF_TERM::default();
        let payload = enif_make_new_binary(env.as_ptr(), 3, &mut reserved);
        assert!(!payload.is_null());
        core::ptr::copy_nonoverlapping(b"xyz".as_ptr(), payload, 3);
        assert!(is_binary(eterm_from_nif(reserved)));
        let mut view2 = ErlNifBinary::default();
        assert_eq!(enif_inspect_binary(env.as_ptr(), reserved, &mut view2), 1);
        assert_eq!(core::slice::from_raw_parts(view2.data, 3), b"xyz");
    }
}

#[test]
fn iolist_inspection_flattens_to_an_owned_buffer() {
    let mut host = common::TestHost::new();
    let mut env = NifEnv::new(&mut host);
    // SAFETY: `env` is live; `arr` is live; `bin` is a live struct.
    unsafe {
        // `[<<"ab">>, 99]` == `<<"ab">> ++ [99]` == `<<"abc">>` (the head is emitted first).
        let inner = {
            let bytes = b"ab";
            let term = enif_make_new_binary(env.as_ptr(), 2, std::ptr::null_mut());
            assert!(term.is_null());
            // Use a proper heap binary instead.
            let mut b = ErlNifBinary::default();
            assert_eq!(enif_alloc_binary(2, &mut b), 1);
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), b.data, 2);
            enif_make_binary(env.as_ptr(), &mut b)
        };
        let list = enif_make_list_cell(env.as_ptr(), nif_from_eterm(make_small(99)), nif_from_eterm(NIL));
        let list = enif_make_list_cell(env.as_ptr(), inner, list);
        let mut bin = ErlNifBinary::default();
        assert_eq!(enif_inspect_iolist_as_binary(env.as_ptr(), list, &mut bin), 1);
        assert_eq!(bin.size, 3);
        assert_eq!(core::slice::from_raw_parts(bin.data, 3), b"abc");
        // Owned: release frees the flattening buffer.
        enif_release_binary(&mut bin);

        // A bare byte integer is **not** an iolist: `erts_iolist_size` accepts only a list, a
        // bitstring or `nil` at the top level (`erl_iolist.c:150-233`), so `65` is refused even
        // though a byte is a perfectly good iolist *element*.
        assert_eq!(
            enif_inspect_iolist_as_binary(env.as_ptr(), nif_from_eterm(make_small(65)), &mut bin),
            0
        );
        // An integer is accepted only in the **head** position of a cons cell, so the improper tail
        // `[1|2]` is a type error (`erl_iolist.c:206-233`)...
        let improper = enif_make_list_cell(
            env.as_ptr(),
            nif_from_eterm(make_small(1)),
            nif_from_eterm(make_small(2)),
        );
        assert_eq!(enif_inspect_iolist_as_binary(env.as_ptr(), improper, &mut bin), 0);
        // ... and a small integer outside `0..=255` is not a byte even in head position.
        let wide = enif_make_list_cell(
            env.as_ptr(),
            nif_from_eterm(make_small(256)),
            nif_from_eterm(NIL),
        );
        assert_eq!(enif_inspect_iolist_as_binary(env.as_ptr(), wide, &mut bin), 0);
        // The same bytes in the head position of a proper list are accepted.
        let proper = enif_make_list_cell(
            env.as_ptr(),
            nif_from_eterm(make_small(1)),
            enif_make_list_cell(env.as_ptr(), nif_from_eterm(make_small(2)), nif_from_eterm(NIL)),
        );
        assert_eq!(enif_inspect_iolist_as_binary(env.as_ptr(), proper, &mut bin), 1);
        assert_eq!(core::slice::from_raw_parts(bin.data, bin.size), &[1u8, 2u8]);
        enif_release_binary(&mut bin);

        // A term that is neither a binary nor an iolist is refused.
        assert_eq!(
            enif_inspect_iolist_as_binary(env.as_ptr(), nif_from_eterm(make_small(999)), &mut bin),
            0
        );

        // `[]` is accepted with the C's shape: `size == 0` and a **non-NULL** dummy `data`
        // (`bin->data = (unsigned char *)&bin->data;`, `erl_nif.c:1392`).
        let mut empty = ErlNifBinary::default();
        assert_eq!(enif_inspect_iolist_as_binary(env.as_ptr(), nif_from_eterm(NIL), &mut empty), 1);
        assert_eq!(empty.size, 0);
        assert!(!empty.data.is_null(), "the C gives `[]` a dummy non-NULL `data`");
        // The dummy is an owned block here, so releasing the view stays well defined.
        enif_release_binary(&mut empty);
        assert!(empty.data.is_null() && empty.size == 0);
    }
}

#[test]
fn badarg_raise_pending_exception_and_clear() {
    let mut host = common::TestHost::new();
    // The `badarg` atom must be resolvable through the host.
    let badarg_term = host.atom("badarg");
    let mut env = NifEnv::new(&mut host);
    assert_eq!(env.pending_exception(), None);
    // SAFETY: `env` is live; out-params are live locals.
    unsafe {
        let raised = enif_make_badarg(env.as_ptr());
        assert_eq!(eterm_from_nif(raised), badarg_term);
        assert_eq!(env.pending_exception(), Some(badarg_term));

        let mut reason = ERL_NIF_TERM::default();
        assert_eq!(enif_has_pending_exception(env.as_ptr(), &mut reason), 1);
        assert_eq!(eterm_from_nif(reason), badarg_term);

        // The C `enif_clear_env` frees temporary objects / rewinds the heap top but leaves the
        // `exception_thrown` flag alone (`erl_nif.c:700-723`), so the pending reason survives it.
        enif_clear_env(env.as_ptr());
        assert_eq!(env.pending_exception(), Some(badarg_term));
        assert_eq!(enif_has_pending_exception(env.as_ptr(), &mut reason), 1);
        assert_eq!(eterm_from_nif(reason), badarg_term);
        // A null out-pointer is still fine: only the count matters.
        assert_eq!(enif_has_pending_exception(env.as_ptr(), std::ptr::null_mut()), 1);

        // `NifEnv::clear` is the Rust-only reset helper — deliberately *not* the C `enif_clear_env`.
        env.clear();
        assert_eq!(env.pending_exception(), None);
        assert_eq!(enif_has_pending_exception(env.as_ptr(), &mut reason), 0);
        // No pending exception: a null out-pointer is still fine.
        assert_eq!(enif_has_pending_exception(env.as_ptr(), std::ptr::null_mut()), 0);

        // An explicit reason round-trips.
        let reason_term = nif_from_eterm(make_small(42));
        let returned = enif_raise_exception(env.as_ptr(), reason_term);
        assert_eq!(returned, reason_term);
        let mut back = ERL_NIF_TERM::default();
        assert_eq!(enif_has_pending_exception(env.as_ptr(), &mut back), 1);
        assert_eq!(eterm_from_nif(back), make_small(42));
        enif_clear_env(env.as_ptr());
        assert_eq!(enif_has_pending_exception(env.as_ptr(), &mut back), 1);
        assert_eq!(eterm_from_nif(back), make_small(42));

        // Null env: the exception API degrades to a safe default, never a panic.
        assert_eq!(enif_has_pending_exception(std::ptr::null_mut(), &mut back), 0);
        enif_clear_env(std::ptr::null_mut());
        assert_eq!(eterm_from_nif(enif_raise_exception(std::ptr::null_mut(), reason_term)), make_small(42));
    }
}

#[test]
fn make_ref_without_a_host_hook_raises_badarg() {
    let mut host = common::TestHost::new();
    let badarg_term = host.atom("badarg");
    let mut env = NifEnv::new(&mut host);
    // SAFETY: `env` is live.
    unsafe {
        // `TestHost` uses the default `make_ref` (unsupported) → `badarg` + a pending exception.
        let term = enif_make_ref(env.as_ptr());
        assert_eq!(eterm_from_nif(term), badarg_term);
        assert_eq!(env.pending_exception(), Some(badarg_term));
    }
}

#[test]
fn identical_and_compare() {
    let one = nif_from_eterm(make_small(1));
    let one_again = nif_from_eterm(make_small(1));
    let two = nif_from_eterm(make_small(2));
    let nil = nif_from_eterm(NIL);
    // SAFETY: no pointers are involved; any terms are accepted.
    unsafe {
        assert_eq!(enif_is_identical(one, one_again), 1);
        assert_eq!(enif_is_identical(one, two), 0);
        assert_eq!(enif_compare(one, one_again), 0);
        assert_eq!(enif_compare(one, two), -1);
        assert_eq!(enif_compare(two, one), 1);
        // Numbers sort before atoms, which sort before `[]` (Erlang term order).
        let an_atom = nif_from_eterm(make_atom(beam_term::atom::Atom::from_raw(1)));
        assert_eq!(enif_compare(one, an_atom), -1);
        assert_eq!(enif_compare(an_atom, nil), -1);
    }
}

#[test]
fn malformed_input_never_panics() {
    let mut host = common::TestHost::new();
    // Intern the atom *before* the env borrows the host.
    let badarg = atom(&mut host, "badarg");
    let mut env = NifEnv::new(&mut host);
    let env_ptr = env.as_ptr();
    // SAFETY: every call below is deliberately handed garbage, nulls or wrong-kind terms. The
    // contract under test is *totality*: none of them may panic, and each returns a safe default.
    unsafe {
        // Null env everywhere.
        assert_eq!(enif_is_atom(std::ptr::null_mut(), nif_from_eterm(make_small(1))), 0);
        assert_eq!(enif_is_binary(std::ptr::null_mut(), nif_from_eterm(make_small(1))), 0);
        assert_eq!(eterm_from_nif(enif_make_int(std::ptr::null_mut(), 7)), make_small(7));
        assert_eq!(eterm_from_nif(enif_make_long(std::ptr::null_mut(), 1)), NIL);
        assert_eq!(eterm_from_nif(enif_make_atom(std::ptr::null_mut(), std::ptr::null())), NIL);
        assert_eq!(eterm_from_nif(enif_make_double(std::ptr::null_mut(), 0.0)), NIL);
        assert_eq!(eterm_from_nif(enif_make_ref(std::ptr::null_mut())), NIL);
        assert_eq!(eterm_from_nif(enif_make_new_map(std::ptr::null_mut())), NIL);
        assert_eq!(eterm_from_nif(enif_make_list_cell(std::ptr::null_mut(), nif_from_eterm(NIL), nif_from_eterm(NIL))), NIL);
        assert_eq!(enif_make_map_put(std::ptr::null_mut(), nif_from_eterm(NIL), nif_from_eterm(NIL), nif_from_eterm(NIL), std::ptr::null_mut()), 0);
        // A null out-pointer is refused (the length is returned through `*len`), no panic.
        assert_eq!(enif_get_list_length(std::ptr::null_mut(), nif_from_eterm(NIL), std::ptr::null_mut()), 0);

        // A garbage word (raw 0 = `THE_NON_VALUE` in release) must not crash the predicates.
        let garbage = ERL_NIF_TERM::from_raw(0);
        let _ = enif_is_atom(env_ptr, garbage);
        let _ = enif_is_binary(env_ptr, garbage);
        let _ = enif_is_list(env_ptr, garbage);
        let _ = enif_is_number(env_ptr, garbage);
        let _ = enif_is_tuple(env_ptr, garbage);
        let _ = enif_is_map(env_ptr, garbage);
        let _ = enif_term_type(env_ptr, garbage);
        let _ = enif_term_size(garbage);

        // Null strings / buffers.
        assert_eq!(enif_make_atom(env_ptr, std::ptr::null()), nif_from_eterm(badarg));
        env.clear();
        assert!(enif_make_string(env_ptr, std::ptr::null(), beam_abi::nif::types::ErlNifCharEncoding::Utf8).raw() > 0);
        // Null `size_t` out-pointer for a map size.
        assert_eq!(enif_get_map_size(env_ptr, nif_from_eterm(make_small(1)), std::ptr::null_mut()), 0);
        // Null tuple out-pointers.
        assert_eq!(
            enif_get_tuple(env_ptr, nif_from_eterm(make_small(1)), std::ptr::null_mut(), std::ptr::null_mut()),
            0
        );
    }
}

#[test]
fn alloc_free_realloc_round_trip() {
    // SAFETY: a plain malloc/free/realloc round-trip; the block is owned by this test.
    unsafe {
        let p = enif_alloc(64).cast::<u8>();
        assert!(!p.is_null());
        core::ptr::write_bytes(p, 0xAB, 64);
        let grown = enif_realloc(p.cast::<c_void>(), 128).cast::<u8>();
        assert!(!grown.is_null());
        assert_eq!(*grown, 0xAB, "realloc preserves the contents");
        enif_free(grown.cast::<c_void>());
        // Freeing null is a no-op.
        enif_free(std::ptr::null_mut());
    }
}

#[test]
fn maps_new_put_get_size_and_predicate() {
    let mut host = common::TestHost::new();
    let mut env = NifEnv::new(&mut host);
    // SAFETY: `env` is live; out-params are live locals.
    unsafe {
        let empty = enif_make_new_map(env.as_ptr());
        assert_eq!(enif_is_map(env.as_ptr(), empty), 1);
        let mut size: usize = 99;
        assert_eq!(enif_get_map_size(env.as_ptr(), empty, &mut size), 1);
        assert_eq!(size, 0);

        let mut put = ERL_NIF_TERM::default();
        assert_eq!(
            enif_make_map_put(env.as_ptr(), empty, nif_from_eterm(make_small(1)), nif_from_eterm(make_small(10)), &mut put),
            1
        );
        assert_eq!(enif_get_map_size(env.as_ptr(), put, &mut size), 1);
        assert_eq!(size, 1);

        let mut value = ERL_NIF_TERM::default();
        assert_eq!(
            enif_get_map_value(env.as_ptr(), put, nif_from_eterm(make_small(1)), &mut value),
            1
        );
        assert_eq!(eterm_from_nif(value), make_small(10));
        // An absent key fails.
        assert_eq!(
            enif_get_map_value(env.as_ptr(), put, nif_from_eterm(make_small(2)), &mut value),
            0
        );
        // Updating an absent key fails; updating a present one succeeds.
        assert_eq!(
            enif_make_map_update(env.as_ptr(), put, nif_from_eterm(make_small(2)), nif_from_eterm(make_small(20)), &mut value),
            0
        );
        assert_eq!(
            enif_make_map_update(env.as_ptr(), put, nif_from_eterm(make_small(1)), nif_from_eterm(make_small(11)), &mut value),
            1
        );
        let mut updated = ERL_NIF_TERM::default();
        assert_eq!(
            enif_get_map_value(env.as_ptr(), value, nif_from_eterm(make_small(1)), &mut updated),
            1
        );
        assert_eq!(eterm_from_nif(updated), make_small(11));

        // From arrays.
        let keys = [make_small(1), make_small(2)].map(nif_from_eterm);
        let values = [make_small(100), make_small(200)].map(nif_from_eterm);
        let mut built = ERL_NIF_TERM::default();
        assert_eq!(
            enif_make_map_from_arrays(
                env.as_ptr(),
                keys.as_ptr().cast_mut(),
                values.as_ptr().cast_mut(),
                2,
                &mut built
            ),
            1
        );
        assert_eq!(enif_get_map_size(env.as_ptr(), built, &mut size), 1);
        assert_eq!(size, 2);
        // A non-map is refused.
        assert_eq!(
            enif_make_map_put(env.as_ptr(), nif_from_eterm(make_small(1)), nif_from_eterm(make_small(1)), nif_from_eterm(make_small(1)), &mut put),
            0
        );
    }
}

#[test]
fn maps_remove_existing_missing_and_non_map() {
    let mut host = common::TestHost::new();
    let mut env = NifEnv::new(&mut host);
    // SAFETY: `env` is live for the whole test; every out-param is a live local.
    unsafe {
        // Build #{1 => 10, 2 => 20}.
        let empty = enif_make_new_map(env.as_ptr());
        let mut m = ERL_NIF_TERM::default();
        assert_eq!(
            enif_make_map_put(env.as_ptr(), empty, nif_from_eterm(make_small(1)), nif_from_eterm(make_small(10)), &mut m),
            1
        );
        assert_eq!(
            enif_make_map_put(env.as_ptr(), m, nif_from_eterm(make_small(2)), nif_from_eterm(make_small(20)), &mut m),
            1
        );

        // Removing a present key succeeds and yields the reduced map (`erl_nif.c:3759`).
        let mut removed = ERL_NIF_TERM::default();
        assert_eq!(
            enif_make_map_remove(env.as_ptr(), m, nif_from_eterm(make_small(1)), &mut removed),
            1
        );
        let mut size: usize = 99;
        assert_eq!(enif_get_map_size(env.as_ptr(), removed, &mut size), 1);
        assert_eq!(size, 1);
        let mut value = ERL_NIF_TERM::default();
        assert_eq!(
            enif_get_map_value(env.as_ptr(), removed, nif_from_eterm(make_small(2)), &mut value),
            1
        );
        assert_eq!(eterm_from_nif(value), make_small(20));
        assert_eq!(
            enif_get_map_value(env.as_ptr(), removed, nif_from_eterm(make_small(1)), &mut value),
            0
        );

        // An absent key is still a success: the C ignores `erts_maps_take`'s found/absent result
        // and returns 1 for any map, leaving `map_out` equal to `map_in`.
        let mut unchanged = ERL_NIF_TERM::default();
        assert_eq!(
            enif_make_map_remove(env.as_ptr(), m, nif_from_eterm(make_small(3)), &mut unchanged),
            1
        );
        assert_eq!(enif_get_map_size(env.as_ptr(), unchanged, &mut size), 1);
        assert_eq!(size, 2);

        // Non-maps fail (0) — the only `0` return — without raising or panicking.
        let mut out = ERL_NIF_TERM::default();
        assert_eq!(
            enif_make_map_remove(env.as_ptr(), nif_from_eterm(make_small(7)), nif_from_eterm(make_small(1)), &mut out),
            0
        );
        // Nil (`[]`) and a boxed non-map (a 2-tuple) are refused the same way.
        assert_eq!(
            enif_make_map_remove(env.as_ptr(), nif_from_eterm(NIL), nif_from_eterm(make_small(1)), &mut out),
            0
        );
        let pair = [make_small(1), make_small(2)].map(nif_from_eterm);
        let tuple = enif_make_tuple_from_array(env.as_ptr(), pair.as_ptr(), 2);
        assert_eq!(
            enif_make_map_remove(env.as_ptr(), tuple, nif_from_eterm(make_small(1)), &mut out),
            0
        );
        // A null env or null out-param never panics.
        assert_eq!(
            enif_make_map_remove(std::ptr::null_mut(), m, nif_from_eterm(make_small(1)), &mut out),
            0
        );
        assert_eq!(
            enif_make_map_remove(env.as_ptr(), m, nif_from_eterm(make_small(1)), std::ptr::null_mut()),
            0
        );
    }
}

#[test]
fn pid_port_and_term_type_helpers() {
    let mut host = common::TestHost::new();
    let mut env = NifEnv::new(&mut host);
    // SAFETY: `env` is live; out-structs are live locals.
    unsafe {
        let pid = nif_from_eterm(make_internal_pid(make_pid_data(1, 5)));
        assert_eq!(enif_is_pid(env.as_ptr(), pid), 1);
        let mut pid_out = beam_abi::nif::types::ErlNifPid::default();
        assert_eq!(enif_get_local_pid(env.as_ptr(), pid, &mut pid_out), 1);
        assert_eq!(eterm_from_nif(pid_out.pid), make_internal_pid(make_pid_data(1, 5)));
        // A non-pid is refused.
        assert_eq!(enif_get_local_pid(env.as_ptr(), nif_from_eterm(make_small(1)), &mut pid_out), 0);

        let port = nif_from_eterm(make_internal_port(make_port_data(9)));
        assert_eq!(enif_is_port(env.as_ptr(), port), 1);
        let mut port_out = beam_abi::nif::types::ErlNifPort::default();
        assert_eq!(enif_get_local_port(env.as_ptr(), port, &mut port_out), 1);

        enif_set_pid_undefined(&mut pid_out);
        assert_eq!(enif_is_pid_undefined(&pid_out), 1);
        assert_eq!(enif_is_pid_undefined(std::ptr::null()), 0);

        // `self/0` is unsupported by `TestHost` → NULL, no panic.
        let mut self_pid = beam_abi::nif::types::ErlNifPid::default();
        assert!(enif_self(env.as_ptr(), &mut self_pid).is_null());

        assert!(matches!(
            enif_term_type(env.as_ptr(), nif_from_eterm(make_small(1))),
            beam_abi::nif::types::ErlNifTermType::Integer
        ));
        assert!(matches!(
            enif_term_type(env.as_ptr(), nif_from_eterm(NIL)),
            beam_abi::nif::types::ErlNifTermType::List
        ));
        // Immediates occupy no heap: the C `size_object` is 0 words for a small int (copy.c:250).
        assert_eq!(enif_term_size(nif_from_eterm(make_small(1))), 0);
        // A boxed term is `arity + 1` heap words (`2-tuple` → 3 words).
        let pair = [make_small(1), make_small(2)].map(nif_from_eterm);
        let pair_term = enif_make_tuple_from_array(env.as_ptr(), pair.as_ptr(), 2);
        assert_eq!(enif_term_size(pair_term), 3 * core::mem::size_of::<ERL_NIF_TERM>());
    }
}

#[test]
fn populated_table_has_the_expected_shape() {
    let table = enif_api_table();
    // A representative implemented slot from each area.
    assert!(table.enif_alloc.is_some());
    assert!(table.enif_alloc_binary.is_some());
    assert!(table.enif_inspect_binary.is_some());
    assert!(table.enif_get_int.is_some());
    assert!(table.enif_make_int.is_some());
    assert!(table.enif_make_atom.is_some());
    assert!(table.enif_make_tuple_from_array.is_some());
    assert!(table.enif_make_list_from_array.is_some());
    assert!(table.enif_make_map_put.is_some());
    assert!(table.enif_make_map_update.is_some());
    assert!(table.enif_make_map_remove.is_some());
    assert!(table.enif_make_badarg.is_some());
    assert!(table.enif_raise_exception.is_some());
    assert!(table.enif_has_pending_exception.is_some());
    assert!(table.enif_clear_env.is_some());
    assert!(table.enif_compare.is_some());
    assert!(table.enif_is_identical.is_some());
    // Which slots are still `None` is deliberately NOT asserted here: it changes as families land,
    // and each family's own test pins its own slots. See `CONTEXT.md` for the current status.
    // The frozen shape is untouched by populating it.
    assert_eq!(
        core::mem::size_of::<beam_abi::nif::api::ErlNifApi>(),
        176 * 8
    );
}

// ---------------------------------------------------------------------------------------------
// The `enif_inspect_iolist_as_binary` shape rules, end to end through a real C NIF
// ---------------------------------------------------------------------------------------------
//
// `tests/fixtures/nif_iolist.c` is a real NIF — compiled with the system `cc` against the system
// `erl_nif.h`, `dlopen`ed through this crate's loader — whose `made/1` / `probe/1` report
// `{Accepted, Size, DataNonNull, Checksum}` for `enif_inspect_iolist_as_binary`. The *same* `.so` is
// then loaded by the **system** Erlang, so the shape rules are pinned by an oracle cross-check and
// not only by this crate's own reading of `erl_iolist.c` (see the fixture header for the ground
// truth). The refusal of a *bare byte integer* and of an improper-list tail is the regression these
// cases lock down.

/// The committed C fixture (a real NIF module — see its header for the A.3 rationale).
const IOLIST_FIXTURE_C: &str = include_str!("fixtures/nif_iolist.c");

/// The fixture's `ERL_NIF_INIT` name: the loader (and the oracle) require it to match the calling
/// module, so it doubles as the name of the Erlang module the oracle test generates.
const IOLIST_MODULE: &str = "test_nif_iolist";

/// `{Accepted, Size, DataNonNull, Checksum}` — the fixture's observation.
type Observation = [i64; 4];

/// A case whose term the **fixture itself** builds in C, selected by `made/1`.
///
/// Selector 1 is `enif_make_int(env, 65)` and selector 2 `enif_make_list_cell(env, 1, 2)` — the
/// recorded reproducers of the divergence. Every observation below is the **measured oracle value**
/// (system OTP 29.1.1 / erts-17.1); `the_iolist_shape_rules_agree_with_the_erl_oracle` re-measures
/// them on every run.
const MADE_CASES: &[(i64, &str, Observation)] = &[
    (1, "the bare byte integer 65", [0, -1, 0, -1]),
    (2, "the improper list [1|2]", [0, -1, 0, -1]),
    (3, "the non-byte head [256]", [0, -1, 0, -1]),
    (4, "the byte-less nested list [[]]", [1, 0, 1, 0]),
    (5, "the empty list []", [1, 0, 1, 0]),
    (6, "the deeply nested [[<<\"ab\">>,[99]]]", [1, 3, 1, 590]),
    (7, "the binary tail [1|<<\"xy\">>]", [1, 3, 1, 604]),
    (8, "the empty binary <<>>", [1, 0, 1, 0]),
    (9, "the plain binary <<\"xyz\">>", [1, 3, 1, 728]),
    (10, "the large nested iolist", [1, 1500, 1, 139_276_846]),
];

/// A case whose term the **caller** builds — the shapes the public `enif_*` API cannot construct (a
/// non-byte-aligned bitstring, an atom) plus a few worth pinning through the argument path too.
struct ProbeCase {
    /// The Erlang source text of the term (the oracle driver's `probe/1` argument).
    erl: &'static str,
    /// The same term, built through `beam_term`'s writers.
    build: fn(&mut NifEnv) -> Eterm,
    /// The measured oracle observation of that term.
    want: Observation,
}

/// Every probe case with its measured oracle observation:
///
/// * `[<<1:7>>]` — a bitstring whose bit size is not a multiple of 8 is a type error *inside* a list
///   (`erl_iolist.c:176-195`), unlike `[<<1:8>>]`, which is a one-byte element;
/// * `[65,66,67]` — the byte-in-head-position case that has always agreed;
/// * `[1|<<2>>]` — a byte-aligned bitstring in **tail** position is accepted (the post-loop check);
/// * `nope` — a non-iolist term.
const PROBE_CASES: &[ProbeCase] = &[
    ProbeCase { erl: "[<<1:7>>]", build: seven_bit_in_list, want: [0, -1, 0, -1] },
    ProbeCase { erl: "[<<1:8>>]", build: eight_bit_in_list, want: [1, 1, 1, 1] },
    ProbeCase { erl: "[65,66,67]", build: byte_list, want: [1, 3, 1, 398] },
    ProbeCase { erl: "[1|<<2>>]", build: byte_with_binary_tail, want: [1, 2, 1, 5] },
    ProbeCase { erl: "nope", build: atom_term, want: [0, -1, 0, -1] },
];

/// A byte-aligned heap binary of `bytes`.
fn heap_binary(heap: &mut beam_alloc::Heap, bytes: &[u8]) -> Eterm {
    // SAFETY: `heap_bits_size_words` sizes the `[hdr][size][data…]` block for these bits, and
    // `write_binary` writes every word of it before the term escapes.
    let hp = unsafe { heap.alloc_words(heap_bits_size_words(bytes.len() * 8)) }
        .expect("the test heap has room for the binary");
    // SAFETY: `hp` heads the reserved block; `bytes` provides the payload.
    unsafe { write_binary(hp, bytes) }.expect("a byte-aligned binary")
}

/// A heap bitstring of `n_bits` bits packed from `bytes` (MSB first, `<<…>>` semantics).
fn heap_bitstring(heap: &mut beam_alloc::Heap, bytes: &[u8], n_bits: usize) -> Eterm {
    // SAFETY: as `heap_binary`, for a bit count.
    let hp = unsafe { heap.alloc_words(heap_bits_size_words(n_bits)) }
        .expect("the test heap has room for the bitstring");
    // SAFETY: `hp` heads the reserved block and `bytes` holds the `n_bits` bits.
    unsafe { write_heap_bits(hp, bytes, n_bits) }.expect("a bitstring")
}

/// The cons cell `car :: cdr`.
fn cons(heap: &mut beam_alloc::Heap, car: Eterm, cdr: Eterm) -> Eterm {
    heap.cons(car, cdr).expect("the test heap has room for a cons cell")
}

/// `[<<1:7>>]` — seven bits (`0000001`, packed MSB-first into `0b0000_0010`).
fn seven_bit_in_list(env: &mut NifEnv) -> Eterm {
    let head = heap_bitstring(env.heap(), &[0b0000_0010], 7);
    cons(env.heap(), head, NIL)
}

/// `[<<1:8>>]` — the same value as a whole byte.
fn eight_bit_in_list(env: &mut NifEnv) -> Eterm {
    let head = heap_bitstring(env.heap(), &[1], 8);
    cons(env.heap(), head, NIL)
}

/// `[65,66,67]`.
fn byte_list(env: &mut NifEnv) -> Eterm {
    let tail = cons(env.heap(), make_small(67), NIL);
    let tail = cons(env.heap(), make_small(66), tail);
    cons(env.heap(), make_small(65), tail)
}

/// `[1|<<2>>]` — a byte followed by a byte-aligned binary tail.
fn byte_with_binary_tail(env: &mut NifEnv) -> Eterm {
    let tail = heap_binary(env.heap(), &[2]);
    cons(env.heap(), make_small(1), tail)
}

/// The atom `nope` — not an iolist at all.
fn atom_term(env: &mut NifEnv) -> Eterm {
    env.host().intern_atom("nope").expect("the test host interns atoms")
}

/// The small integers of a fixture result tuple, in order (the fixture returns integers only).
fn result_ints(t: Eterm) -> Vec<i64> {
    // SAFETY: `t` is checked to be a tuple before its header is dereferenced.
    assert!(unsafe { prelude::is_tuple(t) }, "the fixture returns a tuple; got {t:?}");
    // SAFETY: `t` is a well-formed tuple produced by the fixture.
    let n = unsafe { prelude::tuple_size(t) };
    (0..n)
        .map(|i| {
            // SAFETY: `i < n == tuple_size(t)`, so the element is in bounds.
            let elem = unsafe { prelude::tuple_element(t, i) };
            assert!(prelude::is_small(elem), "every result element is a small integer");
            prelude::signed_value(elem)
        })
        .collect()
}

/// Compile the committed fixture; `None` (after a loud skip) when the toolchain is absent.
fn compile_iolist_fixture() -> Option<PathBuf> {
    compile_so(IOLIST_MODULE, IOLIST_FIXTURE_C)
}

/// The path `load` takes — the compiled `.so` **without** its suffix, which the loader appends
/// itself — or `None` (after a loud skip) when it is not valid UTF-8.
fn iolist_load_path(so: &Path) -> Option<String> {
    match so.with_extension("").to_str() {
        Some(path) => Some(path.to_string()),
        None => {
            skip("the compiled .so path is not valid UTF-8");
            None
        }
    }
}

#[test]
fn the_iolist_shape_rules_hold_through_a_real_c_nif() {
    // Touching the table keeps the `enif_*`-carrying codegen unit in the link, so the `.so`'s
    // imports resolve and `dlopen(RTLD_NOW)` can succeed.
    std::hint::black_box(enif_api_table());

    let Some(so) = compile_iolist_fixture() else { return };
    let Some(path) = iolist_load_path(&so) else { return };

    let mut host = common::TestHost::new();
    // The host outlives the library (drop order), which the loaded fixture requires.
    let lib = load(&path, IOLIST_MODULE, &mut host, NIL)
        .expect("a real NIF library must load: its enif_* imports resolve from the test binary");
    let mut env = NifEnv::new(&mut host);

    for (selector, what, want) in MADE_CASES {
        // Each case builds its own terms, so rewinding the arena in between keeps the small test
        // heap enough for the 1500-byte large case (each result is read out before the rewind).
        env.heap().reset();
        let got = lib
            .call(&mut env, "made", 1, &[make_small(*selector)])
            .unwrap_or_else(|err| panic!("made/{selector} ({what}) must be callable: {err:?}"));
        assert_eq!(result_ints(got), want.to_vec(), "made/{selector} ({what})");
    }

    for case in PROBE_CASES {
        env.heap().reset();
        let arg = (case.build)(&mut env);
        let got = lib
            .call(&mut env, "probe", 1, &[arg])
            .unwrap_or_else(|err| panic!("probe/1 of {} must be callable: {err:?}", case.erl));
        assert_eq!(result_ints(got), case.want.to_vec(), "probe/1 of {}", case.erl);
    }
}

// ---------------------------------------------------------------------------------------------
// The system-Erlang oracle (skipped loudly when the toolchain is absent)
// ---------------------------------------------------------------------------------------------

/// Whether `cmd` can be spawned with `args` at all (the oracle-toolchain probe; a non-zero exit is
/// fine — e.g. `erlc -h` — only a missing binary counts as absent).
fn runnable(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd).args(args).output().is_ok()
}

/// Render one expected observation as the Erlang text `~w` prints for it.
fn erl_tuple(values: &[i64]) -> String {
    let inner: Vec<String> = values.iter().map(i64::to_string).collect();
    format!("{{{}}}", inner.join(","))
}

/// Load the *same* fixture into the **system** Erlang and print every observation, one per line.
///
/// Returns `None` (after a loud skip) when `erl`/`erlc` are unavailable or the generated module
/// cannot be built; a *runtime* failure of the Erlang side is a hard error, because that would mean
/// the fixture is not a valid NIF rather than that the toolchain is missing.
fn oracle_print(so: &Path) -> Option<Vec<String>> {
    if !runnable("erl", &["-noshell", "-eval", "halt(0)."]) || !runnable("erlc", &["-h"]) {
        skip("the erl/erlc oracle toolchain is not available");
        return None;
    }
    let dir = so.parent()?.to_path_buf();
    // `erlang:load_nif/2` appends the shared-object suffix itself, so hand it the path WITHOUT `.so`.
    let stem = so.file_stem()?.to_str()?;
    let nif_path = dir.join(stem);
    let nif_path = nif_path.to_str()?;

    // The oracle module: the stub bodies are replaced by the NIFs on `load_nif`. The compiler marks
    // both functions `nif_start` because `init/0` calls `load_nif/2`, which is what makes the
    // replacement possible.
    let mut erl = format!("-module({IOLIST_MODULE}).\n-export([made/1, probe/1]).\n-on_load(init/0).\n\n");
    erl.push_str(&format!("init() ->\n    erlang:load_nif(\"{nif_path}\", 0).\n\n"));
    erl.push_str("made(_) -> erlang:nif_error(nif_not_loaded).\n");
    erl.push_str("probe(_) -> erlang:nif_error(nif_not_loaded).\n");

    let erl_file = dir.join(format!("{IOLIST_MODULE}.erl"));
    std::fs::write(&erl_file, erl).expect("the oracle module is writable");

    let compiled = Command::new("erlc")
        .arg("-o")
        .arg(&dir)
        .arg(&erl_file)
        .output()
        .expect("erlc is runnable (probed above)");
    assert!(
        compiled.status.success(),
        "erlc failed:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );

    let mut expr = String::new();
    for (selector, _, _) in MADE_CASES {
        expr.push_str(&format!("io:format(\"~w~n\", [{IOLIST_MODULE}:made({selector})]),\n"));
    }
    for case in PROBE_CASES {
        expr.push_str(&format!("io:format(\"~w~n\", [{IOLIST_MODULE}:probe({})]),\n", case.erl));
    }
    expr.push_str("halt(0).\n");

    let out = Command::new("erl")
        .arg("-noshell")
        .arg("-pa")
        .arg(&dir)
        .arg("-eval")
        .arg(&expr)
        .output()
        .expect("erl is runnable (probed above)");
    assert!(
        out.status.success(),
        "the oracle run failed (exit {:?}):\n--- stdout ---\n{}\n--- stderr ---\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

#[test]
fn the_iolist_shape_rules_agree_with_the_erl_oracle() {
    let Some(so) = compile_iolist_fixture() else { return };
    let Some(lines) = oracle_print(&so) else { return };

    let expected: Vec<String> = MADE_CASES
        .iter()
        .map(|(_, _, want)| erl_tuple(want))
        .chain(PROBE_CASES.iter().map(|case| erl_tuple(&case.want)))
        .collect();
    assert_eq!(
        lines, expected,
        "the system Erlang and this crate must agree on every pinned iolist observation"
    );
}
