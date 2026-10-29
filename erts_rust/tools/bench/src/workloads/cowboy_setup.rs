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

//! The `cowboy` setup half: stage → build → compile the harness → per-runtime argv.
//!
//! Both rows of the `cowboy` workload must serve the **identical** `.beam`s through the
//! **identical** dispatch (`{'_', [{"/", toppage_h, []}]}`), so that the only difference
//! between the two rows is which runtime executes them. This module therefore owns the
//! one recipe both sides use to get a stage ready:
//!
//! 1. **stage** — copy the read-only Cowboy checkout's build inputs (`STAGE_ENTRIES`)
//!    into a private `ScratchDir`, never touching the checkout;
//! 2. **build** — one `make` in the staged copy (erlang.mk git-fetches `cowlib` and
//!    `ranch` into `<stage>/deps` and compiles `cowboy` into `<stage>/ebin`);
//! 3. **compile** — one `erlc` that compiles the `examples/hello_world` sources *and*
//!    the `HARNESS_SOURCE` harness into `<stage>/hw_ebin`;
//! 4. **argv** — `server_argv` builds the per-runtime command line: both sides carry the
//!    same four `-pa` directories in the same order (`pa_dirs`) and the same `CW_PORT`
//!    environment (`PORT_ENV`); only the tail differs (`-noshell … -eval
//!    cw_serve:start().` for the oracle, `run … cw_serve:start/0` for the Rust runtime,
//!    which is a drop-in `erl` for the flags but needs its `run` action to execute a
//!    `.beam` — contract A.6).
//!
//! Driving the staged tree — spawn, ready wait, drain, peak RSS, load, kill and port
//! release — is `cowboy_serve`'s half.
//!
//! Two honesty rules shape the failure surface:
//!
//! * a setup failure is a **typed** `SetupError` that knows whether it is a capability
//!   gap (`skipped: true` — the deps could not be fetched) or a genuine failure;
//! * the Rust runtime's `run` action kills the whole run at `RUN_DEADLINE_MS` (60 s,
//!   **not** overridable), so the load window is `cowboy_serve::clamp_load_duration`-
//!   clamped into a documented `RUN_SAFETY_MS` bound and the clamp is stated in a note —
//!   a row never silently rides into a `did not terminate within 60000 ms`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::proc;
use crate::runtime::RuntimeKind;
use crate::scratch::{self, ScratchDir};

/// The port the harness listens on (handed to the child as [`PORT_ENV`]).
pub(super) const PORT: u16 = 8080;

/// The environment variable through which the port reaches the harness.
pub(super) const PORT_ENV: &str = "CW_PORT";

/// The harness module compiled next to the example sources.
pub(super) const HARNESS_MODULE: &str = "cw_serve";

/// The stdout line the harness prints once the listener is up.
pub(super) const READY_MARKER: &str = "CW_READY";

/// Entries never staged: `.git` is irrelevant to the build and `_rel` is a release
/// artefact the harness path does not produce (or need).
pub(super) const SKIP_NAMES: &[&str] = &[".git", "_rel"];

/// The Rust runtime's `run` action kills the whole run at this deadline
/// (`beam_rt`'s `RUN_DEADLINE`), counting *everything* — boot, load, teardown.
pub(super) const RUN_DEADLINE_MS: f64 = 60_000.0;

/// The safety bound this driver keeps the whole run under, well inside
/// [`RUN_DEADLINE_MS`] so a boot hiccup cannot turn a row into a killed run.
pub(super) const RUN_SAFETY_MS: f64 = 50_000.0;

/// How much of the safety bound is reserved for kill + reap + port release.
pub(super) const TEARDOWN_ALLOWANCE_MS: f64 = 3_000.0;

/// The shortest load window worth publishing (below this the row fails loudly instead).
pub(super) const MIN_LOAD_MS: f64 = 250.0;

/// How much of the `make`/`erlc`/server output to embed in a note or failure reason.
pub(super) const OUTPUT_TAIL_CHARS: usize = 400;

/// The relocatable-path components of the staged tree.
const EXAMPLE_SRC_REL: &str = "examples/hello_world/src";
const HW_EBIN_REL: &str = "hw_ebin";

/// The harness: boots the `hello_world` app on [`PORT_ENV`]'s port, serves the
/// documented route through the example's own handler, announces readiness on stdout
/// and then blocks forever (the driver kills it).
///
/// The port comes from the environment because neither side's argv carries integers:
/// `run` accepts only a module/function and integer arguments, and the oracle's `-eval`
/// expression is a fixed string.
pub(super) const HARNESS_SOURCE: &str = r#"-module(cw_serve).
-export([start/0]).

%% Serve Cowboy's hello_world example on $CW_PORT and block until killed.
start() ->
    Port = port(),
    {ok, _} = application:ensure_all_started(cowboy),
    Dispatch = cowboy_router:compile([{'_', [{"/", toppage_h, []}]}]),
    {ok, _} = cowboy:start_clear(my_listener, [{port, Port}], #{env => #{dispatch => Dispatch}}),
    io:format("CW_READY~n"),
    receive never -> ok end.

port() ->
    case os:getenv("CW_PORT") of
        false -> 8080;
        Value ->
            try list_to_integer(Value) of
                P when is_integer(P), P > 0, P < 65536 -> P
            catch _:_ -> 8080 end
    end.
"#;

/// Whether an entry of the checkout is a directory or a single file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EntryKind {
    /// A directory subtree (staged with [`scratch::copy_tree`]).
    Dir,
    /// A single file (staged with [`scratch::copy_files`]).
    File,
}

/// One top-level checkout entry the staged build needs.
#[derive(Debug, Clone, Copy)]
pub(super) struct StageEntry {
    /// The entry's name in the checkout (and in the staged copy).
    pub name: &'static str,
    /// Whether it is a directory or a file.
    pub kind: EntryKind,
    /// Whether the build cannot work without it.
    pub required: bool,
}

/// The staged entries: the build inputs (required) plus the optional extras.
pub(super) const STAGE_ENTRIES: &[StageEntry] = &[
    StageEntry {
        name: "src",
        kind: EntryKind::Dir,
        required: true,
    },
    StageEntry {
        name: "examples",
        kind: EntryKind::Dir,
        required: true,
    },
    StageEntry {
        name: "Makefile",
        kind: EntryKind::File,
        required: true,
    },
    StageEntry {
        name: "erlang.mk",
        kind: EntryKind::File,
        required: true,
    },
    StageEntry {
        name: "ebin",
        kind: EntryKind::Dir,
        required: false,
    },
    StageEntry {
        name: "plugins.mk",
        kind: EntryKind::File,
        required: false,
    },
    StageEntry {
        name: "rebar.config",
        kind: EntryKind::File,
        required: false,
    },
];

/// The deadline-bound safety window a runtime imposes on the whole run, if any.
///
/// Only the Rust runtime has one: its `run` action enforces [`RUN_DEADLINE_MS`]. The
/// oracle is killed by us, so it has none.
pub(super) fn run_deadline_ms(kind: RuntimeKind) -> Option<f64> {
    match kind {
        RuntimeKind::Rust => Some(RUN_SAFETY_MS),
        RuntimeKind::Sys => None,
    }
}

/// The four `-pa` directories both sides get, in this order.
pub(super) fn pa_dirs(root: &Path) -> Vec<PathBuf> {
    vec![
        root.join("deps/ranch/ebin"),
        root.join("deps/cowlib/ebin"),
        root.join("ebin"),
        root.join(HW_EBIN_REL),
    ]
}

/// The per-runtime command line: `(program, argv)`.
///
/// Both sides get the **same** `-pa` directories in the **same** order; only the tail
/// differs, because the Rust runtime executes a `.beam` through its `run` action while
/// the oracle evaluates an expression (contract A.6: `beam_rt` is a drop-in `erl` for
/// the flags, never assumed to share its internals).
pub(super) fn server_argv(
    kind: RuntimeKind,
    program: &Path,
    pa: &[PathBuf],
    harness_beam: &Path,
) -> (PathBuf, Vec<String>) {
    let mut argv: Vec<String> = Vec::with_capacity(2 * pa.len() + 3);
    match kind {
        RuntimeKind::Sys => {
            argv.push("-noshell".to_string());
            push_pa(&mut argv, pa);
            argv.push("-eval".to_string());
            argv.push(format!("{HARNESS_MODULE}:start()."));
        }
        RuntimeKind::Rust => {
            push_pa(&mut argv, pa);
            argv.push("run".to_string());
            argv.push(harness_beam.display().to_string());
            argv.push(format!("{HARNESS_MODULE}:start/0"));
        }
    }
    (program.to_path_buf(), argv)
}

/// Appends `-pa <dir>` for every directory, in order.
fn push_pa(argv: &mut Vec<String>, pa: &[PathBuf]) {
    for dir in pa {
        argv.push("-pa".to_string());
        argv.push(dir.display().to_string());
    }
}

/// The child command for one side, with the port in its environment.
pub(super) fn serve_command(program: &Path, argv: &[String], port: u16) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(argv).env(PORT_ENV, port.to_string());
    cmd
}

/// How a side is driven once it is ready.
#[derive(Debug, Clone, Copy)]
pub(super) struct LoadPlan {
    /// Concurrent keep-alive connections.
    pub connections: usize,
    /// How long each connection issues requests (clamped by
    /// [`clamp_load_duration`](super::cowboy_serve::clamp_load_duration)).
    pub duration: Duration,
    /// Upper bound on the stored latency samples.
    pub max_latency_samples: usize,
}

/// A staged and built Cowboy tree, ready to serve.
///
/// Holds the [`ScratchDir`] so the tree survives for the whole row and is removed when
/// the row ends (including the error paths).
pub(super) struct Built {
    /// Held only for RAII: the staged tree lives exactly as long as this value.
    _scratch: ScratchDir,
    /// The four `-pa` directories, in order.
    pub pa: Vec<PathBuf>,
    /// `<root>/hw_ebin/cw_serve.beam`.
    pub harness_beam: PathBuf,
    /// Wall time of the single `make`.
    pub build_ms: f64,
    /// Wall time of the single `erlc`.
    pub compile_ms: f64,
    /// Informational notes (absent optional entries, …).
    pub notes: Vec<String>,
}

/// A setup failure, classified: a capability gap (`skipped`) or a genuine failure.
#[derive(Debug, Clone)]
pub(super) struct SetupError {
    /// `true` when the cause is a missing capability (the deps could not be fetched).
    pub skipped: bool,
    /// The short reason recorded on the result.
    pub reason: String,
    /// A build time that was measured before the failure, if any.
    pub build_ms: Option<f64>,
    /// A compile time that was measured before the failure, if any.
    pub compile_ms: Option<f64>,
    /// Notes gathered before the failure.
    pub notes: Vec<String>,
}

impl SetupError {
    /// A capability gap (`skipped(<reason>)`).
    pub(super) fn skip(reason: impl Into<String>) -> SetupError {
        SetupError {
            skipped: true,
            reason: reason.into(),
            build_ms: None,
            compile_ms: None,
            notes: Vec::new(),
        }
    }

    /// A genuine failure (`failed(<reason>)`).
    pub(super) fn fail(reason: impl Into<String>) -> SetupError {
        SetupError {
            skipped: false,
            reason: reason.into(),
            build_ms: None,
            compile_ms: None,
            notes: Vec::new(),
        }
    }

    /// Records the measured `make` wall time.
    fn with_build_ms(mut self, ms: f64) -> SetupError {
        self.build_ms = Some(ms);
        self
    }

    /// Records the measured `erlc` wall time.
    fn with_compile_ms(mut self, ms: f64) -> SetupError {
        self.compile_ms = Some(ms);
        self
    }

    /// Appends notes gathered during the failed step.
    fn with_notes(mut self, mut notes: Vec<String>) -> SetupError {
        self.notes.append(&mut notes);
        self
    }
}

/// Stages the checkout, builds it and compiles the harness — the one shared recipe.
pub(super) fn stage_and_build(
    repo: &Path,
    erlc: &Path,
    make: &Path,
) -> Result<Built, SetupError> {
    let scratch = ScratchDir::new("cowboy")
        .map_err(|e| SetupError::fail(format!("scratch-failed: {e}")))?;
    let root = scratch.path().join("cowboy");
    let mut notes = Vec::new();
    if let Err(e) = stage_entries(repo, &root, &mut notes) {
        return Err(e.with_notes(notes));
    }

    let build_ms = build(&root, make)?;
    let compile_ms = compile(&root, erlc)?;
    let harness_beam = root.join(HW_EBIN_REL).join(format!("{HARNESS_MODULE}.beam"));
    let pa = pa_dirs(&root);

    Ok(Built {
        _scratch: scratch,
        pa,
        harness_beam,
        build_ms,
        compile_ms,
        notes,
    })
}

/// Copies the [`STAGE_ENTRIES`] of the read-only checkout into `root`.
fn stage_entries(
    repo: &Path,
    root: &Path,
    notes: &mut Vec<String>,
) -> Result<(), SetupError> {
    for entry in STAGE_ENTRIES {
        let from = repo.join(entry.name);
        let present = match entry.kind {
            EntryKind::Dir => from.is_dir(),
            EntryKind::File => from.is_file(),
        };
        if !present {
            if entry.required {
                return Err(SetupError::fail(format!(
                    "repo-entry-missing: `{}` is absent from {}",
                    entry.name,
                    repo.display()
                )));
            }
            notes.push(format!(
                "note: optional staging entry `{}` is absent from the checkout",
                entry.name
            ));
            continue;
        }
        match entry.kind {
            EntryKind::Dir => {
                scratch::copy_tree(&from, &root.join(entry.name), SKIP_NAMES).map_err(|e| {
                    SetupError::fail(format!("copy-failed: `{}`: {e}", entry.name))
                })?;
            }
            EntryKind::File => {
                scratch::copy_files(&[from], root).map_err(|e| {
                    SetupError::fail(format!("copy-failed: `{}`: {e}", entry.name))
                })?;
            }
        }
    }
    Ok(())
}

/// Runs the single `make`, then checks the three directories the harness needs.
fn build(root: &Path, make: &Path) -> Result<f64, SetupError> {
    let mut cmd = Command::new(make);
    cmd.current_dir(root);
    let out = proc::run(&mut cmd)
        .map_err(|e| SetupError::fail(format!("make-spawn-failed: {e}")))?;
    if !out.success {
        let output = format!("{}\n{}", out.stdout, out.stderr);
        let tail = tail(&output, OUTPUT_TAIL_CHARS);
        if looks_like_deps_failure(&output) {
            let notes = vec![format!("make output tail: {tail}")];
            return Err(SetupError::skip("deps-unavailable")
                .with_build_ms(out.wall_ms)
                .with_notes(notes));
        }
        return Err(
            SetupError::fail(format!("build-failed: {tail}")).with_build_ms(out.wall_ms)
        );
    }
    for rel in ["deps/cowlib/ebin", "deps/ranch/ebin", "ebin"] {
        if !root.join(rel).is_dir() {
            return Err(SetupError::fail(format!(
                "build-missing: `{rel}` was not produced by `make`"
            ))
            .with_build_ms(out.wall_ms));
        }
    }
    Ok(out.wall_ms)
}

/// Compiles the example sources **and** the harness in one `erlc` invocation.
fn compile(root: &Path, erlc: &Path) -> Result<f64, SetupError> {
    let example_src = root.join(EXAMPLE_SRC_REL);
    let sources = crate::workloads::collect_files(&example_src, "erl");
    if sources.is_empty() {
        return Err(SetupError::fail(format!(
            "example-sources-missing: no `.erl` under {}",
            example_src.display()
        )));
    }

    let hw_ebin = root.join(HW_EBIN_REL);
    std::fs::create_dir_all(&hw_ebin)
        .map_err(|e| SetupError::fail(format!("hw-ebin-failed: {e}")))?;
    let harness_src = hw_ebin.join(format!("{HARNESS_MODULE}.erl"));
    std::fs::write(&harness_src, HARNESS_SOURCE)
        .map_err(|e| SetupError::fail(format!("harness-write-failed: {e}")))?;

    let mut cmd = Command::new(erlc);
    cmd.arg("-o").arg(&hw_ebin);
    for src in &sources {
        cmd.arg(src);
    }
    cmd.arg(&harness_src);
    let out = proc::run(&mut cmd)
        .map_err(|e| SetupError::fail(format!("erlc-spawn-failed: {e}")))?;
    if !out.success {
        let output = format!("{}\n{}", out.stdout, out.stderr);
        return Err(
            SetupError::fail(format!(
                "module-compile-failed: {}",
                tail(&output, OUTPUT_TAIL_CHARS)
            ))
            .with_compile_ms(out.wall_ms),
        );
    }

    // Every input must have produced a `.beam`, or the run would execute a stale one.
    let mut expected: Vec<String> = sources
        .iter()
        .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(str::to_string))
        .collect();
    expected.push(HARNESS_MODULE.to_string());
    for module in expected {
        if !hw_ebin.join(format!("{module}.beam")).is_file() {
            return Err(SetupError::fail(format!(
                "module-compile-failed: no `{module}.beam` in {}",
                hw_ebin.display()
            ))
            .with_compile_ms(out.wall_ms));
        }
    }
    Ok(out.wall_ms)
}

/// The last `max_chars` characters of `text` (trimmed), for a bounded note.
pub(super) fn tail(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    let count = trimmed.chars().count();
    if count <= max_chars {
        return trimmed.to_string();
    }
    trimmed.chars().skip(count - max_chars).collect()
}

/// Whether `make`'s output looks like a dependency/network fetch failure (as opposed
/// to a genuine compile error), so the workload can be `skipped` rather than `failed`.
///
/// erlang.mk fetches `cowlib`/`ranch` with `git`, so both the git transport's own
/// messages and the usual resolver/connect errors count.
pub(super) fn looks_like_deps_failure(output: &str) -> bool {
    let lowered = output.to_ascii_lowercase();
    const SIGNS: &[&str] = &[
        // DNS / routing / connect
        "could not resolve host",
        "could not resolve hostname",
        "temporary failure in name resolution",
        "network is unreachable",
        "no route to host",
        "connection refused",
        "connection timed out",
        "operation timed out",
        // git transport (erlang.mk fetches deps with `git`)
        "fatal: unable to access",
        "unable to access",
        "could not read from remote repository",
        "fatal: repository",
        "repository not found",
        "the remote end hung up unexpectedly",
        "unexpected disconnect while reading sideband packet",
        "transfer closed with outstanding read data remaining",
        "early eof",
        "ssl connect error",
        // generic fetchers
        "failed to fetch",
        "could not fetch",
        "rpc failed",
    ];
    SIGNS.iter().any(|sign| lowered.contains(sign))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workloads::cowboy_serve::clamp_load_duration;

    fn pa() -> Vec<PathBuf> {
        pa_dirs(Path::new("/stage"))
    }

    #[test]
    fn the_harness_pins_the_route_the_ready_marker_and_the_port_env() {
        assert!(HARNESS_SOURCE.contains(READY_MARKER), "no ready marker");
        assert!(
            HARNESS_SOURCE.contains(r#"[{'_', [{"/", toppage_h, []}]}]"#),
            "the documented dispatch/handler must be served by both sides"
        );
        assert!(HARNESS_SOURCE.contains("cowboy:start_clear"));
        assert!(HARNESS_SOURCE.contains("application:ensure_all_started(cowboy)"));
        assert!(
            HARNESS_SOURCE.contains(&format!("os:getenv(\"{PORT_ENV}\")")),
            "the port must come in through the environment"
        );
        assert!(HARNESS_SOURCE.contains(&format!("-module({HARNESS_MODULE}).")));
    }

    #[test]
    fn the_staged_entries_are_the_documented_set() {
        let names: Vec<&str> = STAGE_ENTRIES.iter().map(|e| e.name).collect();
        for required in ["src", "examples", "Makefile", "erlang.mk"] {
            let entry = STAGE_ENTRIES
                .iter()
                .find(|e| e.name == required)
                .unwrap_or_else(|| panic!("`{required}` must be staged"));
            assert!(entry.required, "`{required}` must be required");
        }
        for optional in ["ebin", "plugins.mk", "rebar.config"] {
            let entry = STAGE_ENTRIES
                .iter()
                .find(|e| e.name == optional)
                .unwrap_or_else(|| panic!("`{optional}` must be staged"));
            assert!(!entry.required, "`{optional}` must stay optional");
        }
        for skipped in [".git", "_rel"] {
            assert!(SKIP_NAMES.contains(&skipped), "`{skipped}` must be skipped");
        }
        for name in &names {
            assert!(
                !SKIP_NAMES.contains(name),
                "`{name}` cannot be both staged and skipped"
            );
        }
        assert_eq!(names.len(), 7, "staged set: {names:?}");
    }

    #[test]
    fn the_pa_dirs_are_the_four_core_build_outputs_in_order() {
        assert_eq!(
            pa(),
            vec![
                PathBuf::from("/stage/deps/ranch/ebin"),
                PathBuf::from("/stage/deps/cowlib/ebin"),
                PathBuf::from("/stage/ebin"),
                PathBuf::from("/stage/hw_ebin"),
            ]
        );
    }

    /// Collects the directories that follow each `-pa` flag, in order.
    fn pa_pairs(argv: &[String]) -> Vec<String> {
        argv.windows(2)
            .filter(|pair| pair[0] == "-pa")
            .map(|pair| pair[1].clone())
            .collect()
    }

    #[test]
    fn both_sides_carry_the_same_pa_dirs_and_only_the_tail_differs() {
        let beam = Path::new("/stage/hw_ebin/cw_serve.beam");
        let (sys_prog, sys_argv) =
            server_argv(RuntimeKind::Sys, Path::new("/usr/bin/erl"), &pa(), beam);
        let (rust_prog, rust_argv) =
            server_argv(RuntimeKind::Rust, Path::new("/rt/beam_rt"), &pa(), beam);

        let expected: Vec<String> = pa().iter().map(|d| d.display().to_string()).collect();
        assert_eq!(pa_pairs(&sys_argv), expected, "sys -pa set/order");
        assert_eq!(pa_pairs(&rust_argv), expected, "rust -pa set/order");

        assert_eq!(sys_prog, PathBuf::from("/usr/bin/erl"));
        assert_eq!(rust_prog, PathBuf::from("/rt/beam_rt"));

        // The oracle evaluates an expression; the Rust runtime executes the `.beam`.
        assert_eq!(sys_argv.first().map(String::as_str), Some("-noshell"));
        assert_eq!(
            &sys_argv[sys_argv.len() - 2..],
            ["-eval".to_string(), "cw_serve:start().".to_string()]
        );
        assert_eq!(
            &rust_argv[rust_argv.len() - 3..],
            [
                "run".to_string(),
                "/stage/hw_ebin/cw_serve.beam".to_string(),
                "cw_serve:start/0".to_string()
            ]
        );
        // Neither passes the port on the command line: it rides the environment.
        assert!(sys_argv.iter().all(|a| !a.contains(PORT_ENV)));
        assert!(rust_argv.iter().all(|a| !a.contains(PORT_ENV)));
    }

    #[test]
    fn both_sides_get_the_port_in_their_environment() {
        let beam = Path::new("/stage/hw_ebin/cw_serve.beam");
        for kind in [RuntimeKind::Sys, RuntimeKind::Rust] {
            let (program, argv) = server_argv(kind, Path::new("/bin/erl"), &pa(), beam);
            let cmd = serve_command(&program, &argv, PORT);
            let envs: Vec<(String, Option<String>)> = cmd
                .get_envs()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.map(|v| v.to_string_lossy().into_owned()),
                    )
                })
                .collect();
            assert!(
                envs.contains(&(PORT_ENV.to_string(), Some(PORT.to_string()))),
                "{kind:?} must carry {PORT_ENV}: {envs:?}"
            );
        }
    }

    #[test]
    fn the_deadline_applies_to_the_rust_runtime_only() {
        assert_eq!(run_deadline_ms(RuntimeKind::Rust), Some(RUN_SAFETY_MS));
        assert_eq!(run_deadline_ms(RuntimeKind::Sys), None);
        // The safety bound is the deadline minus a positive margin: pin it by behaviour,
        // not by comparing the two constants (which clippy reads as a constant assertion).
        assert_eq!(
            clamp_load_duration(Duration::from_secs(1), 0.0, run_deadline_ms(RuntimeKind::Rust))
                .expect("fits")
                .duration,
            Duration::from_secs(1),
            "a fresh boot must leave the full window inside the safety bound"
        );
    }

    #[test]
    fn tail_truncates_and_keeps_the_end() {
        assert_eq!(tail("  hello  ", 10), "hello");
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(tail("abc", 3), "abc");
        // Multi-byte safety: never split a char.
        assert_eq!(tail("aé€x", 2), "€x");
    }

    #[test]
    fn deps_failure_detection_matches_network_and_git_signs_not_compile_errors() {
        assert!(looks_like_deps_failure(
            "fatal: unable to access 'https://github.com/ninenines/cowlib/': Could not resolve host: github.com"
        ));
        assert!(looks_like_deps_failure("curl: (7) Failed to fetch repo"));
        assert!(looks_like_deps_failure(
            "error: RPC failed; curl 56 GnuTLS recv error"
        ));
        assert!(looks_like_deps_failure(
            "fatal: the remote end hung up unexpectedly"
        ));
        assert!(looks_like_deps_failure(
            "error: unexpected disconnect while reading sideband packet"
        ));
        assert!(looks_like_deps_failure(
            "gnutls_handshake() failed: Connection timed out"
        ));
        assert!(looks_like_deps_failure("fatal: repository not found"));
        // A genuine compile error never matches.
        assert!(!looks_like_deps_failure(
            "src/hello_world_handler.erl:12: syntax error before: '->'"
        ));
        assert!(!looks_like_deps_failure(
            "make: *** [erlang.mk:4919: deps] Error 2"
        ));
    }

    #[test]
    fn a_setup_error_keeps_its_classification_and_partial_measurements() {
        let skip = SetupError::skip("deps-unavailable").with_build_ms(12.0);
        assert!(skip.skipped);
        assert_eq!(skip.reason, "deps-unavailable");
        assert_eq!(skip.build_ms, Some(12.0));

        let fail = SetupError::fail("build-failed: x")
            .with_build_ms(34.0)
            .with_compile_ms(56.0)
            .with_notes(vec!["note: y".to_string()]);
        assert!(!fail.skipped);
        assert_eq!(fail.compile_ms, Some(56.0));
        assert_eq!(fail.notes, vec!["note: y".to_string()]);
    }
}
