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

//! Oracle-pinned integration tests for the `os` / `file` / `prim_file` BIFs
//! registered by `src/os_bif.rs`, driven through the **public** API only
//! (`default_registry()` + `BifRegistry::call`).
//!
//! Every golden below carries the probe that pinned it (system OTP 29.1.1 /
//! erts-17.1). The inspection toolkit is itself made of registered BIFs —
//! `erlang:length/1`, `erlang:hd/1`, `erlang:tl/1`, `erlang:element/2`,
//! `erlang:tuple_size/1`, `erlang:byte_size/1`, `binary:at/2` — plus
//! `beam_term::{atom_val, signed_value}` for atoms and small integers.
//!
//! One live cross-check shells out to the system `erl` and **skips loudly** when
//! the oracle is absent (the `tests/nif.rs` pattern).

use std::process::Command;

use beam_alloc::Heap;
use beam_bifs::prelude::*;
use beam_common::Result;
use beam_process::ProcessId;
use beam_term::{atom_val, signed_value, AtomTable, Eterm, NIL};

/// A generous heap: these BIFs build small lists, tuples and binaries.
fn heap() -> Heap {
    Heap::new(4096).expect("test heap")
}

/// The core atoms plus the ones this module resolves by name.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true", "false", "error", "warning", "utf8", "latin1", "abc",
        "second", "seconds", "millisecond", "milli_seconds", "microsecond",
        "micro_seconds", "nanosecond", "nano_seconds", "native", "perf_counter",
        "fortnight", "os_bif_test_atom",
    ]);
    atoms
}

/// Calls `module:function/arity` through the full public registry.
fn call(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<Eterm> {
    reg.call(module, function, arity, ctx, args)
}

/// The small-integer value of `term`.
///
/// Only a valid **small** integer; the clock BIFs return epoch-nanosecond counts
/// as heap bignums, for which this yields garbage — use [`as_big_int`] there.
fn as_int(term: Eterm) -> i64 {
    signed_value(term)
}

/// The value of `term` as an `i128`, small integer **or** heap bignum (the clock
/// BIFs return epoch-nanosecond readings past the 60-bit small range).
fn as_big_int(term: Eterm) -> i128 {
    match beam_codec::term::big_parts(term) {
        Ok((negative, digits)) => {
            let mut magnitude: u128 = 0;
            for (i, limb) in digits.iter().enumerate() {
                // The readings below are far under `i128`; a limb past its range
                // saturates rather than shifting out of the type.
                let shift = 64 * i as u32;
                if shift >= 128 {
                    break;
                }
                magnitude = magnitude.wrapping_add(u128::from(*limb) << shift);
            }
            let value = i128::try_from(magnitude).unwrap_or(i128::MAX);
            if negative {
                -value
            } else {
                value
            }
        }
        Err(_) => i128::from(signed_value(term)),
    }
}

/// The name of an atom term, as the shared atom table spells it.
fn atom_name(atoms: &AtomTable, term: Eterm) -> String {
    atoms.name(atom_val(term)).expect("an interned atom name").to_string()
}

/// A char list `"..."` as an Erlang term.
fn string_term(ctx: &mut BifContext<'_>, s: &str) -> Eterm {
    let mut elems = Vec::new();
    for byte in s.bytes() {
        elems.push(ctx.int(i64::from(byte)).expect("fits"));
    }
    ctx.list(&elems).expect("the string fits")
}

/// A binary as an Erlang term, built by the registered
/// `erlang:list_to_binary/1` (there is no public binary constructor).
fn binary_term(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    bytes: &[u8],
) -> Eterm {
    let mut elems = Vec::new();
    for byte in bytes {
        elems.push(ctx.int(i64::from(*byte)).expect("fits"));
    }
    let list = ctx.list(&elems).expect("the binary fits");
    call(reg, ctx, "erlang", "list_to_binary", 1, &[list]).expect("list_to_binary")
}

/// The bytes of a binary term, read back one byte at a time through the
/// registered `binary:at/2`/`erlang:byte_size/1` BIFs.
fn binary_bytes(reg: &BifRegistry, ctx: &mut BifContext<'_>, term: Eterm) -> Vec<u8> {
    let size = as_int(call(reg, ctx, "erlang", "byte_size", 1, &[term]).expect("byte_size"));
    let mut out = Vec::new();
    for i in 0..size {
        let index = ctx.int(i).expect("fits");
        let byte = call(reg, ctx, "binary", "at", 2, &[term, index]).expect("binary:at");
        out.push(as_int(byte) as u8);
    }
    out
}

/// The elements of a proper list term (`length` + `hd`/`tl`).
fn list_elements(reg: &BifRegistry, ctx: &mut BifContext<'_>, term: Eterm) -> Vec<Eterm> {
    let len = as_int(call(reg, ctx, "erlang", "length", 1, &[term]).expect("length"));
    let mut out = Vec::with_capacity(len as usize);
    let mut cur = term;
    for _ in 0..len {
        out.push(call(reg, ctx, "erlang", "hd", 1, &[cur]).expect("hd"));
        cur = call(reg, ctx, "erlang", "tl", 1, &[cur]).expect("tl");
    }
    out
}

/// `NIL` is the only proper list of length zero.
fn is_nil_term(reg: &BifRegistry, ctx: &mut BifContext<'_>, term: Eterm) -> bool {
    as_int(call(reg, ctx, "erlang", "length", 1, &[term]).expect("length")) == 0
}

/// `expected`/`got` comparison over a char list whose elements are Unicode
/// **code points** (`erts_convert_native_to_filename`'s shape): each element is
/// re-encoded to UTF-8, so `[0xE5, 0xE4, 0xF6]` matches `"åäö"`.
fn assert_string(reg: &BifRegistry, ctx: &mut BifContext<'_>, term: Eterm, expected: &str) {
    let text: String = list_elements(reg, ctx, term)
        .into_iter()
        .map(|e| char::from_u32(as_int(e) as u32).expect("a code point"))
        .collect();
    assert_eq!(text, expected);
}

/// Runs one `-eval` statement under the system `erl`; `None` when the oracle is
/// unavailable (the test then skips loudly).
fn oracle_eval(statement: &str) -> Option<String> {
    let output = Command::new("erl")
        .args(["-noshell", "-eval", statement, "-s", "init", "stop"])
        .output();
    match output {
        Ok(out) if out.status.success() => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => {
            eprintln!(
                "SKIP: the oracle `erl` failed ({}); stderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            None
        }
        Err(err) => {
            eprintln!("SKIP: no `erl` oracle on PATH ({err})");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

#[test]
fn getenv_missing_is_false_and_an_atom_is_badarg() {
    // ORACLE: `os:getenv("OS_BIF_TEST_NOPE")` => false;
    //         `os:getenv(abc)` => badarg; `os:getenv(42)` => badarg.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let missing = string_term(&mut ctx, "OS_BIF_TEST_DEFINITELY_UNSET");
    assert_eq!(atom_name(&atoms, call(&reg, &mut ctx, "os", "getenv", 1, &[missing]).expect("getenv")), "false");

    let atom_key = ctx.atom("os_bif_test_atom").expect("seeded");
    assert!(call(&reg, &mut ctx, "os", "getenv", 1, &[atom_key]).is_err());
    let int_key = ctx.int(42).expect("fits");
    assert!(call(&reg, &mut ctx, "os", "getenv", 1, &[int_key]).is_err());

    // An empty key is accepted (the C's `length == 0` check is on a pointer) and
    // is not in the environment.
    // ORACLE: `os:getenv("")` => false.
    assert_eq!(atom_name(&atoms, call(&reg, &mut ctx, "os", "getenv", 1, &[NIL]).expect("getenv")), "false");
}

#[test]
fn putenv_getenv_unsetenv_round_trip() {
    // ORACLE: `os:putenv("GENESIS_T","V")` => true; `os:getenv("GENESIS_T")` =>
    //         "V"; `os:unsetenv("GENESIS_T")` => true; then `false`. An overwrite
    //         also returns true. A Unicode value round-trips as itself.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let key = string_term(&mut ctx, "OS_BIF_TEST_ROUNDTRIP");
    let value = string_term(&mut ctx, "V");

    assert_eq!(atom_name(&atoms, call(&reg, &mut ctx, "os", "putenv", 2, &[key, value]).expect("putenv")), "true");
    let got = call(&reg, &mut ctx, "os", "getenv", 1, &[key]).expect("getenv");
    assert_string(&reg, &mut ctx, got, "V");

    // Overwrite (`os:putenv/2` returns `true` for an existing key too).
    let second = string_term(&mut ctx, "2");
    assert_eq!(atom_name(&atoms, call(&reg, &mut ctx, "os", "putenv", 2, &[key, second]).expect("putenv")), "true");
    let got = call(&reg, &mut ctx, "os", "getenv", 1, &[key]).expect("getenv");
    assert_string(&reg, &mut ctx, got, "2");

    assert_eq!(atom_name(&atoms, call(&reg, &mut ctx, "os", "unsetenv", 1, &[key]).expect("unsetenv")), "true");
    assert_eq!(atom_name(&atoms, call(&reg, &mut ctx, "os", "getenv", 1, &[key]).expect("getenv")), "false");

    // A nested Unicode char list round-trips byte-for-byte.
    // ORACLE: `os:putenv("GEN_U","åäö")` => true, `os:getenv("GEN_U")` => "åäö".
    let ukey = string_term(&mut ctx, "OS_BIF_TEST_UNICODE");
    let inner = ctx.list(&[ctx.int(0xE4).expect("fits")]).expect("fits");
    let uvalue = ctx.list(&[
        ctx.int(0xE5).expect("fits"),
        inner,
        ctx.int(0xF6).expect("fits"),
    ]).expect("fits");
    assert_eq!(atom_name(&atoms, call(&reg, &mut ctx, "os", "putenv", 2, &[ukey, uvalue]).expect("putenv")), "true");
    let got = call(&reg, &mut ctx, "os", "getenv", 1, &[ukey]).expect("getenv");
    assert_string(&reg, &mut ctx, got, "åäö");

    // An empty value is stored and read back as `""`.
    // ORACLE: `os:putenv("GENZ","")` => true, `os:getenv("GENZ")` => "".
    let ekey = string_term(&mut ctx, "OS_BIF_TEST_EMPTY");
    assert_eq!(atom_name(&atoms, call(&reg, &mut ctx, "os", "putenv", 2, &[ekey, NIL]).expect("putenv")), "true");
    let got = call(&reg, &mut ctx, "os", "getenv", 1, &[ekey]).expect("getenv");
    assert!(is_nil_term(&reg, &mut ctx, got), "an empty value is NIL");
}

#[test]
fn env_and_getenv_reject_badarg_shapes() {
    // ORACLE: `os:putenv(nope,"V")`/`os:putenv("K",nope)` =>
    //         badarg; a binary key or value => badarg; an improper list or a NUL
    //         code point => badarg; `=` inside a key => badarg;
    //         `os:unsetenv(5)` => badarg.
    let reg = default_registry();
    // `os:env/0` below renders the WHOLE ambient environment as char lists, which
    // is far past the small default test heap, so size it from the live
    // environment (the `src/os_bif.rs` unit test uses the same formula).
    let (env_bytes, env_entries) =
        std::env::vars_os().fold((0usize, 0usize), |(bytes, entries), (k, v)| {
            (bytes + k.len() + v.len(), entries + 1)
        });
    let mut heap = Heap::new(2 * env_bytes + 8 * env_entries + 4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let key = string_term(&mut ctx, "OS_BIF_TEST_BADARG");
    let value = string_term(&mut ctx, "V");
    let atom_arg = ctx.atom("os_bif_test_atom").expect("seeded");
    let int_arg = ctx.int(5).expect("fits");

    let bin = binary_term(&reg, &mut ctx, b"x");
    let nul = ctx.list(&[ctx.int(0).expect("fits"), ctx.int(97).expect("fits")]).expect("fits");
    let equals = string_term(&mut ctx, "A=B");
    let improper = ctx.cons(ctx.int(97).expect("fits"), ctx.int(98).expect("fits")).expect("fits");
    let non_list = ctx.int(97).expect("fits");

    for bad_key in [atom_arg, int_arg, bin, nul, equals, improper, non_list] {
        assert!(
            call(&reg, &mut ctx, "os", "putenv", 2, &[bad_key, value]).is_err(),
            "a bad key must be badarg for putenv/2"
        );
        assert!(call(&reg, &mut ctx, "os", "getenv", 1, &[bad_key]).is_err());
        assert!(call(&reg, &mut ctx, "os", "unsetenv", 1, &[bad_key]).is_err());
    }
    for bad_value in [atom_arg, bin, nul, improper] {
        assert!(
            call(&reg, &mut ctx, "os", "putenv", 2, &[key, bad_value]).is_err(),
            "a bad value must be badarg for putenv/2"
        );
    }

    // `os:env/0` is a list of 2-tuples whose first element is a char list.
    // ORACLE: `hd(os:env())` => {"SHELL", "..."}.
    let env = call(&reg, &mut ctx, "os", "env", 0, &[]).expect("os:env/0");
    let first = call(&reg, &mut ctx, "erlang", "hd", 1, &[env]).expect("hd");
    assert_eq!(as_int(call(&reg, &mut ctx, "erlang", "tuple_size", 1, &[first]).expect("size")), 2);
    let one = ctx.int(1).expect("fits");
    let first_key = call(&reg, &mut ctx, "erlang", "element", 2, &[one, first]).expect("element");
    let bytes: Vec<u8> = list_elements(&reg, &mut ctx, first_key)
        .into_iter()
        .map(|e| as_int(e) as u8)
        .collect();
    assert!(!bytes.is_empty(), "an environment key is a non-empty string");
}

#[test]
fn getpid_is_a_string_of_digits() {
    // ORACLE: `os:getpid()` => "4" (a *string*, never an integer).
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let pid = call(&reg, &mut ctx, "os", "getpid", 0, &[]).expect("os:getpid/0");
    let bytes: Vec<u8> = list_elements(&reg, &mut ctx, pid)
        .into_iter()
        .map(|e| as_int(e) as u8)
        .collect();
    let text = String::from_utf8(bytes).expect("ASCII digits");
    assert!(!text.is_empty(), "the pid string must not be empty");
    assert!(text.bytes().all(|b| b.is_ascii_digit()), "pid = {text:?} is not a digit string");
    assert_eq!(text.parse::<u32>().expect("a u32 pid"), std::process::id());
}

// ---------------------------------------------------------------------------
// Clocks
// ---------------------------------------------------------------------------

#[test]
fn system_time_units_follow_the_oracle_set() {
    // ORACLE: each of `native`, `nanosecond`, `nano_seconds`, `perf_counter`
    //         equals `system_time(nanosecond)`; `seconds`/`millisecond`/
    //         `microsecond` (and the plural spellings, and the integer forms
    //         `1`/`1000`/`1000000`/`1000000000`) are the coarser readings.
    //         `fortnight`/`0`/`-1` are badarg.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let ns_atom = ctx.atom("nanosecond").expect("seeded");
    let sec_atom = ctx.atom("second").expect("seeded");
    let secs_atom = ctx.atom("seconds").expect("seeded");
    let ms_atom = ctx.atom("millisecond").expect("seeded");
    let mss_atom = ctx.atom("milli_seconds").expect("seeded");
    let us_atom = ctx.atom("microsecond").expect("seeded");
    let native_atom = ctx.atom("native").expect("seeded");
    let perf_atom = ctx.atom("perf_counter").expect("seeded");
    let thousand = ctx.int(1000).expect("fits");
    let million = ctx.int(1_000_000).expect("fits");

    // Epoch-nanosecond readings are heap bignums, so they must be read with
    // `as_big_int` (the small-int `as_int` would yield garbage).
    //
    // Each `system_time/1` call re-reads the wall clock, so the coarser unit
    // cannot be compared to a fixed earlier reading within ±1: instead bracket
    // every reading between a nanosecond reading taken *before* and one taken
    // *after*, and require `lo / ratio <= v <= hi / ratio` (`ratio` = ns/unit;
    // the BIF truncates, and integer division is monotone, so this bound is
    // exact, not slack).
    let lo = as_big_int(call(&reg, &mut ctx, "os", "system_time", 1, &[ns_atom]).expect("ns"));
    let mut readings: Vec<(i128, i128)> = Vec::new();
    for (unit, ratio) in [
        (ns_atom, 1),
        (native_atom, 1),
        (perf_atom, 1),
        (sec_atom, 1_000_000_000),
        (secs_atom, 1_000_000_000),
        (ms_atom, 1_000_000),
        (mss_atom, 1_000_000),
        (us_atom, 1_000),
    ] {
        let v = as_big_int(call(&reg, &mut ctx, "os", "system_time", 1, &[unit]).expect("unit"));
        readings.push((ratio, v));
    }
    // The integer forms: `1000` is milliseconds, `1000000` microseconds.
    let ms_int =
        as_big_int(call(&reg, &mut ctx, "os", "system_time", 1, &[thousand]).expect("1000"));
    let us_int =
        as_big_int(call(&reg, &mut ctx, "os", "system_time", 1, &[million]).expect("1e6"));
    let hi = as_big_int(call(&reg, &mut ctx, "os", "system_time", 1, &[ns_atom]).expect("ns"));

    for (ratio, v) in readings {
        assert!(
            lo / ratio <= v && v <= hi / ratio,
            "system_time(ratio {ratio}) = {v} is outside [{}, {}]",
            lo / ratio,
            hi / ratio
        );
    }
    assert!(lo / 1_000_000 <= ms_int && ms_int <= hi / 1_000_000);
    assert!(lo / 1_000 <= us_int && us_int <= hi / 1_000);

    // The /0 form is the native (nanosecond) reading, past the small range.
    let zero = call(&reg, &mut ctx, "os", "system_time", 0, &[]).expect("system_time/0");
    assert!(
        beam_codec::term::big_parts(zero).is_ok(),
        "an epoch-nanosecond count is a bignum"
    );

    // badarg cases.
    let fortnight = ctx.atom("fortnight").expect("seeded");
    for bad in [fortnight, ctx.int(0).expect("fits"), ctx.int(-1).expect("fits"), ctx.make_float(1.0).expect("fits")] {
        assert!(call(&reg, &mut ctx, "os", "system_time", 1, &[bad]).is_err());
    }
}

#[test]
fn timestamp_is_the_three_tuple() {
    // ORACLE: `os:timestamp()` => {MegaSecs, Secs, MicroSecs} (e.g.
    //         {1791,409099,250343}) with `Secs < 1000000`, `MicroSecs < 1000000`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let ts = call(&reg, &mut ctx, "os", "timestamp", 0, &[]).expect("os:timestamp/0");
    assert_eq!(as_int(call(&reg, &mut ctx, "erlang", "tuple_size", 1, &[ts]).expect("size")), 3);
    for i in 1..=3i64 {
        let index = ctx.int(i).expect("fits");
        let elem = call(&reg, &mut ctx, "erlang", "element", 2, &[index, ts]).expect("element");
        let value = as_int(elem);
        assert!(value >= 0, "element {i} = {value} is negative");
        if i >= 2 {
            assert!(value < 1_000_000, "element {i} = {value} is out of range");
        }
    }
}

#[test]
fn perf_counter_is_a_monotonic_integer() {
    // ORACLE: `os:perf_counter()` => 273373379456688 (an integer in the
    //         perf_counter unit, 1e9/s here); it never decreases. Its origin is
    //         opaque, so only the unit/monotonicity are pinned.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let a = as_int(call(&reg, &mut ctx, "os", "perf_counter", 0, &[]).expect("perf_counter"));
    let b = as_int(call(&reg, &mut ctx, "os", "perf_counter", 0, &[]).expect("perf_counter"));
    assert!(a >= 0, "perf_counter = {a} is negative");
    assert!(b >= a, "perf_counter decreased: {a} then {b}");
}

// ---------------------------------------------------------------------------
// `file` / `prim_file` name conversion
// ---------------------------------------------------------------------------

#[test]
fn native_name_encoding_is_utf8() {
    // ORACLE: `file:native_name_encoding()` => utf8.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let encoding = call(&reg, &mut ctx, "file", "native_name_encoding", 0, &[]).expect("encoding");
    assert_eq!(atom_name(&atoms, encoding), "utf8");
}

#[test]
fn internal_name2native_appends_a_nul() {
    // ORACLE: `prim_file:internal_name2native(<<"abc">>)` => <<97,98,99,0>>;
    //         `(<<>>)` => <<0>>; `("abc")` => <<97,98,99,0>>; `([])` => <<0>>;
    //         `(<<"åäö"/utf8>>)` => <<195,165,195,164,195,182,0>>;
    //         `(<<255,254>>)` => <<255,254,0>> (raw copy);
    //         `(abc)` => badarg; `(<<1:3>>)` => badarg (not byte aligned);
    //         `(<<97,0>>)` => badarg (NUL in the middle); `([1|2])` => badarg;
    //         `([16#110000])` => badarg.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let abc_bin = binary_term(&reg, &mut ctx, b"abc");
    let out = call(&reg, &mut ctx, "prim_file", "internal_name2native", 1, &[abc_bin]).expect("n2n");
    assert_eq!(binary_bytes(&reg, &mut ctx, out), b"abc\0");

    let empty_bin = binary_term(&reg, &mut ctx, b"");
    let out = call(&reg, &mut ctx, "prim_file", "internal_name2native", 1, &[empty_bin]).expect("n2n");
    assert_eq!(binary_bytes(&reg, &mut ctx, out), b"\0");

    let abc_list = string_term(&mut ctx, "abc");
    let out = call(&reg, &mut ctx, "prim_file", "internal_name2native", 1, &[abc_list]).expect("n2n");
    assert_eq!(binary_bytes(&reg, &mut ctx, out), b"abc\0");

    let out = call(&reg, &mut ctx, "prim_file", "internal_name2native", 1, &[NIL]).expect("n2n");
    assert_eq!(binary_bytes(&reg, &mut ctx, out), b"\0");

    let ua = ctx.list(&[ctx.int(0xE5).expect("fits"), ctx.int(0xE4).expect("fits"), ctx.int(0xF6).expect("fits")]).expect("fits");
    let out = call(&reg, &mut ctx, "prim_file", "internal_name2native", 1, &[ua]).expect("n2n");
    assert_eq!(binary_bytes(&reg, &mut ctx, out), b"\xC3\xA5\xC3\xA4\xC3\xB6\0");

    let raw = binary_term(&reg, &mut ctx, &[255, 254]);
    let out = call(&reg, &mut ctx, "prim_file", "internal_name2native", 1, &[raw]).expect("n2n");
    assert_eq!(binary_bytes(&reg, &mut ctx, out), &[255, 254, 0]);

    let atom_arg = ctx.atom("abc").expect("seeded");
    let nul_bin = binary_term(&reg, &mut ctx, &[97, 0]);
    let improper = ctx.cons(ctx.int(1).expect("fits"), ctx.int(2).expect("fits")).expect("fits");
    let past = ctx.list(&[ctx.int(0x11_0000).expect("fits")]).expect("fits");
    for bad in [atom_arg, nul_bin, improper, past] {
        assert!(
            call(&reg, &mut ctx, "prim_file", "internal_name2native", 1, &[bad]).is_err(),
            "internal_name2native must reject a bad argument"
        );
    }
}

#[test]
fn internal_native2name_and_is_translatable() {
    // ORACLE: `prim_file:internal_native2name(<<"abc">>)` => "abc";
    //         `(<<>>)` => []; `(<<"åäö"/utf8>>)` => "åäö";
    //         `(<<255,254>>)`/`(<<195>>)` => {error,warning};
    //         `(abc)`/`("abc")` => badarg.
    // ORACLE: `prim_file:is_translatable(<<>>)`/`(<<"abc">>)`/
    //         `(<<"åäö"/utf8>>)` => true; `(<<255,254>>)` => false;
    //         `(utf8)`/`(latin1)`/`("abc")` => badarg.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let abc = binary_term(&reg, &mut ctx, b"abc");
    let out = call(&reg, &mut ctx, "prim_file", "internal_native2name", 1, &[abc]).expect("n2n");
    assert_string(&reg, &mut ctx, out, "abc");
    assert_eq!(as_int(call(&reg, &mut ctx, "erlang", "length", 1, &[out]).expect("length")), 3);

    let empty = binary_term(&reg, &mut ctx, b"");
    let out = call(&reg, &mut ctx, "prim_file", "internal_native2name", 1, &[empty]).expect("n2n");
    assert!(is_nil_term(&reg, &mut ctx, out), "the empty binary decodes to []");

    let utf8 = binary_term(&reg, &mut ctx, &[0xC3, 0xA5, 0xC3, 0xA4, 0xC3, 0xB6]);
    let out = call(&reg, &mut ctx, "prim_file", "internal_native2name", 1, &[utf8]).expect("n2n");
    assert_string(&reg, &mut ctx, out, "åäö");

    for bad_utf8 in [vec![255u8, 254], vec![0xC3]] {
        let bin = binary_term(&reg, &mut ctx, &bad_utf8);
        let out = call(&reg, &mut ctx, "prim_file", "internal_native2name", 1, &[bin]).expect("n2n");
        assert_eq!(as_int(call(&reg, &mut ctx, "erlang", "tuple_size", 1, &[out]).expect("size")), 2);
        let one = ctx.int(1).expect("fits");
        let two = ctx.int(2).expect("fits");
        let tag = call(&reg, &mut ctx, "erlang", "element", 2, &[one, out]).expect("element");
        let reason = call(&reg, &mut ctx, "erlang", "element", 2, &[two, out]).expect("element");
        assert_eq!(atom_name(&atoms, tag), "error");
        assert_eq!(atom_name(&atoms, reason), "warning");
    }

    for good in [b"".to_vec(), b"abc".to_vec(), vec![0xC3, 0xA5]] {
        let bin = binary_term(&reg, &mut ctx, &good);
        let out = call(&reg, &mut ctx, "prim_file", "is_translatable", 1, &[bin]).expect("is_translatable");
        assert_eq!(atom_name(&atoms, out), "true");
    }
    let bad = binary_term(&reg, &mut ctx, &[255, 254]);
    let out = call(&reg, &mut ctx, "prim_file", "is_translatable", 1, &[bad]).expect("is_translatable");
    assert_eq!(atom_name(&atoms, out), "false");

    let utf8_atom = ctx.atom("utf8").expect("seeded");
    let latin1 = ctx.atom("latin1").expect("seeded");
    let as_list = string_term(&mut ctx, "abc");
    for bad in [utf8_atom, latin1, as_list] {
        assert!(call(&reg, &mut ctx, "prim_file", "is_translatable", 1, &[bad]).is_err());
        assert!(call(&reg, &mut ctx, "prim_file", "internal_native2name", 1, &[bad]).is_err());
    }

    // `prim_file:internal_normalize_utf8/1` is deliberately unregistered.
    let plain = binary_term(&reg, &mut ctx, b"abc");
    assert!(
        call(&reg, &mut ctx, "prim_file", "internal_normalize_utf8", 1, &[plain]).is_err(),
        "internal_normalize_utf8/1 must stay unregistered (no faithful NFC normalizer)"
    );
    // `os:set_signal/2` is deliberately unregistered.
    let sigterm = ctx.atom("abc").expect("seeded");
    assert!(call(&reg, &mut ctx, "os", "set_signal", 2, &[sigterm, sigterm]).is_err());
}

// ---------------------------------------------------------------------------
// Live oracle cross-check
// ---------------------------------------------------------------------------

#[test]
fn live_oracle_agrees_on_name_conversion_and_units() {
    // One live cross-check against the system ERTS: the shapes this module
    // pins above must also come out of the real `erl` today. Skips loudly when
    // the oracle is unavailable.
    let Some(output) = oracle_eval(
        "io:format(\"~p|~p|~p|~p|~w~n\",[\
             prim_file:internal_name2native([97,98,99]),\
             prim_file:internal_native2name(<<97,98,99>>),\
             prim_file:internal_native2name(<<255,254>>),\
             prim_file:is_translatable(<<195>>),\
             os:system_time(1000) =:= os:system_time(nanosecond) div 1000000])",
    ) else {
        return;
    };
    assert_eq!(
        output.trim(),
        "<<97,98,99,0>>|\"abc\"|{error,warning}|false|true",
        "the live oracle disagrees with the pinned goldens"
    );
}
