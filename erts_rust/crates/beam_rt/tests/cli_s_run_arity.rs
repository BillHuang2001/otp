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
//! The `-s` / `-run` **Start instruction**'s arity (contract A.6).
//!
//! ERTS never applies one argument per token. `init`'s `parse_boot_args/1` collects the
//! tokens up to the next switch and hands them to `run_args_to_start_instructions/2`
//! (`erts/preloaded/src/init.erl:607-624`), which folds them into **one**
//! `{apply, M, F, A}`:
//!
//! ```text
//! run_args_to_mfa([M]) -> {b2a(M), start, []};
//! run_args_to_mfa([M, F]) -> {b2a(M), b2a(F), []};
//! run_args_to_mfa([M, F | A]) -> {b2a(M), b2a(F), [A]}.
//! ```
//!
//! So `-s Mod F A1 A2` is `Mod:F(['A1','A2'])` — **arity 1**, one argument, the list — and
//! `-s Mod F` is `Mod:F()` — **arity 0**. `-run` has the same shape with every token a
//! **string** (`bs2ss/1`) instead of an atom (`bs2as/1`), and a numeric token is a *name*:
//! `-s mod f 1 2` ⇒ `['1','2']`, `-run mod f 1 2` ⇒ `[[49],[50]]`.
//!
//! Every row below is measured against the system oracle (OTP 29.1.2 / erts-17.1.1) on the
//! same compiled fixture: this runtime's bytes must equal the oracle's, and the pinned
//! expectation makes the suite meaningful without the oracle too. The fixture prints the
//! shape it was called with, so an arity-*n* regression is a visible stdout difference
//! (the module exports the larger arities as well, so the regression prints a different
//! line instead of merely failing).
use std::path::{Path, PathBuf};

// The shared harness is compiled in whole; this suite uses only its fixture compiler, its
// bounded runner and its skip helpers.
#[allow(dead_code)]
mod support;

/// The fixture: every arity prints the shape it was called with.
///
/// `f/2` and `main/2` exist on purpose: if this runtime went back to applying one
/// argument per token, `-s sarity f x y` would print `f/2 x y` instead of `f/1 [x,y]`,
/// which is a visible disagreement with the oracle rather than an `undef`.
const SOURCE: &str = r#"
-module(sarity).
-export([f/0, f/1, f/2, main/0, main/1, main/2]).

f() -> io:format("f/0~n", []).
f(X) -> io:format("f/1 ~w~n", [X]).
f(X, Y) -> io:format("f/2 ~w ~w~n", [X, Y]).

main() -> io:format("main/0~n", []).
main(X) -> io:format("main/1 ~w~n", [X]).
main(X, Y) -> io:format("main/2 ~w ~w~n", [X, Y]).
"#;

const BANNER: &str = "cli_s_run_arity";

/// One Start-instruction row: the switches after `-noshell -pa DIR`, and the line both
/// runtimes must print.
struct Row {
    /// The `-s`/`-run` switches, verbatim.
    argv: &'static [&'static str],
    /// What the instruction's callee prints (`~w` of the shape it was called with).
    expected: &'static str,
}

/// The three `-s` shapes and the three `-run` shapes — no token (arity 0), one token and
/// two tokens (arity 1, one list).
const ROWS: [Row; 6] = [
    Row {
        argv: &["-s", "sarity", "f"],
        expected: "f/0\n",
    },
    Row {
        argv: &["-s", "sarity", "f", "x"],
        expected: "f/1 [x]\n",
    },
    Row {
        argv: &["-s", "sarity", "f", "x", "y"],
        expected: "f/1 [x,y]\n",
    },
    Row {
        argv: &["-run", "sarity", "main"],
        expected: "main/0\n",
    },
    Row {
        argv: &["-run", "sarity", "main", "a"],
        expected: "main/1 [[97]]\n",
    },
    Row {
        argv: &["-run", "sarity", "main", "a", "b"],
        expected: "main/1 [[97],[98]]\n",
    },
];

/// A numeric token is a **name**, never a number, on both flags.
const NUMERIC_ROWS: [Row; 2] = [
    Row {
        argv: &["-s", "sarity", "f", "1", "2"],
        expected: "f/1 ['1','2']\n",
    },
    Row {
        argv: &["-run", "sarity", "main", "1", "2"],
        expected: "main/1 [[49],[50]]\n",
    },
];

/// A trailing switch ends the collected tokens: `-noshell` is a switch, not an argument.
const TERMINATED_ROW: Row = Row {
    argv: &["-s", "sarity", "f", "x", "-noshell"],
    expected: "f/1 [x]\n",
};

/// The compiled fixture plus the oracle, or `None` after a loud skip.
///
/// The [`support::ErlangFixture`] is **owned** here: its scratch directory is removed when
/// it is dropped, so a harness that kept only the path would run against a deleted module.
struct Harness {
    fixture: support::ErlangFixture,
    oracle: PathBuf,
}

impl Harness {
    /// Compiles [`SOURCE`]; skips loudly (returning `None`) without `erl`/`erlc`.
    fn new() -> Option<Harness> {
        let fixture = support::ErlangFixture::compile(BANNER, "sarity", "sarity", SOURCE)?;
        let oracle = support::resolve_oracle()?;
        Some(Harness { fixture, oracle })
    }

    fn dir_str(&self) -> &str {
        self.fixture
            .dir()
            .to_str()
            .expect("the fixture dir is UTF-8")
    }

    /// Runs the row on this runtime: `-noshell -pa DIR <argv>` — the action itself ends
    /// the process (no `-s init stop`; the `-s`/`-run` action is this runtime's one
    /// action slot).
    fn run_ours(&self, argv: &[&str]) -> support::Observed {
        let mut args = vec!["-noshell", "-pa", self.dir_str()];
        args.extend_from_slice(argv);
        support::run_runtime(&args).unwrap_or_else(|| {
            panic!("beam_rt {argv:?} could not be run to completion")
        })
    }

    /// Runs the same row on the oracle, with the `-s init stop` terminator the system
    /// `erl` needs (this runtime's action already ends its process).
    fn run_oracle(&self, argv: &[&str]) -> Option<support::Observed> {
        let mut args = vec!["-noshell", "-pa", self.dir_str()];
        args.extend_from_slice(argv);
        args.extend_from_slice(&["-s", "init", "stop"]);
        let output = support::run_bounded(Path::new(&self.oracle), &args)?;
        Some(support::Observed {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    /// Asserts one row on both runtimes: the row's pinned bytes, then the oracle's own
    /// bytes. A missing oracle answer is a loud skip, never a silent pass.
    fn check(&self, row: &Row) {
        let ours = self.run_ours(row.argv);
        assert_eq!(
            ours.code, 0,
            "{:?}: exit code — stderr {:?}",
            row.argv, ours.stderr
        );
        assert_eq!(
            ours.stdout, row.expected,
            "{:?}: the Start instruction's argument shape — stderr {:?}",
            row.argv, ours.stderr
        );
        assert!(
            ours.stderr.is_empty(),
            "{:?}: a successful action is silent — {:?}",
            row.argv,
            ours.stderr
        );
        let Some(theirs) = self.run_oracle(row.argv) else {
            support::skip_loudly(BANNER, &format!("the oracle did not answer {:?}", row.argv));
            return;
        };
        assert_eq!(
            theirs.code, 0,
            "{:?}: the oracle's exit code — stderr {:?}",
            row.argv, theirs.stderr
        );
        assert_eq!(
            ours.stdout, theirs.stdout,
            "{:?}: this runtime must agree with the oracle",
            row.argv
        );
    }
}

/// The six shapes: `-s`/`-run` × (no token, one token, two tokens).
#[test]
fn a_start_instruction_applies_one_list_argument_or_none() {
    let Some(harness) = Harness::new() else {
        return;
    };
    for row in &ROWS {
        harness.check(row);
    }
}

/// A numeric token is `'1'` for `-s` and `"1"` (`[49]`) for `-run` — never the integer `1`.
#[test]
fn a_numeric_token_is_a_name_not_a_number() {
    let Some(harness) = Harness::new() else {
        return;
    };
    for row in &NUMERIC_ROWS {
        harness.check(row);
    }
}

/// `-s Mod F x -noshell`: the trailing switch ends the argument list, so the call is
/// `F(['x'])` and `-noshell` stays a switch.
#[test]
fn a_following_switch_ends_the_argument_list() {
    let Some(harness) = Harness::new() else {
        return;
    };
    harness.check(&TERMINATED_ROW);
}
