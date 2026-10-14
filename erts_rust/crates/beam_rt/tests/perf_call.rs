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

//! The `examples/perf_call` harness's **own** smoke test.
//!
//! `perf_call` is the crate's registered-BIF dispatch micro-benchmark (`PERF.md`), and a
//! measurement harness that silently stops measuring is worse than none: a wrong row set would
//! produce a plausible table of numbers for workloads nobody selected. So this suite drives the
//! **built example binary** and asserts the two contracts the harness documents in its header:
//!
//! * every row it claims to measure is actually printed, under the block that names it — the
//!   `erlang:abs/1` / `erlang:trunc/1` / `lists:reverse/1` / `erlang:'++'/2` / `lists:seq/2`
//!   direct rows, the two ETS rows, the three measurement blocks and the copy-out delta table;
//! * a missing `erlc` (path C compiles its fixture with it) is a **loud skip that exits 0** —
//!   a banner naming what is missing and stating that nothing was measured — never a silent
//!   pass and never a panic.
//!
//! # Locating the example binary
//!
//! Cargo exposes `CARGO_BIN_EXE_<name>` for **bins** only, so an example has no compile-time
//! path env var. It *is* built by a plain `cargo test -p beam_rt` (examples are compiled as
//! part of the default target selection) into `<profile>/examples/`, and this test binary runs
//! from `<profile>/deps/`, so the artifact is `current_exe()`'s grandparent joined with
//! `examples/perf_call`. When it is genuinely absent (a `--lib`-only or hand-assembled run),
//! the test says so **loudly** and fails rather than passing vacuously.
//!
//! # Environment discipline
//!
//! The oracle toolchain is discovered by **filesystem only** (`which erlc` is never run), and
//! the "missing toolchain" case is driven by handing the harness a `PATH` containing no
//! executables at all — the same probe the harness itself makes.

#![deny(unsafe_code)]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// The row labels the harness must print, one per measured registered-BIF key.
const ROW_LABELS: &[&str] = &[
    "erlang:abs/1",
    "erlang:trunc/1",
    "lists:reverse/1",
    "erlang:'++'/2",
    "lists:seq/2",
    "ets:insert/2",
    "ets:lookup/2",
];

/// The block/banner titles that make the table readable, so a renamed or dropped block is
/// caught too.
const BLOCK_TITLES: &[&str] = &[
    "block A: RuntimeContext::new",
    "block A2: the same context, the ETS rows",
    "block B: RuntimeContext::for_process",
    "block B2: the same context, the ETS rows",
    "block C: the scheduled `run` route",
    "copy-out delta (path B - path A)",
    "control = the same loop with inlined arithmetic only",
];

/// The iteration count the smoke test drives the direct rows with: enough for a real sample,
/// far below the harness's own default so the suite stays fast.
const SMOKE_ITERATIONS: &str = "2000";

/// The path of the built `examples/perf_call`, or `None` when it was not built.
fn example_binary() -> Option<PathBuf> {
    // `<profile>/deps/perf_call-<hash>` -> `<profile>` -> `<profile>/examples/perf_call`.
    let exe = std::env::current_exe().ok()?;
    let profile = exe.parent()?.parent()?;
    let candidate = profile.join("examples").join("perf_call");
    candidate.is_file().then_some(candidate)
}

/// The built example binary, or a loud failure naming the command that builds it.
fn require_example_binary() -> PathBuf {
    match example_binary() {
        Some(path) => path,
        None => panic!(
            "the `perf_call` example binary was not found next to this test binary — build it \
             with `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml \
             -p beam_rt --example perf_call` (or just run `cargo test -p beam_rt`, which builds \
             the crate's examples), then re-run this suite. Failing loudly rather than passing \
             without having driven the harness."
        ),
    }
}

/// Whether the system `erlc` is on `$PATH` — a **filesystem** probe; the tool is never run.
fn erlc_available() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .any(|dir| is_executable(&dir.join("erlc")))
}

/// Whether `path` is an existing executable file.
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match fs::metadata(path) {
            Ok(metadata) => metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Runs the example with `args`, optionally replacing `$PATH`, and returns its output.
fn run_example(exe: &Path, args: &[&str], path: Option<OsString>) -> Output {
    let mut command = Command::new(exe);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(path) = path {
        command.env("PATH", path);
    }
    command
        .output()
        .unwrap_or_else(|error| panic!("cannot run {}: {error}", exe.display()))
}

/// An empty directory to point `$PATH` at, so no executable can be found.
fn empty_path() -> OsString {
    let dir = std::env::temp_dir().join(format!("beam_rt-perf-call-nopath-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("a scratch directory for the empty-PATH probe");
    OsString::from(dir)
}

#[test]
fn the_harness_prints_every_row_and_block_it_documents() {
    let exe = require_example_binary();
    let output = run_example(&exe, &[SMOKE_ITERATIONS], None);
    if !erlc_available() {
        // Path C compiles its fixture with `erlc`, so a toolchain-less host gets the harness's
        // documented loud skip instead of a table. Assert *that* contract — the point is that
        // this case is never mistaken for a pass.
        assert_loud_skip(&output);
        eprintln!(
            "perf_call smoke: no `erlc` on $PATH, so the row set could not be driven; the \
             harness's loud-skip contract was asserted instead (NOT a silent pass)."
        );
        return;
    }
    assert!(
        output.status.success(),
        "perf_call exited with {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    for label in ROW_LABELS {
        assert!(
            stdout.contains(label),
            "the harness no longer prints the row `{label}`; the measured row set changed"
        );
    }
    for title in BLOCK_TITLES {
        assert!(
            stdout.contains(title),
            "the harness no longer prints the block `{title}`"
        );
    }
    // The harness is a *measurement*: every block must carry its load caveat, and the delta
    // table must be there (it is what attributes the copy-out cost).
    assert!(
        stdout.contains("loadavg before:") && stdout.contains("loadavg after:"),
        "the harness must record /proc/loadavg around every block"
    );
    assert!(
        stdout.contains("marginal_ns"),
        "the harness must print the load-invariant marginal column"
    );
}

#[test]
fn a_missing_erlc_is_a_loud_skip_with_exit_zero() {
    let exe = require_example_binary();
    // An empty `$PATH`: `find_on_path("erlc")` finds nothing, and the harness must classify
    // that as a missing prerequisite *before* it boots a runtime — exit 0, banner on stderr,
    // and no measurement claimed.
    let output = run_example(&exe, &[SMOKE_ITERATIONS], Some(empty_path()));
    assert_loud_skip(&output);
}

/// Asserts `output` is the harness's **loud skip**: exit 0, a banner on stderr naming the
/// missing prerequisite, an explicit "NOT a pass", and nothing that looks like a measurement.
fn assert_loud_skip(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "a missing prerequisite must be a skip (exit 0), not a failure — and never a panic"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("SKIPPED"),
        "the skip must be loud (a banner on stderr); got:\n{stderr}"
    );
    assert!(
        stderr.contains("NOT a pass"),
        "the skip must state that nothing was measured; got:\n{stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("ns/call"),
        "a skipped run must print no measurement table; got:\n{stdout}"
    );
}
