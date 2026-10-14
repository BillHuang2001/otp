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

//! `perf_call` - an **in-process** micro-benchmark for the *registered-BIF host path*: the
//! cost of one `beam_engine::ExecutionContext::call_bif` round trip through the real
//! [`RuntimeContext`](beam_rt::context::RuntimeContext), plus the `beam_bifs` BIF body that
//! serves it.
//!
//! # Recipe
//!
//! ```text
//! erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml -p beam_rt \
//!     --example perf_call [ITERATIONS]
//! erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml --release -p beam_rt \
//!     --example perf_call [ITERATIONS]
//! ```
//!
//! No CLI subprocess, no `-eval` parser and **no engine**: the harness holds a real
//! `RuntimeContext` and calls `call_bif` in a loop, so what is on the clock is the dispatch
//! prologue, the registered `beam_bifs` body, the result copy-out and the pooled-heap rewind
//! - and nothing else. That is the whole point: `examples/perf_bif.rs` measures a **native**
//! loop whose `N-1`/`N+1` are inlined by the baseline JIT and never reach `call_bif` at all,
//! so it cannot see this path.
//!
//! # The three measurement paths
//!
//! Every row is labelled with the path it measures, because the host-side cost differs:
//!
//! * **Path A** - `RuntimeContext::new`. Its `mailbox_table` field is `None`, so
//!   `call_registered`'s copy-out (`move_result_into_process_heap`) is **skipped by
//!   construction** (`mailbox_table.as_deref_mut()?` returns `None`) and a container result
//!   is left on the pooled BIF heap, which is then **not** rewound. This is the `-eval` /
//!   single-pass shape.
//! * **Path B** - `RuntimeContext::for_process` over a caller-owned `ProcessTable`. The
//!   `mailbox_table` is `Some`, so for a **container** result the copy-out runs (the result
//!   moves onto the process's own young heap through `beam_message::copy_struct`) and the
//!   pooled heap is rewound to this call's mark. This is the scheduler-pass shape.
//! * **Path C** - `Runtime::run_scheduled`: a compiled `.beam` executed as a **scheduled
//!   process** (the CLI's `run` route), measured with a two-point slope so the per-run
//!   process spawn / scheduler set-up cancels. It exercises the same copy-out as path B but
//!   end to end *through the engine*, so it also carries the engine's per-instruction cost.
//!
//! # What each row does and does NOT measure
//!
//! The rows are chosen so the two halves of `call_registered`'s tail separate cleanly:
//!
//! | row | result | exercises |
//! |---|---|---|
//! | `erlang:abs/1` (small int) | immediate | dispatch + BIF body + (rewind, no copy) |
//! | `erlang:trunc/1` (float) | immediate | as above, through the float path |
//! | `lists:reverse/1` (4-elem list) | **container** | + copy-out + rewind (path B) |
//! | `erlang:'++'/2` (2+2 list) | **container** | + copy-out + rewind (path B) |
//! | `lists:seq/2` (1..8) | **container** | + copy-out + rewind (path B) |
//! | `ets:insert/2` | immediate (`true`) | dispatch + BIF body + ETS store |
//! | `ets:lookup/2` (1 live object) | **container** | ETS store + copy-out (path B) |
//!
//! The absolute `ns/call` is **not** a signal on a shared, loaded host. What is a signal is
//! (a) the row-to-row ordering, (b) the marginal over the load-invariant **control** row (the
//! same loop shape with inlined arithmetic only, i.e. no BIF call at all), and (c) the
//! **B - A delta** for a container row, which is what the copy-out costs.
//!
//! What is **not** measured: the engine's own per-`call_ext` cost (~11-12 ns, pinned by
//! `beam_engine/tests/bif_cost.rs`), the baseline JIT, process start-up, and the `beam_bifs`
//! BIF body in isolation - a row's `ns/call` is dispatch **plus** that body.
//!
//! # Method
//!
//! `/proc/loadavg` is printed before and after every measurement block (this host is shared
//! and the loader is not under our control). Each row is warmed up and then timed
//! [`BEST_OF`] times; the **fastest** run is reported, because a timing can only be perturbed
//! upwards and the minimum is the best estimator available. A row whose first call does not
//! return the answer the oracle gives is a **loud error** (exit 1), never a bogus number: the
//! workload is its own self-check.
//!
//! A missing `erlc` (path C needs it to compile its fixture) is a **loud skip that exits 0**
//! - never a silent pass, and never a panic.

#![deny(unsafe_code)]
#![warn(missing_docs)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::time::{Duration, Instant};

use beam_engine::ExecutionContext;
use beam_process::{ProcessTable, SignalAtoms};
use beam_rt::{Runtime, RuntimeContext};
use beam_term::{is_boxed, is_list, make_small, signed_value, Eterm};

/// The default iteration count for the direct (`call_bif`) rows.
///
/// Each iteration is a full host round trip (~100 ns), so 100 000 is ~10 ms of work per timed
/// run: comfortably above the clock's resolution while keeping a container row's heap growth
/// (one copied result per call, ~5-20 words) around a few megabytes.
const DEFAULT_ITERATIONS: u64 = 100_000;

/// How many timed runs are kept at each direct measurement; the **fastest** is used.
const BEST_OF: usize = 5;

/// How many untimed runs warm each direct measurement (the first call of a row interns the
/// names the registry lookup touches and faults the BIF's code in).
const WARMUP: usize = 1;

/// The low iteration count of path C's two-point slope.
const SLOPE_N1: u64 = 50_000;

/// The high point of path C's slope is `SLOPE_N1 * SLOPE_SPAN`.
const SLOPE_SPAN: u64 = 4;

/// How many times path C's slope points may be scaled up when no positive slope is found.
const MAX_SLOPE_GROWTH: u32 = 3;

/// The fixture path C compiles and runs; `rev(N, [1,2,3])` performs **two** registered BIF
/// calls per iteration - one container-returning (`lists:reverse/1`, which takes the copy-out)
/// and one immediate (`erlang:length/1`) - and returns `N * 3`, so the workload is its own
/// self-check.
const FIXTURE_SOURCE: &str = "\
-module(nb).
-export([rev/2]).

rev(N, L) -> rev(N, L, 0).

rev(0, _L, A) -> A;
rev(N, L, A) -> rev(N - 1, L, A + length(lists:reverse(L))).
";

/// How the harness ended. A prerequisite that is absent is a `Skip` (a loud banner and exit
/// 0); everything else is a failure the caller reports.
enum Failure {
    /// A prerequisite is missing - loud skip, exit 0, never a silent pass.
    Skip(String),
    /// The invocation itself was malformed - exit 2 with the usage text.
    Usage(String),
    /// `-h`/`--help` - print the usage text and exit 0.
    Help,
    /// The measurement could not be taken or a row misbehaved - exit 1.
    Error(String),
}

fn main() {
    match run() {
        Ok(()) => {}
        Err(Failure::Skip(why)) => skip_loudly(&why),
        Err(Failure::Help) => print_usage(),
        Err(Failure::Usage(why)) => {
            eprintln!("perf_call: {why}");
            print_usage();
            process::exit(2);
        }
        Err(Failure::Error(why)) => {
            eprintln!("perf_call: {why}");
            process::exit(1);
        }
    }
}

/// What a row's first call must return, so a broken row is an error and not a number.
#[derive(Clone, Copy)]
enum Expect {
    /// A small integer with this value.
    Small(i64),
    /// This exact atom term (resolved from the runtime's own shared table).
    Atom(Eterm),
    /// A list (or other boxed container) - checked structurally, never by value.
    Container,
}

/// One measured registered-BIF call: the `(module, function, arity)` key, its stable
/// argument terms and the answer the oracle gives.
struct Call {
    /// The label the tables print.
    label: &'static str,
    /// The BIF's module name.
    module: &'static str,
    /// The BIF's function name.
    function: &'static str,
    /// The BIF's arity (must equal `args.len()`).
    arity: u32,
    /// The argument terms. They live on the runtime's long-lived `-eval` heap (or are
    /// immediates), so they stay valid for the whole run.
    args: Vec<Eterm>,
    /// What the first call must return.
    expect: Expect,
}

impl Call {
    /// A human name for the expectation, for the error message.
    fn expect_kind(&self) -> &'static str {
        match self.expect {
            Expect::Small(_) => "small integer",
            Expect::Atom(_) => "atom",
            Expect::Container => "container",
        }
    }
}

/// One direct row's measurement.
struct Row {
    /// The BIF key.
    label: &'static str,
    /// Best-of-[`BEST_OF`] nanoseconds per call.
    ns_per_call: f64,
    /// The row's checksum, computed from every call's raw word so the loop cannot be
    /// optimised away.
    checksum: usize,
}

impl Row {
    /// The marginal over `control`: how much of this row is the BIF call itself.
    fn marginal(&self, control: f64) -> f64 {
        self.ns_per_call - control
    }
}

/// Measures every block and prints the tables.
fn run() -> Result<(), Failure> {
    let iterations = parse_iterations()?;
    // Path C needs `erlc` to compile its fixture. Checked **before** anything is built, so a
    // toolchain-less host gets one loud banner instead of a half-finished table.
    let erlc = find_on_path("erlc").ok_or_else(|| {
        Failure::Skip("no `erlc` on $PATH, so path C's fixture cannot be compiled".to_string())
    })?;

    let mut rt = Runtime::with_defaults()
        .map_err(|error| Failure::Error(format!("the default runtime: {error}")))?;
    rt.boot()
        .map_err(|error| Failure::Error(format!("the runtime could not boot: {error}")))?;

    let dir = TempDir::create()?;
    let beam = compile_fixture(&erlc, dir.path())?;

    // The argument terms every row reuses. `Runtime::eval` materializes them on the runtime's
    // long-lived `-eval` heap, which Milestone 1 never resets, so they stay valid for the run.
    // `1.5` drives `trunc/1` through the **float** path (`make_small` cannot build a float).
    let reverse_arg = eval(&mut rt, "[1,2,3,4]")?;
    let plusplus_args = vec![eval(&mut rt, "[1,2]")?, eval(&mut rt, "[3,4]")?];
    let seq_args = vec![make_small(1), make_small(8)];
    let abs_arg = make_small(-3);
    let float_arg = eval(&mut rt, "1.5")?;
    let atom_true = eval(&mut rt, "true")?;
    let table_name = eval(&mut rt, "nb_tab")?;
    let table_opts = eval(&mut rt, "[named_table]")?;
    let insert_arg = eval(&mut rt, "{k,1}")?;
    let key = eval(&mut rt, "k")?;

    let abs = Call {
        label: "erlang:abs/1",
        module: "erlang",
        function: "abs",
        arity: 1,
        args: vec![abs_arg],
        expect: Expect::Small(3),
    };
    let trunc = Call {
        label: "erlang:trunc/1",
        module: "erlang",
        function: "trunc",
        arity: 1,
        args: vec![float_arg],
        expect: Expect::Small(1),
    };
    let reverse = Call {
        label: "lists:reverse/1",
        module: "lists",
        function: "reverse",
        arity: 1,
        args: vec![reverse_arg],
        expect: Expect::Container,
    };
    let plusplus = Call {
        label: "erlang:'++'/2",
        module: "erlang",
        function: "++",
        arity: 2,
        args: plusplus_args,
        expect: Expect::Container,
    };
    let seq = Call {
        label: "lists:seq/2",
        module: "lists",
        function: "seq",
        arity: 2,
        args: seq_args,
        expect: Expect::Container,
    };

    // The ETS table is created once, **outside** the timed rows: `ets:new/2` registers a fresh
    // table per call, so timing it in a loop would measure table allocation and leak 100 000
    // tables rather than the dispatch path. A `named_table`'s handle is its **name atom**, so
    // `table_name` is the handle both paths pass to `ets:insert/2` and `ets:lookup/2`, and a
    // `named_table` is global in the runtime's registry, so path B reaches the same table.
    let table_handle = {
        let mut ctx = RuntimeContext::new(&mut rt);
        let new = Call {
            label: "ets:new/2",
            module: "ets",
            function: "new",
            arity: 2,
            args: vec![table_name, table_opts],
            expect: Expect::Atom(table_name),
        };
        let handle = call(&mut ctx, &new)?;
        check(&new, handle)?;
        let insert = Call {
            label: "ets:insert/2",
            module: "ets",
            function: "insert",
            arity: 2,
            args: vec![handle, insert_arg],
            expect: Expect::Atom(atom_true),
        };
        let _ = call(&mut ctx, &insert)?;
        handle
    };
    let insert = Call {
        label: "ets:insert/2",
        module: "ets",
        function: "insert",
        arity: 2,
        args: vec![table_handle, insert_arg],
        expect: Expect::Atom(atom_true),
    };
    let lookup = Call {
        label: "ets:lookup/2",
        module: "ets",
        function: "lookup",
        arity: 2,
        args: vec![table_handle, key],
        expect: Expect::Container,
    };

    print_header(iterations, &beam, &erlc);

    let control = measure_control(iterations);

    // ---- block A: the `-eval` / single-pass shape (copy-out skipped by construction) ----
    let mut rows_a = Vec::new();
    {
        let mut ctx = RuntimeContext::new(&mut rt);
        for spec in [&abs, &trunc, &reverse, &plusplus, &seq] {
            rows_a.push(measure(&mut ctx, spec, iterations)?);
        }
    }
    let mut rows_a_ets = Vec::new();
    {
        let mut ctx = RuntimeContext::new(&mut rt);
        for spec in [&insert, &lookup] {
            rows_a_ets.push(measure(&mut ctx, spec, iterations)?);
        }
    }

    // ---- block B: the scheduler-pass shape (copy-out + pooled rewind ON) ----
    let mut rows_b = Vec::new();
    let mut rows_b_ets = Vec::new();
    {
        // A caller-owned table: `for_process` requires the pid it runs as to live in it. The
        // signal vocabulary is interned into the runtime's own shared table so the spawned
        // process's ids agree with the runtime's.
        let signals = SignalAtoms::from_table(rt.code_server_atoms_mut());
        let mut table = ProcessTable::new(signals);
        let pid = table
            .spawn_default()
            .map_err(|error| Failure::Error(format!("could not spawn a process: {error}")))?;
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        for spec in [&abs, &reverse, &seq] {
            rows_b.push(measure(&mut ctx, spec, iterations)?);
        }
        for spec in [&insert, &lookup] {
            rows_b_ets.push(measure(&mut ctx, spec, iterations)?);
        }
    }

    print_block(
        "block A: RuntimeContext::new - copy-out SKIPPED by construction (the `-eval` shape)",
        iterations,
        &rows_a,
        control,
    );
    print_block(
        "block A2: the same context, the ETS rows (table created once during set-up)",
        iterations,
        &rows_a_ets,
        control,
    );
    print_block(
        "block B: RuntimeContext::for_process - copy-out + pooled rewind ON (the scheduler shape)",
        iterations,
        &rows_b,
        control,
    );
    print_block(
        "block B2: the same context, the ETS rows",
        iterations,
        &rows_b_ets,
        control,
    );
    print_delta(&rows_a, &rows_a_ets, &rows_b, &rows_b_ets);

    // ---- block C: the scheduled `run` route, two-point slope ----
    let list3 = eval(&mut rt, "[1,2,3]")?;
    measure_slope(&mut rt, &beam, list3)?;

    println!();
    println!("loadavg (end): {}", loadavg());
    Ok(())
}

// --- measurement ------------------------------------------------------------------

/// One `call_bif` through `ctx`, mapped to the harness's error type.
fn call(ctx: &mut RuntimeContext<'_>, spec: &Call) -> Result<Eterm, Failure> {
    ctx.call_bif(spec.module, spec.function, spec.arity, &spec.args)
        .map_err(|error| {
            Failure::Error(format!(
                "{}:{}/{} raised {error}",
                spec.module, spec.function, spec.arity
            ))
        })
}

/// Verifies `spec`'s first answer against the oracle's, then times `iterations` calls and
/// returns the fastest sample's ns/call.
fn measure(ctx: &mut RuntimeContext<'_>, spec: &Call, iterations: u64) -> Result<Row, Failure> {
    let first = call(ctx, spec)?;
    check(spec, first)?;
    for _ in 0..WARMUP {
        let _ = call(ctx, spec)?;
    }
    let mut best = f64::INFINITY;
    let mut checksum = 0usize;
    for _ in 0..BEST_OF {
        let mut acc = 0usize;
        let started = Instant::now();
        for _ in 0..iterations {
            acc = acc.wrapping_add(call(ctx, spec)?.raw());
        }
        let elapsed = started.elapsed();
        checksum = std::hint::black_box(acc);
        best = best.min(elapsed.as_nanos() as f64 / iterations as f64);
    }
    Ok(Row {
        label: spec.label,
        ns_per_call: best,
        checksum,
    })
}

/// The load-invariant control: the **same loop shape** with inlined arithmetic only, so the
/// marginal of every row over this figure is what the BIF call itself costs on this host
/// under this load. It calls nothing at all - no registry, no context, no heap.
fn measure_control(iterations: u64) -> f64 {
    let mut best = f64::INFINITY;
    for _ in 0..BEST_OF {
        let mut acc = 0usize;
        let started = Instant::now();
        for i in 0..iterations {
            acc = acc.wrapping_add((i & 1) as usize);
        }
        let elapsed = started.elapsed();
        std::hint::black_box(acc);
        best = best.min(elapsed.as_nanos() as f64 / iterations as f64);
    }
    best
}

/// Asserts `term` is what the oracle returns for `spec`.
fn check(spec: &Call, term: Eterm) -> Result<(), Failure> {
    let ok = match spec.expect {
        Expect::Small(value) => signed_value(term) == value,
        Expect::Atom(expected) => term == expected,
        Expect::Container => is_list(term) || is_boxed(term),
    };
    if ok {
        Ok(())
    } else {
        Err(Failure::Error(format!(
            "{} answered {:#x}, which is not the oracle's {}",
            spec.label,
            term.raw(),
            spec.expect_kind()
        )))
    }
}

// --- path C: the two-point slope ----------------------------------------------------

/// Measures the compiled fixture as a **scheduled process** through
/// [`Runtime::run_scheduled`] at two large iteration counts and prints the slope.
///
/// The per-run cost (spawning the process, the scheduler's first slice, the module lookup) is
/// added to *both* points and cancels in the difference, exactly as `examples/perf_bif.rs`
/// cancels the CLI's process start-up.
fn measure_slope(rt: &mut Runtime, beam: &Path, list3: Eterm) -> Result<(), Failure> {
    println!();
    println!("== block C: the scheduled `run` route (Runtime::run_scheduled), two-point slope ==");
    println!("loadavg before: {}", loadavg());
    let mut n1 = SLOPE_N1;
    for _ in 0..=MAX_SLOPE_GROWTH {
        let n2 = n1 * SLOPE_SPAN;
        let t1 = time_scheduled(rt, beam, n1, list3)?;
        let t2 = time_scheduled(rt, beam, n2, list3)?;
        if t2 > t1 {
            let span = t2 - t1;
            let ns_per_iteration = span.as_nanos() as f64 / (n2 - n1) as f64;
            println!(
                "  N1 = {n1} ({:.3} ms)   N2 = {n2} ({:.3} ms)   slope = {ns_per_iteration:.2} ns/iteration",
                t1.as_secs_f64() * 1000.0,
                t2.as_secs_f64() * 1000.0,
            );
            println!("  ns/iteration = (t(N2) - t(N1)) / (N2 - N1)");
            println!(
                "  Each iteration is ONE `lists:reverse/1` call_ext (a container, so the copy-out"
            );
            println!(
                "  fires) plus ONE `erlang:length/1` call_ext plus the engine's instructions around"
            );
            println!("  them, so no single BIF's cost is isolated here.");
            println!("loadavg after: {}", loadavg());
            return Ok(());
        }
        println!("  N1 = {n1}/{n2}: no positive slope (below this host's resolution), scaling up");
        n1 *= SLOPE_SPAN;
    }
    Err(Failure::Error(
        "the scheduled route's slope never became positive; nothing was measured".to_string(),
    ))
}

/// The fastest of [`BEST_OF`] `run_scheduled` calls at `n`, after [`WARMUP`] discarded runs.
fn time_scheduled(
    rt: &mut Runtime,
    beam: &Path,
    n: u64,
    list3: Eterm,
) -> Result<Duration, Failure> {
    let args = [make_small(n as i64), list3];
    let expected = (n as i64) * 3;
    let check = |value: Eterm| -> Result<(), Failure> {
        if signed_value(value) == expected {
            Ok(())
        } else {
            Err(Failure::Error(format!(
                "nb:rev/2 at N = {n} answered {} instead of the oracle's {expected}",
                signed_value(value)
            )))
        }
    };
    for _ in 0..WARMUP {
        let value = rt
            .run_scheduled(beam, "rev", 2, &args)
            .map_err(|error| Failure::Error(format!("run_scheduled at N = {n}: {error}")))?;
        check(value)?;
    }
    let mut best = Duration::MAX;
    for _ in 0..BEST_OF {
        let started = Instant::now();
        let value = rt
            .run_scheduled(beam, "rev", 2, &args)
            .map_err(|error| Failure::Error(format!("run_scheduled at N = {n}: {error}")))?;
        let elapsed = started.elapsed();
        check(value)?;
        best = best.min(elapsed);
    }
    Ok(best)
}

// --- printing ---------------------------------------------------------------------

/// The run's configuration.
fn print_header(iterations: u64, beam: &Path, erlc: &Path) {
    println!("beam_rt registered-BIF host-call micro-benchmark (in-process)");
    println!("  iterations:        {iterations} per timed run (warm-up {WARMUP}, best of {BEST_OF})");
    println!(
        "  fixture (path C):  {} (compiled by {})",
        beam.display(),
        erlc.display()
    );
    println!("  loadavg (start):   {}", loadavg());
    println!();
    println!("  Path A = RuntimeContext::new         mailbox_table = None -> copy-out skipped, no rewind");
    println!("  Path B = RuntimeContext::for_process mailbox_table = Some -> copy-out + pooled rewind");
    println!("  Path C = Runtime::run_scheduled      a compiled .beam as a scheduled process (slope)");
    println!("  control = the same loop with inlined arithmetic only (no BIF call at all)");
    println!();
}

/// One block's table, with `/proc/loadavg` printed around it.
fn print_block(title: &str, iterations: u64, rows: &[Row], control: f64) {
    println!();
    println!("== {title} ==");
    println!("loadavg before: {}", loadavg());
    println!(
        "  {:<18} {:>12} {:>12} {:>14} {:>12}",
        "row", "ns/call", "control_ns", "marginal_ns", "checksum"
    );
    for row in rows {
        println!(
            "  {:<18} {:>12.2} {:>12.2} {:>14.2} {:#14x}",
            row.label,
            row.ns_per_call,
            control,
            row.marginal(control),
            row.checksum
        );
    }
    println!(
        "  ({iterations} calls per sample; marginal = ns/call - control, i.e. the BIF call itself)"
    );
    println!("loadavg after: {}", loadavg());
}

/// The copy-out delta: every row measured under both paths, B minus A.
fn print_delta(rows_a: &[Row], rows_a_ets: &[Row], rows_b: &[Row], rows_b_ets: &[Row]) {
    println!();
    println!("== copy-out delta (path B - path A), i.e. what the copy-out + rewind costs ==");
    println!("loadavg: {}", loadavg());
    println!(
        "  {:<18} {:>12} {:>12} {:>12} {:>12}",
        "row", "A ns/call", "B ns/call", "delta_ns", "B/A"
    );
    for a in rows_a {
        if let Some(b) = rows_b.iter().find(|b| b.label == a.label) {
            print_delta_row(a, b);
        }
    }
    for a in rows_a_ets {
        if let Some(b) = rows_b_ets.iter().find(|b| b.label == a.label) {
            print_delta_row(a, b);
        }
    }
    println!("  A container row's delta is the copy-out (+ the rewind it gates); an immediate row's");
    println!("  delta is the mark-based rewind alone. A negative delta is host noise, not a speed-up.");
}

/// One `print_delta` line.
fn print_delta_row(a: &Row, b: &Row) {
    println!(
        "  {:<18} {:>12.2} {:>12.2} {:>12.2} {:>12.3}",
        a.label,
        a.ns_per_call,
        b.ns_per_call,
        b.ns_per_call - a.ns_per_call,
        b.ns_per_call / a.ns_per_call
    );
}

/// The loud skip banner - a missing prerequisite is never a silent pass.
fn skip_loudly(why: &str) {
    eprintln!();
    eprintln!("============ beam_rt perf_call harness SKIPPED ============");
    eprintln!("{why}");
    eprintln!("Install an Erlang/OTP toolchain on $PATH and re-run:");
    eprintln!("  erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml \\");
    eprintln!("      -p beam_rt --example perf_call");
    eprintln!("NOT a pass: nothing was measured.");
    eprintln!("==========================================================");
    eprintln!();
}

/// The usage text (`-h`/`--help`, and on a malformed invocation).
fn print_usage() {
    println!("usage: perf_call [ITERATIONS]");
    println!();
    println!("  ITERATIONS  calls per timed run for the direct rows (default {DEFAULT_ITERATIONS})");
}

/// The iteration count from `argv[1]`, or [`DEFAULT_ITERATIONS`].
fn parse_iterations() -> Result<u64, Failure> {
    let mut args = env::args().skip(1);
    let raw = match args.next() {
        Some(raw) => raw,
        None => return Ok(DEFAULT_ITERATIONS),
    };
    if raw == "-h" || raw == "--help" {
        return Err(Failure::Help);
    }
    if args.next().is_some() {
        return Err(Failure::Usage(
            "too many arguments (only the iteration count is accepted)".to_string(),
        ));
    }
    let n = raw
        .parse::<u64>()
        .map_err(|_| Failure::Usage(format!("{raw:?} is not an iteration count")))?;
    if n == 0 {
        return Err(Failure::Usage(
            "the iteration count must be at least 1".to_string(),
        ));
    }
    Ok(n)
}

/// `/proc/loadavg`, or `"<unreadable>"` where the file does not exist. This host is shared,
/// so the load every block ran under is part of the record.
fn loadavg() -> String {
    match fs::read_to_string("/proc/loadavg") {
        Ok(text) => text.trim().to_string(),
        Err(_) => "<unreadable>".to_string(),
    }
}

/// `rt.eval(expr)` mapped to the harness's error type.
fn eval(rt: &mut Runtime, expr: &str) -> Result<Eterm, Failure> {
    rt.eval(expr)
        .map_err(|error| Failure::Error(format!("cannot materialize `{expr}`: {error}")))
}

// --- the fixture ------------------------------------------------------------------

/// The fixture directory, removed when the harness ends (including on an early skip).
struct TempDir(PathBuf);

impl TempDir {
    /// Creates `<tmp>/beam_rt-perf-call-<pid>/`.
    fn create() -> Result<TempDir, Failure> {
        let base = env::temp_dir().join(format!("beam_rt-perf-call-{}", process::id()));
        fs::create_dir_all(&base)
            .map_err(|error| Failure::Error(format!("cannot create {}: {error}", base.display())))?;
        Ok(TempDir(base))
    }

    /// The directory.
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Writes `nb.erl` into `dir` and compiles it with the system `erlc`, returning the `.beam`.
fn compile_fixture(erlc: &Path, dir: &Path) -> Result<PathBuf, Failure> {
    let source = dir.join("nb.erl");
    fs::write(&source, FIXTURE_SOURCE)
        .map_err(|error| Failure::Error(format!("cannot write {}: {error}", source.display())))?;
    let output = Command::new(erlc)
        .arg("-o")
        .arg(dir)
        .arg(&source)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| Failure::Error(format!("cannot run {}: {error}", erlc.display())))?;
    if !output.status.success() {
        return Err(Failure::Error(format!(
            "erlc failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let beam = dir.join("nb.beam");
    if !beam.is_file() {
        return Err(Failure::Error(format!("erlc produced no {}", beam.display())));
    }
    Ok(beam)
}

/// The first `name` on `$PATH` that is an executable file (a filesystem probe - the program
/// is never run here).
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
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
