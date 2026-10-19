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
//! **NIF-resource reclamation** end to end (contract A.3): a resource exposed as an
//! Erlang term really has its type's destructor run when the term dies.
//!
//! `beam_abi`'s resource family mints the term and takes one reference on the object for
//! it (`enif_make_resource`), and its release seam (`release_resource_terms`) drops that
//! reference for every term a **caller-owned** reachability predicate calls unreachable —
//! a wrong "unreachable" is a use-after-free, so only the host can make the decision.
//! [`beam_rt`]'s `context::gc::resources` is that caller, driven from the runner's
//! `Yield` park arm
//! ([`src/runner/run.rs`](../../../src/runner/run.rs), beside the off-heap refc-binary
//! prune and under the same `parked_published` gate). This suite is the pin: the fixture's
//! destructor counts its own runs, so the observable is a plain integer read back through
//! the same NIF.
//!
//! # The counts (measured against the system oracle, OTP 29.1.1 / erts-17.1)
//!
//! | step | oracle | pre-fix `beam_rt` | now |
//! |---|---|---|---|
//! | fresh library | `0` | `0` | `0` |
//! | term dropped, forced GC | `1` | **`0`** | `1` |
//! | a second forced GC | still `1` (never twice) | `0` | `1` |
//! | term still referenced (variable / tuple / list) | `0` | `0` | `0` |
//! | term still referenced (sent in a message) | `0` | `0` | `0` |
//! | never exported as a term (`alloc` + `release`) | `1` | `1` | `1` |
//!
//! The pre-fix column is the measured divergence this suite exists to catch: the
//! reference the term took was never dropped, so the destructor never ran (a bounded
//! leak, never a wrong result). Assertion `(a)` below therefore **fails** on the pre-fix
//! runtime with `dtor_count == 0`.
//!
//! # The cadence caveat, and how this suite handles it
//!
//! The prune is **park**-driven, not collection-driven: it runs at the slice boundary a
//! pre-empted process parks on (`parked_published`), and an *exempt* park — the `receive`
//! `Wait` window — publishes no roots and is refused outright. A target that returns
//! immediately never parks at all, so reading `dtor_count()` right after dropping the
//! term would pass **for the wrong reason** on a runtime that never prunes.
//!
//! Every dropping/keeping function below therefore runs
//! [`park/2`](CALLER_ERL) — the allocating tail loop of
//! `tests/fixtures/preempt.erl`'s `alloc_loop/1` shape — after the term's fate is sealed.
//! That loop's heap-admission points charge the reduction budget, so it is *pre-empted*
//! (yielding, so parking, so pruning) on every tier; `PARK_ITERATIONS` is far above one
//! slice's 4 000-reduction budget, so a run crosses many slice boundaries. For assertion
//! `(a)` the count is read **after** those parks, and it moves `0` → `1` across them —
//! which is what makes the pin real rather than accidental.
//!
//! # The second site: the `-eval` statement boundary
//!
//! The `-eval` action process is **never enqueued**, so it never parks and the park arm above can
//! never serve it; without a second site an `-eval`-only run would retain every resource term it
//! ever minted. [`the_eval_statement_boundary_keeps_the_resource_table_bounded`] is that site's
//! pin and [`without_the_resource_prune_the_eval_route_is_linear`] its A/B control; both drive the
//! `-eval` route **in process** (`Runtime::eval`), because the figures that make the claim
//! falsifiable (`Runtime::resource_prune_stats()`) are runtime state rather than observable
//! output.//!
//! # Everything here is real
//!
//! The C fixture (`tests/fixtures/nif_resource_gc.c`) is compiled at test time with the
//! host `cc` against the host `erl_nif.h`; the two Erlang modules are compiled with the
//! host `erlc`; and the whole thing is driven through the **built binary**
//! ([`CARGO_BIN_EXE_beam_rt`]) exactly as a user would. The same `.so` and the same
//! `.beam`s then run under the system `erl`, so the counts are compared against the
//! oracle, not against a hard-coded table.
//!
//! # Why the fixture needs an external caller
//!
//! A NIF replaces the Erlang stub of the same `{module, function, arity}` when the
//! runtime **dispatches a call**. A *local* call inside the NIF's own module is resolved
//! in-machine and never reaches the runtime's dispatch, so `gc_caller` calls
//! `nif_resource_gc:*` from *another* module (an external `call_ext`).
//!
//! # Skip policy
//!
//! Four host facilities are needed (the `erl` oracle, `erlc`, a C compiler and the host
//! `erl_nif.h`). When one is missing the test prints a **loud** banner and returns — it
//! never silently passes and never panics.
mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use support::{find_on_path, require_oracle, run_bounded};

/// The committed C fixture (see its header for what it proves).
const NIF_C: &str = include_str!("fixtures/nif_resource_gc.c");

/// The NIF module: `init/1` loads the library, the three probes are the (replaced) stubs.
const NIF_ERL: &str = r#"%% Generated by `beam_rt/tests/nif_resource_gc.rs` (never committed).
-module(nif_resource_gc).
-export([init/1, probe/0, probe_unexported/0, dtor_count/0, is_resource/1]).
init(Path) -> erlang:load_nif(Path, 0).
probe() -> erlang:nif_error(not_loaded).
probe_unexported() -> erlang:nif_error(not_loaded).
dtor_count() -> erlang:nif_error(not_loaded).
is_resource(X) -> erlang:nif_error(not_loaded).
"#;

/// The caller: every probe reaches the NIF through an **external** call, and every
/// function that seals a term's fate runs the allocating [`park/2`](Self) loop before the
/// count is read (see the module docs' cadence section).
///
/// `hold/1` is the liveness anchor: a plain local call whose argument the compiler must
/// keep alive across the park loop, so a "kept" term really is in the parked root set.
/// It uses no BIF, so the shape of the keep tests cannot be perturbed by whether some
/// predicate happens to be registered.
const CALLER_ERL: &str = r#"%% Generated by `beam_rt/tests/nif_resource_gc.rs` (never committed).
-module(gc_caller).
-export([fresh/1, drop/1, drop_twice/1, keep_var/1, keep_tuple/1, keep_list/1,
         keep_msg/1, unexported/1, keep_timer/1, drop_timer/1, start_via_pass/0,
         drive_child/1, starter/1, hold_in_child/1]).

%% (0) A freshly loaded library has run no destructor.
fresh(Path) ->
    ok = nif_resource_gc:init(Path),
    nif_resource_gc:dtor_count().

%% (a) The term is minted and dropped: the destructor runs at the next park.
drop(Path) ->
    ok = nif_resource_gc:init(Path),
    _ = nif_resource_gc:probe(),
    _ = park(100000, 0),
    nif_resource_gc:dtor_count().

%% (b) ... and a SECOND forced collection does not run it again (no double free).
drop_twice(Path) ->
    ok = nif_resource_gc:init(Path),
    _ = nif_resource_gc:probe(),
    _ = park(100000, 0),
    First = nif_resource_gc:dtor_count(),
    _ = erlang:garbage_collect(),
    _ = park(100000, 0),
    {First, nif_resource_gc:dtor_count()}.

%% (c1) Held in a variable across the parks: it must SURVIVE (count unchanged).
keep_var(Path) ->
    ok = nif_resource_gc:init(Path),
    Res = nif_resource_gc:probe(),
    _ = park(100000, 0),
    _ = hold(Res),
    nif_resource_gc:dtor_count().

%% (c2) Held inside a tuple: the same survival through a boxed container.
keep_tuple(Path) ->
    ok = nif_resource_gc:init(Path),
    Held = {nif_resource_gc:probe()},
    _ = park(100000, 0),
    _ = hold(Held),
    nif_resource_gc:dtor_count().

%% (c3) Held inside a list -- `[Res|[]]`, the cons-cell shape a boxed-object-only
%% reachability view cannot see.
keep_list(Path) ->
    ok = nif_resource_gc:init(Path),
    Held = [nif_resource_gc:probe()],
    _ = park(100000, 0),
    _ = hold(Held),
    nif_resource_gc:dtor_count().

%% (c4) Sent in a message: the payload is COPIED onto this process's own heap by the
%% send path, so the reachable word is a copy at a different address -- the shape that
%% separates "the term's address is a root" from "the term's identity is reachable".
keep_msg(Path) ->
    ok = nif_resource_gc:init(Path),
    self() ! nif_resource_gc:probe(),
    Got = receive X -> X end,
    _ = park(100000, 0),
    _ = hold(Got),
    nif_resource_gc:dtor_count().

%% (d) Never exported as a term: the allocator's reference is the only one, so the
%% release destroys the object with no GC and no prune involved.
unexported(Path) ->
    ok = nif_resource_gc:init(Path),
    Before = nif_resource_gc:dtor_count(),
    _ = nif_resource_gc:probe_unexported(),
    _ = park(100000, 0),
    {Before, nif_resource_gc:dtor_count()}.

%% (e) Named ONLY by a pending `erlang:send_after` delivery: the scheduling helper returns `ok`,
%% so after it returns no register, stack slot or dictionary entry of this process names the
%% term -- the runtime's timer store is its only owner. The parks then run the park-arm prune,
%% and only the timer family can keep the term alive there. `Before` is the baseline count.
keep_timer(Path) ->
    ok = nif_resource_gc:init(Path),
    Before = nif_resource_gc:dtor_count(),
    ok = schedule_probe(500),
    _ = park(100000, 0),
    _ = erlang:garbage_collect(),
    Got = receive X -> X after 5000 -> timeout end,
    {Before, nif_resource_gc:dtor_count(), nif_resource_gc:is_resource(Got)}.

%% The mint AND the schedule live in this helper so the caller holds neither: it returns `ok`.
%% The delay keeps delivery well past the first park boundary (microseconds after the schedule),
%% which is the window the park-arm prune runs in.
schedule_probe(DelayMs) ->
    _ = erlang:send_after(DelayMs, self(), nif_resource_gc:probe()),
    ok.

%% (f) The negative control for the timer family: the SAME mint into a timer store, then the
%% timer is CANCELLED, so nothing names the term and the destructor MUST run. A root family that
%% turned the prune into an over-retainer would answer 0 here.
drop_timer(Path) ->
    ok = nif_resource_gc:init(Path),
    Ref = erlang:send_after(60000, self(), nif_resource_gc:probe()),
    _ = erlang:cancel_timer(Ref),
    _ = park(100000, 0),
    _ = erlang:garbage_collect(),
    nif_resource_gc:dtor_count().

%% (g) The engine-arguments family: this process (entered from `-eval` as a nested run) spawns
%% `starter/1`, which runs in a SCHEDULER SLICE; the spawn it makes there is the pass route, whose
%% arguments the runtime copies onto the pointer-stable arena (`arena_args`), so the child's first
%% root set is named by nothing but its own engine. The receive forces the nested run to pump the
%% scheduler, so `starter/1` really does run before this returns.
start_via_pass() ->
    Parent = self(),
    _ = spawn(gc_caller, starter, [Parent]),
    receive {started, Child} -> Child after 5000 -> timeout end.

%% Runs inside a scheduler slice (`start_via_pass/0`'s child): mint the resource and hand it to a
%% real export spawn, so the child's argument list is the term's only owner.
starter(Parent) ->
    Child = spawn(gc_caller, hold_in_child, [nif_resource_gc:probe()]),
    Parent ! {started, Child},
    ok.

%% The grandchild: it reads its own view of the resource only when asked, so the read happens
%% after the parent's statement boundary (where the resource prune runs).
hold_in_child(Res) ->
    receive
        {from, From} ->
            From ! {nif_resource_gc:dtor_count(), nif_resource_gc:is_resource(Res)}
    end.

%% Asks the grandchild for its view and answers it.
drive_child(Child) ->
    Child ! {from, self()},
    receive {Dtors, IsResource} -> {Dtors, IsResource} after 5000 -> timeout end.

%% The liveness anchor (see the module docs).
hold(X) -> X.

%% The allocating tail loop: one cons cell per iteration, far above one slice's
%% 4000-reduction budget, so the process is pre-empted and parks at slice boundaries.
park(0, Acc) -> Acc;
park(N, Acc) -> park(N - 1, [N | Acc]).
"#;

/// A built fixture: the scratch directory holding the sources and compiled artifacts.
struct Fixture {
    /// The scratch directory.
    dir: PathBuf,
    /// The compiled `gc_caller.beam` (the `run` file argument).
    caller: PathBuf,
}

impl Fixture {
    /// The path handed to `erlang:load_nif/2` — the `.so` **without** its suffix (the
    /// loader appends `.so`).
    fn load_path(&self) -> PathBuf {
        self.dir.join("nif_resource_gc")
    }

    /// Removes the scratch directory (`cc`/`erlc` output is not source).
    fn cleanup(self) {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            // The directory is ours and disposable; a failure is reported, not hidden.
            Err(error) => println!("note: could not remove {}: {error}", self.dir.display()),
        }
    }
}

/// Prints the loud skip banner (never a silent pass).
fn skip(reason: &str) {
    support::skip_loudly("nif_resource_gc", reason);
}

/// The C compiler to use: `$CC` when set, else `cc`.
fn cc() -> String {
    std::env::var("CC").unwrap_or_else(|_| "cc".to_string())
}

/// The C compiler's program path, when it is runnable.
fn cc_program() -> Option<PathBuf> {
    let cc = cc();
    run_bounded(Path::new(&cc), &["--version"]).map(|_| PathBuf::from(cc))
}

/// The host `erl_nif.h` include directory: the one the host `erl` reports, else the
/// `include` sibling of the `erts-*` directory the `erl` on `$PATH` lives under.
fn erl_include_dir(oracle: &Path) -> Option<PathBuf> {
    let queried = run_bounded(
        oracle,
        &[
            "-noshell",
            "-eval",
            "io:format(\"~s\",[code:lib_dir(erts,include)])",
            "-s",
            "init",
            "stop",
        ],
    )
    .map(|output| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string()));
    if let Some(dir) = queried {
        if dir.join("erl_nif.h").is_file() {
            return Some(dir);
        }
    }
    let bin = oracle.canonicalize().ok()?.parent()?.to_path_buf();
    let include = bin.parent()?.join("include");
    include.join("erl_nif.h").is_file().then_some(include)
}

/// Compiles the fixture (the `.so` and both `.beam`s) into a fresh scratch directory, or
/// `None` after a loud skip when a host facility is missing.
fn build_fixture(label: &str) -> Option<Fixture> {
    let oracle = require_oracle("nif_resource_gc")?;
    let Some(include) = erl_include_dir(&oracle) else {
        skip("no `erl_nif.h` include directory could be found");
        return None;
    };
    let Some(cc) = cc_program() else {
        skip(&format!("no C compiler ({} / $CC) available", cc()));
        return None;
    };
    let Some(erlc) = find_on_path("erlc") else {
        skip("no `erlc` on $PATH");
        return None;
    };
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_nif_resource_gc_{label}_{}_{n}",
        std::process::id()
    ));
    let c_src = dir.join("nif_resource_gc.c");
    if let Err(error) = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&c_src, NIF_C)) {
        skip(&format!("cannot prepare {}: {error}", dir.display()));
        return None;
    }
    let mut erl_paths = Vec::new();
    for (stem, source) in [("nif_resource_gc", NIF_ERL), ("gc_caller", CALLER_ERL)] {
        let path = dir.join(format!("{stem}.erl"));
        if let Err(error) = std::fs::write(&path, source) {
            skip(&format!("cannot write {}: {error}", path.display()));
            return None;
        }
        erl_paths.push(path);
    }
    // The shared object: `cc -shared -fPIC -I<include> -o <dir>/nif_resource_gc.so <c>`.
    let linked = Command::new(&cc)
        .arg("-shared")
        .arg("-fPIC")
        .arg(format!("-I{}", include.display()))
        .arg("-o")
        .arg(dir.join("nif_resource_gc.so"))
        .arg(&c_src)
        .stdin(std::process::Stdio::null())
        .output()
        .map(|output| (output.status.success(), output.status, output.stderr));
    match linked {
        Ok((true, _, _)) => {}
        Ok((false, status, stderr)) => {
            skip(&format!(
                "cc failed ({status}):\n{}",
                String::from_utf8_lossy(&stderr)
            ));
            return None;
        }
        Err(error) => {
            skip(&format!("could not run the C compiler: {error}"));
            return None;
        }
    }
    // The Erlang modules: `erlc -o <dir> <dir>/nif_resource_gc.erl <dir>/gc_caller.erl`.
    let compiled = Command::new(&erlc)
        .arg("-o")
        .arg(&dir)
        .args(&erl_paths)
        .stdin(std::process::Stdio::null())
        .output()
        .map(|output| (output.status.success(), output.status, output.stderr));
    match compiled {
        Ok((true, _, _)) => {}
        Ok((false, status, stderr)) => {
            skip(&format!(
                "erlc failed ({status}):\n{}",
                String::from_utf8_lossy(&stderr)
            ));
            return None;
        }
        Err(error) => {
            skip(&format!("could not run `erlc`: {error}"));
            return None;
        }
    }
    let caller = dir.join("gc_caller.beam");
    if !caller.is_file() {
        skip(&format!("erlc produced no {}", caller.display()));
        return None;
    }
    Some(Fixture { dir, caller })
}

/// Runs `gc_caller:<function>/1` under **this** runtime: `-noshell -pa <dir> run
/// <caller.beam> gc_caller:<function>/1 "<load-path>"`, bounded.
///
/// The load path is double-quoted: the `run` argument grammar parses a bare token as an
/// atom and only a quoted token as an Erlang char list, and `erlang:load_nif/2` requires
/// a list or a binary. The scratch path carries no `"` or backslash.
fn invoke_runtime(fixture: &Fixture, function: &str) -> Option<Output> {
    let dir = fixture.dir.display().to_string();
    let caller = fixture.caller.display().to_string();
    let load = format!("\"{}\"", fixture.load_path().display());
    let entry = format!("gc_caller:{function}/1");
    run_bounded(
        Path::new(env!("CARGO_BIN_EXE_beam_rt")),
        &["-noshell", "-pa", &dir, "run", &caller, &entry, &load],
    )
}

/// Runs the identical fixture under the **system** `erl` (the oracle) through the shared
/// bounded probe.
fn invoke_oracle(oracle: &Path, fixture: &Fixture, function: &str) -> Option<String> {
    let expression = format!(
        "io:format(\"~w~n\", [gc_caller:{function}(\"{}\")]), halt(0).",
        fixture.load_path().display()
    );
    support::oracle_eval(oracle, Some(&fixture.dir), &expression)
}

/// The observable of one `gc_caller:<function>/1` call on **both** sides.
struct Observed {
    /// This runtime's printed answer (`~w~n`, trimmed).
    rust: String,
    /// The oracle's printed answer, when it could be measured.
    oracle: Option<String>,
}

/// Runs `function` under this runtime and (when available) under the oracle, asserting
/// this runtime's exit status and returning both answers.
///
/// The exit-status assertion is hard: a run that refused (exit `3`, an `unsupported`
/// capability gap) or raised must be reported as such, never folded into a wrong count.
fn observe(fixture: &Fixture, function: &str) -> Option<Observed> {
    let run = invoke_runtime(fixture, function)?;
    let stdout = String::from_utf8_lossy(&run.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&run.stderr).trim().to_string();
    assert_eq!(
        run.status.code(),
        Some(0),
        "gc_caller:{function}/1 must run to completion; stdout={stdout:?} stderr={stderr:?}"
    );
    let oracle = require_oracle("nif_resource_gc").and_then(|oracle| {
        let answer = invoke_oracle(&oracle, fixture, function);
        if answer.is_none() {
            skip(&format!(
                "the oracle invocation of gc_caller:{function}/1 could not answer"
            ));
        }
        answer
    });
    println!(
        "nif_resource_gc/{function}: rust={stdout} oracle={:?}",
        oracle
    );
    Some(Observed {
        rust: stdout,
        oracle,
    })
}

/// Asserts this runtime's answer against the expected count and, when the oracle could
/// be measured, against the oracle's own answer.
#[track_caller]
fn assert_matches(observed: &Observed, function: &str, expected: &str) {
    assert_eq!(
        observed.rust, expected,
        "gc_caller:{function}/1 must answer {expected} (the oracle's count)"
    );
    if let Some(oracle) = &observed.oracle {
        assert_eq!(
            &observed.rust, oracle,
            "gc_caller:{function}/1 must answer what the system ERTS answers"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------------

/// **(0)** A freshly loaded library has run no destructor — the baseline every other count
/// is measured against.
#[test]
fn a_fresh_library_has_run_no_destructor() {
    let Some(fixture) = build_fixture("fresh") else {
        return;
    };
    let Some(observed) = observe(&fixture, "fresh") else {
        skip("the beam_rt invocation outlived the probe bound");
        fixture.cleanup();
        return;
    };
    assert_matches(&observed, "fresh", "0");
    fixture.cleanup();
}

/// **(a) THE PIN.** A resource term that is dropped and then parked over must have its
/// type's destructor run — exactly once.
///
/// `probe/0` leaves the object held by the term alone, so the destructor can only run if
/// the runtime notices the term dying: the pre-fix runtime answered `0` here (the
/// reference the term took was never dropped), which is the divergence
/// `context::gc::resources` closes. The park loop inside `drop/1` is what gives the
/// park-driven prune its chance to run *after* the term died — reading the count
/// immediately, with no park in between, would pass on the pre-fix runtime too.
#[test]
fn the_park_after_a_dropped_resource_term_runs_its_destructor() {
    let Some(fixture) = build_fixture("drop") else {
        return;
    };
    let Some(observed) = observe(&fixture, "drop") else {
        skip("the beam_rt invocation outlived the probe bound");
        fixture.cleanup();
        return;
    };
    assert_matches(&observed, "drop", "1");
    fixture.cleanup();
}

/// **(b)** A second forced collection does not run the destructor a second time.
///
/// The release is idempotent by term identity (the registry binding is taken out once),
/// so `{First, Second}` must be `{1, 1}`: a prune that released the same term twice, or a
/// destructor that ran per collection, would answer `{1, 2}`.
#[test]
fn a_second_forced_collection_does_not_run_the_destructor_again() {
    let Some(fixture) = build_fixture("twice") else {
        return;
    };
    let Some(observed) = observe(&fixture, "drop_twice") else {
        skip("the beam_rt invocation outlived the probe bound");
        fixture.cleanup();
        return;
    };
    assert_matches(&observed, "drop_twice", "{1,1}");
    fixture.cleanup();
}

/// **(c1)** A resource term still held in a **variable** across the parks survives: the
/// destructor must not run.
///
/// The parked root set names the term's own word, so the reachability walk finds it and
/// the prune releases nothing. (The word lives in the runtime's pooled-BIF-heap region —
/// a region the process's collector does not scan — so the prune-only valve
/// `reaches_an_unscanned_word` declines the whole prune here: a leak, never a dangle.)
#[test]
fn a_resource_term_held_in_a_variable_survives_the_prune() {
    let Some(fixture) = build_fixture("keep_var") else {
        return;
    };
    let Some(observed) = observe(&fixture, "keep_var") else {
        skip("the beam_rt invocation outlived the probe bound");
        fixture.cleanup();
        return;
    };
    assert_matches(&observed, "keep_var", "0");
    fixture.cleanup();
}

/// **(c2)** ... and one held inside a **tuple** survives the same way.
#[test]
fn a_resource_term_held_in_a_tuple_survives_the_prune() {
    let Some(fixture) = build_fixture("keep_tuple") else {
        return;
    };
    let Some(observed) = observe(&fixture, "keep_tuple") else {
        skip("the beam_rt invocation outlived the probe bound");
        fixture.cleanup();
        return;
    };
    assert_matches(&observed, "keep_tuple", "0");
    fixture.cleanup();
}

/// **(c3)** ... and one held inside a **list** (`[Res|[]]`) survives — the cons-cell
/// shape, where both words of the header-less cell are references.
#[test]
fn a_resource_term_held_in_a_list_survives_the_prune() {
    let Some(fixture) = build_fixture("keep_list") else {
        return;
    };
    let Some(observed) = observe(&fixture, "keep_list") else {
        skip("the beam_rt invocation outlived the probe bound");
        fixture.cleanup();
        return;
    };
    assert_matches(&observed, "keep_list", "0");
    fixture.cleanup();
}

/// **(c4)** A resource term **sent in a message** survives too.
///
/// This is the discriminating shape: the send path copies the term onto the receiving
/// process's own (scanned) heap, so the reachable word is a *copy at a different address*
/// while the minted word is no longer in any root. A walk that matched only term
/// addresses would answer "unreachable" and free an object Erlang still references; the
/// reference's **identity** is what has to be matched.
#[test]
fn a_resource_term_sent_in_a_message_survives_the_prune() {
    let Some(fixture) = build_fixture("keep_msg") else {
        return;
    };
    let Some(observed) = observe(&fixture, "keep_msg") else {
        skip("the beam_rt invocation outlived the probe bound");
        fixture.cleanup();
        return;
    };
    assert_matches(&observed, "keep_msg", "0");
    fixture.cleanup();
}

/// **(d)** A resource that is **never exported as a term** is destroyed by its own
/// `enif_release_resource` — no collection and no prune involved — which is the oracle's
/// transcript step (`{0, 1}`: nothing before, one destructor after).
///
/// The park loop sits between the two reads so the oracle's *deferred* destructor (misc
/// aux work) has settled by the second read; this runtime's is synchronous, so the park
/// changes nothing there.
#[test]
fn a_never_exported_resource_runs_its_destructor_without_the_prune() {
    let Some(fixture) = build_fixture("unexported") else {
        return;
    };
    let Some(observed) = observe(&fixture, "unexported") else {
        skip("the beam_rt invocation outlived the probe bound");
        fixture.cleanup();
        return;
    };
    assert_matches(&observed, "unexported", "{0,1}");
    fixture.cleanup();
}

// ---------------------------------------------------------------------------------------------
// The pending-timer root family — the term's ONLY owner is the runtime's timer store
// ---------------------------------------------------------------------------------------------

/// **(e) THE TIMER-FAMILY PIN.** A resource term that only a **pending `erlang:send_after`
/// delivery** names must survive the park-arm prune, and be readable when it arrives.
///
/// `keep_timer/1` mints the term *inside* the scheduling call and returns `ok`, so when it
/// returns no register, stack slot or dictionary entry of the process names the term any more:
/// the timer store the runtime copies the message into is its only owner. The park loop then
/// crosses many slice boundaries, which is where `context::gc::resources::reclaim` runs — and
/// that walk is seeded with the runtime's non-process root sources
/// ([`Runtime::gather_off_heap_roots`]), whose pending-timer family is the only one that can
/// reach this term.
///
/// The expected answer is `{0, 0, 1}`: no destructor before, none after (the term is alive), and
/// the delivered term is still a live resource. A resource copy carries its reference **identity**,
/// which is what the prune's second match tier keys on — so a prune that does not see the timer
/// family answers "unreachable" and drops the object's reference while the message is still in
/// flight, which runs the destructor (measured `{1, 0}` in the reverted build).
///
/// `declines` is deliberately **not** asserted here: a resource held in a *variable* across the
/// park loop makes the prune-only valve decline (the word stands in the pooled-BIF-heap region),
/// which is a leak rather than a dangle. This shape keeps the term out of the process's own roots,
/// but the count/read pair below is the observable either way.
#[test]
fn a_resource_named_only_by_a_pending_timer_survives_the_park_prune() {
    let Some(fixture) = build_fixture("keep_timer") else {
        return;
    };
    let Some(observed) = observe(&fixture, "keep_timer") else {
        skip("the beam_rt invocation outlived the probe bound");
        fixture.cleanup();
        return;
    };
    assert_matches(&observed, "keep_timer", "{0,0,1}");
    fixture.cleanup();
}

/// **(f) The over-retention control for the same family.** A resource minted into a timer store
/// that is then **cancelled** is named by nothing, so the destructor must run — the new root
/// family must not turn the prune into an over-retainer.
///
/// This is a positive assertion about the mechanism (a keep-everything prune would answer `0`),
/// not a dependence on a defect: the term's fate is sealed by `erlang:cancel_timer/1`, and the
/// oracle answers the same `1`.
#[test]
fn a_cancelled_timer_releases_its_resource() {
    let Some(fixture) = build_fixture("drop_timer") else {
        return;
    };
    let Some(observed) = observe(&fixture, "drop_timer") else {
        skip("the beam_rt invocation outlived the probe bound");
        fixture.cleanup();
        return;
    };
    assert_matches(&observed, "drop_timer", "1");
    fixture.cleanup();
}

// ---------------------------------------------------------------------------------------------
// The `-eval` route's statement boundary — the second, NON-park cadence site
// ---------------------------------------------------------------------------------------------
//
// Everything above drives a **scheduled** process (`run <beam> ...`), which parks at its slice
// boundaries; that park arm is where `context::gc::resources` reclaims. The `-eval` **action
// process is never enqueued**, so it never parks: without a second site an `-eval`-only run would
// retain every resource term it ever minted for the runtime's life. That site is the top-level
// statement boundary of `Evaluator::run_top_level`
// (`src/eval/mod.rs` → `reclaim::prune_at_statement_boundary`), and the two legs below are its pin
// and its A/B control.
//
// The `-eval` route is driven **in process** (`Runtime::eval`) rather than through the binary,
// because the figures that make the claim falsifiable — `Runtime::resource_prune_stats()` — are
// runtime state, not observable output. Neither figure is a differential one: the oracle refcounts
// resource terms straight off its process heap, so it has no counterpart for a frontier, a release
// count or a live-term table (which is why no leg below compares against `erl`).

use std::sync::Mutex;

use beam_rt::config::RuntimeConfig;
use beam_rt::runtime::Runtime;

/// The mint counts the sweep drives.
///
/// 1 000 is included on purpose: the claim under test is that the resource table is **flat in N**,
/// and it takes a hundred-fold step for a linear table to be unmistakable.
const EVAL_SWEEP: [usize; 3] = [10, 100, 1_000];

/// Serialises the two in-process legs, and the reason is not politeness: `beam_abi`'s resource-term
/// registry is **process-global** (`beam_abi::nif::enif::resources::resource_terms`, one
/// `OnceLock<Mutex<…>>`), so two legs running concurrently in this test binary would each see the
/// other's minted terms in `live_terms` and in the frontier snapshot.
static EVAL_LEG: Mutex<()> = Mutex::new(());

/// One measured leg of the sweep.
///
/// `live_before`/`live_after` are **deltas by construction**: the registry is process-global, so a
/// leg measures its own growth (`live_after - live_before`) instead of assuming an empty table.
#[derive(Debug, Clone, Copy)]
struct LegSample {
    /// How many resource terms the expression minted.
    mints: usize,
    /// The fixture's own destructor counter, read as `dtor_count() after - dtor_count() before`.
    dtors: u64,
    /// `erlang:memory(total)` at the end of the leg, in bytes.
    total_bytes: u64,
    /// `ResourcePruneStats::live_terms` before the expression.
    live_before: usize,
    /// ... and after it.
    live_after: usize,
    /// `ResourcePruneStats::released` (this runtime only, so it starts at `0`).
    released: u64,
    /// `ResourcePruneStats::prunes`.
    prunes: u64,
    /// `ResourcePruneStats::declines`.
    declines: u64,
}

impl LegSample {
    /// The table growth this leg caused.
    fn growth(&self) -> usize {
        self.live_after.saturating_sub(self.live_before)
    }
}

/// Prints one leg, so a failure (and the reported measurement table) is diagnosable from the log.
fn report_legs(label: &str, series: &[LegSample]) {
    for leg in series {
        println!(
            "nif_resource_gc/eval {label}: mints={} dtors={} released={} prunes={} declines={} \
             live={}->{} (growth {}) memory_total={}B",
            leg.mints,
            leg.dtors,
            leg.released,
            leg.prunes,
            leg.declines,
            leg.live_before,
            leg.live_after,
            leg.growth(),
            leg.total_bytes,
        );
    }
}

/// The `-eval` expression: load the library, take the destructor counter's baseline, then
/// **discard** one minted resource term per statement, and finally report the destructor delta and
/// the runtime's total memory.
///
/// Each minting statement must leave its term un-rooted by the time the statement boundary runs,
/// which is why it is `erlang:length([probe()])` — the statement's **value** is a small integer.
/// A bare `probe()` would not do, and neither would a bound variable: both the evaluator's
/// environment (a bound variable) and the reduced value of the statement just finished are handed
/// to the prune as roots, so either shape is *correctly* retained — and the pin would then measure
/// the root set instead of the table.
///
/// The load path is double-quoted so the parser builds a char list, which is what
/// `erlang:load_nif/2` accepts (and what the fixture's `init/1` receives here).
fn eval_sweep_expression(fixture: &Fixture, mints: usize) -> String {
    let mut parts = vec![
        format!(
            "nif_resource_gc:init(\"{}\")",
            fixture.load_path().display()
        ),
        "D0 = nif_resource_gc:dtor_count()".to_string(),
    ];
    for _ in 0..mints {
        parts.push("erlang:length([nif_resource_gc:probe()])".to_string());
    }
    parts.push("{nif_resource_gc:dtor_count() - D0, erlang:memory(total)}".to_string());
    parts.join(", ")
}

/// Reads a `{A,B}` 2-tuple off the `~w` printer's output.
fn parse_pair(text: &str) -> (u64, u64) {
    let trimmed = text.trim();
    let inner = trimmed
        .strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
        .unwrap_or_else(|| panic!("the witness must print a 2-tuple, got {text:?}"));
    let (left, right) = inner
        .split_once(',')
        .unwrap_or_else(|| panic!("the witness must print a 2-tuple, got {text:?}"));
    let parse = |field: &str| {
        field
            .trim()
            .parse::<u64>()
            .unwrap_or_else(|error| panic!("unparsable witness field {field:?} in {text:?}: {error}"))
    };
    (parse(left), parse(right))
}

/// Drives one leg: one `-eval` expression of `mints` mint-and-discard statements on `runtime`.
///
/// Both sweeps below run their legs on **one** runtime rather than a fresh one per leg, and the
/// reason is a measured property of the ABI's registry, not convenience: a NIF result is built on
/// the running call's pooled BIF heap, so a *fresh* `Runtime` mints at the same words its
/// predecessor did — and `Registry::bind_term` **evicts** a binding whose word the new mint now
/// occupies ("a reused address evicts a dead mint", `beam_abi`'s resources module). Across two
/// runtimes that eviction destroys the earlier runtime's still-live terms, which would put
/// destructor runs into the *other* leg's counter. Within one runtime every mint gets its own word
/// (a NIF result is not rewound onto the pool), so a leg's figures are its own.
fn drive_eval_leg(runtime: &mut Runtime, fixture: &Fixture, mints: usize) -> LegSample {
    let before = runtime.resource_prune_stats();
    let expression = eval_sweep_expression(fixture, mints);
    let value = runtime
        .eval(&expression)
        .unwrap_or_else(|error| panic!("the {mints}-mint eval leg failed: {error:?}"));
    let text = runtime
        .format_term_w(value, None)
        .expect("the witness formats");
    let stats = runtime.resource_prune_stats();
    let (dtors, total_bytes) = parse_pair(&text);
    LegSample {
        mints,
        dtors,
        total_bytes,
        live_before: before.live_terms,
        live_after: stats.live_terms,
        released: stats.released,
        prunes: stats.prunes,
        declines: stats.declines,
    }
}

/// A fresh `-eval` runtime over the fixture directory, with the resource prune set to `enabled`.
fn eval_runtime(fixture: &Fixture, enabled: bool) -> Runtime {
    let config = RuntimeConfig::default().with_code_paths(vec![fixture.dir.clone()]);
    let mut runtime = Runtime::new(config).expect("the eval runtime");
    runtime.set_resource_prune_enabled(enabled);
    runtime
}

/// How many resource terms the **process-global** registry holds right now, through the public
/// runtime view.
///
/// Both `-eval` legs below need this to be `0` before they start, and the reason is a measured
/// property of the ABI's registry rather than a preference: a resource term's mint block stands on
/// the running call's pooled BIF heap, so a *fresh* `Runtime` in the same process mints at the very
/// words its predecessor did — and `Registry::bind_term` evicts a binding whose word the new mint
/// takes over ("a reused address evicts a dead mint"). With a dirty registry the reuse is
/// **word-for-word**, so the frontier's candidate set is identical at every boundary, gate 2
/// (`now == frontier`) defers every prune, and the leg would measure the *previous* leg's state
/// instead of its own. Each leg therefore runs in its own process: this suite's other tests drive
/// the built binary (a subprocess), so a plain `cargo test` run starts clean, and the control leg is
/// `#[ignore]`d so it can never run beside this one.
fn live_resource_terms() -> usize {
    Runtime::new(RuntimeConfig::default())
        .expect("the probe runtime")
        .resource_prune_stats()
        .live_terms
}

/// The host's load average, printed with the measurement (this host is shared, so a wall-time
/// figure would be meaningless while a deterministic counter is not).
fn load_average() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .map(|text| text.trim().to_string())
        .unwrap_or_else(|error| format!("unavailable ({error})"))
}

/// **THE CADENCE PIN.** An `-eval`-only run that mints and drops `N` resource terms must leave the
/// resource table **bounded (flat in N)**, not linear.
///
/// Asserted per leg, for `N` ∈ [`EVAL_SWEEP`] (the sweep runs on **one** runtime, so `released` and
/// `prunes` are asserted against the cumulative mint count):
///
/// * `dtors == N` — the fixture's own destructor counter, taken as a delta, so it is **exactly** one
///   destructor run per minted-and-dropped term. No residual constant is needed here: the very first
///   statement boundary of the run follows `nif_resource_gc:init/1`, which mints nothing, so the
///   frontier records an empty candidate set and is already correct by the first mint (the
///   one-statement leak the design documents is *zero* in this shape rather than one term);
/// * `released == ΣN` and `prunes == ΣN` — one walk per minting statement, releasing exactly that
///   statement's mint — and `declines == 0`, so no walk was refused;
/// * `live_after <= live_before` — the table does not grow by a single entry, which is the "bounded"
///   half stated on the table itself;
/// * the sweep's own comparison: the table after `1 110` mints is no larger than it was after the
///   first `10` (a linear table answers `1 110` there);
/// * `erlang:memory(total)` is printed, never asserted. Measured: it does **not** move either way
///   (`19 107` B after the first leg, `19 121` B after the last, in both legs) — a resource term's
///   object is a host `malloc` outside every runtime allocator, so this figure cannot see the table
///   and is not evidence either way.
///
/// # Measured (debug build, shared host, `loadavg` ≈ 36 at run time)
///
/// | mints (cumulative) | dtors | released | prunes | declines | live terms |
/// |---|---|---|---|---|---|
/// | 10 | 10 | 10 | 10 | 0 | 0 → 0 |
/// | 100 (110) | 100 | 110 | 110 | 0 | 0 → 0 |
/// | 1 000 (1 110) | 1 000 | 1 110 | 1 110 | 0 | 0 → 0 |
///
/// # The revert evidence
///
/// With the boundary call reverted (both `self.prune_resources(env, …)` lines in
/// `src/eval/mod.rs::run_top_level` commented out) this test fails at its **first** assertion, with
/// the table growing linearly instead:
///
/// ```text
/// mints=10   dtors=0    released=0   prunes=0   declines=0 live=0->10    (growth 10)
/// mints=100  dtors=0    released=0   prunes=0   declines=0 live=10->110  (growth 100)
/// mints=1000 dtors=0    released=0   prunes=0   declines=0 live=110->1110 (growth 1000)
/// left: 0, right: 10   ("every minted-and-dropped resource term must run its destructor …")
/// ```
///
/// — i.e. exactly the pre-fix shape the site exists to remove, and identical to the control leg
/// below, which is what makes that leg a meaningful negative control.
#[test]
fn the_eval_statement_boundary_keeps_the_resource_table_bounded() {
    let dirty = live_resource_terms();
    if dirty > 0 {
        skip(&format!(
            "the process-global NIF-resource registry already holds {dirty} term(s): the -eval \
             legs must not share a process (run the `#[ignore]`d control leg with `--ignored` \
             instead of `--include-ignored`)"
        ));
        return;
    }
    let Some(fixture) = build_fixture("eval_cadence") else {
        return;
    };
    let _serialised = EVAL_LEG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    println!("nif_resource_gc/eval reclaim-on loadavg={}", load_average());
    // ONE runtime for the whole sweep (see [`drive_eval_leg`]).
    let mut runtime = eval_runtime(&fixture, true);
    let mut series = Vec::with_capacity(EVAL_SWEEP.len());
    for mints in EVAL_SWEEP {
        series.push(drive_eval_leg(&mut runtime, &fixture, mints));
    }
    report_legs("reclaim-on", &series);
    let mut cumulative = 0usize;
    for leg in &series {
        cumulative += leg.mints;
        assert_eq!(
            leg.dtors, leg.mints as u64,
            "every minted-and-dropped resource term must run its destructor at the next statement \
             boundary ({} mints): {leg:?}",
            leg.mints
        );
        assert_eq!(
            leg.released, cumulative as u64,
            "the boundary must have released exactly the {cumulative} terms the sweep has minted \
             so far: {leg:?}"
        );
        assert_eq!(
            leg.prunes, cumulative as u64,
            "one walk per minting statement ({cumulative} so far): {leg:?}"
        );
        assert_eq!(
            leg.declines, 0,
            "this root graph is fully walkable, so a declined walk would be a bug: {leg:?}"
        );
        assert!(
            leg.live_after <= leg.live_before,
            "the resource table must not grow ({} mints left {} live term(s) behind: {} -> {}): \
             {leg:?}",
            leg.mints,
            leg.growth(),
            leg.live_before,
            leg.live_after
        );
    }
    // Flat in N, stated as the sweep's own comparison: the table after a hundred-fold more mints may
    // not be larger than it was after the smallest step (a linear table would answer 1 110 here).
    let early_peak = series
        .iter()
        .take(2)
        .map(|leg| leg.live_after)
        .max()
        .unwrap_or(0);
    let large = series.last().copied().expect("the sweep has a last leg");
    assert!(
        large.live_after <= early_peak,
        "the table drifted upward across the sweep: {} live term(s) after {} mints vs an early \
         peak of {early_peak}",
        large.live_after,
        cumulative
    );
    fixture.cleanup();
}

/// The A/B control for the pin above: **the same sweep** with the reclaim switched off
/// (`Runtime::set_resource_prune_enabled(false)`) retains one resource term per mint, so the table
/// is **linear in N**.
///
/// This is a positive assertion about the control, not a shipped dependence on a defect: it is what
/// makes [`the_eval_statement_boundary_keeps_the_resource_table_bounded`] falsifiable, exactly as
/// `tests/eval_off_heap_reclaim.rs`'s `without_the_reclaim_the_same_loop_is_linear` does for the refc
/// half. With the mechanism off, nothing is observed, so no frontier is recorded, no walk runs and
/// no destructor is run by it.
///
/// # Measured
///
/// Debug build on a shared host; the figures below are this leg's own printed lines from a green
/// run (`resource_prune_stats()` is per-runtime, so `released`/`prunes` start at `0`; `live` is the
/// process-global registry's size, so it is reported as the cumulative figure the sweep produces):
///
/// | mints (cumulative) | dtors | released | prunes | live terms (cumulative) |
/// |---|---|---|---|---|
/// | 10 | 0 | 0 | 0 | 10 |
/// | 100 (110) | 0 | 0 | 0 | 110 |
/// | 1 000 (1 110) | 0 | 0 | 0 | 1 110 |
///
/// So the control's table is **exactly linear**: one entry per mint, never released. `erlang:memory
/// (total)` is printed beside it and deliberately not asserted — it is an arena figure (the pooled
/// BIF heap a NIF result is built on is not rewound after a NIF call), not a table figure.
///
/// # Why this leg is `#[ignore]`d
///
/// It leaves its `1 110` minted terms **live** in the process-global registry (that is the whole
/// point of the control), and a live set with those exact words is precisely what silences the pin
/// above if the two legs share a process (`Registry::bind_term` evicts a binding whose word a new
/// mint takes over, so the frontier's candidate set would stop changing). Neither leg is therefore
/// allowed to run beside the other:
///
/// ```text
/// cargo test --manifest-path erts_rust/Cargo.toml -p beam_rt --test nif_resource_gc            # the pin
/// cargo test --manifest-path erts_rust/Cargo.toml -p beam_rt --test nif_resource_gc -- --ignored --nocapture   # this control
/// ```
///
/// A plain `cargo test` run selects the pin and not this leg; `--ignored` selects only this leg. The
/// dirty-registry guard below turns the one remaining combination (`--include-ignored`) into a loud
/// skip instead of a wrong figure.
#[test]
#[ignore = "leaves its minted terms live; run alone with --ignored (see the docs)"]
fn without_the_resource_prune_the_eval_route_is_linear() {
    let dirty = live_resource_terms();
    if dirty > 0 {
        skip(&format!(
            "the process-global NIF-resource registry already holds {dirty} term(s): this control \
             needs its own process (run it alone with `--ignored --exact`)"
        ));
        return;
    }
    let Some(fixture) = build_fixture("eval_control") else {
        return;
    };
    let _serialised = EVAL_LEG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    println!("nif_resource_gc/eval reclaim-off loadavg={}", load_average());
    // ONE runtime for the whole sweep (see [`drive_eval_leg`]), so the legs cannot evict each
    // other's mints and every retained term is this control's own.
    let mut runtime = eval_runtime(&fixture, false);
    let mut series = Vec::with_capacity(EVAL_SWEEP.len());
    for mints in EVAL_SWEEP {
        series.push(drive_eval_leg(&mut runtime, &fixture, mints));
    }
    report_legs("reclaim-off", &series);
    let mut cumulative = 0usize;
    for leg in &series {
        cumulative += leg.mints;
        assert_eq!(
            leg.dtors, 0,
            "with the reclaim switched off nothing is released, so no destructor runs ({} mints, \
             {cumulative} so far): {leg:?}",
            leg.mints
        );
        assert_eq!(leg.released, 0, "nothing may be released: {leg:?}");
        assert_eq!(leg.prunes, 0, "no walk may run: {leg:?}");
        assert_eq!(leg.declines, 0, "and nothing may decline: {leg:?}");
        assert!(
            leg.live_after >= cumulative,
            "the control must be LINEAR: {cumulative} mints must leave at least {cumulative} live \
             resource term(s), not {}: {leg:?}",
            leg.live_after
        );
    }
    // Linear stated as the sweep's own comparison: a hundred-fold more mints leaves a hundred-fold
    // more terms (a bounded table would answer 0 at both ends).
    let small = series.first().copied().expect("the sweep has a first leg");
    let large = series.last().copied().expect("the sweep has a last leg");
    assert!(
        large.live_after >= 10 * small.live_after,
        "the control must grow with N, not stay bounded ({} mints → {} term(s), {} mints → {})",
        small.mints,
        small.live_after,
        cumulative,
        large.live_after
    );
    fixture.cleanup();
}

// ---------------------------------------------------------------------------------------------
// The **engine-arguments** root family — a child's first root set, named by its own engine only
// ---------------------------------------------------------------------------------------------
//
// A spawn made from **inside a scheduler slice** is the pass route: `Runtime::note_pending_spawn`
// copies the entry fun and every argument onto the pointer-stable arena and the pass registers the
// engine with `arena_args`, so the child's first root set is named by that engine and by nothing
// else in this runtime ([`RunnerState::gather_off_heap_roots`]).
//
// The park arm can never cover that family (`RunnerState` is `mem::take`n for the duration of a
// pass, which is exactly when the arm runs), so the two **non-park** sites are its only witnesses —
// and the `-eval` statement boundary is the one a test can pause at. The shape below therefore:
//
// 1. mints a resource **inside a scheduler slice** (a grandchild spawned by a child of the
//    `-eval` expression, so the spawn that carries it is the pass route);
// 2. lets the statement boundary between the spawn statement and the read run the prune; and
// 3. reads the grandchild's own view afterwards.
//
// The figures that make the claim falsifiable (`Runtime::resource_prune_stats()`) are runtime
// state, and the ABI's resource-term registry is **process-global**, so the leg runs in a child
// process of this test binary (see [`stable_child_args_worker_leg`]).

/// The `#[ignore]`d in-process leg of the engine-arguments pin.
const ENGINE_ARGS_WORKER: &str = "stable_child_args_worker_leg";

/// The prefix of the worker's result line.
const ENGINE_ARGS_PREFIX: &str = "engine_args_leg:";

/// The `-eval` expression of the engine-arguments case: load the library, start the child that
/// mints in its own slice and spawns the grandchild, then drive the grandchild's read.
///
/// The middle statement's **value** is the grandchild's pid — an immediate — so the minted
/// resource is not a root of the evaluator's environment; the read statement is a *later*
/// statement, so the boundary that runs the prune falls between them.
fn engine_args_expression(fixture: &Fixture) -> String {
    format!(
        "nif_resource_gc:init(\"{}\"), Child = gc_caller:start_via_pass(), \
         gc_caller:drive_child(Child)",
        fixture.load_path().display()
    )
}

/// One parsed engine-arguments leg: the answer Erlang saw and the runtime's own prune counters.
#[derive(Debug, Clone)]
struct EngineArgsLeg {
    /// The formatted `{Dtors, IsResource}` the grandchild answered.
    answer: String,
    /// `ResourcePruneStats::prunes` after the expression.
    prunes: u64,
    /// `ResourcePruneStats::released` after the expression.
    released: u64,
    /// `ResourcePruneStats::declines` after the expression.
    declines: u64,
    /// `ResourcePruneStats::live_terms` after the expression.
    live: usize,
}

/// Reads the worker's `engine_args_leg:` line out of its captured stdout.
///
/// The captured stream is flattened first (`~w` output may itself carry a line break, and the test
/// harness interleaves its own progress line), so the parse depends on the field names only: the
/// record ends at the first token that is not a `key=value` field (libtest appends `ok`,
/// `test result: ok. …`), so the five fields must all precede it.
fn parse_engine_args_leg(stdout: &str) -> Option<EngineArgsLeg> {
    let flattened = stdout.replace('\n', " ");
    let start = flattened.find(ENGINE_ARGS_PREFIX)?;
    let line = &flattened[start + ENGINE_ARGS_PREFIX.len()..];
    let mut answer = None;
    let mut prunes = None;
    let mut released = None;
    let mut declines = None;
    let mut live = None;
    for field in line.split_whitespace() {
        let Some((key, value)) = field.split_once('=') else {
            break; // the first non-`key=value` token ends the record
        };
        let number = |value: &str| -> Option<u64> {
            value
                .parse()
                .map_err(|error| println!("engine_args_leg field {key}={value}: {error}"))
                .ok()
        };
        match key {
            "answer" => answer = Some(value.to_string()),
            "prunes" => prunes = number(value),
            "released" => released = number(value),
            "declines" => declines = number(value),
            "live" => live = number(value).map(|n| n as usize),
            other => println!("engine_args_leg: ignoring unknown field {other}"),
        }
    }
    Some(EngineArgsLeg {
        answer: answer?,
        prunes: prunes?,
        released: released?,
        declines: declines?,
        live: live?,
    })
}

/// **THE ENGINE-ARGUMENTS PIN (the in-process half).** A resource minted inside a scheduler slice
/// and handed to a **pass-route** spawn is named by that child's engine alone, so the `-eval`
/// statement boundary must find it and release nothing.
///
/// Driven by [`a_resource_in_a_stable_childs_arguments_survives_the_eval_statement_boundary`] in a
/// child process: the resource-term registry is process-global (`beam_abi`'s `resource_terms`), and
/// a retained term stays live for the runtime's life (a candidate in the frontier is never fresh
/// again), so a leg that ran inside the shared test process would silence the sibling `-eval` legs.
///
/// With the engine-arguments family gathered, the boundary's walk reaches the term through the
/// child's arguments: `answer={0,1}` (no destructor, the term still resolves), `prunes` > 0 (the
/// walk really ran) and `released=0`. Reverting
/// `RunnerState::gather_off_heap_roots`'s call from `Runtime::gather_off_heap_roots` answers
/// `answer={1,0}` with `released=1`.
#[test]
#[ignore = "the in-process leg of the engine-arguments pin; run by its sibling, which gives it a process of its own"]
fn stable_child_args_worker_leg() {
    let Some(fixture) = build_fixture("engine_args_worker") else {
        return;
    };
    let dirty = live_resource_terms();
    if dirty > 0 {
        skip(&format!(
            "the process-global NIF-resource registry already holds {dirty} term(s): this leg needs \
             a clean process"
        ));
        return;
    }
    let _serialised = EVAL_LEG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut runtime = eval_runtime(&fixture, true);
    let expression = engine_args_expression(&fixture);
    let value = runtime
        .eval(&expression)
        .unwrap_or_else(|error| panic!("the engine-arguments leg failed: {error:?}"));
    let answer = runtime
        .format_term_w(value, None)
        .expect("the leg's answer formats")
        .trim()
        .to_string();
    let stats = runtime.resource_prune_stats();
    println!(
        "{ENGINE_ARGS_PREFIX} answer={answer} prunes={} released={} declines={} live={}",
        stats.prunes, stats.released, stats.declines, stats.live_terms
    );
    fixture.cleanup();
}

/// **THE ENGINE-ARGUMENTS PIN.** A resource that only a **stabilized child's arguments** name must
/// survive the `-eval` statement boundary, and the child must still see it as a resource.
///
/// The expression ([`engine_args_expression`]) mints inside a scheduler slice and spawns the
/// grandchild through the pass route, so the child's first root set is `arena_args`-stable and no
/// other store names the term. The boundary between the spawn statement and the read statement is
/// where `reclaim::prune_at_statement_boundary` runs, and its walk is seeded with
/// [`Runtime::gather_off_heap_roots`], whose **engine-arguments** family is the only one that can
/// reach this term (the park arm cannot: `RunnerState` is moved out for a pass).
///
/// Asserts, in one place:
/// * the answer is `{0, 1}` — no destructor ran and the child's copy still resolves — and it is
///   the **live oracle's** answer for the same `.beam` and `.so`;
/// * `prunes > 0` — the prune really ran (so the retention is not the trivial "no prune happened");
/// * `released == 0 && declines == 0` — it released nothing and refused nothing.
///
/// The in-process figures come from [`stable_child_args_worker_leg`], which runs in a child process
/// of this test binary (the ABI's resource-term registry is process-global).
#[test]
fn a_resource_in_a_stable_childs_arguments_survives_the_eval_statement_boundary() {
    let Some(fixture) = build_fixture("engine_args") else {
        return;
    };
    let expression = engine_args_expression(&fixture);
    let oracle = require_oracle("nif_resource_gc").and_then(|oracle| {
        // `begin … end`, not parentheses: `-eval` takes one *expression*, and only `begin` sequences
        // several of them without a parse error.
        let probe = format!("io:format(\"~w~n\", [begin {expression} end]), halt(0).");
        let answer = support::oracle_eval(&oracle, Some(&fixture.dir), &probe);
        if answer.is_none() {
            skip("the oracle invocation of the engine-arguments expression could not answer");
        }
        answer.map(|text| text.trim().to_string())
    });
    let exe = std::env::current_exe().expect("the test binary's own path");
    let worker = Command::new(&exe)
        .args([
            "--exact",
            ENGINE_ARGS_WORKER,
            "--ignored",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap_or_else(|error| panic!("cannot run the in-process leg in {}: {error}", exe.display()));
    // Both streams: the child's libtest harness may route the `--nocapture` output of its single
    // leg to either, and the result line is what matters.
    let stdout = format!(
        "{}{}",
        String::from_utf8_lossy(&worker.stdout),
        String::from_utf8_lossy(&worker.stderr)
    );
    let stderr = String::new();
    assert!(
        worker.status.success(),
        "the in-process engine-arguments leg failed ({:?}): stdout={stdout} stderr={stderr}",
        worker.status
    );
    let Some(leg) = parse_engine_args_leg(&stdout) else {
        skip(&format!(
            "the in-process engine-arguments leg produced no result line (its own setup declined?): \
             {stdout}"
        ));
        fixture.cleanup();
        return;
    };
    println!(
        "nif_resource_gc/engine_args: rust={} oracle={oracle:?} prunes={} released={} declines={} \
         live={}",
        leg.answer, leg.prunes, leg.released, leg.declines, leg.live
    );
    assert_eq!(
        leg.answer, "{0,1}",
        "a resource named only by a stabilized child's arguments must survive the statement \
         boundary (no destructor, and the child's copy still resolves): {leg:?}"
    );
    assert!(
        leg.prunes > 0,
        "the statement boundary must really have run a prune, otherwise the retention proves \
         nothing: {leg:?}"
    );
    assert_eq!(
        leg.released, 0,
        "the boundary released a live resource term (it is named by the child's arguments): {leg:?}"
    );
    assert_eq!(leg.declines, 0, "nothing may decline here: {leg:?}");
    if let Some(oracle) = &oracle {
        assert_eq!(
            &leg.answer, oracle,
            "the answer must be the live oracle's own for the same .beam and .so"
        );
    }
    fixture.cleanup();
}
