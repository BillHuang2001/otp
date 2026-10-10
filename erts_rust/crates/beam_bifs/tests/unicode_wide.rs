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

//! The `unicode` module's **wide encodings** (`utf16`/`utf32`) driven through the
//! crate's public API and cross-checked against the system `erl`.
//!
//! `unicode:characters_to_binary/1,2,3` and `unicode:characters_to_list/1,2` accept
//! the encodings `latin1`/`unicode`/`utf8` plus the four wide ones — the bare atoms
//! `utf16`/`utf32` (big-endian aliases) and the tuple forms
//! `{utf16,big}`/`{utf16,little}`/`{utf32,big}`/`{utf32,little}` — implemented by
//! `src/unicode/wide.rs`. This file is the *public-API* companion to that module's
//! in-module golden tests: it drives the **registered keys** through
//! [`BifRegistry::call`] over a real `beam_alloc::Heap` + a seeded
//! `beam_term::AtomTable` (exactly as `beam_rt` does) and compares each result
//! value-for-value with the live oracle.
//!
//! Both sides are rendered with `io:format("~w", …)` — never `~p` — so the
//! `~p` printable-string/binary re-flow rules cannot disguise a byte difference:
//! the oracle prints one `~w` line per probe in a **single** `erl -noshell -eval`
//! process, and the crate renders the same term through its registered
//! `io:format/2` BIF into a capturing [`BytesSink`].
//!
//! The corpus covers: bare vs `{wide, endian}` spellings; a leading BOM (which is
//! just `U+FEFF`); odd-length/truncated input (`{incomplete,_,_}`) versus a genuine
//! `{error,_,_}`; surrogate pairs; lone surrogates; code points above `U+10FFFF`;
//! list / mixed / nested / improper input; every in×out combination of
//! `latin1`/`unicode`/`utf8` with the wide encodings; and the untranslated `Rest`
//! carried in the error/incomplete tuples.
//!
//! [`oracle_agrees_on_the_wide_encoding_surface`] skips **loudly** (and still
//! passes) when no usable `erl` is on `$PATH`; the pinned-golden tests below it
//! always run.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, BytesSink};
use beam_codec::term::{make_binary, make_bitstring};
use beam_common::BeamError;
use beam_process::ProcessId;
use beam_term::{AtomTable, Eterm};

// ---------------------------------------------------------------------------
// Term builder (shared by both sides of the comparison)
// ---------------------------------------------------------------------------

/// A corpus argument, built on the Rust side through the crate's public API and
/// rendered on the oracle side as Erlang source. Keeping one description for both
/// sides is what makes the two probe sequences index-aligned by construction.
#[derive(Clone, Debug)]
enum T {
    /// A small integer (a code point, inside a list).
    Int(i64),
    /// A float (an invalid character-list element).
    Float(f64),
    /// A byte-aligned binary.
    Bin(Vec<u8>),
    /// A bitstring of `1` bits (`bytes` must be `ceil(bits / 8)` long).
    Bits(Vec<u8>, usize),
    /// A bare atom (an encoding name, or an unknown atom for the `badarg` probes).
    Atom(&'static str),
    /// A proper list.
    List(Vec<T>),
    /// An improper list `[Head… | Tail]`.
    Improp(Vec<T>, Box<T>),
    /// A tuple.
    Tuple(Vec<T>),
}

/// A small integer argument.
fn i(n: i64) -> T {
    T::Int(n)
}

/// A float argument.
fn f(v: f64) -> T {
    T::Float(v)
}
/// A binary argument.
fn bin(bytes: &[u8]) -> T {
    T::Bin(bytes.to_vec())
}

/// A bitstring argument of `bits` bits (`<<data…>>` with a trailing `V:K` field).
fn bits(data: &[u8], bits: usize) -> T {
    T::Bits(data.to_vec(), bits)
}

/// An atom argument.
fn atom(name: &'static str) -> T {
    T::Atom(name)
}

/// A proper-list argument.
fn list(elems: Vec<T>) -> T {
    T::List(elems)
}

/// An improper-list argument `[elem… | tail]`.
fn improp(elems: Vec<T>, tail: T) -> T {
    T::Improp(elems, Box::new(tail))
}

/// A tuple argument.
fn tup(elems: Vec<T>) -> T {
    T::Tuple(elems)
}

/// The `{utf16|utf32, big|little}` encoding tuple.
fn pair(tag: &'static str, endian: &'static str) -> T {
    tup(vec![atom(tag), atom(endian)])
}

/// A proper list of small integers.
fn cps(nums: &[i64]) -> T {
    list(nums.iter().map(|&n| T::Int(n)).collect())
}

/// The Erlang source literal for `t`.
///
/// Every atom in the corpus is a **bare** lowercase atom (no quoting needed), and
/// the integer/bitstring forms use the same bit-syntax the crate's writers produce
/// (`<<V:K>>` carries the leading `K` bits of the byte at `full = nbits / 8`).
fn erl_literal(t: &T) -> String {
    match t {
        T::Int(n) => n.to_string(),
        T::Float(v) => format!("{v:?}"),
        T::Bin(bytes) => {
            let parts: Vec<String> = bytes.iter().map(|b| b.to_string()).collect();
            format!("<<{}>>", parts.join(","))
        }
        T::Bits(bytes, n) => {
            let full = n / 8;
            let rem = n % 8;
            let mut parts: Vec<String> = bytes[..full].iter().map(|b| b.to_string()).collect();
            if rem > 0 {
                // The trailing field carries the *high* `rem` bits of the last byte
                // (most significant bit first), which is what `V:K` builds.
                let value = bytes[full] >> (8 - rem);
                parts.push(format!("{value}:{rem}"));
            }
            format!("<<{}>>", parts.join(","))
        }
        T::Atom(name) => (*name).to_string(),
        T::List(elems) => {
            let parts: Vec<String> = elems.iter().map(erl_literal).collect();
            format!("[{}]", parts.join(","))
        }
        T::Improp(elems, tail) => {
            let parts: Vec<String> = elems.iter().map(erl_literal).collect();
            format!("[{}|{}]", parts.join(","), erl_literal(tail))
        }
        T::Tuple(elems) => {
            let parts: Vec<String> = elems.iter().map(erl_literal).collect();
            format!("{{{}}}", parts.join(","))
        }
    }
}

/// Materializes `t` on `ctx`'s heap.
fn build(t: &T, ctx: &mut BifContext<'_>) -> Eterm {
    match t {
        T::Int(n) => ctx.int(*n).expect("a small int fits"),
        T::Float(v) => ctx.make_float(*v).expect("a float fits"),
        T::Bin(bytes) => make_binary(ctx.heap(), bytes).expect("the binary fits on the heap"),
        T::Bits(bytes, n) => {
            make_bitstring(ctx.heap(), bytes, *n).expect("the bitstring fits on the heap")
        }
        T::Atom(name) => ctx.atom(name).expect("the corpus atom is seeded"),
        T::Tuple(elems) => {
            let mut terms = Vec::with_capacity(elems.len());
            for e in elems {
                terms.push(build(e, ctx));
            }
            ctx.tuple(&terms).expect("a short tuple fits")
        }
        T::List(elems) => {
            let mut terms = Vec::with_capacity(elems.len());
            for e in elems {
                terms.push(build(e, ctx));
            }
            ctx.list(&terms).expect("a short list fits")
        }
        T::Improp(elems, tail) => {
            let mut acc = build(tail, ctx);
            for e in elems.iter().rev() {
                let head = build(e, ctx);
                acc = ctx.cons(head, acc).expect("a short list fits");
            }
            acc
        }
    }
}

// ---------------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------------

/// One probe: the `unicode` function, its arguments, and a human label.
struct Case {
    label: String,
    fun: &'static str,
    args: Vec<T>,
}

/// `Case` constructor (arity is `args.len()`).
fn case(label: &str, fun: &'static str, args: Vec<T>) -> Case {
    Case {
        label: label.to_string(),
        fun,
        args,
    }
}

/// `unicode:characters_to_binary(Data, InEncoding)`.
fn cb2(label: &str, data: T, enc: T) -> Case {
    case(label, "characters_to_binary", vec![data, enc])
}

/// `unicode:characters_to_binary(Data, InEncoding, OutEncoding)`.
fn cb3(label: &str, data: T, enc: T, out: T) -> Case {
    case(label, "characters_to_binary", vec![data, enc, out])
}

/// `unicode:characters_to_list(Data, InEncoding)`.
fn cl2(label: &str, data: T, enc: T) -> Case {
    case(label, "characters_to_list", vec![data, enc])
}

/// Every atom the corpus needs interned. Without an `AtomInterner` seam only a
/// pre-interned name resolves, so the table is seeded up front with the encoding
/// names, the two result tags (`error`/`incomplete`) and the unknown names the
/// `badarg` probes use.
const CORPUS_ATOMS: [&str; 16] = [
    "big",
    "error",
    "foo",
    "incomplete",
    "latin1",
    "little",
    "ok",
    "unicode",
    "utf16",
    "utf16_big",
    "utf16_le",
    "utf16_little",
    "utf32",
    "utf32_big",
    "utf32_little",
    "utf8",
];

/// The whole corpus: `unicode` wide-encoding probes, in the order they are probed
/// on both sides.
fn corpus() -> Vec<Case> {
    let mut cases = Vec::new();

    // --- wide input, `characters_to_binary/2` (output is always `unicode`) -----
    cases.push(cb2("utf16 bare decodes big-endian", bin(&[0, 65, 0, 66]), atom("utf16")));
    cases.push(cb2("utf16 {big} decodes big-endian", bin(&[0, 65, 0, 66]), pair("utf16", "big")));
    cases.push(cb2("utf16 {little} decodes little-endian", bin(&[65, 0, 66, 0]), pair("utf16", "little")));
    cases.push(cb2("utf16 bare misreads little-endian bytes", bin(&[65, 0, 66, 0]), atom("utf16")));
    cases.push(cb2("utf32 bare decodes big-endian", bin(&[0, 0, 0, 65]), atom("utf32")));
    cases.push(cb2("utf32 {big} decodes two code points", bin(&[0, 0, 0, 65, 0, 0, 0, 66]), pair("utf32", "big")));
    cases.push(cb2("utf32 {little} decodes two code points", bin(&[65, 0, 0, 0, 66, 0, 0, 0]), pair("utf32", "little")));
    cases.push(cb2("utf16 leading BOM is U+FEFF", bin(&[254, 255, 0, 65]), atom("utf16")));
    cases.push(cb2("utf16 {little} leading BOM is U+FEFF", bin(&[255, 254, 65, 0]), pair("utf16", "little")));
    cases.push(cb2("utf32 leading BOM is U+FEFF", bin(&[0, 0, 254, 255]), atom("utf32")));
    cases.push(cb2("utf32 non-BOM U+FFFF", bin(&[0, 0, 255, 255]), atom("utf32")));
    cases.push(cb2("utf32 {little} leading BOM is U+FEFF", bin(&[255, 254, 0, 0]), pair("utf32", "little")));
    cases.push(cb2("utf32 NUL code point", bin(&[0, 0, 0, 0]), atom("utf32")));
    cases.push(cb2("utf32 {little} NUL code point", bin(&[0, 0, 0, 0]), pair("utf32", "little")));
    cases.push(cb2("utf16 surrogate pair is one code point", bin(&[216, 60, 222, 0]), atom("utf16")));
    cases.push(cb2("utf16 {little} surrogate pair is one code point", bin(&[60, 216, 0, 222]), pair("utf16", "little")));
    cases.push(cb2("utf16 lone high surrogate is an error", bin(&[216, 0, 0, 65]), atom("utf16")));
    cases.push(cb2("utf16 lone low surrogate is an error", bin(&[220, 0]), atom("utf16")));
    cases.push(cb2("utf16 high surrogate then a non-low is an error", bin(&[216, 60, 65, 66]), atom("utf16")));
    cases.push(cb2("utf16 non-lead high byte is a plain code unit", bin(&[0, 216]), atom("utf16")));
    cases.push(cb2("utf16 U+FFFE decodes", bin(&[255, 254]), atom("utf16")));
    // Truncated input is `incomplete`; the `cbv/2` clauses decide the shapes.
    cases.push(cb2("utf16 odd length is incomplete", bin(&[0, 65, 0]), atom("utf16")));
    cases.push(cb2("utf16 lone byte is incomplete", bin(&[0]), atom("utf16")));
    cases.push(cb2("utf16 lone high-surrogate lead is incomplete", bin(&[216, 0]), atom("utf16")));
    cases.push(cb2("utf16 truncated after a full code point", bin(&[0, 65, 216, 0]), atom("utf16")));
    cases.push(cb2("utf16 three-quarter surrogate pair is incomplete", bin(&[216, 60, 222]), atom("utf16")));
    cases.push(cb2("utf16 single 0xD8 byte is incomplete", bin(&[216]), atom("utf16")));
    cases.push(cb2("utf16 single 0xFF byte is incomplete", bin(&[255]), atom("utf16")));
    cases.push(cb2("utf16 single 0xDC byte is an error", bin(&[220]), atom("utf16")));
    cases.push(cb2("utf16 {little} trailing lone byte is incomplete", bin(&[60, 216, 0, 222, 65]), pair("utf16", "little")));
    cases.push(cb2("utf16 {little} single 0xFF byte is incomplete", bin(&[255]), pair("utf16", "little")));
    cases.push(cb2("utf16 {little} 0xDC00 lead is an error", bin(&[0, 220]), pair("utf16", "little")));
    cases.push(cb2("utf32 surrogate value is an error", bin(&[0, 0, 216, 0]), atom("utf32")));
    cases.push(cb2("utf32 {little} surrogate value is an error", bin(&[0, 216, 0, 0]), pair("utf32", "little")));
    cases.push(cb2("utf32 above U+10FFFF is an error", bin(&[0, 17, 0, 0]), atom("utf32")));
    cases.push(cb2("utf32 above U+10FFFF with trailing bytes", bin(&[0, 17, 0, 0, 65]), atom("utf32")));
    cases.push(cb2("utf32 one byte is incomplete", bin(&[0]), atom("utf32")));
    cases.push(cb2("utf32 two zero bytes are incomplete", bin(&[0, 0]), atom("utf32")));
    cases.push(cb2("utf32 three zero bytes are incomplete", bin(&[0, 0, 0]), atom("utf32")));
    cases.push(cb2("utf32 three bytes of an over-range value is an error", bin(&[0, 17, 0]), atom("utf32")));
    cases.push(cb2("utf32 {little} one byte is incomplete", bin(&[0]), pair("utf32", "little")));
    cases.push(cb2("utf32 {little} three bytes are incomplete", bin(&[65, 0, 0]), pair("utf32", "little")));
    cases.push(cb2("utf32 {little} byte pattern 65,216,1 is incomplete", bin(&[65, 216, 1]), pair("utf32", "little")));
    cases.push(cb2("utf32 {little} 254,255,0 is an error", bin(&[254, 255, 0]), pair("utf32", "little")));
    cases.push(cb2("utf32 {little} 255,255,0 is an error", bin(&[255, 255, 0]), pair("utf32", "little")));
    cases.push(cb2("utf32 {little} 1,255,0 is incomplete", bin(&[1, 255, 0]), pair("utf32", "little")));
    cases.push(cb2("utf16 empty binary", bin(&[]), atom("utf16")));

    // --- list / mixed / nested / improper input -------------------------------
    cases.push(cb2("empty list", list(Vec::new()), atom("utf16")));
    cases.push(cb2("list of code points ignores the encoding", cps(&[65, 66]), atom("utf16")));
    cases.push(cb2("list with an astral code point", cps(&[0x1F600]), atom("utf16")));
    cases.push(cb2("nested lists flatten", list(vec![cps(&[65]), cps(&[66])]), atom("utf16")));
    cases.push(cb2("mixed integer and binary parts", list(vec![i(65), bin(&[0, 66])]), atom("utf16")));
    cases.push(cb2("single binary part in a list", list(vec![bin(&[0, 65])]), atom("utf16")));
    cases.push(cb2("improper list ending in a binary", improp(vec![i(65)], bin(&[0, 66])), atom("utf16")));
    cases.push(cb2("split surrogate halves in two parts", list(vec![bin(&[216, 60]), bin(&[222, 0])]), atom("utf16")));
    cases.push(cb2("split surrogate halves across three parts", list(vec![bin(&[216]), bin(&[60, 222, 0])]), atom("utf16")));
    cases.push(cb2("two whole code points in two parts", list(vec![bin(&[0, 65]), bin(&[0, 66])]), atom("utf16")));
    cases.push(cb2("split code point across binary parts", list(vec![bin(&[0]), bin(&[65, 0, 66])]), atom("utf16")));
    cases.push(cb2("long part after a short one", list(vec![bin(&[0, 65]), bin(&[216, 60, 222, 0])]), atom("utf16")));
    cases.push(cb2("pending part then an integer is an error", list(vec![bin(&[0, 65]), bin(&[216, 60]), i(66)]), atom("utf16")));
    cases.push(cb2("surrogate split across utf32 parts", list(vec![bin(&[0, 0]), bin(&[0, 65])]), atom("utf32")));
    cases.push(cb2("short utf32 part before a full one", list(vec![bin(&[0]), bin(&[0, 65])]), atom("utf32")));
    cases.push(cb2("three-byte utf32 part then one byte", list(vec![bin(&[0, 0, 0]), bin(&[65])]), atom("utf32")));
    cases.push(cb2("single utf32 binary part", list(vec![bin(&[0, 0, 0, 65])]), atom("utf32")));
    cases.push(cb2("lone surrogate part is a Latin-1 error", list(vec![bin(&[0, 220, 0])]), atom("utf16")));
    cases.push(cb2("code point above U+10FFFF in a list", cps(&[0x110000]), atom("utf16")));
    cases.push(cb2("surrogate code point in a list", cps(&[0xD800]), atom("utf16")));
    cases.push(cb2("negative code point in a list", cps(&[-1]), atom("utf16")));
    cases.push(cb2("float element is badarg", list(vec![f(1.0)]), atom("utf16")));
    cases.push(cb2("improper list not ending in a binary is badarg", improp(vec![i(65)], i(66)), atom("utf16")));
    cases.push(cb2("non-byte-aligned bitstring is badarg (binary)", bits(&[0, 65, 0], 20), atom("utf16")));
    cases.push(cl2("non-byte-aligned bitstring is badarg (list)", bits(&[0, 65, 0], 20), atom("utf16")));

    // --- `characters_to_binary/3`: narrow input, wide output -------------------
    cases.push(cb3("latin1 -> utf16 (big-endian default)", cps(&[65, 66]), atom("latin1"), atom("utf16")));
    cases.push(cb3("latin1 -> {utf16,little}", cps(&[65, 66]), atom("latin1"), pair("utf16", "little")));
    cases.push(cb3("unicode -> utf16 with an astral code point", cps(&[0x1F600]), atom("unicode"), atom("utf16")));
    cases.push(cb3("latin1 -> utf32", cps(&[65, 66]), atom("latin1"), atom("utf32")));
    cases.push(cb3("latin1 -> {utf32,little}", cps(&[65, 66]), atom("latin1"), pair("utf32", "little")));
    cases.push(cb3("unicode -> {utf32,little} with an astral code point", cps(&[0x1F600]), atom("unicode"), pair("utf32", "little")));
    cases.push(cb3("unencodable astral code point -> utf16", cps(&[0x110000]), atom("unicode"), atom("utf16")));
    cases.push(cb3("unencodable astral code point -> utf32", cps(&[0x110000]), atom("unicode"), atom("utf32")));
    cases.push(cb3("surrogate code point -> utf16", cps(&[0xD800]), atom("unicode"), atom("utf16")));
    cases.push(cb3("surrogate code point -> utf32", cps(&[0xD800]), atom("unicode"), atom("utf32")));
    cases.push(cb3("negative code point -> utf16", cps(&[-2]), atom("unicode"), atom("utf16")));
    cases.push(cb3("latin1 code point above 255 -> utf16", cps(&[256]), atom("latin1"), atom("utf16")));

    // --- `characters_to_binary/3`: the in×out matrix --------------------------
    for &inp in &["latin1", "unicode", "utf8"] {
        for &out in &["latin1", "unicode", "utf8"] {
            let label = format!("ASCII list in={inp} out={out}");
            cases.push(cb3(&label, cps(&[65, 66]), atom(inp), atom(out)));
        }
    }
    for &inp in &["latin1", "unicode", "utf8"] {
        for &out in &["utf16", "utf32"] {
            let label = format!("ASCII list in={inp} out={out}");
            cases.push(cb3(&label, cps(&[65, 66]), atom(inp), atom(out)));
        }
    }
    for &inp in &["latin1", "unicode", "utf8"] {
        for out in [pair("utf16", "little"), pair("utf32", "little")] {
            let label = format!("ASCII list in={inp} out={}", erl_literal(&out));
            cases.push(cb3(&label, cps(&[65, 66]), atom(inp), out));
        }
    }
    // utf8 binary input against every output encoding.
    for &out in &["latin1", "unicode", "utf8", "utf16", "utf32"] {
        let label = format!("AB binary in=utf8 out={out}");
        cases.push(cb3(&label, bin(b"AB"), atom("utf8"), atom(out)));
    }
    for out in [pair("utf16", "big"), pair("utf16", "little"), pair("utf32", "big"), pair("utf32", "little")] {
        let label = format!("AB binary in=utf8 out={}", erl_literal(&out));
        cases.push(cb3(&label, bin(b"AB"), atom("utf8"), out));
    }

    // --- `characters_to_binary/3`: wide input on either side ------------------
    cases.push(cb3("utf16 in / utf8 out", bin(&[0, 65, 0, 66]), atom("utf16"), atom("utf8")));
    cases.push(cb3("utf16 in / unicode out", bin(&[0, 65]), atom("utf16"), atom("unicode")));
    cases.push(cb3("utf16 in / utf16 out", bin(&[0, 65]), atom("utf16"), atom("utf16")));
    cases.push(cb3("utf16 in / {utf16,little} out", bin(&[0, 65]), atom("utf16"), pair("utf16", "little")));
    cases.push(cb3("{utf16,little} in / {utf16,big} out", cps(&[65, 66]), pair("utf16", "little"), pair("utf16", "big")));
    cases.push(cb3("utf32 in / utf16 out", bin(&[0, 0, 0, 65]), atom("utf32"), atom("utf16")));
    cases.push(cb3("utf16 in / utf32 out", bin(&[0, 65]), atom("utf16"), atom("utf32")));
    cases.push(cb3("utf32 in / {utf32,little} out", bin(&[0, 0, 0, 65]), atom("utf32"), pair("utf32", "little")));
    cases.push(cb3("surrogate pair utf16 -> {utf16,little}", bin(&[216, 60, 222, 0]), atom("utf16"), pair("utf16", "little")));
    cases.push(cb3("surrogate pair utf16 -> utf32", bin(&[216, 60, 222, 0]), atom("utf16"), atom("utf32")));
    cases.push(cb3("utf16 in / latin1 out", bin(&[0, 65, 0, 66]), atom("utf16"), atom("latin1")));
    cases.push(cb3("astral utf16 in / latin1 out is an error", bin(&[216, 60, 222, 0]), atom("utf16"), atom("latin1")));
    cases.push(cb3("utf16 in / latin1 out error keeps the Rest", bin(&[0, 65, 1, 0]), atom("utf16"), atom("latin1")));
    cases.push(cb3("utf32 in / latin1 out", bin(&[0, 0, 0, 65]), atom("utf32"), atom("latin1")));
    cases.push(cb3("truncated utf32 in / latin1 out", bin(&[0, 0, 0]), atom("utf32"), atom("latin1")));
    cases.push(cb3("truncated utf32 little in / latin1 out", bin(&[0, 0, 0]), pair("utf32", "little"), atom("latin1")));
    cases.push(cb3("incomplete utf16 in / latin1 out", bin(&[0, 65, 216]), atom("utf16"), atom("latin1")));
    cases.push(cb3("incomplete utf16 in / utf8 out", bin(&[0, 65, 216]), atom("utf16"), atom("utf8")));
    cases.push(cb3("lone surrogate utf16 in / utf8 out", bin(&[0, 65, 220, 0]), atom("utf16"), atom("utf8")));
    cases.push(cb3("over-range utf32 in / utf8 out", bin(&[0, 17, 0, 0]), atom("utf32"), atom("utf8")));
    cases.push(cb3("split utf8 bytes -> utf16", list(vec![bin(&[195]), bin(&[169])]), atom("utf8"), atom("utf16")));
    cases.push(cb3("split utf8 bytes -> latin1", list(vec![bin(&[195]), bin(&[169])]), atom("utf8"), atom("latin1")));
    cases.push(cb3("binary part then an integer part", list(vec![bin(&[0, 65]), i(66)]), atom("utf16"), atom("utf16")));

    // --- `characters_to_list` with a wide input -------------------------------
    cases.push(cl2("utf16 -> code points", bin(&[0, 65, 0, 66]), atom("utf16")));
    cases.push(cl2("utf16 {little} -> code points", bin(&[65, 0]), pair("utf16", "little")));
    cases.push(cl2("utf32 -> code points", bin(&[0, 0, 0, 65]), atom("utf32")));
    cases.push(cl2("utf32 {little} -> code points", bin(&[65, 0, 0, 0]), pair("utf32", "little")));
    cases.push(cl2("utf16 surrogate pair -> one code point", bin(&[216, 60, 222, 0]), atom("utf16")));
    cases.push(cl2("empty utf16 binary", bin(&[]), atom("utf16")));
    cases.push(cl2("empty utf32 little binary", bin(&[]), pair("utf32", "little")));
    cases.push(cl2("empty list under a wide encoding", list(Vec::new()), atom("utf16")));
    cases.push(cl2("list of code points", cps(&[65, 66]), atom("utf16")));
    cases.push(cl2("mixed integer and binary parts (list)", list(vec![i(65), bin(&[0, 66])]), atom("utf16")));
    cases.push(cl2("incomplete utf16 keeps the Rest bare", bin(&[0, 65, 216]), atom("utf16")));
    cases.push(cl2("error utf16 keeps the Rest bare", bin(&[0, 65, 220, 0]), atom("utf16")));
    cases.push(cl2("over-range utf32 error", bin(&[0, 17, 0, 0]), atom("utf32")));
    cases.push(cl2("lone byte is incomplete", bin(&[0]), atom("utf16")));
    cases.push(cl2("truncated utf32 after one code point", bin(&[0, 0, 0, 65, 0, 0]), atom("utf32")));
    cases.push(cl2("single-part incomplete list", list(vec![bin(&[0])]), atom("utf16")));
    cases.push(cl2("surrogate halves in two list parts", list(vec![bin(&[216, 60]), bin(&[222, 0])]), atom("utf16")));
    cases.push(cl2("split surrogate halves across three parts (list)", list(vec![bin(&[216]), bin(&[60, 222, 0])]), atom("utf16")));
    cases.push(cl2("pending part then a binary is an error", list(vec![bin(&[0, 65]), bin(&[220, 0]), bin(&[0, 66])]), atom("utf16")));

    // --- argument validation --------------------------------------------------
    cases.push(cb2("unknown encoding atom", bin(&[1]), atom("foo")));
    cases.push(cl2("unknown encoding atom (list)", bin(&[1]), atom("foo")));
    cases.push(cb2("unknown endian in a utf16 tuple", bin(&[1]), pair("utf16", "foo")));
    cases.push(cb2("unknown endian in a utf32 tuple", bin(&[1]), pair("utf32", "foo")));
    cases.push(cb3("unknown output encoding", cps(&[65]), atom("latin1"), atom("foo")));
    cases.push(cb3("unknown output endian", cps(&[65]), atom("latin1"), pair("utf16", "foo")));
    cases.push(cb2("one-element encoding tuple", bin(&[1]), tup(vec![atom("utf16")])));
    cases.push(cb2("three-element encoding tuple", bin(&[1]), tup(vec![atom("utf16"), atom("big"), atom("big")])));
    cases.push(cb2("non-atom encoding tag", bin(&[1]), tup(vec![cps(&[5]), atom("big")])));
    cases.push(cb2("integer encoding", bin(&[1]), i(5)));
    cases.push(cl2("integer encoding (list)", bin(&[1]), i(5)));
    // The bare atom spellings are NOT valid: only `utf16`/`utf32` or the
    // `{utf16|utf32, big|little}` tuples parse (verified against the oracle).
    cases.push(cb2("utf16_big is not an encoding atom", bin(&[1]), atom("utf16_big")));
    cases.push(cb2("utf16_little is not an encoding atom", bin(&[1]), atom("utf16_little")));
    cases.push(cb2("utf32_big is not an encoding atom", bin(&[1]), atom("utf32_big")));
    cases.push(cb2("utf32_little is not an encoding atom", bin(&[1]), atom("utf32_little")));
    cases.push(cb2("utf16_le is not an encoding atom", bin(&[1]), atom("utf16_le")));
    cases.push(cl2("utf16_big is not an encoding atom (list)", bin(&[1]), atom("utf16_big")));
    cases.push(cl2("utf32_little is not an encoding atom (list)", bin(&[1]), atom("utf32_little")));
    // The wide atoms themselves ARE accepted — the failures above are about the
    // *shape* of the argument, not an unknown name.
    cases.push(cb2("one byte under utf16 is incomplete", bin(&[1]), atom("utf16")));
    cases.push(cb2("one byte under utf32 is an error", bin(&[1]), atom("utf32")));
    cases.push(cb2("whole code point under utf16", bin(&[0, 65]), atom("utf16")));
    cases.push(cb2("whole code point under utf32", bin(&[0, 0, 0, 65]), atom("utf32")));
    cases.push(cb2("atom element is badarg", list(vec![atom("ok")]), atom("utf16")));

    cases
}

// ---------------------------------------------------------------------------
// The crate side
// ---------------------------------------------------------------------------

/// Renders `term` with the registered `io_lib`-independent `io:format/2` BIF and
/// the `"~w"` directive, writing into the context's output sink.
fn render_w(ctx: &mut BifContext<'_>, reg: &BifRegistry, term: Eterm) {
    let tilde = ctx.int(126).expect("a char code fits");
    let w = ctx.int(119).expect("a char code fits");
    let format = ctx.list(&[tilde, w]).expect("a two-element list fits");
    let args = ctx.list(&[term]).expect("a one-element list fits");
    reg.call("io", "format", 2, ctx, &[format, args])
        .expect("io:format/2 renders the term");
}

// ---------------------------------------------------------------------------
// Header: the pinned goldens (always run)
// ---------------------------------------------------------------------------

/// The crate's own rendering path must produce the oracle's `~w` shape — a binary
/// with a zero byte is `<<0,65>>` (never a printable string, never `~p`'s form).
#[test]
fn render_w_matches_the_oracle_shape() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let mut atoms = AtomTable::new();
    atoms.seed(CORPUS_ATOMS.iter().copied());
    let mut sink = BytesSink::new();

    let term = {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        make_binary(ctx.heap(), &[0, 65]).expect("the binary fits")
    };
    {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
        render_w(&mut ctx, &reg, term);
    }
    assert_eq!(sink.as_str(), "<<0,65>>");
}

/// The bare `utf16_big`/`utf32_little`-style atom spellings are `badarg`: only the
/// bare `utf16`/`utf32` atoms and the `{utf16|utf32, big|little}` tuples name a
/// wide encoding. Pinned here (independently of the oracle) because it is the one
/// shape decision a reader is most likely to get backwards.
#[test]
fn wide_atom_spellings_are_rejected() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let mut atoms = AtomTable::new();
    atoms.seed(CORPUS_ATOMS.iter().copied());
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let one = make_binary(ctx.heap(), &[1]).expect("the binary fits");
    for spelling in ["utf16_big", "utf16_little", "utf32_big", "utf32_little", "utf16_le"] {
        let enc = ctx.atom(spelling).expect("the atom is seeded");
        for fun in ["characters_to_binary", "characters_to_list"] {
            assert!(
                matches!(
                    reg.call("unicode", fun, 2, &mut ctx, &[one, enc]),
                    Err(BeamError::BadArg)
                ),
                "unicode:{fun}/2 with the encoding atom `{spelling}` must be badarg"
            );
        }
    }
    // The `{utf16, le}` tuple is equally rejected, and the accepted spellings are
    // not (`<<1>>` under `utf16` is the `cbv/2` one-byte prefix case).
    let bad = tup(vec![atom("utf16"), atom("utf16_le")]);
    let bad = build(&bad, &mut ctx);
    assert!(matches!(
        reg.call("unicode", "characters_to_binary", 2, &mut ctx, &[one, bad]),
        Err(BeamError::BadArg)
    ));
    let good = build(&atom("utf16"), &mut ctx);
    assert!(reg
        .call("unicode", "characters_to_binary", 2, &mut ctx, &[one, good])
        .is_ok());
}

/// The corpus must stay broad enough for the live comparison to be meaningful —
/// an accidentally empty or narrowed corpus would make it vacuously pass.
#[test]
fn the_corpus_covers_both_functions_and_every_wide_encoding() {
    let cases = corpus();
    assert!(
        cases.len() >= 90,
        "the corpus shrank to {} cases",
        cases.len()
    );
    let labels: Vec<&str> = cases.iter().map(|c| c.label.as_str()).collect();
    let mut sorted = labels.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), labels.len(), "the corpus labels must be unique");

    let sources: String = cases
        .iter()
        .map(|c| {
            let args: Vec<String> = c.args.iter().map(erl_literal).collect();
            format!("{}({})", c.fun, args.join(","))
        })
        .collect::<Vec<_>>()
        .join("\n");
    for needle in [
        "characters_to_binary(",
        "characters_to_list(",
        "utf16",
        "{utf16,little}",
        "{utf16,big}",
        "utf32",
        "{utf32,little}",
        "{utf32,big}",
        "latin1",
        "utf8",
        "unicode",
    ] {
        assert!(
            sources.contains(needle),
            "the corpus no longer mentions `{needle}`"
        );
    }
    assert!(
        cases.iter().any(|c| c.args.len() == 3),
        "the corpus no longer probes the /3 arity"
    );
    // Every case's arity must match a registered BIF (checked here without an
    // oracle so a typo cannot silently degrade into an `undef`).
    let reg = default_registry();
    for c in &cases {
        assert!(
            reg.contains("unicode", c.fun, c.args.len() as u32),
            "`unicode:{}`/{} is not registered",
            c.fun,
            c.args.len()
        );
    }
}

// ---------------------------------------------------------------------------
// Live oracle cross-check (skips loudly when `erl` is absent)
// ---------------------------------------------------------------------------

/// Wall-clock budget for the `erl` probe before the oracle is declared unusable.
const ERL_TIMEOUT: Duration = Duration::from_secs(60);

/// `$ERTS_ORACLE` if it names an existing executable, else `erl` on `$PATH` — the
/// resolution `tools/differential` uses.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_bifs unicode_wide] $ERTS_ORACLE='{}' is not an existing executable — \
             treating it as no oracle.",
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

/// Runs one `erl -noshell -eval <program>` under a bounded wall-clock budget and
/// returns its stdout, or `None` (after a loud skip message) when no oracle is
/// available / it cannot answer.
///
/// The child's stdout and stderr are drained on worker threads (so a full pipe can
/// never wedge it) while the main thread waits with a deadline; a child that
/// overruns [`ERL_TIMEOUT`] is **killed** and reaped, so a hung `erl` can never hang
/// the suite.
fn erl_stdout(oracle: &Path, program: &str) -> Option<String> {
    let spawned = Command::new(oracle)
        .arg("-noshell")
        .arg("-eval")
        .arg(program)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            eprintln!(
                "[beam_bifs unicode_wide] SKIP: could not spawn the oracle '{}': {e}",
                oracle.display()
            );
            return None;
        }
    };

    let stdout_pipe = child.stdout.take().expect("piped stdout");
    let stderr_pipe = child.stderr.take().expect("piped stderr");
    let stdout_reader = thread::spawn(move || read_pipe(stdout_pipe));
    let stderr_reader = thread::spawn(move || read_pipe(stderr_pipe));

    let deadline = Instant::now() + ERL_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_reader.join();
                    let _ = stderr_reader.join();
                    eprintln!(
                        "[beam_bifs unicode_wide] SKIP: the oracle did not answer within \
                         {ERL_TIMEOUT:?} and was killed."
                    );
                    return None;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                eprintln!("[beam_bifs unicode_wide] SKIP: waiting for the oracle failed: {e}");
                return None;
            }
        }
    };

    let stdout = join_pipe(stdout_reader, "stdout")?;
    let stderr = join_pipe(stderr_reader, "stderr")?;

    if !status.success() {
        eprintln!(
            "[beam_bifs unicode_wide] SKIP: the oracle exited with {:?} for:\n{program}\n\
             --- stderr ---\n{}",
            status.code(),
            String::from_utf8_lossy(&stderr),
        );
        return None;
    }
    Some(String::from_utf8_lossy(&stdout).into_owned())
}

/// Joins a pipe-reader thread, turning a panic or an I/O error into a loud skip
/// rather than a silently truncated oracle output.
fn join_pipe(reader: thread::JoinHandle<std::io::Result<Vec<u8>>>, which: &str) -> Option<Vec<u8>> {
    match reader.join() {
        Ok(Ok(buf)) => Some(buf),
        Ok(Err(e)) => {
            eprintln!("[beam_bifs unicode_wide] SKIP: reading the oracle {which} failed: {e}");
            None
        }
        Err(_) => {
            eprintln!("[beam_bifs unicode_wide] SKIP: the oracle {which} reader panicked");
            None
        }
    }
}

/// Reads a child pipe to EOF, surfacing an I/O error instead of swallowing it.
fn read_pipe(mut pipe: impl Read) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    pipe.read_to_end(&mut buf)?;
    Ok(buf)
}

/// The one-process oracle program: every case printed as a single `~w` line.
///
/// Each expression is wrapped in a `try` that turns an exception into its reason
/// (so a `badarg` probe compares as the atom `badarg`, and an unexpected class is
/// still printed rather than aborting the whole program). The catch-clause
/// variables are unique per statement because `erl -eval` threads bindings across
/// the whole comma-sequence.
fn oracle_program(cases: &[Case]) -> String {
    let mut statements = Vec::with_capacity(cases.len());
    for (idx, c) in cases.iter().enumerate() {
        let args: Vec<String> = c.args.iter().map(erl_literal).collect();
        statements.push(format!(
            "io:format(\"~w~n\",[try unicode:{}({}) \
             catch error:E{idx} -> E{idx}; C{idx}:R{idx} -> {{C{idx},R{idx}}} end])",
            c.fun,
            args.join(","),
        ));
    }
    format!("{},\nhalt(0).", statements.join(",\n"))
}

/// Re-derives the whole wide-encoding corpus **live** under the system `erl` and
/// compares it value-for-value with the same BIFs driven through
/// [`BifRegistry::call`]. Skips loudly (and passes) when no usable oracle exists.
#[test]
fn oracle_agrees_on_the_wide_encoding_surface() {
    let cases = corpus();

    // The crate side first: build every argument, then render each result with
    // `~w` through the registered `io:format/2` BIF.
    let reg = default_registry();
    let mut heap = Heap::new(1 << 22).expect("test heap");
    let mut atoms = AtomTable::new();
    atoms.seed(CORPUS_ATOMS.iter().copied());

    let arg_terms: Vec<Vec<Eterm>> = {
        let mut builder = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let mut all = Vec::with_capacity(cases.len());
        for c in &cases {
            let mut terms = Vec::with_capacity(c.args.len());
            for a in &c.args {
                terms.push(build(a, &mut builder));
            }
            all.push(terms);
        }
        all
    };

    let mut got: Vec<String> = Vec::with_capacity(cases.len());
    for (idx, c) in cases.iter().enumerate() {
        let mut sink = BytesSink::new();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
        let outcome = reg.call("unicode", c.fun, c.args.len() as u32, &mut ctx, &arg_terms[idx]);
        let special = match outcome {
            Ok(term) => {
                render_w(&mut ctx, &reg, term);
                None
            }
            Err(BeamError::BadArg) => Some("badarg".to_string()),
            Err(other) => panic!(
                "case #{idx} `{}` (unicode:{}/{}) returned an unexpected error: {other:?}",
                c.label,
                c.fun,
                c.args.len(),
            ),
        };
        drop(ctx);
        let rendered = special.unwrap_or_else(|| sink.as_str().trim_end().to_string());
        assert!(
            !rendered.is_empty(),
            "case #{idx} `{}` rendered an empty string",
            c.label
        );
        got.push(rendered);
    }

    let Some(oracle) = resolve_oracle() else {
        eprintln!("===============================================================");
        eprintln!("[beam_bifs unicode_wide] No usable system `erl` oracle found");
        eprintln!("($ERTS_ORACLE unset or not executable, and no `erl` on $PATH).");
        eprintln!(
            "This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP); {} crate-side \
             probes were rendered but never compared.",
            got.len()
        );
        eprintln!("===============================================================");
        return;
    };

    let program = oracle_program(&cases);
    let Some(stdout) = erl_stdout(&oracle, &program) else {
        eprintln!("[beam_bifs unicode_wide] oracle cross-check SKIPPED");
        return;
    };

    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        cases.len(),
        "the oracle printed {} lines, expected {} probes\n--- stdout ---\n{stdout}",
        lines.len(),
        cases.len(),
    );

    let mut mismatches = Vec::new();
    for (idx, c) in cases.iter().enumerate() {
        let want = lines[idx].trim();
        if want != got[idx] {
            mismatches.push(format!(
                "case #{idx} `{}` — unicode:{}({})\n  oracle: {want}\n  crate : {}",
                c.label,
                c.fun,
                c.args.iter().map(erl_literal).collect::<Vec<_>>().join(","),
                got[idx],
            ));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} of {} wide-encoding probes disagree with the oracle:\n{}",
        mismatches.len(),
        cases.len(),
        mismatches.join("\n")
    );

    eprintln!(
        "[beam_bifs unicode_wide] oracle cross-check RAN against '{}': {} probes agreed \
         (characters_to_binary/2,3 + characters_to_list/2 over latin1/unicode/utf8 + \
         utf16/{{utf16,big}}/{{utf16,little}}/utf32/{{utf32,big}}/{{utf32,little}}).",
        oracle.display(),
        cases.len(),
    );
}
