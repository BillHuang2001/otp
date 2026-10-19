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
//! **The `-eval` route's escaped off-heap payload store, witnessed end to end** — the
//! integration suite for `src/eval/reclaim.rs` (reached from `Evaluator::prune_escaped` at the
//! tree-walker's depth-0 statement boundaries).
//!
//! # What is under test
//!
//! `Runtime::eval` runs the expression on a **table-less** route: there is no scheduled process,
//! so a term that escapes an evaluation (the result, a bitstring, a BIF's container result) is
//! copied onto the runtime's never-reset store (`Runtime::eval_heap`) and the off-heap refc
//! payload such a term points at is **owned** by `Runtime::eval_off_heap`
//! ([`EvalOffHeapStats`]). Until `src/eval/reclaim.rs` existed the store only ever grew.
//!
//! The claim this suite exists to falsify is "the store grows with the number of payloads the
//! expression builds". It does not: the reclamation releases every payload that nothing live
//! reaches, at each top-level statement boundary, so an expression that builds *N* throw-away
//! off-heap payloads holds **O(1)** of them.
//!
//! # The producer, and why 40 000 bytes of `<<1>>`
//!
//! `base64:encode(binary:copy(<<1>>, 40000))` is the measured producer: `binary:copy/2` is a
//! registered BIF whose result is `40 000` `0x01` bytes (well past
//! `beam_term::refc::ERL_ONHEAP_BINARY_LIMIT`), and `base64:encode/1` is entered as an external
//! module whose compiled bitstring code builds **off-heap** payloads, which the runtime adopts into
//! the store. It is also the exact shape that used to kill the process: the store region holds
//! those `0x01` bytes as raw data, so a scan that treated region *words* as terms read them as
//! `0x0101_0101_0101_0101` — low bits `TAG_PRIMARY_LIST`, "pointer" `0x0101_0101_0101_0100` — and
//! faulted. [`the_off_heap_producer_no_longer_faults_the_walk`] is that regression.
//!
//! # The bound is structural, never a wall-clock claim
//!
//! Every figure below is a **count of payload bytes the store reserves**
//! ([`EvalOffHeapStats::retained_bytes`], the sum of `RefcBinary::reserved_bytes`) and the handle
//! count beside it — both deterministic, both independent of the host's load. The oracle has no
//! counterpart for any of it (its payloads are refcounted straight off the process heap), so
//! nothing here is a differential figure.
//!
//! # What each test pins
//!
//! 1. [`the_store_is_bounded_across_a_multi_statement_loop`] — the headline: `EVALS` evaluations,
//!    each a block of [`STATEMENTS_PER_EVAL`] throw-away payload builds plus a final `ok`, on ONE
//!    runtime. The store must stay under [`BOUND_BYTES`] and its handle count under
//!    [`BOUND_HANDLES`] at **every** sample, must not drift upward across the run, and the run must
//!    show `prunes > 0` — a suite that would pass with the reclaim disabled is a lie.
//! 2. [`without_the_reclaim_the_same_loop_is_linear`] — the A/B: the *same* loop with the reclaim
//!    switched off (`Runtime::set_eval_prune_enabled(false)`) must be linear in the payload count
//!    (measured: one handle per payload build, `EVALS × STATEMENTS_PER_EVAL` handles). That pair is
//!    what makes test 1 non-vacuous: it proves the *reclaim* is the reason the store is bounded.
//! 3. [`a_retained_off_heap_binary_survives_every_prune_byte_exactly`] — the safety half: a payload
//!    bound in the evaluator's environment stays live across many prune rounds and must come back
//!    **byte-exact** (`=:=` against a fresh rebuild of the same bytes, plus `byte_size`), with
//!    `prunes > 0` so it cannot pass vacuously. A prune that freed a live payload cannot satisfy it.
//! 4. [`the_off_heap_producer_no_longer_faults_the_walk`] — the crash regression: driving the
//!    producer past the byte trigger (the point where a prune first runs) must complete, and the
//!    prune must have fired.
//! 5. [`a_missing_toolchain_is_a_loud_skip_never_a_silent_pass`] — the loud-skip guard
//!    (`tests/support/mod.rs`): no `erl` declines the oracle half loudly, never silently.
//!
//! [`a_fun_loop_keeps_a_retained_payload_byte_exact_across_nested_prunes`] adds the
//! **nested-call** half of the safety contract: a 64 KiB payload a `lists:foldl/3` lambda's
//! throw-away builds cannot name must survive the prune that runs at every lambda's epilogue,
//! checked by `erlang:md5/1` before and after the churn and `=:=` a fresh rebuild.
//!
//! The two remaining cases of the reclaim's own contract are pinned **in-crate**, where the store
//! is reachable at all: the `RefCount > 1` belt and the decline-on-doubt rule both need to write a
//! handle into `Runtime::eval_off_heap` directly, which is `pub(crate)`
//! (`src/eval/reclaim.rs`'s `a_shared_payload_is_retained_by_the_store`,
//! `an_exhausted_walk_declines_and_leaves_the_store_untouched`).

use std::path::Path;

use beam_rt::config::RuntimeConfig;
use beam_rt::runtime::{EvalOffHeapStats, Runtime};

// The shared helper module is compiled into every integration-test binary, so this binary would
// otherwise warn on the entry points it does not call.
#[allow(dead_code)]
mod support;

/// The suite's name in the loud-skip banner.
const SUITE: &str = "eval_off_heap_reclaim";

/// The measured off-heap producer (see the module docs).
const PRODUCER: &str = "base64:encode(binary:copy(<<1>>, 40000))";

/// The size of the binary [`PRODUCER`] copies, in bytes.
const PRODUCER_BYTES: usize = 40_000;

/// How many throw-away payload builds one evaluation's block holds.
const STATEMENTS_PER_EVAL: usize = 6;

/// How many evaluations test 1 and test 2 drive.
const EVALS: usize = 20;

/// The store bound test 1 asserts, in **bytes** (8 × the ≈65 536 a single retained payload
/// reserves; the un-reclaimed series reaches ≈7.9 MB over the same loop).
const BOUND_BYTES: usize = 8 * 65_536;

/// The handle-count bound test 1 asserts beside it.
const BOUND_HANDLES: usize = 16;

/// How many prune rounds the anti-dangle case must survive (each one is a real rewrite of the
/// store, asserted through `prunes`).
const KEEP_ROUNDS: usize = 24;

/// The source bytes `binary:copy/2` copies for the **fun-loop** anti-dangle payload: base64 of
/// 49 152 bytes is exactly 65 536, one 64 KiB payload.
const FUN_KEEP_SOURCE_BYTES: u32 = 49_152;

/// That payload's own size, in bytes: `ceil(49 152 / 3) * 4`.
const FUN_KEEP_PAYLOAD_BYTES: u32 = 65_536;

/// How many throw-away payload builds the fun loop runs while the 64 KiB payload is retained.
///
/// Each application of the lambda is its own nested run (`apply_fun` -> `run_label_blocking`),
/// so the loop crosses many nested-call prunes; 40 builds is also enough on its own to cross the
/// store's 46 422-word trigger with [`PRODUCER`]'s 53 336-byte payloads.
const FUN_LOOP_ROUNDS: u32 = 40;

/// A single `Runtime` on the default configuration.
fn runtime() -> Runtime {
    Runtime::new(RuntimeConfig::default()).expect("the default runtime")
}

/// A comma-separated block of `statements` throw-away payload builds followed by `ok`, so the
/// block's **value** is an immediate and nothing but the builds' own payloads reach the store.
fn garbage_block(statements: usize) -> String {
    let mut parts: Vec<String> = (0..statements).map(|_| PRODUCER.to_string()).collect();
    parts.push("ok".to_string());
    parts.join(", ")
}

/// One measured sample of the store.
#[derive(Debug, Clone, Copy)]
struct Sample {
    evals: usize,
    handles: usize,
    bytes: usize,
    prunes: u64,
}

impl Sample {
    fn of(evals: usize, stats: EvalOffHeapStats) -> Sample {
        Sample {
            evals,
            handles: stats.handles,
            bytes: stats.retained_bytes,
            prunes: stats.prunes,
        }
    }
}

/// Prints a series as one line per sample, so a failure is diagnosable from the test log.
fn report(label: &str, series: &[Sample]) {
    for sample in series {
        eprintln!(
            "[{SUITE}] {label} evals={} handles={} bytes={} prunes={}",
            sample.evals, sample.handles, sample.bytes, sample.prunes
        );
    }
}

/// Drives the garbage block for `evals` evaluations, sampling the store after each one.
fn drive(mut runtime: Runtime, evals: usize) -> (Runtime, Vec<Sample>) {
    let expr = garbage_block(STATEMENTS_PER_EVAL);
    let mut series = Vec::with_capacity(evals);
    series.push(Sample::of(0, runtime.eval_off_heap_stats()));
    for step in 1..=evals {
        runtime
            .eval(&expr)
            .unwrap_or_else(|error| panic!("evaluation {step} failed: {error:?}"));
        series.push(Sample::of(step, runtime.eval_off_heap_stats()));
    }
    (runtime, series)
}

// ---------------------------------------------------------------------------------------------
// 1. The envelope.
// ---------------------------------------------------------------------------------------------

/// The headline evidence: a long `-eval` block loop builds `EVALS × STATEMENTS_PER_EVAL`
/// throw-away off-heap payloads and the store stays **O(1)**.
///
/// "Bounded" is asserted three ways, all structural: no sample exceeds [`BOUND_BYTES`] /
/// [`BOUND_HANDLES`], the last sample is no larger than the largest of the first third (no
/// upward drift with N), and the run's `prunes` counter is non-zero — so the bound can only be
/// satisfied by a reclaim that actually ran. The measured series is printed; it also shows the
/// **non-monotone** shape (the store rises while a block runs and falls back at the boundary
/// prune) that distinguishes a sawtooth from a plateau.
#[test]
fn the_store_is_bounded_across_a_multi_statement_loop() {
    let (runtime, series) = drive(runtime(), EVALS);
    report("bounded", &series);
    let stats = runtime.eval_off_heap_stats();
    assert!(
        stats.prunes > 0,
        "the reclaim never ran, so the bound below would be vacuous: {stats:?}"
    );
    assert_eq!(
        stats.declines, 0,
        "a decline retains everything; this loop must be fully prunable: {stats:?}"
    );
    for sample in &series {
        assert!(
            sample.bytes <= BOUND_BYTES,
            "the store exceeded its bound at eval {}: {sample:?}",
            sample.evals
        );
        assert!(
            sample.handles <= BOUND_HANDLES,
            "the store held too many handles at eval {}: {sample:?}",
            sample.evals
        );
    }
    let early_peak = series
        .iter()
        .take(EVALS / 3 + 1)
        .map(|sample| sample.bytes)
        .max()
        .unwrap_or(0);
    let last = series.last().copied().expect("the series has a last sample");
    assert!(
        last.bytes <= early_peak,
        "the store drifted upward across the run (last {} bytes vs early peak {early_peak}): {}",
        last.bytes,
        series
            .iter()
            .map(|s| s.bytes.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    assert!(
        series.windows(2).any(|pair| pair[1].bytes < pair[0].bytes),
        "the series never fell, so no prune visibly released anything: {series:?}"
    );
    println!(
        "[{SUITE}] {EVALS} evals × {STATEMENTS_PER_EVAL} payload builds stayed at ≤ {} bytes / \
         ≤ {} handles with {} prunes",
        series.iter().map(|s| s.bytes).max().unwrap_or(0),
        series.iter().map(|s| s.handles).max().unwrap_or(0),
        stats.prunes
    );
}

// ---------------------------------------------------------------------------------------------
// 2. The A/B: the same loop with the reclaim switched off is linear.
// ---------------------------------------------------------------------------------------------

/// The control half of test 1: **the same loop** with the reclaim disabled
/// (`Runtime::set_eval_prune_enabled(false)`) retains one payload per build.
///
/// Without this half, "the store is bounded" would be an assertion about a wire nobody could
/// falsify. The measured slope is what makes it decisive: `EVALS × STATEMENTS_PER_EVAL` handles
/// and the last sample is larger than the first sample of the same loop with the reclaim on.
#[test]
fn without_the_reclaim_the_same_loop_is_linear() {
    let mut control = runtime();
    control.set_eval_prune_enabled(false);
    let (control, series) = drive(control, EVALS);
    report("no-reclaim", &series);
    let stats = control.eval_off_heap_stats();
    assert_eq!(
        stats.prunes, 0,
        "the reclaim was switched off, so no prune may have run: {stats:?}"
    );
    let last = series.last().copied().expect("the series has a last sample");
    let first = series.first().copied().expect("the series has a first sample");
    assert_eq!(first.bytes, 0, "a fresh runtime holds no payloads");
    // Every one of the loop's payload builds is still owned by the store.
    let builds = EVALS * STATEMENTS_PER_EVAL;
    assert!(
        last.handles >= builds,
        "the un-reclaimed store must hold at least one handle per build ({builds}): {last:?}"
    );
    assert!(
        last.bytes > BOUND_BYTES,
        "the un-reclaimed store must blow the bound test 1 asserts ({BOUND_BYTES} bytes): {last:?}"
    );
    // Linear means monotone non-decreasing here: nothing ever releases.
    assert!(
        series.windows(2).all(|pair| pair[1].bytes >= pair[0].bytes),
        "a disabled reclaim cannot release anything, so the series cannot fall: {series:?}"
    );
    println!(
        "[{SUITE}] the control loop ({builds} builds) retained {} bytes in {} handles with the \
         reclaim off",
        last.bytes, last.handles
    );
}

// ---------------------------------------------------------------------------------------------
// 3. Anti-dangle: a retained payload survives every prune, byte-exactly.
// ---------------------------------------------------------------------------------------------

/// The safety half — the one that would catch an over-eager prune.
///
/// One expression binds a payload in the evaluator's environment, then runs
/// [`KEEP_ROUNDS`] throw-away builds (each one its own top-level statement, hence its own prune
/// boundary), and finally asks the *bound* value for `=:=` against a **fresh rebuild of the same
/// bytes** and for its size. Both ends are proven: the retained payload is still there
/// (`byte_size`) and still byte-identical (`=:=` compares the payload, not the handle).
///
/// The witness is the **last statement's value**, not a second `Runtime::eval`: an `Env` lives for
/// one evaluation, so a second call could not name `Keep` at all (`variable "Keep" is unbound`) —
/// and folding the witness into the same expression is also the stronger test, because the
/// statement that re-reads `Keep` is itself preceded by a prune boundary.
///
/// `prunes > 0` is asserted, so the case cannot pass on a build where no prune happened. A prune
/// that freed the bound payload would make `=:=` answer `false` (a freed-then-reused payload) or
/// fault; there is no third outcome.
#[test]
fn a_retained_off_heap_binary_survives_every_prune_byte_exactly() {
    let mut parts = vec![format!("Keep = {PRODUCER}")];
    for _ in 0..KEEP_ROUNDS {
        // A throw-away build as its own statement: the value is dropped by the next statement,
        // so its payload is exactly what a prune must release.
        parts.push(PRODUCER.to_string());
    }
    // The witness: a fresh rebuild compared with the retained value, and its size.
    parts.push(format!(
        "{{Keep =:= {PRODUCER}, byte_size(Keep)}}"
    ));
    let expr = parts.join(", ");

    let mut runtime = runtime();
    let value = runtime
        .eval(&expr)
        .expect("the retaining expression evaluates without dangling");
    let stats = runtime.eval_off_heap_stats();
    eprintln!(
        "[{SUITE}] retained across the loop: handles={} bytes={} prunes={} declines={}",
        stats.handles, stats.retained_bytes, stats.prunes, stats.declines
    );
    assert!(
        stats.prunes > 0,
        "the retained-payload case must cross at least one prune boundary: {stats:?}"
    );
    assert_eq!(
        stats.declines, 0,
        "this graph is fully walkable, so a decline would be a bug: {stats:?}"
    );
    // The retained payload is still owned by the store (the environment names it, and the
    // statement's own build round-tripped through it).
    assert!(
        stats.handles >= 1,
        "the bound payload must still be retained: {stats:?}"
    );
    // The witness statement's own value is `{Keep =:= <a fresh rebuild>, byte_size(Keep)}`; both
    // halves must be the true ones, so the payload survived every boundary byte-exactly and at
    // full length.
    let text = runtime
        .format_term_w(value, None)
        .expect("the witness formats");
    let expected = PRODUCER_BYTES.div_ceil(3) * 4;
    assert_eq!(
        text.trim_end(),
        format!("{{true,{expected}}}"),
        "the retained payload is no longer byte-identical, or lost its length"
    );
}

// ---------------------------------------------------------------------------------------------
// 4. Anti-dangle for the **nested-call** prune: a fun-loop payload survives it.
// ---------------------------------------------------------------------------------------------

/// The nested-call **anti-dangle** case: a payload a live term names must survive the prune that
/// runs at a fun loop's nested-call epilogue.
///
/// The fun-loop arm restores the store from a different place than the statement boundaries
/// above: a `lists:foldl/3` lambda is applied on its own short-lived `Machine`
/// ([`RuntimeContext::apply_fun`] -> `run_label_blocking`), and that machine's epilogue adopts
/// the payloads the lambda built into this store and then prunes it against **that run's own**
/// published root set (`RuntimeContext::adopt_nested_payloads`: its X registers, its whole
/// Erlang stack, its FR file and the term it returns).
///
/// The payload bound to `Keep` is named by none of those while the lambda runs — yet it must
/// survive, because a nested call may only release the handles it adopted **itself**: an earlier
/// statement's payload is out of its scope and is retained unconditionally. That rule is what
/// makes the nested-call prune sound, and this case is its witness.
///
/// The retained payload is checked at **both ends** of the loop: `D0` is its `erlang:md5/1`
/// taken *before* the churn and compared *after* it, and the value itself is compared `=:=` with
/// a **fresh rebuild** of the same bytes (`=:=` on binaries compares content, not identity).
/// `byte_size/1` is the independent third check, and `prunes > 0` below is what stops the case
/// passing on a runtime where no prune ran at all.
#[test]
fn a_fun_loop_keeps_a_retained_payload_byte_exact_across_nested_prunes() {
    let keep = format!("base64:encode(binary:copy(<<7>>, {FUN_KEEP_SOURCE_BYTES}))");
    let expr = format!(
        "Keep = {keep}, D0 = erlang:md5(Keep), \
         lists:foldl(fun(_, Acc) -> _ = {PRODUCER}, Acc end, ok, lists:seq(1, {FUN_LOOP_ROUNDS})), \
         {{erlang:md5(Keep) =:= D0, Keep =:= {keep}, byte_size(Keep)}}"
    );
    let mut runtime = runtime();
    let value = runtime
        .eval(&expr)
        .expect("the fun-loop retaining expression evaluates without dangling");
    let stats = runtime.eval_off_heap_stats();
    eprintln!(
        "[{SUITE}] fun-loop retain: handles={} bytes={} prunes={} declines={}",
        stats.handles, stats.retained_bytes, stats.prunes, stats.declines
    );
    assert!(
        stats.prunes > 0,
        "the fun-loop case must cross at least one prune boundary: {stats:?}"
    );
    assert_eq!(
        stats.declines, 0,
        "this graph is fully walkable, so a decline would be a bug: {stats:?}"
    );
    let text = runtime
        .format_term_w(value, None)
        .expect("the witness formats");
    assert_eq!(
        text.trim_end(),
        format!("{{true,true,{FUN_KEEP_PAYLOAD_BYTES}}}"),
        "the payload retained across the fun loop is no longer byte-identical (md5 before/after \
         the churn, `=:=` a fresh rebuild) or lost its length"
    );
}

/// The size half of the anti-dangle witness, in a second `Runtime` so the first case's store
/// shape cannot mask a wrong `byte_size` (a payload whose window shrank would still compare
/// `true` against a rebuild only if the bytes matched, but the size is the independent check).
///
/// The loop holds [`STATEMENTS_PER_EVAL`] builds, not fewer: the store's trigger only fires past
/// [`EvalOffHeapStats::retained_bytes`] ≈ 371 KB, and one `base64:encode` payload reserves 65 536
/// of them, so a four-build loop would never reach a prune boundary and the case could pass while
/// proving nothing (`prunes > 0` is asserted for exactly that reason).
#[test]
fn a_retained_off_heap_binary_reports_its_full_size_after_the_loop() {
    let mut runtime = runtime();
    let expr = format!(
        "Keep = {PRODUCER}, {}, byte_size(Keep)",
        std::iter::repeat_n(PRODUCER, STATEMENTS_PER_EVAL)
            .collect::<Vec<_>>()
            .join(", ")
    );
    let value = runtime
        .eval(&expr)
        .expect("the retaining expression evaluates");
    let text = runtime
        .format_term_w(value, None)
        .expect("the size formats");
    // base64 of `PRODUCER_BYTES` bytes: ceil(n/3)*4.
    let expected = PRODUCER_BYTES.div_ceil(3) * 4;
    // `format_term_w` is the `~w~n` printer, so the rendering carries the newline the `io`
    // format control adds; the figure under test is the text in front of it.
    assert_eq!(
        text.trim_end(),
        expected.to_string(),
        "the retained payload's size must survive every prune"
    );
    assert!(
        runtime.eval_off_heap_stats().prunes > 0,
        "the size witness must also cross a prune boundary"
    );
}

// ---------------------------------------------------------------------------------------------
// 4. The crash regression.
// ---------------------------------------------------------------------------------------------

/// The SIGBUS regression: driving [`PRODUCER`] past the byte trigger used to kill the process.
///
/// The store's trigger is "the handle count moved **and** the summed payload bytes exceed
/// 46 422 words (≈371 KB)", so an evaluation only ever ran a prune once enough payloads had
/// accumulated. With this producer that is the **fourth** `Runtime::eval` call — which is exactly
/// where the process died (`SIGSEGV`/`General Protection Fault` inside `walk`, faulting at
/// `0x0101_0101_0101_0100`: the producer's own `0x01` payload bytes read as a `LIST` word).
///
/// The regression therefore asserts the *completed* run: eight evaluations, a non-zero `prune`
/// count (i.e. the trigger really was crossed, so the case cannot pass by never reaching the
/// code), and no decline.
#[test]
fn the_off_heap_producer_no_longer_faults_the_walk() {
    let mut runtime = runtime();
    for step in 1..=8 {
        runtime
            .eval(PRODUCER)
            .unwrap_or_else(|error| panic!("evaluation {step} failed: {error:?}"));
    }
    let stats = runtime.eval_off_heap_stats();
    eprintln!(
        "[{SUITE}] producer past the trigger: handles={} bytes={} prunes={} declines={}",
        stats.handles, stats.retained_bytes, stats.prunes, stats.declines
    );
    assert!(
        stats.prunes > 0,
        "the producer never crossed the prune trigger, so the walk never ran: {stats:?}"
    );
    assert_eq!(stats.declines, 0, "the walk must not decline here: {stats:?}");
}

// ---------------------------------------------------------------------------------------------
// 5. The loud skip.
// ---------------------------------------------------------------------------------------------

/// The loud-skip guard: without an `erl` there is no oracle, and the suite says so instead of
/// quietly passing.
///
/// This suite's *own* cases are in-process (they need no compiler), so the guard is asserted on
/// the shared helper the oracle-cross-checked half uses: an empty search path resolves no
/// executable, and `require_oracle` declines with the shared `SKIPPED` / `NOT a pass` banner.
#[test]
fn a_missing_toolchain_is_a_loud_skip_never_a_silent_pass() {
    assert!(
        !support::is_executable(Path::new("/nonexistent/erl")),
        "a path that does not exist is never an executable"
    );
    let empty = std::env::temp_dir().join("beam_rt_eval_offheap_no_such_dir");
    assert!(
        !support::is_executable(&empty.join("erl")),
        "a directory without `erl` resolves no oracle"
    );
    if support::resolve_oracle().is_some() {
        println!("[{SUITE}] an oracle is available: the loud-skip path is not exercised here");
        return;
    }
    assert!(
        support::require_oracle(SUITE).is_none(),
        "no oracle must decline (loudly), never hand back a usable path"
    );
    println!("[{SUITE}] the loud-skip guard holds: a missing toolchain is NOT a pass");
}
