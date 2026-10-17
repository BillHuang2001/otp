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

//! Integration tests for the **integer-promotion seam**: every BIF that used to
//! report `system_limit` past the 60-bit small range now returns the exact bignum
//! the OTP 29 oracle returns, and the bignum *operands* the oracle accepts
//! (`integer_to_list/1,2`, `binary:encode_unsigned/1,2`, `float/1`) are accepted
//! too (public API only, driven through [`BifRegistry`]).
//!
//! Every expectation is pinned against the oracle (`erl -noshell -eval`), by
//! **limbs** — the `ErtsDigit` magnitude `beam_codec::term::big_parts` exposes,
//! least significant first — rather than by a re-rendered decimal string.

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{atom_val, make_atom, make_small, signed_value, AtomTable, Eterm, NIL};
/// `ERTS_MONOTONIC_TIME_START_EXTERNAL` (`erl_time.h:312-323`) — the origin of
/// `erlang:monotonic_time/0`, i.e. the value `erlang:system_info(start_time)`
/// answers on this build (ORACLE: `-576460752000000000`). The private constant it
/// mirrors lives in `src/time.rs`.
const ERTS_START: i64 = -576_460_752_000_000_000;

/// A registry + heap + atom table the tests drive BIF calls through.
struct Fixture {
    registry: BifRegistry,
    heap: Heap,
    atoms: AtomTable,
}

impl Fixture {
    fn new() -> Fixture {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "true",
            "false",
            "ok",
            "big",
            "little",
            "native",
            "second",
            "millisecond",
            "nanosecond",
        ]);
        Fixture {
            registry: default_registry(),
            heap: Heap::new(1 << 16).expect("test heap"),
            atoms,
        }
    }

    /// Calls `module:function/args.len()` through the registry.
    fn call(&mut self, module: &str, function: &str, args: &[Eterm]) -> Result<Eterm> {
        let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
        self.registry
            .call(module, function, args.len() as u32, &mut ctx, args)
    }

    /// Calls an `erlang` BIF.
    fn erlang(&mut self, function: &str, args: &[Eterm]) -> Result<Eterm> {
        self.call("erlang", function, args)
    }

    /// The atom term for a seeded name.
    fn atom(&self, name: &str) -> Eterm {
        make_atom(self.atoms.lookup(name).expect("seeded atom"))
    }

    /// The bignum term for `(negative, digits)`, built on the fixture's heap
    /// (digits least significant first).
    fn bignum(&mut self, negative: bool, digits: &[u64]) -> Eterm {
        beam_codec::term::make_big(&mut self.heap, negative, digits).expect("test bignum")
    }

    /// The float term for `value`, built on the fixture's heap.
    fn float(&mut self, value: f64) -> Eterm {
        beam_codec::term::make_float(&mut self.heap, value).expect("test float")
    }

    /// The binary term for `data`, built through `erlang:list_to_binary/1`.
    fn binary(&mut self, data: &[u8]) -> Eterm {
        let elems: Vec<Eterm> = data.iter().map(|&b| make_small(i64::from(b))).collect();
        let list = {
            let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
            ctx.list(&elems).expect("fits")
        };
        self.erlang("list_to_binary", &[list]).expect("list_to_binary/1")
    }
}

/// The `(negative, digits)` parts of a bignum result, copied out of its storage.
fn parts(term: Eterm) -> (bool, Vec<u64>) {
    let (negative, digits) = beam_codec::term::big_parts(term).expect("expected a bignum");
    (negative, digits.to_vec())
}

/// Whether `term` is a bignum (`big_parts` is the safe reading; the `beam_term`
/// predicate dereferences a header word).
fn is_bignum(term: Eterm) -> bool {
    beam_codec::term::big_parts(term).is_ok()
}

/// The value of a small-integer term.
fn small(term: Eterm) -> i64 {
    assert!(beam_term::is_small(term), "expected a small int, got {term:?}");
    signed_value(term)
}

/// Asserts a result is the exact bignum `(negative, digits)`.
fn assert_bignum(term: Eterm, negative: bool, digits: &[u64], what: &str) {
    assert!(is_bignum(term), "{what}: expected a bignum, got {term:?}");
    assert_eq!(parts(term), (negative, digits.to_vec()), "{what}");
}

/// The bytes of a binary result.
fn bytes(term: Eterm) -> Vec<u8> {
    let (data, offset, size) = beam_codec::term::bitstring_bytes(term).expect("a binary");
    assert_eq!(offset, 0, "a whole-byte binary");
    assert_eq!(size, data.len() * 8);
    data.to_vec()
}

/// The string spelled by a proper list of byte-sized small integers.
fn spell(mut term: Eterm) -> String {
    let mut out = String::new();
    while !beam_term::is_nil(term) {
        let head = beam_codec::term::cons_head(term).expect("a proper list");
        out.push(char::from(u8::try_from(small(head)).expect("a byte")));
        term = beam_codec::term::cons_tail(term).expect("a proper list");
    }
    out
}

/// The boolean value of a `true`/`false` atom term.
fn truth(fx: &Fixture, term: Eterm) -> bool {
    match fx.atoms.name(atom_val(term)) {
        Some("true") => true,
        Some("false") => false,
        other => panic!("expected a boolean atom, got {other:?}"),
    }
}

/// A proper list of the bytes of `text`, built on the fixture's heap.
fn str_list(fx: &mut Fixture, text: &str) -> Eterm {
    let mut ctx = BifContext::new(&mut fx.heap, &fx.atoms, ProcessId::new(1, 0));
    let mut head = NIL;
    for byte in text.bytes().rev() {
        head = ctx.cons(make_small(i64::from(byte)), head).expect("fits");
    }
    head
}

// ---------------------------------------------------------------------------
// system_time/0,1 and time_offset/0,1
// ---------------------------------------------------------------------------

#[test]
fn native_time_values_promote_to_bignums() {
    let mut fx = Fixture::new();
    let native = fx.atom("native");
    let second = fx.atom("second");
    let millisecond = fx.atom("millisecond");
    let nanosecond = fx.atom("nanosecond");
    let billion = make_small(1_000_000_000);
    let zero = make_small(0);

    // An epoch-nanosecond count has exceeded the 60-bit small range since 1988, so
    // `system_time/0`, `system_time(nanosecond)` and `system_time(native)` are
    // bignums — never `system_limit`, never truncated.
    let system = fx.erlang("system_time", &[]).expect("system_time/0");
    assert!(is_bignum(system), "system_time/0 must be a bignum");
    let term = fx.erlang("system_time", &[native]).expect("system_time/1");
    assert!(is_bignum(term), "system_time(native) must be a bignum");
    let term = fx.erlang("system_time", &[nanosecond]).expect("system_time/1");
    assert!(is_bignum(term), "system_time(nanosecond) must be a bignum");

    // `div 1_000_000_000` is the same value in whole seconds: a plausible epoch
    // (2020-01-01 .. 2100-01-01) at the oracle's scale.
    let secs = fx.erlang("div", &[system, billion]).expect("div/2");
    let secs_value = small(secs);
    assert!(
        (1_577_836_800..4_102_444_800).contains(&secs_value),
        "system_time()/1e9 = {secs_value} is not a plausible epoch value"
    );

    // The coarser units stay small integers.
    for unit in [second, millisecond] {
        let coarse = fx.erlang("system_time", &[unit]).expect("system_time/1");
        assert!(
            beam_term::is_small(coarse),
            "system_time of a coarse unit must be a small int"
        );
    }

    // `time_offset/0` is `system_time - monotonic_time` in native units, so it is
    // the same scale and likewise a bignum, non-negative, and — because the
    // monotonic reading is on the **ERTS epoch** (`start_time` =
    // `-576_460_752_000_000_000`) — it exceeds the system time by the whole
    // `|start_time|`: `offset/1e9` is `sys/1e9 + 576_460_752`.
    let offset = fx.erlang("time_offset", &[]).expect("time_offset/0");
    assert!(is_bignum(offset), "time_offset/0 must be a bignum");
    let non_negative = fx.erlang(">=", &[offset, zero]).expect(">=/2");
    assert!(truth(&fx, non_negative), "time_offset/0 must not be negative");
    let offset_secs = small(fx.erlang("div", &[offset, billion]).expect("div/2"));
    let epoch_secs = -ERTS_START / 1_000_000_000;
    assert!(
        (offset_secs - secs_value - epoch_secs).abs() <= 1,
        "time_offset()/1e9 = {offset_secs} is not the epoch {epoch_secs} past \
         the system time {secs_value}"
    );

    // The invariant `system_time = monotonic_time + time_offset` holds: the
    // difference is the monotonic reading itself, a **negative** small integer just
    // above `start_time` on the ERTS epoch (a few hundred ms of uptime past it).
    let difference = fx.erlang("-", &[system, offset]).expect("-/2");
    assert!(
        beam_term::is_small(difference),
        "system_time - time_offset is the monotonic reading"
    );
    let elapsed = small(difference);
    let uptime = elapsed - ERTS_START;
    // `system_time` and the offset are read a few microseconds apart, so the skewed
    // subtraction can land just below `start_time` — one millisecond of slack below,
    // and a generous ceiling above.
    assert!(
        (-1_000_000..1_000_000_000_000).contains(&uptime),
        "system_time - time_offset = {elapsed} is not start_time plus a plausible uptime"
    );

    // `monotonic_time/0` is a negative small integer on the ERTS epoch (ORACLE:
    // `-576460751768187266`), inside `start_time .. 0`.
    let monotonic = small(fx.erlang("monotonic_time", &[]).expect("monotonic_time/0"));
    assert!(monotonic < 0, "monotonic_time must be negative: {monotonic}");
    assert!(
        monotonic > ERTS_START,
        "monotonic_time {monotonic} must be above start_time {ERTS_START}"
    );
    // `time_offset(second)` / `time_offset(millisecond)` are small.
    for unit in [second, millisecond] {
        let offset = fx.erlang("time_offset", &[unit]).expect("time_offset/1");
        assert!(
            beam_term::is_small(offset),
            "time_offset of a coarse unit must be a small int"
        );
    }
}

// ---------------------------------------------------------------------------
// binary:decode_unsigned/1,2
// ---------------------------------------------------------------------------

#[test]
fn decode_unsigned_accepts_every_width_both_endians() {
    let mut fx = Fixture::new();
    let big = fx.atom("big");
    let little = fx.atom("little");

    // 8 bytes: the widest single-limb magnitude (`18446744073709551615`).
    let eight_ff = fx.binary(&[255; 8]);
    let term = fx
        .call("binary", "decode_unsigned", &[eight_ff])
        .expect("decode_unsigned/1");
    assert_bignum(term, false, &[18446744073709551615], "decode_unsigned(<<255 x8>>)");
    let term = fx
        .call("binary", "decode_unsigned", &[eight_ff, big])
        .expect("decode_unsigned/2");
    assert_bignum(
        term,
        false,
        &[18446744073709551615],
        "decode_unsigned(<<255 x8>>, big)",
    );

    // 9 bytes: `2^64`, one limb past a single `Uint` (`18446744073709551616`).
    let nine = fx.binary(&[1, 0, 0, 0, 0, 0, 0, 0, 0]);
    let term = fx
        .call("binary", "decode_unsigned", &[nine])
        .expect("decode_unsigned/1");
    assert_bignum(term, false, &[0, 1], "decode_unsigned(<<1,0 x8>>)");
    let term = fx
        .call("binary", "decode_unsigned", &[nine, big])
        .expect("decode_unsigned/2");
    assert_bignum(term, false, &[0, 1], "decode_unsigned(<<1,0 x8>>, big)");

    // The little-endian mirror (`<<0 x8,1>>`) is the same value; the little-endian
    // form of `<<1,0 x8>>` is `1`, a small integer.
    let little_nine = fx.binary(&[0, 0, 0, 0, 0, 0, 0, 0, 1]);
    let term = fx
        .call("binary", "decode_unsigned", &[little_nine, little])
        .expect("decode_unsigned/2");
    assert_bignum(term, false, &[0, 1], "decode_unsigned(<<0 x8,1>>, little)");
    let term = fx
        .call("binary", "decode_unsigned", &[nine, little])
        .expect("decode_unsigned/2");
    assert!(beam_term::is_small(term), "decode_unsigned(<<1,0 x8>>, little) is 1");
    assert_eq!(small(term), 1);

    // 9 bytes of `0xff` little-endian: `4722366482869645213695`.
    let nine_ff = fx.binary(&[255; 9]);
    let term = fx
        .call("binary", "decode_unsigned", &[nine_ff, little])
        .expect("decode_unsigned/2");
    assert_bignum(
        term,
        false,
        &[18446744073709551615, 255],
        "decode_unsigned(<<255 x9>>, little)",
    );

    // 10 bytes big-endian: `2^72`.
    let ten = fx.binary(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    let term = fx
        .call("binary", "decode_unsigned", &[ten])
        .expect("decode_unsigned/1");
    assert_bignum(term, false, &[0, 256], "decode_unsigned(2^72)");

    // 100 bytes of `0xff`: 13 limbs (12 saturated plus `0xffffffff`).
    let hundred = fx.binary(&[255; 100]);
    let term = fx
        .call("binary", "decode_unsigned", &[hundred])
        .expect("decode_unsigned/1");
    let mut expected = vec![18446744073709551615u64; 12];
    expected.push(4294967295);
    assert_bignum(term, false, &expected, "decode_unsigned(<<255 x100>>)");

    // Leading zero bytes are still stripped, and the empty binary is `0`.
    let padded = fx.binary(&[0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    let term = fx
        .call("binary", "decode_unsigned", &[padded])
        .expect("decode_unsigned/1");
    assert_bignum(term, false, &[0, 256], "leading zeros strip to 2^64");
    let empty = fx.binary(&[]);
    let term = fx
        .call("binary", "decode_unsigned", &[empty])
        .expect("decode_unsigned/1");
    assert!(beam_term::is_small(term));
    assert_eq!(small(term), 0);
}

// ---------------------------------------------------------------------------
// binary:encode_unsigned/1,2
// ---------------------------------------------------------------------------

#[test]
fn encode_unsigned_accepts_bignum_operands_both_endians() {
    let mut fx = Fixture::new();
    let big = fx.atom("big");
    let little = fx.atom("little");

    // `2^64` (two limbs) big-endian is nine bytes; little-endian is its mirror.
    let two_to_64 = fx.bignum(false, &[0, 1]);
    assert_eq!(
        bytes(fx.call("binary", "encode_unsigned", &[two_to_64]).expect("ok")),
        vec![1, 0, 0, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(
        bytes(
            fx.call("binary", "encode_unsigned", &[two_to_64, little])
                .expect("ok")
        ),
        vec![0, 0, 0, 0, 0, 0, 0, 0, 1]
    );
    assert_eq!(
        bytes(fx.call("binary", "encode_unsigned", &[two_to_64, big]).expect("ok")),
        vec![1, 0, 0, 0, 0, 0, 0, 0, 0]
    );

    // `2^70` big-endian keeps the high limb's leading byte (`0x40`), so its
    // minimal image is `<<64,0 x8>>` (from the oracle).
    let two_to_70 = fx.bignum(false, &[0, 64]);
    assert_eq!(
        bytes(fx.call("binary", "encode_unsigned", &[two_to_70]).expect("ok")),
        vec![64, 0, 0, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(
        bytes(
            fx.call("binary", "encode_unsigned", &[two_to_70, little])
                .expect("ok")
        ),
        vec![0, 0, 0, 0, 0, 0, 0, 0, 64]
    );

    // A bignum with a saturated low limb emits all nine bytes.
    let saturated = fx.bignum(false, &[18446744073709551615, 255]);
    assert_eq!(
        bytes(fx.call("binary", "encode_unsigned", &[saturated]).expect("ok")),
        vec![255; 9]
    );

    // The small-integer cases are unchanged (`0` ⇒ `<<0>>`).
    for (value, expected) in [
        (0i64, vec![0u8]),
        (1, vec![1]),
        (255, vec![255]),
        (256, vec![1, 0]),
    ] {
        let arg = make_small(value);
        assert_eq!(
            bytes(fx.call("binary", "encode_unsigned", &[arg]).expect("ok")),
            expected,
            "encode_unsigned({value})"
        );
    }

    // A negative bignum, a negative small and a float are `badarg`.
    let negative = fx.bignum(true, &[0, 64]);
    assert!(matches!(
        fx.call("binary", "encode_unsigned", &[negative]),
        Err(BeamError::BadArg)
    ));
    let float = fx.float(1.0);
    assert!(matches!(
        fx.call("binary", "encode_unsigned", &[float]),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        fx.call("binary", "encode_unsigned", &[make_small(-1)]),
        Err(BeamError::BadArg)
    ));
}

// ---------------------------------------------------------------------------
// list_to_integer / integer_to_list and the binary forms
// ---------------------------------------------------------------------------

#[test]
fn integer_string_conversions_promote_to_bignums() {
    let mut fx = Fixture::new();

    // `list_to_integer("123456789012345678901234567890")` — two limbs.
    let text = fx.binary(b"123456789012345678901234567890");
    let arg = str_list(&mut fx, "123456789012345678901234567890");
    let term = fx.erlang("list_to_integer", &[arg]).expect("list_to_integer/1");
    assert_bignum(
        term,
        false,
        &[14083847773837265618, 6692605942],
        "list_to_integer(123…890)",
    );
    let arg = str_list(&mut fx, "-123456789012345678901234567890");
    let negative = fx.erlang("list_to_integer", &[arg]).expect("list_to_integer/1");
    assert_bignum(
        negative,
        true,
        &[14083847773837265618, 6692605942],
        "list_to_integer(-123…890)",
    );

    // `2^59` is one past `MAX_SMALL`: the first magnitude to promote.
    let arg = str_list(&mut fx, "576460752303423488");
    let edge = fx.erlang("list_to_integer", &[arg]).expect("list_to_integer/1");
    assert_bignum(edge, false, &[576_460_752_303_423_488], "2^59");
    // `2^59 - 1` (= `MAX_SMALL`) is still a small integer.
    let arg = str_list(&mut fx, "576460752303423487");
    let max_small = fx.erlang("list_to_integer", &[arg]).expect("list_to_integer/1");
    assert!(beam_term::is_small(max_small));
    assert_eq!(small(max_small), beam_term::MAX_SMALL);

    // A non-decimal base is arbitrary precision too (28 hex digits = 2^108).
    let arg = str_list(&mut fx, "1000000000000000000000000000");
    let base16 = fx
        .erlang("list_to_integer", &[arg, make_small(16)])
        .expect("list_to_integer/2");
    assert_bignum(base16, false, &[0, 17592186044416], "2^108 in base 16");

    // `integer_to_list/1,2` renders a bignum exactly.
    let two_to_64 = fx.bignum(false, &[0, 1]);
    let rendered = fx
        .erlang("integer_to_list", &[two_to_64])
        .expect("integer_to_list/1");
    assert_eq!(spell(rendered), "18446744073709551616");
    let rendered = fx
        .erlang("integer_to_list", &[two_to_64, make_small(16)])
        .expect("integer_to_list/2");
    assert_eq!(spell(rendered), "10000000000000000");
    let two_to_70 = fx.bignum(false, &[0, 64]);
    let rendered = fx
        .erlang("integer_to_list", &[two_to_70, make_small(16)])
        .expect("integer_to_list/2");
    assert_eq!(spell(rendered), "400000000000000000");
    let negative = fx.bignum(true, &[0, 64]);
    let rendered = fx
        .erlang("integer_to_list", &[negative, make_small(16)])
        .expect("integer_to_list/2");
    assert_eq!(spell(rendered), "-400000000000000000");
    let rendered = fx.erlang("integer_to_list", &[make_small(0)]).expect("ok");
    assert_eq!(spell(rendered), "0");

    // `list_to_integer` ∘ `integer_to_list` is the identity for a bignum.
    let rendered = fx
        .erlang("integer_to_list", &[two_to_70])
        .expect("integer_to_list/1");
    let back = fx.erlang("list_to_integer", &[rendered]).expect("list_to_integer/1");
    assert_bignum(back, false, &[0, 64], "integer_to_list/list_to_integer round-trip");

    // The binary forms: `binary_to_integer/1,2` and `integer_to_binary/1,2`.
    let term = fx.erlang("binary_to_integer", &[text]).expect("binary_to_integer/1");
    assert_bignum(
        term,
        false,
        &[14083847773837265618, 6692605942],
        "binary_to_integer/1",
    );
    let hex = fx.binary(b"1000000000000000000000000000");
    let term = fx
        .erlang("binary_to_integer", &[hex, make_small(16)])
        .expect("binary_to_integer/2");
    assert_bignum(term, false, &[0, 17592186044416], "binary_to_integer/2");
    assert_eq!(
        bytes(fx.erlang("integer_to_binary", &[two_to_64]).expect("ok")),
        b"18446744073709551616"
    );
    assert_eq!(
        bytes(
            fx.erlang("integer_to_binary", &[two_to_64, make_small(16)])
                .expect("ok")
        ),
        b"10000000000000000"
    );

    // Malformed input stays `badarg`; a bad base too.
    for bad in ["", " 12", "12 ", "12.0", "-", "+", "a", "1a"] {
        let arg = str_list(&mut fx, bad);
        assert!(
            matches!(fx.erlang("list_to_integer", &[arg]), Err(BeamError::BadArg)),
            "list_to_integer({bad:?})"
        );
    }
    let digits = str_list(&mut fx, "10");
    for base in [1i64, 37, 0] {
        assert!(
            matches!(
                fx.erlang("list_to_integer", &[digits, make_small(base)]),
                Err(BeamError::BadArg)
            ),
            "base {base}"
        );
    }
    let float = fx.float(1.0);
    assert!(matches!(
        fx.erlang("integer_to_list", &[float]),
        Err(BeamError::BadArg)
    ));
}

// ---------------------------------------------------------------------------
// float/1, round/1, trunc/1, ceil/1, floor/1
// ---------------------------------------------------------------------------

#[test]
fn float_of_a_bignum_rounds_to_the_nearest_double() {
    let mut fx = Fixture::new();

    // Oracle: `float(18446744073709551616)` ⇒ 1.8446744073709552e19.
    let two_to_64 = fx.bignum(false, &[0, 1]);
    let term = fx.erlang("float", &[two_to_64]).expect("float/1");
    assert_eq!(
        beam_codec::term::float_value(term).expect("a float"),
        1.8446744073709552e19
    );

    // `float(12345678901234567890)` ⇒ 1.2345678901234567e19.
    let nineteen = fx.bignum(false, &[12345678901234567890]);
    let term = fx.erlang("float", &[nineteen]).expect("float/1");
    assert_eq!(
        beam_codec::term::float_value(term).expect("a float"),
        1.2345678901234567e19
    );

    // `float(-(1 bsl 70))` ⇒ -1.1805916207174113e21.
    let negative = fx.bignum(true, &[0, 64]);
    let term = fx.erlang("float", &[negative]).expect("float/1");
    assert_eq!(
        beam_codec::term::float_value(term).expect("a float"),
        -1.1805916207174113e21
    );

    // `float(1 bsl 2000)` is `badarg` in the oracle (the magnitude is not a finite
    // `f64`).
    let mut huge = vec![0u64; 31];
    huge.push(1 << 16);
    let huge = fx.bignum(false, &huge);
    assert!(matches!(fx.erlang("float", &[huge]), Err(BeamError::BadArg)));

    // A small integer and a float keep working.
    let one = fx.erlang("float", &[make_small(1)]).expect("float/1");
    assert_eq!(beam_codec::term::float_value(one).expect("a float"), 1.0);
    let float = fx.float(1.5);
    assert_eq!(fx.erlang("float", &[float]).expect("float/1"), float);
}

/// `float/1` at the finite-double boundary: the exact nearest double just below,
/// `badarg` just above — the `beam_term::number::big_to_f64` `None` contract.
#[test]
fn float_at_the_finite_double_boundary() {
    let mut fx = Fixture::new();

    // `(1 bsl 1024) - (1 bsl 971) = (2^53 - 1)·2^971` is the largest finite double.
    // Oracle: `float((1 bsl 1024) - (1 bsl 971))` ⇒ 1.7976931348623157e308.
    let mut max_limbs = vec![0u64; 16];
    max_limbs[15] = ((1u64 << 53) - 1) << 11;
    let max = fx.bignum(false, &max_limbs);
    let term = fx.erlang("float", &[max]).expect("float/1");
    assert_eq!(beam_codec::term::float_value(term).expect("a float"), f64::MAX);

    // One half-ulp above — `(1 bsl 1024) - (1 bsl 970)` (bits 970..=1023) — rounds up
    // to `2^1024`, not a finite double, so `float/1` is `badarg`, as the oracle.
    let mut carry_limbs = vec![0u64; 16];
    carry_limbs[15] = u64::MAX << 10;
    let carry = fx.bignum(false, &carry_limbs);
    assert!(matches!(fx.erlang("float", &[carry]), Err(BeamError::BadArg)));
    // `1 bsl 1024` (bit length 1025) is the same failure.
    let mut over = vec![0u64; 17];
    over[16] = 1;
    let over = fx.bignum(false, &over);
    assert!(matches!(fx.erlang("float", &[over]), Err(BeamError::BadArg)));

    // Ties to even: `float((1 bsl 53) + 1)` ⇒ 9.007199254740992e15 (`2^53`).
    let tie = fx.bignum(false, &[(1u64 << 53) + 1]);
    let term = fx.erlang("float", &[tie]).expect("float/1");
    assert_eq!(
        beam_codec::term::float_value(term).expect("a float"),
        9007199254740992.0
    );
}

#[test]
fn round_trunc_ceil_floor_promote_past_i64() {
    let mut fx = Fixture::new();

    // Oracle-pinned exact values, as limbs.
    let cases: [(f64, bool, Vec<u64>); 4] = [
        (1.0e18, false, vec![1000000000000000000]),
        (1.0e30, false, vec![5076964154930102272, 54210108624]),
        (-1.0e30, true, vec![5076964154930102272, 54210108624]),
        (
            1.0e300,
            false,
            vec![
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 8474648598804430848, 102613420032,
            ],
        ),
    ];

    for (value, negative, digits) in cases.iter() {
        let arg = fx.float(*value);
        for function in ["round", "trunc", "ceil", "floor"] {
            let term = fx.erlang(function, &[arg]).expect(function);
            assert_bignum(term, *negative, digits, &format!("{function}({value})"));
        }
    }

    // `round(1.0e300)` is the exact 997-bit double, not a saturated integer.
    let (_, _, digits) = cases.last().expect("a case");
    assert_eq!(digits.len(), 16, "the 997-bit magnitude needs 16 limbs");
    let arg = fx.float(1.0e300);
    let term = fx.erlang("round", &[arg]).expect("round");
    assert_bignum(term, false, digits, "round(1.0e300)");

    // A **small integer** past `2^53` is returned unchanged: going through `f64`
    // would round `576460752303423487` up to `576460752303423488`.
    let odd = make_small(576_460_752_303_423_487);
    for function in ["round", "trunc", "ceil", "floor"] {
        assert_eq!(fx.erlang(function, &[odd]).expect(function), odd);
    }

    // A bignum integer argument is returned unchanged too.
    let big = fx.bignum(false, &[0, 1]);
    for function in ["round", "trunc", "ceil", "floor"] {
        assert_eq!(fx.erlang(function, &[big]).expect(function), big);
    }

    // The small cases are unchanged.
    let half = fx.float(1.5);
    assert_eq!(small(fx.erlang("round", &[half]).expect("round")), 2);
    assert_eq!(small(fx.erlang("trunc", &[half]).expect("trunc")), 1);

    // An infinite float is never turned into a saturated integer.
    let infinite = fx.float(f64::INFINITY);
    assert!(matches!(
        fx.erlang("trunc", &[infinite]),
        Err(BeamError::BadArg)
    ));
}
