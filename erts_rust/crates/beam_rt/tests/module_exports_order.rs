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

//! `Module:module_info(exports)` **order** (contract A.8) — the rule, pinned against the
//! live oracle.
//!
//! `exports` is the one `module_info/1` item whose *order* is observable: `eunit` walks
//! `Module:module_info(exports)` to find a module's `*_test_/0` generators
//! (`eunit_autoexport.erl`), so a reversed list reverses the per-test transcript order.
//!
//! # The rule (measured, and the C ground truth)
//!
//! - `exported_from_module` (`../../erts/emulator/beam/erl_bif_info.c:5838-5862`) walks the
//!   **global** export list — an insertion-ordered `IndexTable`
//!   (`../../erts/emulator/beam/index.c:79-107`) — from index `0` and **cons**es each entry
//!   of the requested module onto the result, i.e. it answers **reverse insertion order**.
//! - The loader inserts a module's entries in `ExpT`-chunk order
//!   (`../../erts/emulator/beam/emu/emu_load.c:693-726`; the JIT's
//!   `../../erts/emulator/beam/jit/asm_load.c:1186` likewise, after the `ImpT` loop).
//!
//! ⇒ **`Module:module_info(exports)` is the module's `ExpT` chunk order, reversed.** (For
//! `erlc` output, `ExpT` is written in reverse *code* order, so the answer is also the
//! module's function order restricted to its exports — which is why it agrees with
//! `module_info(functions)`, a code-order walk of the code header.)
//!
//! Measured over eight `erlc`-compiled probes (several `-export` attributes, several arities
//! per name, source order ≠ sorted order) against the OTP 29 oracle; the answer is
//! **stable** when 5 000 junk atoms are interned first, which rules out any atom-index/hash
//! dependence. It is neither the sorted order nor the raw `ExpT` order — both are asserted
//! below, so a "sort it" or "leave the loader order" regression cannot pass.
//!
//! # What this suite pins
//!
//! 1. [`the_export_item_is_the_oracles_order_element_for_element`] — a compiled fixture whose
//!    source order differs from its sorted order: every `module_info/1` item must print
//!    **identically** under `beam_rt` and under the oracle (string equality, so order counts),
//!    plus a recorded pin so the assertion also holds without a toolchain.
//! 2. [`the_eunit_verbose_generator_order_is_the_oracles`] — the behavioural consequence: a
//!    three-generator `_tests` module run through `eunit:test(…, [verbose])` reports its
//!    generators in the oracle's order, not reversed.
//!
//! A missing toolchain (`erl`/`erlc`) is a **loud skip**, never a silent pass.

mod support;

use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicUsize, Ordering};

use support::{find_on_path, is_executable, require_oracle, skip_loudly};

/// The suite's name in skip/warning banners.
const SUITE: &str = "module_exports_order";

/// The compiled fixture: exported names/arities in a source order that differs **both** from
/// the sorted order and from the `ExpT` order, spread over three `-export` attributes.
const PLAIN_SOURCE: &str = "\
-module(mi_order_plain).
-export([zebra/2, alpha/1]).
-export([zulu_extra/0, alpha/0, zebra/1]).
-export([mike/10, mike/2]).

zebra(_, _) -> 2.
zebra(_) -> 1.
alpha(_) -> 1.
alpha() -> 0.
zulu_extra() -> 0.
mike(_, _) -> 2.
mike(_,_,_,_,_,_,_,_,_,_) -> 10.
";

/// The oracle's `mi_order_plain:module_info(exports)`, recorded from the OTP 29 oracle. It is
/// the fixture's **source** order (= its code order = `reverse(ExpT)`), not its sorted order
/// (`alpha/0, alpha/1, mike/2, mike/10, module_info/0, module_info/1, zebra/1, zebra/2,
/// zulu_extra/0`) and not its `ExpT` order (which starts at `module_info/1`).
const PLAIN_EXPORTS: [&str; 9] = [
    "zebra/2",
    "zebra/1",
    "alpha/1",
    "alpha/0",
    "zulu_extra/0",
    "mike/2",
    "mike/10",
    "module_info/0",
    "module_info/1",
];

/// The eunit fixture: three generators, declared in a source order that is **not** sorted.
const EUNIT_SOURCE: &str = "\
-module(mi_order_tests).
-include_lib(\"eunit/include/eunit.hrl\").

zeta_group_test_() -> [?_assertEqual(Z, Z) || Z <- [1, 2]].
alpha_group_test_() -> [?_assert(true)].
mike_group_test_() -> [?_assert(is_list([]))].
";

/// The generators in the oracle's transcript order — the declaration (code/`reverse(ExpT)`)
/// order, not the sorted one (`alpha_group_test_` first).
const EUNIT_GENERATORS: [&str; 3] =
    ["zeta_group_test_", "alpha_group_test_", "mike_group_test_"];

/// The built runtime under test.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

// ---------------------------------------------------------------------------------------
// The compiled fixtures
// ---------------------------------------------------------------------------------------

/// A fresh scratch directory under `$TMPDIR` holding **only** the compiled fixtures, so the
/// directory is also a safe single `-pa` for the oracle (nothing shadows stdlib).
struct Fixture {
    /// The scratch directory.
    dir: PathBuf,
}

impl Fixture {
    /// The directory to hand to `-pa`.
    fn path(&self) -> &Path {
        &self.dir
    }

    /// Removes the scratch directory (`erlc` output is not source).
    fn cleanup(self) {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            // The directory is ours and disposable; a failure is reported, not hidden.
            Err(error) => println!("note: could not remove {}: {error}", self.dir.display()),
        }
    }
}

/// A fresh scratch directory for `label`.
fn scratch(label: &str) -> Option<PathBuf> {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_module_exports_order_{label}_{}_{n}",
        std::process::id()
    ));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        skip_loudly(
            SUITE,
            &format!("cannot prepare {}: {error}", dir.display()),
        );
        return None;
    }
    Some(dir)
}

/// `erlc` beside the resolved oracle, else `erlc` on `$PATH`.
fn resolve_erlc(oracle: &Path) -> Option<PathBuf> {
    if let Some(dir) = oracle.parent() {
        let candidate = dir.join("erlc");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    find_on_path("erlc")
}

/// Compiles the two fixtures into one scratch directory, or answers `None` after a loud skip.
///
/// A rejected fixture source **panics**: it is a repository defect, not a skip.
fn compile_fixtures() -> Option<Fixture> {
    let oracle = require_oracle(SUITE)?;
    let Some(erlc) = resolve_erlc(&oracle) else {
        skip_loudly(SUITE, "no `erlc` beside the oracle and none on `$PATH`");
        return None;
    };
    let dir = scratch("compile")?;
    let mut sources = Vec::new();
    for (module, content) in [("mi_order_plain", PLAIN_SOURCE), ("mi_order_tests", EUNIT_SOURCE)] {
        let source = dir.join(format!("{module}.erl"));
        std::fs::write(&source, content)
            .unwrap_or_else(|error| panic!("cannot write {}: {error}", source.display()));
        sources.push(source);
    }
    let mut command = std::process::Command::new(&erlc);
    command.arg("-o").arg(&dir);
    command.args(&sources);
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("cannot run `{}`: {error}", erlc.display()));
    assert!(
        output.status.success(),
        "`{}` rejected a fixture: {}\n{}",
        erlc.display(),
        String::from_utf8_lossy(&output.stderr).trim(),
        String::from_utf8_lossy(&output.stdout).trim()
    );
    for module in ["mi_order_plain", "mi_order_tests"] {
        assert!(
            dir.join(format!("{module}.beam")).is_file(),
            "`erlc` produced no {module}.beam"
        );
    }
    Some(Fixture { dir })
}

// ---------------------------------------------------------------------------------------
// Running one expression under either runtime
// ---------------------------------------------------------------------------------------

/// Runs `expr` under `program` with `-pa dir` ahead of `-eval` (an `erl` evaluates the
/// command line in order, so a later `-pa` would be too late — the same rule holds for
/// `beam_rt`), and answers its stdout.
///
/// A child that could not be spawned/collected answers `None` after the harness's warning; a
/// child that exits non-zero is a **failure** (the expression is a probe we wrote).
fn run_expr(program: &Path, dir: &Path, expr: &str) -> Option<Output> {
    let dir_arg = dir.to_string_lossy().into_owned();
    let output = support::run_bounded(program, &["-noshell", "-pa", &dir_arg, "-eval", expr])?;
    Some(output)
}

/// [`run_expr`], asserting a clean exit and returning stdout as text.
fn run_ok(program: &Path, dir: &Path, expr: &str, what: &str) -> String {
    let output = run_expr(program, dir, expr)
        .unwrap_or_else(|| panic!("{what}: could not run `{}`", program.display()));
    assert!(
        output.status.success(),
        "{what}: `{}` exited with {:?}\nstderr:\n{}\n--- eval ---\n{expr}",
        program.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Runs `expr` under the oracle with the fixture directory as its code path.
///
/// `None` (after the harness's warning) is a loud skip: the probe could not answer.
fn oracle_probe(oracle: &Path, dir: &Path, expr: &str) -> Option<String> {
    let answer = support::oracle_eval(oracle, Some(dir), expr);
    if answer.is_none() {
        skip_loudly(SUITE, "the oracle probe could not answer");
    }
    answer
}

/// The `{Name, Arity}` pairs of a printed `[{name,arity}, …]` list, in transcript order.
///
/// The printer wraps long lists over several lines, so the text is scanned for
/// `{name,arity}` occurrences rather than split by lines.
fn printed_callables(text: &str) -> Vec<String> {
    let mut pairs = Vec::new();
    let bytes: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '{' {
            let mut j = i + 1;
            let name_start = j;
            while j < bytes.len() && (bytes[j].is_alphanumeric() || bytes[j] == '_' || bytes[j] == '@') {
                j += 1;
            }
            let name: String = bytes[name_start..j].iter().collect();
            if j < bytes.len() && bytes[j] == ',' {
                let arity_start = j + 1;
                let mut k = arity_start;
                while k < bytes.len() && bytes[k].is_ascii_digit() {
                    k += 1;
                }
                if k > arity_start && k < bytes.len() && bytes[k] == '}' && !name.is_empty() {
                    let arity: String = bytes[arity_start..k].iter().collect();
                    pairs.push(format!("{name}/{arity}"));
                    i = k + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    pairs
}

// ---------------------------------------------------------------------------------------
// 1. `exports` — the order itself, against the oracle and against the recorded pin
// ---------------------------------------------------------------------------------------

/// Every `module_info/1` item of a compiled fixture prints **identically** under `beam_rt` and
/// under the oracle, so `exports` is the oracle's order element for element.
///
/// The items are printed in one probe so the two runtimes answer the same shapes back to back
/// (`attributes`/`compile` are decoded `term_to_binary` chunks; `md5` the file's digest).
#[test]
fn the_export_item_is_the_oracles_order_element_for_element() {
    let Some(fixture) = compile_fixtures() else {
        return;
    };
    let oracle = require_oracle(SUITE).expect("the fixtures were compiled, so an oracle exists");
    // `~w` (not `~p`) so every item stays on one line: the lists are compared as whole
    // lines, and a pretty-printer's line breaking is not part of what is being pinned.
    let expr = "io:format(\"exports=~w~n\",[mi_order_plain:module_info(exports)]),\
                io:format(\"functions=~w~n\",[mi_order_plain:module_info(functions)]),\
                io:format(\"module=~w~n\",[mi_order_plain:module_info(module)]),\
                io:format(\"attributes=~w~n\",[mi_order_plain:module_info(attributes)]),\
                io:format(\"md5=~w~n\",[mi_order_plain:module_info(md5)]),\
                halt(0).";
    let Some(theirs) = oracle_probe(&oracle, fixture.path(), expr) else {
        fixture.cleanup();
        return;
    };
    let ours = run_ok(Path::new(BINARY), fixture.path(), expr, "beam_rt");
    assert_eq!(
        ours.trim(),
        theirs.trim(),
        "every `module_info/1` item must print exactly as the oracle prints it (order included)"
    );
    // The recorded pin, so the order is still asserted when the oracle cannot be run.
    let exports_line = ours
        .lines()
        .find(|line| line.starts_with("exports="))
        .expect("the exports line");
    assert_eq!(
        printed_callables(exports_line),
        PLAIN_EXPORTS
            .iter()
            .map(|pair| pair.to_string())
            .collect::<Vec<_>>(),
        "`exports` is the oracle's order: reverse(ExpT), i.e. the fixture's source order"
    );
    fixture.cleanup();
}

// ---------------------------------------------------------------------------------------
// 2. The behaviour the order exists for: eunit's verbose transcript
// ---------------------------------------------------------------------------------------

/// A three-generator `_tests` module reports its generators in the oracle's order under
/// `eunit:test(…, [verbose])`.
///
/// This is the observable the `exports` order exists for: `eunit` iterates
/// `Module:module_info(exports)` to find the generators, so a reversed list reverses the
/// transcript. (The per-test wall-clock annotations differ run to run, so the assertion is on
/// the **order of the generator names**, not on the transcript text.)
#[test]
fn the_eunit_verbose_generator_order_is_the_oracles() {
    let Some(fixture) = compile_fixtures() else {
        return;
    };
    let oracle = require_oracle(SUITE).expect("the fixtures were compiled, so an oracle exists");
    let expr = "R = eunit:test([mi_order_tests], [verbose]),\
                io:format(\"RESULT=~p~n\",[R]),\
                halt(0).";
    let Some(theirs) = oracle_probe(&oracle, fixture.path(), expr) else {
        fixture.cleanup();
        return;
    };
    let ours = run_ok(Path::new(BINARY), fixture.path(), expr, "beam_rt");
    for (who, text) in [("the oracle", &theirs), ("beam_rt", &ours)] {
        assert!(
            text.contains("All 4 tests passed.") && text.contains("RESULT=ok"),
            "{who} must pass the fixture's four tests; transcript:\n{text}"
        );
    }
    assert_eq!(
        generator_order(&ours),
        generator_order(&theirs),
        "the verbose transcript's generator order must be the oracle's"
    );
    assert_eq!(
        generator_order(&ours),
        EUNIT_GENERATORS
            .iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>(),
        "and it is the declaration order, not the sorted one"
    );
    fixture.cleanup();
}

/// The generator names of an `eunit` verbose transcript, in the order they are reported (one
/// entry per `*_test_/0` occurrence, first appearance only).
fn generator_order(transcript: &str) -> Vec<String> {
    let mut order = Vec::new();
    for line in transcript.lines() {
        for candidate in EUNIT_GENERATORS {
            if line.contains(candidate) && !order.iter().any(|seen| seen == candidate) {
                order.push(candidate.to_string());
            }
        }
    }
    order
}
