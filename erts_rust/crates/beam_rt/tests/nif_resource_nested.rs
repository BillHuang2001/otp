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
//!   [`a_dropped_resource_is_accounted_for_at_its_own_runs_epilogue`]): the exact `lists:map/2`
//!   shape that pinned the refc scope. Elements built by an earlier nested run are collected into a
//!   returned list, so each must still resolve (and no destructor may run for them); the companion
//!   leg drops each minted term inside the fun, and the objects must be released — flat in N, never
//!   linear.
//!
//! # What the two `dropped` legs assert: exact parity with the oracle
//!
//! `dropped/2` mints `N` resource terms inside a `lists:foreach/2` and drops each one. The prune
//! releases exactly what its own run no longer names, so the answer is `{N,0}` — every minted
//! object destroyed, none retained — at both N = 10 and N = 200, which is what the system ERTS
//! answers. [`assert_dropped`] asserts that equality (plus the soundness invariant
//! `dtor + live == N`, the anti-dangle half through `nif_resource_nested:peek_retained/0`, and a
//! prune that really ran with nothing declined) on the `-eval` route, on the retained-set probe and
//! on the in-process leg; the oracle's own answers (`{10,0}`/`{200,0}` and `{10,0,0}`/`{200,0,0}`)
//! are asserted too, on the identical `.beam`s and `.so`.
//!
//! Zero retention is not an accident of this node's prune: a reference term's identity is its `id`
//! word alone, and **every** locally minted id — `erlang:make_ref/0`, monitors, aliases, timers,
//! `seq_trace` and NIF resource terms — is a draw of the **one** shared monotone space
//! (`beam_common::next_ref_id`). Two producers therefore cannot issue the same id, so a dead
//! resource term can never be word-identical to a live plain reference and the prune's identity
//! tier can never answer "reached" for one.
//!
//! The in-process half — which drives the same expression with no boot script — agrees
//! (`{10,0,0}` / `{200,0,0}` with `prunes` = N, `released` = N and `declines` = 0), so both routes
//! release everything they mint.
//!
//! # Non-vacuity: the linear control
//!
//! "Live objects is bounded after the loop" proves nothing unless the counter *can* grow, so
//! [`a_reference_the_nif_itself_holds_is_never_reclaimed`] drives a variant that keeps its own
//! reference on every object (`probe_held/0`): no prune and no collection may destroy one, so
//! the same counter is linear in the call count. Without that leg a broken counter would make
//! the bounded half pass silently.
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
//! that, and does nothing for this runtime). The settled answers still coincide, so both sides are
//! asserted to the same value — [`assert_dropped`] is the whole of that.
//!
//! # Skip policy
//!
//! Four host facilities are needed (the `erl` oracle, `erlc`, a C compiler and the host
//! `erl_nif.h`). When one is missing the test prints a **loud** banner and returns — it never
//! silently passes and never panics.
//!
//! # Why this file carries its own C fixture instead of extending `fixtures/nif_resource_gc.c`
//!
//! The shared fixture could not gain the `peek/1` and `peek_retained/0` entry points this suite
//! needs: real ERTS **refuses to load** a NIF library whose `ErlNifFunc` table names a function the
//! module does not define (`erts/emulator/beam/erl_nif.c:4876`, `Function not found M:F/A`), and the
//! sibling suite's generated `nif_resource_gc` module exports only `init/1`, `probe/0`,
//! `probe_unexported/0` and `dtor_count/0` — so adding either to that table would break every test
//! in the sibling suite. The library is therefore a separate one (`nif_resource_nested`),
//! embedded here as [`NIF_C`] the way the Erlang fixtures are, so this file stays a single
//! self-contained unit and the sibling's fixture keeps growing independently.
mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_rt::config::RuntimeConfig;
use beam_rt::runtime::Runtime;
use support::{find_on_path, require_oracle, run_bounded, run_bounded_with_env};

/// The C fixture: a resource type whose destructor counts its own runs **and** the live object
/// count, plus two probes (`probe/0` exports a term that owns the object's only reference,
/// `probe_held/0` keeps a C reference so no prune may destroy it), `peek/1`, the
/// `enif_get_resource` + `enif_sizeof_resource` resolution probe, and `peek_retained/0`, the same
/// marker/size check over the objects a prune **retained** (whose terms are gone, so `peek/1` has
/// nothing to resolve).
///
/// Every object is 8 bytes and its first byte is `42`, so either probe can also refuse a
/// *recycled* block (right address, wrong contents) rather than trust the binding alone.
///
/// Only `enif_*` slots this tree populates are referenced, and none of them is C-variadic, so
/// `dlopen(RTLD_NOW)` succeeds.
const NIF_C: &str = r#"/* Generated by `beam_rt/tests/nif_resource_nested.rs` (never committed). */
#include <erl_nif.h>

#define HELD_MAX 256
/* Room for the largest drop leg (200 objects) with margin. `probe/0` registers every live object
 * it mints and the destructor removes its own entry, so the array holds exactly the objects no
 * destructor has run for. */
#define MINTED_MAX 512

static ErlNifResourceType *nested_type;
static int dtor_runs;
static int live_objects;
static void *held[HELD_MAX];
static int held_count;
static void *minted[MINTED_MAX];
static int minted_count;

/* Drop a destroyed object's registry entry first, so `peek_retained/0` can never dereference a
 * freed block: an object leaves the array before its memory is released. */
static void forget_minted(void *obj)
{
    int i;
    for (i = 0; i < minted_count; i++) {
        if (minted[i] == obj) {
            minted[i] = minted[minted_count - 1];
            minted_count--;
            return;
        }
    }
}

static void nested_dtor(ErlNifEnv *env, void *obj)
{
    (void)env;
    forget_minted(obj);
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
    if (minted_count < MINTED_MAX) minted[minted_count++] = obj;
    term = enif_make_resource(env, obj);
    /* Drop the allocator's reference: only the term's is left. */
    enif_release_resource(obj);
    return term;
}
/* Mint an object THIS reference keeps alive, and discard the term. No prune and no
 * collection may destroy it, so `live_objects` grows with every call -- the linear
 * control for the flat half. It is deliberately NOT registered in `minted[]`: that array is
 * `probe/0`'s alone, so `peek_retained/0` counts exactly the objects a drop loop minted and
 * no reference keeps. */
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

/* The anti-dangle probe over what a prune RETAINED: `peek/1`'s marker-byte and payload-size check,
 * applied to every object the destructor has not run for. `peek/1` cannot express it -- a retained
 * object's term is gone, so there is nothing to hand `enif_get_resource` -- and minting one here
 * would bind a fresh resource-term word and perturb the very retention these legs pin
 * (`beam_abi`'s `Registry::bind_term` evicts a binding whose word a new mint reuses), so the check
 * runs on the pointer the registry still owns. Answers how many of them still resolve. */
static ERL_NIF_TERM peek_retained(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{
    int resolved = 0;
    int i;
    (void)argc; (void)argv;
    for (i = 0; i < minted_count; i++) {
        void *obj = minted[i];
        if (*(unsigned char *)obj != 42) continue;
        if ((int)enif_sizeof_resource(obj) != 8) continue;
        resolved++;
    }
    return enif_make_int(env, resolved);
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
    {"peek_retained", 0, peek_retained},
    {"dtor_count", 0, dtor_count}, {"live_count", 0, live_count}
};
ERL_NIF_INIT(nif_resource_nested, funcs, load, NULL, NULL, NULL)
"#;

/// The NIF module: `init/1` loads the library, the six probes are the (replaced) stubs.
const NIF_ERL: &str = r#"%% Generated by `beam_rt/tests/nif_resource_nested.rs` (never committed).
-module(nif_resource_nested).
-export([init/1, probe/0, probe_held/0, peek/1, peek_retained/0, dtor_count/0, live_count/0]).
init(Path) -> erlang:load_nif(Path, 0).
probe() -> erlang:nif_error(not_loaded).
probe_held() -> erlang:nif_error(not_loaded).
peek(_Term) -> erlang:nif_error(not_loaded).
peek_retained() -> erlang:nif_error(not_loaded).
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
-export([keep_var/1, keep_tuple/1, keep_list/1, mapped/2, dropped/2, retained/2, held/2, plain/1]).
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
%% fun. The answer is `{dtor_count, live_count}`: the oracle's is a destructor per mint with
%% nothing left alive, while this runtime destroys part of the loop and RETAINS the rest (a
%% leak, never a dangle -- see the suite's `dropped` legs and `M1_GAPS.md`), so the counts are
%% checked for the shared shape `dtor + live == N` rather than for a zero live count.
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

%% (B2 pin) The same loop, answering the retained-set probe as well:
%% `{dtor_count, live_count, peek_retained()}`. Every object the prune retained must still carry
%% its marker payload, so a release that destroyed the C object yet kept the bookkeeping in place
%% cannot pass. The probe is read INSIDE this function, i.e. before the outer run's own epilogue
%% touches the retained set.
retained(Path, N) ->
    ok = nif_resource_nested:init(Path),
    lists:foreach(fun(_) -> _ = nif_resource_nested:probe(), ok end, lists:seq(1, N)),
    _ = erlang:garbage_collect(),
    _ = nudge(50),
    timer:sleep(50),
    {nif_resource_nested:dtor_count(), nif_resource_nested:live_count(),
     nif_resource_nested:peek_retained()}.
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

/// The `{integer, integer, integer}` answer of [`CALLER_ERL`]'s `retained/2`, split — the
/// dropped legs' `{dtor, live, resolved}` probe.
fn parse_triple(printed: &str) -> Option<(u64, u64, u64)> {
    let inner = printed.strip_prefix('{')?.strip_suffix('}')?;
    let mut fields = inner.split(',');
    let first = fields.next()?.trim().parse().ok()?;
    let second = fields.next()?.trim().parse().ok()?;
    let third = fields.next()?.trim().parse().ok()?;
    fields.next().is_none().then_some((first, second, third))
}

// ---------------------------------------------------------------------------------------------
// The dropped legs' in-process half: the fixture's counters, the retained-set probe and the
// runtime's own prune counters (which are runtime state, not observable output)
// ---------------------------------------------------------------------------------------------

/// The environment variable that tells [`dropped_worker_leg`] which drop loop to drive.
const DROPPED_ENV: &str = "NIF_RESOURCE_NESTED_DROP_N";

/// The worker leg's name (its caller spawns it as `--exact <name> --ignored`).
const DROPPED_WORKER: &str = "dropped_worker_leg";

/// The prefix of the worker's one report line.
const DROPPED_PREFIX: &str = "nif_resource_nested/dropped_leg:";

/// One in-process drop leg: the fixture's own counters, the retained-set probe's count and the
/// runtime's prune counters.
#[derive(Debug, Clone)]
struct DropLeg {
    /// How many objects the loop minted.
    mints: u64,
    /// `dtor_count/0` — the destructors that ran.
    destroyed: u64,
    /// `live_count/0` — the objects still alive.
    live: u64,
    /// `peek_retained/0` — how many of those still resolve to their marker payload.
    resolved: u64,
    /// `ResourcePruneStats::prunes`.
    prunes: u64,
    /// `ResourcePruneStats::declines`.
    declines: u64,
    /// `ResourcePruneStats::released`.
    released: u64,
    /// `ResourcePruneStats::live_terms`.
    live_terms: usize,
}

/// Reads the worker's `DROPPED_PREFIX` record out of its captured output.
///
/// The stream is flattened first (the harness interleaves its own progress line, and `~w` output
/// may itself carry a line break), so the parse depends on the field names only: the record ends at
/// the first token that is not a `key=value` field (libtest appends `ok`, `test result: ok. …`).
fn parse_drop_leg(stdout: &str) -> Option<DropLeg> {
    let flattened = stdout.replace('\n', " ");
    let start = flattened.find(DROPPED_PREFIX)?;
    let line = &flattened[start + DROPPED_PREFIX.len()..];
    let mut mints = None;
    let mut answer = None;
    let mut prunes = None;
    let mut released = None;
    let mut declines = None;
    let mut live_terms = None;
    for field in line.split_whitespace() {
        let Some((key, value)) = field.split_once('=') else {
            break; // the first non-`key=value` token ends the record
        };
        let number = |value: &str| -> Option<u64> {
            value
                .parse()
                .map_err(|error| println!("dropped_leg field {key}={value}: {error}"))
                .ok()
        };
        match key {
            "n" => mints = number(value),
            "answer" => answer = Some(value.to_string()),
            "prunes" => prunes = number(value),
            "released" => released = number(value),
            "declines" => declines = number(value),
            "live_terms" => live_terms = number(value).map(|n| n as usize),
            other => println!("dropped_leg: ignoring unknown field {other}"),
        }
    }
    let (destroyed, live, resolved) = parse_triple(&answer?)?;
    Some(DropLeg {
        mints: mints?,
        destroyed,
        live,
        resolved,
        prunes: prunes?,
        declines: declines?,
        released: released?,
        live_terms: live_terms?,
    })
}

/// A fresh `-eval` runtime over the fixture directory, with the resource prune **enabled** (the
/// default, stated explicitly so a leg can never silently measure a disabled prune).
fn eval_runtime(fixture: &Fixture) -> Runtime {
    let config = RuntimeConfig::default().with_code_paths(vec![fixture.dir.clone()]);
    let mut runtime = Runtime::new(config).expect("the eval runtime");
    runtime.set_resource_prune_enabled(true);
    runtime
}

/// How many resource terms the **process-global** registry holds right now, through the public
/// runtime view.
///
/// Both in-process legs need this to be `0` before they start: a resource term's mint block stands
/// on the running call's pooled BIF heap, so a *fresh* `Runtime` in the same process mints at the
/// very words its predecessor did and `Registry::bind_term` evicts a binding a new mint reuses —
/// with a dirty registry the leg would measure the previous leg's state instead of its own. Each
/// leg therefore runs in a child process of its own ([`run_drop_leg`]).
fn live_resource_terms() -> usize {
    Runtime::new(RuntimeConfig::default())
        .expect("the probe runtime")
        .resource_prune_stats()
        .live_terms
}

/// Runs the drop loop **in-process** in a child of this test binary and returns its figures.
///
/// The child indirection is not politeness: `beam_abi`'s resource-term registry is
/// **process-global**, so two legs sharing a process would evict each other's retained terms and
/// move the counters being measured. One process per leg keeps every figure its own.
///
/// `None` after a loud skip when the child could not report (a missing toolchain already skipped in
/// the caller; an unusable worker is reported rather than folded into a pass).
fn run_drop_leg(mints: u64) -> Option<DropLeg> {
    let Some(exe) = std::env::current_exe().ok() else {
        skip("the test binary's own path is unavailable, so the in-process leg cannot run");
        return None;
    };
    let n = mints.to_string();
    let args = [
        "--exact",
        DROPPED_WORKER,
        "--ignored",
        "--nocapture",
        "--test-threads",
        "1",
    ];
    let Some(run) = run_bounded_with_env(&exe, &args, &[(DROPPED_ENV, Some(n.as_str()))]) else {
        skip(&format!(
            "the in-process drop leg for N = {mints} outlived the {}s probe bound",
            support::PROBE_TIMEOUT.as_secs()
        ));
        return None;
    };
    // Both streams: the child's libtest harness routes its single leg's `--nocapture` output to
    // either, and a failed child is reported with everything it printed.
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(
        run.status.success(),
        "the in-process drop leg for N = {mints} failed ({:?}): {printed}",
        run.status
    );
    let leg = parse_drop_leg(&printed);
    if leg.is_none() {
        skip(&format!(
            "the in-process drop leg for N = {mints} printed no report: {printed}"
        ));
    }
    leg
}

/// **The in-process half of the two `dropped` legs** — never run on its own (`#[ignore]`d; each
/// leg spawns it in a child process of its own through [`run_drop_leg`]).
///
/// It drives `nested_caller:retained/2` on the **`-eval`** route — the same route the CLI legs
/// ([`observe`]) drive — and prints the fixture's counters, the retained-set probe's count and the
/// runtime's prune counters as one `key=value` record. The figures that make the legs falsifiable
/// (`ResourcePruneStats`) are runtime state rather than observable output, which is why this half
/// has to run in-process at all.
#[test]
#[ignore = "the in-process half of the dropped legs; each leg runs it in a child process of its own"]
fn dropped_worker_leg() {
    let Some(mints) = std::env::var(DROPPED_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    else {
        println!("{DROPPED_PREFIX} no {DROPPED_ENV} in the environment; the calling leg reports the skip");
        return;
    };
    let Some(fixture) = build_fixture(&format!("dropped_worker_{mints}")) else {
        return;
    };
    let dirty = live_resource_terms();
    if dirty > 0 {
        skip(&format!(
            "the process-global NIF-resource registry already holds {dirty} term(s): this leg needs \
             a clean process"
        ));
        fixture.cleanup();
        return;
    }
    let mut runtime = eval_runtime(&fixture);
    let call = call_text(&fixture, "retained", &[&mints.to_string()]);
    let value = runtime
        .eval(&call)
        .unwrap_or_else(|error| panic!("nested_caller:retained({mints}) failed: {error:?}"));
    let answer = runtime
        .format_term_w(value, None)
        .expect("the retained leg's answer formats")
        .trim()
        .to_string();
    let stats = runtime.resource_prune_stats();
    println!(
        "{DROPPED_PREFIX} n={mints} answer={answer} prunes={} released={} declines={} \
         live_terms={}",
        stats.prunes, stats.released, stats.declines, stats.live_terms
    );
    fixture.cleanup();
}

/// Asserts a **`dropped` leg** against the oracle's exact answer: every minted object destroyed,
/// nothing retained.
///
/// The leg mints `mints` objects inside a `lists:foreach/2` and drops every term, so the answer is
/// `{mints, 0}` — the system ERTS's answer at both N = 10 and N = 200. Asserted in order:
///
/// 1. **Soundness** — `dtor + live == mints` on the CLI figure *and* on the in-process one: every
///    minted object is either destroyed or still alive, so nothing was released twice and nothing
///    vanished. (A weaker invariant than 2, kept because its failure mode — a double release or a
///    lost object — reads differently from retention.)
/// 2. **Exact parity** — `(dtor, live) == (mints, 0)` on the CLI figure, on the retained-set probe
///    *and* on the in-process leg. Retention is now a hard failure, not a bounded ratchet: every
///    locally minted reference id is a draw of the one shared space
///    ([`beam_common::next_ref_id`]), so a dead resource term can never be word-identical to a live
///    reference and the prune's identity tier can never answer "reached" for one.
/// 3. **Anti-dangle, exact** — `peek_retained() == live`: every still-live object (there are none)
///    would still have to resolve to its `42` marker payload at its own address, so a regression
///    that freed the C object while keeping its bookkeeping cannot pass — and the check is
///    non-vacuous because the same probe answers non-zero in
///    [`a_reference_the_nif_itself_holds_is_never_reclaimed`].
/// 4. **The prune ran** — `prunes > 0` (the walk was not merely absent, which would make the zeros
///    vacuous) and `declines == 0` (nothing was refused; a decline is the other way to retain).
/// 5. **The oracle agrees exactly** — the identical `.beam`/`.so`/expression under the system `erl`
///    answers `{mints,0}` (and the retained probe `{mints,0,0}`), which is asserted rather than
///    merely recorded.
///
/// `retained` is the same loop's `{dtor, live, resolved}` answer ([`CALLER_ERL`]'s `retained/2`).
/// `leg` is the in-process half ([`run_drop_leg`]).
#[track_caller]
fn assert_dropped(
    observed: &Observed,
    retained: &Observed,
    leg: &DropLeg,
    function: &str,
    mints: u64,
) {
    let (destroyed, live) = parse_pair(&observed.rust).unwrap_or_else(|| {
        panic!(
            "nested_caller:{function}: this runtime answered a non-pair {:?}",
            observed.rust
        )
    });
    let (probe_destroyed, probe_live, probe_resolved) =
        parse_triple(&retained.rust).unwrap_or_else(|| {
            panic!(
                "nested_caller:{function}: this runtime answered a non-triple {:?} for the \
                 retained-set probe",
                retained.rust
            )
        });
    assert_eq!(
        leg.mints, mints,
        "nested_caller:{function}: the in-process leg drove N = {} instead of {mints}",
        leg.mints
    );
    // 1. Soundness: nothing released twice, nothing lost — on every figure the leg produced.
    for (side, destroyed, live) in [
        ("the `-eval` route", destroyed, live),
        ("the retained-set probe", probe_destroyed, probe_live),
        ("the in-process leg", leg.destroyed, leg.live),
    ] {
        assert_eq!(
            destroyed + live,
            mints,
            "nested_caller:{function}: {side} answered dtor({destroyed}) + live({live}), which does \
             not account for the {mints} minted objects; a total below the mint count would mean an \
             object was destroyed twice, one above it that an object vanished"
        );
    }
    // 2. Exact parity: every minted object destroyed, none retained — on the CLI figure, on the
    //    retained-set probe and on the in-process leg alike.
    for (side, destroyed, live) in [
        ("the `-eval` route", destroyed, live),
        ("the retained-set probe", probe_destroyed, probe_live),
        ("the in-process leg", leg.destroyed, leg.live),
    ] {
        assert_eq!(
            (destroyed, live),
            (mints, 0),
            "nested_caller:{function}: {side} must destroy all {mints} minted resources and retain \
             none — the system ERTS's answer; it destroyed {destroyed} and retained {live} \
             (in-process leg: {leg:?})"
        );
    }
    // 3. Anti-dangle, exact: every still-live object still resolves to its marker payload at its
    //    own address. With nothing retained this is `0 == 0`; the same probe answering non-zero in
    //    `a_reference_the_nif_itself_holds_is_never_reclaimed` is what keeps it non-vacuous.
    assert_eq!(
        probe_resolved, probe_live,
        "nested_caller:{function}: all {probe_live} still-live resources must still resolve through \
         the fixture's marker probe, but only {probe_resolved} did"
    );
    assert_eq!(
        leg.resolved, leg.live,
        "nested_caller:{function}: the in-process leg's {} still-live resource(s) must all resolve \
         ({} did)",
        leg.live, leg.resolved
    );
    // 4. The prune really ran, and nothing was refused.
    assert!(
        leg.prunes > 0,
        "nested_caller:{function}: the resource prune must run a walk at least once, or the zero \
         retention is vacuous: {leg:?}"
    );
    assert_eq!(
        leg.declines, 0,
        "nested_caller:{function}: nothing may decline here: {leg:?}"
    );
    // 5. The oracle answers the same thing, exactly.
    let Some(oracle) = &observed.oracle else {
        skip(&format!(
            "nested_caller:{function}: the oracle could not be measured, so the exact-parity half of \
             this leg did not run"
        ));
        return;
    };
    let (oracle_destroyed, oracle_live) = parse_pair(oracle).unwrap_or_else(|| {
        panic!("nested_caller:{function}: the oracle answered a non-pair {oracle:?}")
    });
    assert_eq!(
        (oracle_destroyed, oracle_live),
        (mints, 0),
        "nested_caller:{function}: the oracle answered {oracle:?}, which is not the expected \
         {{{mints},0}}"
    );
    if let Some(probe_oracle) = &retained.oracle {
        assert_eq!(
            parse_triple(probe_oracle),
            Some((mints, 0, 0)),
            "nested_caller:{function}: the oracle's retained-set probe must answer \
             {{{mints},0,0}}, got {probe_oracle:?}"
        );
    }
    println!(
        "nested_caller:{function}: exact parity with the oracle — dtor/live={destroyed}/{live} of \
         {mints} (retained-set probe {probe_destroyed}/{probe_live}/{probe_resolved}, oracle \
         {retained_oracle:?}), in-process dtor/live={in_dtor}/{in_live}, prunes={prunes} \
         declines={declines} released={released} live_terms={live_terms}",
        retained_oracle = retained.oracle,
        in_dtor = leg.destroyed,
        in_live = leg.live,
        prunes = leg.prunes,
        declines = leg.declines,
        released = leg.released,
        live_terms = leg.live_terms,
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
/// inside the fun, so the only question left is what became of the objects.
///
/// The answer is the oracle's exactly — `{10,0}`: every minted object is destroyed and none is
/// retained. [`assert_dropped`] asserts that on the `-eval` route, on the retained-set probe and on
/// the in-process leg alike, together with the soundness invariant `dtor + live == N`, the
/// anti-dangle half (every still-live object — there are none — would have to resolve through
/// `nif_resource_nested:peek_retained/0`), and a prune that really ran with nothing declined.
///
/// N = 10 is the small sample; [`dropping_them_in_a_longer_loop_keeps_the_retention_bounded`] is the
/// 200-iteration one. Together they are the "flat in N" claim: every minted object is released at
/// either size.
///
/// Oracle: `{10,0}`, asserted as an exact equality. (`timer:sleep/1` in the fixture is the
/// thread-progress point the **oracle** needs for its deferred destructors; this runtime needs
/// nothing after its epilogue, and both answers are read after the same settle.)
#[test]
fn a_dropped_resource_is_accounted_for_at_its_own_runs_epilogue() {
    let Some(observed) = run_case("dropped10", "dropped", &["10"]) else {
        return;
    };
    let Some(retained) = run_case("dropped10_probe", "retained", &["10"]) else {
        return;
    };
    let Some(leg) = run_drop_leg(10) else {
        return;
    };
    assert_dropped(&observed, &retained, &leg, "dropped", 10);
}

/// **(B2 large)** ... and at N = 200 the same conditions hold with the same answer, `{200,0}`:
/// the retention is zero at both sizes, so the figure is flat in N rather than a bounded residue.
///
/// Oracle: `{200,0}`, asserted as an exact equality.
#[test]
fn dropping_them_in_a_longer_loop_keeps_the_retention_bounded() {
    let Some(observed) = run_case("dropped200", "dropped", &["200"]) else {
        return;
    };
    let Some(retained) = run_case("dropped200_probe", "retained", &["200"]) else {
        return;
    };
    let Some(leg) = run_drop_leg(200) else {
        return;
    };
    assert_dropped(&observed, &retained, &leg, "dropped", 200);
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
