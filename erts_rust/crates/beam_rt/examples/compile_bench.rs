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

//! `compile_bench` - a **re-runnable throughput harness** for this runtime's in-process
//! `compile:file/2` (contract A.9): the real OTP compiler running inside `beam_rt`, with no
//! `erlc` subprocess anywhere on the Rust side.
//!
//! # Recipe
//!
//! ```text
//! erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml --release -p beam_rt \
//!     --example compile_bench [OPTIONS]
//! ```
//!
//! | option | meaning |
//! |---|---|
//! | *(none)* | the default corpus: the four sources named below, smallest first |
//! | `--source <path>` | compile this source; **repeatable**, and any occurrence replaces the default corpus (the given order is kept) |
//! | `--repeat <n>` | how many fresh runtimes compile each source (default 1); the **fastest** run is reported, since a timing can only be perturbed upwards |
//! | `--oracle` | additionally run the *system* `erl` on the identical option list, for the side-by-side columns (needs `erl` on `$PATH`) |
//! | `-h`, `--help` | the usage text |
//!
//! The default corpus walks three orders of magnitude of source size inside this very tree:
//! `lib/stdlib/src/ordsets.erl` (13 585 B), `lib/stdlib/src/maps.erl` (37 745 B),
//! `lib/stdlib/src/string.erl` (103 287 B) and `lib/compiler/src/v3_core.erl` (193 385 B).
//! The compiler compiling its own core is the heaviest legitimate workload on this route -
//! roughly a minute on a loaded host - so the whole default run is a ~1.5 min job, and it is
//! a foreground run: nothing here is backgrounded.
//!
//! # What is measured, and through which route
//!
//! Each source is compiled through **exactly the production in-process route** - the one
//! `beam_rt -noshell -eval '<compile:file(Src, Opts)>'` takes, and the one the crate's own A.9
//! suites drive ([`Runtime::eval`](beam_rt::Runtime::eval), i.e. the `erl_eval`-style
//! tree-walker dispatching into the real `compiler` application in this machine, whose
//! blocking `receive`s the nested pump resumes). Nothing shells out to `erlc` on the measured
//! path: the only subprocess the harness ever starts is the optional `--oracle` one.
//!
//! The option list is the one the A.9 byte-comparison uses and is deliberately **not**
//! configurable here, so two runs of this harness always measure the same work:
//! `[no_spawn_compiler_process, deterministic, no_docs, {outdir, Dir}]`.
//!
//! Per source the harness reports:
//!
//! * the source size in bytes and the compile's **wall** milliseconds, timed with
//!   [`Instant`] around the one `eval` call (so the runtime's boot - printed separately and
//!   never inside the figure - cannot leak into it);
//! * the source throughput in `KB/s` (source bytes per second of wall clock), and three ratios
//!   against the **smallest** source on the list: `size x`, `time x` and `time/byte x`. The last
//!   one is the shape signal: a compile whose cost is **linear** in source size keeps
//!   `time/byte x` flat at ≈1 while `size x` grows, and a **quadratic** one makes it grow with
//!   `size x` itself;
//! * `erlang:statistics(garbage_collection)` as the C's `{collections, words_reclaimed, 0}`
//!   shape, **delta'd across the compile** (read from `Runtime::gc_stats` before and after, so
//!   the boot's own collections are excluded) - the deterministic counter that survives a
//!   loaded host;
//! * `erlang:memory(processes_used)` / `(binary)` / `(total)` in bytes, read after the compile
//!   returns from this runtime's live state (own-state figures, comparable *across* the rows
//!   of one run and meaningless against the oracle's);
//! * the artifact's size in bytes - a deterministic figure that also proves the compile really
//!   produced a `.beam`;
//! * with `--oracle`, the system `erl`'s own wall time for the whole `erl` invocation **and**
//!   the compile's interior time as that VM measured it (`erlang:monotonic_time/1` around the
//!   call), so the oracle's process start-up can be subtracted by eye.
//!
//! # Reading the numbers on a shared host
//!
//! The host is shared and its load is not under this harness's control, so an **absolute** wall
//! time can move by ±100 % between two runs while the *ratios* between rows are stable enough
//! to compare across runs. The harness therefore prints `/proc/loadavg` and this process's
//! `Cpus_allowed_list` before and after the measurement, and it never asserts a duration: a
//! slow compile is a *result*, not a failure. The exit code is non-zero only when the compile
//! itself fails (a `BeamError` - including the in-process route's own
//! `BLOCKING_PUMP_DEADLINE` trip on a source that cannot finish - or an answer other than
//! `{ok, Module}`), when a named source is unreadable, or on a malformed invocation.
//!
//! `BEAM_RT_PERF_COUNTERS=1` (see `crate::context::calls::perf_counters` and
//! `crate::context::machine_pool`) prints the dispatch-memo, machine-pool-reuse and root-walk
//! census on stderr at the end of the run - the counters that explain *why* a row costs what
//! it costs, in a per-key breakdown that no wall clock can give.
//!
//! # Why an example and not a test
//!
//! There is no un-ignored timing test anywhere in this crate, and this harness does not add
//! one: on a loaded host a wall-clock threshold is a flake generator, and the durable evidence
//! for the route is the crate's *correctness* suites (`tests/compile_file.rs`,
//! `src/context/pump.rs`'s byte-comparison and the opt-in corpus sweeps). What a performance
//! pass needs is a number a future agent can re-derive in one command - that is this file.
#![deny(unsafe_code)]
#![warn(missing_docs)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::time::{Duration, Instant};

use beam_rt::{CodeServer, Runtime};

use beam_term::{is_small, signed_value};

/// The default corpus, smallest source first: three `lib/stdlib` modules spanning an order of
/// magnitude, then the compiler's own core (the heaviest compile on this route).
///
/// Paths are relative to the repository root (see [`source_root`]).
const DEFAULT_SOURCES: [&str; 4] = [
    "lib/stdlib/src/ordsets.erl",
    "lib/stdlib/src/maps.erl",
    "lib/stdlib/src/string.erl",
    "lib/compiler/src/v3_core.erl",
];

/// How many fresh runtimes compile each source unless `--repeat` says otherwise.
const DEFAULT_REPEAT: usize = 1;

/// The compile options **every** measured run uses, up to the `{outdir, Dir}` element that
/// [`compile_options`] appends.
///
/// `no_spawn_compiler_process` keeps the compiler in this machine (the whole point of the
/// route), `deterministic` makes the artifact a function of the source alone and `no_docs`
/// keeps the compressed `Docs` chunk out - the A.9 byte-comparison's exact list.
const COMPILE_OPTIONS: &str = "[no_spawn_compiler_process, deterministic, no_docs, {outdir, ";

/// How the harness ended.
enum Failure {
    /// The invocation itself was malformed - exit 2 with the usage text.
    Usage(String),
    /// `-h`/`--help` - print the usage text and exit 0.
    Help,
    /// A compile failed, a source was unreadable, or the oracle misbehaved - exit 1.
    Error(String),
}

/// The parsed command line.
struct Config {
    /// The sources to compile, in the order they will be reported.
    sources: Vec<PathBuf>,
    /// How many fresh-runtime runs per source; the fastest is reported.
    repeat: usize,
    /// Whether to run the system `erl` beside each source.
    oracle: bool,
}

/// One runtime's view of a single compile.
struct Sample {
    /// Wall clock around the one `eval` call - the runtime's boot is excluded.
    wall: Duration,
    /// The node-global GC counters, delta'd across the compile.
    gc_collections: u64,
    /// Words those collections reclaimed (`erlang:statistics(garbage_collection)`'s 2nd field).
    gc_words: u64,
    /// `erlang:memory(processes_used)`, in bytes, after the compile.
    processes_used: u64,
    /// `erlang:memory(binary)`, in bytes, after the compile.
    binary: u64,
    /// `erlang:memory(total)`, in bytes, after the compile.
    total: u64,
    /// The `.beam` the compile wrote, in bytes.
    artifact: u64,
    /// How long the fresh runtime this sample ran on took to boot (reported, never measured).
    boot: Duration,
}

/// The system `erl`'s view of the same compile.
struct OracleSample {
    /// Wall clock around the whole `erl` invocation (so it carries the VM's start-up).
    wall: Duration,
    /// The compile's interior time, as that VM measured it with `erlang:monotonic_time/1`.
    in_vm: Duration,
}

/// One reported source: its size and the fastest of its [`Sample`]s.
struct Row {
    /// The source's file name (the table's label).
    name: String,
    /// The source's size in bytes.
    bytes: u64,
    /// The fastest sample of the run.
    sample: Sample,
    /// The oracle's figures, when `--oracle` was given.
    oracle: Option<OracleSample>,
}

fn main() {
    match run() {
        Ok(()) => {}
        Err(Failure::Help) => print_usage(),
        Err(Failure::Usage(why)) => {
            eprintln!("compile_bench: {why}");
            print_usage();
            process::exit(2);
        }
        Err(Failure::Error(why)) => {
            eprintln!("compile_bench: {why}");
            process::exit(1);
        }
    }
}

/// Parses the command line, measures every source and prints the report.
fn run() -> Result<(), Failure> {
    let config = parse_args()?;
    let root = source_root();
    let sources = resolve_sources(&config.sources, &root)?;
    let oracle = if config.oracle {
        Some(find_on_path("erl").ok_or_else(|| {
            Failure::Error("`--oracle` was given but there is no `erl` on $PATH".to_string())
        })?)
    } else {
        None
    };

    print_header(&config, &sources, oracle.as_deref());

    let scratch = TempDir::create()?;
    let mut rows: Vec<Row> = Vec::with_capacity(sources.len());
    for source in &sources {
        let row = measure(source, &scratch, config.repeat, oracle.as_deref())?;
        print_row(&row);
        rows.push(row);
    }
    print_tables(&rows);
    println!();
    println!("loadavg (end): {}", loadavg());
    Ok(())
}

/// Compiles one source `repeat` times (a fresh runtime each time) and keeps the fastest run.
///
/// The fresh runtime per run is **load-bearing**, not tidiness: the `-eval` route allocates on
/// an arena this runtime never resets, so driving several compiles through one runtime retains
/// every earlier compile's garbage and a large source then dies on that arena's own
/// `allocation failure` rather than on anything the compile decided. It is also what the
/// production route looks like - one `beam_rt` invocation compiles one file.
fn measure(
    source: &Path,
    scratch: &TempDir,
    repeat: usize,
    oracle: Option<&Path>,
) -> Result<Row, Failure> {
    let name = file_name(source);
    let bytes = fs::metadata(source)
        .map_err(|error| Failure::Error(format!("cannot stat {}: {error}", source.display())))?
        .len();
    let expect = format!("{{ok,{}}}", module_of(source)?);

    let mut best: Option<Sample> = None;
    for index in 0..repeat {
        let outdir = scratch.child(&format!("{name}-{index}"))?;
        let sample = one_sample(source, &outdir, &expect)?;
        match &best {
            Some(current) if current.wall <= sample.wall => {}
            _ => best = Some(sample),
        }
    }
    let sample =
        best.ok_or_else(|| Failure::Error("at least one repeat must run".to_string()))?;

    let oracle_sample = match oracle {
        Some(erl) => {
            let outdir = scratch.child(&format!("{name}-oracle"))?;
            Some(oracle_sample(erl, source, &outdir)?)
        }
        None => None,
    };
    Ok(Row {
        name,
        bytes,
        sample,
        oracle: oracle_sample,
    })
}

/// Boots one runtime on the CLI's own route, compiles `source` on it and reads the counters.
fn one_sample(source: &Path, outdir: &Path, expect: &str) -> Result<Sample, Failure> {
    let boot_started = Instant::now();
    let mut runtime = cli_route_runtime()?;
    let boot = boot_started.elapsed();

    // The boot's own collections belong to the boot, so the reported figure is the delta.
    let before = runtime.gc_stats();
    let expression = format!(
        "compile:file({}, {})",
        literal(source)?,
        compile_options(outdir)?
    );
    let started = Instant::now();
    let answer = runtime.eval(&expression).map_err(|error| {
        Failure::Error(format!(
            "the in-process compile of `{}` failed: {error}",
            source.display()
        ))
    })?;
    let wall = started.elapsed();
    let after = runtime.gc_stats();

    let rendered = runtime.format_term_w(answer, None).map_err(|error| {
        Failure::Error(format!(
            "the compile of `{}` answered a term that cannot be printed: {error}",
            source.display()
        ))
    })?;
    if rendered.trim() != expect {
        return Err(Failure::Error(format!(
            "the in-process compile of `{}` answered `{}`, expected `{expect}`",
            source.display(),
            rendered.trim()
        )));
    }

    let processes_used = memory(&mut runtime, "processes_used")?;
    let binary = memory(&mut runtime, "binary")?;
    let total = memory(&mut runtime, "total")?;
    let artifact = outdir.join(format!("{}.beam", module_of(source)?));
    let artifact_bytes = fs::metadata(&artifact).map(|meta| meta.len()).unwrap_or(0);

    Ok(Sample {
        wall,
        gc_collections: after.collections.saturating_sub(before.collections),
        gc_words: after.reclaimed.saturating_sub(before.reclaimed),
        processes_used,
        binary,
        total,
        artifact: artifact_bytes,
        boot,
    })
}

/// A runtime booted exactly the way the CLI boots one for an action (`-eval`/`-s`/`-run`):
/// the preloaded set and the system code path, then this release's own
/// [`bootstrap`](CodeServer::own_bootstrap_boot_script) script.
///
/// The configuration comes from the CLI's **own parser** ([`beam_rt::parse`]) with
/// `-noshell`, so it is the configuration `beam_rt -noshell -eval '<compile:file(…)>'` builds:
/// `-noshell` is recorded as an init argument, which is what the booted kernel's
/// `user`/`user_drv` gates the OTP slogan on (`init:get_argument(noshell)`), so the report is
/// not polluted by a banner the way an interactive `erl` would be.
///
/// The boot script is what makes the run *the* production route: its `kernel_sup` wins the
/// `file_server_2` name, so the compiler's `file:open/2` and `{read_handle_info, Opts}` round
/// trips (`epp`'s own reads) are answered by the real Erlang file server rather than the
/// runtime's native stand-in. The config must **name** a script
/// ([`beam_rt::RuntimeConfig::with_resolved_boot_script`]) for the same reason the CLI records
/// one before building: [`beam_rt::RuntimeConfig::drives_a_boot_script`] is the one predicate behind the
/// native service's name decision, and a config that names no script makes the booted
/// `kernel_sup`'s own `{file_server, start_link, []}` child abort on `{already_started, Pid}`.
fn cli_route_runtime() -> Result<Runtime, Failure> {
    let config = beam_rt::parse(&["-noshell".to_string()])
        .map_err(|error| Failure::Error(format!("the CLI's `-noshell` config: {error}")))?
        .with_resolved_boot_script(Some("start".to_string()));
    let mut runtime = Runtime::new(config)
        .map_err(|error| Failure::Error(format!("the runtime could not be built: {error}")))?;
    let _ = runtime.install_nif_thread_env();
    runtime
        .boot()
        .map_err(|error| Failure::Error(format!("the boot sequence failed: {error}")))?;
    let script = CodeServer::own_bootstrap_boot_script().ok_or_else(|| {
        Failure::Error(
            "no `bootstrap/bin/start.boot` above this executable or under $ERL_TOP, so the CLI's \
             boot route cannot be reproduced"
                .to_string(),
        )
    })?;
    runtime
        .boot_from_script(&script)
        .map_err(|error| Failure::Error(format!("the boot script {} failed: {error}", script.display())))?;
    Ok(runtime)
}

/// `erlang:memory(Key)` in bytes, read through the real dispatch (so the figure is this
/// runtime's own live state, never a constant).
fn memory(runtime: &mut Runtime, key: &str) -> Result<u64, Failure> {
    let term = runtime
        .eval(&format!("erlang:memory({key})"))
        .map_err(|error| Failure::Error(format!("erlang:memory({key}) failed: {error}")))?;
    if !is_small(term) {
        return Err(Failure::Error(format!(
            "erlang:memory({key}) answered a non-integer figure"
        )));
    }
    u64::try_from(signed_value(term)).map_err(|_| {
        Failure::Error(format!("erlang:memory({key}) answered a negative figure"))
    })
}

/// The system `erl` compiling the same source on the identical option list, timed from the
/// outside (the whole invocation) and from the inside (`erlang:monotonic_time/1`).
fn oracle_sample(erl: &Path, source: &Path, outdir: &Path) -> Result<OracleSample, Failure> {
    let program = format!(
        "T0 = erlang:monotonic_time(millisecond), {{ok, _}} = compile:file({}, {}), \
         T1 = erlang:monotonic_time(millisecond), io:format(\"~w~n\", [T1 - T0]), halt(0).",
        literal(source)?,
        compile_options(outdir)?
    );
    let started = Instant::now();
    let output = Command::new(erl)
        .arg("-noshell")
        .arg("-eval")
        .arg(&program)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| Failure::Error(format!("cannot run {}: {error}", erl.display())))?;
    let wall = started.elapsed();
    if !output.status.success() {
        return Err(Failure::Error(format!(
            "the oracle {} could not compile {}: {:?}\n--- stderr ---\n{}",
            erl.display(),
            source.display(),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let milliseconds = text
        .split_whitespace()
        .next()
        .and_then(|field| field.parse::<u64>().ok())
        .ok_or_else(|| {
            Failure::Error(format!(
                "the oracle printed no interior time for {}: {text:?}",
                source.display()
            ))
        })?;
    Ok(OracleSample {
        wall,
        in_vm: Duration::from_millis(milliseconds),
    })
}

/// The `[no_spawn_compiler_process, deterministic, no_docs, {outdir, "…"}]` option list.
fn compile_options(outdir: &Path) -> Result<String, Failure> {
    let directory = literal(outdir)?;
    Ok(format!("{COMPILE_OPTIONS}{directory}}}]"))
}

/// `path` as an **Erlang string literal**: `Debug` quotes and escapes a Rust `&str` the way
/// Erlang's string syntax does, provided the path is ASCII (a non-ASCII path would be escaped
/// as `\u{…}`, which Erlang does not understand, so it is refused rather than guessed at).
fn literal(path: &Path) -> Result<String, Failure> {
    let text = path.to_str().ok_or_else(|| {
        Failure::Error(format!("{} is not valid UTF-8", path.display()))
    })?;
    if !text.is_ascii() {
        return Err(Failure::Error(format!(
            "{} is not an ASCII path, which the `-eval` grammar cannot express",
            path.display()
        )));
    }
    Ok(format!("{text:?}"))
}

/// The repository root: `$ERL_TOP` when set, else three levels up from this crate's manifest
/// directory (`<root>/erts_rust/crates/beam_rt`).
fn source_root() -> PathBuf {
    match env::var_os("ERL_TOP") {
        Some(value) => PathBuf::from(value),
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.."),
    }
}

/// The sources to compile: the `--source` values verbatim when any were given, else
/// [`DEFAULT_SOURCES`] resolved against `root`. A relative `--source` is resolved against the
/// current directory (so `--source lib/stdlib/src/lists.erl` works from the repository root)
/// and, failing that, against `root`.
fn resolve_sources(given: &[PathBuf], root: &Path) -> Result<Vec<PathBuf>, Failure> {
    let mut sources = Vec::with_capacity(given.len().max(DEFAULT_SOURCES.len()));
    if given.is_empty() {
        for relative in DEFAULT_SOURCES {
            sources.push(root.join(relative));
        }
    } else {
        for path in given {
            if path.is_absolute() || path.is_file() {
                sources.push(path.clone());
            } else {
                sources.push(root.join(path));
            }
        }
    }
    for source in &sources {
        if !source.is_file() {
            return Err(Failure::Error(format!(
                "no source to compile at {} (pass `--source <path>`, or set $ERL_TOP to the \
                 repository root)",
                source.display()
            )));
        }
    }
    // Canonicalised so the report and the `{outdir, …}`-style expression carry one absolute
    // path per source rather than one relative to wherever the harness was started, and so the
    // compiler's error messages (if any) name the file unambiguously.
    for source in &mut sources {
        *source = fs::canonicalize(&*source).map_err(|error| {
            Failure::Error(format!("cannot resolve {}: {error}", source.display()))
        })?;
    }
    Ok(sources)
}

/// The source's file name, for the report's label.
fn file_name(source: &Path) -> String {
    source
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| source.display().to_string())
}

/// The module name the compile must answer, from the file stem.
fn module_of(source: &Path) -> Result<String, Failure> {
    source
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .ok_or_else(|| Failure::Error(format!("{} has no file stem", source.display())))
}

/// Parses the command line (see the module docs for the surface).
fn parse_args() -> Result<Config, Failure> {
    let mut sources: Vec<PathBuf> = Vec::new();
    let mut repeat = DEFAULT_REPEAT;
    let mut oracle = false;
    let mut args = env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "-h" | "--help" => return Err(Failure::Help),
            "--oracle" => oracle = true,
            "--source" => {
                let value = args.next().ok_or_else(|| {
                    Failure::Usage("`--source` needs a path".to_string())
                })?;
                sources.push(PathBuf::from(value));
            }
            "--repeat" => {
                let value = args.next().ok_or_else(|| {
                    Failure::Usage("`--repeat` needs a count".to_string())
                })?;
                repeat = parse_repeat(&value)?;
            }
            other => {
                if let Some(value) = other.strip_prefix("--source=") {
                    sources.push(PathBuf::from(value));
                } else if let Some(value) = other.strip_prefix("--repeat=") {
                    repeat = parse_repeat(value)?;
                } else {
                    return Err(Failure::Usage(format!("unexpected argument {other:?}")));
                }
            }
        }
    }
    Ok(Config {
        sources,
        repeat,
        oracle,
    })
}

/// A repeat count: at least 1 (a zero-repeat measurement has nothing to report).
fn parse_repeat(value: &str) -> Result<usize, Failure> {
    let count = value
        .parse::<usize>()
        .map_err(|_| Failure::Usage(format!("{value:?} is not a repeat count")))?;
    if count == 0 {
        return Err(Failure::Usage(
            "the repeat count must be at least 1".to_string(),
        ));
    }
    Ok(count)
}

/// The usage text (`-h`/`--help`, and on a malformed invocation).
fn print_usage() {
    println!("usage: compile_bench [--source <path>]... [--repeat <n>] [--oracle]");
    println!();
    println!("  --source <path>  compile this source instead of the default corpus (repeatable;");
    println!("                   the given order is kept and the first one is the ratio base)");
    println!("  --repeat <n>     fresh-runtime runs per source, fastest reported (default {DEFAULT_REPEAT})");
    println!("  --oracle         also run the system `erl` on the identical option list");
    println!("  -h, --help       this text");
    println!();
    println!("the default corpus, smallest first:");
    for source in DEFAULT_SOURCES {
        println!("  {source}");
    }
    println!();
    println!("environment:");
    println!("  ERL_TOP                 the repository root the default corpus is resolved against");
    println!("  BEAM_RT_PERF_COUNTERS   set to 1 to print the dispatch/machine-pool/walk census");
}

/// The run's banner: what will be measured, on which host, and how to read it.
fn print_header(config: &Config, sources: &[PathBuf], oracle: Option<&Path>) {
    println!("compile_bench: in-process `compile:file/2` (contract A.9) on beam_rt");
    println!(
        "options: {COMPILE_OPTIONS}\"<outdir>\"}}]  (fixed, so two runs measure the same work)"
    );
    println!(
        "sources: {} (repeat {}), oracle: {}",
        sources.len(),
        config.repeat,
        match oracle {
            Some(erl) => format!("{}", erl.display()),
            None => "off (pass `--oracle`)".to_string(),
        }
    );
    println!("host: {}  loadavg (start): {}", affinity(), loadavg());
    println!(
        "note: the host is shared, so read the RATIOS between rows; an absolute wall time can \
         move by +/-100 % between runs."
    );
    for source in sources {
        println!("  {}", source.display());
    }
}

/// The per-source line, printed as soon as that source finishes (so a long run shows progress).
fn print_row(row: &Row) {
    println!();
    println!(
        "{}: {} B source, {:.1} ms compile ({:.1} KB/s), boot {:.1} ms",
        row.name,
        row.bytes,
        milliseconds(row.sample.wall),
        kilobytes_per_second(row.bytes, row.sample.wall),
        milliseconds(row.sample.boot),
    );
    println!(
        "  erlang:statistics(garbage_collection)={{{},{},0}}  erlang:memory(processes_used)={} \
         (binary)={} (total)={}  artifact={} B",
        row.sample.gc_collections,
        row.sample.gc_words,
        row.sample.processes_used,
        row.sample.binary,
        row.sample.total,
        row.sample.artifact,
    );
    if let Some(oracle) = &row.oracle {
        println!(
            "  oracle erl: {:.1} ms wall, {:.1} ms in-VM compile (rust/oracle in-VM {:.2}x, \
             rust/oracle wall {:.2}x)",
            milliseconds(oracle.wall),
            milliseconds(oracle.in_vm),
            ratio(milliseconds(row.sample.wall), milliseconds(oracle.in_vm)),
            ratio(milliseconds(row.sample.wall), milliseconds(oracle.wall)),
        );
    }
}

/// The two report tables: the timing **shape** and the counters.
///
/// The reference row is the **smallest source** (the corpus's first entry by contract), so
/// `size x` / `time x` / `time/byte x` all read `1.00` there and every other row is relative
/// to it.
fn print_tables(rows: &[Row]) {
    let Some(base) = rows.iter().min_by_key(|row| row.bytes) else {
        return;
    };
    let base_wall = milliseconds(base.sample.wall);
    let base_bytes = base.bytes.max(1) as f64;

    println!();
    println!(
        "timing (reference row: {} - the smallest source; a linear compile keeps `time/byte x` \
         at ~1.00, a quadratic one makes it grow with `size x`)",
        base.name
    );
    println!(
        "{:<30} {:>10} {:>10} {:>9} {:>8} {:>8} {:>13}",
        "source", "bytes", "wall ms", "KB/s", "size x", "time x", "time/byte x"
    );
    println!("{}", "-".repeat(92));
    for row in rows {
        let wall = milliseconds(row.sample.wall);
        let bytes = row.bytes.max(1) as f64;
        println!(
            "{:<30} {:>10} {:>10.1} {:>9.1} {:>8.2} {:>8.2} {:>13.2}",
            row.name,
            row.bytes,
            wall,
            kilobytes_per_second(row.bytes, row.sample.wall),
            bytes / base_bytes,
            ratio(wall, base_wall),
            ratio(wall / bytes, base_wall / base_bytes),
        );
    }

    println!();
    println!(
        "counters (own-state, deterministic; the GC figures are delta'd across the compile so \
         the boot's own collections are excluded)"
    );
    println!(
        "{:<30} {:>9} {:>12} {:>11} {:>13} {:>12} {:>13} {:>9}",
        "source",
        "gc colls",
        "gc words",
        "words/B",
        "processes_used",
        "binary",
        "total",
        "artifact"
    );
    println!("{}", "-".repeat(115));
    for row in rows {
        let sample = &row.sample;
        println!(
            "{:<30} {:>9} {:>12} {:>11.1} {:>13} {:>12} {:>13} {:>9}",
            row.name,
            sample.gc_collections,
            sample.gc_words,
            sample.gc_words as f64 / row.bytes.max(1) as f64,
            sample.processes_used,
            sample.binary,
            sample.total,
            sample.artifact,
        );
    }

    let Some(largest) = rows.iter().max_by_key(|row| row.bytes) else {
        return;
    };
    if std::ptr::eq(largest, base) {
        println!();
        println!("shape: one size only, so no shape can be read - pass more sources.");
        return;
    }
    let size_ratio = largest.bytes.max(1) as f64 / base_bytes;
    let time_ratio = ratio(milliseconds(largest.sample.wall), base_wall);
    println!();
    println!(
        "shape: `{}` is {size_ratio:.2}x the size of `{}` and {time_ratio:.2}x its wall time, so \
         time-per-byte changed by {:.2}x (1.00 = linear, ~{size_ratio:.2}x = quadratic).",
        largest.name,
        base.name,
        time_ratio / size_ratio,
    );
}

/// `duration` in milliseconds.
fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// The compile's **source throughput**: bytes of source per second of wall clock, in KB/s.
///
/// A compile that is linear in source size holds this figure roughly constant as the corpus
/// grows; a super-linear one makes it fall.
fn kilobytes_per_second(bytes: u64, wall: Duration) -> f64 {
    let seconds = wall.as_secs_f64();
    if seconds > 0.0 {
        bytes as f64 / seconds / 1000.0
    } else {
        f64::INFINITY
    }
}

/// `numerator / denominator`, with a zero denominator reported as `inf` rather than as a panic.
fn ratio(numerator: f64, denominator: f64) -> f64 {
    if denominator > 0.0 {
        numerator / denominator
    } else {
        f64::INFINITY
    }
}

/// `/proc/loadavg`, or a note where the file does not exist.
fn loadavg() -> String {
    match fs::read_to_string("/proc/loadavg") {
        Ok(text) => text.trim().to_string(),
        Err(_) => "<no /proc/loadavg>".to_string(),
    }
}

/// This process's CPU affinity (`/proc/self/status`'s `Cpus_allowed_list`), so a figure taken
/// under `taskset` is recognisable.
fn affinity() -> String {
    match fs::read_to_string("/proc/self/status") {
        Ok(text) => text
            .lines()
            .find(|line| line.starts_with("Cpus_allowed_list:"))
            .map(|line| line.trim().to_string())
            .unwrap_or_else(|| "<unknown>".to_string()),
        Err(_) => "<unreadable>".to_string(),
    }
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

/// The first `name` on `$PATH` that is an executable file (a filesystem probe - the program is
/// only run when `--oracle` asked for it).
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// The harness's scratch root, removed when the run ends (including on an early failure).
struct TempDir(PathBuf);

impl TempDir {
    /// Creates a fresh, uniquely named scratch directory for this process.
    fn create() -> Result<TempDir, Failure> {
        let base = env::temp_dir().join(format!("beam_rt-compile-bench-{}", process::id()));
        fs::create_dir_all(&base)
            .map_err(|error| Failure::Error(format!("cannot create {}: {error}", base.display())))?;
        Ok(TempDir(base))
    }

    /// Creates (and answers) the child directory `label` under the scratch root.
    ///
    /// Each compile gets its own `{outdir, …}`, so a stale artifact from an earlier run can
    /// never be mistaken for the one this run wrote.
    fn child(&self, label: &str) -> Result<PathBuf, Failure> {
        let path = self.0.join(label);
        fs::create_dir_all(&path)
            .map_err(|error| Failure::Error(format!("cannot create {}: {error}", path.display())))?;
        Ok(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
