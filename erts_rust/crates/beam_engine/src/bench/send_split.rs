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

//! The two-process copy-bound **split**: `copy_bound`'s `send_recv` versus `send_recv_plain`,
//! decomposed per stage **in process**.
//!
//! `tools/bench` publishes one figure for that pair — the marginal `send_recv −
//! send_recv_plain` — but its loop contains `beam_rt`'s CLI boot, its scheduler, its copy-out
//! glue and `beam_message`'s copy engine, and none of those is separable from the outside. This
//! harness drives the same two-process ping-pong over the engine's own seams
//! ([`ExecutionContext`](crate::context::ExecutionContext) + [`Mailbox`](crate::context::Mailbox)),
//! so the **engine-side** stages are counted exactly (deterministic host-side tallies, not
//! timings) and priced against a wall clock measured in the same process.
//!
//! # What is measured, and what cannot be
//!
//! Three legs, identical in shape except for how the worker answers the payload it was sent:
//!
//! * **`plain`** — `T = payload()` where every constant in the payload is literal-able, so the
//!   compiler hoists the whole map into the module's literal area (`beam_ssa_codegen`'s literal
//!   collection) and the per-iteration cost is one literal `move`. This is the shape of
//!   `copy_bound/send_recv_plain`.
//! * **`heap`** — `T = payload(N)`, one payload field derived from the loop counter, so the map
//!   **cannot** be a literal and is built on the heap by `put_map_assoc` on every iteration.
//!   This is the shape `copy_bound/send_recv` reaches *after* its worker's `binary_to_term`
//!   rebuilds the payload from the ETF image: a heap-resident, unshared term.
//! * **`codec`** — the heap payload *plus* the two host crossings the real worker pays
//!   (`bench_codec:encode/1` then `bench_codec:decode/1`), served by the harness' hook as
//!   **identity** stubs. The codec's own encode/decode work is `beam_codec`'s and is not
//!   reproduced here; the leg's marginal over `heap` is the engine's share of those two
//!   `call_bif` crossings (argument marshalling + the host call itself), which is exactly the
//!   part of `term_to_binary`/`binary_to_term` this crate owns.
//!
//! The marginal `heap − plain` is therefore the payload's **residency + construction** cost as
//! the engine and its host see it, and `codec − heap` is the **crossing** cost of the codec pair.
//! Three things are deliberately **not** here, and the report says so rather than guessing:
//!
//! 1. **`beam_message::copy_struct` does not run at all.** Both "processes" are
//!    [`MinimalContext`](crate::context::MinimalContext) region heaps in one OS process, and the
//!    [`Mailbox`](crate::context::Mailbox) seam hands the message term over *verbatim* (no copy,
//!    no second heap), so the copy engine's objects/words are a cross-crate figure the sibling
//!    harness records — here the value is the **zero** that makes the split visible.
//! 2. **The ETF codec's own work is not executed.** `term_to_binary/1`/`binary_to_term/1`'s
//!    encoding lives in `beam_bifs`/`beam_codec`; the `codec` leg prices the two crossings and
//!    nothing of the image, so the codec's ns come from the project record, never from here.
//! 3. **No collector.** `MinimalContext` only grows its reserved heap, so a collection-free
//!    figure is reported and GC cadence is out of scope here.
//!
//! # Reproduce
//!
//! ```text
//! taskset -c 3 setsid -w erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
//!     -p beam_engine --lib -- --ignored --nocapture send_recv_split_decomposition
//! ```
//!
//! `BENCH_ITERS`/`BENCH_BATCH`/`BENCH_WARMUP` override the op counts and `BENCH_TIER` selects
//! `interpreter`/`baseline`/`both` (the same knobs the sibling workloads take).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use beam_loader::{load_file, Module};
use beam_term::{make_small, Eterm, NIL};

use crate::context::BifHook;
use crate::jit::Tier;
use crate::machine::{Machine, RunOutcome};

use super::counting::{Counters, CountingContext};
use super::{atom_named, compile_fixture, env_usize, selected_tiers};
/// The fixture module the legs are compiled from.
const MODULE: &str = "bench_send_split";

/// The driver process's "pid" (a small integer: the mailbox routes on a word compare, the
/// engine never inspects a pid term).
const DRIVER_PID: i64 = 1;

/// The worker process's "pid".
const WORKER_PID: i64 = 2;

/// Default measured ping-pong round trips (one round trip = `copy_bound`'s "op").
const DEFAULT_OPS: usize = 10_000;

/// Default round trips per fresh machine/host batch.
const DEFAULT_BATCH: usize = 2_500;

/// Default untimed warmup round trips.
const DEFAULT_WARMUP: usize = 1_000;

/// The measured phase is this many timed sub-runs (min/median are reported over them).
const SUB_RUNS: usize = 5;

/// Round trips in a baseline-census (recording) pass: recording re-formats one opcode name per
/// deopt, so the pass is small — the counts are per op, so the size does not change them.
const CENSUS_OPS: usize = 64;

/// How many `deopt_count[...]` rows a report prints.
const TOP_ROWS: usize = 10;

/// The fixture source: `copy_bound`'s two-process shape, both legs.
///
/// `payload/0` is the `tools/bench` `copy_bound` payload verbatim (a 5-key map holding a
/// 10-element list, a `{ok, <<"hello world">>, 3.5}` tuple, a 12-byte binary, a char-list string
/// and a negative integer); `payload/1` is the same map with one field derived from the loop
/// counter, so it cannot be constant-folded into the literal area. The worker bodies are the
/// benchmark's `worker_plain`/`worker_heap`: reply the payload directly, i.e. **no** codec call,
/// which is exactly what isolates payload residency from the codec.
const SOURCE: &str = r#"-module(bench_send_split).
-export([plain/3, heap/3, codec/3, worker_plain/0, worker_heap/0, worker_codec/0]).

%% The literal payload: every field is a compile-time constant, so the whole map is hoisted
%% into the module's literal area and a per-iteration `payload()` costs one literal move.
payload() ->
    #{a => [1,2,3,4,5,6,7,8,9,10],
      b => {ok, <<"hello world">>, 3.5},
      c => <<1,2,3,4,5,6,7,8,9,10,11,12>>,
      d => "a char list",
      e => -42}.

%% The same payload with one counter-derived field: not literal-able, so the map is built on the
%% heap on every iteration (`put_map_assoc`).
payload(N) ->
    #{a => [1,2,3,4,5,6,7,8,9,10],
      b => {ok, <<"hello world">>, 3.5},
      c => <<1,2,3,4,5,6,7,8,9,10,11,12>>,
      d => "a char list",
      e => N}.
plain(N, Worker, Acc) -> plain_loop(N, Worker, Acc).
plain_loop(0, _Worker, Acc) -> Acc;
plain_loop(N, Worker, Acc) ->
    T = payload(),
    Worker ! {self(), T},
    receive
        {Worker, T2} ->
            case T2 of
                T -> plain_loop(N - 1, Worker, Acc + 1)
            end
    after 30000 ->
        exit(message_timeout)
    end.

heap(N, Worker, Acc) -> heap_loop(N, Worker, Acc).
heap_loop(0, _Worker, Acc) -> Acc;
heap_loop(N, Worker, Acc) ->
    T = payload(N),
    Worker ! {self(), T},
    receive
        {Worker, T2} ->
            case T2 of
                T -> heap_loop(N - 1, Worker, Acc + 1)
            end
    after 30000 ->
        exit(message_timeout)
    end.

worker_plain() ->
    receive
        {From, T} -> From ! {self(), T}, worker_plain();
        stop -> ok
    end.

worker_heap() ->
    receive
        {From, T} -> From ! {self(), T}, worker_heap();
        stop -> ok
    end.

%% The same loop over the heap payload, with the two host crossings the real worker pays around
%% its reply (`binary_to_term(term_to_binary(T))`). The harness serves `bench_codec:encode/1` and
%% `bench_codec:decode/1` as identity stubs, so this leg prices the crossings only.
codec(N, Worker, Acc) -> codec_loop(N, Worker, Acc).
codec_loop(0, _Worker, Acc) -> Acc;
codec_loop(N, Worker, Acc) ->
    T = payload(N),
    Worker ! {self(), T},
    receive
        {Worker, T2} ->
            case T2 of
                T -> codec_loop(N - 1, Worker, Acc + 1)
            end
    after 30000 ->
        exit(message_timeout)
    end.

worker_codec() ->
    receive
        {From, T} ->
            Up = bench_codec:encode(T),
            Down = bench_codec:decode(Up),
            From ! {self(), Down},
            worker_codec();
        stop -> ok
    end.
"#;

/// One leg of the split: the driver entry, its worker, and what the leg's payload is.
struct Leg {
    /// The leg's name, as printed.
    name: &'static str,
    /// The driver entry point (`/3`, called with `(Ops, WorkerPid, 0)`).
    entry: &'static str,
    /// The worker entry point (`/0`).
    worker: &'static str,
    /// How the payload reaches the loop, as printed.
    payload: &'static str,
}

/// The three legs, in report order.
const LEGS: &[Leg] = &[
    Leg {
        name: "plain",
        entry: "plain",
        worker: "worker_plain",
        payload: "literal (hoisted into the module's literal area, one move per op)",
    },
    Leg {
        name: "heap",
        entry: "heap",
        worker: "worker_heap",
        payload: "built on the heap per op (put_map_assoc, unshared)",
    },
    Leg {
        name: "codec",
        entry: "codec",
        worker: "worker_codec",
        payload: "heap payload + the two codec host crossings (identity stubs, no image)",
    },
];

// ---------------------------------------------------------------------------
// The two-process router (two inboxes, two pids)
// ---------------------------------------------------------------------------

/// The two "processes'" inboxes and their pids.
struct Router {
    /// The driver's inbox.
    driver_inbox: Rc<RefCell<Vec<Eterm>>>,
    /// The worker's inbox.
    worker_inbox: Rc<RefCell<Vec<Eterm>>>,
    /// The driver's pid term.
    driver_pid: Eterm,
    /// The worker's pid term.
    worker_pid: Eterm,
}

impl Router {
    /// A fresh router with both inboxes empty.
    fn new() -> Router {
        Router {
            driver_inbox: Rc::new(RefCell::new(Vec::new())),
            worker_inbox: Rc::new(RefCell::new(Vec::new())),
            driver_pid: make_small(DRIVER_PID),
            worker_pid: make_small(WORKER_PID),
        }
    }
}

/// The mailbox seam over the two inboxes: a `send` addressed to the peer pid lands in the peer's
/// inbox, everything else in the sender's own.
///
/// This is a **delivery** seam, not a copy boundary: the message term is stored verbatim (see
/// the module docs' point 1), so the copy engine is out of this harness' path by construction.
struct PeeredMailbox {
    /// This process's inbox.
    own: Rc<RefCell<Vec<Eterm>>>,
    /// The counterpart's inbox (where a `send` addressed to the peer pid lands).
    peer: Rc<RefCell<Vec<Eterm>>>,
    /// The peer's pid term.
    peer_pid: Eterm,
    /// This process's scan position (`loop_rec_end` advances it, `remove_message` rewinds it).
    scan: Rc<Cell<usize>>,
}

impl crate::context::Mailbox for PeeredMailbox {
    fn send_message(&mut self, to: Eterm, message: Eterm) -> beam_common::Result<()> {
        if to == self.peer_pid {
            self.peer.borrow_mut().push(message);
        } else {
            self.own.borrow_mut().push(message);
        }
        Ok(())
    }

    fn receive_next(&mut self) -> beam_common::Result<Option<Eterm>> {
        let scan = self.scan.get();
        Ok(self.own.borrow().get(scan).copied())
    }

    fn receive_skip(&mut self) -> beam_common::Result<()> {
        self.scan.set(self.scan.get() + 1);
        Ok(())
    }

    fn receive_remove(&mut self) -> beam_common::Result<()> {
        let scan = self.scan.get();
        let mut own = self.own.borrow_mut();
        if scan < own.len() {
            own.remove(scan);
        }
        self.scan.set(0);
        Ok(())
    }

    fn receive_reset(&mut self) -> beam_common::Result<()> {
        self.scan.set(0);
        Ok(())
    }

    // This shape carries no `recv_marker_*` instruction, so the marker family keeps the seam's
    // honest typed "cannot serve it" answer (never a fabricated marker).
    fn receive_marker_reserve(&mut self) -> beam_common::Result<Eterm> {
        Err(crate::error::unsupported("receive_marker_reserve"))
    }

    fn receive_marker_bind(
        &mut self,
        _marker: Eterm,
        _reference: Eterm,
    ) -> beam_common::Result<()> {
        Err(crate::error::unsupported("receive_marker_bind"))
    }

    fn receive_marker_use(&mut self, _reference: Eterm) -> beam_common::Result<()> {
        Err(crate::error::unsupported("receive_marker_use"))
    }

    fn receive_marker_clear(&mut self, _reference: Eterm) -> beam_common::Result<()> {
        Err(crate::error::unsupported("receive_marker_clear"))
    }
}

/// The BIF hook for one process: `erlang:self/0` answers `self_pid`, and the `codec` leg's two
/// `bench_codec` stubs answer their argument unchanged (the crossing without the image).
///
/// Everything else falls through to the harness' own floor (`+`/`-` over small integers), so a
/// name the floor does not model keeps the inner host's typed `unsupported_bif` and the run
/// fails loudly instead of producing a figure.
fn bif_hook(self_pid: Eterm) -> BifHook {
    Box::new(
        move |module: &str, function: &str, arity: u32, args: &[Eterm]| {
            if module == "erlang" && function == "self" && arity == 0 && args.is_empty() {
                return Ok(self_pid);
            }
            if module == "bench_codec"
                && matches!(function, "encode" | "decode")
                && arity == 1
                && args.len() == 1
            {
                return Ok(args[0]);
            }
            Err(crate::error::unsupported(&format!(
                "send_split bif {module}:{function}/{arity}"
            )))
        },
    )
}

/// Build one process's context: counting, with `self/0` from `bif_hook` and this process's view
/// of the two inboxes.
fn process_context(
    module: &Arc<Module>,
    pid: Eterm,
    own: &Rc<RefCell<Vec<Eterm>>>,
    peer: &Rc<RefCell<Vec<Eterm>>>,
    peer_pid: Eterm,
) -> CountingContext {
    let mut ctx = CountingContext::new(Arc::clone(module));
    ctx.inner_mut().set_bif_hook(bif_hook(pid));
    ctx.inner_mut().set_mailbox(Box::new(PeeredMailbox {
        own: Rc::clone(own),
        peer: Rc::clone(peer),
        peer_pid,
        scan: Rc::new(Cell::new(0)),
    }));
    ctx
}

// ---------------------------------------------------------------------------
// The cooperative two-process driver
// ---------------------------------------------------------------------------

/// What one cooperative ping-pong run produced.
struct Run {
    /// Wall time of the whole ping-pong (the worker's first park included; it is one park per
    /// batch, against `ops` round trips).
    elapsed: Duration,
    /// Round trips actually performed.
    ops: usize,
    /// The driver process's tallies.
    driver: Counters,
    /// The worker process's tallies.
    worker: Counters,
    /// `RunOutcome::Wait` outcomes across both processes: the scheduler's park events.
    parks: u64,
    /// `resume_outcome_with_tier` calls across both processes: the scheduler's resume hops.
    resumes: u64,
    /// The driver's return value.
    result: Eterm,
}

/// Drive `ops` round trips of the two-process ping-pong on `tier`.
///
/// The two machines are driven cooperatively in one OS thread, exactly as a scheduler would
/// interleave them: the worker parks on its empty inbox, the driver sends and parks, then the two
/// alternate (worker consumes and replies, driver consumes and sends the next) until the driver's
/// counter reaches zero. A run that does not terminate in `4 * ops + 16` slices is a harness bug
/// and is reported as such, never allowed to spin.
fn drive(module: &Arc<Module>, leg: &Leg, ops: usize, tier: Tier) -> beam_common::Result<Run> {
    let entry = atom_named(module, leg.entry);
    let worker_entry = atom_named(module, leg.worker);
    let router = Router::new();
    let mut driver_ctx = process_context(
        module,
        router.driver_pid,
        &router.driver_inbox,
        &router.worker_inbox,
        router.worker_pid,
    );
    let mut worker_ctx = process_context(
        module,
        router.worker_pid,
        &router.worker_inbox,
        &router.driver_inbox,
        router.driver_pid,
    );
    let mut driver = Machine::new();
    let mut worker_machine = Machine::new();
    let mut parks = 0u64;
    let mut resumes = 0u64;
    let start = Instant::now();

    let mut worker_outcome = worker_machine.run_export_outcome_with_tier(
        module,
        worker_entry,
        0,
        &[],
        &mut worker_ctx,
        tier,
    )?;
    parks += u64::from(matches!(worker_outcome, RunOutcome::Wait { .. }));
    let mut driver_outcome = driver.run_export_outcome_with_tier(
        module,
        entry,
        3,
        &[make_small(ops as i64), router.worker_pid, make_small(0)],
        &mut driver_ctx,
        tier,
    )?;
    parks += u64::from(matches!(driver_outcome, RunOutcome::Wait { .. }));

    let mut result = None;
    for _ in 0..=(4 * ops + 16) {
        let driver_position = match driver_outcome {
            RunOutcome::Returned(value) => {
                result = Some(value);
                break;
            }
            RunOutcome::Wait { func, pc } => (func, pc),
            // The bench host answers every BIF directly, so a suspend would mean the measuring
            // context changed shape: the ping-pong is a park/resume workload only.
            RunOutcome::Suspend { .. } => {
                return Err(crate::error::unsupported(
                    "send_split: the driver suspended on a BIF; the bench host never suspends",
                ))
            }
        };
        let worker_position = match worker_outcome {
            RunOutcome::Wait { func, pc } => (func, pc),
            RunOutcome::Returned(_) => {
                return Err(crate::error::unsupported(
                    "send_split: the worker returned before the driver",
                ))
            }
            RunOutcome::Suspend { .. } => {
                return Err(crate::error::unsupported(
                    "send_split: the worker suspended on a BIF; the bench host never suspends",
                ))
            }
        };
        resumes += 1;
        worker_outcome = worker_machine.resume_outcome_with_tier(
            module,
            worker_position.0,
            worker_position.1,
            &mut worker_ctx,
            tier,
        )?;
        parks += u64::from(matches!(worker_outcome, RunOutcome::Wait { .. }));
        resumes += 1;
        driver_outcome = driver.resume_outcome_with_tier(
            module,
            driver_position.0,
            driver_position.1,
            &mut driver_ctx,
            tier,
        )?;
        parks += u64::from(matches!(driver_outcome, RunOutcome::Wait { .. }));
    }
    let elapsed = start.elapsed();
    let result = result.ok_or_else(|| {
        crate::error::unsupported("send_split: the driver never ran its loop to completion")
    })?;
    Ok(Run {
        elapsed,
        ops,
        driver: driver_ctx.counters,
        worker: worker_ctx.counters,
        parks,
        resumes,
        result,
    })
}

// ---------------------------------------------------------------------------
// The measured phases
// ---------------------------------------------------------------------------

/// One leg's measured phase on one tier.
struct Measurement {
    /// The round trips the phase measured (`SUB_RUNS` sub-runs of [`Self::per_run`]).
    ops: usize,
    /// Round trips per timed sub-run.
    per_run: usize,
    /// ns per round trip, one sample per timed sub-run.
    samples: Vec<f64>,
    /// The driver process's tallies over the whole phase.
    driver: Counters,
    /// The worker process's tallies over the whole phase.
    worker: Counters,
    /// Parks over the whole phase.
    parks: u64,
    /// Resumes over the whole phase.
    resumes: u64,
}

impl Measurement {
    /// The fastest sub-run (the robust statistic on a host with additive background load).
    fn min(&self) -> f64 {
        self.samples.iter().copied().fold(f64::INFINITY, f64::min)
    }

    /// The median sub-run.
    fn median(&self) -> f64 {
        let mut sorted = self.samples.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).expect("finite ns/op"));
        let middle = sorted.len() / 2;
        if sorted.len() % 2 == 0 {
            (sorted[middle - 1] + sorted[middle]) / 2.0
        } else {
            sorted[middle]
        }
    }
}

/// Run the untimed warmup, then [`SUB_RUNS`] timed sub-runs of `leg` on `tier`.
///
/// Every sub-run drives whole batches of fresh machines and throwaway contexts, because a
/// [`MinimalContext`](crate::context::MinimalContext) region heap never frees what it hands out:
/// one 10 000-round-trip machine would measure the harness' own growth.
fn measure_leg(
    module: &Arc<Module>,
    leg: &Leg,
    tier: Tier,
    ops: usize,
    batch: usize,
    warmup: usize,
) -> beam_common::Result<Measurement> {
    let batch = batch.max(1);
    let per_run = ops.div_ceil(SUB_RUNS).max(1);
    run_batches(module, leg, tier, warmup, batch)?;

    let mut samples = Vec::with_capacity(SUB_RUNS);
    let mut driver = Counters::default();
    let mut worker = Counters::default();
    let mut parks = 0u64;
    let mut resumes = 0u64;
    for _ in 0..SUB_RUNS {
        let run = run_batches(module, leg, tier, per_run, batch)?;
        samples.push(run.elapsed.as_secs_f64() * 1e9 / run.ops.max(1) as f64);
        accumulate(&mut driver, &run.driver);
        accumulate(&mut worker, &run.worker);
        parks += run.parks;
        resumes += run.resumes;
    }
    Ok(Measurement {
        ops: per_run * SUB_RUNS,
        per_run,
        samples,
        driver,
        worker,
        parks,
        resumes,
    })
}

/// One measured batch group: `ops` round trips in batches of `batch`, each batch with a fresh
/// pair of machines and contexts, and only the ping-pong's own wall time accumulated.
fn run_batches(
    module: &Arc<Module>,
    leg: &Leg,
    tier: Tier,
    ops: usize,
    batch: usize,
) -> beam_common::Result<Run> {
    let mut elapsed = Duration::ZERO;
    let mut driver = Counters::default();
    let mut worker = Counters::default();
    let mut parks = 0u64;
    let mut resumes = 0u64;
    let mut result = NIL;
    let mut remaining = ops;
    while remaining > 0 {
        let n = remaining.min(batch);
        let run = drive(module, leg, n, tier)?;
        elapsed += run.elapsed;
        accumulate(&mut driver, &run.driver);
        accumulate(&mut worker, &run.worker);
        parks += run.parks;
        resumes += run.resumes;
        result = run.result;
        remaining -= n;
    }
    Ok(Run {
        elapsed,
        ops,
        driver,
        worker,
        parks,
        resumes,
        result,
    })
}

/// Fold `source`'s tallies into `into` (the counters are one unit of measure across batches).
fn accumulate(into: &mut Counters, source: &Counters) {
    into.instructions += source.instructions;
    into.rooted_calls += source.rooted_calls;
    into.rooted_words += source.rooted_words;
    into.heap_calls += source.heap_calls;
    into.heap_words += source.heap_words;
    into.bif_calls += source.bif_calls;
    into.apply_calls += source.apply_calls;
    into.apply_fun_calls += source.apply_fun_calls;
    into.external_call_requests += source.external_call_requests;
    into.local_fun_requests += source.local_fun_requests;
    into.local_call_requests += source.local_call_requests;
    into.unrooted_calls += source.unrooted_calls;
    into.sends += source.sends;
    into.receive_nexts += source.receive_nexts;
    into.receive_skips += source.receive_skips;
    into.receive_removes += source.receive_removes;
    into.receive_resets += source.receive_resets;
    into.floor_declines += source.floor_declines;
    // The histogram is per distinct key, so a phase's figure is the sum over its batches.
    into.bif_histogram.merge(&source.bif_histogram);
}
/// `/proc/loadavg`, for the report header (this host is shared; every absolute figure is only
/// meaningful next to it).
fn loadavg() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "<unavailable>".to_string())
}

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

/// `count` per round trip.
fn per_op(count: u64, ops: usize) -> f64 {
    count as f64 / ops.max(1) as f64
}

/// Print one leg's `key = value` block.
fn print_leg(leg: &Leg, tier: Tier, measurement: &Measurement) {
    let ops = measurement.ops;    let min = measurement.min();
    let median = measurement.median();
    let instr = per_op(measurement.driver.instructions, ops)
        + per_op(measurement.worker.instructions, ops);
    println!("---");
    println!("leg = {}", leg.name);
    println!("payload = {}", leg.payload);
    println!("tier = {}", tier.name());
    println!("ops = {ops}");
    println!("sub_runs = {}", measurement.samples.len());
    println!("per_run_ops = {}", measurement.per_run);
    println!("min_ns_per_op = {min:.1}");
    println!("median_ns_per_op = {median:.1}");
    println!("range_ns_per_op = {:.1}..{:.1}", min, max(&measurement.samples));
    println!("instr_per_op = {instr:.2}");
    println!(
        "ns_per_instruction = {:.2}",
        if instr > 0.0 { min / instr } else { 0.0 }
    );
    println!(
        "driver_instr_per_op = {:.2}",
        per_op(measurement.driver.instructions, ops)
    );
    println!(
        "worker_instr_per_op = {:.2}",
        per_op(measurement.worker.instructions, ops)
    );
    let total_rooted_calls = measurement.driver.rooted_calls + measurement.worker.rooted_calls;
    let total_rooted_words = measurement.driver.rooted_words + measurement.worker.rooted_words;
    println!("rooted_calls_per_op = {:.2}", per_op(total_rooted_calls, ops));
    println!("rooted_words_per_op = {:.2}", per_op(total_rooted_words, ops));
    println!(
        "heap_door_calls_per_op = {:.2}",
        per_op(measurement.driver.heap_calls + measurement.worker.heap_calls, ops)
    );
    println!(
        "heap_door_words_per_op = {:.2}",
        per_op(measurement.driver.heap_words + measurement.worker.heap_words, ops)
    );
    println!(
        "bif_calls_per_op = {:.2}",
        per_op(measurement.driver.bif_calls + measurement.worker.bif_calls, ops)
    );
    println!(
        "local_call_requests_per_op = {:.2}",
        per_op(
            measurement.driver.local_call_requests + measurement.worker.local_call_requests,
            ops
        )
    );
    println!(
        "external_call_requests_per_op = {:.2}",
        per_op(
            measurement.driver.external_call_requests + measurement.worker.external_call_requests,
            ops
        )
    );
    println!(
        "unrooted_calls_per_op = {:.2}",
        per_op(measurement.driver.unrooted_calls + measurement.worker.unrooted_calls, ops)
    );
    println!("sends_per_op = {:.2}", per_op(measurement.driver.sends, ops));
    println!(
        "receive_nexts_per_op = {:.2}",
        per_op(
            measurement.driver.receive_nexts + measurement.worker.receive_nexts,
            ops
        )
    );
    println!(
        "receive_removes_per_op = {:.2}",
        per_op(
            measurement.driver.receive_removes + measurement.worker.receive_removes,
            ops
        )
    );
    println!(
        "receive_skips_per_op = {:.2}",
        per_op(
            measurement.driver.receive_skips + measurement.worker.receive_skips,
            ops
        )
    );
    println!(
        "receive_resets_per_op = {:.2}",
        per_op(
            measurement.driver.receive_resets + measurement.worker.receive_resets,
            ops
        )
    );
    println!("parks_per_op = {:.2}", per_op(measurement.parks, ops));
    println!("resume_slices_per_op = {:.2}", per_op(measurement.resumes, ops));
    println!(
        "bif_floor_declines_per_op = {:.2}",
        per_op(measurement.driver.floor_declines + measurement.worker.floor_declines, ops)
    );
    for (name, count) in super::ranked(&measurement.driver.bif_histogram.to_map(), TOP_ROWS) {
        println!("driver_bif_histogram[{name}] = {count}");
    }
    for (name, count) in super::ranked(&measurement.worker.bif_histogram.to_map(), TOP_ROWS) {
        println!("worker_bif_histogram[{name}] = {count}");
    }
    // A cheap sanity figure: the legs must be the two-process shape, not a single process with
    // both loops inlined.
    println!(
        "note = a leg's ops are {} round trips of two cooperative processes (one send and one \
         receive each), driven in one OS thread; instr_per_op is the sum over BOTH processes.",
        measurement.ops
    );
}
/// The largest sample.
fn max(samples: &[f64]) -> f64 {
    samples.iter().copied().fold(f64::NEG_INFINITY, f64::max)
}

/// Print the phase's baseline-tier structural census per op (dynamic counters only: the pass
/// records on a module whose blocks may already be compiled, so the *static* emitted-instruction
/// census would legitimately be empty and is not reported here).
fn print_baseline_census(label: &str, ops: usize) {
    let snapshot = crate::jit::baseline::counters::snapshot();
    let ops = ops.max(1) as f64;
    let per = |count: u64| count as f64 / ops;
    println!("--- {label}: baseline-tier structural counters per op (one recording pass) ---");
    println!("census_ops = {ops}");
    println!("driver_round_trips_per_op = {:.2}", per(snapshot.driver_round_trips));
    println!("driver_interp_steps_per_op = {:.2}", per(snapshot.driver_interp_steps));
    println!("jit_step_one_per_op = {:.2}", per(snapshot.step_one));
    println!("jit_step_ext_per_op = {:.2}", per(snapshot.step_ext));
    println!("in_block_calls_per_op = {:.2}", per(snapshot.in_block_calls));
    println!(
        "in_block_call_declines_per_op = {:.2}",
        per(snapshot.in_block_call_declines)
    );
    println!(
        "block_entry_lookups_per_op = {:.2}",
        per(snapshot.block_entry_lookups)
    );
    println!("block_entry_hits_per_op = {:.2}", per(snapshot.block_entry_hits));
    println!(
        "block_entry_natives_per_op = {:.2}",
        per(snapshot.block_entry_natives)
    );
    println!("ext_call_resolved_per_op = {:.2}", per(snapshot.ext_call_resolved));
    println!("ext_call_declined_per_op = {:.2}", per(snapshot.ext_call_declined));
    for (variant, count) in super::ranked(&snapshot.deopts_by_variant, TOP_ROWS) {
        println!("deopt_count_per_op[{variant}] = {:.2}", per(count));
    }
    if snapshot.deopts_by_variant.is_empty() {
        println!("note = deopt_count is empty: the native tier lowered every executed instance");
    }
}

/// The in-process decomposition of `copy_bound`'s `send_recv − send_recv_plain` marginal.
///
/// `#[ignore]`d: it is a measurement, not a gate (the load-bearing invariants it can assert are
/// pinned by `the_two_legs_of_the_copy_bound_split_agree`).
#[test]
#[ignore = "measurement harness"]
fn send_recv_split_decomposition() {
    let beam = match compile_fixture(MODULE, SOURCE, "send_split") {
        Ok(beam) => beam,
        Err(why) => {
            println!("SKIP: {why} -- the copy_bound split decomposition did not run");
            return;
        }
    };
    let module = Arc::new(
        load_file(&beam)
            .unwrap_or_else(|error| panic!("load {}: {error:?}", beam.display())),
    );
    let ops = env_usize("BENCH_ITERS", DEFAULT_OPS);
    let batch = env_usize("BENCH_BATCH", DEFAULT_BATCH);
    let warmup = env_usize("BENCH_WARMUP", DEFAULT_WARMUP);
    let tiers = selected_tiers();

    println!("workload = copy_bound split (two-process ping-pong, in-process, no copy engine)");
    println!("beam = {}", beam.display());
    println!("loadavg_start = {}", loadavg());
    println!(
        "note = every *_per_op count is deterministic (a host-side tally summed over BOTH \
         processes);"
    );
    println!(
        "note = min/median_ns_per_op are wall clock on a shared host -- read the RATIOS and the \
         counts,"
    );
    println!("note = never the absolute ns.");
    println!("loadavg_after_compile = {}", loadavg());

    let mut measured: Vec<(&Leg, Tier, Measurement)> = Vec::new();
    for tier in &tiers {
        for leg in LEGS {
            let measurement = measure_leg(&module, leg, *tier, ops, batch, warmup)
                .unwrap_or_else(|error| panic!("{} on {}: {error:?}", leg.name, tier.name()));
            print_leg(leg, *tier, &measurement);            println!("loadavg_after_leg = {}", loadavg());
            measured.push((leg, *tier, measurement));
        }
    }

    // The baseline tier's own structural census (driver round trips, deopts, in-block links),
    // one recording pass per leg.
    for tier in &tiers {
        if *tier != Tier::BaselineJit {
            continue;
        }
        for leg in LEGS {
            crate::jit::baseline::counters::set_enabled(true);
            crate::jit::baseline::counters::reset();
            let run = run_batches(&module, leg, Tier::BaselineJit, CENSUS_OPS, CENSUS_OPS)
                .expect("the baseline census pass");
            crate::jit::baseline::counters::set_enabled(false);
            assert_eq!(
                run.result,
                make_small(CENSUS_OPS as i64),
                "the census pass returned the wrong result"
            );
            print_baseline_census(leg.name, CENSUS_OPS);
        }
    }

    // The marginal: `heap − plain`, per tier.
    for tier in &tiers {
        let pick = |name: &str| {
            measured
                .iter()
                .find(|entry| entry.1 == *tier && entry.0.name == name)
                .map(|entry| &entry.2)
        };
        let (Some(plain), Some(heap)) = (pick("plain"), pick("heap")) else {
            continue;
        };        let plain_instr = instr(plain);
        let heap_instr = instr(heap);
        let plain_words = words(plain);
        let heap_words = words(heap);
        let plain_calls = calls(plain);
        let heap_calls = calls(heap);
        let ns_instr_plain = if plain_instr > 0.0 {
            plain.min() / plain_instr
        } else {
            0.0
        };
        let delta_ns = heap.min() - plain.min();
        let delta_instr = heap_instr - plain_instr;
        let dispatch_ns = delta_instr * ns_instr_plain;
        let delta_words = heap_words - plain_words;
        println!("===");
        println!("marginal = heap − plain (both tiers named; per op, both processes)");
        println!("tier = {}", tier.name());
        println!("plain_min_ns_per_op = {:.1}", plain.min());
        println!("heap_min_ns_per_op = {:.1}", heap.min());
        println!("delta_ns_per_op = {delta_ns:.1}");
        println!("delta_instr_per_op = {delta_instr:.2}");
        println!("delta_rooted_calls_per_op = {:.2}", heap_calls - plain_calls);
        println!("delta_rooted_words_per_op = {delta_words:.2}");
        println!(
            "payload_build_ns_per_op_at_plain_dispatch_cost = {dispatch_ns:.1} (predicted: \
             delta_instr x the plain leg's own ns/instr)"
        );
        println!(
            "payload_build_residual_ns_per_op = {:.1} (measured delta minus that prediction: the \
             per-built-term allocation's own cost)",
            delta_ns - dispatch_ns
        );
        if delta_words != 0.0 {
            println!(
                "payload_build_residual_ns_per_word = {:.2}",
                (delta_ns - dispatch_ns) / delta_words
            );
        }
        // The codec pair's own marginal: the two host crossings, with the image absent.
        if let (Some(heap), Some(codec)) = (pick("heap"), pick("codec")) {
            let delta = codec.min() - heap.min();
            let crossings = bif_per_op(codec) - bif_per_op(heap);
            println!("===");
            println!("marginal = codec − heap (the codec pair's two host crossings, identity stubs)");
            println!("tier = {}", tier.name());
            println!("heap_min_ns_per_op = {:.1}", heap.min());
            println!("codec_min_ns_per_op = {:.1}", codec.min());
            println!("delta_ns_per_op = {delta:.1}");
            println!("delta_instr_per_op = {:.2}", instr(codec) - instr(heap));
            println!("delta_bif_calls_per_op = {crossings:.2}");
            if crossings > 0.0 {
                println!("ns_per_crossing = {:.2}", delta / crossings);
            }
        }
        println!("loadavg_end = {}", loadavg());
    }
}

/// A leg's instruction count per op (both processes).
fn instr(measurement: &Measurement) -> f64 {
    per_op(measurement.driver.instructions + measurement.worker.instructions, measurement.ops)
}

/// A leg's rooted allocation-word count per op (both processes).
fn words(measurement: &Measurement) -> f64 {
    per_op(measurement.driver.rooted_words + measurement.worker.rooted_words, measurement.ops)
}

/// A leg's rooted allocation-call count per op (both processes).
fn calls(measurement: &Measurement) -> f64 {
    per_op(measurement.driver.rooted_calls + measurement.worker.rooted_calls, measurement.ops)
}

/// A leg's `call_bif` crossing count per op (both processes).
fn bif_per_op(measurement: &Measurement) -> f64 {
    per_op(measurement.driver.bif_calls + measurement.worker.bif_calls, measurement.ops)
}

// ---------------------------------------------------------------------------
// The invariant gate
// ---------------------------------------------------------------------------

/// The split harness' own correctness gate (cheap, not `#[ignore]`d): every leg runs the whole
/// two-process shape, returns the iteration count, consumes every message, and dispatches nothing
/// the harness' BIF floor had to decline — and the `codec` leg's only difference from `heap` is
/// its two extra host crossings.
///
/// This is what makes the report trustworthy: a figure measured on a loop that silently took a
/// different branch would be a figure about some other program. It skips loudly when the system
/// `erlc` (which compiles the fixture) is unavailable.
#[test]
fn the_two_legs_of_the_copy_bound_split_agree() {
    let beam = match compile_fixture(MODULE, SOURCE, "send_split_gate") {
        Ok(beam) => beam,
        Err(why) => {
            println!("SKIP: {why} -- the copy_bound split gate did not run");
            return;
        }
    };
    let module = Arc::new(
        load_file(&beam)
            .unwrap_or_else(|error| panic!("load {}: {error:?}", beam.display())),
    );
    for tier in [Tier::Interpreter, Tier::BaselineJit] {
        let mut heap_crossings = None;
        for leg in LEGS {
            let run = drive(&module, leg, 16, tier)
                .unwrap_or_else(|error| panic!("{} on {}: {error:?}", leg.name, tier.name()));
            assert_eq!(
                run.result,
                make_small(16),
                "{} on {} must return the iteration count",
                leg.name,
                tier.name()
            );
            assert_eq!(
                run.driver.sends, 16,
                "{} on {}: the driver must send once per op",
                leg.name,
                tier.name()
            );
            assert_eq!(
                run.worker.sends, 16,
                "{} on {}: the worker must reply once per op",
                leg.name,
                tier.name()
            );
            assert_eq!(
                (run.driver.receive_removes, run.worker.receive_removes),
                (16, 16),
                "{} on {}: every message must be consumed by both processes",
                leg.name,
                tier.name()
            );
            assert_eq!(
                run.driver.floor_declines + run.worker.floor_declines,
                0,
                "{} on {}: the harness' BIF floor declined a call",
                leg.name,
                tier.name()
            );
            // The legs differ only in how the payload is produced: the heap-built legs must build
            // it (a `put_map_assoc` allocation the literal leg does not pay).
            if leg.name != "plain" {
                assert!(
                    run.driver.rooted_calls > 0,
                    "{} on {}: the heap payload must be built through the rooted door",
                    leg.name,
                    tier.name()
                );
            }
            if leg.name == "heap" {
                heap_crossings = Some((run.driver.bif_calls, run.worker.bif_calls));
            }
            if leg.name == "codec" {
                let (heap_driver, heap_worker) = heap_crossings
                    .expect("the `heap` leg runs before the `codec` leg (report order)");
                assert_eq!(
                    run.worker.bif_calls,
                    heap_worker + 2 * 16,
                    "codec on {}: the worker must pay exactly two host crossings per op",
                    tier.name()
                );
                assert_eq!(
                    run.driver.bif_calls, heap_driver,
                    "codec on {}: the driver's own crossings must not move",
                    tier.name()
                );
            }
        }
    }
}
