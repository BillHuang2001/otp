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
//! **The NIF-resource prune at a *nested* site** (contract A.3): the anti-dangle half and the
//! scope half, end to end.
//!
//! `crates/beam_rt/src/eval/reclaim/resource.rs` adds the second of its two **non-park**
//! reclamation sites: a **nested run's epilogue** (`RuntimeContext::adopt_nested_payloads`,
//! `src/context/pump.rs`), reached by every fun application / cross-module call a
//! `-eval`-route statement makes (`lists:map/2`, `lists:foreach/2`, a plain `M:F(…)` call).
//! The site snapshots a [`Frontier`] — the live resource-term set — at run entry and releases
//! only candidates **minted during that run** that the run's own published live set does not
//! name (the same `only`-list restriction the sibling refc payload prune needs, whose measured
//! justification was that an *unscoped* nested prune corrupted 190 of 200 elements of a
//! `lists:map/2` over 40 000-byte binaries).
//!
//! This suite is the pin for both halves of that contract, and it does not re-test the
//! park-arm site: the sibling suite (`tests/nif_resource_gc.rs`) owns the **scheduled** route,
//! where the runner's `Yield` arm prunes at the slice boundary. Every test here drives the
//! **`-eval`** route instead, where the action process is never enqueued and therefore never
//! parks — so a one-statement expression's *only* possible reclamation site is the nested
//! epilogue (the statement boundary releases nothing on a runtime's first boundary). That is
//! what makes the counts below a measurement of the nested site rather than of the park arm.
//!
//! # The two halves
//!
//! * **Anti-dangle** ([`a_resource_held_in_a_variable_survives_a_nested_map`] and its tuple and
//!   list siblings): a resource term the expression still holds **across** a nested `lists:map/2`
//!   must survive it — its type's destructor must not run, *and* the term must still resolve
//!   through `enif_get_resource` with its payload intact. The second half is the one that
//!   matters: a `release_resource_terms` predicate that answered "unreachable" for a live term
//!   would free the object, and the term would then be a dangling magic reference. Peek is the
//!   probe: the fixture's `peek/1` answers `8` (the payload size `enif_sizeof_resource` reports)
//!   only when `enif_get_resource` still resolves the term to *this* type's object with the
//!   marker byte `probe/0` wrote; anything else is the atom `error`.
//! * **Scope** ([`every_element_of_a_nested_map_stays_reachable_and_usable`] and
//!   [`a_dropped_resource_is_released_at_its_own_runs_epilogue`]): the exact `lists:map/2` shape
//!   that pinned the refc scope. Elements built by an earlier nested run are collected into a
//!   returned list, so each must still resolve (and no destructor may run for them); the
//!   companion leg drops each minted term inside the fun, and the objects must **not** accumulate
//!   — the destructor count tracks N and the fixture's own live-object count returns to `0` at
//!   both N = 10 and N = 200 (flat, not linear).
//!
//! # Non-vacuity: the linear control
//!
//! "Live objects is `0` after the loop" proves nothing unless the counter *can* grow, so
//! [`a_reference_the_nif_itself_holds_is_never_reclaimed`] drives a variant that keeps its own
//! reference on every object (`probe_held/0`): no prune and no collection may destroy one, so
//! the same counter is linear in the call count. Without that leg a broken counter would make
//! the flat half pass silently.
//!
//! # The oracle
//!
//! Every probe runs the **identical compiled `.beam`s and the identical `.so`**, from the
//! **identical expression text**, under this runtime and under the system `erl`, so the answer is
//! compared rather than asserted against a hard-coded table. Measured on OTP 29.1.1 / erts-17.1:
//!
//! | probe | this runtime | oracle |
//! |---|---|---|
//! | `keep_var` / `keep_tuple` / `keep_list` | `{8,0}` | `{8,0}` |
//! | `mapped(10)` / `mapped(200)` | `{10,0}` / `{200,0}` | `{10,0}` / `{200,0}` |
//! | `dropped(10)` / `dropped(200)` | `{10,0}` / `{200,0}` | `{10,0}` / `{200,0}` |
//! | `held(16)` | `{16,0}` | `{16,0}` |
//! | `plain` | `{error,8,0}` | `{error,8,0}` |
//!
//! The one *timing* difference is the destructor's: this runtime releases synchronously at each
//! run's epilogue, while the C defers the `dtor` as misc aux work, so the oracle's count only moves
//! once a GC and a thread-progress point have been reached (the fixture's `dropped/2` does exactly
//! that, and does nothing for this runtime). Because the oracle can lag **strictly** behind under a
//! loaded host, the dropped legs compare exactly first and otherwise only the shared **shape** —
//! `dtor + live == N`, i.e. every minted object is either destroyed or still alive — which is what
//! [`assert_settled`] does.
//!
//! # Skip policy
//!
//! Four host facilities are needed (the `erl` oracle, `erlc`, a C compiler and the host
//! `erl_nif.h`). When one is missing the test prints a **loud** banner and returns — it never
//! silently passes and never panics.
//!
//! # Why this file carries its own C fixture instead of extending `fixtures/nif_resource_gc.c`
//!
//! The shared fixture could not gain the `peek/1` entry point this suite needs: real ERTS
//! **refuses to load** a NIF library whose `ErlNifFunc` table names a function the module does
//! not define (`erts/emulator/beam/erl_nif.c:4876`, `Function not found M:F/A`), and the sibling
//! suite's generated `nif_resource_gc` module exports only `init/1`, `probe/0`,
//! `probe_unexported/0` and `dtor_count/0` — so adding `peek/1` to that table would break every
//! test in the sibling suite. The library is therefore a separate one (`nif_resource_nested`),
//! embedded here as [`NIF_C`] the way the Erlang fixtures are, so this file stays a single
//! self-contained unit and the sibling's fixture keeps growing independently.
mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use support::{find_on_path, require_oracle, run_bounded};

/// The C fixture: a resource type whose destructor counts its own runs **and** the live object
/// count, plus two probes (`probe/0` exports a term that owns the object's only reference,
/// `probe_held/0` keeps a C reference so no prune may destroy it) and `peek/1`, the
/// `enif_get_resource` + `enif_sizeof_resource` resolution probe.
///
/// Every object is 8 bytes and its first byte is `42`, so `peek/1` can also refuse a *recycled*
/// block (right address, wrong contents) rather than trust the binding alone.
///
/// Only `enif_*` slots this tree populates are referenced, and none of them is C-variadic, so
/// `dlopen(RTLD_NOW)` succeeds.
const NIF_C: &str = r#"/* Generated by `beam_rt/tests/nif_resource_nested.rs` (never committed). */
#include <erl_nif.h>

#define HELD_MAX 256

static ErlNifResourceType *nested_type;
static int dtor_runs;
static int live_objects;
static void *held[HELD_MAX];
static int held_count;

static void nested_dtor(ErlNifEnv *env, void *obj)
{
    (void)env; (void)obj;
    dtor_runs++;
    live_objects--;
}

/* Mint a term that owns the object's only reference. */
static ERL_NIF_TERM probe(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{
    void *obj;
    ERL_NIF_TERM term;
    (void)argc; (void)argv;
    obj = enif_alloc_resource(nested_type, 8);
    if (obj == NULL) return enif_make_badarg(env);
    *(unsigned char *)obj = 42;
    live_objects++;
    term = enif_make_resource(env, obj);
    /* Drop the allocator's reference: only the term's is left. */
    enif_release_resource(obj);
    return term;
}

/* Mint an object THIS reference keeps alive, and discard the term. No prune and no
 * collection may destroy it, so `live_objects` grows with every call -- the linear
 * control for the flat half. */
static ERL_NIF_TERM probe_held(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{
    void *obj;
    (void)argc; (void)argv;
    if (held_count >= HELD_MAX) return enif_make_int(env, 0);
    obj = enif_alloc_resource(nested_type, 8);
    if (obj == NULL) return enif_make_badarg(env);
    *(unsigned char *)obj = 42;
    live_objects++;
    held[held_count++] = obj;
    (void)enif_make_resource(env, obj); /* the term dies; the C reference remains */
    return enif_make_int(env, 1);
}

/* The anti-dangle probe: `8` when `arg` still resolves to THIS type's live object with its
 * marker byte intact (the payload size `enif_sizeof_resource` reports), else `error`. */
static ERL_NIF_TERM peek(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{
    void *obj = NULL;
    if (argc != 1) return enif_make_badarg(env);
    if (!enif_get_resource(env, argv[0], nested_type, &obj) || obj == NULL)
        return enif_make_atom(env, "error");
    if (*(unsigned char *)obj != 42) return enif_make_atom(env, "error");
    return enif_make_int(env, (int)enif_sizeof_resource(obj));
}

static ERL_NIF_TERM dtor_count(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{ (void)argc; (void)argv; return enif_make_int(env, dtor_runs); }

static ERL_NIF_TERM live_count(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{ (void)argc; (void)argv; return enif_make_int(env, live_objects); }

static int load(ErlNifEnv *env, void **priv_data, ERL_NIF_TERM load_info)
{
    ErlNifResourceFlags tried;
    (void)priv_data; (void)load_info;
    nested_type = enif_open_resource_type(env, NULL, "nested_type", nested_dtor,
                                          ERL_NIF_RT_CREATE|ERL_NIF_RT_TAKEOVER, &tried);
    return nested_type == NULL ? 1 : 0;
}

static ErlNifFunc funcs[] = {
    {"probe", 0, probe}, {"probe_held", 0, probe_held}, {"peek", 1, peek},
    {"dtor_count", 0, dtor_count}, {"live_count", 0, live_count}
};

ERL_NIF_INIT(nif_resource_nested, funcs, load, NULL, NULL, NULL)
"#;

/// The NIF module: `init/1` loads the library, the five probes are the (replaced) stubs.
const NIF_ERL: &str = r#"%% Generated by `beam_rt/tests/nif_resource_nested.rs` (never committed).
-module(nif_resource_nested).
-export([init/1, probe/0, probe_held/0, peek/1, dtor_count/0, live_count/0]).
init(Path) -> erlang:load_nif(Path, 0).
probe() -> erlang:nif_error(not_loaded).
probe_held() -> erlang:nif_error(not_loaded).
peek(_Term) -> erlang:nif_error(not_loaded).
dtor_count() -> erlang:nif_error(not_loaded).
live_count() -> erlang:nif_error(not_loaded).
"#;

/// The caller. Every probe reaches the NIF through an **external** call (a NIF replaces the
/// stub of the same `{module, function, arity}` only when the runtime *dispatches* a call; a
/// local call inside the NIF's own module is resolved in-machine), and every function that
/// exercises the nested site does it through a real `lists:map/2` / `lists:foreach/2` so the
/// fun application is the nested run whose epilogue prunes.
///
/// Each answer is `{<the observable>, dtor_count()}` (or a triple where the test needs a third
/// figure), read **inside** the same function, i.e. before the outer run's own epilogue.
const CALLER_ERL: &str = r#"%% Generated by `beam_rt/tests/nif_resource_nested.rs` (never committed).
-module(nested_caller).
-export([keep_var/1, keep_tuple/1, keep_list/1, mapped/2, dropped/2, held/2, plain/1]).

%% The nested run every keep-test crosses: a `lists:map/2` (a cross-module call whose every
%% fun application is its own nested run) that mints nothing, so only the HELD term's fate is
%% at stake.
nudge(N) -> lists:map(fun(X) -> X + 1 end, lists:seq(1, N)).

%% (A1) Held in a variable across the nested runs: it must SURVIVE and stay usable.
keep_var(Path) ->
    ok = nif_resource_nested:init(Path),
    Res = nif_resource_nested:probe(),
    _ = nudge(20),
    {nif_resource_nested:peek(Res), nif_resource_nested:dtor_count()}.

%% (A2) ... and inside a tuple: the same survival through a boxed container.
keep_tuple(Path) ->
    ok = nif_resource_nested:init(Path),
    Held = {nif_resource_nested:probe()},
    _ = nudge(20),
    {nif_resource_nested:peek(element(1, Held)), nif_resource_nested:dtor_count()}.

%% (A3) ... and inside a list (`[Res|[]]`): the cons-cell shape.
keep_list(Path) ->
    ok = nif_resource_nested:init(Path),
    Held = [nif_resource_nested:probe()],
    _ = nudge(20),
    [Only] = Held,
    {nif_resource_nested:peek(Only), nif_resource_nested:dtor_count()}.

%% (B1) The scope shape: `lists:map/2` over a resource-minting call. Every element was
%% minted by an EARLIER nested run than the one that reads it, so each must still resolve
%% (and none may have run its destructor). The comprehension's filter is itself an external
%% call, i.e. a nested run per element, with that element held across it.
mapped(Path, N) ->
    ok = nif_resource_nested:init(Path),
    Built = lists:map(fun(_) -> nif_resource_nested:probe() end, lists:seq(1, N)),
    Resolved = length([E || E <- Built, nif_resource_nested:peek(E) =:= 8]),
    {Resolved, nif_resource_nested:dtor_count()}.

%% (B2) ... and the companion leg: the SAME loop, but each minted term is dropped inside the
%% fun. Nothing accumulates: the destructors run and the fixture's live-object count is back
%% to zero -- at any N (that is the flat-in-N claim).
%%
%% The three settle steps are for the ORACLE: its `dtor` is deferred misc aux work, so a forced
%% GC has to free the terms and a thread-progress point (`timer:sleep/1`) has to run the queued
%% destructors before the counts mean anything. This runtime releases synchronously at each
%% nested run's epilogue, so for it they change nothing (and `timer:sleep/1` really waits on the
%% `-eval` route: `Runtime::wait_for_timer`).
dropped(Path, N) ->
    ok = nif_resource_nested:init(Path),
    lists:foreach(fun(_) -> _ = nif_resource_nested:probe(), ok end, lists:seq(1, N)),
    _ = erlang:garbage_collect(),
    _ = nudge(50),
    timer:sleep(50),
    {nif_resource_nested:dtor_count(), nif_resource_nested:live_count()}.

%% (C) The linear control: the same loop via `probe_held/0`, whose objects this NIF's own
%% reference keeps alive. No prune and no collection may destroy one.
held(Path, N) ->
    ok = nif_resource_nested:init(Path),
    lists:foreach(fun(_) -> _ = nif_resource_nested:probe_held(), ok end, lists:seq(1, N)),
    {nif_resource_nested:live_count(), nif_resource_nested:dtor_count()}.

%% (D) The probe discriminates: a plain integer is not a resource of this type, while a live
%% minted term is. Without this leg a `peek/1` that always answered `8` would satisfy every
%% anti-dangle assertion above.
plain(Path) ->
    ok = nif_resource_nested:init(Path),
    {nif_resource_nested:peek(42), nif_resource_nested:peek(nif_resource_nested:probe()),
     nif_resource_nested:dtor_count()}.
"#;

/// A built fixture: the scratch directory holding the sources and the compiled artifacts.
struct Fixture {
    /// The scratch directory.
    dir: PathBuf,
}

impl Fixture {
    /// The path handed to `erlang:load_nif/2` — the `.so` **without** its suffix (the loader
    /// appends `.so`).
    fn load_path(&self) -> PathBuf {
        self.dir.join("nif_resource_nested")
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
    support::skip_loudly("nif_resource_nested", reason);
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

/// The host `erl_nif.h` include directory: the one the host `erl` reports, else the `include`
/// sibling of the `erts-*` directory the `erl` on `$PATH` lives under.
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

/// Compiles the fixture (the `.so` and both `.beam`s) into a fresh scratch directory, or `None`
/// after a loud skip when a host facility is missing.
fn build_fixture(label: &str) -> Option<Fixture> {
    let oracle = require_oracle("nif_resource_nested")?;
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
        "beam_rt_nif_resource_nested_{label}_{}_{n}",
        std::process::id()
    ));
    let c_src = dir.join("nif_resource_nested.c");
    if let Err(error) = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&c_src, NIF_C)) {
        skip(&format!("cannot prepare {}: {error}", dir.display()));
        return None;
    }
    let mut erl_paths = Vec::new();
    for (stem, source) in [("nif_resource_nested", NIF_ERL), ("nested_caller", CALLER_ERL)] {
        let path = dir.join(format!("{stem}.erl"));
        if let Err(error) = std::fs::write(&path, source) {
            skip(&format!("cannot write {}: {error}", path.display()));
            return None;
        }
        erl_paths.push(path);
    }
    // The shared object: `cc -shared -fPIC -I<include> -o <dir>/nif_resource_nested.so <c>`.
    let linked = Command::new(&cc)
        .arg("-shared")
        .arg("-fPIC")
        .arg(format!("-I{}", include.display()))
        .arg("-o")
        .arg(dir.join("nif_resource_nested.so"))
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
    // The Erlang modules: `erlc -o <dir> <dir>/nif_resource_nested.erl <dir>/nested_caller.erl`.
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
    let caller = dir.join("nested_caller.beam");
    if !caller.is_file() {
        skip(&format!("erlc produced no {}", caller.display()));
        return None;
    }
    Some(Fixture { dir })
}

/// The **one** call text both runtimes are given: `nested_caller:<function>/<arity>(…)`, with
/// the fixture's load path as its first argument (double-quoted: `erlang:load_nif/2` needs a
/// char list or a binary, and the scratch path carries no `"` or backslash) followed by `extra`.
fn call_text(fixture: &Fixture, function: &str, extra: &[&str]) -> String {
    let mut args = vec![format!("\"{}\"", fixture.load_path().display())];
    args.extend(extra.iter().map(|arg| (*arg).to_string()));
    format!("nested_caller:{function}({})", args.join(", "))
}

/// Runs the call under **this** runtime on the `-eval` route:
/// `-noshell -pa <dir> -eval 'io:format("~w~n",[<call>]).'`, bounded.
///
/// `-pa` goes **before** `-eval` (the code path must already hold the module when the expression
/// runs), exactly as the oracle probe does.
fn invoke_runtime(fixture: &Fixture, function: &str, extra: &[&str]) -> Option<Output> {
    let dir = fixture.dir.display().to_string();
    let expr = format!(
        "io:format(\"~w~n\",[{}]).",
        call_text(fixture, function, extra)
    );
    run_bounded(
        Path::new(env!("CARGO_BIN_EXE_beam_rt")),
        &["-noshell", "-pa", &dir, "-eval", &expr],
    )
}

/// Runs the identical call under the **system** `erl` through the shared bounded probe
/// (`erl -noshell -pa <dir> -eval '<same io:format>' -s init stop`).
fn invoke_oracle(oracle: &Path, fixture: &Fixture, function: &str, extra: &[&str]) -> Option<String> {
    let expr = format!(
        "io:format(\"~w~n\",[{}]).",
        call_text(fixture, function, extra)
    );
    support::oracle_eval(oracle, Some(&fixture.dir), &expr)
}

/// The observable of one call on **both** sides.
struct Observed {
    /// This runtime's printed answer (`~w~n`, trimmed).
    rust: String,
    /// The oracle's printed answer, when it could be measured.
    oracle: Option<String>,
}

/// Runs `function` under this runtime and (when available) under the oracle, asserting this
/// runtime's exit status and returning both answers.
///
/// The exit-status assertion is hard: a run that refused (exit `3`, an `unsupported` capability
/// gap) or raised must be reported as such, never folded into a wrong count.
fn observe(fixture: &Fixture, function: &str, extra: &[&str]) -> Option<Observed> {
    let run = invoke_runtime(fixture, function, extra)?;
    let stdout = String::from_utf8_lossy(&run.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&run.stderr).trim().to_string();
    assert_eq!(
        run.status.code(),
        Some(0),
        "nested_caller:{function} must run to completion; stdout={stdout:?} stderr={stderr:?}"
    );
    let oracle = require_oracle("nif_resource_nested").and_then(|oracle| {
        let answer = invoke_oracle(&oracle, fixture, function, extra);
        if answer.is_none() {
            skip(&format!(
                "the oracle invocation of nested_caller:{function} could not answer"
            ));
        }
        answer
    });
    println!(
        "nif_resource_nested/{function}{extra:?}: rust={stdout} oracle={oracle:?}"
    );
    Some(Observed {
        rust: stdout,
        oracle,
    })
}

/// Asserts this runtime's answer against the expected value and, when the oracle could be
/// measured, against the oracle's own answer.
#[track_caller]
fn assert_matches(observed: &Observed, function: &str, expected: &str) {
    assert_eq!(
        observed.rust, expected,
        "nested_caller:{function} must answer {expected}"
    );
    if let Some(oracle) = &observed.oracle {
        assert_eq!(
            &observed.rust, oracle,
            "nested_caller:{function} must answer what the system ERTS answers"
        );
    }
}

/// The `{integer, integer}` answer, split — the dropped/held legs' shape probe.
fn parse_pair(printed: &str) -> Option<(u64, u64)> {
    let inner = printed.strip_prefix('{')?.strip_suffix('}')?;
    let (first, second) = inner.split_once(',')?;
    Some((first.trim().parse().ok()?, second.trim().parse().ok()?))
}

/// Asserts the **settled** count of a loop that minted `mints` objects, allowing only for the
/// oracle's own *timing*.
///
/// This runtime's release is synchronous at each run's epilogue, so its answer is exact:
/// `{mints,0}` — every destructor ran and nothing is left alive. The system ERTS defers each
/// destructor as misc aux work, so under a loaded host its count can lag **strictly behind** by
/// the objects it has not got to yet; where that happens only the shared **shape** is compared
/// (`dtor + live == mints`: every minted object is either destroyed or still alive, so nothing was
/// leaked and nothing was freed twice). The measured answers agree exactly — `{10,0}` and
/// `{200,0}` on both — which is why the exact comparison comes first.
#[track_caller]
fn assert_settled(observed: &Observed, function: &str, mints: u64) {
    let expected = format!("{{{mints},0}}");
    assert_eq!(
        observed.rust, expected,
        "nested_caller:{function} must answer {expected}"
    );
    let Some(oracle) = &observed.oracle else {
        return;
    };
    if oracle == &observed.rust {
        return;
    }
    let (destroyed, alive) = parse_pair(oracle).unwrap_or_else(|| {
        panic!("nested_caller:{function}: the oracle answered a non-pair {oracle:?}")
    });
    assert_eq!(
        destroyed + alive,
        mints,
        "nested_caller:{function}: the oracle answered {oracle:?}, which does not account for \
         the {mints} minted objects (this runtime answered {:?}); a destroyed+alive total below \
         the mint count would mean an object was destroyed twice, one above it that an object \
         vanished",
        observed.rust
    );
    println!(
        "nested_caller:{function}: the oracle's deferred destructors lag (dtor={destroyed} \
         live={alive}); the shared shape dtor+live=={mints} holds"
    );
}

/// The shared body of one probe: build, run, compare, clean up. `None` means the fixture could
/// not be built (already a loud skip).
fn run_case(label: &str, function: &str, extra: &[&str]) -> Option<Observed> {
    let fixture = build_fixture(label)?;
    let observed = observe(&fixture, function, extra);
    fixture.cleanup();
    let Some(observed) = observed else {
        skip("the beam_rt invocation outlived the probe bound");
        return None;
    };
    Some(observed)
}

// ---------------------------------------------------------------------------------------------
// Test A — anti-dangle: a held resource survives the nested-site prune and stays usable
// ---------------------------------------------------------------------------------------------

/// **(A1)** A resource term held in a **variable** across a `lists:map/2` whose every fun
/// application is its own nested run must survive it: `peek/1` still resolves the term to this
/// type's live object (`8`, the payload size) and `dtor_count/0` is `0`.
///
/// The nested site's frontier — the live-term set at run entry — already lists the held term, so
/// it is retained unconditionally. The other half of the assertion is the anti-dangle one: a
/// release would have freed the object, and `enif_get_resource` would then answer `error`
/// (or the payload's marker byte would be gone).
///
/// Oracle: `erl -noshell -pa DIR -eval 'io:format("~w~n",[nested_caller:keep_var("PATH")]).'
/// answers `{8,0}` — the held term is alive and no destructor has run.
#[test]
fn a_resource_held_in_a_variable_survives_a_nested_map() {
    let Some(observed) = run_case("keep_var", "keep_var", &[]) else {
        return;
    };
    assert_matches(&observed, "keep_var", "{8,0}");
}

/// **(A2)** ... and one held inside a **tuple** survives the same way.
#[test]
fn a_resource_held_in_a_tuple_survives_a_nested_map() {
    let Some(observed) = run_case("keep_tuple", "keep_tuple", &[]) else {
        return;
    };
    assert_matches(&observed, "keep_tuple", "{8,0}");
}

/// **(A3)** ... and one held inside a **list** (`[Res|[]]`) — the cons-cell shape, where both
/// words of the header-less cell are references.
#[test]
fn a_resource_held_in_a_list_survives_a_nested_map() {
    let Some(observed) = run_case("keep_list", "keep_list", &[]) else {
        return;
    };
    assert_matches(&observed, "keep_list", "{8,0}");
}

/// **(A4)** The probe itself discriminates, so the three anti-dangle assertions above cannot be
/// satisfied vacuously: `peek(42)` answers `error` (a plain integer is not a resource of this
/// type) while `peek(probe())` answers `8`.
///
/// The third element is `dtor_count/0` read inside the same function, i.e. before the outer
/// run's epilogue — the term minted for the second `peek` is still live at that point.
#[test]
fn the_resolution_probe_refuses_a_non_resource_and_accepts_a_live_one() {
    let Some(observed) = run_case("plain", "plain", &[]) else {
        return;
    };
    assert_matches(&observed, "plain", "{error,8,0}");
}

// ---------------------------------------------------------------------------------------------
// Test B — nested-site scope: the `lists:map/2` shape that pinned the refc scope
// ---------------------------------------------------------------------------------------------

/// **(B1)** The `lists:map/2` shape. Every element of the built list was minted by a **different,
/// earlier** nested run than the one that reads it back (the comprehension's filter is itself an
/// external call), so an unscoped nested prune — one that released everything its own machine's
/// roots do not name — would free every element: the refc half's measured 190-of-200 corruption,
/// in its resource form.
///
/// `{200,0}`: all `200` elements still resolve through `enif_get_resource`, and no destructor ran
/// for any of them. N = 10 gives the same shape (`{10,0}`, asserted by
/// [`a_small_nested_map_keeps_every_element_reachable`]), so the figure tracks the list, never the
/// runtime.
#[test]
fn every_element_of_a_nested_map_stays_reachable_and_usable() {
    let Some(observed) = run_case("mapped200", "mapped", &["200"]) else {
        return;
    };
    assert_matches(&observed, "mapped", "{200,0}");
}

/// **(B1 small)** The same probe at N = 10 — the flat-in-N half of the scope claim: the answer is
/// the list's own length at either N, with no destructor run.
#[test]
fn a_small_nested_map_keeps_every_element_reachable() {
    let Some(observed) = run_case("mapped10", "mapped", &["10"]) else {
        return;
    };
    assert_matches(&observed, "mapped", "{10,0}");
}

/// **(B2)** The companion leg: the same `lists:foreach/2` loop, but each minted term is dropped
/// inside the fun. Nothing may accumulate — the destructors run for the dropped terms and the
/// fixture's live-object count is back to `0`.
///
/// N = 10 is the small sample; [`dropping_them_in_a_longer_loop_stays_flat`] is the 200-iteration
/// one. Together they are the "bounded, not linear" claim: the figures are the loop's own length
/// and `0`, never `0` and `N`.
///
/// Oracle: `{10,0}` — identical. (`timer:sleep/1` in the fixture is the thread-progress point the
/// **oracle** needs for its deferred destructors; this runtime needs nothing after its epilogue,
/// and both answers are read after the same settle.)
#[test]
fn a_dropped_resource_is_released_at_its_own_runs_epilogue() {
    let Some(observed) = run_case("dropped10", "dropped", &["10"]) else {
        return;
    };
    assert_settled(&observed, "dropped", 10);
}

/// **(B2 large)** ... and at N = 200 the answer is `{200,0}`: the live-object count is **flat in
/// N** (it returns to zero at both sizes), which is what a per-run epilogue gives and a leak does
/// not. Oracle: `{200,0}`, identical.
#[test]
fn dropping_them_in_a_longer_loop_stays_flat() {
    let Some(observed) = run_case("dropped200", "dropped", &["200"]) else {
        return;
    };
    assert_settled(&observed, "dropped", 200);
}

/// **(C)** The non-vacuity control for `live_count/0`: the same loop through `probe_held/0`,
/// whose objects the NIF's own reference keeps alive. No prune and no collection may destroy one,
/// so the counter is **linear** in the call count — `{16,0}` — while a destructor for them never
/// runs.
///
/// Without this leg, the flat `{N,0}` of the two `dropped` tests could be satisfied by a counter
/// that is simply always zero.
///
/// (The oracle answers the same `{16,0}`: its resource blocks are refcounted, so holding a
/// reference really does keep them alive there too.)
#[test]
fn a_reference_the_nif_itself_holds_is_never_reclaimed() {
    let Some(observed) = run_case("held16", "held", &["16"]) else {
        return;
    };
    assert_matches(&observed, "held", "{16,0}");
}

/// The loud-skip guard: with no `erl` on `$PATH` (and no `$ERTS_ORACLE`) the whole suite reports
/// the missing facility instead of passing silently.
///
/// It asserts only the mechanism — [`require_oracle`] prints the banner and answers `None` — so it
/// holds on a machine that *does* have the oracle and on one that does not.
#[test]
fn a_missing_toolchain_is_a_loud_skip_never_a_silent_pass() {
    if require_oracle("nif_resource_nested").is_none() {
        // The loud banner above is this test's report; every other test in the file returns
        // after the same banner rather than asserting against nothing.
        return;
    }
}
