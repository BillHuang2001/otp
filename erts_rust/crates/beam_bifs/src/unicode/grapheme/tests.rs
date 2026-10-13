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

//! The **oracle corpus** for the grapheme-cluster port, with its two gates.
//!
//! * [`the_corpus_matches_the_oracle_bit_for_bit`] — the committed goldens: each
//!   row is the verbatim `~w` rendering of the system oracle's answer (OTP 29.1.1 /
//!   erts-17.1) for the same input, so the behaviour is pinned even in a sandbox
//!   without `erl`.
//! * [`the_live_oracle_agrees_on_the_whole_corpus`] — re-derives the *same* corpus
//!   against the live oracle (`string:next_grapheme/1`, `string:length/1`) and
//!   requires the `erlang:term_to_binary/1` image of every answer to agree. A
//!   missing or failing oracle is a **failure**, never a silent pass: the defect
//!   class this guards against is a silently wrong result, and skipping the check
//!   would re-create exactly that blindness.
//!
//! The corpus lives here rather than in `mod.rs` because it is *data plus a
//! harness*, not part of the walk; keeping it inline would push `mod.rs` well past
//! the crate's ~1000-line guideline.
//!
//! Both gates drive the port through the module's frozen `pub(crate)` surface —
//! `super::{next_grapheme, grapheme_count}`, i.e. the very path a consumer outside
//! this module resolves.

use super::*;
use beam_alloc::Heap;
use beam_codec::term::{bitstring_bytes, make_big, make_binary, make_bitstring};
use beam_common::BeamError;
use beam_process::ProcessId;
use beam_term::compare::eq;
use beam_term::AtomTable;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// A term in the corpus, either as input or as an expected value.
#[derive(Clone, Debug)]
enum Expr {
    Int(i64),
    /// `1 bsl k` — a bignum.
    BigPow2(u32),
    Float(f64),
    Bin(Vec<u8>),
    /// `<<bytes:bits>>`.
    Bits(Vec<u8>, usize),
    Nil,
    List(Vec<Expr>),
    Cons(Box<Expr>, Box<Expr>),
    Tuple(Vec<Expr>),
    /// `{error, X}`.
    ErrorTuple(Box<Expr>),
}

/// What the oracle does for a case.
#[derive(Clone, Debug)]
enum Outcome {
    Value(Expr),
    /// A bare `error` reason atom (`function_clause`, `badarg`).
    AtomRaise(&'static str),
    /// A structured `{badarg, X}` raise.
    BadArgRaise(Expr),
}

struct Case {
    name: &'static str,
    input: Expr,
    next: Outcome,
    len: Outcome,
}

/// The atoms every gate needs: `error` (the `{error, Rest}` shape `cp/1` builds),
/// the two booleans, the two bare-raise reasons the corpus pins and `raised` (the
/// tag the live-oracle harness puts on a caught raise).
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["error", "true", "false", "badarg", "function_clause", "raised"]);
    atoms
}

fn pow2_limbs(k: u32) -> Vec<u64> {
    let mut limbs = vec![0u64; (k / 36) as usize + 1];
    limbs[(k / 36) as usize] = 1u64 << (k % 36);
    limbs
}

fn build(ctx: &mut BifContext<'_>, expr: &Expr) -> Eterm {
    match expr {
        Expr::Int(value) => ctx.int(*value).expect("a small int fits"),
        Expr::BigPow2(k) => make_big(ctx.heap(), false, &pow2_limbs(*k)).expect("a bignum"),
        Expr::Float(value) => ctx.make_float(*value).expect("a float"),
        Expr::Bin(bytes) => make_binary(ctx.heap(), bytes).expect("a binary"),
        Expr::Bits(bytes, bits) => {
            make_bitstring(ctx.heap(), bytes, *bits).expect("a bitstring")
        }
        Expr::Nil => NIL,
        Expr::List(items) => {
            let items: Vec<Eterm> = items.iter().map(|item| build(ctx, item)).collect();
            ctx.list(&items).expect("a short list fits")
        }
        Expr::Cons(head, tail) => {
            let head = build(ctx, head);
            let tail = build(ctx, tail);
            ctx.cons(head, tail).expect("a cons cell fits")
        }
        Expr::Tuple(items) => {
            let items: Vec<Eterm> = items.iter().map(|item| build(ctx, item)).collect();
            ctx.tuple(&items).expect("a short tuple fits")
        }
        Expr::ErrorTuple(payload) => {
            let tag = ctx.atom("error").expect("the atom `error` is seeded");
            let payload = build(ctx, payload);
            ctx.tuple(&[tag, payload]).expect("a tuple fits")
        }
    }
}

/// The live reason term of a structured raise.
fn live_reason(error: &BeamError) -> Eterm {
    match error {
        BeamError::Raise(raise) => raise
            .live_reason()
            .and_then(|live| live.downcast_ref::<Eterm>().copied())
            .expect("a structured raise carries its reason term"),
        other => panic!("expected a raise, got {other:?}"),
    }
}

fn check(
    ctx: &mut BifContext<'_>,
    atoms: &AtomTable,
    case: &Case,
    what: &str,
    got: Result<Eterm>,
    want: &Outcome,
) {
    match (got, want) {
        (Ok(got), Outcome::Value(want)) => {
            let want = build(ctx, want);
            assert!(
                eq(got, want, atoms),
                "case `{}` {what}: got a different term",
                case.name
            );
        }
        (Err(error), Outcome::AtomRaise(atom)) => {
            assert_eq!(
                error.as_atom(),
                Some(*atom),
                "case `{}` {what}: wrong raise",
                case.name
            );
        }
        (Err(error), Outcome::BadArgRaise(payload)) => {
            assert_eq!(error.as_atom(), None, "case `{}` {what}", case.name);
            // The raise's live reason is the oracle's **whole** `{badarg, Payload}`
            // tuple (`length_gc/2` / `length_b/4` raise `error({badarg, Err})`), so
            // the row's payload is re-tagged here before the comparison.
            let payload = build(ctx, payload);
            let tag = ctx.atom("badarg").expect("`badarg` is seeded");
            let want = ctx.tuple(&[tag, payload]).expect("a tuple fits");
            let got = live_reason(&error);
            assert!(
                eq(got, want, atoms),
                "case `{}` {what}: wrong `{{badarg, _}}` payload",
                case.name
            );
        }
        (Ok(_), want) => panic!("case `{}` {what}: expected {want:?}, got a value", case.name),
        (Err(error), want) => panic!(
            "case `{}` {what}: expected {want:?}, got the raise {error:?}",
            case.name
        ),
    }
}

/// `nil` — a helper so the table reads like the oracle's `~w` output.
fn ints(values: &[i64]) -> Expr {
    Expr::List(values.iter().copied().map(Expr::Int).collect())
}

fn bytes(bytes: &[u8]) -> Expr {
    Expr::Bin(bytes.to_vec())
}

/// Every corpus case: `(input, next_grapheme, string:length)`.
///
/// Each row is the verbatim `~w` rendering of the system oracle's answer
/// (OTP 29.1.1 / erts-17.1); a row is a `Value`, a bare `badarg`, or a bare
/// `function_clause`/`{badarg, X}` raise.
fn cases() -> Vec<Case> {
    fn case(
        name: &'static str,
        input: Expr,
        next: Outcome,
        len: Outcome,
    ) -> Case {
        Case {
            name,
            input,
            next,
            len,
        }
    }
    fn value(expr: Expr) -> Outcome {
        Outcome::Value(expr)
    }
    fn err(payload: Expr) -> Outcome {
        Outcome::BadArgRaise(payload)
    }
    let fc = Outcome::AtomRaise("function_clause");
    let ba = Outcome::AtomRaise("badarg");
    vec![
        case(
            "combining-mark list",
            ints(&[97, 769, 98]),
            value(Expr::List(vec![ints_raw(&[97, 769]), Expr::Int(98)])),
            value(Expr::Int(2)),
        ),
        case(
            "combining-mark binary",
            bytes(&[97, 204, 129, 98]),
            value(Expr::Cons(
                Box::new(ints_raw(&[97, 769])),
                Box::new(bytes(&[98])),
            )),
            value(Expr::Int(2)),
        ),
        case(
            "cjk binary tail",
            bytes(&[230, 151, 165, 226, 130, 172]),
            value(Expr::Cons(
                Box::new(Expr::Int(26085)),
                Box::new(bytes(&[226, 130, 172])),
            )),
            value(Expr::Int(2)),
        ),
        case("two latin1", ints(&[255, 254]), value(ints(&[255, 254])), value(Expr::Int(2))),
        case(
            "binary inside a list",
            Expr::List(vec![bytes(b"abc"), Expr::Int(100)]),
            value(Expr::List(vec![
                Expr::Int(97),
                bytes(&[98, 99]),
                Expr::Int(100),
            ])),
            value(Expr::Int(4)),
        ),
        case(
            "nested lists",
            Expr::List(vec![ints(&[97, 98]), ints(&[99, 100])]),
            value(ints(&[97, 98, 99, 100])),
            value(Expr::Int(4)),
        ),
        case(
            "empty binary then a cp",
            Expr::List(vec![bytes(b""), Expr::Int(97)]),
            value(ints(&[97])),
            value(Expr::Int(1)),
        ),
        case(
            "improper cons with a binary tail",
            Expr::Cons(Box::new(Expr::Int(97)), Box::new(bytes(&[98, 99]))),
            value(Expr::Cons(Box::new(Expr::Int(97)), Box::new(bytes(&[98, 99])))),
            value(Expr::Int(3)),
        ),
        case(
            "improper cons with an integer tail",
            Expr::Cons(Box::new(Expr::Int(97)), Box::new(Expr::Int(98))),
            fc.clone(),
            fc.clone(),
        ),
        case(
            "truncated utf-8 tail",
            bytes(&[97, 204, 129, 200]),
            value(Expr::List(vec![ints_raw(&[97, 769]), bytes(&[200])])),
            err(bytes(&[200])),
        ),
        // A stray continuation byte at the head of a binary: `cp/1` has no
        // clause for it, so the whole (unconsumed) binary comes back.
        case(
            "invalid utf-8 head",
            bytes(&[169, 200]),
            value(Expr::ErrorTuple(Box::new(bytes(&[169, 200])))),
            err(bytes(&[169, 200])),
        ),
        case(
            "invalid utf-8 head (cont)",
            bytes(&[169, 199]),
            value(Expr::ErrorTuple(Box::new(bytes(&[169, 199])))),
            err(bytes(&[169, 199])),
        ),
        case(
            "invalid utf-8 head (ascii)",
            bytes(&[169, 97]),
            value(Expr::ErrorTuple(Box::new(bytes(&[169, 97])))),
            err(bytes(&[169, 97])),
        ),
        case(
            "invalid utf-8 head (two lead bytes)",
            bytes(&[200, 201]),
            value(Expr::ErrorTuple(Box::new(bytes(&[200, 201])))),
            err(bytes(&[200, 201])),
        ),
        case(
            "invalid utf-8 head (nul)",
            bytes(&[169, 0]),
            value(Expr::ErrorTuple(Box::new(bytes(&[169, 0])))),
            err(bytes(&[169, 0])),
        ),
        case(
            "invalid utf-8 head (single byte)",
            bytes(&[169]),
            value(Expr::ErrorTuple(Box::new(bytes(&[169])))),
            err(bytes(&[169])),
        ),
        case(
            "invalid utf-8 head (single continuation)",
            bytes(&[200]),
            value(Expr::ErrorTuple(Box::new(bytes(&[200])))),
            err(bytes(&[200])),
        ),
        // 173 (`SOFT HYPHEN`) is a control singleton: it must be caught before
        // the Latin-1 fast path, so it does not absorb the following extend.
        case(
            "soft hyphen is a control",
            ints(&[173, 769, 97]),
            value(ints(&[173, 769, 97])),
            value(Expr::Int(3)),
        ),
        case(
            "prepend then an ascii cp",
            ints(&[1536, 97]),
            value(Expr::List(vec![ints_raw(&[1536, 97])])),
            value(Expr::Int(1)),
        ),
        case(
            "prepend then a combining mark",
            ints(&[1536, 769, 97]),
            value(Expr::List(vec![ints_raw(&[1536, 769]), Expr::Int(97)])),
            value(Expr::Int(2)),
        ),
        case(
            "prepend then a control",
            ints(&[1536, 10, 97]),
            value(ints(&[1536, 10, 97])),
            value(Expr::Int(3)),
        ),
        case(
            "regional indicator pair",
            ints(&[127462, 127463, 127464, 127465, 97]),
            value(Expr::List(vec![
                ints_raw(&[127462, 127463]),
                Expr::Int(127464),
                Expr::Int(127465),
                Expr::Int(97),
            ])),
            value(Expr::Int(3)),
        ),
        case(
            "hangul L+V then a control",
            ints(&[1100, 1161, 11, 97]),
            value(Expr::List(vec![
                ints_raw(&[1100, 1161]),
                Expr::Int(11),
                Expr::Int(97),
            ])),
            value(Expr::Int(3)),
        ),
        case(
            "hangul L+V+T",
            ints(&[4352, 4449, 4520, 97]),
            value(Expr::List(vec![ints_raw(&[4352, 4449, 4520]), Expr::Int(97)])),
            value(Expr::Int(2)),
        ),
        case(
            "hangul LV+T",
            ints(&[44032, 4520, 97]),
            value(Expr::List(vec![ints_raw(&[44032, 4520]), Expr::Int(97)])),
            value(Expr::Int(2)),
        ),
        case(
            "hangul L+L",
            ints(&[4352, 4352, 97]),
            value(Expr::List(vec![ints_raw(&[4352, 4352]), Expr::Int(97)])),
            value(Expr::Int(2)),
        ),
        case(
            "indic consonant + zwj",
            ints(&[2325, 8205, 2326, 97]),
            value(Expr::List(vec![ints_raw(&[2325, 8205]), Expr::Int(2326), Expr::Int(97)])),
            value(Expr::Int(3)),
        ),
        case(
            "indic consonant + vowel sign",
            ints(&[2325, 2366, 97]),
            value(Expr::List(vec![ints_raw(&[2325, 2366]), Expr::Int(97)])),
            value(Expr::Int(2)),
        ),
        case(
            "indic conjunct cluster",
            ints(&[2325, 2366, 2381, 2326, 97]),
            value(Expr::List(vec![
                ints_raw(&[2325, 2366, 2381, 2326]),
                Expr::Int(97),
            ])),
            value(Expr::Int(2)),
        ),
        case(
            "lone indic linker",
            ints(&[2381, 97]),
            value(ints(&[2381, 97])),
            value(Expr::Int(2)),
        ),
        case(
            "emoji zwj sequence",
            ints(&[128104, 8205, 128105, 97]),
            value(Expr::List(vec![
                ints_raw(&[128104, 8205, 128105]),
                Expr::Int(97),
            ])),
            value(Expr::Int(2)),
        ),
        case("empty list", Expr::Nil, value(Expr::Nil), value(Expr::Int(0))),
        case("empty binary", bytes(b""), value(Expr::Nil), value(Expr::Int(0))),
        case("single cp", ints(&[400]), value(ints(&[400])), value(Expr::Int(1))),
        case(
            "single cp then ascii",
            ints(&[400, 97]),
            value(ints(&[400, 97])),
            value(Expr::Int(2)),
        ),
        case("out-of-range cp", ints(&[1114112]), fc.clone(), fc.clone()),
        case(
            "negative then ascii",
            Expr::List(vec![Expr::Int(-1), Expr::Int(97)]),
            fc.clone(),
            fc.clone(),
        ),
        case(
            "bignum",
            Expr::List(vec![Expr::BigPow2(70)]),
            fc.clone(),
            fc.clone(),
        ),
        case("float", Expr::Float(1.5), fc.clone(), fc.clone()),
        case(
            "tuple",
            Expr::Tuple(vec![Expr::Int(1), Expr::Int(2)]),
            fc.clone(),
            fc.clone(),
        ),
        case(
            "non-byte-aligned bitstring",
            Expr::Bits(vec![1], 1),
            fc.clone(),
            fc.clone(),
        ),
        case(
            "one nested list",
            Expr::List(vec![ints(&[97, 98])]),
            value(ints(&[97, 98])),
            value(Expr::Int(2)),
        ),
        case(
            "improper tail inside a list",
            Expr::List(vec![Expr::Int(97), Expr::Cons(Box::new(Expr::Int(98)), Box::new(Expr::Int(99)))]),
            // `gc/1`'s Latin-1 fast path does not fire (`[98|99]` is not a Latin-1
            // integer head), so `cp/1` flattens the nested tail: the oracle answers
            // the whole input as the **flat** improper chain `[97,98|99]`
            // (`string:next_grapheme([97,[98|99]])`, live-oracle cross-checked by
            // `the_live_oracle_agrees_on_the_whole_corpus`).
            value(Expr::Cons(
                Box::new(Expr::Int(97)),
                Box::new(Expr::Cons(Box::new(Expr::Int(98)), Box::new(Expr::Int(99)))),
            )),
            fc.clone(),
        ),
        case(
            "binary then an empty binary",
            Expr::List(vec![bytes(&[97]), bytes(b"")]),
            value(Expr::List(vec![Expr::Int(97), bytes(b""), bytes(b"")])),
            value(Expr::Int(1)),
        ),
        case("nul", ints(&[0]), value(ints(&[0])), value(Expr::Int(1))),
        case(
            "crlf then ascii",
            ints(&[13, 10, 97]),
            value(Expr::List(vec![ints_raw(&[13, 10]), Expr::Int(97)])),
            value(Expr::Int(2)),
        ),
        case(
            "ascii then crlf",
            ints(&[97, 13, 10, 98]),
            value(ints(&[97, 13, 10, 98])),
            value(Expr::Int(3)),
        ),
        case(
            "two combining marks",
            ints(&[776, 769]),
            value(Expr::List(vec![ints_raw(&[776, 769])])),
            value(Expr::Int(1)),
        ),
        case(
            "two prepends",
            ints(&[1536, 1537]),
            value(Expr::List(vec![ints_raw(&[1536, 1537])])),
            value(Expr::Int(1)),
        ),
        case("zwj alone", ints(&[8205]), value(ints(&[8205])), value(Expr::Int(1))),
        case(
            "ascii then crlf (no trailing)",
            ints(&[97, 13, 10]),
            value(ints(&[97, 13, 10])),
            value(Expr::Int(2)),
        ),
        case(
            "three nested lists",
            Expr::List(vec![ints(&[97, 98]), ints(&[99, 100]), ints(&[101, 102])]),
            value(Expr::List(vec![
                Expr::Int(97),
                Expr::Int(98),
                Expr::Int(99),
                Expr::Int(100),
                ints_raw(&[101, 102]),
            ])),
            value(Expr::Int(6)),
        ),
        case(
            "surrogate-range integer",
            ints(&[97, 204, 129, 200, 201]),
            value(ints(&[97, 204, 129, 200, 201])),
            value(Expr::Int(5)),
        ),
        case(
            "extended pictographic below 256, binary roll-back",
            Expr::List(vec![Expr::Int(169), bytes(&[200])]),
            value(Expr::List(vec![
                Expr::Int(169),
                bytes(&[200]),
                bytes(&[200]),
            ])),
            err(Expr::List(vec![bytes(&[200]), bytes(&[200])])),
        ),
        case(
            "extended pictographic then a suffix",
            Expr::List(vec![Expr::Int(169), bytes(&[200]), Expr::Int(97)]),
            value(Expr::List(vec![
                Expr::Int(169),
                bytes(&[200]),
                Expr::Int(97),
                Expr::List(vec![bytes(&[200]), Expr::Int(97)]),
            ])),
            err(Expr::List(vec![
                bytes(&[200]),
                Expr::Int(97),
                Expr::List(vec![bytes(&[200]), Expr::Int(97)]),
            ])),
        ),
        case(
            "extended pictographic 174, binary roll-back",
            Expr::List(vec![Expr::Int(174), bytes(&[200])]),
            value(Expr::List(vec![
                Expr::Int(174),
                bytes(&[200]),
                bytes(&[200]),
            ])),
            err(Expr::List(vec![bytes(&[200]), bytes(&[200])])),
        ),
        // The saved prefix is a binary here, so the `++` in `gc_ext_pict/3`
        // raises the bare `badarg` (never `{badarg, _}`).
        case(
            "extended pictographic with a binary prefix",
            Expr::List(vec![
                Expr::List(vec![Expr::Int(169)]),
                bytes(&[200]),
            ]),
            ba.clone(),
            ba.clone(),
        ),
        case(
            "one nested list of one cp",
            Expr::List(vec![ints(&[169, 200])]),
            value(ints(&[169, 200])),
            value(Expr::Int(2)),
        ),
        case(
            "soft hyphen then a cp",
            ints(&[169, 200]),
            value(ints(&[169, 200])),
            value(Expr::Int(2)),
        ),
        case("control 8225", ints(&[8225, 97]), value(ints(&[8225, 97])), value(Expr::Int(2))),
        case("ascii 111", ints(&[111]), value(ints(&[111])), value(Expr::Int(1))),
        case("ascii 128", ints(&[128]), value(ints(&[128])), value(Expr::Int(1))),
        case(
            "ascii 128 twice",
            ints(&[128, 128]),
            value(ints(&[128, 128])),
            value(Expr::Int(2)),
        ),
        case("indic linker+consonant", ints(&[2381, 2326, 97]), value(ints(&[2381, 2326, 97])), value(Expr::Int(3))),
        case(
            "two picTOgraphics",
            ints(&[128104, 128105]),
            value(ints(&[128104, 128105])),
            value(Expr::Int(2)),
        ),
        case("soft hyphen alone (169)", ints(&[169]), value(ints(&[169])), value(Expr::Int(1))),
        case("two combining marks (2)", ints(&[771, 775]), value(Expr::List(vec![ints_raw(&[771, 775])])), value(Expr::Int(1))),
        case("cr alone", ints(&[13]), value(ints(&[13])), value(Expr::Int(1))),
        case("two cr", ints(&[13, 13]), value(ints(&[13, 13])), value(Expr::Int(2))),
        case("lf then cr", ints(&[10, 13]), value(ints(&[10, 13])), value(Expr::Int(2))),
        case("prepend then ascii (2)", ints(&[1757, 97]), value(Expr::List(vec![ints_raw(&[1757, 97])])), value(Expr::Int(1))),
        case(
            "prepend then soft hyphen",
            ints(&[1757, 173, 97]),
            value(ints(&[1757, 173, 97])),
            value(Expr::Int(3)),
        ),
        case("prepend then crlf", ints(&[1536, 13, 10]), value(ints(&[1536, 13, 10])), value(Expr::Int(2))),
        case("ascii prepend ascii", ints(&[97, 1536, 97]), value(ints(&[97, 1536, 97])), value(Expr::Int(2))),
        case("regional alone", ints(&[127462, 97]), value(ints(&[127462, 97])), value(Expr::Int(2))),
        case(
            "three regionals",
            ints(&[127462, 127463, 127463]),
            value(Expr::List(vec![ints_raw(&[127462, 127463]), Expr::Int(127463)])),
            value(Expr::Int(2)),
        ),
        case("LV then L", ints(&[44032, 4352, 97]), value(ints(&[44032, 4352, 97])), value(Expr::Int(3))),
        case(
            "L then LV",
            ints(&[4352, 44032, 97]),
            value(Expr::List(vec![ints_raw(&[4352, 44032]), Expr::Int(97)])),
            value(Expr::Int(2)),
        ),
        case("T then LV", ints(&[4520, 44032, 97]), value(ints(&[4520, 44032, 97])), value(Expr::Int(3))),
        case("max cp", ints(&[1114111]), value(ints(&[1114111])), value(Expr::Int(1))),
        case("max cp then ascii", ints(&[1114111, 97]), value(ints(&[1114111, 97])), value(Expr::Int(2))),
        case(
            "variation selectors",
            ints(&[65024, 65024, 97]),
            value(Expr::List(vec![ints_raw(&[65024, 65024]), Expr::Int(97)])),
            value(Expr::Int(2)),
        ),
        case("control 8400", ints(&[8400, 97]), value(ints(&[8400, 97])), value(Expr::Int(2))),
        case("control 8288", ints(&[8288, 97]), value(ints(&[8288, 97])), value(Expr::Int(2))),
        case("169 then ascii", ints(&[169, 97]), value(ints(&[169, 97])), value(Expr::Int(2))),
        case("174 then ascii", ints(&[174, 97]), value(ints(&[174, 97])), value(Expr::Int(2))),
        case(
            "169 and 174 then ascii",
            ints(&[169, 174, 97]),
            value(ints(&[169, 174, 97])),
            value(Expr::Int(3)),
        ),
        case(
            "hangul run",
            ints(&[1100, 1161, 1161, 1162, 97]),
            value(Expr::List(vec![
                ints_raw(&[1100, 1161, 1161]),
                Expr::Int(1162),
                Expr::Int(97),
            ])),
            value(Expr::Int(3)),
        ),
    ]
}

/// A list built from an owned slice of small integers.
fn ints_raw(values: &[i64]) -> Expr {
    Expr::List(values.iter().copied().map(Expr::Int).collect())
}

#[test]
fn the_corpus_matches_the_oracle_bit_for_bit() {
    let cases = cases();
    assert!(!cases.is_empty(), "the grapheme corpus must not be empty");
    let mut heap = Heap::new(1 << 22).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    for case in &cases {
        let input = build(&mut ctx, &case.input);
        // Both entry points are reached through `mod.rs`'s `pub(crate)` re-export —
        // the module's frozen surface — rather than through `api` directly.
        let next = super::next_grapheme(&mut ctx, input);
        check(&mut ctx, &atoms, case, "next_grapheme", next, &case.next);
        let len = super::grapheme_count(&mut ctx, input);
        check(&mut ctx, &atoms, case, "string:length", len, &case.len);
    }
}

// ---------------------------------------------------------------------------
// The live-oracle cross-check
//
// Every answer is compared as the hex of its `erlang:term_to_binary/1` image — the
// same device `tests/io_lib_format_ts.rs` uses — so the two halves are compared
// structurally, never through a printer, and a raise is comparable without a stack
// trace (it is tagged `{raised, Reason}` on both sides).
//
// A missing or failing oracle is a **failure** here, never a skip: the defect class
// this guards against is a *silently wrong* result (the wrong cluster boundary, the
// wrong count), which is exactly what a skipped check cannot see.
// ---------------------------------------------------------------------------

/// The Erlang **source** of a corpus term — the input half the oracle runs.
///
/// The source is ASCII by construction: a binary is `<<230,151,165>>`, never a raw
/// non-ASCII byte (a raw byte would corrupt the `-eval` program), and a code point
/// above 255 is its decimal integer literal inside a list.
fn erl_source(expr: &Expr) -> String {
    fn join(items: &[Expr]) -> String {
        items
            .iter()
            .map(erl_source)
            .collect::<Vec<String>>()
            .join(",")
    }
    match expr {
        Expr::Int(value) => value.to_string(),
        // The same bignum the Rust half builds, without a 22-digit literal.
        Expr::BigPow2(k) => format!("(1 bsl {k})"),
        Expr::Float(value) => format!("{value:?}"),
        Expr::Bin(bytes) => format!("<<{}>>", join_bytes(bytes)),
        // `beam_codec::term::make_bitstring`'s payload is MSB-first, so the leading
        // `bits` bits of `bytes` *are* the bitstring's value: `<<value:bits>>`.
        Expr::Bits(bytes, bits) => {
            assert!(
                !bytes.is_empty() && bytes.len() <= 16,
                "the corpus renderer handles payloads up to 16 bytes"
            );
            assert!(
                *bits <= bytes.len() * 8,
                "the corpus bit count must fit its payload"
            );
            let mut value: u128 = 0;
            for &byte in bytes {
                value = (value << 8) | u128::from(byte);
            }
            format!("<<{}:{bits}>>", value >> (bytes.len() * 8 - bits))
        }
        Expr::Nil => "[]".to_string(),
        Expr::List(items) => format!("[{}]", join(items)),
        Expr::Cons(head, tail) => format!("[{}|{}]", erl_source(head), erl_source(tail)),
        Expr::Tuple(items) => format!("{{{}}}", join(items)),
        Expr::ErrorTuple(payload) => format!("{{error,{}}}", erl_source(payload)),
    }
}

/// `<<230,151,165>>` — one decimal byte per element, in order.
fn join_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| byte.to_string())
        .collect::<Vec<String>>()
        .join(",")
}

/// The hex of `term`'s `erlang:term_to_binary/1` image (lowercase, no separators).
fn term_to_binary_hex(ctx: &mut BifContext<'_>, term: Eterm) -> String {
    let reg = crate::default_registry();
    let image = reg
        .call("erlang", "term_to_binary", 1, ctx, &[term])
        .expect("erlang:term_to_binary/1 is a registered BIF");
    let (bytes, offset, size_bits) = bitstring_bytes(image).expect("the image is a binary");
    assert_eq!(offset, 0, "the image is a whole-byte binary");
    assert_eq!(size_bits % 8, 0, "the image is whole-byte aligned");
    let mut out = String::with_capacity(size_bits / 4);
    for &byte in &bytes[..size_bits / 8] {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// The oracle's **reason term** for a port failure.
///
/// [`BeamError::BadArg`] (the bare `badarg`) and a raise whose reason is an atom map
/// to that atom; a structured raise carries the real reason term in the
/// `Raise::live_reason` slot (see `../../error.rs`). Anything else is a bug in the
/// port, not a corpus answer.
fn reason_term(ctx: &mut BifContext<'_>, error: &BeamError) -> Eterm {
    if let BeamError::Raise(raise) = error {
        if let Some(name) = raise.reason.as_atom() {
            return ctx
                .atom(name)
                .unwrap_or_else(|e| panic!("the reason atom `{name}` must be seeded: {e:?}"));
        }
        return raise
            .live_reason()
            .and_then(|live| live.downcast_ref::<Eterm>().copied())
            .expect("a structured raise carries its reason term");
    }
    match error.as_atom() {
        Some(name) => ctx
            .atom(name)
            .unwrap_or_else(|e| panic!("the reason atom `{name}` must be seeded: {e:?}")),
        None => panic!("the corpus must not produce {error:?}"),
    }
}

/// The hex of a probe's answer, tagged the way the oracle's `-eval` tags it: a value
/// is the value itself, a raise is `{raised, Reason}`.
fn answer_hex(ctx: &mut BifContext<'_>, result: Result<Eterm>) -> String {
    let term = match result {
        Ok(term) => term,
        Err(error) => {
            let reason = reason_term(ctx, &error);
            let raised = ctx.atom("raised").expect("`raised` is seeded");
            ctx.tuple(&[raised, reason]).expect("a short tuple fits")
        }
    };
    term_to_binary_hex(ctx, term)
}

/// The `erl -eval` body for the corpus: one line per case, holding the `Hex` of the
/// `string:next_grapheme/1` answer and the `Hex` of the `string:length/1` answer,
/// space-separated.
///
/// A raising call is caught (`catch error:R`) and encoded as `{raised, R}`, so a
/// raise-vs-value divergence is visible in the comparison.
fn oracle_eval(sources: &[String]) -> String {
    let mut body = String::from(
        "Hex = fun(B) -> lists:flatten([io_lib:format(\"~2.16.0b\", [X]) \
         || X <- binary_to_list(B)]) end, ",
    );
    // Every probe binds its own variables: `-eval` evaluates the whole sequence
    // in one `erl_eval` environment, and re-binding `V`/`R` in a later `try`
    // aborts the evaluation with a `{badmatch, …}` *before any line is printed*
    // (the process dies with exit 1 and empty stdout — a silent zero-line run).
    for (index, source) in sources.iter().enumerate() {
        body.push_str(&format!(
            "io:format(\"~s ~s~n\", [Hex(erlang:term_to_binary((try V{index} = \
             string:next_grapheme({source}), V{index} catch error:R{index} -> {{raised, R{index}}} end))), \
             Hex(erlang:term_to_binary((try W{index} = string:length({source}), W{index} \
             catch error:E{index} -> {{raised, E{index}}} end)))]), ",
        ));
    }
    body.push_str("halt().");
    body
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
            "[beam_bifs grapheme] $ERTS_ORACLE='{}' is not an existing executable.",
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

/// Runs one `erl -noshell -eval <eval>` (which must `halt()`) and returns stdout.
///
/// A non-zero exit or a spawn failure is a panic: the harness never degrades to a
/// skip.
fn run_erl(oracle: &Path, eval: &str) -> String {
    let output = Command::new(oracle)
        .arg("-noshell")
        .arg("-eval")
        .arg(eval)
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

/// Re-runs the whole corpus under the **live** oracle and requires the
/// `erlang:term_to_binary/1` image of every answer to agree.
///
/// This test **fails loudly**: with no usable `$ERTS_ORACLE` (or no `erl` on
/// `$PATH`) it panics rather than skipping, and it asserts both a non-zero corpus size
/// and one parsed oracle line per case, so a mangled generated `-eval` (or a corpus
/// that silently shrank to nothing) cannot pass.
#[test]
fn the_live_oracle_agrees_on_the_whole_corpus() {
    let oracle = resolve_oracle().unwrap_or_else(|| {
        panic!(
            "the grapheme live-oracle cross-check requires a system `erl` \
             ($ERTS_ORACLE naming an executable, or one on $PATH). Refusing to pass \
             silently: install Erlang/OTP or set $ERTS_ORACLE."
        )
    });

    let cases = cases();
    assert!(!cases.is_empty(), "the grapheme corpus must not be empty");

    let mut heap = Heap::new(1 << 22).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // One source + one pair of answer images per case, all built from the *same*
    // `Expr` the committed goldens are built from.
    let mut sources = Vec::with_capacity(cases.len());
    let mut ours = Vec::with_capacity(cases.len());
    for case in &cases {
        let input = build(&mut ctx, &case.input);
        sources.push(erl_source(&case.input));
        let next = super::next_grapheme(&mut ctx, input);
        let next = answer_hex(&mut ctx, next);
        let len = super::grapheme_count(&mut ctx, input);
        let len = answer_hex(&mut ctx, len);
        ours.push([next, len]);
    }

    let stdout = run_erl(&oracle, &oracle_eval(&sources));
    let lines: Vec<&str> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();

    assert_eq!(
        lines.len(),
        cases.len(),
        "the oracle printed {} non-empty lines for {} cases — the generated \
         `-eval` did not run every case:\n{stdout}",
        lines.len(),
        cases.len(),
    );

    for ((case, ours), line) in cases.iter().zip(ours.iter()).zip(lines.iter()) {
        let answers: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(
            answers.len(),
            2,
            "case `{}`: the oracle line must hold the `string:next_grapheme/1` and \
             `string:length/1` images, got `{line}`",
            case.name,
        );
        assert_eq!(
            ours[0], answers[0],
            "case `{}`: `string:next_grapheme/1` disagrees with the live oracle \
             (erlang:term_to_binary image; `{{raised, _}}` tags a raise)",
            case.name,
        );
        assert_eq!(
            ours[1], answers[1],
            "case `{}`: `string:length/1` disagrees with the live oracle \
             (erlang:term_to_binary image; `{{raised, _}}` tags a raise)",
            case.name,
        );
    }
}
