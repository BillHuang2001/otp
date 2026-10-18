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

//! The **code path** (contract A.9) — the oracle-comparison suite.
//!
//! # Why this suite exists
//!
//! `code:get_path/0` is not a detail: it is the list the loader searches, so every application
//! the path is missing is an application whose modules the runtime cannot load. The system
//! `erl` derives that list from the **release layout** — `code_server.erl`'s `init/2` scans the
//! installation's `<lib>` (`make_path/2` over `choose_bundles/1`), adds the `$ERL_LIBS`
//! directories (`get_user_lib_dirs/0,1`) and puts the two prim-loaded applications
//! (`erl_prim_loader:get_path/0`) in front — where a runtime that knew only `stdlib`/`kernel`/
//! `erts` answered five entries on a host whose oracle answered thirty-six.
//!
//! `beam_rt` reproduces that derivation (`src/codeserver/path.rs`); this suite is its
//! **compatibility gate**. Both runtimes are handed the *identical* `-eval` program and their
//! printed lists are compared **element by element, in order** — a missing application, an
//! extra one, a different version or a different position all fail.
//!
//! # The three observations
//!
//! | Test | What it pins |
//! |---|---|
//! | [`the_system_code_path_is_the_oracles_exactly`] | the whole release-derived list, in order, with the entry count |
//! | [`a_user_lib_directory_is_resolved_like_the_oracle`] | `$ERL_LIBS`: bundle scan, highest version, `ebin`-only, position |
//! | [`the_derivation_agrees_for_the_ambient_erl_libs_too`] | the same agreement for the inherited environment |
//!
//! Both sides are run with `$ERL_LIBS` **cleared** in the first two (`support::run_bounded_with_env`
//! removes it from the child only, so no other test is affected), because the oracle and the
//! runtime must see the same environment for the comparison to mean anything; the third test
//! deliberately hands both the ambient value instead.
//!
//! A missing toolchain is a **loud skip**, never a silent pass (`support::require_oracle`), and
//! every probe is bounded by [`support::PROBE_TIMEOUT`] — an `erl` probe must terminate, so the
//! oracle gets `-s init stop` as its backstop exactly as the other suites give it.

// The shared harness is compiled in whole, but this suite uses only its bounded runner and its
// toolchain discovery; the unused remainder is expected here — the same shape `tests/io_scan.rs`
// uses.
#[allow(dead_code)]
mod support;

use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The loud-skip banner.
const BANNER: &str = "code_path";

/// The executable under test.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// `-eval` that prints `code:get_path/0` — the observation both runtimes are compared on.
const PATH_PROBE: &str = "io:format(\"~p~n\",[code:get_path()]),halt(0).";

/// `-eval` that prints the oracle's own `length(code:get_path())`, so the entry count is
/// compared from the **oracle's** answer rather than only from the two parsed lists.
const COUNT_PROBE: &str = "io:format(\"~p~n\",[length(code:get_path())]),halt(0).";

// ---------------------------------------------------------------------------------------
// Running the two runtimes
// ---------------------------------------------------------------------------------------

/// One finished run.
struct Ran {
    code: i32,
    out: String,
    err: String,
}

impl Ran {
    fn of(output: Output) -> Ran {
        Ran {
            code: output.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
            err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        }
    }
}

/// Runs `expr` under `program`, bounded by [`support::PROBE_TIMEOUT`] and with `env` applied to
/// the **child** only (`None` removes a variable, so a comparison can be made hermetic without
/// mutating this process's environment).
///
/// `oracle` selects the oracle's command line, which needs its `-s init stop` backstop: a bare
/// `erl -noshell` never exits on its own.
fn run(program: &Path, oracle: bool, env: &[(&str, Option<&str>)], expr: &str, what: &str) -> Ran {
    let mut args: Vec<&str> = vec!["-noshell", "-eval", expr];
    if oracle {
        args.extend(["-s", "init", "stop"]);
    }
    let output = support::run_bounded_with_env(program, &args, env).unwrap_or_else(|| {
        panic!(
            "{what}: `{}` {args:?} could not be run to completion",
            program.display()
        )
    });
    Ran::of(output)
}

/// Asserts a run succeeded silently (the oracle's own warnings would otherwise hide in a diff).
fn assert_clean(run: &Ran, what: &str) {
    assert_eq!(
        run.code, 0,
        "{what}: the run must exit 0 — stdout {:?} stderr:\n{}",
        run.out, run.err
    );
    assert!(
        run.err.is_empty(),
        "{what}: a successful run must be silent on stderr — {}",
        run.err
    );
}

/// The string elements of an Erlang list literal printed with `~p`, in order.
///
/// The printer wraps a long list over several lines (and never escapes these paths, which are
/// plain ASCII), so this scans for `"`-delimited runs and undoes the escapes a path could carry.
/// `None` when the text is not a list literal at all — an unparsable answer must fail the test,
/// never be read as an empty path.
fn parse_strings(text: &str) -> Option<Vec<String>> {
    let text = text.trim();
    if !text.starts_with('[') || !text.ends_with(']') {
        return None;
    }
    let mut strings: Vec<String> = Vec::new();
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '"' {
            continue;
        }
        let mut value = String::new();
        loop {
            match chars.next()? {
                '"' => break,
                '\\' => match chars.next()? {
                    'n' => value.push('\n'),
                    't' => value.push('\t'),
                    other => value.push(other),
                },
                other => value.push(other),
            }
        }
        strings.push(value);
    }
    Some(strings)
}

/// The two lists side by side, one row per index, with `!!` marking the rows that differ.
///
/// A failure message that only prints two long lists leaves the reader diffing them by eye; this
/// names the first divergence's index and position directly.
fn side_by_side(ours: &[String], theirs: &[String]) -> String {
    let mut rows: Vec<String> = Vec::new();
    for index in 0..ours.len().max(theirs.len()) {
        let ours_entry = ours.get(index).map(String::as_str).unwrap_or("<missing>");
        let theirs_entry = theirs.get(index).map(String::as_str).unwrap_or("<missing>");
        let marker = if ours_entry == theirs_entry { "  " } else { "!!" };
        rows.push(format!(
            "{marker} [{index:2}] beam_rt: {ours_entry}\n              erl:     {theirs_entry}"
        ));
    }
    rows.join("\n")
}

/// The two runtimes' `code:get_path/0`, asserted **equal element by element and in order**.
///
/// Returns this runtime's list so a caller can make its own, narrower assertions on it — or
/// `None` after the loud skip when no oracle is available.
fn assert_paths_agree(env: &[(&str, Option<&str>)], what: &str) -> Option<Vec<String>> {
    let oracle = support::require_oracle(BANNER)?;
    let beam = PathBuf::from(BINARY);
    let ours = run(&beam, false, env, PATH_PROBE, &format!("beam_rt: {what}"));
    assert_clean(&ours, &format!("beam_rt: {what}"));
    let theirs = run(&oracle, true, env, PATH_PROBE, &format!("erl: {what}"));
    assert_clean(&theirs, &format!("erl: {what}"));

    let ours_path = parse_strings(&ours.out).unwrap_or_else(|| {
        panic!("beam_rt: {what}: the printed code path is not a list literal: {:?}", ours.out)
    });
    let theirs_path = parse_strings(&theirs.out).unwrap_or_else(|| {
        panic!("erl: {what}: the printed code path is not a list literal: {:?}", theirs.out)
    });
    assert!(
        !ours_path.is_empty(),
        "beam_rt: {what}: the code path must not be empty"
    );
    assert_eq!(
        ours_path,
        theirs_path,
        "beam_rt: {what}: the code path differs from the oracle's ({} vs {} entries):\n{}",
        ours_path.len(),
        theirs_path.len(),
        side_by_side(&ours_path, &theirs_path)
    );

    // The entry count, asked of both runtimes and of the oracle's own `length/1`.
    let count = run(&beam, false, env, COUNT_PROBE, &format!("beam_rt: {what} (length)"));
    assert_clean(&count, &format!("beam_rt: {what} (length)"));
    let oracle_count = run(&oracle, true, env, COUNT_PROBE, &format!("erl: {what} (length)"));
    assert_clean(&oracle_count, &format!("erl: {what} (length)"));
    assert_eq!(
        count.out, oracle_count.out,
        "beam_rt: {what}: length(code:get_path()) differs from the oracle's"
    );
    assert_eq!(
        count.out.parse::<usize>().ok(),
        Some(ours_path.len()),
        "beam_rt: {what}: length(code:get_path()) must be the printed list's length"
    );
    Some(ours_path)
}

// ---------------------------------------------------------------------------------------
// The `$ERL_LIBS` fixture
// ---------------------------------------------------------------------------------------

/// A scratch `$ERL_LIBS` directory, removed when the test ends.
struct Fixture {
    root: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.root) {
            eprintln!(
                "warning: could not remove the fixture root {}: {error}",
                self.root.display()
            );
        }
    }
}

impl Fixture {
    /// The bundle directory `<root>/<name>`.
    fn bundle(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}

/// Builds `$ERL_LIBS` with two versions of one application, an unversioned second application
/// and a bundle with no `ebin` at all — or `None` after a loud skip.
fn user_libs_fixture() -> Option<Fixture> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "beam_rt_code_path_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let fixture = Fixture { root };
    // A stale directory from a killed run must not decide the result: start clean.
    match std::fs::remove_dir_all(&fixture.root) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            support::skip_loudly(
                BANNER,
                &format!("cannot clear {}: {error}", fixture.root.display()),
            );
            return None;
        }
    }
    for dir in [
        fixture.bundle("foo-1.0").join("ebin"),
        fixture.bundle("foo-2.0").join("ebin"),
        fixture.bundle("bar").join("ebin"),
        // A bundle whose `ebin` is missing contributes no entry at all.
        fixture.bundle("no_ebin"),
    ] {
        if let Err(error) = std::fs::create_dir_all(&dir) {
            support::skip_loudly(BANNER, &format!("cannot create {}: {error}", dir.display()));
            return None;
        }
    }
    Some(fixture)
}

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// The **whole** release-derived code path: `beam_rt` must answer exactly the oracle's list, in
/// the oracle's order, with the oracle's entry count.
///
/// This is the observation the 14 `undef_parse_transform` failures of the A.9 self-hosted-build
/// sweep came from: the parse transforms of `eunit`/`syntax_tools` live in applications whose
/// `ebin` a five-entry path does not carry, so the compiler could not load them. On this host the
/// oracle answers a path of many entries (`kernel` first, then `stdlib`, then the release's
/// applications descending by name, then `"."`); the assertion here is the **oracle's**, never a
/// literal of this suite's own.
#[test]
fn the_system_code_path_is_the_oracles_exactly() {
    // `$ERL_LIBS` is cleared for **both** children: the comparison is only meaningful when the
    // two runtimes see the same environment.
    let Some(path) = assert_paths_agree(&[("ERL_LIBS", None)], "the default code path") else {
        return;
    };
    assert!(
        path.iter().any(|entry| entry == "."),
        "the current directory must be on the path: {path:?}"
    );
}

/// `$ERL_LIBS` is scanned like a `<lib>` directory: each application contributes its `ebin`, the
/// **highest** version of an application wins, a bundle without `ebin` contributes nothing, and
/// the entries land after the prim pair and before the release layout — which is what
/// `code_server.erl`'s `get_user_lib_dirs/0,1` over `merge_path/3` produces.
#[test]
fn a_user_lib_directory_is_resolved_like_the_oracle() {
    let Some(fixture) = user_libs_fixture() else {
        return;
    };
    let root = fixture
        .root
        .to_str()
        .expect("an ASCII temporary path")
        .to_string();
    let env: &[(&str, Option<&str>)] = &[("ERL_LIBS", Some(&root))];
    let Some(path) = assert_paths_agree(env, "a $ERL_LIBS directory") else {
        return;
    };

    let foo_new = fixture.bundle("foo-2.0").join("ebin");
    let foo_old = fixture.bundle("foo-1.0").join("ebin");
    let bar = fixture.bundle("bar").join("ebin");
    assert!(
        path.contains(&foo_new.display().to_string()),
        "the highest version of `foo` must be on the path: {path:?}"
    );
    assert!(
        !path.contains(&foo_old.display().to_string()),
        "only one version of an application may be on the path: {path:?}"
    );
    assert!(
        path.contains(&bar.display().to_string()),
        "an unversioned bundle must be on the path: {path:?}"
    );
    assert!(
        !path.contains(&fixture.bundle("no_ebin").display().to_string()),
        "a bundle with no `ebin` must not be on the path: {path:?}"
    );
    // `merge_path/3` puts the `$ERL_LIBS` entries after the prim pair (kernel, stdlib) and
    // before the release's own applications.
    assert_eq!(
        path.get(2).map(String::as_str),
        Some(foo_new.to_str().expect("a UTF-8 path")),
        "$ERL_LIBS entries come directly after the prim pair, descending: {path:?}"
    );
    assert_eq!(
        path.get(3).map(String::as_str),
        Some(bar.to_str().expect("a UTF-8 path")),
        "$ERL_LIBS order is the scan's, not the filesystem's: {path:?}"
    );
}

/// The same agreement for the **inherited** `$ERL_LIBS`: whatever the ambient value is, both
/// runtimes must derive the same path from it.
///
/// A non-empty ambient value would name directories this suite does not control, so it is
/// reported loudly instead of being silently compared — the two tests above already cover the
/// derivation itself, with the variable cleared and with a value this suite builds.
#[test]
fn the_derivation_agrees_for_the_ambient_erl_libs_too() {
    match std::env::var("ERL_LIBS") {
        Err(_) => {
            let Some(path) = assert_paths_agree(&[], "an unset ambient $ERL_LIBS") else {
                return;
            };
            assert!(!path.is_empty(), "the code path must not be empty");
        }
        Ok(value) if value.is_empty() => {
            let Some(path) = assert_paths_agree(&[], "an empty ambient $ERL_LIBS") else {
                return;
            };
            assert!(!path.is_empty(), "the code path must not be empty");
        }
        Ok(value) => {
            eprintln!(
                "note: $ERL_LIBS is set to {value:?}, which names directories this suite does \
                 not control; the ambient comparison is not run (the cleared and synthetic \
                 cases above still pin the derivation)."
            );
        }
    }
}
