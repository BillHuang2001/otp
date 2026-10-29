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
//! `lists:seq/2,3` with **bignum** bounds and steps.
//!
//! ## Why
//!
//! `src/lists.rs` used to classify every operand with `small_int_clause`, so a bound
//! beyond the 60-bit small range turned `seq/2` into `error:function_clause` and
//! `seq/3` into `error:badarg` — while the oracle (`lib/stdlib/src/lists.erl:476-489`
//! and `:532-553`) guards with `is_integer/1` only and computes with exact term
//! arithmetic. The small-integer fast path is unchanged and measured hot; the new
//! wide path (`seq2_wide`/`seq3_wide`/`seq_walk`) takes over as soon as one operand is
//! an integer that is not a small int.
//!
//! ## What is pinned here
//!
//! 1. `pins_the_oracle_transcript` — every `(label, expression, verdict)` below
//!    against the transcript this file was written from, so a regression fails without
//!    an `erl` on the host (`CI`'s `cargo test` job has none).
//! 2. `the_live_oracle_agrees_when_erl_is_on_path` — the same table evaluated by the
//!    system `erl` and compared case by case. With no oracle on `PATH` it prints a
//!    loud `SKIP` and returns (a hard failure there would be wrong).
//!
//! A verdict is either `ok <~w rendering>` or `<class>:<reason>` — the shape the
//! oracle probe prints — so the two sides compare as text:
//!
//! ```text
//! $ erl -noshell -eval '...probe...'
//! seq(B,B+2) => ok [748274827583793785928592859,748274827583793785928592860,748274827583793785928592861]
//! seq(B,B-1) => ok []
//! seq(1.0,2) => error:function_clause
//! seq(1.0,2,1) => error:badarg
//! ```
//!
//! Oracle: Erlang/OTP 29.1.2 / erts-17.1.1 (the drift a erts-patch mismatch could
//! cause does not apply — `lists:seq/2,3` reads no version).
//!
//! Public API only: the cases run through [`BifRegistry::call`], and the result is
//! rendered by this file's own `~w`-shaped renderer ([`render`]) — a walk over the
//! cons cells printing each integer in decimal. It is deliberately **not** the
//! crate's `%T` printer (`beam_bifs::print::c_printf::term_text`), which follows the
//! C `erl_printf_term.c` convention of writing a *printable* list as raw bytes
//! (`[10]` ⇒ `"\n"`), where `~w` prints `[10]` — the oracle probe below prints `~w`.
use std::process::Command;
use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_common::{BeamError, ExitReason, Result};
use beam_process::ProcessId;
use beam_term::{make_small, signed_value, AtomTable, Eterm, NIL};
/// The bignum this file's transcript uses — `748274827583793785928592859`, ~90 bits,
/// three 64-bit limbs (so it is comfortably outside the 60-bit small range).
const B: &str = "748274827583793785928592859";
// ---------------------------------------------------------------------------
// The fixture: a registry + heap + atom table driven through the public seam
// ---------------------------------------------------------------------------
/// A registry, a heap and an atom table the cases call through.
struct Fixture {
    registry: BifRegistry,
    heap: Heap,
    atoms: AtomTable,
}
impl Fixture {
    fn new() -> Fixture {
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok"]);
        Fixture {
            registry: default_registry(),
            heap: Heap::new(1 << 16).expect("test heap"),
            atoms,
        }
    }
    /// `lists:seq/2` on the given operands.
    fn seq2(&mut self, first: Eterm, last: Eterm) -> Result<Eterm> {
        let Fixture {
            registry,
            heap,
            atoms,
        } = self;
        let mut ctx = BifContext::new(heap, atoms, ProcessId::new(1, 0));
        registry.call("lists", "seq", 2, &mut ctx, &[first, last])
    }
    /// `lists:seq/3` on the given operands.
    fn seq3(&mut self, first: Eterm, last: Eterm, incr: Eterm) -> Result<Eterm> {
        let Fixture {
            registry,
            heap,
            atoms,
        } = self;
        let mut ctx = BifContext::new(heap, atoms, ProcessId::new(1, 0));
        registry.call("lists", "seq", 3, &mut ctx, &[first, last, incr])
    }
    /// The term for an operand's [`Source`].
    fn operand(&mut self, source: Source) -> Eterm {
        match source {
            Source::Small(value) => make_small(value),
            Source::Big(decimal) => self.bignum(decimal),
            Source::Float(value) => {
                beam_codec::term::make_float(&mut self.heap, value).expect("the float fits")
            }
        }
    }
    /// The bignum term for a **decimal** string (`"-123"` allowed), built limb by
    /// limb here so no expected value depends on the code under test.
    fn bignum(&mut self, decimal: &str) -> Eterm {
        let (negative, digits) = match decimal.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, decimal),
        };
        let mut mag: Vec<u64> = Vec::new();
        for byte in digits.bytes() {
            assert!(byte.is_ascii_digit(), "not a decimal literal: {decimal}");
            let mut carry = u64::from(byte - b'0');
            for limb in mag.iter_mut() {
                let wide = u128::from(*limb) * 10 + u128::from(carry);
                *limb = wide as u64;
                carry = (wide >> 64) as u64;
            }
            while carry != 0 {
                mag.push(carry);
                carry = 0;
            }
        }
        beam_codec::term::make_big(&mut self.heap, negative, &mag).expect("the bignum fits")
    }
    /// The verdict text of one case — `ok <rendering>` or `<class>:<reason>`.
    fn verdict(&mut self, case: &Case) -> String {
        let (first, last, incr) = (case.first, case.last, case.incr);
        let first = self.operand(first);
        let last = self.operand(last);
        let result = match incr {
            None => self.seq2(first, last),
            Some(incr) => {
                let incr = self.operand(incr);
                self.seq3(first, last, incr)
            }
        };
        match result {
            Ok(term) => format!("ok {}", render(term)),
            Err(err) => format!("{}", Verdict::of(&err)),
        }
    }
}
/// The `~w` rendering of an integer list: `[e1,e2,…]`, each element in decimal, and
/// `[]` for `NIL`.
///
/// A non-list, an improper tail or a non-integer element is reported verbatim (it
/// then disagrees with every golden, so a regression fails instead of passing
/// silently) — this test never needs a general term printer.
fn render(term: Eterm) -> String {
    if term == NIL {
        return "[]".to_string();
    }
    let mut out = String::from("[");
    let mut cursor = term;
    let mut first = true;
    while cursor != NIL {
        if !beam_term::is_list(cursor) {
            return format!("<improper tail {cursor:?} of {out}>");
        }
        if !first {
            out.push(',');
        }
        first = false;
        let head = beam_codec::term::cons_head(cursor).expect("a cons head");
        match decimal(head) {
            Some(text) => out.push_str(&text),
            None => return format!("<non-integer element {head:?}>"),
        }
        cursor = beam_codec::term::cons_tail(cursor).expect("a cons tail");
    }
    out.push(']');
    out
}
/// The decimal text of an **integer** term (a small int or a bignum), else `None`.
fn decimal(term: Eterm) -> Option<String> {
    if beam_term::is_small(term) {
        return Some(format!("{}", signed_value(term)));
    }
    let (negative, digits) = beam_codec::term::big_parts(term).ok()?;
    let mut mag: Vec<u64> = digits.to_vec();
    // Long division by `10^19` collects the decimal digits nineteen at a time, most
    // significant chunk last.
    const CHUNK: u128 = 10_000_000_000_000_000_000;
    let mut chunks: Vec<u128> = Vec::new();
    while !mag.is_empty() {
        let mut remainder: u128 = 0;
        for limb in mag.iter_mut().rev() {
            let current = (remainder << 64) | u128::from(*limb);
            *limb = (current / CHUNK) as u64;
            remainder = current % CHUNK;
        }
        chunks.push(remainder);
        while mag.last() == Some(&0) {
            mag.pop();
        }
    }
    let mut text = String::new();
    if negative {
        text.push('-');
    }
    let Some((first, rest)) = chunks.split_last() else {
        return Some("0".to_string());
    };
    text.push_str(&format!("{first}"));
    for chunk in rest.iter().rev() {
        text.push_str(&format!("{chunk:019}"));
    }
    Some(text)
}
/// One operand of a case, in the form both sides can build.
#[derive(Clone, Copy)]
enum Source {
    /// A 60-bit small integer.
    Small(i64),
    /// A bignum, as a decimal string.
    Big(&'static str),
    /// An `f64`.
    Float(f64),
}
impl Source {
    /// The operand as **Erlang source** — the oracle probe's spelling.
    fn erl_text(self) -> String {
        match self {
            Source::Small(value) => format!("{value}"),
            Source::Big(decimal) => format!("({decimal})"),
            Source::Float(value) => format!("{value:?}"),
        }
    }
}
/// One `lists:seq` call, its label, its operands and the oracle's verdict text.
struct Case {
    /// The label the transcript is keyed by.
    label: &'static str,
    first: Source,
    last: Source,
    /// `None` for `seq/2`.
    incr: Option<Source>,
    /// The oracle's verdict — `ok <~w>` or `<class>:<reason>`.
    expected: &'static str,
}
impl Case {
    /// The call as **Erlang source**, e.g. `lists:seq(B, B+2)`.
    fn erl_text(&self) -> String {
        match self.incr {
            None => format!(
                "lists:seq({}, {})",
                self.first.erl_text(),
                self.last.erl_text()
            ),
            Some(incr) => format!(
                "lists:seq({}, {}, {})",
                self.first.erl_text(),
                self.last.erl_text(),
                incr.erl_text()
            ),
        }
    }
}
/// The `error:<reason>` text of a [`BeamError`], in the oracle probe's spelling.
struct Verdict;
impl Verdict {
    /// `<class>:<reason>` for a raise, `badarg`-style variants included. Anything
    /// that is not the class this file knows about is reported verbatim, so a wrong
    /// error class fails the comparison instead of being papered over.
    fn of(err: &BeamError) -> String {
        match err {
            BeamError::BadArg => "error:badarg".to_string(),
            BeamError::Raise(raise) if raise.reason == ExitReason::atom("function_clause") => {
                "error:function_clause".to_string()
            }
            other => format!("unexpected {other:?}"),
        }
    }
}
// ---------------------------------------------------------------------------
// The transcript (host `erl`, OTP 29.1.2 / erts-17.1.1)
// ---------------------------------------------------------------------------
/// `B = 748274827583793785928592859` in Erlang source terms, spelled out so the
/// expected texts below are literally the oracle's own `~w` renderings.
fn cases() -> Vec<Case> {
    // `B` and the neighbours the cases need, as decimal strings.
    const B_PLUS_2: &str = "748274827583793785928592861";
    const B_MINUS_1: &str = "748274827583793785928592858";
    const B_MINUS_2: &str = "748274827583793785928592857";
    const B_MINUS_3: &str = "748274827583793785928592856";
    const B_TIMES_2: &str = "1496549655167587571857185718";
    const B_TIMES_3: &str = "2244824482751381357785778577";
    // `B-3, B-1` and `B, B-2` are the two non-contiguous bignum sequences.
    const NEG_B: &str = "-748274827583793785928592859";
    const NEG_B_PLUS_2: &str = "-748274827583793785928592857";
    let b = Source::Big(B);
    vec![
        // --- the four reported failures: bignum bounds/steps ---------------
        Case {
            label: "seq(B,B+2)",
            first: b,
            last: Source::Big(B_PLUS_2),
            incr: None,
            expected: "ok [748274827583793785928592859,748274827583793785928592860,748274827583793785928592861]",
        },
        Case {
            label: "seq(B,B+2,1)",
            first: b,
            last: Source::Big(B_PLUS_2),
            incr: Some(Source::Small(1)),
            expected: "ok [748274827583793785928592859,748274827583793785928592860,748274827583793785928592861]",
        },
        Case {
            label: "seq(B+2,B,-1)",
            first: Source::Big(B_PLUS_2),
            last: b,
            incr: Some(Source::Small(-1)),
            expected: "ok [748274827583793785928592861,748274827583793785928592860,748274827583793785928592859]",
        },
        Case {
            label: "seq(B,B-1)",
            first: b,
            last: Source::Big(B_MINUS_1),
            incr: None,
            expected: "ok []",
        },
        // --- the same shapes one step further ------------------------------
        Case {
            label: "seq(B,B)",
            first: b,
            last: b,
            incr: None,
            expected: "ok [748274827583793785928592859]",
        },
        Case {
            label: "seq(B,B-1,1)",
            first: b,
            last: Source::Big(B_MINUS_1),
            incr: Some(Source::Small(1)),
            expected: "ok []",
        },
        Case {
            label: "seq(B,B,0)",
            first: b,
            last: b,
            incr: Some(Source::Small(0)),
            expected: "ok [748274827583793785928592859]",
        },
        Case {
            label: "seq(B,B+2,0)",
            first: b,
            last: Source::Big(B_PLUS_2),
            incr: Some(Source::Small(0)),
            expected: "error:badarg",
        },
        Case {
            label: "seq(B,B,1)",
            first: b,
            last: b,
            incr: Some(Source::Small(1)),
            expected: "ok [748274827583793785928592859]",
        },
        Case {
            label: "seq(B-3,B,2)",
            first: Source::Big(B_MINUS_3),
            last: b,
            incr: Some(Source::Small(2)),
            expected: "ok [748274827583793785928592856,748274827583793785928592858]",
        },
        Case {
            label: "seq(B,B-3,-2)",
            first: b,
            last: Source::Big(B_MINUS_3),
            incr: Some(Source::Small(-2)),
            expected: "ok [748274827583793785928592859,748274827583793785928592857]",
        },
        // --- a bignum **step** ---------------------------------------------
        Case {
            label: "seq(B,B*3,B)",
            first: b,
            last: Source::Big(B_TIMES_3),
            incr: Some(b),
            expected: "ok [748274827583793785928592859,1496549655167587571857185718,2244824482751381357785778577]",
        },
        Case {
            label: "seq(B,B*2,B)",
            first: b,
            last: Source::Big(B_TIMES_2),
            incr: Some(b),
            expected: "ok [748274827583793785928592859,1496549655167587571857185718]",
        },
        Case {
            label: "seq(1,10,B)",
            first: Source::Small(1),
            last: Source::Small(10),
            incr: Some(b),
            expected: "ok [1]",
        },
        Case {
            label: "seq(1,10,-B)",
            first: Source::Small(1),
            last: Source::Small(10),
            incr: Some(Source::Big(NEG_B)),
            expected: "ok []",
        },
        Case {
            label: "seq(10,1,-B)",
            first: Source::Small(10),
            last: Source::Small(1),
            incr: Some(Source::Big(NEG_B)),
            expected: "ok [10]",
        },
        // --- negative bounds, and the small/bignum boundary ----------------
        Case {
            label: "seq(-B,-B+2)",
            first: Source::Big(NEG_B),
            last: Source::Big(NEG_B_PLUS_2),
            incr: None,
            expected: "ok [-748274827583793785928592859,-748274827583793785928592858,-748274827583793785928592857]",
        },
        Case {
            label: "seq(-B+2,-B,-1)",
            first: Source::Big(NEG_B_PLUS_2),
            last: Source::Big(NEG_B),
            incr: Some(Source::Small(-1)),
            expected: "ok [-748274827583793785928592857,-748274827583793785928592858,-748274827583793785928592859]",
        },
        Case {
            label: "seq(MAX_SMALL,MAX_SMALL+1)",
            first: Source::Small(576_460_752_303_423_487),
            last: Source::Big("576460752303423488"),
            incr: None,
            expected: "ok [576460752303423487,576460752303423488]",
        },
        // --- the error classes that must not move -------------------------
        Case {
            label: "seq(B,2)",
            first: b,
            last: Source::Small(2),
            incr: None,
            expected: "error:function_clause",
        },
        Case {
            label: "seq(B,B-2)",
            first: b,
            last: Source::Big(B_MINUS_2),
            incr: None,
            expected: "error:function_clause",
        },
        Case {
            label: "seq(1.0,2)",
            first: Source::Float(1.0),
            last: Source::Small(2),
            incr: None,
            expected: "error:function_clause",
        },
        Case {
            label: "seq(1.0,2,1)",
            first: Source::Float(1.0),
            last: Source::Small(2),
            incr: Some(Source::Small(1)),
            expected: "error:badarg",
        },
        Case {
            label: "seq(1.0,2.0,1.0)",
            first: Source::Float(1.0),
            last: Source::Float(2.0),
            incr: Some(Source::Float(1.0)),
            expected: "error:badarg",
        },
        Case {
            label: "seq(B,B+2,1.0)",
            first: b,
            last: Source::Big(B_PLUS_2),
            incr: Some(Source::Float(1.0)),
            expected: "error:badarg",
        },
        Case {
            label: "seq(1.0,1.0,0)",
            first: Source::Float(1.0),
            last: Source::Float(1.0),
            incr: Some(Source::Small(0)),
            expected: "error:badarg",
        },
        // --- the untouched small-integer fast path -------------------------
        Case {
            label: "seq(1,5)",
            first: Source::Small(1),
            last: Source::Small(5),
            incr: None,
            expected: "ok [1,2,3,4,5]",
        },
        Case {
            label: "seq(5,1)",
            first: Source::Small(5),
            last: Source::Small(1),
            incr: None,
            expected: "error:function_clause",
        },
        Case {
            label: "seq(1,0)",
            first: Source::Small(1),
            last: Source::Small(0),
            incr: None,
            expected: "ok []",
        },
        Case {
            label: "seq(1,6,2)",
            first: Source::Small(1),
            last: Source::Small(6),
            incr: Some(Source::Small(2)),
            expected: "ok [1,3,5]",
        },
        Case {
            label: "seq(6,2,-2)",
            first: Source::Small(6),
            last: Source::Small(2),
            incr: Some(Source::Small(-2)),
            expected: "ok [6,4,2]",
        },
        Case {
            label: "seq(5,5,0)",
            first: Source::Small(5),
            last: Source::Small(5),
            incr: Some(Source::Small(0)),
            expected: "ok [5]",
        },
        Case {
            label: "seq(5,4,0)",
            first: Source::Small(5),
            last: Source::Small(4),
            incr: Some(Source::Small(0)),
            expected: "error:badarg",
        },
        Case {
            label: "seq(5,5,2)",
            first: Source::Small(5),
            last: Source::Small(5),
            incr: Some(Source::Small(2)),
            expected: "ok [5]",
        },
        Case {
            label: "seq(0,0)",
            first: Source::Small(0),
            last: Source::Small(0),
            incr: None,
            expected: "ok [0]",
        },
        Case {
            label: "seq(1,-3,-1)",
            first: Source::Small(1),
            last: Source::Small(-3),
            incr: Some(Source::Small(-1)),
            expected: "ok [1,0,-1,-2,-3]",
        },
    ]
}
// ---------------------------------------------------------------------------
// (1) the pinned transcript
// ---------------------------------------------------------------------------
/// Every case reproduces the oracle's verdict text — including the `[]` results and
/// the `function_clause`/`badarg` split of the rejected shapes.
#[test]
fn pins_the_oracle_transcript() {
    let mut fx = Fixture::new();
    let mut failures = Vec::new();
    for case in cases() {
        let got = fx.verdict(&case);
        if got != case.expected {
            failures.push(format!(
                "{}: expected {:?}, got {:?}",
                case.label, case.expected, got
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of the pinned seq verdicts disagree:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
/// A bignum result is a **proper** list of exact integers — the walk's own element
/// order and values, read back through the cons cells rather than through a render.
#[test]
fn a_bignum_sequence_is_a_proper_list() {
    let mut fx = Fixture::new();
    let first = fx.bignum(B);
    let last = fx.bignum("748274827583793785928592861");
    let seq = fx.seq2(first, last).expect("lists:seq(B, B+2)");
    let mut elements = Vec::new();
    let mut cursor = seq;
    while cursor != NIL {
        assert!(
            beam_term::is_list(cursor),
            "an improper tail at {cursor:?} — the wide walk must build a proper list"
        );
        elements.push(beam_codec::term::cons_head(cursor).expect("a cons head"));
        cursor = beam_codec::term::cons_tail(cursor).expect("a cons tail");
    }
    let rendered: Vec<String> = elements
        .iter()
        .map(|&element| decimal(element).expect("an integer element"))
        .collect();
    assert_eq!(
        rendered,
        vec![
            "748274827583793785928592859".to_string(),
            "748274827583793785928592860".to_string(),
            "748274827583793785928592861".to_string(),
        ],
        "the three consecutive integers from B"
    );
    assert_eq!(
        render(seq),
        "[748274827583793785928592859,748274827583793785928592860,748274827583793785928592861]"
    );
}
// ---------------------------------------------------------------------------
// (2) the live-oracle cross-check
// ---------------------------------------------------------------------------
/// The same table, evaluated by the system `erl`, must produce the identical verdict
/// texts. Skips loudly (never fails) when no oracle is on `PATH`.
#[test]
fn the_live_oracle_agrees_when_erl_is_on_path() {
    let mut fx = Fixture::new();
    let cases = cases();
    let local: Vec<String> = cases.iter().map(|case| fx.verdict(case)).collect();
    let Some(oracle) = oracle_verdicts(&cases) else {
        eprintln!(
            "SKIP: the_live_oracle_agrees_when_erl_is_on_path — no `erl` oracle on PATH \
             (CI's `cargo test` job has none); the {} case(s) are still pinned by \
             `pins_the_oracle_transcript`.",
            cases.len()
        );
        return;
    };
    let mut failures = Vec::new();
    for (case, (got, want)) in cases.iter().zip(local.iter().zip(oracle.iter())) {
        if got != want {
            failures.push(format!(
                "{}: rust {got:?} vs oracle {want:?} (expr `{}`)",
                case.label,
                case.erl_text()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} case(s) disagree with the live oracle:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}
/// Evaluates every case under the system `erl` and returns its verdict texts in the
/// same order; `None` when no oracle is available (the caller then skips loudly).
fn oracle_verdicts(cases: &[Case]) -> Option<Vec<String>> {
    let labels = cases
        .iter()
        .map(|case| {
            format!(
                r#"{{"{}", fun() -> {} end}}"#,
                case.label,
                case.erl_text()
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let script = format!(
        "Cases = [{labels}], \
         Out = [begin \
                    V = try F() of R -> io_lib:format(\"ok ~w\", [R]) \
                        catch C:E -> io_lib:format(\"~w:~w\", [C, E]) end, \
                    io:format(\"~s => ~s~n\", [L, V]) \
                end || {{L, F}} <- Cases], \
         ok = length(Out), \
         ok."
    );
    let output = Command::new("erl")
        .args(["-noshell", "-eval", &script, "-s", "init", "stop"])
        .output();
    let out = match output {
        Ok(out) if out.status.success() => out,
        Ok(out) => {
            eprintln!(
                "SKIP: the oracle `erl` failed ({}); stderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            return None;
        }
        Err(err) => {
            eprintln!("SKIP: no `erl` oracle on PATH ({err})");
            return None;
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut verdicts = Vec::new();
    for line in stdout.lines() {
        let Some((label, verdict)) = line.split_once(" => ") else {
            continue;
        };
        // The probe prints `label => verdict`; keep the order the cases were built in.
        let _ = label;
        verdicts.push(verdict.to_string());
    }
    assert_eq!(
        verdicts.len(),
        cases.len(),
        "the oracle probe printed {} verdict(s) for {} case(s):\n{stdout}",
        verdicts.len(),
        cases.len()
    );
    Some(verdicts)
}
