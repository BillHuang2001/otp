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

//! **`min_heap_size` sizes a new process's initial nursery** (contracts A.6/A.8).
//!
//! The C's spawn-time heap rule (`erts/emulator/beam/erl_process.c:12586-12592`) is
//!
//! ```c
//! sz = heap_need < p->min_heap_size ? p->min_heap_size : erts_next_heap_size(heap_need, 0);
//! p->heap_sz = sz;
//! ```
//!
//! and `HEAP_SIZE(p) == p->heap_sz` is what `erlang:process_info(P, heap_size)` reports
//! (`erl_bif_info.c:936`). This runtime models no `heap_need` at spawn (the child's arguments
//! are copied onto its heap afterwards and the space grows on demand), so the rule it
//! implements is the first branch: a fresh process is born with exactly its **`min_heap_size`**
//! — [`beam_gc::HeapPolicy::initial_heap_words`] — and both collector constructors install that
//! as the initial nursery.
//!
//! Two consequences are pinned here, because both are observable through `process_info/2`:
//!
//! 1. `heap_size == min_heap_size` for a **freshly spawned** process, and (the C-shaped
//!    `total_heap_size = heap_size + old_heap_size`, `erl_bif_info.c:1023-1030`) also
//!    `total_heap_size == heap_size`, since the old heap is created by a *collection*
//!    (`erl_gc.c:1470-1478`), not at spawn.
//! 2. `spawn_opt`'s `{min_heap_size, N}` is **floored against the global policy** — the C's
//!    `H_MIN_SIZE` (`erl_process.c:12321-12360`) — so a request below `+hms` is raised to it
//!    (`+hms 4000` + `{min_heap_size, 100}` ⇒ 4185) while a request above it wins
//!    (`{min_heap_size, 8000}` ⇒ 10958), and every stored value is a fixed point of the growth
//!    table (`erts_next_heap_size(x, 0)`).
//!
//! # The probe, and why it reads a **parked** child
//!
//! Every case reads a child that was just spawned and then parks in `receive stop -> ok end`.
//! A process that *reports its own* figures would have allocated on its way to the report and
//! its nursery would have grown (a `test_heap` admission collects and resizes), so the reading
//! process is always the parent and the subject is always a child that has been neither
//! stepped nor asked to build anything.
//!
//! # Measured oracle answers (OTP 29.1.1 / erts-17.1)
//!
//! `P = spawn_opt(fun() -> receive stop -> ok end end, [Opts]),
//!  io:format("~p~n", [process_info(P, [heap_size, total_heap_size, min_heap_size])])`:
//!
//! ```text
//! global                        {heap_size,total_heap_size,min_heap_size}
//! (default)                     233 / 233 / 233
//! +hms 100                      142 / 142 / 142
//! +hms 234                      376 / 376 / 376
//! +hms 1000                    1598 / 1598 / 1598
//! +hms 4000                    4185 / 4185 / 4185
//! +hms 8000                   10958 / 10958 / 10958
//! +hms 20000                  28690 / 28690 / 28690
//! {min_heap_size,1000} (default global)   1598
//! {min_heap_size,100}  (default global)    233   (floored at H_DEFAULT_SIZE)
//! {min_heap_size,8000} (default global)  10958
//! {min_heap_size,100}  under +hms 4000    4185   (floored at the global policy)
//! {min_heap_size,100}  under +hms 100      142   (floored at the *smaller* global policy)
//! {min_heap_size,2000},{max_heap_size,500}          → badarg
//! {max_heap_size,500} under +hms 4000               → badarg (500 < the 4185 floor)
//! {max_heap_size,500} under the default global      → ok
//! ```
//!
//! # Layers
//!
//! 1. **The rule** — `beam_gc`'s `initial_heap_words`/`spawn_policy` arithmetic, pinned against
//!    the growth table with no runtime involved ([`the_initial_heap_is_min_heap_words_when_set_else_the_young_seed`],
//!    [`spawn_opt_is_floored_against_the_global_policy`]).
//! 2. **The runtime** — a `Runtime` built from [`HeapOptions`] spawns a child whose collector
//!    reports exactly those figures ([`a_fresh_child_is_born_at_min_heap_size`]). No external
//!    tool is needed for layers 1–4.
//!
//!    A `spawn_opt` **effect** cannot be asserted at this layer: the runtime's spawn-with-a-policy
//!    entry point (`Runtime::spawn_process_with_policy`) is crate-private, so the only public
//!    in-process spawn is the plain one. The rule itself is asserted directly
//!    ([`spawn_opt_is_floored_against_the_global_policy`]) and its effect end to end through the
//!    CLI, which is where `spawn_opt/2` lives.
//! 3. **The CLI** — the executable's own routes report the same figures, including the
//!    `spawn_opt` and `badarg` rows ([`the_cli_route_reports_the_min_heap_size`],
//!    [`spawn_opt_through_the_cli_reports_the_floored_minimum`],
//!    [`a_max_heap_size_below_the_floored_minimum_is_badarg`]).
//! 4. **The oracle cross-check** — the same probe under the system `erl` and under this
//!    runtime, compared byte for byte ([`the_fresh_heap_figures_match_the_live_oracle`],
//!    [`the_spawn_opt_figures_match_the_live_oracle`]).
//!
//! # Skip policy
//!
//! Layers 1–3 run everywhere. Layer 4 needs the toolchain: no `erl` (`$ERTS_ORACLE` or
//! `$PATH`) and no `erlc` (the probe module is compiled at test time, so no stale committed
//! image can drift) is a **loud** skip per test, never a silent pass
//! ([`support::require_oracle`], [`support::skip_loudly`]).
#![deny(unsafe_code)]

mod support;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_gc::{next_heap_size, HeapObservable, HeapPolicy, DEFAULT_MIN_HEAP_WORDS};
use beam_rt::config::HeapOptions;
use beam_rt::{Runtime, RuntimeConfig};

/// The runtime binary under test (cargo builds it for this suite).
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The banner every loud skip prints.
const BANNER: &str = "min_heap_size";

/// The committed mailbox fixture: `mailbox:wait/0` parks on an empty mailbox, so it is the
/// one already-committed module that gives this suite a child that never allocates.
const MAILBOX_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/mailbox.beam"
);

/// The emulator's default `min_heap_size` (`H_DEFAULT_SIZE`, `erl_init.c:812`) — the value
/// the oracle reports for an unconfigured process, and the floor `spawn_opt` is raised to.
const DEFAULT_NURSERY: usize = 233;

/// The `+hms` matrix: the **raw** switch value and the round-tripped `min_heap_size` the
/// oracle reports for a fresh process (the growth table's fixed point).
const HMS_ROWS: [(&str, usize); 6] = [
    ("100", 142),
    ("234", 376),
    ("1000", 1598),
    ("4000", 4185),
    ("8000", 10958),
    ("20000", 28690),
];

/// The `spawn_opt` matrix: `(the global +hms raw value, None for the default, the requested
/// {min_heap_size, N}, the nursery the request rounds into)`.
const SPAWN_OPT_ROWS: [(Option<&str>, usize, usize); 5] = [
    (None, 1000, 1598),
    (None, 100, DEFAULT_NURSERY),
    (None, 8000, 10958),
    (Some("4000"), 100, 4185),
    (Some("100"), 100, 142),
];

// ---------------------------------------------------------------------------------------
// The probe expressions and their answers
// ---------------------------------------------------------------------------------------

/// The `-eval` probe: spawn a child that **parks** in a `receive`, then read the three heap
/// items off it. The reading process is never the subject, so no allocation of the probe's own
/// can move the figure it reports.
fn probe_expr(options: &str) -> String {
    format!(
        "P = spawn_opt(fun() -> receive stop -> ok end end, [{options}]), \
         io:format(\"~p~n\",[process_info(P,[heap_size,total_heap_size,min_heap_size])]), \
         halt(0)."
    )
}

/// The `try … catch` probe for the `spawn_opt` refusal rows: `ok` when the spawn succeeds and
/// the raised reason when it does not.
fn refusal_expr(options: &str) -> String {
    format!(
        "R = try spawn_opt(fun()->ok end,[{options}]) of _ -> ok catch error:B -> B end, \
         io:format(\"~p~n\",[R]), halt(0)."
    )
}

/// The three `process_info` items of a fresh process whose `min_heap_size` is `words`, in the
/// probe's own item order — the oracle's printed shape.
fn figures(words: usize) -> String {
    format!(
        "[{{heap_size,{words}}},{{total_heap_size,{words}}},{{min_heap_size,{words}}}]"
    )
}

/// The `+hms` flags for a raw switch value (`[]` for the default global).
fn hms_flags<'a>(raw: Option<&'a str>) -> Vec<&'a str> {
    raw.map_or_else(Vec::new, |raw| vec!["+hms", raw])
}

// ---------------------------------------------------------------------------------------
// Layer 1 — the rule (no runtime, no toolchain)
// ---------------------------------------------------------------------------------------

/// The C's spawn-size rule, as `beam_gc` implements it: **`min_heap_size` when one is set, the
/// young-space seed otherwise**.
///
/// The default policy carries no floor (`min_heap_words == 0`), so its `initial_young_words`
/// (233 — the C `H_DEFAULT_SIZE`, which `HeapOptions::default` *does* install as the floor
/// the runtime's processes are born with) is what a runtime-spawned process gets.
#[test]
fn the_initial_heap_is_min_heap_words_when_set_else_the_young_seed() {
    // The seed is the C's default minimum, pinned as a literal as well.
    assert_eq!(DEFAULT_NURSERY, 233);
    assert_eq!(DEFAULT_MIN_HEAP_WORDS, DEFAULT_NURSERY);
    let growth_default = HeapPolicy::default();
    assert_eq!(growth_default.min_heap_words, 0, "the growth policy has no floor");
    assert_eq!(growth_default.initial_young_words, DEFAULT_NURSERY);
    assert_eq!(growth_default.initial_heap_words(), DEFAULT_NURSERY);

    // A policy with a floor ignores the seed …
    assert_eq!(
        HeapPolicy::from_min_max(4185, 0).initial_heap_words(),
        4185,
        "a set min_heap_words is the nursery"
    );
    // … and one without a floor follows it, whatever it is.
    assert_eq!(
        HeapPolicy {
            initial_young_words: 1000,
            ..HeapPolicy::default()
        }
        .initial_heap_words(),
        1000
    );

    // The emulator's own policy — what a plain `spawn` child is seeded with — floors at 233,
    // and `+hms N` moves that floor through the growth table.
    let emulator = HeapOptions::default().policy().clone();
    assert_eq!(emulator.min_heap_words, DEFAULT_NURSERY);
    assert_eq!(emulator.initial_heap_words(), DEFAULT_NURSERY);
    for (raw, expected) in HMS_ROWS {
        let mut options = HeapOptions::new();
        options.set_min_heap_words(raw.parse().expect("a numeric switch value"));
        let policy = options.policy();
        assert_eq!(
            next_heap_size(raw.parse().expect("a numeric switch value"), 0),
            expected,
            "+hms {raw} rounds through the growth table to {expected}"
        );
        assert_eq!(policy.min_heap_words, expected);
        assert_eq!(policy.initial_heap_words(), expected);
    }
    // The row that proves the global policy really *replaces* the default rather than flooring
    // at it: `+hms 100` is 142, i.e. **below** the unconfigured 233.
    assert!(
        HMS_ROWS[0].1 < DEFAULT_NURSERY,
        "`+hms 100` must report a heap smaller than the default"
    );
}

/// `spawn_opt`'s `{min_heap_size, N}` is floored against the **global** policy (the C
/// `H_MIN_SIZE`) and rounded through the growth table otherwise — the rule the runtime's
/// `spawn_opt` path applies through [`HeapPolicy::spawn_policy`].
#[test]
fn spawn_opt_is_floored_against_the_global_policy() {
    let emulator = HeapOptions::default().policy().clone();
    for (global, requested, expected) in SPAWN_OPT_ROWS {
        let base = match global {
            Some(raw) => {
                let mut options = HeapOptions::new();
                options.set_min_heap_words(raw.parse().expect("a numeric switch value"));
                options.policy().clone()
            }
            None => emulator.clone(),
        };
        let child = base.spawn_policy(requested, 0, None);
        assert_eq!(
            child.initial_heap_words(),
            expected,
            "global {global:?}, request {requested}: the child's nursery"
        );
        assert_eq!(child.min_heap_words, expected, "and its min_heap_size");
        // The stored minimum is a fixed point of the growth table (the C's
        // `ASSERT(p->min_heap_size == erts_next_heap_size(p->min_heap_size, 0))`).
        assert_eq!(next_heap_size(child.min_heap_words, 0), child.min_heap_words);
    }
    // The boundary of the "larger wins" half: one word above the default floor is rounded up,
    // one word below it is replaced by the floor.
    assert_eq!(
        emulator.spawn_policy(234, 0, None).min_heap_words,
        next_heap_size(234, 0)
    );
    assert_eq!(
        emulator.spawn_policy(232, 0, None).min_heap_words,
        DEFAULT_NURSERY
    );
    // An absent / zero request is the floor itself — the C's `hmin` default.
    assert_eq!(
        emulator.spawn_policy(0, 0, None).min_heap_words,
        DEFAULT_NURSERY
    );
}

// ---------------------------------------------------------------------------------------
// Layer 2 — the runtime's own children (no toolchain)
// ---------------------------------------------------------------------------------------

/// A runtime carrying `raw` as its `+hms` value (`None` for the emulator default), with the
/// committed mailbox fixture loaded.
fn runtime_with_hms(raw: Option<&str>) -> Runtime {
    let mut options = HeapOptions::new();
    if let Some(raw) = raw {
        options.set_min_heap_words(raw.parse().expect("a numeric switch value"));
    }
    let config = RuntimeConfig::new().with_heap_options(options);
    let mut runtime = Runtime::new(config).expect("the runtime is created");
    runtime.load_module(MAILBOX_BEAM).expect("the fixture loads");
    runtime
}

/// The heap figures of the process `pid`, read the way `process_info/2` reads them: the
/// collector's `heap_size`, the C-shaped `total_heap_size` (`heap_size + old_heap_size`) and
/// the policy the process was born with.
struct Figures {
    /// `process_info(P, heap_size)`.
    heap_size: usize,
    /// `process_info(P, total_heap_size)` — `heap_size_words() + old_heap_size_words()`.
    total_heap_size: usize,
    /// `process_info(P, heap_size)` of the old generation (only a collection creates one).
    old_heap_size: usize,
    /// `process_info(P, min_heap_size)` — the process's own policy.
    min_heap_size: usize,
}

impl Figures {
    /// The three `process_info` items of a **freshly spawned, never-stepped** child.
    fn of_fresh_child(runtime: &mut Runtime) -> Figures {
        let pid = runtime
            .spawn_process("mailbox", "wait", Vec::new())
            .expect("the parked fixture is spawned");
        let process = runtime
            .processes()
            .get(pid)
            .expect("a just-spawned process stays in the process table");
        let collector = process.collector();
        Figures {
            heap_size: collector.heap_size_words(),
            total_heap_size: collector.heap_size_words() + collector.old_heap_size_words(),
            old_heap_size: collector.old_heap_size_words(),
            min_heap_size: collector.policy().min_heap_words,
        }
    }

    /// Asserts all three items are `words`, and that no old heap exists yet.
    fn assert_is(&self, words: usize, what: &str) {
        assert_eq!(self.min_heap_size, words, "{what}: min_heap_size");
        assert_eq!(self.heap_size, words, "{what}: heap_size");
        assert_eq!(self.old_heap_size, 0, "{what}: no collection has run");
        assert_eq!(
            self.total_heap_size, self.heap_size,
            "{what}: a fresh process's total_heap_size is its heap_size"
        );
    }
}

/// A child a runtime spawns is **born at its `min_heap_size`** — the rule reaches the live
/// process table, not just the policy arithmetic.
///
/// The child is never stepped (it is spawned and read in the same breath), so nothing but the
/// spawn rule can have decided the figure.
#[test]
fn a_fresh_child_is_born_at_min_heap_size() {
    // The emulator default: 233 words, the C `H_DEFAULT_SIZE`.
    let mut runtime = runtime_with_hms(None);
    assert_eq!(
        runtime.config().heap_options().policy().initial_heap_words(),
        DEFAULT_NURSERY
    );
    Figures::of_fresh_child(&mut runtime).assert_is(DEFAULT_NURSERY, "the default runtime");

    for (raw, expected) in HMS_ROWS {
        let mut runtime = runtime_with_hms(Some(raw));
        assert_eq!(
            runtime.config().heap_options().policy().min_heap_words,
            expected,
            "+hms {raw} is rounded before it is stored"
        );
        Figures::of_fresh_child(&mut runtime).assert_is(expected, &format!("+hms {raw}"));
    }
}

// ---------------------------------------------------------------------------------------
// Layer 3 — the CLI routes (no toolchain)
// ---------------------------------------------------------------------------------------

/// Runs the built `beam_rt` with `flags` + `-noshell -eval expr` and returns its trimmed
/// stdout, asserting a clean exit.
fn rust_answer(flags: &[&str], expr: &str) -> String {
    let mut args: Vec<&str> = flags.to_vec();
    args.extend(["-noshell", "-eval", expr]);
    let output = support::run_bounded(Path::new(BINARY), &args)
        .unwrap_or_else(|| panic!("beam_rt could not answer `{expr}` within the probe bound"));
    assert_eq!(
        output.status.code(),
        Some(0),
        "beam_rt {flags:?} -eval {expr} must exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// `beam_rt run <beam> <module>:<function>/0` — the **scheduled** route (a compiled fixture
/// function, driven through the full scheduler), whose result the CLI prints.
fn rust_run_route(flags: &[&str], beam: &Path, spec: &str) -> String {
    let beam = beam.to_str().expect("a UTF-8 fixture path");
    let mut args: Vec<&str> = flags.to_vec();
    args.extend(["run", beam, spec]);
    let output = support::run_bounded(Path::new(BINARY), &args)
        .unwrap_or_else(|| panic!("beam_rt could not run {spec} within the probe bound"));
    assert_eq!(
        output.status.code(),
        Some(0),
        "beam_rt {flags:?} run {spec} must exit 0; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// The CLI route reports the same figures the rule predicts, for the default and every `+hms`
/// row (the `-eval` route, whose `spawn_opt(…, [])` is `spawn/1`).
#[test]
fn the_cli_route_reports_the_min_heap_size() {
    let mut cases: Vec<(Option<&str>, usize)> = vec![(None, DEFAULT_NURSERY)];
    cases.extend(HMS_ROWS.map(|(raw, n)| (Some(raw), n)));
    for (raw, expected) in cases {
        let got = rust_answer(&hms_flags(raw), &probe_expr(""));
        assert_eq!(
            got,
            figures(expected),
            "+hms {raw:?}: a fresh child's three heap items"
        );
    }
}

/// `spawn_opt`'s `{min_heap_size, N}` through the CLI: larger wins, smaller is floored by the
/// **global** policy (and by the default floor when no `+hms` was given).
#[test]
fn spawn_opt_through_the_cli_reports_the_floored_minimum() {
    for (global, requested, expected) in SPAWN_OPT_ROWS {
        let got = rust_answer(&hms_flags(global), &probe_expr(&format!("{{min_heap_size,{requested}}}")));
        assert_eq!(
            got,
            figures(expected),
            "global +hms {global:?}, request {requested}: the child's three heap items"
        );
    }
}

/// A `{max_heap_size, …}` below the child's **floored** `min_heap_size` is `badarg`
/// (`spawn_opt`'s cross-field rule `max_heap_size < min_heap_size ⇒ -1`), while the same
/// setting spawns under a global policy whose floor it clears — so the two refusals below are
/// about the floor, never about `max_heap_size` itself.
#[test]
fn a_max_heap_size_below_the_floored_minimum_is_badarg() {
    // {min_heap_size, 2000} rounds to 2167, so a 500-word bound is below the floor.
    assert_eq!(
        rust_answer(&[], &refusal_expr("{min_heap_size,2000},{max_heap_size,500}")),
        "badarg"
    );
    // Under the default global, the 500-word bound clears the 233-word floor.
    assert_eq!(rust_answer(&[], &refusal_expr("{max_heap_size,500}")), "ok");
    // Under `+hms 4000` the floor is 4185, so the very same setting is refused.
    assert_eq!(
        rust_answer(&["+hms", "4000"], &refusal_expr("{max_heap_size,500}")),
        "badarg"
    );
}

// ---------------------------------------------------------------------------------------
// Layer 4 — the oracle cross-check
// ---------------------------------------------------------------------------------------

/// The probe module, compiled at test time by the system `erlc`.
///
/// Compiling it here (rather than committing a `.beam`) keeps the probe and the assertion in
/// one file, so no stale image can drift from the source. `parked/0` is the child that is
/// spawned and read without ever being stepped.
const FIXTURE_SOURCE: &str = r#"%% The min_heap_size probe (compiled by the test suite with the system erlc).
-module(minheap).
-export([probe/0, probe_opt/1, badarg/1]).

%% The subject: a child that parks immediately and allocates nothing of its own.
parked() ->
    receive
        stop -> ok
    end.

probe() ->
    P = spawn(fun parked/0),
    process_info(P, [heap_size, total_heap_size, min_heap_size]).

probe_opt(Options) ->
    P = spawn_opt(fun parked/0, Options),
    process_info(P, [heap_size, total_heap_size, min_heap_size]).

badarg(Options) ->
    try spawn_opt(fun() -> ok end, Options) of
        _ -> ok
    catch
        error:Reason -> Reason
    end.
"#;

/// A unique scratch directory, removed on drop.
///
/// `tests/fixtures/` must never be handed to the oracle as a single `-pa` (it holds a
/// `lists.beam` that would shadow the real stdlib module), so the probe is compiled into a
/// directory of its own — unique per call (label + pid + a counter) so concurrent tests cannot
/// collide.
struct Scratch {
    /// The directory `erlc` writes the probe into.
    dir: PathBuf,
}

impl Scratch {
    /// Creates the directory.
    fn new(label: &str) -> Scratch {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "beam_rt_min_heap_{label}_{}_{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("the scratch directory is created");
        Scratch { dir }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Best effort: a failure here must not fail the test.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The system `erlc`, or `None` after a loud skip naming `banner`.
fn require_erlc(banner: &str) -> Option<PathBuf> {
    match support::find_on_path("erlc") {
        Some(erlc) => Some(erlc),
        None => {
            support::skip_loudly(banner, "no `erlc` on $PATH (the probe module cannot be compiled)");
            None
        }
    }
}

/// Writes [`FIXTURE_SOURCE`] into `scratch` and compiles it, answering the `.beam` path.
fn compile_probe(erlc: &Path, scratch: &Scratch) -> PathBuf {
    let source = scratch.dir.join("minheap.erl");
    std::fs::write(&source, FIXTURE_SOURCE).expect("the probe source is written");
    let out_dir = scratch.dir.to_str().expect("a UTF-8 scratch path");
    let source_arg = source.to_str().expect("a UTF-8 source path");
    let output = support::run_bounded(erlc, &["-o", out_dir, source_arg])
        .unwrap_or_else(|| panic!("`erlc` could not compile the probe within the probe bound"));
    assert!(
        output.status.success(),
        "`erlc` failed with {:?}:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let beam = scratch.dir.join("minheap.beam");
    assert!(beam.is_file(), "`erlc` produced no {}", beam.display());
    beam
}

/// The oracle's answer to `expr` under `flags`, with `code_dir` on the code path, or `None`
/// after a loud skip.
///
/// With no `flags` this is the shared [`support::oracle_eval`] exactly (so the two sides cannot
/// drift); the flags form adds the emulator switches under test in front of `-noshell`. The
/// command is `erl <flags> -noshell [-pa code_dir] -eval expr -s init stop`, so the node
/// terminates even when the expression raises; `support::run_bounded` kills and reaps it if it
/// outlives the probe bound.
fn oracle_answer(
    oracle: &Path,
    flags: &[&str],
    code_dir: Option<&Scratch>,
    expr: &str,
) -> Option<String> {
    let dir = code_dir.map(|scratch| scratch.dir.as_path());
    if flags.is_empty() {
        return support::oracle_eval(oracle, dir, expr);
    }
    let mut args: Vec<&str> = flags.to_vec();
    args.push("-noshell");
    if let Some(dir) = dir {
        args.push("-pa");
        args.push(dir.to_str().expect("a UTF-8 scratch path"));
    }
    args.extend(["-eval", expr, "-s", "init", "stop"]);
    let output = support::run_bounded(oracle, &args)?;
    assert!(
        output.status.success(),
        "the oracle exited with {:?} for {expr}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `io:format("~p~n", [minheap:probe()])` — the compiled probe, the same call on both runtimes.
const PROBE_CALL: &str = "io:format(\"~p~n\",[minheap:probe()]), halt(0).";

/// The compiled `probe_opt/1` call for one option list.
fn probe_opt_call(options: &str) -> String {
    format!("io:format(\"~p~n\",[minheap:probe_opt([{options}])]), halt(0).")
}

/// The compiled `badarg/1` call for one option list.
fn refusal_call(options: &str) -> String {
    format!("io:format(\"~p~n\",[minheap:badarg([{options}])]), halt(0).")
}

/// **The fresh-process figures agree with the live oracle**, for the default global and the
/// three `+hms` rows that matter most (`100` below the default, `4000` and `8000` above it).
///
/// The two runtimes are asked the **same** compiled module function: this runtime through its
/// scheduled route (`run <beam> minheap:probe/0`, the analogue of the `run` action's contract)
/// and the oracle through `-eval`, as `max_heap_enforce.rs` does — the only difference between
/// the runs is which runtime executes the module.
#[test]
fn the_fresh_heap_figures_match_the_live_oracle() {
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    let Some(erlc) = require_erlc(BANNER) else {
        return;
    };
    let scratch = Scratch::new("oracle_fresh");
    let beam = compile_probe(&erlc, &scratch);

    let mut rows: Vec<(Option<&str>, usize)> = vec![(None, DEFAULT_NURSERY)];
    rows.extend(
        HMS_ROWS
            .iter()
            .filter(|(raw, _)| ["100", "4000", "8000"].contains(raw))
            .map(|(raw, n)| (Some(*raw), *n)),
    );
    for (raw, expected) in rows {
        let expected = figures(expected);
        let Some(theirs) = oracle_answer(&oracle, &hms_flags(raw), Some(&scratch), PROBE_CALL)
        else {
            support::skip_loudly(BANNER, "the oracle could not answer `minheap:probe/0`");
            return;
        };
        assert_eq!(
            theirs, expected,
            "+hms {raw:?}: the oracle's own answer changed — re-measure the table"
        );
        let ours = rust_run_route(&hms_flags(raw), &beam, "minheap:probe/0");
        assert_eq!(
            ours, theirs,
            "+hms {raw:?}: this runtime and the oracle must report the same heap items"
        );
    }
}

/// **The `spawn_opt` rows agree with the live oracle**, on both the accepted and the refused
/// side: the floored `min_heap_size` figures of the five `SPAWN_OPT_ROWS`, plus the two
/// `badarg` rows and their `ok` control.
///
/// Both sides run the compiled module through `-eval` here, because the `run` action's argument
/// tokens (`Int`/`Atom`/`Str`) cannot spell a `spawn_opt` option list.
#[test]
fn the_spawn_opt_figures_match_the_live_oracle() {
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };
    let Some(erlc) = require_erlc(BANNER) else {
        return;
    };
    let scratch = Scratch::new("oracle_spawn_opt");
    let _beam = compile_probe(&erlc, &scratch);

    for (global, requested, expected) in SPAWN_OPT_ROWS {
        let call = probe_opt_call(&format!("{{min_heap_size,{requested}}}"));
        let expected = figures(expected);
        let Some(theirs) = oracle_answer(&oracle, &hms_flags(global), Some(&scratch), &call) else {
            support::skip_loudly(BANNER, "the oracle could not answer `minheap:probe_opt/1`");
            return;
        };
        assert_eq!(
            theirs, expected,
            "global +hms {global:?}, request {requested}: the oracle's own answer"
        );
        let ours = rust_answer(&with_pa(&hms_flags(global), &scratch), &call);
        assert_eq!(
            ours, theirs,
            "global +hms {global:?}, request {requested}: the two runtimes must agree"
        );
    }

    // The refusal rows, and the control that proves they are about the floor.
    let refusals: [(&[&str], &str, &str); 3] = [
        (&[], "{min_heap_size,2000},{max_heap_size,500}", "badarg"),
        (&["+hms", "4000"], "{max_heap_size,500}", "badarg"),
        (&[], "{max_heap_size,500}", "ok"),
    ];
    for (flags, options, expected) in refusals {
        let call = refusal_call(options);
        let Some(theirs) = oracle_answer(&oracle, flags, Some(&scratch), &call) else {
            support::skip_loudly(BANNER, "the oracle could not answer `minheap:badarg/1`");
            return;
        };
        assert_eq!(theirs, expected, "{flags:?} [{options}]: the oracle's own answer");
        let ours = rust_answer(&with_pa(flags, &scratch), &call);
        assert_eq!(ours, theirs, "{flags:?} [{options}]: the two runtimes must agree");
    }
}

/// `flags` plus `-pa <scratch>`, so the runtime's `-eval` route can call the probe module.
fn with_pa<'a>(flags: &[&'a str], scratch: &'a Scratch) -> Vec<&'a str> {
    let mut args: Vec<&str> = flags.to_vec();
    args.push("-pa");
    args.push(scratch.dir.to_str().expect("a UTF-8 scratch path"));
    args
}
