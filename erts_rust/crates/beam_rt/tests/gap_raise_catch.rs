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
//! **A capability gap is catchable by user code** — the raise-landing regression suite.
//!
//! # The defect this pins
//!
//! A raise's reason may be an atom **name** rather than an atom term any module built: every
//! runtime diagnostic carries one (`Raise::error(ExitReason::atom("unsupported: …"))`), and no
//! module's atom chunk spells it because the runtime, not Erlang code, produced it. The engine's
//! raise funnel spells the reason through its atom-namespace seam and **declines** when the name
//! is unknown (`beam_engine`'s `namespace_atom`), so unless the runtime names the atom in its one
//! shared live namespace first, the raise escapes every `try … catch` and kills the process.
//!
//! `RuntimeContext::name_raise_reason_atom` is that naming step. It used to **skip** a
//! capability gap, on the theory that a gap must stay uncatchable so the CLI could report it;
//! the effect was that *any* user-level `try … catch` around a typed gap was defeated and the
//! process died. Naming is what makes the raise land, and the gap's **classification**
//! (`crate::error::unsupported_reason` → the `unsupported: …` stderr line and exit 3) never
//! depended on it.
//!
//! # Why the probe cannot depend on another crate
//!
//! The raise is produced by **this** node: `erlang:system_monitor/1` is a curated capability gap
//! of this runtime (`src/error.rs`'s `KNOWN_UNIMPLEMENTED_ERLANG_BIFS`, consulted by
//! `call_bif_routed`), so nothing about the case rides on `beam_term`/`beam_bifs` coverage. The
//! test then proves its own premise twice over: the uncaught run's reason is read back off the
//! CLI's `unsupported: …` line, and the compiled fixture's **atom chunk** is loaded
//! (`beam_loader`) and asserted to hold no such atom — the module provably cannot spell the
//! reason, so a landed raise can only come from the runtime's naming.
//!
//! The oracle is not compared on the reason (it has no gap here — it raises `badarg` for the
//! same call); [`the_oracle_catches_the_same_shape_with_a_program_level_reason`] compares the
//! **shape** instead: class `error`, non-empty stacktrace, only the reason differing.
//!
//! # Running
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt \
//!     --test gap_raise_catch
//! ```
use std::path::{Path, PathBuf};
use beam_loader::load_file;
mod support;
/// The suite's banner in a loud skip.
const SUITE: &str = "gap_raise_catch";
/// The runtime binary under test.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");
/// The one transcript line a tier must produce, for both the catching run and its result line.
const CAUGHT_LINE: &str = "caught class=error gap_atom=true frames_nonempty=true";
/// The two execution tiers every observation is made on.
#[derive(Clone, Copy)]
enum Tier {
    /// The default: the native baseline JIT.
    Native,
    /// `BEAM_RT_TIER=interpreter`, the direct-threaded reference engine.
    Interpreter,
}
impl Tier {
    /// The name a failure message uses.
    fn label(self) -> &'static str {
        match self {
            Tier::Native => "the default (baseline) tier",
            Tier::Interpreter => "the interpreter tier",
        }
    }
    /// The `BEAM_RT_TIER` value: `None` removes the variable so the built-in default applies.
    fn env_setting(self) -> Option<&'static str> {
        match self {
            Tier::Native => None,
            Tier::Interpreter => Some("interpreter"),
        }
    }
}
/// The compiler beside `$ERTS_ORACLE` first (the same installation), then `$PATH`.
///
/// Filesystem discovery only — a probe must never *run* `erlc` to find out whether it exists.
fn resolve_erlc() -> Option<PathBuf> {
    if let Some(dir) = support::resolve_oracle().as_deref().and_then(Path::parent) {
        let candidate = dir.join("erlc");
        if support::is_executable(&candidate) {
            return Some(candidate);
        }
    }
    support::find_on_path("erlc")
}
/// A compiled fixture in its own scratch directory (removed on drop).
///
/// The directory holds **only** this module's `.beam`, so it is a safe single `-pa` for the
/// oracle, and the module is the only image whose atom chunk the premise check can see.
struct Fixture {
    /// The scratch directory holding the `.beam`.
    dir: PathBuf,
    /// The compiled `.beam`.
    beam: PathBuf,
}
impl Fixture {
    /// The compiled image as a `str`, for the CLI's argv.
    fn beam_str(&self) -> &str {
        self.beam
            .to_str()
            .unwrap_or_else(|| panic!("the fixture path is not UTF-8: {}", self.beam.display()))
    }
    /// The scratch directory as a `str`, for `-pa`.
    fn dir_str(&self) -> &str {
        self.dir
            .to_str()
            .unwrap_or_else(|| panic!("the fixture dir is not UTF-8: {}", self.dir.display()))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            eprintln!(
                "warning: could not remove the fixture dir {}: {error}",
                self.dir.display()
            );
        }
    }
}
/// A per-call serial, so two tests of this binary compiling the same fixture **concurrently**
/// never share a scratch directory (each one's `Drop` removes only its own).
fn scratch_serial() -> usize {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}
/// Compiles `tests/fixtures/<module>.erl` into a fresh scratch directory, or `None` after a
/// **loud** skip naming what is missing.
fn build_fixture(module: &'static str) -> Option<Fixture> {
    let Some(erlc) = resolve_erlc() else {
        support::skip_loudly(SUITE, "no `erlc` available (and none beside $ERTS_ORACLE)");
        return None;
    };
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(format!("{module}.erl"));
    assert!(
        source.is_file(),
        "the committed fixture source is missing: {}",
        source.display()
    );
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_gap_raise_{module}_{}_{}",
        std::process::id(),
        scratch_serial()
    ));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        support::skip_loudly(SUITE, &format!("cannot prepare {}: {error}", dir.display()));
        return None;
    }
    let output = match std::process::Command::new(&erlc)
        .arg("-o")
        .arg(&dir)
        .arg(&source)
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            support::skip_loudly(SUITE, &format!("cannot run `erlc`: {error}"));
            return None;
        }
    };
    if !output.status.success() {
        support::skip_loudly(
            SUITE,
            &format!(
                "`erlc` exited with {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        );
        return None;
    }
    let beam = dir.join(format!("{module}.beam"));
    assert!(
        beam.is_file(),
        "`erlc` produced no {}; stderr:\n{}",
        beam.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    Some(Fixture { dir, beam })
}
/// One finished CLI invocation: its exit code and captured output.
struct Cli {
    /// The process exit code (`-1` when killed by a signal, which no probe here does).
    code: i32,
    /// Trimmed stdout.
    out: String,
    /// Trimmed stderr.
    err: String,
}
/// The environment every probe gets: no crash-dump litter, and the requested execution tier.
fn probe_env(tier: Tier) -> [(&'static str, Option<&'static str>); 2] {
    [
        ("ERL_CRASH_DUMP_SECONDS", Some("0")),
        ("BEAM_RT_TIER", tier.env_setting()),
    ]
}
/// Runs `program` bounded, so a probe that blocks can never hang the suite.
fn run(program: &Path, args: &[&str], tier: Tier, what: &str) -> Cli {
    let output =
        support::run_bounded_with_env(program, args, &probe_env(tier)).unwrap_or_else(|| {
            panic!(
                "{what}: `{}` did not finish within the probe bound",
                program.display()
            )
        });
    Cli {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}
/// This runtime's scheduled-route transcript for `<module>:<function>/0`.
fn ours_run(fixture: &Fixture, function: &str, tier: Tier, what: &str) -> Cli {
    let spec = format!("gap_raise:{function}/0");
    run(
        Path::new(BINARY),
        &["-noshell", "run", fixture.beam_str(), &spec],
        tier,
        what,
    )
}
#[test]
fn a_capability_gap_named_only_by_the_runtime_is_caught_by_user_code() {
    let Some(fixture) = build_fixture("gap_raise") else {
        return;
    };
    let uncaught = ours_run(&fixture, "uncaught", Tier::Native, "the uncaught gap run");
    assert_eq!(
        uncaught.code, 3,
        "an uncaught capability gap is the `unsupported` exit code; stderr:\n{}",
        uncaught.err
    );
    assert!(
        uncaught.out.is_empty(),
        "the dying process prints no transcript: {:?}",
        uncaught.out
    );
    let reason = uncaught
        .err
        .strip_prefix("beam_rt: unsupported: ")
        .unwrap_or_else(|| {
            panic!(
                "an uncaught gap renders one `beam_rt: unsupported: <reason>` line, got: {:?}",
                uncaught.err
            )
        });
    assert!(!reason.is_empty(), "the reason is named: {:?}", uncaught.err);
    assert_eq!(
        reason.lines().count(),
        1,
        "one stderr line, no usage block: {:?}",
        uncaught.err
    );
    // **The premise, proven from the artifact**: the fixture's atom chunk cannot spell the
    // reason atom, so the raise below can only land because the runtime named it.
    let reason_atom = format!("unsupported: {reason}");
    let module = load_file(fixture.beam_str()).expect("the fixture loads");
    assert!(
        module.atom_table.lookup(&reason_atom).is_none(),
        "premise: the fixture's atom chunk must not hold `{reason_atom}`"
    );
    for tier in [Tier::Native, Tier::Interpreter] {
        let caught = ours_run(&fixture, "caught", tier, &format!("the catch on {}", tier.label()));
        assert_eq!(
            caught.code,
            0,
            "the process survives a caught gap on {}; stderr:\n{}",
            tier.label(),
            caught.err
        );
        assert!(
            caught.err.is_empty(),
            "a caught gap writes nothing to stderr on {}: {:?}",
            tier.label(),
            caught.err
        );
        let lines: Vec<&str> = caught.out.lines().collect();
        assert_eq!(
            lines.first().copied(),
            Some(CAUGHT_LINE),
            "class `error`, an atom reason carrying the gap prefix and a non-empty stacktrace \
             on {}: {:?}",
            tier.label(),
            caught.out
        );
        assert_eq!(
            lines.get(1).copied(),
            Some("ok"),
            "the fixture returns `ok`, so the `run` action's result line follows the transcript \
             on {}: {:?}",
            tier.label(),
            caught.out
        );
    }
}
#[test]
fn the_oracle_catches_the_same_shape_with_a_program_level_reason() {
    let Some(oracle) = support::require_oracle(SUITE) else {
        return;
    };
    let Some(fixture) = build_fixture("gap_raise") else {
        return;
    };
    let Some(out) = support::oracle_eval(
        &oracle,
        Some(Path::new(fixture.dir_str())),
        "gap_raise:caught(), halt(0).",
    ) else {
        support::skip_loudly(SUITE, "the oracle probe did not answer");
        return;
    };
    // The same **shape** — `error` class, non-empty stacktrace — with a *program-level* reason:
    // the oracle has no capability gap here (`erlang:system_monitor/1` raises `badarg`), which is
    // why the runtime's own run asserts the gap reason and this one does not.
    assert_eq!(
        out.lines().next(),
        Some("caught class=error gap_atom=false frames_nonempty=true"),
        "the oracle's catch: {out:?}"
    );
}
