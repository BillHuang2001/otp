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

//! `enif_make_sub_binary` on **any** bitstring source (contract A.3).
//!
//! The C (`erl_nif.c:1759-1788`) adds `NBITS(pos)` to the source's *bit* offset and returns
//! `NBITS(size)` bits:
//!
//! ```c
//! ERTS_GET_BITSTRING_REF(bin_term, br_flags, br, base, source_offset, source_size);
//! ...
//! return erts_build_sub_bitstring(&hp, br_flags, br, base, source_offset + NBITS(pos), NBITS(size));
//! ```
//!
//! so `pos`/`size` are byte counts applied to the source *value*'s bit string, the source may be a
//! non-byte-aligned and/or non-byte-sized bitstring, and the result is always `NBITS(size)` (hence
//! byte-sized) bits. `erts_build_sub_bitstring` (`erl_bits.c:2635-2670`) then splits on
//! `ERL_ONHEAP_BITS_LIMIT` (512 bits): at or below it the bits are **copied** into a heap bitstring,
//! above it an `ErlSubBits` **window** over the source's storage is built — which is what keeps a
//! sub-window of an off-heap binary a window instead of a fresh heap blob.
//!
//! The `ASSERT`s bounding `pos`/`size` are compiled out of the release oracle, so the oracle accepts
//! an out-of-range `pos` whenever `size == 0` (nothing is read) and answers `<<>>`. The oracle
//! (OTP 29.1.1 / erts-17.1) was measured for exactly those rows — including `MisBin`, a 16-bit
//! window at bit offset 3 of `<<1,2,3,4>>`, whose `{1,1}` request yields `<<16>>`.
//!
//! Sources are built with `beam_term`'s heap-agnostic writers over the test host's
//! `beam_alloc::Heap`, in the frozen 8-word `[BinRef][ErlSubBits]` shape `write_refc_binary` writes
//! (the same construction `nif_bitslice.rs` and `beam_gc/tests/refc_binary_copy.rs` use). The
//! crate's own logic is driven through the public `enif_*` entry points only.

mod common;

use beam_abi::nif::enif::{
    enif_inspect_binary, enif_is_identical, enif_make_sub_binary, enif_release_binary,
};
use beam_abi::nif::host::NifEnv;
use beam_abi::nif::term::{eterm_from_nif, nif_from_eterm, ERL_NIF_TERM};
use beam_abi::nif::types::ErlNifBinary;
use beam_alloc::RefcBinary;
use beam_term::prelude::{self, *};

// ---------------------------------------------------------------------------------------------
// Fixtures and independent expectations
// ---------------------------------------------------------------------------------------------

/// `<<1,2,3,4>>` — the 4-byte payload of the oracle's `MisBin` row.
fn small_payload() -> Vec<u8> {
    vec![1, 2, 3, 4]
}

/// `0, 1, 2, …` — the payloads of the oracle's `Big200` / `Big160` rows.
fn counting_payload(n: u16) -> Vec<u8> {
    (0..n).map(|i| i as u8).collect()
}

/// Bits `offset .. offset + size_bits` of `bytes`, reassembled byte by byte (most significant bit
/// first) — an independent per-bit expectation, not a call into the crate under test.
/// `nif_bitslice.rs` pins the same routine against the system `erl`'s bit syntax.
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

/// Writes the 8-word `[BinRef][ErlSubBits]` pair describing bits `offset .. offset + size_bits` of
/// `payload` onto the env's heap and returns the root (`ErlSubBits`) term.
///
/// # Safety
///
/// `env` must be live (its heap allocatable); `payload` must stay alive and unmoved while the
/// returned term (and any view of it) is used, and `offset + size_bits` must not exceed its bits.
unsafe fn window_term(
    env: &mut NifEnv,
    payload: &RefcBinary,
    offset: usize,
    size_bits: usize,
) -> Eterm {
    // SAFETY: `ERL_REFC_BITS_SIZE` (8) exclusively-owned words are reserved at `hp` — the
    // `write_refc_binary` contract; `payload.data_ptr()` is a live, word-aligned pointer to the
    // payload's bits (kept alive by `payload`) and `null` is the empty off-heap chain link.
    let hp = unsafe { env.heap().alloc_words(ERL_REFC_BITS_SIZE) }.expect("8 words for the pair");
    // SAFETY: as above; the window lies inside the payload's bits.
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

/// An on-heap bitstring (`ErlHeapBits`) holding `n_bits` bits of `bytes` — an *independently built*
/// expected value, never the crate's own result.
///
/// # Safety
///
/// `env` must be live; `bytes` must hold at least `ceil(n_bits / 8)` bytes.
unsafe fn heap_bits_term(env: &mut NifEnv, bytes: &[u8], n_bits: usize) -> Eterm {
    // SAFETY: `heap_bits_size_words` sizes the `[hdr][size][data…]` block for `n_bits` bits.
    let hp = unsafe { env.heap().alloc_words(prelude::heap_bits_size_words(n_bits)) }
        .expect("the test heap has room for the bitstring");
    // SAFETY: `hp` heads the reserved block and `bytes` provides the payload.
    unsafe { prelude::write_heap_bits(hp, bytes, n_bits) }.expect("a well-sized bitstring")
}

/// `enif_make_sub_binary(env, term, pos, size)`.
///
/// # Safety
///
/// `env` must be live and `term` any term.
unsafe fn sub(env: &mut NifEnv, term: Eterm, pos: usize, size: usize) -> ERL_NIF_TERM {
    let ptr = env.as_ptr();
    // SAFETY: `ptr` is the live env's C-facing pointer; the term is a live term word.
    unsafe { enif_make_sub_binary(ptr, nif_from_eterm(term), pos, size) }
}

/// The bytes of a byte-sized bitstring term, read back through the A.3 inspection API (the C's
/// `enif_inspect_binary`, which returns `1` for any byte-sized bitstring).
///
/// # Safety
///
/// `env` must be live and `term` a live bitstring.
unsafe fn inspect(env: &mut NifEnv, term: ERL_NIF_TERM) -> Vec<u8> {
    let mut view = ErlNifBinary::default();
    let ptr = env.as_ptr();
    // SAFETY: `ptr` is the live env's pointer and `view` is a live local.
    assert_eq!(
        unsafe { enif_inspect_binary(ptr, term, &mut view) },
        1,
        "an `enif_make_sub_binary` result is always byte-sized"
    );
    // SAFETY: `enif_inspect_binary` just filled `view` in with `size` readable bytes.
    let bytes = unsafe { core::slice::from_raw_parts(view.data, view.size) }.to_vec();
    // SAFETY: `view` is the live struct the inspection just filled in (owned or inspected per its
    // own marker; releasing it is the C's contract either way).
    unsafe { enif_release_binary(&mut view) };
    bytes
}

/// Whether a bitstring term is the `ErlSubBits` (window) representation rather than an on-heap
/// `ErlHeapBits` — the split `erts_build_sub_bitstring` makes at 512 bits.
///
/// # Safety
///
/// `term` must be a live boxed bitstring.
unsafe fn is_sub_bits(term: ERL_NIF_TERM) -> bool {
    let t = eterm_from_nif(term);
    // SAFETY: the caller guarantees a live boxed bitstring, so its header word is live.
    unsafe { *boxed_val(t) == HEADER_SUB_BITS }
}

/// `enif_is_identical(a, b)` — `true` when the two live terms are the same term.
///
/// # Safety
///
/// Both arguments must be live terms.
unsafe fn identical(a: ERL_NIF_TERM, b: ERL_NIF_TERM) -> bool {
    // SAFETY: both are live terms; `enif_is_identical` takes no env and no pointer out-parameter.
    (unsafe { enif_is_identical(a, b) }) == 1
}

/// The `badarg` atom term of `env` — the C's `enif_make_badarg` answer to a refused request.
fn badarg(env: &mut NifEnv) -> ERL_NIF_TERM {
    let atom = env
        .host()
        .intern_atom("badarg")
        .expect("the host interns atoms");
    nif_from_eterm(atom)
}

/// The atom `name` interned on `env` (a non-bitstring source for the refusal cases).
fn atom_term(env: &mut NifEnv, name: &str) -> Eterm {
    env.host().intern_atom(name).expect("the host interns atoms")
}

/// Assert `term` is the empty bitstring `<<>>`, identical to an independently built empty one.
///
/// # Safety
///
/// `env` must be live and `term` a live term.
unsafe fn assert_empty(env: &mut NifEnv, term: ERL_NIF_TERM, what: &str) {
    let t = eterm_from_nif(term);
    // SAFETY: `term` is a live term.
    assert!(
        unsafe { prelude::is_bitstring(t) },
        "{what}: the result must be a bitstring"
    );
    // SAFETY: as above — a live bitstring term.
    assert_eq!(
        unsafe { prelude::bitstring_size(t) },
        0,
        "{what}: `<<>>` has no bits"
    );
    assert!(
        unsafe { prelude::is_binary(t) },
        "{what}: the empty bitstring is byte-sized"
    );
    // SAFETY: `env` is live and `term` a live byte-sized bitstring.
    assert!(unsafe { inspect(env, term) }.is_empty(), "{what}: no bytes");
    // SAFETY: `env` is live; an independently built empty bitstring.
    let want = nif_from_eterm(unsafe { heap_bits_term(env, &[], 0) });
    // SAFETY: both are live terms.
    assert!(
        unsafe { identical(term, want) },
        "{what}: must be identical to an independently built `<<>>`"
    );
}

/// Assert the bytes of `term` equal `want` and that `term` is identical to an independently built
/// bitstring holding them.
///
/// # Safety
///
/// `env` must be live and `term` a live byte-sized bitstring.
unsafe fn assert_bits(env: &mut NifEnv, term: ERL_NIF_TERM, want: &[u8], what: &str) {
    // SAFETY: `env` is live and `term` a live bitstring.
    let got = unsafe { inspect(env, term) };
    assert_eq!(got, want, "{what}");
    // SAFETY: `env` is live; `want` holds exactly the `want.len() * 8` bits the block needs.
    let expected = nif_from_eterm(unsafe { heap_bits_term(env, want, want.len() * 8) });
    // SAFETY: both are live terms.
    assert!(
        unsafe { identical(term, expected) },
        "{what}: the result must be identical to an independently built bitstring"
    );
}

/// `enif_make_sub_binary(env, term, pos, size)` asserted to be the `size`-byte window `want`.
///
/// # Safety
///
/// `env` must be live and `term` a live bitstring.
unsafe fn assert_sub(env: &mut NifEnv, term: Eterm, pos: usize, size: usize, want: &[u8], what: &str) {
    let got = unsafe { sub(env, term, pos, size) };
    unsafe { assert_bits(env, got, want, what) };
}

/// As [`assert_sub`], for a request that must answer `<<>>`.
///
/// # Safety
///
/// `env` must be live and `term` a live bitstring.
unsafe fn assert_sub_empty(env: &mut NifEnv, term: Eterm, pos: usize, size: usize, what: &str) {
    let got = unsafe { sub(env, term, pos, size) };
    unsafe { assert_empty(env, got, what) };
}

/// As [`assert_sub`], for a request that must be refused with `badarg`.
///
/// # Safety
///
/// `env` must be live and `term` any term.
unsafe fn assert_sub_badarg(env: &mut NifEnv, term: Eterm, pos: usize, size: usize, what: &str) {
    let got = unsafe { sub(env, term, pos, size) };
    let want = badarg(env);
    // SAFETY: both are live terms.
    assert!(
        unsafe { identical(got, want) },
        "{what}: a refused request must raise `badarg`"
    );
}

// ---------------------------------------------------------------------------------------------
// The measured oracle rows
// ---------------------------------------------------------------------------------------------

/// A source with a **non-zero bit offset** is windowed by *bits*: `pos`/`size` are added to the
/// source's bit offset, so `enif_make_sub_binary(MisBin, 1, 1)` is bits 11..19 of the payload
/// `<<1,2,3,4>>` — the oracle's measured `<<16>>` (refusing it was the bug).
#[test]
fn a_bit_offset_source_windows_by_bits() {
    let mut host = common::TestHost::new();
    let mut env = NifEnv::new(&mut host);
    let payload = small_payload();
    let bin = RefcBinary::new(&payload).expect("an off-heap payload");
    // SAFETY: `env` is live for the whole test, `bin` outlives every term built from it, and the
    // windows below lie inside its 32 bits.
    unsafe {
        // `MisBin = <<_:3, X:2/binary, _/bitstring>>` of `<<1,2,3,4>>`: bit offset 3, 16 bits.
        let mis = window_term(&mut env, &bin, 3, 16);
        assert!(is_bitstring(mis), "the source is a bitstring");
        assert_eq!(prelude::bitstring_size(mis), 16);
        assert!(is_binary(mis), "a 16-bit window is byte-sized");

        // (1, 1) — bits 11..19 of the payload. Measured oracle: `<<16>>`.
        assert_sub(&mut env, mis, 1, 1, &[16], "MisBin (1, 1)");
        assert_eq!(
            window_bytes(&payload, 11, 8),
            vec![16],
            "the oracle's `<<16>>` is the independent expectation"
        );

        // (0, 2) — the whole 16-bit source, still a bit window (the source offset is kept).
        assert_sub(&mut env, mis, 0, 2, &window_bytes(&payload, 3, 16), "MisBin (0, 2)");
        // Both `pos` boundaries with `size == 0`: the last byte and one past it (`total + 1`) are
        // accepted, and so is a nonsensical `pos` (measured; the C's bound ASSERT is off).
        for pos in [0usize, 1, 2, 3, 9, usize::MAX] {
            assert_sub_empty(&mut env, mis, pos, 0, &format!("MisBin ({pos}, 0)"));
        }
        // A `size > 0` window that runs past the source's last bit is refused.
        assert_sub_badarg(&mut env, mis, 1, 2, "MisBin (1, 2)");
        assert_sub_badarg(&mut env, mis, 2, 1, "MisBin (2, 1)");
    }
}

/// The measured table's remaining rows, verbatim: `<<1:3>>` (a **non-byte-sized** source),
/// `<<1,2,3,4>>` (a 32-bit heap bitstring) and `Big200` (a 200-byte off-heap binary).
#[test]
fn the_measured_rows_agree() {
    let mut host = common::TestHost::new();
    let mut env = NifEnv::new(&mut host);
    let payload = small_payload();
    let big = counting_payload(200);
    let big_bin = RefcBinary::new(&big).expect("an off-heap payload");
    // SAFETY: `env` is live for the whole test; both payloads outlive every term built from them.
    unsafe {
        // `<<1:3>>` — three bits in one byte (`0b0010_0000`), not a binary.
        let three_bits = heap_bits_term(&mut env, &[0b0010_0000], 3);
        assert!(!is_binary(three_bits), "three bits is not byte-sized");
        assert_sub_empty(&mut env, three_bits, 0, 0, "`<<1:3>>` (0, 0)");
        // Any `pos` with `size == 0` is accepted for a bit-sized source too (measured).
        assert_sub_empty(&mut env, three_bits, 1, 0, "`<<1:3>>` (1, 0)");
        // …but an 8-bit window does not fit in 3 bits (the C reads 5 bits past the source).
        assert_sub_badarg(&mut env, three_bits, 0, 1, "`<<1:3>>` (0, 1)");

        // `<<1,2,3,4>>` — a 32-bit heap bitstring.
        let four = heap_bits_term(&mut env, &payload, 32);
        assert_sub(&mut env, four, 0, 4, &payload, "`<<1,2,3,4>>` (0, 4) — the whole binary");
        assert_sub(&mut env, four, 3, 1, &[4], "`<<1,2,3,4>>` (3, 1) — the last byte");
        assert_sub_empty(&mut env, four, 4, 0, "`<<1,2,3,4>>` (4, 0)");
        assert_sub_empty(&mut env, four, 0, 0, "`<<1,2,3,4>>` (0, 0)");
        // `pos == total_bytes + 1` with `size == 0` is accepted (measured).
        assert_sub_empty(&mut env, four, 5, 0, "`<<1,2,3,4>>` (5, 0)");

        // `Big200` — the off-heap 200-byte binary (a byte-aligned whole-binary source).
        let big200 = window_term(&mut env, &big_bin, 0, 200 * 8);
        assert_sub(
            &mut env,
            big200,
            3,
            100,
            &window_bytes(&big, 24, 800),
            "Big200 (3, 100)",
        );
        assert_sub(
            &mut env,
            big200,
            199,
            1,
            &window_bytes(&big, 199 * 8, 8),
            "Big200 (199, 1)",
        );
        assert_sub_empty(&mut env, big200, 200, 0, "Big200 (200, 0)");
        assert_sub_badarg(&mut env, big200, 200, 1, "Big200 (200, 1)");
        assert_sub_badarg(&mut env, big200, 199, 2, "Big200 (199, 2)");
    }
}

/// A **byte-sized** source whose start is not byte-aligned (`<<_:5, W:100/binary, _/bits>>` of a
/// 160-byte payload): a window above 512 bits stays an `ErlSubBits` window over the payload, a
/// window at or below it is copied into a heap bitstring — `erts_build_sub_bitstring`'s split.
#[test]
fn a_byte_sized_bit_offset_source_keeps_the_refc_window_shape() {
    let mut host = common::TestHost::new();
    let mut env = NifEnv::new(&mut host);
    let payload = counting_payload(160);
    let bin = RefcBinary::new(&payload).expect("an off-heap payload");
    // SAFETY: `env` is live for the whole test and `bin` outlives every term built from it.
    unsafe {
        // The 100-byte window at bit offset 5 — the same source `nif_bitslice.rs` measures.
        let w = window_term(&mut env, &bin, 5, 800);
        assert!(is_bitstring(w), "the source is a bitstring");
        assert!(is_binary(w), "100 bytes is byte-sized");

        // A 100-byte (800-bit) window: an `ErlSubBits` *window* onto the payload's storage, at the
        // absolute bit offset `source_offset + NBITS(pos)` — never a fresh heap blob.
        let big = sub(&mut env, w, 0, 100);
        assert!(is_sub_bits(big), "a >512-bit window stays a sub-bits window");
        // SAFETY: `big` is a live bitstring (just checked).
        let (base, offset, size) = prelude::bitstring_parts(eterm_from_nif(big));
        assert_eq!(base as *mut u8, bin.data_ptr(), "the window borrows the payload");
        assert_eq!((offset, size), (5, 800), "source offset + pos * 8");
        assert_bits(&mut env, big, &window_bytes(&payload, 5, 800), "W (0, 100)");

        // A byte-aligned *boundary* window of the same source, still above 512 bits: 65 bytes from
        // source byte 3 => bits 29..549 (a window again, at a different absolute offset).
        let big_aligned = sub(&mut env, w, 3, 65);
        assert!(is_sub_bits(big_aligned));
        // SAFETY: `big_aligned` is a live bitstring (just checked).
        let (_, offset, size) = prelude::bitstring_parts(eterm_from_nif(big_aligned));
        assert_eq!((offset, size), (5 + 24, 520));
        assert_bits(
            &mut env,
            big_aligned,
            &window_bytes(&payload, 29, 520),
            "W (3, 65)",
        );

        // At or below 512 bits the bits are **copied** into a heap bitstring (the C's heap arm) —
        // the result is no longer a view of the payload. `(0, 64)` is the exact 512-bit boundary,
        // `(90, 10)` the far end of the source.
        for (pos, size) in [(0usize, 10usize), (0, 64), (2, 4), (90, 10)] {
            let small = sub(&mut env, w, pos, size);
            assert!(!is_sub_bits(small), "W ({pos}, {size}) is copied");
            assert_bits(
                &mut env,
                small,
                &window_bytes(&payload, 5 + pos * 8, size * 8),
                &format!("W ({pos}, {size})"),
            );
        }
    }
}

/// The genuine out-of-range / undefined requests stay refused (the C's `ASSERT`s are compiled out
/// there, so it reads out of bounds or faults — this crate refuses instead), and a source that is
/// not a bitstring at all is refused too (the oracle SIGSEGVs).
#[test]
fn out_of_range_and_non_bitstrings_are_refused() {
    let mut host = common::TestHost::new();
    let mut env = NifEnv::new(&mut host);
    let payload = small_payload();
    let bin = RefcBinary::new(&payload).expect("an off-heap payload");
    // SAFETY: `env` is live for the whole test; `bin` outlives the terms built from it.
    unsafe {
        let four = heap_bits_term(&mut env, &payload, 32);
        // One byte (or more) past the end with `size > 0`, plus the overflowing byte counts.
        for (pos, size) in [
            (0usize, 5usize),
            (4, 1),
            (1, 4),
            (3, 2),
            (5, 1),
            (usize::MAX, 1),
            (0, usize::MAX),
            (usize::MAX / 8, 2),
        ] {
            assert_sub_badarg(
                &mut env,
                four,
                pos,
                size,
                &format!("`<<1,2,3,4>>` ({pos}, {size})"),
            );
        }

        // A non-bitstring source: an atom (with and without a window), an integer and `[]`.
        let ok = atom_term(&mut env, "ok");
        assert_sub_badarg(&mut env, ok, 0, 0, "the atom `ok` (0, 0)");
        assert_sub_badarg(&mut env, ok, 0, 1, "the atom `ok` (0, 1)");
        assert_sub_badarg(&mut env, make_small(42), 0, 0, "the integer 42");
        assert_sub_badarg(&mut env, make_small(42), 1, 2, "the integer 42 (1, 2)");
        assert_sub_badarg(&mut env, NIL, 0, 1, "`[]`");

        // A 16-bit source still refuses a 24-bit window (the source's own bit size bounds it).
        let mis = window_term(&mut env, &bin, 3, 16);
        assert_sub_badarg(&mut env, mis, 2, 2, "MisBin (2, 2)");
    }
}

/// The expected bits themselves are pinned by the **system `erl`** through its bit syntax (no C
/// fixture needed): `<<_:NBITS(pos), R:size/binary, _/bitstring>>` on the same values, with the
/// `size == 0` rows answering `<<>>` for any `pos`. Skips loudly when `erl` is unavailable.
#[test]
fn the_expected_bits_agree_with_the_erl_oracle() {
    use std::process::Command;

    if Command::new("erl")
        .args(["-noshell", "-eval", "halt(0)."])
        .output()
        .is_err()
    {
        common::skip("the `erl` oracle toolchain is not available");
        return;
    }

    let expr = "\
        B = <<1,2,3,4>>, \
        <<_:3, Mis:16/bits, _/bitstring>> = B, \
        T3 = <<1:3>>, \
        Big200 = list_to_binary(lists:seq(0,199)), \
        Big160 = list_to_binary(lists:seq(0,159)), \
        <<_:5, W:100/binary, _/bitstring>> = Big160, \
        Sub = fun(_, _, 0) -> <<>>; \
                 (V, P, S) -> <<_:P/binary, R:S/binary, _/bitstring>> = V, R \
              end, \
        io:format(\"~w~n~w~n~w~n~w~n~w~n~w~n~w~n~w~n~w~n~w~n\", \
                  [Sub(Mis,1,1), Sub(T3,0,0), Sub(B,5,0), Sub(B,0,4), Sub(B,4,0), \
                   Sub(B,0,0), Sub(Big200,3,100), Sub(Big200,199,1), Sub(Big200,200,0), \
                   Sub(W,0,10)]), \
        halt(0).";
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

    // The same ten requests as `(source, pos, size)` triples plus the expected bytes: `pos`/`size`
    // are bytes counted from the *source value*'s bit 0, which is exactly what the crate does with
    // the source's own bit offset added in.
    let p4 = small_payload();
    let big = counting_payload(200);
    let big160 = counting_payload(160);
    let w = window_bytes(&big160, 5, 800);
    let want: Vec<Vec<u8>> = vec![
        window_bytes(&p4, 3 + 8, 8),        // Sub(Mis, 1, 1)
        Vec::new(),                         // Sub(<<1:3>>, 0, 0)
        Vec::new(),                         // Sub(B, 5, 0)
        window_bytes(&p4, 0, 32),           // Sub(B, 0, 4)
        Vec::new(),                         // Sub(B, 4, 0)
        Vec::new(),                         // Sub(B, 0, 0)
        window_bytes(&big, 3 * 8, 800),     // Sub(Big200, 3, 100)
        window_bytes(&big, 199 * 8, 8),     // Sub(Big200, 199, 1)
        Vec::new(),                         // Sub(Big200, 200, 0)
        w[..10].to_vec(),                   // Sub(W, 0, 10)
    ];
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
    let expected: Vec<String> = want.iter().map(|b| render(b)).collect();
    assert_eq!(
        lines, expected,
        "the system Erlang's bit syntax and this test's expectations must agree"
    );

    // And the crate's own answer for the same ten requests, over sources built with the writers.
    let mut host = common::TestHost::new();
    let mut env = NifEnv::new(&mut host);
    let bin4 = RefcBinary::new(&p4).expect("an off-heap payload");
    let bin200 = RefcBinary::new(&big).expect("an off-heap payload");
    let bin160 = RefcBinary::new(&big160).expect("an off-heap payload");
    // SAFETY: `env` is live for the whole test; the payloads outlive every term built from them.
    unsafe {
        let mis = window_term(&mut env, &bin4, 3, 16);
        let t3 = heap_bits_term(&mut env, &[0b0010_0000], 3);
        let four = heap_bits_term(&mut env, &p4, 32);
        let big200 = window_term(&mut env, &bin200, 0, 200 * 8);
        let wterm = window_term(&mut env, &bin160, 5, 800);
        let requests = [
            (mis, 1usize, 1usize),
            (t3, 0, 0),
            (four, 5, 0),
            (four, 0, 4),
            (four, 4, 0),
            (four, 0, 0),
            (big200, 3, 100),
            (big200, 199, 1),
            (big200, 200, 0),
            (wterm, 0, 10),
        ];
        let got: Vec<Vec<u8>> = requests
            .into_iter()
            .map(|(term, pos, size)| {
                let result = sub(&mut env, term, pos, size);
                inspect(&mut env, result)
            })
            .collect();
        assert_eq!(got, want, "the crate's answers must agree with the oracle's");
    }
}
