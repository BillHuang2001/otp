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

//! The statement window's two claims, each pinned by its own test.
//!
//! **Claim 1 — the store is bounded.** A `-eval` expression whose top-level statements each build
//! and discard a large term answers the *same* `used` for ten statements as for a thousand, and
//! the same expression with the window switched off (the diagnostic kill switch,
//! [`Runtime::set_eval_scratch_enabled`]) is linear in the statement count. The pre-fix
//! measurement is in `PERF.md`.
//!
//! **Claim 2 — a reset never frees a live word.** Every holder family that can keep a store word
//! across a boundary is exercised with a value big enough to *be* the window: a `persistent_term`
//! value, an environment binding, a mailbox message and a pending timer's message each survive
//! many resets and still reduce to the value the oracle produces. `tests/fixtures/` has no module
//! here on purpose — the shapes are `-eval` expressions, which is the route under test.
//!
//! The oracle cross-check lives in `tests/eval_region.rs` (it needs the system `erl`); these are
//! the fast hermetic pins the unit suite runs.

use beam_common::Result as BeamResult;
use beam_term::Eterm;

use crate::config::RuntimeConfig;
use crate::context::RuntimeContext;
use crate::runtime::Runtime;

/// The element count of one "large" term. A list of `{I,I}` pairs of this width copies far more
/// than [`super::MIN_WINDOW_WORDS`] (≈ 4 000 words), which is what makes a statement's dead copy
/// visible in `used`.
const ELEMENTS: u32 = 2_000;

/// Evaluates `expr` on `runtime` through the real `-eval` driver.
fn eval(runtime: &mut Runtime, expr: &str) -> BeamResult<Eterm> {
    let mut sink = beam_bifs::NullSink;
    let mut ctx = RuntimeContext::with_output(runtime, &mut sink);
    crate::eval::eval(&mut ctx, expr)
}

/// `n` statements that each build and discard a large term, then a final `ok`.
///
/// This is the reducible shape: a comma sequence discards every statement's value but the last,
/// so the window taken before each statement may reclaim the previous statement's copy.
fn discarded_builds(n: usize) -> String {
    let statement = format!("_ = lists:seq(1,{ELEMENTS}), ");
    let mut expr = String::with_capacity(n * statement.len() + 4);
    for _ in 0..n {
        expr.push_str(&statement);
    }
    expr.push_str("ok");
    expr
}

/// The three statement counts every flat-in-N assertion uses: small, medium and large enough that
/// a linear mechanism would be unmistakable.
const COUNTS: [usize; 3] = [10, 100, 1_000];

#[test]
fn the_store_does_not_grow_with_the_statement_count() {
    let mut used = Vec::new();
    for n in COUNTS {
        let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
        let value = eval(&mut runtime, &discarded_builds(n)).expect("the expression evaluates");
        assert!(beam_term::is_atom(value), "the last statement is `ok`");
        let stats = runtime.eval_region_stats();
        // One reset per statement after the first: the window is taken before every statement,
        // so the second statement's boundary reclaims the first's copy. The first boundary has
        // nothing to reclaim (its window is empty), which is why the count is `n - 1`.
        assert!(
            stats.resets as usize >= n - 1,
            "n={n}: only {} resets happened",
            stats.resets
        );
        assert!(stats.words_reclaimed > 0, "n={n}: nothing was reclaimed");
        used.push(stats.used);
    }
    assert_eq!(
        used[0], used[1],
        "the store grew from {} to {} words between {} and {} statements",
        used[0], used[1], COUNTS[0], COUNTS[1]
    );
    assert_eq!(
        used[1], used[2],
        "the store grew from {} to {} words between {} and {} statements",
        used[1], used[2], COUNTS[1], COUNTS[2]
    );
}

#[test]
fn switching_the_window_off_restores_the_linear_growth() {
    // The same two expressions with the window held out of the picture: this is the pre-fix
    // shape, and the pair is what proves the window is doing the work rather than the expression
    // being cheap.
    let mut used = Vec::new();
    for n in [COUNTS[1], COUNTS[2]] {
        let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
        runtime.set_eval_scratch_enabled(false);
        eval(&mut runtime, &discarded_builds(n)).expect("the expression evaluates");
        let stats = runtime.eval_region_stats();
        assert_eq!(stats.resets, 0, "the window was switched off");
        used.push(stats.used);
    }
    assert!(
        used[1] >= 5 * used[0],
        "with the window off the store should be linear in the statement count: \
         {} statements used {} words, {} used {}",
        COUNTS[1],
        used[0],
        COUNTS[2],
        used[1]
    );
}

#[test]
fn a_persistent_term_value_survives_every_reset() {
    // The value is written by `stabilize_term` (a runtime-lifetime store this module cannot
    // enumerate), which latches the window — so this is also the pin for the refusal path: a
    // wrong reset here would reduce the *reclaimed* words instead of the stored ones.
    let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
    let expr = format!(
        "persistent_term:put(scratch_retained, [{{I,I,I}} || I <- lists:seq(1,{ELEMENTS})]), \
         {builds}\
         lists:sum([A+B+C || {{A,B,C}} <- persistent_term:get(scratch_retained)])",
        builds = discarded_builds(COUNTS[2])
            .trim_end_matches("ok")
            .to_string(),
    );
    let value = eval(&mut runtime, &expr).expect("the expression evaluates");
    assert_eq!(
        as_integer(value),
        3 * u64::from(ELEMENTS) * u64::from(ELEMENTS + 1) / 2,
        "the stored value was corrupted by a reset"
    );
    let stats = runtime.eval_region_stats();
    assert!(
        stats.declines > 0,
        "the lifetime store must have latched the window"
    );
}

#[test]
fn an_environment_binding_survives_every_reset() {
    // The binding is a root of the descent, so the boundary that sees it declines and every later
    // boundary may reclaim. A false recycle would corrupt the binding in place, which the reduce
    // below would then read.
    let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
    let expr = format!(
        "A = [{{I,I,I}} || I <- lists:seq(1,{ELEMENTS})], \
         {builds}\
         lists:sum([X+Y+Z || {{X,Y,Z}} <- A])",
        builds = discarded_builds(COUNTS[2])
            .trim_end_matches("ok")
            .to_string(),
    );
    let value = eval(&mut runtime, &expr).expect("the expression evaluates");
    assert_eq!(
        as_integer(value),
        3 * u64::from(ELEMENTS) * u64::from(ELEMENTS + 1) / 2,
        "the environment binding was corrupted by a reset"
    );
    assert!(
        runtime.eval_region_stats().resets > 0,
        "the window must have reset around the retained binding"
    );
}

#[test]
fn a_mailbox_message_survives_every_reset() {
    // The message is copied into this process's mailbox, which is one of the enumerated root
    // families — a reset that ran while a stale copy was live would show up in the reduce.
    let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
    let expr = format!(
        "self() ! [{{I,I}} || I <- lists:seq(1,{ELEMENTS})], \
         {builds}\
         receive L -> lists:sum([A || {{A,_}} <- L]) after 5000 -> timeout end",
        builds = discarded_builds(COUNTS[2])
            .trim_end_matches("ok")
            .to_string(),
    );
    let value = eval(&mut runtime, &expr).expect("the expression evaluates");
    assert_eq!(
        as_integer(value),
        u64::from(ELEMENTS) * u64::from(ELEMENTS + 1) / 2,
        "the mailbox message was corrupted by a reset"
    );
}

#[test]
fn a_pending_timer_message_survives_every_reset() {
    // The sharpest one: between `erlang:send_after/3` and the delivery the message lives in the
    // runtime's **timer heap**, a store outside every heap — and it is exactly the family
    // `Runtime::stabilize_term`'s latch exists for.
    let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
    let expr = format!(
        "erlang:send_after(10, self(), [{{I,I}} || I <- lists:seq(1,{ELEMENTS})]), \
         {builds}\
         receive L -> lists:sum([A || {{A,_}} <- L]) after 5000 -> timeout end",
        builds = discarded_builds(COUNTS[2])
            .trim_end_matches("ok")
            .to_string(),
    );
    let value = eval(&mut runtime, &expr).expect("the expression evaluates");
    assert_eq!(
        as_integer(value),
        u64::from(ELEMENTS) * u64::from(ELEMENTS + 1) / 2,
        "the pending timer's message was corrupted by a reset"
    );
}

#[test]
fn a_window_a_writer_latched_reports_its_reason() {
    let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
    let expr = format!(
        "persistent_term:put(scratch_latched, [{{I,I}} || I <- lists:seq(1,{ELEMENTS})]), \
         {builds}\
         ok",
        builds = discarded_builds(3).trim_end_matches("ok").to_string(),
    );
    eval(&mut runtime, &expr).expect("the expression evaluates");
    let stats = runtime.eval_region_stats();
    assert!(stats.declines > 0, "a lifetime store must decline a window");
    assert_eq!(
        stats.last_decline,
        Some("a stabilized term may be kept by a store the statement window cannot enumerate"),
        "the decline must name the holder family"
    );
}

/// **The driver's terminal term is a write-back slot, never a copy of one.**
///
/// The exit-site pass hands the store move its corpus as *writable slots*, so the word a move
/// rewrites is the word the caller hands back. For the `-eval` driver the caller's half of that
/// corpus is the evaluator's environment **plus the terminal flow the driver is about to return**
/// (`Ok(value)`, or a raise's `reason`/`head`) — and the flow is the one holder that lives in a
/// Rust local no runtime family can reach, so the driver lends its own fields (`&mut outcome`).
///
/// Lending a *copy* instead is invisible until a region the word stands in is evacuated: the pass
/// rewrites the copy, `finish` hands back the original word, and that word names a region the move
/// has just dropped — a dangling term, which any reader answers as `badarg` (the shape a
/// `filelib:wildcard/1` result took on the `-eval` route).
///
/// One top-level statement, so the pass runs only at the exit (the boundary before it has an empty
/// window): the expression builds three large lists, keeps one as its value, and leaves the store
/// with far more dead words than live ones — the shape the move fires on. Both halves are asserted:
/// the move really ran (`store_compactions`), and the returned list still reads.
#[test]
fn the_drivers_terminal_term_is_rewritten_by_a_store_move_rather_than_a_copy_of_it() {
    let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
    let expr = format!(
        "element(2, {{lists:seq(1,{ELEMENTS}), lists:seq(1,{ELEMENTS}), \
         lists:seq(1,{ELEMENTS})}})"
    );
    let value = eval(&mut runtime, &expr).expect("the expression evaluates");
    let stats = runtime.eval_region_stats();
    assert!(
        stats.reclaim.store_compactions > 0,
        "the exit-site pass must move the store for this shape, else the pin proves nothing: \
         used={} retired={} move_declines={} last_decline={:?}",
        stats.used,
        stats.retired_regions,
        stats.reclaim.store_move_declines,
        stats.reclaim.store_move_last_decline,
    );
    let expected = format!(
        "[{}]",
        (1..=ELEMENTS)
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
    let rendered = runtime
        .format_term(value, None)
        .expect("the returned term is readable after the move");
    // `~p` breaks a list past the printer's line width onto continuation lines; only the layout is
    // dropped, never an element (the same comparison `tests/a9_native_floor.rs` makes).
    let rendered: String = rendered.lines().map(str::trim_start).collect();
    assert_eq!(rendered, expected, "the returned list is intact");
}

/// The measurement behind `PERF.md`'s growth table: the store's own figures for the reducible
/// shape, per statement count, with the window **on** and **off** on two runtimes.
///
/// `#[ignore]`d because it prints rather than asserts: it is a *measurement*, run by hand
/// (`--ignored --nocapture`) and recorded in `PERF.md`. The asserting pins are
/// [`the_store_does_not_grow_with_the_statement_count`] and
/// [`switching_the_window_off_restores_the_linear_growth`], which bracket the same two shapes.
///
/// The host is shared, so `/proc/loadavg` is printed with the figures: every number here is a
/// **word count**, a deterministic consequence of the expression, never a timing.
#[test]
#[ignore = "measurement: prints the store's word counts, asserts nothing"]
fn the_growth_figures() {
    eprintln!("== eval store growth, loadavg at start ==");
    eprintln!("{}", loadavg());
    for n in [1_usize, 10, 100, 1_000] {
        let expr = discarded_builds(n);
        for enabled in [true, false] {
            print_figure(&format!("discarded_builds({n})"), &expr, enabled);
        }
    }
    eprintln!("== the two shapes the pre-fix note cited ==");
    for (label, expr) in [
        (
            "one lists:seq(1,5000)",
            "_ = lists:seq(1,5000), ok",
        ),
        (
            "two lists:seq(1,5000)",
            "_ = lists:seq(1,5000), _ = lists:seq(1,5000), ok",
        ),
        (
            "one binary:copy(<<1>>,40000)",
            "_ = binary:copy(<<1>>,40000), ok",
        ),
        (
            "two binary:copy(<<1>>,40000)",
            "_ = binary:copy(<<1>>,40000), _ = binary:copy(<<1>>,40000), ok",
        ),
    ] {
        for enabled in [true, false] {
            print_figure(label, expr, enabled);
        }
    }
    eprintln!("== loadavg at end ==");
    eprintln!("{}", loadavg());
}

/// Evaluates `expr` on a fresh runtime with the window on or off and prints the store's figures.
fn print_figure(label: &str, expr: &str, enabled: bool) {
    let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
    if !enabled {
        runtime.set_eval_scratch_enabled(false);
    }
    eval(&mut runtime, expr).expect("the expression evaluates");
    let stats = runtime.eval_region_stats();
    eprintln!(
        "shape={label:<28} window={:<3} used={:<8} capacity={:<9} opens={:<5} resets={:<5} \
         declines={:<4} last_decline={:?} words_reclaimed={:<8} \
         retired_regions={} retired_capacity_words={}",
        if enabled { "on" } else { "off" },
        stats.used,
        stats.capacity,
        stats.opens,
        stats.resets,
        stats.declines,
        stats.last_decline,
        stats.words_reclaimed,
        stats.retired_regions,
        stats.retired_capacity_words,
    );
}
/// `/proc/loadavg` verbatim — the shared-host context every figure in `PERF.md` is quoted with.
///
/// Never panics: a platform without the file answers a note instead.
fn loadavg() -> String {
    match std::fs::read_to_string("/proc/loadavg") {
        Ok(text) => text.trim().to_string(),
        Err(_) => "<no /proc/loadavg>".to_string(),
    }
}

/// The `u64` a `-eval` expression answered, for the reduces above.
fn as_integer(term: Eterm) -> u64 {
    assert!(beam_term::is_small(term), "an integer result");
    u64::try_from(beam_term::signed_value(term)).expect("a non-negative result")
}
