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

//! Integration tests for the `erlang` checksum/digest BIFs — `crc32/1,2`,
//! `crc32_combine/3`, `adler32/1,2`, `adler32_combine/3`, `md5/1`, `md5_init/0`,
//! `md5_update/2` and `md5_final/1`.
//!
//! Every expectation below is **pinned against the system oracle** (OTP 29 /
//! erts-17.1), derived by running
//!
//! ```text
//! erl -noshell -eval 'io:format("~w~n",[erlang:crc32(<<"123456789">>)]),halt().'
//! ```
//!
//! and the same for `adler32`/`md5`/`*_combine`/`md5_update`/`md5_final`; checksum
//! values are *not* derivable from first principles, so they are pinned goldens (the
//! `tests/phash.rs` convention). `checksums_cross_check_the_live_oracle`
//! additionally re-derives a corpus **live** and skips loudly when the oracle is
//! absent.
//!
//! The tests drive only the public API — [`BifRegistry`]/[`BifContext`] plus the
//! declared dependency crates (`beam_alloc` for the heap, `beam_codec::term` for the
//! binary constructors/readers).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_codec::term::{bitstring_bytes, make_binary, make_bitstring};
use beam_common::{BeamError, ExitReason, Result};
use beam_process::ProcessId;
use beam_term::{make_atom, make_small, signed_value, AtomTable, Eterm};

/// A registry + heap + atom table the tests drive BIF calls through.
struct Fixture {
    registry: BifRegistry,
    heap: Heap,
    atoms: AtomTable,
}

impl Fixture {
    /// No optional seam is installed: none of these BIFs needs one.
    fn new() -> Fixture {
        let mut atoms = AtomTable::new();
        atoms.seed(["abc", "true", "false", "ok"]);
        Fixture {
            registry: default_registry(),
            heap: Heap::new(1 << 17).expect("test heap"),
            atoms,
        }
    }

    /// Calls `module:function(Args)` through the real registry.
    fn call(&mut self, module: &str, function: &str, args: &[Eterm]) -> Result<Eterm> {
        let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
        self.registry
            .call(module, function, args.len() as u32, &mut ctx, args)
    }

    /// Calls `erlang:function(Args)` and unwraps the result.
    fn ok(&mut self, function: &str, args: &[Eterm]) -> Eterm {
        self.call("erlang", function, args)
            .unwrap_or_else(|e| panic!("erlang:{function}/{} failed: {e:?}", args.len()))
    }

    /// The bytes of a byte-aligned binary result.
    fn bytes_of(&self, term: Eterm) -> Vec<u8> {
        let (bytes, offset, size_bits) = bitstring_bytes(term).expect("a bitstring result");
        assert_eq!(offset, 0, "the result is a whole-byte binary");
        assert_eq!(size_bits % 8, 0, "the result is whole-byte aligned");
        bytes[..size_bits / 8].to_vec()
    }

    /// Calls `erlang:function(Args)` and returns the bytes of its binary result.
    fn bytes_of_call(&mut self, function: &str, args: &[Eterm]) -> Vec<u8> {
        let term = self.ok(function, args);
        self.bytes_of(term)
    }

    /// A seeded atom term.
    fn atom(&self, name: &str) -> Eterm {
        let index = self
            .atoms
            .lookup(name)
            .unwrap_or_else(|| panic!("the atom `{name}` must be seeded in the fixture"));
        make_atom(index)
    }

    /// Builds the binary `<<bytes>>` on this fixture's heap.
    fn binary(&mut self, bytes: &[u8]) -> Eterm {
        make_binary(&mut self.heap, bytes).expect("the binary fits")
    }

    /// Builds the bitstring `<<Bytes:BitLen>>` (MSB-first packing) on the heap.
    fn bitstring(&mut self, bytes: &[u8], bit_len: usize) -> Eterm {
        make_bitstring(&mut self.heap, bytes, bit_len).expect("the bitstring fits")
    }

    /// Builds a proper list of the given terms on this fixture's heap.
    fn list(&mut self, items: &[Eterm]) -> Eterm {
        let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
        ctx.list(items).expect("the list fits")
    }

    /// Builds the cons cell `head :: tail` on this fixture's heap.
    fn cons(&mut self, head: Eterm, tail: Eterm) -> Eterm {
        let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
        ctx.cons(head, tail).expect("the cons cell fits")
    }

    /// Builds a float term on this fixture's heap.
    fn float(&mut self, value: f64) -> Eterm {
        beam_codec::term::make_float(&mut self.heap, value).expect("the float fits")
    }

    /// Builds the iolist `bytes` as a proper list of small integers.
    fn byte_list(&mut self, bytes: &[u8]) -> Eterm {
        let items: Vec<Eterm> = bytes.iter().map(|b| make_small(i64::from(*b))).collect();
        self.list(&items)
    }

    /// A single-element list holding the small integer `value`.
    fn int_list(&mut self, value: i64) -> Eterm {
        self.list(&[make_small(value)])
    }
}

/// Asserts a checksum result equals the oracle's (always a small non-negative int).
fn assert_int(term: Eterm, expected: u32, what: &str) {
    assert!(
        beam_term::is_small(term),
        "{what}: expected a small integer, got {term:?}"
    );
    assert_eq!(signed_value(term) as u32, expected, "{what}");
}

/// Asserts the call failed with `badarg`.
fn assert_badarg(result: Result<Eterm>, what: &str) {
    match result {
        Err(BeamError::BadArg) => {}
        other => panic!("{what}: expected badarg, got {other:?}"),
    }
}

/// Asserts the call failed with the crate's typed `unsupported_bif` raise.
fn assert_unsupported(result: Result<Eterm>, what: &str) {
    match result {
        Err(BeamError::Raise(raise)) if raise.reason == ExitReason::atom("unsupported_bif") => {}
        other => panic!("{what}: expected unsupported_bif, got {other:?}"),
    }
}

/// Asserts `erlang:Function(Args)` fails with `badarg`.
macro_rules! badarg_case {
    ($fx:expr, $function:expr, [$($arg:expr),* $(,)?], $what:expr) => {{
        let args = [$($arg),*];
        let result = $fx.call("erlang", $function, &args);
        assert_badarg(result, $what);
    }};
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

#[test]
fn checksum_bifs_are_registered() {
    // `bif.tab:48-57` (crc32/adler32 + the md5 family): all ten keys are implemented
    // — none is a registered capability gap.
    let reg = default_registry();
    for (function, arity) in [
        ("crc32", 1u32),
        ("crc32", 2),
        ("crc32_combine", 3),
        ("adler32", 1),
        ("adler32", 2),
        ("adler32_combine", 3),
        ("md5", 1),
        ("md5_init", 0),
        ("md5_update", 2),
        ("md5_final", 1),
    ] {
        assert!(
            reg.contains("erlang", function, arity),
            "erlang:{function}/{arity} must be registered"
        );
    }
    // The arity is part of the key: there is no `crc32/0`, `md5/2` or `md5_init/1`.
    assert!(!reg.contains("erlang", "crc32", 0));
    assert!(!reg.contains("erlang", "md5", 2));
    assert!(!reg.contains("erlang", "md5_init", 1));
    assert!(!reg.contains("erlang", "md5_update", 1));
}

// ---------------------------------------------------------------------------
// CRC-32
// ---------------------------------------------------------------------------

/// Oracle: `erl -noshell -eval 'io:format("~w~n",[erlang:crc32(<<"123456789">>)]),halt().'`
/// ⇒ `3421780262` (the canonical zlib CRC-32 check value).
#[test]
fn crc32_matches_the_oracle() {
    let mut fx = Fixture::new();

    let empty = fx.binary(&[]);
    assert_int(fx.ok("crc32", &[empty]), 0, "crc32(<<>>)");

    let nil = fx.byte_list(&[]);
    assert_int(fx.ok("crc32", &[nil]), 0, "crc32([])");

    let abc = fx.binary(b"abc");
    assert_int(fx.ok("crc32", &[abc]), 891_568_578, "crc32(<<\"abc\">>)");

    let digits = fx.binary(b"123456789");
    assert_int(
        fx.ok("crc32", &[digits]),
        3_421_780_262,
        "crc32(<<\"123456789\">>)",
    );

    // A byte list and a nested iolist are the same input.
    let bytes = fx.byte_list(&[1, 2, 3]);
    assert_int(fx.ok("crc32", &[bytes]), 1_438_416_925, "crc32([1,2,3])");

    let ab = fx.binary(b"ab");
    let c = fx.binary(b"c");
    let nested = fx.list(&[ab, c]);
    assert_int(
        fx.ok("crc32", &[nested]),
        891_568_578,
        "crc32([<<\"ab\">>,<<\"c\">>])",
    );

    // A cons tail may be a byte-aligned binary: `[1|<<2>>]` == `[1,2]`.
    let tail = fx.binary(&[2]);
    let improper = fx.cons(make_small(1), tail);
    assert_int(
        fx.ok("crc32", &[improper]),
        3_066_839_698,
        "crc32([1|<<2>>])",
    );

    // A byte tail is an improper list ⇒ `badarg` (`ALLOW_BYTE_TAIL` is off).
    let byte_tail = fx.cons(make_small(1), make_small(2));
    badarg_case!(fx, "crc32", [byte_tail], "crc32([1|2])");

    // The continuation form: `crc32(0, D) == crc32(D)`.
    let abc = fx.binary(b"abc");
    let zero = make_small(0);
    assert_int(
        fx.ok("crc32", &[zero, abc]),
        891_568_578,
        "crc32(0,<<\"abc\">>)",
    );
    let all_ones = make_small(i64::from(u32::MAX));
    assert_int(
        fx.ok("crc32", &[all_ones, abc]),
        899_311_407,
        "crc32(4294967295,<<\"abc\">>)",
    );
}

/// Oracle-pinned `crc32_combine/3` goldens: `C1 = crc32(<<"1234">>) = 2615402659`,
/// `C2 = crc32(<<"56789">>) = 320708720`.
#[test]
fn crc32_is_incremental_and_combinable() {
    let mut fx = Fixture::new();
    let c1 = 2_615_402_659u32;
    let c2 = 320_708_720u32;

    let one_two_three_four = fx.binary(b"1234");
    let five_to_nine = fx.binary(b"56789");
    assert_int(
        fx.ok("crc32", &[one_two_three_four]),
        c1,
        "crc32(<<\"1234\">>)",
    );
    assert_int(fx.ok("crc32", &[five_to_nine]), c2, "crc32(<<\"56789\">>)");

    // The incremental form: `crc32(crc32(A), B) == crc32(A ++ B)`.
    let c1_term = make_small(i64::from(c1));
    assert_int(
        fx.ok("crc32", &[c1_term, five_to_nine]),
        3_421_780_262,
        "crc32(crc32(<<\"1234\">>),<<\"56789\">>)",
    );

    // The combine form, with `Len2` the second stream's byte length.
    let combine = |fx: &mut Fixture, len2: i64| {
        let a = make_small(i64::from(c1));
        let b = make_small(i64::from(c2));
        let l = make_small(len2);
        fx.ok("crc32_combine", &[a, b, l])
    };
    assert_int(
        combine(&mut fx, 5),
        3_421_780_262,
        "crc32_combine(C1,C2,5)",
    );
    assert_int(combine(&mut fx, 1), 1_549_382_082, "crc32_combine(C1,C2,1)");
    assert_int(combine(&mut fx, 9), 135_094_578, "crc32_combine(C1,C2,9)");
    // A zero length is `C1 bxor C2` (the modern zlib operator form, *not* `C1`).
    assert_int(
        combine(&mut fx, 0),
        c1 ^ c2,
        "crc32_combine(C1,C2,0) = C1 bxor C2",
    );

    let zero = make_small(0);
    let a = make_small(i64::from(c1));
    let b = make_small(i64::from(c2));
    assert_int(
        fx.ok("crc32_combine", &[a, zero, zero]),
        c1,
        "crc32_combine(C1,0,0)",
    );
    assert_int(
        fx.ok("crc32_combine", &[zero, b, zero]),
        c2,
        "crc32_combine(0,C2,0)",
    );
    assert_int(
        fx.ok("crc32_combine", &[zero, zero, zero]),
        0,
        "crc32_combine(0,0,0)",
    );
}

#[test]
fn crc32_rejects_non_iodata_and_out_of_range_integers() {
    let mut fx = Fixture::new();

    // Non-iodata inputs.
    badarg_case!(fx, "crc32", [make_small(5)], "crc32(5)");
    let float = fx.float(1.0);
    badarg_case!(fx, "crc32", [float], "crc32(1.0)");
    let abc_atom = fx.atom("abc");
    badarg_case!(fx, "crc32", [abc_atom], "crc32(abc)");
    // A byte outside `0..=255`, a float leaf, and a non-byte-aligned bitstring.
    let too_big = fx.int_list(300);
    badarg_case!(fx, "crc32", [too_big], "crc32([300])");
    let negative = fx.int_list(-1);
    badarg_case!(fx, "crc32", [negative], "crc32([-1])");
    let float_leaf = fx.float(1.5);
    let float_list = fx.list(&[float_leaf]);
    badarg_case!(fx, "crc32", [float_list], "crc32([1.5])");
    let odd_bits = fx.bitstring(&[0b1000_0000], 3);
    badarg_case!(fx, "crc32", [odd_bits], "crc32(<<1:3>>)");

    // The `OldCrc` of the continuation form must be a plain integer in `0..=2^32-1`.
    let abc = fx.binary(b"abc");
    let two_to_32 = make_small(4_294_967_296);
    badarg_case!(fx, "crc32", [two_to_32, abc], "crc32(2^32, D)");
    let negative = make_small(-1);
    badarg_case!(fx, "crc32", [negative, abc], "crc32(-1, D)");
    let float = fx.float(1.0);
    badarg_case!(fx, "crc32", [float, abc], "crc32(1.0, D)");
    let binary = fx.binary(&[1]);
    badarg_case!(fx, "crc32", [binary, abc], "crc32(<<1>>, D)");

    // `crc32_combine/3` takes three plain integers in `0..=2^32-1`.
    let float = fx.float(1.0);
    let two = make_small(2);
    let three = make_small(3);
    badarg_case!(
        fx,
        "crc32_combine",
        [float, two, three],
        "crc32_combine(1.0,2,3)"
    );
    let two_to_32 = make_small(4_294_967_296);
    badarg_case!(
        fx,
        "crc32_combine",
        [two_to_32, two, three],
        "crc32_combine(2^32,2,3)"
    );
}

// ---------------------------------------------------------------------------
// Adler-32
// ---------------------------------------------------------------------------

/// Oracle-pinned Adler-32 goldens (initial value 1).
#[test]
fn adler32_matches_the_oracle() {
    let mut fx = Fixture::new();

    let empty = fx.binary(&[]);
    assert_int(fx.ok("adler32", &[empty]), 1, "adler32(<<>>)");
    let nil = fx.byte_list(&[]);
    assert_int(fx.ok("adler32", &[nil]), 1, "adler32([])");

    let abc = fx.binary(b"abc");
    assert_int(fx.ok("adler32", &[abc]), 38_600_999, "adler32(<<\"abc\">>)");
    let zero = make_small(0);
    assert_int(
        fx.ok("adler32", &[zero, abc]),
        38_404_390,
        "adler32(0,<<\"abc\">>)",
    );
    let one = make_small(1);
    assert_int(
        fx.ok("adler32", &[one, abc]),
        38_600_999,
        "adler32(1,<<\"abc\">>)",
    );

    let digits = fx.binary(b"123456789");
    assert_int(
        fx.ok("adler32", &[digits]),
        152_961_502,
        "adler32(<<\"123456789\">>)",
    );
    let one_two_three_four = fx.binary(b"1234");
    let five_to_nine = fx.binary(b"56789");
    assert_int(
        fx.ok("adler32", &[one_two_three_four]),
        33_030_347,
        "adler32(<<\"1234\">>)",
    );
    assert_int(
        fx.ok("adler32", &[five_to_nine]),
        53_739_796,
        "adler32(<<\"56789\">>)",
    );

    // A non-canonical accumulator is reduced by the `len == 1` fast path.
    let odd = make_small(4_286_578_688);
    let x = fx.binary(b"x");
    assert_int(
        fx.ok("adler32", &[odd, x]),
        458_872,
        "adler32(4286578688,<<\"x\">>)",
    );

    // A 20-byte payload uses the 16-byte block path.
    let all_ones = make_small(i64::from(u32::MAX));
    let twenty_ffs = fx.binary(&[255u8; 20]);
    assert_int(
        fx.ok("adler32", &[all_ones, twenty_ffs]),
        3_528_725_498,
        "adler32(4294967295, 20 x 255)",
    );
    // 6000 bytes exceeds zlib's `NMAX = 5552` block, exercising the blocking path.
    let six_thousand_ffs = fx.binary(&[255u8; 6000]);
    assert_int(
        fx.ok("adler32", &[all_ones, six_thousand_ffs]),
        3_580_123_639,
        "adler32(4294967295, 6000 x 255)",
    );

    // `[1,2,3|<<4>>]` — a list whose cons tail is a binary.
    let four = fx.binary(&[4]);
    let three_four = fx.cons(make_small(3), four);
    let two_three_four = fx.cons(make_small(2), three_four);
    let improper = fx.cons(make_small(1), two_three_four);
    assert_int(
        fx.ok("adler32", &[improper]),
        1_572_875,
        "adler32([1,2,3|<<4>>])",
    );
}

/// The `adler32/2` distinction between a zero-length *call* and no call at all:
/// `adler32(X, <<>>)` reduces a non-canonical `X`, `adler32(X, [])` does not (the
/// `is_nil(ioterm)` early return in `do_chksum`).
#[test]
fn adler32_zero_length_call_vs_empty_list() {
    let mut fx = Fixture::new();
    let all_ones = make_small(i64::from(u32::MAX));

    let empty_binary = fx.binary(&[]);
    assert_int(
        fx.ok("adler32", &[all_ones, empty_binary]),
        917_518,
        "adler32(4294967295, <<>>) — one zero-length call reduces",
    );
    let empty_list = fx.byte_list(&[]);
    assert_int(
        fx.ok("adler32", &[all_ones, empty_list]),
        u32::MAX,
        "adler32(4294967295, []) — no call at all",
    );
    // A list containing only an empty list performs no call either.
    let nested_empty = fx.list(&[empty_list]);
    assert_int(
        fx.ok("adler32", &[all_ones, nested_empty]),
        u32::MAX,
        "adler32(4294967295, [[]])",
    );
}

/// Oracle-pinned `adler32_combine/3` goldens: `A1 = adler32(<<"1234">>) = 33030347`,
/// `A2 = adler32(<<"56789">>) = 53739796`.
#[test]
fn adler32_combine_matches_the_oracle() {
    let mut fx = Fixture::new();
    let a1: u32 = 33_030_347;
    let a2: u32 = 53_739_796;

    let a1_term = make_small(i64::from(a1));
    let five_to_nine = fx.binary(b"56789");
    // The incremental form matches a single call over the concatenation.
    assert_int(
        fx.ok("adler32", &[a1_term, five_to_nine]),
        152_961_502,
        "adler32(adler32(<<\"1234\">>),<<\"56789\">>)",
    );

    let combine = |fx: &mut Fixture, len2: u32| {
        let x = make_small(i64::from(a1));
        let y = make_small(i64::from(a2));
        let l = make_small(i64::from(len2));
        fx.ok("adler32_combine", &[x, y, l])
    };
    assert_int(combine(&mut fx, 5), 152_961_502, "adler32_combine(A1,A2,5)");
    // zlib reduces the length modulo BASE (65521) before combining.
    assert_int(
        combine(&mut fx, 5 + 65_521),
        152_961_502,
        "adler32_combine(A1,A2,5+65521)",
    );
    // OTP's own `len2 == 0` workaround returns `A1` unchanged.
    assert_int(combine(&mut fx, 0), a1, "adler32_combine(A1,A2,0) = A1");
}

#[test]
fn adler32_rejects_bad_arguments() {
    let mut fx = Fixture::new();
    badarg_case!(fx, "adler32", [make_small(5)], "adler32(5)");
    let odd_bits = fx.bitstring(&[0b1000_0000], 3);
    badarg_case!(fx, "adler32", [odd_bits], "adler32(<<1:3>>)");
    let too_big = fx.int_list(300);
    badarg_case!(fx, "adler32", [too_big], "adler32([300])");

    let two_to_32 = make_small(4_294_967_296);
    let a = fx.binary(b"a");
    badarg_case!(fx, "adler32", [two_to_32, a], "adler32(2^32, D)");

    let one = make_small(1);
    let two = make_small(2);
    let float = fx.float(1.0);
    badarg_case!(
        fx,
        "adler32_combine",
        [one, two, float],
        "adler32_combine(1,2,1.0)"
    );
}

// ---------------------------------------------------------------------------
// MD5
// ---------------------------------------------------------------------------

/// Oracle-pinned MD5 goldens.
#[test]
fn md5_matches_the_oracle() {
    let mut fx = Fixture::new();

    let abc = fx.binary(b"abc");
    assert_eq!(
        fx.bytes_of_call("md5", &[abc]),
        vec![
            144, 1, 80, 152, 60, 210, 79, 176, 214, 150, 63, 125, 40, 225, 127, 114
        ],
        "md5(<<\"abc\">>)"
    );

    let empty = fx.binary(&[]);
    assert_eq!(
        fx.bytes_of_call("md5", &[empty]),
        vec![
            212, 29, 140, 217, 143, 0, 178, 4, 233, 128, 9, 152, 236, 248, 66, 126
        ],
        "md5(<<>>)"
    );

    // More than one 64-byte block, plus a partial tail.
    let hundred_as = fx.binary(&[b'A'; 100]);
    assert_eq!(
        fx.bytes_of_call("md5", &[hundred_as]),
        vec![
            138, 220, 89, 55, 230, 53, 246, 201, 175, 100, 111, 11, 35, 86, 15, 174
        ],
        "md5(100 x $A)"
    );
    let sixty_four_as = fx.binary(&[b'A'; 64]);
    assert_eq!(
        fx.bytes_of_call("md5", &[sixty_four_as]),
        vec![
            210, 137, 169, 117, 101, 188, 45, 39, 172, 139, 133, 69, 165, 221, 186, 69
        ],
        "md5(64 x $A)"
    );
}

/// `md5/1` takes **iodata**, not only a binary (unlike `md5_update/2`'s context
/// argument, which must be the 96-byte image).
#[test]
fn md5_accepts_iodata_and_rejects_other_shapes() {
    let mut fx = Fixture::new();
    let abc = fx.binary(b"abc");
    let expected = fx.bytes_of_call("md5", &[abc]);

    // A byte list and a one-element iolist both hash as `<<"abc">>`.
    let list = fx.byte_list(b"abc");
    assert_eq!(
        fx.bytes_of_call("md5", &[list]),
        expected,
        "md5([97,98,99])"
    );
    let nested = fx.list(&[abc]);
    assert_eq!(
        fx.bytes_of_call("md5", &[nested]),
        expected,
        "md5([<<\"abc\">>])"
    );

    // Non-iodata shapes are `badarg`.
    badarg_case!(fx, "md5", [make_small(5)], "md5(5)");
    let odd_bits = fx.bitstring(&[0b1000_0000], 3);
    badarg_case!(fx, "md5", [odd_bits], "md5(<<1:3>>)");
    let too_big = fx.int_list(300);
    badarg_case!(fx, "md5", [too_big], "md5([300])");
    let negative = fx.int_list(-1);
    badarg_case!(fx, "md5", [negative], "md5([-1])");
    let byte_tail = fx.cons(make_small(1), make_small(2));
    badarg_case!(fx, "md5", [byte_tail], "md5([1|2])");
    let float_leaf = fx.float(1.5);
    let float_list = fx.list(&[float_leaf]);
    badarg_case!(fx, "md5", [float_list], "md5([1.5])");
}

/// The `erts_md5_state` image (`erl_md5.h:30-35`) is observable, so it is pinned
/// byte-for-byte against the oracle: the four IV words, then `tot_len`, `len` and the
/// 64-byte block buffer, each little-endian (96 bytes in total).
#[test]
fn md5_context_image_is_byte_exact() {
    let mut fx = Fixture::new();

    const IV: [u8; 16] = [
        1, 35, 69, 103, 137, 171, 205, 239, 254, 220, 186, 152, 118, 84, 50, 16,
    ];

    // `erlang:md5_init()` ⇒ `<<1,35,69,103,...,16,0,0,...,0>>` (96 bytes).
    let init = fx.ok("md5_init", &[]);
    let mut expected = [0u8; 96];
    expected[..16].copy_from_slice(&IV);
    assert_eq!(fx.bytes_of(init), expected.to_vec(), "md5_init/0");

    // `update(init, "abc")` ⇒ IV untouched, tot_len 0, len 3, buf "abc".
    let init = fx.ok("md5_init", &[]);
    let abc = fx.binary(b"abc");
    let image = fx.bytes_of_call("md5_update", &[init, abc]);
    let mut expected = [0u8; 96];
    expected[..16].copy_from_slice(&IV);
    expected[24..32].copy_from_slice(&3u64.to_le_bytes());
    expected[32..35].copy_from_slice(b"abc");
    assert_eq!(image, expected.to_vec(), "md5_update(init, <<\"abc\">>)");

    // A full block is hashed immediately: tot_len = 64, the buffer is empty.
    let init = fx.ok("md5_init", &[]);
    let sixty_four_as = fx.binary(&[b'A'; 64]);
    let image = fx.bytes_of_call("md5_update", &[init, sixty_four_as]);
    assert_eq!(
        &image[..16],
        &[
            178, 23, 231, 24, 90, 99, 254, 95, 100, 63, 230, 166, 212, 1, 191, 89
        ],
        "update(init, 64 x $A) folds the block"
    );
    assert_eq!(&image[16..24], &64u64.to_le_bytes(), "tot_len = 64");
    assert_eq!(&image[24..32], &0u64.to_le_bytes(), "len = 0");
    assert_eq!(&image[32..], &[0u8; 64], "the block buffer is cleared");

    // Two 32-byte updates buffer exactly one block. The C's `part_len < left` is
    // false (32 < 32), so the block is **not** hashed yet: the IV is untouched,
    // `tot_len` stays 0 and `len` reaches 64 with `A x 32 ++ B x 32` buffered.
    let init = fx.ok("md5_init", &[]);
    let thirty_two_as = fx.binary(&[b'A'; 32]);
    let thirty_two_bs = fx.binary(&[b'B'; 32]);
    let half = fx.ok("md5_update", &[init, thirty_two_as]);
    let image = fx.bytes_of_call("md5_update", &[half, thirty_two_bs]);
    let mut expected = [0u8; 96];
    expected[..16].copy_from_slice(&IV);
    expected[24..32].copy_from_slice(&64u64.to_le_bytes());
    expected[32..64].copy_from_slice(&[b'A'; 32]);
    expected[64..96].copy_from_slice(&[b'B'; 32]);
    assert_eq!(
        image,
        expected.to_vec(),
        "update(update(init, 32 x $A), 32 x $B) buffers one full block"
    );

    // …while a *single* 64-byte update hashes the block right away (`len == 0`
    // bypasses the buffer branch): `tot_len = 64`, `len = 0`, buffer cleared.
    let init = fx.ok("md5_init", &[]);
    let mut joined = vec![b'A'; 32];
    joined.extend_from_slice(&[b'B'; 32]);
    let block = fx.binary(&joined);
    let image = fx.bytes_of_call("md5_update", &[init, block]);
    assert_eq!(
        &image[..16],
        &[
            248, 112, 12, 80, 66, 218, 22, 55, 92, 228, 162, 104, 224, 128, 204, 42
        ],
        "update(init, 32 x $A ++ 32 x $B) hashes the block"
    );
    assert_eq!(&image[16..24], &64u64.to_le_bytes(), "tot_len = 64");
    assert_eq!(&image[24..32], &0u64.to_le_bytes(), "len = 0");

    // `md5_update/2` also accepts iodata for the payload, not only a binary.
    let init = fx.ok("md5_init", &[]);
    let bytes = fx.byte_list(&[1, 2, 3]);
    let image = fx.bytes_of_call("md5_update", &[init, bytes]);
    assert_eq!(&image[32..35], &[1, 2, 3], "update(init, [1,2,3])");
    assert_eq!(&image[24..32], &3u64.to_le_bytes(), "len = 3");
}

/// `md5_final(md5_update(md5_init(), D)) =:= md5(D)` for every split of `D` — the
/// invariant the 96-byte context image exists to support.
#[test]
fn md5_split_invariance() {
    let mut fx = Fixture::new();
    let data: Vec<u8> = (0..200u32).map(|i| (i % 251) as u8).collect();
    let whole = fx.binary(&data);
    let expected = fx.bytes_of_call("md5", &[whole]);

    for split in [0usize, 1, 63, 64, 65, 100, 128, 199, 200] {
        let init = fx.ok("md5_init", &[]);
        let head = fx.binary(&data[..split]);
        let state = fx.ok("md5_update", &[init, head]);
        let tail = fx.binary(&data[split..]);
        let state = fx.ok("md5_update", &[state, tail]);
        let digest = fx.ok("md5_final", &[state]);
        assert_eq!(
            fx.bytes_of(digest),
            expected,
            "md5_final(md5_update(md5_update(md5_init(), D[..{split}]), D[{split}..]))"
        );
    }

    // `md5_final/1` does not mutate its argument: it can be finished twice.
    let init = fx.ok("md5_init", &[]);
    let abc = fx.binary(b"abc");
    let state = fx.ok("md5_update", &[init, abc]);
    let first = fx.bytes_of_call("md5_final", &[state]);
    let second = fx.bytes_of_call("md5_final", &[state]);
    assert_eq!(first, second, "md5_final/1 is pure");
}

/// The `Context` of `md5_update/2` and `md5_final/1` must be a **96-byte binary**
/// (`erts_get_aligned_binary_bytes(...) == NULL || size != sizeof(erts_md5_state)`).
#[test]
fn md5_context_arguments_are_validated() {
    let mut fx = Fixture::new();
    let a = fx.binary(b"a");

    // Wrong lengths (0, 95 and 97 bytes), a non-binary (a 96-element list) and a
    // non-byte-aligned bitstring.
    let empty = fx.binary(&[]);
    badarg_case!(fx, "md5_update", [empty, a], "update(<<>>, D)");
    let short = fx.binary(&[0u8; 95]);
    badarg_case!(fx, "md5_update", [short, a], "update(95 bytes, D)");
    let long = fx.binary(&[0u8; 97]);
    badarg_case!(fx, "md5_update", [long, a], "update(97 bytes, D)");
    let list = fx.byte_list(&[0u8; 96]);
    badarg_case!(fx, "md5_update", [list, a], "update([0 x 96], D)");
    let odd_bits = fx.bitstring(&[0b1000_0000], 3);
    badarg_case!(fx, "md5_update", [odd_bits, a], "update(<<1:3>>, D)");
    badarg_case!(fx, "md5_update", [make_small(5), a], "update(5, D)");

    // A bad *payload* is `badarg` too.
    let init = fx.ok("md5_init", &[]);
    badarg_case!(fx, "md5_update", [init, make_small(5)], "update(init, 5)");

    // `md5_final/1` validates its context the same way.
    let empty = fx.binary(&[]);
    badarg_case!(fx, "md5_final", [empty], "final(<<>>)");
    let short = fx.binary(&[0u8; 95]);
    badarg_case!(fx, "md5_final", [short], "final(95 bytes)");
    let list = fx.byte_list(&[0u8; 96]);
    badarg_case!(fx, "md5_final", [list], "final([0 x 96])");
}

/// A **hand-crafted** context image is accepted exactly as the oracle accepts it: a
/// `len` field of `64` finishes to `md5(64 x $A)`, while a `len` above `64` is this
/// crate's one documented gap — the oracle would read and write past its 64-byte
/// buffer (undefined behaviour), which cannot be reproduced faithfully, so it is a
/// typed `unsupported_bif` rather than a fabricated digest.
#[test]
fn md5_hand_crafted_contexts_are_validated() {
    let mut fx = Fixture::new();

    let mut image = [0u8; 96];
    image[..16].copy_from_slice(&[
        1, 35, 69, 103, 137, 171, 205, 239, 254, 220, 186, 152, 118, 84, 50, 16,
    ]);
    image[16..24].copy_from_slice(&0u64.to_le_bytes());
    image[24..32].copy_from_slice(&64u64.to_le_bytes());
    image[32..].copy_from_slice(&[b'A'; 64]);
    let hand_crafted = fx.binary(&image);
    assert_eq!(
        fx.bytes_of_call("md5_final", &[hand_crafted]),
        vec![
            210, 137, 169, 117, 101, 188, 45, 39, 172, 139, 133, 69, 165, 221, 186, 69
        ],
        "md5_final of a hand-crafted len=64 context == md5(64 x $A)"
    );

    // `len = 100` (past the buffer) is a typed `unsupported_bif`, never a fabricated
    // digest and never a panic.
    let mut image = [0u8; 96];
    image[24..32].copy_from_slice(&100u64.to_le_bytes());
    let out_of_range = fx.binary(&image);
    assert_unsupported(
        fx.call("erlang", "md5_update", &[out_of_range, hand_crafted]),
        "update(len=100, D)",
    );
    assert_unsupported(
        fx.call("erlang", "md5_final", &[out_of_range]),
        "final(len=100)",
    );
}

// ---------------------------------------------------------------------------
// Live oracle cross-check
// ---------------------------------------------------------------------------

/// The corpus the live check re-derives: empty, single-byte, multi-block, an
/// `NMAX`-exceeding payload, 200/1000-byte patterns and the 55..=62-byte MD5 `pad`
/// boundary remainders.
fn corpus() -> Vec<Vec<u8>> {
    let mut corpus = vec![
        Vec::new(),
        b"a".to_vec(),
        b"abc".to_vec(),
        b"123456789".to_vec(),
        b"message digest".to_vec(),
        vec![b'A'; 63],
        vec![b'A'; 64],
        vec![b'A'; 65],
        (0..200u32).map(|i| (i % 251) as u8).collect(),
        (0..1000u32).map(|i| (i % 251) as u8).collect(),
        vec![255u8; 6000],
    ];
    // The MD5 `pad` boundary: with `size` buffered bytes the zero run is
    // `(56 - (size + 1) % 64) mod 64`, and for a 55..=62-byte remainder the 8-byte
    // length field no longer fits the final block. These lengths (plus the 63/64/65
    // above) cover the whole `filled = 55..=64` window against the live oracle.
    corpus.extend((55..63).map(|n| vec![b'A'; n]));
    corpus
}
/// `A ++ B`.
fn cat(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut joined = a.to_vec();
    joined.extend_from_slice(b);
    joined
}

/// Renders `data` as an Erlang binary literal `<<1,2,3>>`.
fn erlang_binary(data: &[u8]) -> String {
    let mut out = String::from("<<");
    for (i, byte) in data.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&byte.to_string());
    }
    out.push_str(">>");
    out
}

/// Parses one `CRC ADLER <<bytes>>` oracle line.
fn parse_line(line: &str) -> (u32, u32, Vec<u8>) {
    let mut fields = line.splitn(3, ' ');
    let crc = fields.next().expect("crc32 field").parse().expect("crc32");
    let adler = fields
        .next()
        .expect("adler32 field")
        .parse()
        .expect("adler32");
    let md5 = fields.next().expect("md5 field");
    let inner = md5
        .trim()
        .strip_prefix("<<")
        .and_then(|s| s.strip_suffix(">>"))
        .unwrap_or_else(|| panic!("`{md5}` is not an Erlang binary literal"));
    let bytes = if inner.is_empty() {
        Vec::new()
    } else {
        inner
            .split(',')
            .map(|b| b.trim().parse::<u8>().expect("a byte"))
            .collect()
    };
    (crc, adler, bytes)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// `$ERTS_ORACLE` if it names an existing executable, else `erl` on `$PATH` — the
/// same resolution `tools/differential` uses.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_bifs checksum] $ERTS_ORACLE='{}' is not an existing executable \
             — treating it as no oracle.",
            program.display()
        );
        return None;
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("erl");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Runs one `erl -noshell -eval <eval> -s init stop` and returns its stdout.
fn run_erl(oracle: &Path, eval: &str) -> String {
    let output = Command::new(oracle)
        .arg("-noshell")
        .arg("-eval")
        .arg(eval)
        .arg("-s")
        .arg("init")
        .arg("stop")
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", oracle.display()));
    assert!(
        output.status.success(),
        "the oracle '{}' exited with {:?}\n--- stderr ---\n{}\n--- eval ---\n{eval}",
        oracle.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout)
        .unwrap_or_else(|e| panic!("the oracle printed non-UTF-8 to stdout: {e}"))
}

/// This crate's `crc32/1` of `data`, as a `u32` (the input `crc32_combine/3` takes).
fn crc32_of(fx: &mut Fixture, data: &[u8]) -> u32 {
    let term = fx.binary(data);
    let value = fx.ok("crc32", &[term]);
    assert!(beam_term::is_small(value), "crc32 returns a small integer");
    signed_value(value) as u32
}

/// This crate's `adler32/1` of `data`, as a `u32`.
fn adler32_of(fx: &mut Fixture, data: &[u8]) -> u32 {
    let term = fx.binary(data);
    let value = fx.ok("adler32", &[term]);
    assert!(beam_term::is_small(value), "adler32 returns a small integer");
    signed_value(value) as u32
}

/// Re-derives `crc32/1`, `adler32/1` and `md5/1` for the whole corpus **live** and
/// compares them with this crate's BIFs. The context-image path
/// (`md5_update`/`md5_final` over a split) must reach the same digest, and the
/// combine identity `combine(H(A), H(B), len(B)) == H(A ++ B)` is checked against
/// this crate's own (oracle-validated) single-call values. Skips loudly when no
/// oracle is available.
#[test]
fn checksums_cross_check_the_live_oracle() {
    let Some(oracle) = resolve_oracle() else {
        eprintln!();
        eprintln!("=========== beam_bifs checksum: live oracle SKIPPED ===========");
        eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or not");
        eprintln!("executable, and no `erl` on $PATH).");
        eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP).");
        eprintln!("===============================================================");
        eprintln!();
        return;
    };

    let data = corpus();
    let list = data
        .iter()
        .map(|d| erlang_binary(d))
        .collect::<Vec<_>>()
        .join(", ");
    let eval = format!(
        "Ds = [{list}], [io:format(\"~w ~w ~w~n\", [erlang:crc32(D), erlang:adler32(D), \
         erlang:md5(D)]) || D <- Ds], halt()."
    );
    let stdout = run_erl(&oracle, &eval);

    let mut lines = stdout.lines().filter(|l| !l.trim().is_empty());
    let mut fx = Fixture::new();
    let mut previous: Option<Vec<u8>> = None;

    for expected in &data {
        let line = lines
            .next()
            .unwrap_or_else(|| panic!("the oracle printed fewer lines than the corpus:\n{stdout}"));
        let (oracle_crc, oracle_adler, oracle_md5) = parse_line(line);
        assert_eq!(
            oracle_md5.len(),
            16,
            "the oracle's md5 is a 16-byte binary: {line}"
        );
        let what = format!("a {}-byte input", expected.len());

        let term = fx.binary(expected);
        assert_int(fx.ok("crc32", &[term]), oracle_crc, &format!("crc32 of {what}"));
        let term = fx.binary(expected);
        assert_int(
            fx.ok("adler32", &[term]),
            oracle_adler,
            &format!("adler32 of {what}"),
        );
        let term = fx.binary(expected);
        assert_eq!(
            fx.bytes_of_call("md5", &[term]),
            oracle_md5,
            "md5 of {what}"
        );

        // The 96-byte context image round-trips to the same digest in a split.
        let mid = expected.len() / 2;
        let init = fx.ok("md5_init", &[]);
        let head = fx.binary(&expected[..mid]);
        let state = fx.ok("md5_update", &[init, head]);
        let tail = fx.binary(&expected[mid..]);
        let state = fx.ok("md5_update", &[state, tail]);
        assert_eq!(
            fx.bytes_of_call("md5_final", &[state]),
            oracle_md5,
            "md5_final(md5_update*) of {what} split at {mid}"
        );

        // The combine identity: `combine(H(A), H(B), len(B)) == H(A ++ B)`, where
        // `H(A)`, `H(B)` and `H(A ++ B)` are this crate's (here oracle-validated)
        // single-call values.
        if let Some(prev) = &previous {
            let joined = cat(prev, expected);
            let len2 = make_small(i64::try_from(expected.len()).expect("fits"));
            let a = make_small(i64::from(crc32_of(&mut fx, prev)));
            let b = make_small(i64::from(oracle_crc));
            let combined = fx.ok("crc32_combine", &[a, b, len2]);
            let expected_crc = crc32_of(&mut fx, &joined);
            assert_int(
                combined,
                expected_crc,
                "crc32_combine over the corpus pair",
            );

            let len2 = make_small(i64::try_from(expected.len()).expect("fits"));
            let a = make_small(i64::from(adler32_of(&mut fx, prev)));
            let b = make_small(i64::from(oracle_adler));
            let combined = fx.ok("adler32_combine", &[a, b, len2]);
            let expected_adler = adler32_of(&mut fx, &joined);
            assert_int(
                combined,
                expected_adler,
                "adler32_combine over the corpus pair",
            );
        }
        previous = Some(expected.clone());
    }
    assert!(
        lines.next().is_none(),
        "the oracle printed more lines than the corpus:\n{stdout}"
    );
}
