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

//! The `-eval` evaluator's **sub-expression evaluation order**, pinned at the executable
//! boundary against the system `erl` oracle (**A.6**).
//!
//! # Why this is pinned here
//!
//! Expression order is observable only through a side effect, so every probe hands each
//! sub-expression to a **spy** closure
//!
//! ```erlang
//! F = fun(X) -> io:format("~w", [X]), X end
//! ```
//!
//! that prints its argument and returns it unchanged: the printed digits **are** the order the
//! sub-expressions ran in, and because the spy returns its argument the enclosing construct's
//! value is unaffected. Each probe is `<spy>, _ = <body>, ok.` — the trailing `ok` discards the
//! body's value, so `-eval`'s "print nothing on success" behaviour leaves the spy's output as the
//! whole of stdout.
//!
//! The evaluator must evaluate a construct's sub-expressions **left-to-right in source order**,
//! exactly as `erl_eval` does: a list's elements in source order *and then* its tail, a tuple's
//! elements, a map field's key before its value, a binary's segments, `++`'s left operand before
//! its right, and a comprehension's template once per generated element in generator order. This
//! suite pins that at the CLI, where a user sees it.
//!
//! # How the cases are compared
//!
//! Every case carries the **exact stdout** this runtime prints, and — when a system `erl` is
//! present — the live oracle is asked to run the very same expression and must print the same
//! bytes. The recorded values are therefore cross-checked, not merely this runtime's own habit. A
//! missing toolchain is a **loud** skip, never a silent pass ([`support::skip_loudly`]), and every
//! probe child is polled at [`support::PROBE_TIMEOUT`] and reaped, so a blocked probe can never
//! hang the suite.

mod support;

use std::path::Path;
use std::process::Output;

/// The executable under test, exactly as the other contract suites drive it.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The `erl`-facing banner for the loud skip.
const BANNER: &str = "eval_order";

/// The spy closure every probe is prefixed with: prints its argument, then returns it unchanged.
///
/// Because it returns its argument, the enclosing construct's **value** is untouched — the
/// printing is the only observable effect.
const SPY: &str = r#"F = fun(X) -> io:format("~w", [X]), X end"#;

/// One finished probe: its exit code and trimmed stdout/stderr.
struct Run {
    /// The child's exit code (`-1` when it was signalled).
    code: i32,
    /// Trimmed stdout.
    out: String,
    /// Trimmed stderr.
    err: String,
}

/// Runs `program args` under the shared bounded runner and fails loudly when it could not run.
fn run(program: &Path, args: &[&str]) -> Run {
    let output: Output = support::run_bounded(program, args).unwrap_or_else(|| {
        panic!("`{}` {args:?} could not be run to completion", program.display())
    });
    Run {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}

/// `beam_rt -noshell -eval <expr>` through the built binary.
fn beam_rt_eval(expr: &str) -> Run {
    run(Path::new(BINARY), &["-noshell", "-eval", expr])
}

/// `<spy>, _ = <body>, ok.` — the body's sub-expressions print in the order they run.
///
/// The space around `=` keeps `= <<` from lexing as the `=<` token for a binary body, exactly as
/// `src/eval/mod.rs`'s own order probes do.
fn probe(body: &str) -> String {
    format!("{SPY}, _ = {body}, ok.")
}

/// The pinned cases: `(label, expression, the exact stdout both runtimes print)`.
///
/// The first eight cover the constructs whose sub-expression order is observable; the third also
/// asserts the construct's **value** (`=123=[1,2|3]` prints `[1,2|3]` after the elements), so the
/// order is shown to come from ordering the side effects, not from a changed result.
fn cases() -> [(&'static str, String, &'static str); 8] {
    [
        (
            "a list's elements, in source order",
            probe("[F(1),F(2),F(3)]"),
            "123",
        ),
        (
            "a list's elements before its tail",
            probe("[F(1),F(2),F(3)|F(4)]"),
            "1234",
        ),
        (
            "a list's value is unchanged by the order",
            format!(r#"{SPY}, V = [F(1),F(2)|F(3)], io:format("=~w", [V]), ok."#),
            "123=[1,2|3]",
        ),
        (
            "a tuple's elements, in source order",
            probe("{F(1),F(2),F(3)}"),
            "123",
        ),
        (
            "a map's fields, each key before its value, in source order",
            probe("#{F(1)=>F(2), F(3)=>F(4)}"),
            "1234",
        ),
        (
            "a binary's segments, in source order",
            probe("<<(F(1)):8,(F(2)):8>>"),
            "12",
        ),
        (
            "`++`'s left operand before its right",
            probe("[F(1)] ++ [F(2)]"),
            "12",
        ),
        (
            "a comprehension's template, once per element in generator order",
            probe("[F(X) || X <- [1,2,3]]"),
            "123",
        ),
    ]
}

/// The built binary must evaluate every construct's sub-expressions left-to-right in source order,
/// printing exactly the pinned digit string.
///
/// These are the golden values the oracle is separately asked to confirm:
/// `literals_evaluate_their_sub_expressions_in_source_order` (`src/eval/mod.rs`) pins the same
/// rule at the evaluator level, and this suite pins it where a user observes it.
#[test]
fn the_eval_evaluator_prints_sub_expressions_in_source_order() {
    for (label, expr, expected) in cases() {
        let ours = beam_rt_eval(&expr);
        assert_eq!(
            ours.code, 0,
            "{label}: `beam_rt -eval {expr:?}` must exit 0, stderr — {}",
            ours.err
        );
        assert_eq!(ours.out, expected, "{label}: beam_rt's printing");
        assert!(
            ours.err.is_empty(),
            "{label}: a successful probe is silent on stderr — {}",
            ours.err
        );
    }
}

/// The system `erl` runs the **same** expressions and must print the **same** bytes, so the pinned
/// order is `erl_eval`'s own and not just this runtime's habit.
///
/// A missing `erl` is a loud skip; a probe the oracle cannot answer is reported, never silently
/// treated as agreement.
#[test]
fn the_system_oracle_prints_the_same_bytes() {
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    for (label, expr, expected) in cases() {
        let Some(theirs) = support::oracle_eval(&oracle, None, &expr) else {
            eprintln!("warning: the oracle could not answer {label} — not compared");
            continue;
        };
        assert_eq!(
            theirs, expected,
            "{label}: the system erl's printing must be the recorded value"
        );

        let ours = beam_rt_eval(&expr);
        assert_eq!(ours.code, 0, "{label}: beam_rt exited {}: {}", ours.code, ours.err);
        assert_eq!(
            ours.out, theirs,
            "{label}: the two runtimes must print the same bytes"
        );
    }
}
