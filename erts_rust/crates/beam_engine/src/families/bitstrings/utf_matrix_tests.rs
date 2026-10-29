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

//! The whole UTF matrix of the bitstring family, in **construction** and in
//! **matching**, every row oracle-pinned.
//!
//! One compiled fixture (`bitsutf.erl`, produced at test time by the system `erlc` and
//! loaded with [`beam_loader::load_file`]) carries every endianness spelling of the
//! three encodings — `utf8`, `utf16`, `utf16-little`, `utf16-native`, `utf32`,
//! `utf32-big`, `utf32-little`, `utf32-native` — twice: a construction function and a
//! matching function per spelling.
//!
//! Construction is pinned through the real public entry ([`Machine::run_export`]) on
//! both construction routes — the generic segment walk and the prepared program — plus
//! the binary-comprehension shape `<< <<E/utf32-little>> || E <- Bytes >>` the compiler
//! lowers to a nested `bs_create_bin` loop. Matching is pinned the same way through the
//! compiled match segment (`bs_get_utf8/16/32` + `bs_skip_*`), which resolves
//! `BSF_LITTLE`/`BSF_NATIVE` through `matching::reads::field_is_little`.
//!
//! The construction rows are the system `erl`'s own answer for the same segment, and
//! the suites re-derive them from a live `erl` on the same input; a `native` spelling's
//! expectation is written per target. Both suites **skip loudly** (never a silent pass)
//! when `erlc`/`erl` are not on `PATH`.
//!
//! The decoder-level surrogate / out-of-range **match failures** stay with the decoder
//! (`matching/utf_tests.rs`), where they are pinned in both byte orders.

use beam_loader::load_file;
use beam_term::signed_value;

use super::run_tests::{bitstring, byte_literal, compile_source, erl_eval, oracle_export};
use crate::context::MinimalContext;
use crate::machine::Machine;
use crate::terms::{make_heap_binary, make_list};

// ---------------------------------------------------------------------------
// The UTF construction matrix -- every endianness spelling, through `exec`
// ---------------------------------------------------------------------------

/// The inline UTF-construction fixture: one function per encoding/endianness spelling,
/// plus the binary-comprehension shape over a **list** generator.
///
/// `utf8`/`utf16`/`utf32` segments take no size and no unit, so each function here is a
/// bare codepoint write — the exact shape that ignored `little`/`native` for `utf32`.
/// The fixture is BIF-free (the [`MinimalContext`] installs no BIF hook), so the run is
/// the bitstring family and nothing else.
const UTF_SRC: &str = "\
-module(bitsutf).
-export([u8/1, u16/1, u16l/1, u16n/1, u32/1, u32b/1, u32l/1, u32n/1, comp/1,
         m8/1, m16/1, m16l/1, m16n/1, m32/1, m32b/1, m32l/1, m32n/1]).

u8(E) -> <<E/utf8>>.
u16(E) -> <<E/utf16>>.
u16l(E) -> <<E/utf16-little>>.
u16n(E) -> <<E/utf16-native>>.
u32(E) -> <<E/utf32>>.
u32b(E) -> <<E/utf32-big>>.
u32l(E) -> <<E/utf32-little>>.
u32n(E) -> <<E/utf32-native>>.
comp(Bytes) -> << <<E/utf32-little>> || E <- Bytes >>.

m8(B) -> <<X/utf8, _/bits>> = B, X.
m16(B) -> <<X/utf16, _/bits>> = B, X.
m16l(B) -> <<X/utf16-little, _/bits>> = B, X.
m16n(B) -> <<X/utf16-native, _/bits>> = B, X.
m32(B) -> <<X/utf32, _/bits>> = B, X.
m32b(B) -> <<X/utf32-big, _/bits>> = B, X.
m32l(B) -> <<X/utf32-little, _/bits>> = B, X.
m32n(B) -> <<X/utf32-native, _/bits>> = B, X.
";

/// Render bytes the way the oracle's `binary:encode_hex/1` does — uppercase hex, no
/// separators (`<<32,172>>` ⇒ `20AC`), so a short list of printable byte values can never
/// be re-rendered as an Erlang string by `~p`.
fn hex_render(bytes: &[u8]) -> String {
    let parts: Vec<String> = bytes.iter().map(|byte| format!("{byte:02X}")).collect();
    parts.concat()
}

/// The UTF construction matrix, end to end and against the live oracle.
///
/// Every row is the system `erl`'s own answer for the same segment (OTP 29.1.2 /
/// erts-17.1.1, a little-endian host): `utf32` **honours** `little`/`native`
/// (`emu/bs_instrs.tab:690-712` calls `erts_bs_put_integer_le` for `BSC_UTF32` with
/// `BSF_LITTLE`), so `<<16#1234/utf32-little>>` is `[52,18,0,0]` and not the big-endian
/// `[0,0,18,52]`. `native` is the machine's own order, so its expectation is spelled
/// per target. The comprehension row pins the nested `bs_create_bin` loop the compiler
/// emits for `<< <<E/utf32-little>> || E <- Bytes >>`.
#[test]
fn the_utf_construction_matrix_matches_the_oracle() {
    let Some(dir) = compile_source("utf", "bitsutf", UTF_SRC) else {
        eprintln!("SKIP: no usable `erlc` on PATH -- the UTF construction matrix skipped");
        return;
    };
    let module = load_file(dir.join("bitsutf.beam"))
        .unwrap_or_else(|e| panic!("load bitsutf.beam: {e:?}"));

    /// A `native`-flagged expectation: `big` on a big-endian host, `little` here.
    fn native(big: Vec<u8>, little: Vec<u8>) -> Vec<u8> {
        if cfg!(target_endian = "little") {
            little
        } else {
            big
        }
    }

    // (export, argument, the oracle's bytes)
    let rows: Vec<(&str, i64, Vec<u8>)> = vec![
        ("u8", 0x20AC, vec![0xE2, 0x82, 0xAC]),
        ("u8", 0x1_F600, vec![0xF0, 0x9F, 0x98, 0x80]),
        ("u16", 0x20AC, vec![0x20, 0xAC]),
        ("u16l", 0x20AC, vec![0xAC, 0x20]),
        ("u16l", 0x1_F600, vec![0x3D, 0xD8, 0x00, 0xDE]),
        (
            "u16n",
            0x20AC,
            native(vec![0x20, 0xAC], vec![0xAC, 0x20]),
        ),
        ("u32", 0x1234, vec![0x00, 0x00, 0x12, 0x34]),
        ("u32", 0x1_F600, vec![0x00, 0x01, 0xF6, 0x00]),
        ("u32b", 0x1234, vec![0x00, 0x00, 0x12, 0x34]),
        ("u32l", 0x0001, vec![0x01, 0x00, 0x00, 0x00]),
        ("u32l", 0x1234, vec![0x34, 0x12, 0x00, 0x00]),
        ("u32l", 0x10_FFFF, vec![0xFF, 0xFF, 0x10, 0x00]),
        ("u32l", 0x1_F600, vec![0x00, 0xF6, 0x01, 0x00]),
        (
            "u32n",
            0x1234,
            native(vec![0x00, 0x00, 0x12, 0x34], vec![0x34, 0x12, 0x00, 0x00]),
        ),
    ];

    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let mut rust_lines = Vec::with_capacity(rows.len() + 1);
    for &(function, argument, ref expected) in &rows {
        let result = machine
            .run_export(
                &module,
                oracle_export(&module, function),
                1,
                &[beam_term::make_small(argument)],
                &mut ctx,
            )
            .unwrap_or_else(|error| panic!("bitsutf:{function}/1: {error:?}"));
        let (bytes, nbits) = bitstring(result);
        assert_eq!(
            (bytes.as_slice(), nbits),
            (expected.as_slice(), expected.len() * 8),
            "bitsutf:{function}({argument})"
        );
        rust_lines.push(hex_render(expected));
    }

    // The comprehension: `<< <<E/utf32-little>> || E <- Bytes >>` over a list.
    let elements = [1i64, 0x1234, 0x1_F600];
    let list = make_list(&mut ctx, &elements.map(beam_term::make_small))
        .expect("a list of small integers");
    let composed = machine
        .run_export(&module, oracle_export(&module, "comp"), 1, &[list], &mut ctx)
        .expect("bitsutf:comp/1");
    let (bytes, nbits) = bitstring(composed);
    let mut want = Vec::new();
    for element in elements {
        want.extend_from_slice(&(element as u32).to_le_bytes());
    }
    assert_eq!(
        (bytes.as_slice(), nbits),
        (want.as_slice(), want.len() * 8),
        "bitsutf:comp([1, 16#1234, 16#1F600])"
    );
    rust_lines.push(hex_render(&want));

    // One `erl` invocation checks the whole matrix, so the pinned rows above are the
    // oracle's and not a re-statement of this runtime's own bytes.
    let mut eval = String::new();
    for &(function, argument, _) in &rows {
        eval.push_str(&format!(
            "io:format(\"~s~n\", [binary:encode_hex(bitsutf:{function}({argument}))]),\n"
        ));
    }
    eval.push_str(
        "io:format(\"~s~n\", [binary:encode_hex(bitsutf:comp([1, 4660, 128512]))]),\nhalt(0).",
    );
    match erl_eval(&dir, &eval) {
        Some(out) => {
            let oracle: Vec<&str> = out.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
            let rust: Vec<&str> = rust_lines.iter().map(String::as_str).collect();
            assert_eq!(oracle, rust, "the oracle disagrees for the UTF matrix");
        }
        None => eprintln!("SKIP: no system `erl` on PATH -- the UTF matrix oracle rows skipped"),
    }
}

/// The **matching** half of the same matrix: `bs_get_utf8/16/32` decodes every
/// endianness spelling of a compiled segment, against the live oracle.
///
/// The matching path already resolves `BSF_LITTLE`/`BSF_NATIVE` through
/// `matching::reads::field_is_little` (a raw `u32` flags operand can carry `BSF_NATIVE`
/// the loader never collapsed), so this suite pins the *whole* path — a compiled module's
/// `<<X/utf32-little, _/bits>> = B` through the instruction, the family `exec` and the
/// decoder — instead of the decoder alone. Every expectation is the system `erl`'s on the
/// same subject. The surrogate / out-of-range **match failures** live at the decoder level
/// (`matching/utf_tests.rs`), where they are pinned in both byte orders.
#[test]
fn the_utf_matching_matrix_matches_the_oracle() {
    let Some(dir) = compile_source("utf_match", "bitsutf", UTF_SRC) else {
        eprintln!("SKIP: no usable `erlc` on PATH -- the UTF matching matrix skipped");
        return;
    };
    let module = load_file(dir.join("bitsutf.beam"))
        .unwrap_or_else(|e| panic!("load bitsutf.beam: {e:?}"));

    /// A `native`-flagged subject: the code unit in the host's own byte order.
    fn native(big: Vec<u8>, little: Vec<u8>) -> Vec<u8> {
        if cfg!(target_endian = "little") {
            little
        } else {
            big
        }
    }

    // (export, subject bytes, the scalar the oracle decodes)
    let rows: Vec<(&str, Vec<u8>, i64)> = vec![
        ("m8", vec![0xE2, 0x82, 0xAC], 0x20AC),
        ("m8", vec![0xF0, 0x9F, 0x98, 0x80], 0x1_F600),
        ("m16", vec![0x20, 0xAC], 0x20AC),
        ("m16l", vec![0xAC, 0x20], 0x20AC),
        ("m16l", vec![0x3D, 0xD8, 0x00, 0xDE], 0x1_F600),
        ("m16n", native(vec![0x20, 0xAC], vec![0xAC, 0x20]), 0x20AC),
        ("m32", vec![0x00, 0x00, 0x20, 0xAC], 0x20AC),
        ("m32b", vec![0x00, 0x00, 0x20, 0xAC], 0x20AC),
        ("m32l", vec![0xAC, 0x20, 0x00, 0x00], 0x20AC),
        ("m32l", vec![0x00, 0xF6, 0x01, 0x00], 0x1_F600),
        ("m32l", vec![0xFF, 0xFF, 0x10, 0x00], 0x10_FFFF),
        (
            "m32n",
            native(vec![0x00, 0x00, 0x20, 0xAC], vec![0xAC, 0x20, 0x00, 0x00]),
            0x20AC,
        ),
    ];

    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let mut eval = String::new();
    for (function, subject, expected) in &rows {
        let term = make_heap_binary(&mut ctx, subject).expect("a heap binary subject");
        let result = machine
            .run_export(&module, oracle_export(&module, function), 1, &[term], &mut ctx)
            .unwrap_or_else(|error| panic!("bitsutf:{function}/1 over {subject:?}: {error:?}"));
        assert_eq!(
            signed_value(result),
            *expected,
            "bitsutf:{function}/1 over {subject:?}"
        );
        eval.push_str(&format!(
            "io:format(\"~p~n\", [bitsutf:{function}(<<{}>>)]),\n",
            byte_literal(subject)
        ));
    }
    eval.push_str("halt(0).");

    match erl_eval(&dir, &eval) {
        Some(out) => {
            let oracle: Vec<String> = out
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect();
            let rust: Vec<String> = rows.iter().map(|(_, _, value)| value.to_string()).collect();
            assert_eq!(oracle, rust, "the oracle disagrees for the UTF matching matrix");
        }
        None => eprintln!("SKIP: no system `erl` on PATH -- the UTF matching oracle rows skipped"),
    }
}
