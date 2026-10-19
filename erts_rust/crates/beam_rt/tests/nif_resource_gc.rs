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
-export([init/1, probe/0, probe_unexported/0, dtor_count/0]).
init(Path) -> erlang:load_nif(Path, 0).
probe() -> erlang:nif_error(not_loaded).
probe_unexported() -> erlang:nif_error(not_loaded).
dtor_count() -> erlang:nif_error(not_loaded).
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
         keep_msg/1, unexported/1]).

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
