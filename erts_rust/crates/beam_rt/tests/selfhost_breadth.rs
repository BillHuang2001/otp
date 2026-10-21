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

//! Contract A.9 (**self-hosted build**) — the **widened** compile corpus: every
//! `<root>/lib/*/src/*.erl` source (772 across 34 applications on this checkout), compiled by the
//! Rust runtime and compared byte-for-byte with the system oracle. The runtime's **in-process**
//! route is the default; `BEAM_RT_BREADTH_SCHEDULED=1` switches the very same corpus selection to
//! the **scheduled** route (the two are described under *The runtime's two compile routes* below).
//!
//! Where `src/context/pump.rs`'s `every_stdlib_source_compiles_in_process_like_the_oracle` sweeps
//! **stdlib only** and `tests/selfhost_chain.rs` proves the *chain* for two hand-picked sources,
//! this suite is the **breadth** measurement: it discovers the whole source corpus from the
//! filesystem, computes the oracle's own compilable subset at run time, and reports (and asserts)
//! the runtime's parity on it.
//!
//! # The two sides (oracle baseline · runtime)
//!
//! * **Oracle baseline** — one `erlc` process per source into that source's own scratch outdir:
//!   `erlc -o D +no_spawn_compiler_process +deterministic +no_docs [-I …] SRC` (the
//!   `+no_spawn_compiler_process` flag is dropped exactly when the runtime's own option is — see
//!   *The runtime's two compile routes* below). `erlc`'s output is byte-identical to the in-VM
//!   `compile:file/2` route with the identical option list (the option list is what `Dbgi` records,
//!   and `{outdir, _}` is not recorded), and one process per source makes the failure attribution
//!   exact: a source the **oracle** cannot compile with this recipe is *excluded from the
//!   baseline*, never counted against the runtime. (The 17 such sources on this checkout need
//!   generated artefacts — `beam_opcodes.hrl`, an ASN.1 `.hrl` set, `-DVSN` — which is why the
//!   baseline is computed rather than assumed.)
//! * **The runtime** — one `beam_rt -noshell -eval '<compile:file(…)>'` process per source: the
//!   real CLI route, so the compile's `epp`/`save_binary` round trips address the booted
//!   `kernel_sup`'s live `file_server_2` exactly as they do for a user invoking the compiler. The
//!   option list is the oracle's, plus `{i, Dir}` for **every** `lib/*/include` directory, so a
//!   source whose headers live outside its own directory is compilable on both sides.
//!
//! # The runtime's two compile routes (`BEAM_RT_BREADTH_SCHEDULED`)
//!
//! `compile:file/2` has two shapes and OTP selects between them with a **single option**:
//!
//! * **in-process** (the default here, and the route `src/context/pump.rs`'s stdlib sweep drives) —
//!   the option list carries `no_spawn_compiler_process`, so the compile runs in the **calling**
//!   process;
//! * **scheduled** (`BEAM_RT_BREADTH_SCHEDULED=1`) — the option is omitted, so OTP's own default
//!   applies and the compiler **spawns a compiler process**, which this runtime schedules, signals
//!   and blocks like any other process.
//!
//! Both are driven through the *same* `beam_rt -noshell -eval '<compile:file(…)>'` invocation; only
//! the option list differs. Both are compared **byte-for-byte against the oracle with the identical
//! option list**: the oracle's `erlc` drops `+no_spawn_compiler_process` exactly when the runtime's
//! option list does, so each run compares like with like. The option list is also the *only* artifact
//! difference the switch induces — `Dbgi` records the compile options, so the two routes' artifacts
//! legitimately differ in that one chunk and (as `beam_lib:cmp/2` confirms) nowhere else. A route's
//! artifact is therefore only ever compared against the oracle's artifact **for that route**, which
//! is what the sweep does: one run drives exactly one route, on both sides.
//!
//! The route names every per-source report line and the summary line (`[in-process]` /
//! `[scheduled]`, `breadth sweep (scheduled route): …`), so a captured log can never be mistaken for
//! the other route — and two runs (one per route) are directly comparable over the same corpus
//! selection, since the filters and the oracle baseline recipe are identical.
//!
//! # Chunk control (all optional; a malformed value is a **loud panic**, never a silently
//! unfiltered run)
//!
//! | Variable | Effect |
//! |---|---|
//! | `BEAM_RT_BREADTH_APPS=kernel,stdlib` | restrict the run to these applications (an unknown name panics) |
//! | `BEAM_RT_BREADTH_SHARD=i/n` | keep the sources whose 0-based index in the filtered, sorted list is `i mod n` |
//! | `BEAM_RT_BREADTH_LIMIT=<n>` | keep the first `n` sources after the two filters above |
//! | `BEAM_RT_BREADTH_MAX_BYTES=<n>` | skip sources whose file is **larger** than `n` bytes, before the shard/limit filters (the skipped count is always reported) |
//! | `BEAM_RT_BREADTH_SCHEDULED=1` | drive the **scheduled** (spawn-a-compiler-process) route; `0`/unset is the in-process route; any other value panics |
//! | `BEAM_RT_BREADTH_PA=a:b:c` | put these directories first on the runtime's code path (one `-pa` group, in the given order): this is how a sweep is run with **the tree's own compiler** (`bootstrap/lib/compiler/ebin:bootstrap/lib/kernel/ebin:bootstrap/lib/stdlib/ebin`) instead of the system one |
//! | `BEAM_RT_BREADTH_COMPARE=bytes\|artifact` | the verdict: `bytes` (default) asserts whole-file byte identity against the oracle; `artifact` asks only that an artifact was produced (the tree's OTP-30 compiler legitimately stamps `opcode_max` 183 where OTP 29 stamps 181, so its output can never be byte-identical) and still reports the byte relation, unasserted |
//!
//! The corpus is sorted by application then module, so a shard (or a `LIMIT`) is a stable,
//! reproducible slice: the shards partition the corpus, and each shard computes its own oracle
//! baseline (so a shard is self-consistent on its own).
//!
//! # What it asserts
//!
//! * **set**: every source the oracle compiles, this runtime compiles too — the *difference* is
//!   asserted, not just the size, because a count can match while the sets differ;
//! * **count**: the route's compile count equals the oracle's compilable count;
//! * **bytes**: every `.beam` both sides produce is byte-identical.
//!
//! # Reporting
//!
//! One stderr line per source on the selected route (`[<route>] [i/n] app/module: <seconds> s,
//! <bytes> B, byte-identical | BYTE MISMATCH at <byte> | <route> FAILED: <reason> | oracle FAILED
//! (excluded): <reason>`), then a `cause × count` failure ledger with one example module per cause,
//! then the failure list, then the asserts. A count is therefore never a bare number: the ledger
//! *is* the residual write-up.
//!
//! # Scale and cost
//!
//! This is a corpus-scale measurement, not an iteration gate — hence `#[ignore]`. The real CLI
//! start-up is ≈0.44 s per source on top of the compile itself, and the heaviest sources
//! (`lists.erl`, `calendar.erl`, …) take tens of seconds under a debug build, so the full corpus is
//! hours of wall time; run a chunk at a time (see the table above) or shard it. The **scheduled**
//! route costs more per source than the in-process one (the compile's work is a spawned process,
//! with the scheduling and message round trips that implies), so every timing this suite prints
//! belongs to a named route and a **shared** host — read it as a route-to-route ratio on one
//! machine, never as a stable absolute figure.
//!
//! The body runs on a dedicated thread with [`SWEEP_STACK_BYTES`], because the deepest sources
//! recurse further than libtest's 2 MiB test thread allows (the real CLI route runs them on the
//! ~8 MiB main thread); the thread's panic payload is propagated with
//! [`std::panic::resume_unwind`], so an assertion message survives the boundary.
//!
//! Every child is bounded by [`SOURCE_DEADLINE`] and **killed and reaped** at the bound, so a hang
//! can never wedge the suite. A missing `erl`/`erlc` or a checkout without `lib/*/src` is a **loud
//! skip** (`support::skip_loudly`), never a silent pass; nothing is ever run to be *discovered*.

// The shared harness is compiled in whole, but this suite uses only its toolchain discovery and its
// loud skip; the bounded runner here takes a per-source deadline of its own (the corpus' heaviest
// sources outlive `support::PROBE_TIMEOUT`), so the unused remainder is expected — the same shape
// `tests/selfhost_chain.rs` and `tests/io_scan.rs` use.
#[allow(dead_code)]
mod support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The executable under test, exactly as the other contract suites drive it.
const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// The suite's banner, used by every loud skip.
const BANNER: &str = "selfhost breadth";

/// The longest a **single** child (one `erlc`, one `beam_rt -eval`) may run before it is killed and
/// reaped.
///
/// Deliberately far longer than [`support::PROBE_TIMEOUT`] (30 s): the corpus' heaviest sources take
/// tens of seconds under a debug build, and the in-crate pump bound is 900 s. 300 s is the
/// per-source deadline the corpus measurement quotes: generous for the slowest real source, short
/// enough that a wedged runtime is reported rather than slept on.
const SOURCE_DEADLINE: Duration = Duration::from_secs(900);

/// How often a running child's exit is polled.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// The stack the sweep body runs with: the deepest `lib/*/src` sources recurse further than
/// libtest's 2 MiB test thread allows, and the real CLI route runs them on an ~8 MiB main thread,
/// so the corpus is given the same order of magnitude with headroom.
const SWEEP_STACK_BYTES: usize = 32 << 20;

// ---------------------------------------------------------------------------------------
// Corpus discovery (filesystem only — a tool is never run to be discovered)
// ---------------------------------------------------------------------------------------

/// One source in the corpus: the application that owns it, its module name (the file stem) and its
/// absolute path.
#[derive(Clone)]
struct Source {
    app: String,
    module: String,
    path: PathBuf,
}

impl Source {
    /// `app/module` — the label every report line uses.
    fn label(&self) -> String {
        format!("{}/{}", self.app, self.module)
    }

    /// The artifact both sides write for this source, inside `outdir`.
    fn artifact(&self, outdir: &Path) -> PathBuf {
        outdir.join(format!("{}.beam", self.module))
    }
}

/// The discovered corpus: every `lib/*/src/*.erl` plus the `lib/*/include` directory set both
/// compiles are given.
struct Corpus {
    /// Every source, sorted by application then module (a stable, shardable order).
    sources: Vec<Source>,
    /// The **application source directories** (`lib/<app>/src`), sorted — the vocabulary
    /// `BEAM_RT_BREADTH_APPS` validates against. An OTP application may keep its Erlang sources in
    /// `src/` subdirectories (`snmp`, `megaco`, …), which the corpus glob does not reach; the
    /// directory is still a valid application, so naming one is never a harness error — it simply
    /// contributes no source here, which the empty-selection guard reports.
    apps: Vec<String>,
    /// Every existing `lib/*/include` directory, sorted.
    include_dirs: Vec<PathBuf>,
}

/// The repository root: `$ERL_TOP` when set, else three levels up from this crate's manifest
/// directory (`<root>/erts_rust/crates/beam_rt`) — the same discovery the in-crate sweep uses.
fn source_root() -> PathBuf {
    match std::env::var_os("ERL_TOP") {
        Some(value) => PathBuf::from(value),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.."),
    }
}

/// Every `<root>/lib/<app>/src/*.erl` (sorted by application then module) plus the
/// `<root>/lib/*/include` set — discovered from the **filesystem** alone. `None` when the tree holds
/// no source at all, which the caller reports as a loud skip.
fn discover(root: &Path) -> Option<Corpus> {
    let lib = root.join("lib");
    let mut apps: Vec<(String, PathBuf)> = std::fs::read_dir(&lib)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter_map(|path| {
            let app = path.file_name()?.to_str()?.to_string();
            Some((app, path))
        })
        .collect();
    apps.sort();

    let mut include_dirs: Vec<PathBuf> = Vec::new();
    let mut app_dirs: Vec<String> = Vec::new();
    let mut sources: Vec<Source> = Vec::new();
    for (app, dir) in &apps {
        let include = dir.join("include");
        if include.is_dir() {
            include_dirs.push(include);
        }
        let src_dir = dir.join("src");
        if !src_dir.is_dir() {
            continue;
        }
        app_dirs.push(app.clone());
        let Ok(entries) = std::fs::read_dir(&src_dir) else {
            continue;
        };
        let mut modules: Vec<Source> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .filter(|path| path.extension().and_then(std::ffi::OsStr::to_str) == Some("erl"))
            .filter_map(|path| {
                let module = path.file_stem()?.to_str()?.to_string();
                Some(Source {
                    app: app.clone(),
                    module,
                    path,
                })
            })
            .collect();
        modules.sort_by(|a, b| a.module.cmp(&b.module));
        sources.extend(modules);
    }
    if sources.is_empty() {
        return None;
    }
    Some(Corpus {
        sources,
        apps: app_dirs,
        include_dirs,
    })
}

// ---------------------------------------------------------------------------------------
// Chunk control (every malformed value is a loud panic)
// ---------------------------------------------------------------------------------------

/// `BEAM_RT_BREADTH_APPS=kernel,stdlib`: the application names to restrict the run to.
///
/// A value naming a directory this checkout has no `lib/<app>/src` for panics: a typo would
/// otherwise select nothing and turn the whole run into a vacuous pass. (An application that keeps
/// its sources one level deeper — `snmp`, `megaco` — passes this check and contributes no source to
/// the `src/*.erl` glob; the empty-selection guard reports that instead of passing.)
fn apps_filter() -> Option<Vec<String>> {
    let value = std::env::var("BEAM_RT_BREADTH_APPS").ok()?;
    let apps: Vec<String> = value.split(',').map(|app| app.trim().to_string()).collect();
    if apps.iter().any(String::is_empty) {
        panic!("BEAM_RT_BREADTH_APPS must be a comma-separated list of application names, got `{value}`");
    }
    Some(apps)
}

/// `BEAM_RT_BREADTH_SHARD=i/n`: keep only the sources whose 0-based index in the filtered, sorted
/// list is `i` modulo `n`.
///
/// A malformed value is a loud harness failure, never a silently unfiltered run.
fn shard_selection() -> Option<(usize, usize)> {
    fn malformed(value: &str) -> ! {
        panic!("BEAM_RT_BREADTH_SHARD must be `i/n` with 0 <= i < n, got `{value}`");
    }
    let value = std::env::var("BEAM_RT_BREADTH_SHARD").ok()?;
    let (index, count) = value.split_once('/').unwrap_or_else(|| malformed(&value));
    let index: usize = index.trim().parse().unwrap_or_else(|_| malformed(&value));
    let count: usize = count.trim().parse().unwrap_or_else(|_| malformed(&value));
    if count == 0 || index >= count {
        malformed(&value);
    }
    Some((index, count))
}

/// `BEAM_RT_BREADTH_LIMIT=<n>`: keep the first `n` sources after the filters above.
fn limit_selection() -> Option<usize> {
    let value = std::env::var("BEAM_RT_BREADTH_LIMIT").ok()?;
    match value.trim().parse::<usize>() {
        Ok(limit) => Some(limit),
        Err(_) => {
            panic!("BEAM_RT_BREADTH_LIMIT must be a non-negative integer, got `{value}`")
        }
    }
}

/// `BEAM_RT_BREADTH_MAX_BYTES=<n>`: keep only sources whose file is at most `n` bytes.
///
/// The threshold exists because the *compile cost is superlinear in source size* while the runtime's
/// in-process route is bounded by `BLOCKING_PUMP_DEADLINE` (900 s): a sweep that wants a **broad**
/// census rather than a handful of hour-long probes selects by size.
fn max_bytes_selection() -> Option<u64> {
    let value = std::env::var("BEAM_RT_BREADTH_MAX_BYTES").ok()?;
    match value.trim().parse::<u64>() {
        Ok(max) => Some(max),
        Err(_) => panic!(
            "BEAM_RT_BREADTH_MAX_BYTES must be a non-negative integer byte count, got `{value}`"
        ),
    }
}

/// The size filter's own predicate: no threshold keeps everything.
fn keep_by_size(max_bytes: Option<u64>, length: u64) -> bool {
    match max_bytes {
        None => true,
        Some(max) => length <= max,
    }
}

/// `BEAM_RT_BREADTH_PA=a:b:c`: the directories a sweep puts **first** on the runtime's code path.
///
/// One `-pa` group (OTP's own form: `-pa D1 D2 D3` heads the path with `D1` first), so a sweep can
/// drive the compiler the *tree* ships in `bootstrap/lib/{compiler,kernel,stdlib}/ebin` rather than
/// the system one — contract A.9's "does the tree compile the tree" measurement. An empty entry is a
/// loud panic (it would silently add the CWD to the code path); an empty/unset value adds nothing.
fn pa_selection() -> Vec<PathBuf> {
    let Some(value) = std::env::var("BEAM_RT_BREADTH_PA").ok() else {
        return Vec::new();
    };
    if value.trim().is_empty() {
        return Vec::new();
    }
    value
        .split(':')
        .map(|dir| {
            if dir.trim().is_empty() {
                panic!("BEAM_RT_BREADTH_PA has an empty entry: `{value}`");
            }
            PathBuf::from(dir)
        })
        .collect()
}

/// What a produced artifact is measured against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Compare {
    /// Whole-file byte identity with the oracle's artifact — the system compiler's own contract.
    Bytes,
    /// The artifact merely **exists** (the byte relation is still reported, never asserted): the
    /// shape a sweep with a *different compiler version* on the code path needs, because the tree's
    /// OTP-30 compiler stamps `opcode_max` 183 where OTP 29 stamps 181 and the `Attr` `vsn` (and each
    /// `FunT` uniq) follow from that payload — so byte identity is impossible by construction.
    Artifact,
}

impl Compare {
    /// The label the header and the summary carry, so a captured log names its contract.
    fn name(self) -> &'static str {
        match self {
            Compare::Bytes => "bytes",
            Compare::Artifact => "artifact",
        }
    }
}

/// `BEAM_RT_BREADTH_COMPARE=bytes|artifact`: the selected verdict (default `bytes`); any other value
/// is a loud panic, so a typo can never quietly weaken the contract.
fn compare_selection() -> Compare {
    compare_from(std::env::var("BEAM_RT_BREADTH_COMPARE").ok().as_deref())
}

/// The switch's parsing, split out so the unit tests can pin it without a process-wide environment.
fn compare_from(value: Option<&str>) -> Compare {
    match value.map(str::trim) {
        None | Some("") | Some("bytes") => Compare::Bytes,
        Some("artifact") => Compare::Artifact,
        Some(other) => panic!(
            "BEAM_RT_BREADTH_COMPARE must be `bytes` (whole-file identity) or `artifact` (an \
             artifact was produced), got `{other}`"
        ),
    }
}
// ---------------------------------------------------------------------------------------
// The route switch (in-process vs scheduled — parsed once, loudly)
// ---------------------------------------------------------------------------------------

/// Which of the runtime's two `compile:file/2` routes the sweep drives.
///
/// Both routes run the *same* `beam_rt -noshell -eval '<compile:file(…)>'` invocation and differ
/// only in the option list handed to the compiler:
///
/// * [`Route::InProcess`] passes `no_spawn_compiler_process`, so the compile runs in the **calling**
///   process;
/// * [`Route::Scheduled`] omits it, so OTP's default applies and the compiler **spawns a compiler
///   process**, which the runtime schedules like any other process.
///
/// The option list is recorded in `Dbgi`, so the two routes' artifacts differ in that one chunk and
/// nowhere else: an artifact is only ever compared against the oracle's *for the same route*, which
/// [`compile_with_erlc`] guarantees by dropping `+no_spawn_compiler_process` in step with the
/// runtime's own list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    /// `compile:file/2` in the calling process (`no_spawn_compiler_process`).
    InProcess,
    /// OTP's default: `compile:file/2` spawns a compiler process.
    Scheduled,
}

impl Route {
    /// The label every per-source line and the summary carry, so a captured log names its route.
    fn name(self) -> &'static str {
        match self {
            Route::InProcess => "in-process",
            Route::Scheduled => "scheduled",
        }
    }

    /// Whether the runtime's option list carries `no_spawn_compiler_process` — the oracle's `erlc`
    /// command line mirrors this answer, so both sides always compile with the identical list.
    fn compiles_in_calling_process(self) -> bool {
        matches!(self, Route::InProcess)
    }
}

/// `BEAM_RT_BREADTH_SCHEDULED=1`: drive the sweep on the **scheduled** route; `0` (or unset) keeps
/// the in-process route; any other value is a loud panic ([`route_from`]).
fn route_selection() -> Route {
    route_from(std::env::var("BEAM_RT_BREADTH_SCHEDULED").ok().as_deref())
}

/// The switch's parsing, split out so the unit tests can pin it without a process-wide environment.
///
/// A value that is neither `0`/`1` nor unset panics: a typo (`true`, `yes`, an empty string) must
/// never silently select the other route and turn one route's measurement into the other's.
fn route_from(value: Option<&str>) -> Route {
    match value.map(str::trim) {
        None | Some("0") => Route::InProcess,
        Some("1") => Route::Scheduled,
        Some(other) => panic!(
            "BEAM_RT_BREADTH_SCHEDULED must be `1` (scheduled route) or `0`/unset (in-process \
             route), got `{other}`"
        ),
    }
}

/// The filtered, sorted selection: application filter, then the size threshold, then shard, then
/// limit — in that order, so `LIMIT` is a prefix of the (sharded) list a reader can replay.
///
/// The second element is how many sources the size threshold dropped, so a run always reports that
/// it measured a *slice* rather than silently pretending the corpus was smaller.
fn select(corpus: &Corpus) -> (Vec<Source>, usize) {
    let mut selected: Vec<Source> = match apps_filter() {
        Some(apps) => {
            for app in &apps {
                if !corpus.apps.iter().any(|known| known == app) {
                    panic!(
                        "BEAM_RT_BREADTH_APPS names `{app}`, which has no `lib/{app}/src` source \
                         directory here; this checkout has {} of them: {}",
                        corpus.apps.len(),
                        corpus.apps.join(", ")
                    );
                }
            }
            corpus
                .sources
                .iter()
                .filter(|source| apps.iter().any(|app| *app == source.app))
                .cloned()
                .collect()
        }
        None => corpus.sources.clone(),
    };
    let mut size_skipped = 0;
    if let Some(max_bytes) = max_bytes_selection() {
        let before = selected.len();
        selected.retain(|source| {
            // A source whose size cannot be read (a race with a checkout) is kept: the compile
            // verdict, not the harness, decides what it is.
            let length = std::fs::metadata(&source.path).map(|meta| meta.len()).unwrap_or(0);
            keep_by_size(Some(max_bytes), length)
        });
        size_skipped = before - selected.len();
    }
    if let Some((index, count)) = shard_selection() {
        selected = selected
            .into_iter()
            .enumerate()
            .filter(|(position, _)| position % count == index)
            .map(|(_, source)| source)
            .collect();
    }
    if let Some(limit) = limit_selection() {
        selected.truncate(limit);
    }
    (selected, size_skipped)
}
// ---------------------------------------------------------------------------------------
// Bounded child execution (a per-source deadline of its own)
// ---------------------------------------------------------------------------------------

/// One finished child: its exit code (`None` when it died by signal) and its captured output.
struct Probe {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Runs `program` with `args` (stdin at `/dev/null`, stdout/stderr captured), bounded by
/// `deadline`; a child that outlives it is **killed and reaped**.
///
/// `Err` is the failure's own diagnostic — a spawn error, a poll error or the timeout — so a caller
/// never has to invent one. The child is polled rather than waited on for the reason
/// [`support::run_bounded`] documents: a probe that blocks must be killable, not waited on for ever.
fn run_bounded(program: &Path, args: &[&str], deadline: Duration) -> Result<Probe, String> {
    let argv: Vec<std::ffi::OsString> = args.iter().map(std::ffi::OsString::from).collect();
    run_bounded_os(program, &argv, deadline)
}

/// [`run_bounded`] over an already-assembled argv (the runtime's invocation carries `-pa` directory
/// arguments as well as the `-eval` expression, so it is built as `OsString`s).
fn run_bounded_os(
    program: &Path,
    args: &[std::ffi::OsString],
    deadline: Duration,
) -> Result<Probe, String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());    let name = program.display().to_string();
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot spawn `{name}`: {error}"))?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let output = child
                    .wait_with_output()
                    .map_err(|error| format!("cannot collect `{name}`'s output: {error}"))?;
                return Ok(Probe {
                    code: output.status.code(),
                    stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
                    stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
                });
            }
            Ok(None) => {
                if started.elapsed() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "outlived the {} s per-source bound; the child was killed and reaped",
                        deadline.as_secs()
                    ));
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("cannot poll `{name}`: {error}"));
            }
        }
    }
}

/// The one-line diagnostic a failed child left behind.
///
/// The runtime's own diagnostics are `beam_rt: <reason>` on stderr, so that shape is preferred (its
/// prefix stripped, leaving `unsupported: …` / `error …` / `badarg` for [`cause`] to bucket); a
/// child that printed anything else contributes its last non-empty line, and a child that printed
/// nothing contributes its exit status — never an invented verdict. The exit code is always
/// reported, because "exit 3" versus "exit 1" is the difference between a capability gap and a
/// genuine runtime error.
fn reason_of(probe: &Probe) -> String {
    let runtime_line = probe
        .stderr
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("beam_rt: "))
        .next_back()
        .map(|line| line.trim_start_matches("beam_rt: ").to_string());
    let any_line = || {
        probe
            .stderr
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .next_back()
            .or_else(|| {
                probe
                    .stdout
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .next_back()
            })
            .map(str::to_string)
    };
    let diagnostic = runtime_line.or_else(any_line);
    match (probe.code, diagnostic) {
        (Some(code), Some(diagnostic)) => format!("exit {code}: {diagnostic}"),
        (Some(code), None) => format!("exit {code} with no diagnostic"),
        (None, Some(diagnostic)) => format!("no exit code (killed by signal): {diagnostic}"),
        (None, None) => "no exit code (killed by signal) and no diagnostic".to_string(),
    }
}

// ---------------------------------------------------------------------------------------
// The two compile routes
// ---------------------------------------------------------------------------------------

/// The oracle's `erlc`: `$ERLC` when it is an executable, else `erlc` on `$PATH` — resolved
/// **without running it** (the [`support::resolve_oracle`] convention).
fn resolve_erlc() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERLC") {
        let program = PathBuf::from(&value);
        return support::is_executable(&program).then_some(program);
    }
    support::find_on_path("erlc")
}

/// The **Erlang string literal** for `path`. The corpus and scratch paths on this host are ASCII,
/// which Rust's `Debug` and Erlang's string syntax quote identically; a non-UTF-8 path is a loud
/// panic rather than a silently mangled expression.
fn literal(path: &Path) -> String {
    format!(
        "{:?}",
        path.to_str()
            .unwrap_or_else(|| panic!("a UTF-8 path, got {}", path.display()))
    )
}

/// The path as an argv element — the same loud-panic rule as [`literal`].
fn path_arg(path: &Path) -> &str {
    path.to_str()
        .unwrap_or_else(|| panic!("a UTF-8 path, got {}", path.display()))
}

/// `compile:file(Source, [no_spawn_compiler_process, deterministic, no_docs, {outdir, D} |
/// [{i, Dir} …]])` — the oracle's option list (`no_spawn_compiler_process` is dropped on the
/// scheduled route), plus an include directory per `lib/*/include`.
///
/// The flat list needs no `++`: both `{outdir, _}` and `{i, _}` are tuple options, so the order does
/// not matter and the expression stays inside the `-eval` grammar.
fn compile_expr(source: &Source, corpus: &Corpus, outdir: &Path, route: Route) -> String {
    let mut options = Vec::new();
    if route.compiles_in_calling_process() {
        options.push("no_spawn_compiler_process".to_string());
    }
    options.push("deterministic".to_string());
    options.push("no_docs".to_string());
    options.push(format!("{{outdir, {}}}", literal(outdir)));
    for dir in &corpus.include_dirs {
        options.push(format!("{{i, {}}}", literal(dir)));
    }
    format!(
        "compile:file({}, [{}])",
        literal(&source.path),
        options.join(", ")
    )
}

/// The oracle's artifact for one source, or the reason it could not produce one.
///
/// `erlc`'s command line is the byte-identity recipe: `-o D` plus the `+` options the runtime's own
/// option list carries **on the same route** — `+no_spawn_compiler_process` is pushed exactly when
/// the runtime's list carries `no_spawn_compiler_process` — so both sides record the same option
/// list in `Dbgi` and the comparison stays within one route.
fn compile_with_erlc(
    erlc: &Path,
    source: &Source,
    corpus: &Corpus,
    outdir: &Path,
    route: Route,
) -> Result<Vec<u8>, String> {
    let mut arguments: Vec<&str> = vec!["-o", path_arg(outdir)];
    if route.compiles_in_calling_process() {
        arguments.push("+no_spawn_compiler_process");
    }
    arguments.push("+deterministic");
    arguments.push("+no_docs");
    for dir in &corpus.include_dirs {
        arguments.push("-I");
        arguments.push(path_arg(dir));
    }
    arguments.push(path_arg(&source.path));

    let probe = run_bounded(erlc, &arguments, SOURCE_DEADLINE)?;
    if probe.code != Some(0) {
        return Err(reason_of(&probe));
    }
    let artifact = source.artifact(outdir);
    std::fs::read(&artifact)
        .map_err(|error| format!("erlc exited 0 but wrote no {}: {error}", artifact.display()))
}

/// The runtime's artifact for one source — on `route` — or the reason it could not produce one.
///
/// The invocation is the real CLI in both cases: `beam_rt -noshell -eval '<compile:file(…)>'`. The
/// expression is the compile call and nothing else — the `-eval` action exits 0 on its own and a
/// successful compile prints nothing, so the artifact on disk is the whole verdict. `pa` (when not
/// empty) is prepended as one `-pa` group, so the compiler the sweep drives is named by the caller.
fn compile_on_route(
    source: &Source,
    corpus: &Corpus,
    outdir: &Path,
    route: Route,
    pa: &[PathBuf],
) -> Result<Vec<u8>, String> {
    let expression = compile_expr(source, corpus, outdir, route);
    let mut argv: Vec<std::ffi::OsString> = Vec::new();
    if !pa.is_empty() {
        argv.push("-pa".into());
        argv.extend(pa.iter().map(|dir| dir.as_os_str().to_owned()));
    }
    argv.push("-noshell".into());
    argv.push("-eval".into());
    argv.push(expression.into());
    let probe = run_bounded_os(Path::new(BINARY), &argv, SOURCE_DEADLINE)?;
    if probe.code != Some(0) {
        return Err(reason_of(&probe));
    }    let artifact = source.artifact(outdir);
    match std::fs::read(&artifact) {
        Ok(bytes) if bytes.is_empty() => Err(format!("wrote an EMPTY {}", artifact.display())),
        Ok(bytes) => Ok(bytes),
        // A `{error, Errors, Warnings}` answer is a *value*, so the action exits 0 and writes
        // nothing; that is a real divergence and must be reported, never silently dropped.
        Err(_) => Err(format!(
            "exited 0 but wrote no {} (compile:file answered an error term or nothing at all)",
            artifact.display()
        )),
    }
}

// ---------------------------------------------------------------------------------------
// Failure classification — the ledger's bucket key
// ---------------------------------------------------------------------------------------

/// Strips the `exit <n>: ` / `no exit code (killed by signal): ` prefix [`reason_of`] adds, so the
/// classification is made on the diagnostic itself rather than on its exit status.
fn diagnostic_of(reason: &str) -> &str {
    if let Some(rest) = reason.strip_prefix("exit ") {
        if let Some((code, rest)) = rest.split_once(':') {
            if !code.is_empty() && code.chars().all(|digit| digit.is_ascii_digit()) {
                return rest.trim_start();
            }
        }
    }
    if let Some(rest) = reason.strip_prefix("no exit code (killed by signal): ") {
        return rest.trim_start();
    }
    reason.trim()
}

/// The bucket a failure reason is counted under: a short, documented classifier so the run's
/// residual table is readable instead of a wall of one-off strings.
///
/// The buckets, in order:
///
/// * `unsupported: <first three tokens>` — a capability gap (`error.rs::unsupported_reason`'s
///   convention): `unsupported: opcode: …` / `unsupported: erlang:…` / `unsupported: <bif>` stay
///   distinguishable, which is what a follow-up needs;
/// * `blocking pump` — the compile parked and the nested pump could not resume it;
/// * `allocation failure` — the compile's arena/heap ran out;
/// * `error <term>` — a genuine runtime raise; the first token of the term is kept;
/// * `timed out (per-source bound)` — the child was killed at [`SOURCE_DEADLINE`];
/// * the first ~60 characters of anything else (a signal, a foreign diagnostic).
///
/// The exit-code prefix [`reason_of`] adds is stripped first ([`diagnostic_of`]), so `exit 3:
/// unsupported: …` and a bare `unsupported: …` land in the same bucket.
fn cause(reason: &str) -> String {
    let reason = diagnostic_of(reason);
    if let Some(rest) = reason.strip_prefix("unsupported") {
        let rest = rest.trim_start_matches(':').trim();
        let head: Vec<&str> = rest.split_whitespace().take(3).collect();
        return if head.is_empty() {
            "unsupported".to_string()
        } else {
            format!("unsupported: {}", head.join(" "))
        };
    }
    if reason.contains("blocking pump") {
        return "blocking pump".to_string();
    }
    if reason.contains("allocation failure") {
        return "allocation failure".to_string();
    }
    if reason.contains("per-source bound") {
        return "timed out (per-source bound)".to_string();
    }
    if reason == "error" || reason.starts_with("error ") || reason.starts_with("error:") {
        let term = reason
            .trim_start_matches("error")
            .trim_start_matches(':')
            .trim()
            .split_whitespace()
            .next()
            .unwrap_or("");
        return if term.is_empty() {
            "error".to_string()
        } else {
            format!("error {}", truncate(term, 60))
        };
    }
    truncate(reason, 60)
}

/// `text` cut to at most `limit` characters (never mid-character), with an ellipsis when it was.
fn truncate(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    }
}

// ---------------------------------------------------------------------------------------
// Scratch directories
// ---------------------------------------------------------------------------------------

/// A private temporary root for the whole run, removed when it is dropped.
///
/// One root with a directory per source and per side (rather than one `TempDir` per source) keeps
/// the corpus' ~1 500 artifacts in one place; each side still compiles into a directory of its own,
/// so the byte comparison never sees the other side's file.
struct TempDir(PathBuf);

impl TempDir {
    /// Creates `$TMPDIR/beam_rt_selfhost_breadth_<pid>_<label>`, unique per run.
    fn new(label: &str) -> TempDir {
        let path = std::env::temp_dir().join(format!(
            "beam_rt_selfhost_breadth_{}_{label}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("a private temporary directory");
        TempDir(path)
    }

    /// `<root>/<app>__<module>_<side>`, created.
    fn outdir(&self, source: &Source, side: &str) -> PathBuf {
        let path = self
            .0
            .join(format!("{}__{}_{side}", source.app, source.module));
        std::fs::create_dir_all(&path).expect("a per-source output directory");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------------------
// The sweep
// ---------------------------------------------------------------------------------------

/// The widened A.9 sweep: every discovered source, oracle baseline first, then the compile on the
/// route `BEAM_RT_BREADTH_SCHEDULED` selects, then the byte comparison.
///
/// Corpus-scale and therefore `#[ignore]`d. Run a chunk (add `BEAM_RT_BREADTH_SCHEDULED=1` for the
/// scheduled route):
///
/// ```text
/// BEAM_RT_BREADTH_APPS=parsetools BEAM_RT_BREADTH_LIMIT=3 \
///   erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt \
///     --test selfhost_breadth -- --ignored --nocapture
/// ```
#[test]
#[ignore = "corpus-scale A.9 breadth sweep: compiles every lib/*/src source (hours under the debug runtime)"]
fn every_app_source_compiles_in_process_like_the_oracle() {
    let body = std::thread::Builder::new()
        .name("breadth-sweep".to_string())
        .stack_size(SWEEP_STACK_BYTES)
        .spawn(sweep_body)
        .expect("the breadth sweep body thread must spawn");
    if let Err(payload) = body.join() {
        std::panic::resume_unwind(payload);
    }
}

/// The sweep body: [`every_app_source_compiles_in_process_like_the_oracle`] runs this on a thread of
/// its own so the recursion depth the corpus needs is available.
fn sweep_body() {
    // Parsed **first**, so a malformed value panics on any host (a loud skip for a missing toolchain
    // must never be able to hide a typo in the switch).
    let route = route_selection();
    let route_name = route.name();
    let compare = compare_selection();
    let compare_name = compare.name();
    let pa = pa_selection();
    let Some(oracle) = support::require_oracle(BANNER) else {
        return;
    };    let Some(erlc) = resolve_erlc() else {
        support::skip_loudly(BANNER, "no `erlc` on $PATH (and no executable $ERLC)");
        return;
    };
    let root = source_root();
    let Some(corpus) = discover(&root) else {
        support::skip_loudly(
            BANNER,
            &format!(
                "no `lib/*/src/*.erl` source tree under {} (an installed-only or relocated build)",
                root.display()
            ),
        );
        return;
    };
    let (sources, size_skipped) = select(&corpus);
    if sources.is_empty() {
        panic!(
            "the BEAM_RT_BREADTH_* filters selected none of this checkout's {} sources across {} \
             applications — a run that compiles nothing must never pass",
            corpus.sources.len(),
            corpus.apps.len()
        );
    }
    let total = sources.len();
    eprintln!(
        "breadth sweep ({route_name} route, {compare_name} compare): {total} sources selected of {} \
         across {} applications; {} include directories; oracle {} (baseline via {}); {} s \
         per-source deadline",
        corpus.sources.len(),
        corpus.apps.len(),
        corpus.include_dirs.len(),
        oracle.display(),
        erlc.display(),
        SOURCE_DEADLINE.as_secs(),
    );
    for dir in &pa {
        eprintln!("  code path first: {}", dir.display());
    }
    if size_skipped > 0 {
        eprintln!(
            "  size filter: {size_skipped} source(s) larger than the threshold were not attempted"
        );
    }    for dir in &corpus.include_dirs {
        eprintln!("  include: {}", dir.display());
    }
    if let Some((index, count)) = shard_selection() {
        eprintln!(
            "  shard {index}/{count} of {} sources",
            corpus.sources.len()
        );
    }
    if let Some(apps) = apps_filter() {
        eprintln!("  applications: {}", apps.join(","));
    }

    let scratch = TempDir::new("corpus");
    let sweep_started = Instant::now();

    // The oracle's compilable set is the baseline: `oracle_ok` is what both counts are measured
    // against, and `excluded` records why a source never entered it.
    let mut oracle_ok: Vec<String> = Vec::new();
    let mut compiled_ok: Vec<String> = Vec::new();
    let mut oracle_only: Vec<String> = Vec::new();
    let mut mismatched: Vec<String> = Vec::new();
    let mut excluded: Vec<String> = Vec::new();
    // cause → the modules that failed that way (the ledger's rows).
    let mut ledger: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // The selected route's own wall time, summed over its sources (start-up included), so the
    // `files/s` figure describes this runtime on this route rather than the machine's total
    // occupancy.
    let mut route_wall = Duration::ZERO;

    for (index, source) in sources.iter().enumerate() {
        let label = source.label();
        let position = index + 1;

        // ---- the oracle's own verdict: the baseline, compiled with this route's option list ----
        let oracle_out = scratch.outdir(source, "oracle");
        let oracle_started = Instant::now();
        let reference = match compile_with_erlc(&erlc, source, &corpus, &oracle_out, route) {
            Ok(bytes) => bytes,
            Err(reason) => {
                eprintln!(
                    "[{route_name}] [{position}/{total}] {label}: {:.3} s, oracle FAILED \
                     (excluded): {reason}",
                    oracle_started.elapsed().as_secs_f64()
                );
                excluded.push(format!("{label}: {reason}"));
                continue;
            }
        };
        oracle_ok.push(label.clone());

        // ---- the runtime's verdict, on the selected route, in a runtime of its own ----
        let rust_out = scratch.outdir(source, "rust");
        let route_started = Instant::now();
        let mine = compile_on_route(source, &corpus, &rust_out, route, &pa);        let route_elapsed = route_started.elapsed();
        route_wall += route_elapsed;

        let mine = match mine {
            Ok(bytes) => bytes,
            Err(reason) => {
                eprintln!(
                    "[{route_name}] [{position}/{total}] {label}: {:.3} s, {route_name} FAILED: \
                     {reason}",
                    route_elapsed.as_secs_f64()
                );
                oracle_only.push(label.clone());
                ledger
                    .entry(cause(&reason))
                    .or_default()
                    .push(label.clone());
                continue;
            }
        };
        compiled_ok.push(label.clone());

        let verdict = if mine == reference {
            "byte-identical".to_string()
        } else {
            let at = mine
                .iter()
                .zip(reference.iter())
                .position(|(ours, theirs)| ours != theirs)
                .unwrap_or_else(|| mine.len().min(reference.len()));
            mismatched.push(format!(
                "{label}: first byte difference at {at} of {} ({route_name}) vs {} (oracle) bytes",
                mine.len(),
                reference.len()
            ));
            match compare {
                Compare::Bytes => format!(
                    "BYTE MISMATCH at {at} ({} vs {} bytes)",
                    mine.len(),
                    reference.len()
                ),
                // Reported, never asserted: a different compiler *version* on the code path stamps a
                // different `opcode_max` (and therefore a different `vsn`), so this is the expected
                // relation rather than a divergence.
                Compare::Artifact => format!(
                    "artifact ok, differs from the oracle at {at} ({} vs {} bytes; not asserted)",
                    mine.len(),
                    reference.len()
                ),
            }
        };        eprintln!(
            "[{route_name}] [{position}/{total}] {label}: {:.3} s, {} B, {verdict}",
            route_elapsed.as_secs_f64(),
            mine.len()
        );
    }

    let elapsed = sweep_started.elapsed();
    eprintln!(
        "breadth sweep ({route_name} route, {compare_name} compare): {total} sources in {:.1} s; \
         oracle compiled {}, {route_name} compiled {}, oracle-only {}, byte-mismatched {}, \
         oracle-excluded {}; {route_name} route {:.1} s → {:.2} files/s",
        elapsed.as_secs_f64(),
        oracle_ok.len(),
        compiled_ok.len(),
        oracle_only.len(),
        mismatched.len(),
        excluded.len(),
        route_wall.as_secs_f64(),
        files_per_second(compiled_ok.len(), route_wall),
    );

    // ---- the ledger: cause × count, one example module each ----
    if ledger.is_empty() {
        eprintln!(
            "failure ledger ({route_name} route): empty — every oracle-compilable source compiled"
        );    } else {
        let mut rows: Vec<(&String, &Vec<String>)> = ledger.iter().collect();
        rows.sort_by(|(cause_a, a), (cause_b, b)| b.len().cmp(&a.len()).then(cause_a.cmp(cause_b)));
        eprintln!("failure ledger ({route_name} route; cause × count, one example module each):");
        for (cause, modules) in rows {
            eprintln!(
                "  {:5} × {} — e.g. {}",
                modules.len(),
                cause,
                modules.first().map(String::as_str).unwrap_or("-")
            );
        }
    }

    // ---- the failure list, printed before any assert (never a bare number) ----
    for exclusion in &excluded {
        eprintln!("  oracle-excluded: {exclusion}");
    }
    for failure in &oracle_only {
        eprintln!("  residual: {failure}");
    }
    for mismatch in &mismatched {
        eprintln!("  artifact differs: {mismatch}");
    }

    // ---- the asserts: set, then count, then bytes (in `bytes` compare mode only) ----
    assert!(
        oracle_only.is_empty(),
        "every source the oracle compiles must compile on the {route_name} route too; {} did \
         not:\n{}\nfailure ledger:\n{}",
        oracle_only.len(),
        oracle_only.join("\n"),
        ledger_summary(&ledger)
    );
    assert_eq!(
        compiled_ok.len(),
        oracle_ok.len(),
        "the {route_name} compile count must equal the oracle's own compilable count ({}):\n{}\n\
         failure ledger:\n{}",
        oracle_ok.len(),
        oracle_only.join("\n"),
        ledger_summary(&ledger)
    );
    if compare == Compare::Bytes {
        assert!(
            mismatched.is_empty(),
            "every `.beam` this runtime writes on the {route_name} route must be byte-identical to \
             the oracle's artifact for that route; {} differ:\n{}",
            mismatched.len(),
            mismatched.join("\n")
        );
    } else {
        eprintln!(
            "artifact compare: {} of {} produced artifact(s) differ from the oracle's — reported, \
             never asserted (a different compiler version on the code path stamps a different \
             opcode_max, so its `vsn`/`FunT` follow)",
            mismatched.len(),
            compiled_ok.len()
        );
    }
}
/// The ledger rendered for an assertion message (the same rows the summary printed).
fn ledger_summary(ledger: &BTreeMap<String, Vec<String>>) -> String {
    let mut rows: Vec<(&String, &Vec<String>)> = ledger.iter().collect();
    rows.sort_by(|(cause_a, a), (cause_b, b)| b.len().cmp(&a.len()).then(cause_a.cmp(cause_b)));
    rows.iter()
        .map(|(cause, modules)| {
            format!(
                "{} × {cause} (e.g. {})",
                modules.len(),
                modules.first().map(String::as_str).unwrap_or("-")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `compiled / wall`, guarding the zero-wall case (a corpus of no sources never reaches here, but a
/// division by zero would panic with a less useful message than `0.00`).
fn files_per_second(compiled: usize, wall: Duration) -> f64 {
    let seconds = wall.as_secs_f64();
    if seconds <= 0.0 {
        return 0.0;
    }
    compiled as f64 / seconds
}

// ---------------------------------------------------------------------------------------
// Unit coverage for the classifier and the filters
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cause_classifier_keeps_capability_gaps_apart() {
        assert_eq!(
            cause("unsupported: opcode: gc_bif: x(3)"),
            "unsupported: opcode: gc_bif: x(3)"
        );
        // The exit-code prefix `reason_of` adds must not hide the bucket.
        assert_eq!(
            cause("exit 3: unsupported: erlang:list_to_atom/1 is not implemented"),
            "unsupported: erlang:list_to_atom/1 is not"
        );
        assert_eq!(cause("unsupported"), "unsupported");
        assert_eq!(
            cause("exit 1: the compile blocked in a blocking pump round"),
            "blocking pump"
        );
        assert_eq!(
            cause("exit 1: allocation failure on the process heap"),
            "allocation failure"
        );
        assert_eq!(
            cause("outlived the 300 s per-source bound; the child was killed and reaped"),
            "timed out (per-source bound)"
        );
        assert_eq!(cause("exit 1: error {badkey,nokey}"), "error {badkey,nokey}");
        assert_eq!(cause("error"), "error");
        assert!(
            cause(&"x".repeat(200)).ends_with('…'),
            "a long unexplained reason is truncated"
        );
    }

    #[test]
    fn truncation_never_splits_a_character() {
        assert_eq!(truncate("short", 60), "short");
        assert_eq!(truncate("åäöåäö", 3), "åäö…");
    }

    #[test]
    fn the_files_per_second_figure_never_divides_by_zero() {
        assert_eq!(files_per_second(0, Duration::ZERO), 0.0);
        assert_eq!(files_per_second(10, Duration::from_secs(2)), 5.0);
    }

    #[test]
    fn erlang_literals_quote_a_path_the_way_erlang_reads_it() {
        assert_eq!(literal(Path::new("/tmp/a b/out")), "\"/tmp/a b/out\"");
    }

    #[test]
    fn the_scheduled_route_switch_is_off_unless_explicitly_on() {
        assert_eq!(route_from(None), Route::InProcess);
        assert_eq!(route_from(Some("0")), Route::InProcess);        assert_eq!(route_from(Some("1")), Route::Scheduled);
        // The route names are the report labels, so they cannot drift.
        assert_eq!(Route::InProcess.name(), "in-process");
        assert_eq!(Route::Scheduled.name(), "scheduled");
        // The flag and the label move together: only the in-process route keeps the option.
        assert!(Route::InProcess.compiles_in_calling_process());
        assert!(!Route::Scheduled.compiles_in_calling_process());
        // Anything else is a loud panic, never a silently ignored value.
        for bogus in ["", "bogus", "true", "2", "-1"] {
            let panicked = std::panic::catch_unwind(|| route_from(Some(bogus))).is_err();
            assert!(panicked, "`{bogus}` must panic, not select a route");
        }
    }

    #[test]
    fn the_compare_switch_defaults_to_byte_identity() {
        assert_eq!(compare_from(None), Compare::Bytes);
        assert_eq!(compare_from(Some("")), Compare::Bytes);
        assert_eq!(compare_from(Some("bytes")), Compare::Bytes);
        assert_eq!(compare_from(Some("artifact")), Compare::Artifact);
        // The labels are what a captured log names its contract by, so they cannot drift.
        assert_eq!(Compare::Bytes.name(), "bytes");
        assert_eq!(Compare::Artifact.name(), "artifact");
        for bogus in ["artifacts", "byte", "none", "1"] {
            let panicked = std::panic::catch_unwind(|| compare_from(Some(bogus))).is_err();
            assert!(panicked, "`{bogus}` must panic, not weaken the contract");
        }
    }

    #[test]
    fn the_size_filter_boundary_is_inclusive() {
        assert!(keep_by_size(None, u64::MAX));
        assert!(keep_by_size(Some(100), 100));
        assert!(!keep_by_size(Some(100), 101));
        assert!(keep_by_size(Some(0), 0));
        // An unreadable file contributes length 0 (`select`'s choice), so it is always kept.
        assert!(keep_by_size(Some(1), 0));
    }
}