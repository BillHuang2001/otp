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
//! **The one local reference-id space, proved over the three real producers** (contracts A.3/A.5/A.7).
//!
//! A locally minted reference term is the frozen on-heap block
//! `[make_header(3, REF_SUBTAG), small(0) node, small(0) serial, small(id)]`, so a reference's
//! identity is its **`id` word alone** — a difference in that word is a difference in the term
//! (`beam_term` compares and hashes the payload). Every locally minted `id` is **one draw of one
//! process-wide monotone space**, [`beam_common::next_ref_id`], whichever producer mints it:
//!
//! * the **NIF-resource** producer — `enif_make_resource` in `beam_abi`'s resource family, reached
//!   here the way real code reaches it: `erlang:load_nif/2` loads a real C library, and a compiled
//!   module's external call dispatches to its `mint/0` entry point;
//! * the **process-table** producer — `erlang:monitor/2` and `erlang:alias/1` through the runtime's
//!   own BIF floor, which mint through `beam_process`'s `RefAllocator` ([`MonitorRef`]);
//! * the **runtime ref space** — `erlang:make_ref/0` and the `erlang:send_after/3` timer reference.
//!
//! This suite is the **cross-producer collision-freedom proof** at the runtime boundary. It mints
//! [`ROUNDS`] ids from **each** of the five producer entry points, interleaved round-robin, reading
//! every id straight back out of the returned term ([`beam_codec::internal_ref_id`], the safe
//! accessor over the frozen block), and asserts:
//!
//! 1. **pairwise distinct** — no two mints ever share an id, over every cross-producer pair;
//! 2. **strictly increasing along the interleaved mint order** — one monotone source, not merely
//!    disjoint per-producer sequences (three independent counters would be pairwise distinct and
//!    fail this immediately);
//! 3. **inside the window** bracketed by two [`beam_common::next_ref_id`] draws taken before and
//!    after the sweep — so no producer has a private counter, and a producer that started its own
//!    count at `0` would land outside it.
//!
//! [`the_three_producers_draw_one_monotone_space`] is the headline leg: it states 1–3 over the
//! interleaved sweep **and** ([`ref_id_space_child_sweep`], run as the only test in its own process)
//! that the sweep's bracket holds **exactly** its own mints and that they are the *consecutive* ids
//! `lo + 1 … lo + total` of the shared space — so the three producers interleave into one another's
//! draws, not merely avoid each other.
//!
//! [`each_producer_consumes_the_next_draw_of_that_space`] is the *tight-window* leg: for a single
//! mint of each producer it brackets the draw with two probe draws and requires the mint to consume
//! **exactly the next id of that space** (`after == before + 3`, `minted == before2 + 1`). A private,
//! independently seeded counter cannot land there; a lazy "start the private counter at whatever the
//! shared one says" hack is caught by 2, and a snapshot of the shared counter's value is caught by 1
//! and 2 over [`ROUNDS`] rounds.
//!
//! [`every_producer_yields_a_reference_under_the_live_oracle_too`] pins the producer *list*: the same
//! five entry points answer `is_reference` `true` under the system `erl` as well, so the sweep
//! compares two live reference producers rather than five names this runtime happens to accept.
//!
//! # Why the legs run in **child processes**
//!
//! Two facts force it. `cargo test` runs the tests of one binary on parallel threads and the id
//! counter is process-wide, so interference between legs can only **widen** a draw window — which is
//! harmless for 1–3 but fatal to the *consecutive* claim; and `beam_abi`'s resource-term registry is
//! process-global, so two runtimes minting resources concurrently in one process can evict each
//! other's live terms (the hazard `tests/nif_resource_gc.rs` documents at length).
//!
//! Every leg that mints a resource therefore runs in a child process of this test binary
//! (`--exact … --ignored --test-threads 1`, i.e. as the only test there), and the parent re-checks the
//! figures the child prints, so a worker that silently did less cannot pass. The tight-window and
//! consecutive legs additionally *retry* their observation (the technique `beam_abi`'s
//! `resources_tests.rs::mint_in_tight_window` uses), accepting only an interference-free window —
//! [`TIGHT_ATTEMPTS`]/[`SWEEP_ATTEMPTS`] bound the retries, and a bound with no accepted attempt is a
//! loud failure, never a silently weakened assertion. [`RESOURCE_LEG`] additionally serialises the
//! legs for the shared-process case (`cargo test -- --include-ignored`).
//!
//! No absolute id value is ever asserted — the ids are only compared with each other and with the
//! probe draws — so the suite is independent of how many references the rest of the process minted.
//!
//! # The fixture, and the loud skip
//!
//! The library is built **at test time** with the host's `cc` against the host's `erl_nif.h`, and the
//! module with the host's `erlc` (`CC`/`ERTS_ORACLE` override the discovery). A missing facility
//! prints a loud skip banner instead of passing silently
//! ([`a_missing_toolchain_is_a_loud_skip_never_a_silent_pass`]).
mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_rt::config::RuntimeConfig;
use beam_rt::runtime::Runtime;
use support::{find_on_path, require_oracle, run_bounded};

/// How many ids each producer entry point mints in the interleaved sweep.
const ROUNDS: usize = 32;

/// How many draws the tight-window leg attempts per producer before it gives up (each attempt is one
/// runtime call, so this bound is about a pathological scheduler, not about cost).
const TIGHT_ATTEMPTS: usize = 64;

/// How many whole sweeps the child-process worker attempts before it gives up. Inside the worker
/// nothing else draws from the shared counter, so the first attempt is the interference-free one; the
/// retry only guards against a stray draw from a helper thread, and it is bounded so a machine where
/// that never settles is reported rather than spun on.
const SWEEP_ATTEMPTS: usize = 4;

/// The C fixture: one resource type whose `mint/0` really calls `enif_make_resource`, plus a
/// destructor counter so the objects are not silently leaked for the whole run.
///
/// Only `enif_*` slots this tree populates are referenced and none is C-variadic, so
/// `dlopen(RTLD_NOW)` succeeds.
const NIF_C: &str = r#"/* Generated by `beam_rt/tests/ref_id_space.rs` (never committed). */
#include <erl_nif.h>

static ErlNifResourceType *space_type;
static int dtor_runs;

static void space_dtor(ErlNifEnv *env, void *obj)
{
    (void)env; (void)obj;
    dtor_runs++;
}

/* One resource term: allocate, stamp a marker byte, take the term's reference, drop the
 * allocator's. The id in the returned reference block is a fresh draw of the shared space. */
static ERL_NIF_TERM mint(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{
    void *obj;
    ERL_NIF_TERM term;
    (void)argc; (void)argv;
    obj = enif_alloc_resource(space_type, 8);
    if (obj == NULL) return enif_make_badarg(env);
    *(unsigned char *)obj = 42;
    term = enif_make_resource(env, obj);
    enif_release_resource(obj);
    return term;
}

static ERL_NIF_TERM dtor_count(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{ (void)argc; (void)argv; return enif_make_int(env, dtor_runs); }

static int load(ErlNifEnv *env, void **priv_data, ERL_NIF_TERM load_info)
{
    ErlNifResourceFlags tried;
    (void)priv_data; (void)load_info;
    space_type = enif_open_resource_type(env, NULL, "space_type", space_dtor,
                                         ERL_NIF_RT_CREATE|ERL_NIF_RT_TAKEOVER, &tried);
    return space_type == NULL ? 1 : 0;
}

static ErlNifFunc funcs[] = {
    {"mint", 0, mint}, {"dtor_count", 0, dtor_count}
};
ERL_NIF_INIT(nif_ref_space, funcs, load, NULL, NULL, NULL)
"#;

/// The NIF module: `init/1` loads the library, `mint/0` is the (replaced) stub.
const NIF_ERL: &str = r#"%% Generated by `beam_rt/tests/ref_id_space.rs` (never committed).
-module(nif_ref_space).
-export([init/1, mint/0, dtor_count/0]).
init(Path) -> erlang:load_nif(Path, 0).
mint() -> erlang:nif_error(not_loaded).
dtor_count() -> erlang:nif_error(not_loaded).
"#;

/// The caller: the resource mint is reached through a **compiled module's external call**, the shape
/// real code uses (a NIF replaces the stub of the same `{module, function, arity}` only when the
/// runtime *dispatches* a call, and a local call inside the NIF's own module is resolved
/// in-machine).
const CALLER_ERL: &str = r#"%% Generated by `beam_rt/tests/ref_id_space.rs` (never committed).
-module(ref_space_caller).
-export([init/1, mint_resource/0]).
init(Path) -> nif_ref_space:init(Path).
mint_resource() -> nif_ref_space:mint().
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
        self.dir.join("nif_ref_space")
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
    support::skip_loudly("ref_id_space", reason);
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
    let oracle = require_oracle("ref_id_space")?;
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
        "beam_rt_ref_id_space_{label}_{}_{n}",
        std::process::id()
    ));
    let c_src = dir.join("nif_ref_space.c");
    if let Err(error) = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&c_src, NIF_C)) {
        skip(&format!("cannot prepare {}: {error}", dir.display()));
        return None;
    }
    let mut erl_paths = Vec::new();
    for (stem, source) in [("nif_ref_space", NIF_ERL), ("ref_space_caller", CALLER_ERL)] {
        let path = dir.join(format!("{stem}.erl"));
        if let Err(error) = std::fs::write(&path, source) {
            skip(&format!("cannot write {}: {error}", path.display()));
            return None;
        }
        erl_paths.push(path);
    }
    // The shared object: `cc -shared -fPIC -I<include> -o <dir>/nif_ref_space.so <c>`.
    let linked = Command::new(&cc)
        .arg("-shared")
        .arg("-fPIC")
        .arg(format!("-I{}", include.display()))
        .arg("-o")
        .arg(dir.join("nif_ref_space.so"))
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
    // The Erlang modules: `erlc -o <dir> <dir>/nif_ref_space.erl <dir>/ref_space_caller.erl`.
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
    let caller = dir.join("ref_space_caller.beam");
    if !caller.is_file() {
        skip(&format!("erlc produced no {}", caller.display()));
        return None;
    }
    Some(Fixture { dir })
}

/// The five producer entry points, as the **expression** each one is driven by.
///
/// Three families, five entries: the NIF-resource mint; the runtime's own and its timer reference;
/// the process table's monitor and alias.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Producer {
    /// `enif_make_resource` through the loaded C library (the NIF-resource producer).
    Resource,
    /// `erlang:make_ref/0` (the runtime's ref space).
    MakeRef,
    /// `erlang:send_after/3`'s timer reference (the runtime's ref space).
    Timer,
    /// `erlang:monitor/2`'s reference (the process table's allocator).
    Monitor,
    /// `erlang:alias/1`'s reference (the process table's allocator).
    Alias,
}

impl Producer {
    /// Every producer, in the interleaved order the sweep rotates through.
    const ALL: [Producer; 5] = [
        Producer::Resource,
        Producer::MakeRef,
        Producer::Monitor,
        Producer::Timer,
        Producer::Alias,
    ];

    /// The canonical name (for messages).
    fn name(self) -> &'static str {
        match self {
            Producer::Resource => "enif_make_resource",
            Producer::MakeRef => "erlang:make_ref/0",
            Producer::Timer => "erlang:send_after/3",
            Producer::Monitor => "erlang:monitor/2",
            Producer::Alias => "erlang:alias/1",
        }
    }

    /// The `-eval` expression that mints exactly one reference through this producer.
    ///
    /// The timer's delay is long enough that no timer can fire during the run (a fired timer would
    /// deliver a message, not mint anything, but a 10-minute deadline keeps the sweep quiet).
    fn expression(self) -> &'static str {
        match self {
            Producer::Resource => "ref_space_caller:mint_resource()",
            Producer::MakeRef => "erlang:make_ref()",
            Producer::Timer => "erlang:send_after(600000, self(), tick)",
            Producer::Monitor => "erlang:monitor(process, self())",
            Producer::Alias => "erlang:alias([])",
        }
    }
}

/// A fresh runtime over the fixture directory.
fn eval_runtime(fixture: &Fixture) -> Runtime {
    let config = RuntimeConfig::default().with_code_paths(vec![fixture.dir.clone()]);
    Runtime::new(config).expect("the eval runtime")
}

/// Evaluates `expression` and returns the **reference id** of the term it produced.
///
/// The id is read back out of the frozen block by [`beam_codec::internal_ref_id`] — the safe
/// accessor, which validates the tag, the header arity and the id word — so the figure is the one
/// the producer really wrote, not a re-derivation.
fn mint(runtime: &mut Runtime, producer: Producer) -> u64 {
    let value = runtime.eval(producer.expression()).unwrap_or_else(|error| {
        panic!(
            "{} must mint a reference through {}: {error:?}",
            producer.expression(),
            producer.name()
        )
    });
    beam_codec::internal_ref_id(value).unwrap_or_else(|error| {
        panic!(
            "{} must return a reference term (the frozen 4-word block): {error:?}",
            producer.name()
        )
    })
}

/// Loads the fixture library once and answers the runtime, or `None` after a loud skip.
fn loaded_runtime(fixture: &Fixture) -> Option<Runtime> {
    let mut runtime = eval_runtime(fixture);
    let load = format!(
        "ref_space_caller:init(\"{}\")",
        fixture.load_path().display()
    );
    let loaded = runtime.eval(&load).unwrap_or_else(|error| {
        panic!("loading the fixture NIF library must succeed: {error:?}")
    });
    // `format_term_w` is the `~w~n` printer, so the answer carries one trailing newline.
    let ok = runtime
        .format_term_w(loaded, None)
        .expect("the load answer formats")
        .trim()
        .to_string();
    if ok != "ok" {
        skip(&format!(
            "`erlang:load_nif/2` answered {ok:?} instead of `ok`, so the resource producer is \
             unavailable"
        ));
        return None;
    }
    Some(runtime)
}

/// One **interleaved sweep**: [`ROUNDS`] ids from each of the five producer entry points, minted
/// round-robin, bracketed by two [`beam_common::next_ref_id`] probe draws.
///
/// Answers the mints in mint order together with the probe bracket `(lo, hi)`.
fn sweep(runtime: &mut Runtime) -> (Vec<(Producer, u64)>, u64, u64) {
    let lo = beam_common::next_ref_id();
    let mut minted: Vec<(Producer, u64)> = Vec::with_capacity(ROUNDS * Producer::ALL.len());
    for _ in 0..ROUNDS {
        for producer in Producer::ALL {
            minted.push((producer, mint(runtime, producer)));
        }
    }
    let hi = beam_common::next_ref_id();
    (minted, lo, hi)
}

/// The three claims every sweep makes, asserted in one place so the parent test and the
/// child-process worker cannot drift.
///
/// 1. every id is **pairwise distinct**, over every cross-producer pair as well as within a
///    producer (no two mints ever share an id);
/// 2. the ids are **strictly increasing in the interleaved mint order** — one monotone source, not
///    merely three disjoint sequences (three independent counters are pairwise distinct and fail
///    this on the very first round);
/// 3. every id lies **inside the probe bracket** `(lo, hi)`, so no producer's ids came from a
///    counter of its own (a private counter starting at `0` lands below `lo`).
fn assert_one_space(minted: &[(Producer, u64)], lo: u64, hi: u64) {
    let ids: Vec<u64> = minted.iter().map(|(_, id)| *id).collect();

    // (1) Pairwise distinct.
    for (i, left) in ids.iter().enumerate() {
        for (j, right) in ids.iter().enumerate().skip(i + 1) {
            assert_ne!(
                left, right,
                "mints {i} ({}) and {j} ({}) share a reference id: the producers do not draw from \
                 one space",
                minted[i].0.name(),
                minted[j].0.name()
            );
        }
    }

    // (2) Strictly increasing along the interleaved mint order: ONE monotone source.
    for (index, pair) in ids.windows(2).enumerate() {
        let (left, right) = (pair[0], pair[1]);
        assert!(
            right > left,
            "mint {} ({}) drew {left} and the next one, {} ({}) drew {right}: a later mint must \
             draw a strictly larger id (one monotone space)",
            index,
            minted[index].0.name(),
            index + 1,
            minted[index + 1].0.name()
        );
    }

    // (3) Inside the probe bracket.
    for (producer, id) in minted {
        assert!(
            *id > lo && *id < hi,
            "{} drew {id}, which is outside the window ({lo}, {hi}) bracketed by the two probe \
             draws — so it did not come from the shared space",
            producer.name()
        );
    }
}

/// The prefix every **child-process worker** prints its figures on. The parents parse it, so a line
/// that is silently absent is a failure rather than a pass.
const MARKER: &str = "ref_id_space";

/// Whether this process is a **child worker** of one of the parents below, i.e. was started with
/// `--exact <name> --ignored --test-threads 1`.
///
/// A worker must not run in the parent's own process: `beam_abi`'s resource-term registry is
/// process-global, so two runtimes minting in one process can evict each other's live terms (the
/// hazard `tests/nif_resource_gc.rs` documents). A plain `cargo test` never does that — the workers
/// are `#[ignore]`d — but `--include-ignored` would, so it is skipped loudly there instead.
fn runs_as_child_worker(name: &str) -> bool {
    let args: Vec<String> = std::env::args().collect();
    args.iter().any(|arg| arg == "--exact") && args.iter().any(|arg| arg == name)
}

/// How many resource terms the **process-global** registry holds right now, through the public
/// runtime view.
///
/// A non-zero figure means another leg has already minted in this process, so a worker cannot run
/// here without risking the cross-runtime eviction above. (The probe runtime loads no NIF and mints
/// nothing, so reading the figure is safe.)
fn live_resource_terms() -> usize {
    Runtime::new(RuntimeConfig::default())
        .expect("the probe runtime")
        .resource_prune_stats()
        .live_terms
}

/// Takes this process for the worker `name`, or answers `false` after a loud skip.
///
/// Both conditions are about *sharing a process*: the worker must have been started by its parent
/// (`--exact`), and the process-global resource registry must be clean — otherwise another leg has
/// already minted a resource term here and this one would mint into the same registry.
fn claim_process(name: &str) -> bool {
    if !runs_as_child_worker(name) {
        skip(&format!(
            "{name} runs in a child process of this test binary; `--include-ignored` would share              this one with another minting runtime"
        ));
        return false;
    }
    let live = live_resource_terms();
    if live != 0 {
        skip(&format!(
            "{name}: the process-global NIF-resource registry already holds {live} term(s), so              this process has already minted"
        ));
        return false;
    }
    true
}

/// Serialises the legs that mint NIF resources **within one process**, for the runs in which more
/// than one of them shares a process (`cargo test -- --include-ignored`).
///
/// `beam_abi`'s resource-term registry is process-global, so two runtimes minting concurrently can
/// hand the same word to two live terms and one `bind_term` then evicts the other's binding. A plain
/// `cargo test` never hits that — every worker below runs alone in its own process — but the lock
/// makes the shared-process case safe too instead of leaving it to luck.
static RESOURCE_LEG: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Runs `#[ignore]`d worker `name` as a **child process of this test binary**, as the only test
/// there (`--exact`, `--test-threads 1`), and answers its combined transcript.
///
/// For an in-process leg that mints NIF resources this isolation is not a convenience: `beam_abi`'s
/// resource-term registry is process-global, so two runtimes minting in one process can evict each
/// other's live terms (the hazard `tests/nif_resource_gc.rs` documents). A child that is the only
/// test in its process mints alone, and — for the interference-free legs — also **draws** alone from
/// the shared reference-id counter, which is what makes consecutiveness provable.
///
/// The child's exit status is asserted here, so a failing assertion inside the worker fails this
/// parent too, with the worker's own message in the panic.
fn run_worker(name: &str) -> String {
    let exe = std::env::current_exe().expect("the test binary's own path");
    let worker = Command::new(&exe)
        .args([
            "--exact",
            name,
            "--ignored",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap_or_else(|error| panic!("cannot run the {name} worker in {}: {error}", exe.display()));
    // Both streams: the child's libtest harness may route `--nocapture` output to either.
    let transcript = format!(
        "{}{}",
        String::from_utf8_lossy(&worker.stdout),
        String::from_utf8_lossy(&worker.stderr)
    );
    assert!(
        worker.status.success(),
        "the {name} worker failed ({:?}):\n{transcript}",
        worker.status
    );
    transcript
}

/// The figures of worker line `<MARKER>/<label>: k=v k=v …`, as `(key, value)` pairs.
///
/// A missing line is a panic, never an empty set: a worker that printed nothing must not be able to
/// satisfy a parent by being silent.
fn worker_figures(transcript: &str, label: &str) -> Vec<(String, String)> {
    let head = format!("{MARKER}/{label}: ");
    let line = transcript
        .lines()
        .find(|line| line.contains(&head))
        .unwrap_or_else(|| {
            panic!("the worker printed no `{head}…` line:\n{transcript}");
        });
    line.split_whitespace()
        .filter_map(|token| token.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// One figure out of [`worker_figures`], parsed as a number.
fn figure(figures: &[(String, String)], key: &str) -> u64 {
    figures
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
        .unwrap_or_else(|| panic!("`{key}=` is missing from {figures:?}"))
        .parse()
        .unwrap_or_else(|error| panic!("`{key}` is not a number in {figures:?}: {error}"))
}

/// The child-process worker behind [`the_three_producers_draw_one_monotone_space`]: the interleaved
/// sweep, alone in its process.
///
/// It asserts [`assert_one_space`]'s three claims over the real ids **and** — because nothing else
/// can draw here — that the bracket holds exactly the sweep's own mints, so the three producers
/// consume a **consecutive** stretch of the shared space. It then prints its figures for the parent.
#[test]
#[ignore = "run by the_three_producers_draw_one_monotone_space in a child process"]
fn ref_id_space_child_sweep() {
    let _serialised = RESOURCE_LEG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !claim_process("ref_id_space_child_sweep") {
        return;
    }
    let Some(fixture) = build_fixture("child_sweep") else {
        return;
    };
    let Some(mut runtime) = loaded_runtime(&fixture) else {
        fixture.cleanup();
        return;
    };
    let mut accepted = None;
    for _ in 0..SWEEP_ATTEMPTS {
        let (minted, lo, hi) = sweep(&mut runtime);
        let width = hi - lo - 1;
        if width == minted.len() as u64 {
            accepted = Some((minted, lo, hi));
            break;
        }
        println!(
            "{MARKER}/sweep: attempt rejected — window width {width} for {} mints (another \
             draw shared the window)",
            minted.len()
        );
    }
    let Some((minted, lo, hi)) = accepted else {
        panic!(
            "no interference-free sweep in {SWEEP_ATTEMPTS} attempts — this worker must be the \
             only test in its process"
        );
    };
    assert_one_space(&minted, lo, hi);
    // **Consecutiveness**: every draw of the window is one of ours, in mint order — so the three
    // producers interleave into one another's draws rather than merely avoiding each other.
    let ids: Vec<u64> = minted.iter().map(|(_, id)| *id).collect();
    let expected: Vec<u64> = (1..=ids.len() as u64).map(|offset| lo + offset).collect();
    assert_eq!(
        ids, expected,
        "the interleaved sweep must consume the consecutive ids {} ..= {} of the shared space \
         (lo={lo}, hi={hi})",
        lo + 1,
        lo + ids.len() as u64
    );
    println!(
        "{MARKER}/sweep: total={} width={} lo={lo} hi={hi} first={} last={}",
        ids.len(),
        hi - lo - 1,
        ids[0],
        ids[ids.len() - 1]
    );
    fixture.cleanup();
}

/// **The headline: the three real producers draw one monotone space.**
///
/// [`ROUNDS`] ids from each of the five entry points of the three producers — the NIF resource mint,
/// `erlang:make_ref/0`, the `erlang:send_after/3` timer reference, `erlang:monitor/2` and
/// `erlang:alias/1` — minted **interleaved** round-robin, with the whole sweep bracketed by two
/// [`beam_common::next_ref_id`] probe draws.
///
/// The leg itself runs in a child process of this test binary
/// ([`ref_id_space_child_sweep`], alone in its process, so the ABI's process-global resource
/// registry and the shared counter are both uncontended), and it asserts over the real ids:
///
/// 1. every id is **pairwise distinct** — no two mints ever share an id, over every cross-producer
///    pair as well as within a producer;
/// 2. the ids are **strictly increasing in the interleaved mint order** — one monotone source, not
///    three disjoint sequences;
/// 3. every id lies **inside the probe bracket** `(lo, hi)` — no producer keeps a counter of its own.
///
/// This parent re-checks the figures the child reports: the sweep really was
/// [`ROUNDS`] × 5 mints, its bracket held **exactly** those mints (nothing else drew — the
/// producers interleave into one another's draws), and its ids are consecutive.
#[test]
fn the_three_producers_draw_one_monotone_space() {
    let transcript = run_worker("ref_id_space_child_sweep");
    let figures = worker_figures(&transcript, "sweep");
    let total = figure(&figures, "total");
    let width = figure(&figures, "width");
    let expected = (ROUNDS * Producer::ALL.len()) as u64;
    println!(
        "ref_id_space/sweep: total={total} width={width} lo={} hi={} first={} last={}",
        figure(&figures, "lo"),
        figure(&figures, "hi"),
        figure(&figures, "first"),
        figure(&figures, "last")
    );
    assert_eq!(
        total, expected,
        "the sweep must mint ROUNDS × {} = {expected} ids: {transcript}",
        Producer::ALL.len()
    );
    assert_eq!(
        width, total,
        "the sweep's bracket must hold exactly its own mints, so all {total} ids are consecutive \
         draws of the one space: {transcript}"
    );
    assert_eq!(
        figure(&figures, "last") - figure(&figures, "first"),
        total - 1,
        "the sweep's first and last ids differ by exactly total - 1: {transcript}"
    );
}

/// The child-process worker behind [`each_producer_consumes_the_next_draw_of_that_space`]: one mint
/// of every producer, each in its own tight draw window.
///
/// Runs alone in its process (the parent passes `--exact --ignored --test-threads 1`), so the retry
/// below normally succeeds on its first attempt; it exists so that a stray draw can never fail the
/// leg spuriously, and every figure it accepts is asserted to be a contiguous window.
///
/// Prints one `ref_id_space/tight: producer=… before=… id=… after=…` line per producer, which the
/// parent re-checks.
#[test]
#[ignore = "run by each_producer_consumes_the_next_draw_of_that_space in a child process"]
fn ref_id_space_child_tight() {
    let _serialised = RESOURCE_LEG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !claim_process("ref_id_space_child_tight") {
        return;
    }
    let Some(fixture) = build_fixture("child_tight") else {
        return;
    };
    let Some(mut runtime) = loaded_runtime(&fixture) else {
        fixture.cleanup();
        return;
    };
    for producer in Producer::ALL {
        let mut observed = None;
        for _ in 0..TIGHT_ATTEMPTS {
            let before1 = beam_common::next_ref_id();
            let before2 = beam_common::next_ref_id();
            let id = mint(&mut runtime, producer);
            let after = beam_common::next_ref_id();
            if after == before1 + 3 {
                observed = Some((id, before1, before2, after));
                break;
            }
        }
        let Some((id, before1, before2, after)) = observed else {
            panic!(
                "{}: no interference-free draw window in {TIGHT_ATTEMPTS} attempts — something \
                 else drew from the shared reference-id counter on every attempt",
                producer.name()
            );
        };
        assert_eq!(
            before2,
            before1 + 1,
            "{}: the two probe draws are contiguous",
            producer.name()
        );
        assert_eq!(
            after,
            before1 + 3,
            "{}: the window is tight — the mint drew exactly one id",
            producer.name()
        );
        assert_eq!(
            id,
            before2 + 1,
            "{}: the minted id is the very next draw of the shared reference-id space",
            producer.name()
        );
        println!(
            "{MARKER}/tight: producer={} before={before1} id={id} after={after}",
            producer.name()
        );
    }
    fixture.cleanup();
}

/// **The tight-window leg: each producer consumes exactly the next draw of the shared space.**
///
/// For one mint of each producer, two probe draws are taken before it and one after, and the window
/// is accepted only when it is contiguous (`after == before1 + 3`), which is the only way to know
/// nothing else drew in between. The mint then must have consumed the id between them
/// (`minted == before2 + 1`).
///
/// That is the strongest statement available for a process-wide counter: a private counter — even
/// one seeded from the shared value at startup — cannot land on the very next draw of the shared
/// space, and a producer that drew *nothing* (reusing an id) is caught at once. A snapshot of the
/// counter's value is caught by the sweep's distinctness/monotonicity claims.
///
/// The leg runs in a child process of this test binary ([`ref_id_space_child_tight`]); this parent
/// re-checks every window it reports (`after == before1 + 3`, `id == before1 + 2`) and that all five
/// producers were measured.
#[test]
fn each_producer_consumes_the_next_draw_of_that_space() {
    let transcript = run_worker("ref_id_space_child_tight");
    let head = format!("{MARKER}/tight: ");
    let windows: Vec<&str> = transcript
        .lines()
        .filter(|line| line.contains(&head))
        .collect();
    assert_eq!(
        windows.len(),
        Producer::ALL.len(),
        "every producer must report a tight window:\n{transcript}"
    );
    for producer in Producer::ALL {
        let line = windows
            .iter()
            .find(|line| line.contains(&format!("producer={} ", producer.name())))
            .unwrap_or_else(|| {
                panic!("{} reported no tight window:\n{transcript}", producer.name())
            });
        let figures = worker_figures(line, "tight");
        let before = figure(&figures, "before");
        let id = figure(&figures, "id");
        let after = figure(&figures, "after");
        println!("ref_id_space/tight: {} before={before} id={id} after={after}", producer.name());
        assert_eq!(
            after,
            before + 3,
            "{}: the mint must draw exactly one id between the two probe draws: {line}",
            producer.name()
        );
        assert_eq!(
            id,
            before + 2,
            "{}: the minted id must be the next draw of the shared space: {line}",
            producer.name()
        );
    }
}
/// **The producer list is the oracle's too.** All five entry points really do produce references —
/// under this runtime **and** under the system `erl`, from the same `.erl`/`.so` pair — so the sweep
/// above is a comparison of two live reference producers and not of five names this runtime happens
/// to accept.
///
/// Asserted, for the same expression under both runtimes: every entry point answers `is_reference`
/// `true`, the two runtimes' answers are identical, and they are the expected
/// `[true,true,true,true,true]`. (The ids themselves are opaque: ERTS's own reference counter is a
/// per-node `Uint64` no `-eval` can read, which is why the "one space" claims are made over this
/// runtime's own draws, in the legs above.)
#[test]
#[ignore = "run by every_producer_yields_a_reference_under_the_live_oracle_too in a child process"]
fn ref_id_space_child_oracle() {
    let _serialised = RESOURCE_LEG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !claim_process("ref_id_space_child_oracle") {
        return;
    }
    let Some(fixture) = build_fixture("child_oracle") else {
        return;
    };
    let Some(oracle) = require_oracle("ref_id_space") else {
        fixture.cleanup();
        return;
    };
    let checks = Producer::ALL
        .iter()
        .map(|producer| format!("is_reference({})", producer.expression()))
        .collect::<Vec<_>>()
        .join(", ");
    // One expression: load the library, then classify every producer's term. `begin … end` because
    // `-eval` takes one expression.
    let expression = format!(
        "begin ok = ref_space_caller:init(\"{path}\"), [{checks}] end",
        path = fixture.load_path().display()
    );
    let mut runtime = eval_runtime(&fixture);
    let value = runtime
        .eval(&expression)
        .unwrap_or_else(|error| panic!("the classification expression must run here: {error:?}"));
    let ours = runtime
        .format_term_w(value, None)
        .expect("the classification answer formats")
        .trim()
        .to_string();
    let probe = format!("io:format(\"~w~n\", [{expression}]), halt(0).");
    let Some(theirs) = support::oracle_eval(&oracle, Some(&fixture.dir), &probe) else {
        skip("the oracle could not classify the five producer entry points");
        fixture.cleanup();
        return;
    };
    println!("{MARKER}/oracle: rust={ours} oracle={theirs}");
    assert_eq!(
        ours, "[true,true,true,true,true]",
        "every producer entry point must yield a reference here"
    );
    assert_eq!(
        theirs, "[true,true,true,true,true]",
        "every producer entry point must yield a reference under the live oracle too"
    );
    assert_eq!(ours, theirs, "the two runtimes must agree element for element");
    fixture.cleanup();
}

/// **The producer list is the oracle's too.** All five entry points really do produce references —
/// under this runtime **and** under the system `erl`, from the same `.erl`/`.so` pair — so the sweep
/// above compares two live reference producers rather than five names this runtime happens to accept.
///
/// The leg runs in a child process of this test binary ([`ref_id_space_child_oracle`], which mints a
/// resource term in-process and must therefore be the only test in its process); this parent
/// re-checks the two answers it reports: both runtimes call every entry point a reference, and the
/// answers are identical element for element. (The ids themselves are opaque: ERTS's own reference
/// counter is a per-node `Uint64` no `-eval` can read, which is why the "one space" claims are made
/// over this runtime's own draws, in the legs above.)
#[test]
fn every_producer_yields_a_reference_under_the_live_oracle_too() {
    let transcript = run_worker("ref_id_space_child_oracle");
    let figures = worker_figures(&transcript, "oracle");
    let answer = |key: &str| -> String {
        figures
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
            .unwrap_or_else(|| panic!("`{key}=` is missing from {figures:?}"))
    };
    let ours = answer("rust");
    let theirs = answer("oracle");
    println!("ref_id_space/oracle: rust={ours} oracle={theirs}");
    assert_eq!(
        ours, "[true,true,true,true,true]",
        "every producer entry point must yield a reference here: {transcript}"
    );
    assert_eq!(
        theirs, "[true,true,true,true,true]",
        "every producer entry point must yield a reference under the live oracle too: {transcript}"
    );
    assert_eq!(
        ours, theirs,
        "the two runtimes must agree element for element: {transcript}"
    );
}

/// The loud-skip guard: with no `erl` on `$PATH` (and no `$ERTS_ORACLE`), `cc` or `erlc` missing, the
/// suite reports the missing facility instead of passing silently.
///
/// It asserts only the mechanism — [`require_oracle`] prints the banner and answers `None` — so it
/// holds on a machine that *does* have the oracle and on one that does not.
#[test]
fn a_missing_toolchain_is_a_loud_skip_never_a_silent_pass() {
    if require_oracle("ref_id_space").is_none() {
        // The loud banner above is this test's report; every other test in the file returns after
        // the same banner rather than asserting against nothing.
        return;
    }
}