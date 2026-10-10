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

//! `enif_inspect_binary` / `enif_inspect_iolist_as_binary` on a **byte-sized bitstring whose
//! start is not byte-aligned** (contract A.3).
//!
//! The C never refuses such a view: `enif_inspect_binary` (`erl_nif.c:1343-1382`) returns `1`
//! whenever `TAIL_BITS(size) == 0`, and for a non-zero `BIT_OFFSET(offset)` it materialises the
//! *real* bits with `erts_copy_bits_fwd(base, offset, bin->data, 0, size)` (a bit copy, not a
//! shifted `memcpy`). `enif_inspect_iolist_as_binary` copies the same way through
//! `erts_iolist_to_buf`. Only a **non-byte-sized** bitstring (`TAIL_BITS(size) != 0`) is refused.
//!
//! The oracle (OTP 29.1.1 / erts-17.1) was measured for `<<_:5, Y:100/binary, _/bits>>` of the
//! 160-byte binary `<<0,1,2,…,159>>` — a genuine off-heap sub-binary, bit offset 5 — and reports
//! `1` with `size = 100` and the bytes `<<0,32,64,96,128,160,192,225,…>>`.
//!
//! The source terms are built directly with `beam_term`'s heap-agnostic writers over the test
//! host's `beam_alloc::Heap`, in the frozen 8-word `[BinRef][ErlSubBits]` shape
//! `write_refc_binary` writes (the same construction `beam_gc/tests/refc_binary_copy.rs` uses).
//! `beam_abi`'s own logic is exercised through the public `enif_*` entry points only.

mod common;

use beam_abi::nif::enif::{
    enif_inspect_binary, enif_inspect_iolist_as_binary, enif_make_list_cell, enif_release_binary,
};
use beam_abi::nif::host::NifEnv;
use beam_abi::nif::term::nif_from_eterm;
use beam_abi::nif::types::ErlNifBinary;
use beam_alloc::RefcBinary;
use beam_term::prelude::{self, *};

/// The oracle's measured payload: `<<0,1,2,…,159>>`. 160 bytes exceeds the 64-byte on-heap
/// binary limit, so the real ERTS stores it off-heap and the window is a real sub-binary.
fn payload() -> Vec<u8> {
    (0..160u16).map(|i| i as u8).collect()
}

/// Bits `offset .. offset + size_bits` of `bytes`, reassembled byte by byte (most significant bit
/// first) — an independent per-bit implementation of the C's `erts_copy_bits_fwd`, used as the
/// expected value so the crate's shift-based copy is checked against something else.
fn window_bytes(bytes: &[u8], offset: usize, size_bits: usize) -> Vec<u8> {
    (0..size_bits / 8)
        .map(|k| {
            let mut byte = 0u8;
            for j in 0..8 {
                let bit = offset + k * 8 + j;
                byte |= ((bytes[bit / 8] >> (7 - bit % 8)) & 1) << (7 - j);
            }
            byte
        })
        .collect()
}

/// Writes the 8-word `[BinRef][ErlSubBits]` pair describing bits `offset .. offset + size_bits`
/// of `payload` onto the env's heap and returns the root (`ErlSubBits`) term.
///
/// # Safety
///
/// `env` must be live (its heap allocatable); `payload` must stay alive and unmoved while the
/// returned term (and any view of it) is used; `offset + size_bits` must not exceed the payload's
/// bit count.
unsafe fn window_term(
    env: &mut NifEnv,
    payload: &RefcBinary,
    offset: usize,
    size_bits: usize,
) -> Eterm {
    // SAFETY: `ERL_REFC_BITS_SIZE` (8) exclusively-owned words are reserved at `hp` — the
    // `write_refc_binary` contract; `payload.data_ptr()` is a live, word-aligned pointer to
    // `payload.len() * 8` bits of off-heap storage (kept alive by `payload`), and `null` is the
    // (empty) MSO/off-heap chain link.
    let hp = unsafe { env.heap().alloc_words(ERL_REFC_BITS_SIZE) }.expect("8 words for the pair");
    // SAFETY: as above; the window `offset .. offset + size_bits` lies inside the payload's bits.
    unsafe {
        write_refc_binary(
            hp,
            payload.data_ptr() as *mut Eterm,
            core::ptr::null_mut(),
            payload.data_ptr(),
            offset,
            size_bits,
        )
    }
}

/// A byte-sized window at a **non-byte-aligned** offset is a binary view of the *real* bits: the
/// C copies them, so `size = 100` and the bytes match the oracle's measurement.
#[test]
fn inspect_binary_copies_the_bits_of_a_bit_offset_window() {
    let mut host = common::TestHost::new();
    let mut env = NifEnv::new(&mut host);
    let bytes = payload();
    let bin = RefcBinary::new(&bytes).expect("an off-heap payload");
    // SAFETY: `env` is live for the whole test and `bin` outlives every term built from it.
    unsafe {
        let term = window_term(&mut env, &bin, 5, 800);
        // 800 % 8 == 0, so this *is* a binary — only its start (bit 5) is unaligned.
        assert!(is_binary(term), "a byte-sized window is a binary");
        let mut view = ErlNifBinary::default();
        assert_eq!(
            enif_inspect_binary(env.as_ptr(), nif_from_eterm(term), &mut view),
            1,
            "the C accepts a byte-sized window regardless of its bit offset"
        );
        assert_eq!(view.size, 100);
        assert!(!view.data.is_null());
        let got = core::slice::from_raw_parts(view.data, view.size);
        assert_eq!(got, &window_bytes(&bytes, 5, 800)[..]);
        // The oracle's measured prefix for `<<_:5, Y:100/binary, _/bits>>`.
        assert_eq!(&got[..8], &[0, 32, 64, 96, 128, 160, 192, 225]);

        // This view is *owned* (the C's env-owned tmp copy; this crate has no env-local
        // allocator, so `enif_release_binary` frees it) — and it is a copy, never the payload.
        assert!(view.__spare__[0].is_null(), "the unaligned copy is owned");
        assert!(
            !core::ptr::eq(view.data, bin.data_ptr()),
            "the copy is a fresh buffer"
        );
        enif_release_binary(&mut view);
        assert!(
            view.data.is_null() && view.size == 0,
            "release frees the copy exactly once"
        );
    }
}

/// The already-agreeing shape: a byte-aligned window stays a **zero-copy** view into the payload.
#[test]
fn inspect_binary_keeps_the_zero_copy_view_for_a_byte_aligned_window() {
    let mut host = common::TestHost::new();
    let mut env = NifEnv::new(&mut host);
    let bytes = payload();
    let bin = RefcBinary::new(&bytes).expect("an off-heap payload");
    // SAFETY: `env` is live for the whole test and `bin` outlives every term built from it.
    unsafe {
        let term = window_term(&mut env, &bin, 16, 800);
        let mut view = ErlNifBinary::default();
        assert_eq!(
            enif_inspect_binary(env.as_ptr(), nif_from_eterm(term), &mut view),
            1
        );
        assert_eq!(view.size, 100);
        // SAFETY: `bin.data_ptr()` is live for `bin.len()` bytes; the window starts 2 bytes in.
        assert_eq!(
            view.data,
            bin.data_ptr().add(2),
            "an aligned window borrows the payload"
        );
        assert!(
            !view.__spare__[0].is_null(),
            "an aligned view is inspected (zero-copy), not owned"
        );
        assert_eq!(
            core::slice::from_raw_parts(view.data, view.size),
            &window_bytes(&bytes, 16, 800)[..]
        );
        // Releasing an inspected view must not touch the payload.
        enif_release_binary(&mut view);
        assert_eq!(&bin.as_bytes()[2..102], &window_bytes(&bytes, 16, 800)[..]);
    }
}

/// The same bit-offset window inside an iolist — as a list head (an element) and as the list tail
/// (an object) — is flattened to its real bytes, exactly as `erts_iolist_to_buf` does.
#[test]
fn inspect_iolist_flattens_a_bit_offset_window() {
    let mut host = common::TestHost::new();
    let mut env = NifEnv::new(&mut host);
    let bytes = payload();
    let bin = RefcBinary::new(&bytes).expect("an off-heap payload");
    let want = window_bytes(&bytes, 5, 800);
    // SAFETY: `env` is live for the whole test; `bin` outlives every term built from it; the
    // out-structs are live locals and are released exactly once each.
    unsafe {
        let term = window_term(&mut env, &bin, 5, 800);

        // `[<window>]` — the window in head (element) position.
        let list = enif_make_list_cell(env.as_ptr(), nif_from_eterm(term), nif_from_eterm(NIL));
        let mut bin_view = ErlNifBinary::default();
        assert_eq!(
            enif_inspect_iolist_as_binary(env.as_ptr(), list, &mut bin_view),
            1,
            "a byte-sized bit-offset window is a valid iolist element"
        );
        assert_eq!(bin_view.size, 100);
        assert_eq!(
            core::slice::from_raw_parts(bin_view.data, bin_view.size),
            &want[..]
        );
        // A flattened iolist is an owned buffer (the same contract as before this shape worked).
        assert!(bin_view.__spare__[0].is_null());
        enif_release_binary(&mut bin_view);

        // `[1 | <window>]` — the window in tail (object) position: same bytes, after the byte 1.
        let tail = enif_make_list_cell(
            env.as_ptr(),
            nif_from_eterm(make_small(1)),
            nif_from_eterm(term),
        );
        let mut tail_view = ErlNifBinary::default();
        assert_eq!(
            enif_inspect_iolist_as_binary(env.as_ptr(), tail, &mut tail_view),
            1
        );
        assert_eq!(tail_view.size, 101);
        let got = core::slice::from_raw_parts(tail_view.data, tail_view.size);
        assert_eq!(got[0], 1);
        assert_eq!(&got[1..], &want[..]);
        enif_release_binary(&mut tail_view);
    }
}

/// The shape rules the C does enforce are unchanged: a **non-byte-sized** bitstring is refused,
/// both as a binary view and inside an iolist (the C fixture's `[<<1:7>>]` case).
#[test]
fn non_byte_sized_bitstrings_are_still_refused() {
    let mut host = common::TestHost::new();
    let mut env = NifEnv::new(&mut host);
    // SAFETY: `env` is live for the whole test; the out-struct is a live local.
    unsafe {
        // `<<1:3>>` — three bits stored in one byte (`0b0010_0000`).
        let hp = env
            .heap()
            .alloc_words(prelude::heap_bits_size_words(3))
            .expect("the test heap has room for the bitstring");
        let three_bits = prelude::write_heap_bits(hp, &[0b0010_0000], 3).expect("a bitstring");
        assert!(!is_binary(three_bits), "three bits is not a binary");

        let mut view = ErlNifBinary::default();
        assert_eq!(
            enif_inspect_binary(env.as_ptr(), nif_from_eterm(three_bits), &mut view),
            0,
            "a non-byte-sized bitstring has no byte view (`TAIL_BITS(size) != 0`)"
        );
        let list = enif_make_list_cell(
            env.as_ptr(),
            nif_from_eterm(three_bits),
            nif_from_eterm(NIL),
        );
        assert_eq!(
            enif_inspect_iolist_as_binary(env.as_ptr(), list, &mut view),
            0,
            "a non-byte-sized element is an iolist type error"
        );
        // The `[]` shape is untouched: `size == 0` with a non-NULL (here owned) dummy `data`.
        let mut empty = ErlNifBinary::default();
        assert_eq!(
            enif_inspect_iolist_as_binary(env.as_ptr(), nif_from_eterm(NIL), &mut empty),
            1
        );
        assert_eq!(empty.size, 0);
        assert!(!empty.data.is_null());
        enif_release_binary(&mut empty);
    }
}

/// A guard against the test helper itself drifting: the per-bit expectation must agree with the
/// crate's own public bit reader for the same window, and the oracle prefix must be reproducible.
#[test]
fn the_window_expectation_matches_the_public_bit_reader() {
    let mut host = common::TestHost::new();
    let mut env = NifEnv::new(&mut host);
    let bytes = payload();
    let bin = RefcBinary::new(&bytes).expect("an off-heap payload");
    // SAFETY: `env` is live and `bin` outlives the term built from it.
    unsafe {
        let term = window_term(&mut env, &bin, 5, 800);
        let want = window_bytes(&bytes, 5, 800);
        for (k, reference) in want.iter().enumerate() {
            let mut built = 0u8;
            for j in 0..8 {
                built |= (prelude::bit_at(term, k * 8 + j) as u8) << (7 - j);
            }
            assert_eq!(&built, reference, "byte {k}");
        }
    }
}

/// The byte expectation itself is pinned by the **system `erl`** through its bit syntax (no C
/// fixture needed): `<<_:5, Y:100/binary, _/bits>>` and the byte-aligned `<<_:16, Y:100/binary,
/// _/bits>>` of the same 160-byte binary, printed by `~w`, must equal [`window_bytes`] for the
/// matching windows. Skips loudly when `erl` is unavailable.
#[test]
fn the_expected_bits_agree_with_the_erl_oracle() {
    use std::process::Command;

    let probe = Command::new("erl")
        .args(["-noshell", "-eval", "halt(0)."])
        .output();
    if probe.is_err() {
        common::skip("the `erl` oracle toolchain is not available");
        return;
    }

    // The windows the Rust side builds with `write_refc_binary(base, offset, size)`.
    let expr = "Bin = list_to_binary(lists:seq(0,159)), \
                <<_:5, U:100/binary, _/bits>> = Bin, \
                <<_:16, A:100/binary, _/bits>> = Bin, \
                io:format(\"~w~n~w~n\", [U, A]), halt(0).";
    let out = Command::new("erl")
        .args(["-noshell", "-eval", expr])
        .output()
        .expect("erl is runnable (probed above)");
    assert!(
        out.status.success(),
        "the oracle run failed (exit {:?}):\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );

    let bytes = payload();
    let render = |b: &[u8]| {
        format!(
            "<<{}>>",
            b.iter().map(u8::to_string).collect::<Vec<_>>().join(",")
        )
    };
    let lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    assert_eq!(
        lines,
        vec![
            render(&window_bytes(&bytes, 5, 800)),
            render(&window_bytes(&bytes, 16, 800)),
        ],
        "the system Erlang's bit syntax and this test's expectation must agree"
    );
}
