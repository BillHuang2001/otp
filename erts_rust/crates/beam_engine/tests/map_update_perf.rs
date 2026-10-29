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

//! A measurement harness for the **four map instructions** —
//! [`Instr::PutMapAssoc`], [`Instr::PutMapExact`], [`Instr::HasMapFields`] and
//! [`Instr::GetMapElements`] — driven as hand-built instructions over a
//! [`MinimalContext`], exactly the way the family tests drive them.
//!
//! # Why this file exists *before* a semantics change
//!
//! The four instructions share one shape today: a *read* side that materializes the
//! whole source map as a Rust `Vec<(Eterm, Eterm)>` ([`beam_engine::terms::map_entries`])
//! and a *write* side that rebuilds the map from scratch through
//! [`beam_engine::terms::make_map`] → [`beam_term::map::write_map`], reserving
//! `map_words_capacity(key count + new pairs)` arena words per call — for a HAMT source
//! that is the `21·n + 18` upper bound, so a single `#{n => -1}` update over a
//! 6 250-key map reserves ~131 000 words (~1 MiB) to change **one** value.
//!
//! This harness pins the two things a follow-up change must move, and it must land
//! **unmodified** on both sides so the "after" figures are comparable:
//!
//! * **ns/call** — the median of nine samples, each a fresh context timed over `N`
//!   calls (see [`calls_per_sample`]);
//! * **words reserved per call** — the arena delta divided by the number of calls
//!   (see [`words_per_call`]), a *load-independent* counter: no scheduler, no clock
//!   and no host contention can perturb it.
//!
//! The rows are reported per instruction and per map size, in the
//! `perf inst=… n=… ns_per_call=… words_per_call=…` shape, so the figures can be
//! copied into a report verbatim.
//!
//! # The host is shared — nothing here asserts on time
//!
//! There is **no wall-time assertion and no absolute-number assertion** anywhere in this
//! file. It exits non-zero only for a *semantic* failure (a produced map that lost a key,
//! a fetched element that is wrong, a `has_map_fields` branch that took the wrong arm) or
//! for a violation of a stated invariant (every call reserving the same number of arena
//! words). `/proc/loadavg` is printed before and after the whole run so a figure is never
//! quoted without the load it was taken under.
//!
//! # Measuring `words_per_call` honestly
//!
//! [`MinimalContext`]'s arena never reclaims on its own, so a *fresh* context is created
//! for every sample — the reservation above would otherwise balloon by `N · 21·n` words
//! inside one context. The word probe therefore uses a *bounded* number of calls
//! ([`PROBE_CALLS`]) and divides; the first call runs before the delta opens, so a
//! one-time demand cannot be attributed to the steady state, and the delta must be an
//! exact multiple of the call count (every call reserves the same block — the invariant
//! this pins).
//!
//! # Running it
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
//!     -p beam_engine --release --test map_update_perf -- --ignored --nocapture
//! ```

use std::collections::BTreeMap;
use std::hint::black_box;
use std::time::Instant;

use beam_engine::{terms, Machine, MinimalContext};
use beam_loader::{Function, Instr, Label, MapWriteArgs, Module, Operand, NO_FAIL};
use beam_term::{make_small, AtomTable, Eterm};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// The measured source-map sizes: a well-past-`MAP_SMALL_MAP_LIMIT` (32) HAMT, a
/// compiler-sized map, and the largest size the map COW benchmark uses.
const SIZES: [usize; 3] = [250, 2_500, 6_250];

/// Samples per (instruction, size) row; the reported figure is the median.
const SAMPLES: usize = 9;

/// Calls run in a throw-away context before the samples, so the first sample is not
/// paying for a cold process (allocator warm-up, code pages).
const WARMUP_CALLS: usize = 4;

/// One warm-up call inside every sample, before the clock starts: it absorbs the
/// per-sample `Machine` setup and the run's label map, so the timed window is the
/// instruction's steady-state cost.
const SAMPLE_WARMUP_CALLS: usize = 1;

/// Calls in the word probe (bounded: each one reserves a whole map block).
const PROBE_CALLS: usize = 4;

/// The entry label every hand-built function starts at.
const ENTRY: Label = Label(1);

/// The failure label of the branch-shaped verification modules.
const FAIL_LABEL: Label = Label(2);

/// The absent key the insert row adds.
const ABSENT_KEY: i64 = 99_999;

/// The value the replace rows store under the rebuilt key (never equal to the source's
/// stored value, so the no-change fast path cannot swallow the update).
const REPLACED_VALUE: i64 = -1;

/// Calls per timed sample at `n` source keys: `clamp(2 000 000 / n, 4, 200)`.
///
/// The budget keeps a sample long enough for the clock at small `n` and bounds the
/// arena a single sample commits at large `n` (`n = 6 250` ⇒ ~1 MiB per call).
fn calls_per_sample(n: usize) -> usize {
    (2_000_000 / n).clamp(4, 200)
}

// ---------------------------------------------------------------------------
// The measured instructions
// ---------------------------------------------------------------------------

/// The five measured rows: the four instructions, with `put_map_assoc` measured twice
/// (replacing a present key vs. inserting an absent one — the two shapes the follow-up
/// change treats differently).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Route {
    /// `put_map_assoc` replacing one present key's value.
    AssocReplace,
    /// `put_map_assoc` inserting one absent key.
    AssocInsert,
    /// `put_map_exact` replacing one present key's value.
    ExactReplace,
    /// `has_map_fields` over two present keys.
    HasFields,
    /// `get_map_elements` over two present keys into two destinations.
    GetElements,
}

impl Route {
    /// Every row, in report order.
    const ALL: [Route; 5] = [
        Route::AssocReplace,
        Route::AssocInsert,
        Route::ExactReplace,
        Route::HasFields,
        Route::GetElements,
    ];

    /// The row's name, as it appears in the machine-readable summary line.
    fn name(self) -> &'static str {
        match self {
            Route::AssocReplace => "assoc-replace",
            Route::AssocInsert => "assoc-insert",
            Route::ExactReplace => "exact-replace",
            Route::HasFields => "has-fields",
            Route::GetElements => "get-elements",
        }
    }

    /// Whether the instruction *builds* a map (and so reserves arena words).
    fn writes(self) -> bool {
        matches!(
            self,
            Route::AssocReplace | Route::AssocInsert | Route::ExactReplace
        )
    }

    /// The `(key, value)` / `(key, destination)` pairs the instruction carries at `n`
    /// source keys.
    fn pairs(self, n: usize) -> Vec<(Operand, Operand)> {
        match self {
            Route::AssocReplace | Route::ExactReplace => {
                vec![(Operand::Integer(n as i64), Operand::Integer(REPLACED_VALUE))]
            }
            Route::AssocInsert => {
                vec![(Operand::Integer(ABSENT_KEY), Operand::Integer(1))]
            }
            Route::HasFields | Route::GetElements => {
                // Two keys that are both present: key `1` (the first) and key `n` (the
                // last), so neither a scan from the front nor one from the back can
                // trivially short-circuit both.
                vec![
                    (Operand::Integer(1), Operand::X(1)),
                    (Operand::Integer(n as i64), Operand::X(2)),
                ]
            }
        }
    }
}

/// The function the row is measured on: the instruction alone, then `return`
/// (which hands back `x(0)`).
///
/// Nothing else is in the timed body — no verification tail — so the figure is the
/// instruction, not the harness around it. `x(0)` holds the source map, and every
/// write row writes its result back to `x(0)`, so the returned term is the update.
fn measured_instruction(route: Route, n: usize) -> Instr {
    let pairs = route.pairs(n);
    match route {
        Route::AssocReplace | Route::AssocInsert => Instr::PutMapAssoc(Box::new(MapWriteArgs {
            fail: NO_FAIL,
            source: Operand::X(0),
            dst: Operand::X(0),
            live: 1,
            pairs,
        })),
        Route::ExactReplace => Instr::PutMapExact(Box::new(MapWriteArgs {
            fail: NO_FAIL,
            source: Operand::X(0),
            dst: Operand::X(0),
            live: 1,
            pairs,
        })),
        Route::HasFields => Instr::HasMapFields {
            fail: NO_FAIL,
            source: Operand::X(0),
            keys: vec![Operand::Integer(1), Operand::Integer(n as i64)],
        },
        Route::GetElements => Instr::GetMapElements {
            fail: NO_FAIL,
            source: Operand::X(0),
            pairs,
        },
    }
}

/// The measured module for `route` at `n` source keys.
fn measured_module(route: Route, n: usize) -> (Module, Label) {
    module_with(|| vec![measured_instruction(route, n), Instr::Return], 1)
}

// ---------------------------------------------------------------------------
// Module construction
// ---------------------------------------------------------------------------

/// Builds a one-function module (`f/arity`) whose code is `ENTRY` followed by `body`.
fn module_with(body: impl FnOnce() -> Vec<Instr>, arity: u32) -> (Module, Label) {
    let mut code = vec![Instr::Label(ENTRY)];
    code.extend(body());
    let mut module = Module::default();
    let name = module.atom_table.intern("f");
    module.name = name;
    let labels: BTreeMap<Label, usize> = code
        .iter()
        .enumerate()
        .filter_map(|(index, instr)| match instr {
            Instr::Label(label) => Some((*label, index)),
            _ => None,
        })
        .collect();
    module.functions.push(Function {
        func_label: ENTRY,
        entry_label: ENTRY,
        module: module.name,
        name,
        arity,
        code,
        blocks: Vec::new(),
        labels,
    });
    (module, ENTRY)
}

// ---------------------------------------------------------------------------
// Driving and measuring
// ---------------------------------------------------------------------------

/// Runs `calls` invocations of the module's entry with `source` in `x(0)`.
fn run_calls(
    machine: &mut Machine,
    module: &Module,
    entry: Label,
    source: Eterm,
    ctx: &mut MinimalContext,
    calls: usize,
) {
    for _ in 0..calls {
        let result = machine
            .run_label(module, entry, 1, &[source], ctx)
            .expect("the measured instruction must run on a hash-map source");
        // The result borrows the arena this context owns; hand it to `black_box` so the
        // optimizer cannot elide a measured call.
        black_box(result);
    }
}

/// One measured run: `calls` invocations, returning the `(result, ns)` pair.
fn timed_calls(module: &Module, entry: Label, source: Eterm, calls: usize) -> (Eterm, f64) {
    // A fresh context per sample: the arena never reclaims on its own, and one
    // `put_map_assoc` over a 6 250-key HAMT reserves ~1 MiB, so reusing a context across
    // samples would both balloon the arena and slow every later sample down.
    let mut ctx = MinimalContext::new();
    let mut machine = Machine::new();
    // Untimed warm-up: the per-sample machine setup and the run's label map.
    run_calls(
        &mut machine,
        module,
        entry,
        source,
        &mut ctx,
        SAMPLE_WARMUP_CALLS,
    );
    let start = Instant::now();
    let mut last = source;
    for _ in 0..calls {
        last = machine
            .run_label(module, entry, 1, &[source], &mut ctx)
            .expect("the measured instruction must run on a hash-map source");
        black_box(last);
    }
    let elapsed = start.elapsed();
    (last, elapsed.as_nanos() as f64 / calls as f64)
}

/// The median of `values` (sorts in place; an odd count is assumed — [`SAMPLES`] is 9).
fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

/// The median ns/call over [`SAMPLES`] fresh-context samples of `calls` invocations.
fn ns_per_call(module: &Module, entry: Label, source: Eterm, calls: usize) -> f64 {
    // The warm-up pass, in its own throw-away context.
    {
        let mut ctx = MinimalContext::new();
        let mut machine = Machine::new();
        run_calls(&mut machine, module, entry, source, &mut ctx, WARMUP_CALLS);
    }
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let (_, ns) = timed_calls(module, entry, source, calls);
        samples.push(ns);
    }
    median(&mut samples)
}

/// The words the instruction reserves per call, with the semantics checked on the way.
///
/// `live_words` is the arena's *current* figure (it falls on a reclaim), so a
/// before/after delta over `PROBE_CALLS` calls is exact. The first call runs before the
/// delta opens, so a one-time demand is never attributed to the steady state, and the
/// delta must divide cleanly by the call count — every call reserves the same block,
/// which is the invariant asserted here (the count itself is never asserted).
///
/// The produced term is read **inside** this function, while the arena that holds it is
/// still alive: a term naming an arena word is freed when its context drops, so handing
/// it back to the caller would be a use-after-free (see `CONTEXT.md`'s "the arena's
/// reclaim tiers, and term/region lifetimes").
fn words_per_call(
    route: Route,
    n: usize,
    source: Eterm,
    module: &Module,
    entry: Label,
    atoms: &AtomTable,
) -> usize {
    let mut ctx = MinimalContext::new();
    let mut machine = Machine::new();
    run_calls(&mut machine, module, entry, source, &mut ctx, 1);
    let before = ctx.heap_stats().live_words;
    let mut last = source;
    for _ in 0..PROBE_CALLS {
        last = machine
            .run_label(module, entry, 1, &[source], &mut ctx)
            .expect("the measured instruction must run on a hash-map source");
    }
    let delta = ctx.heap_stats().live_words - before;
    assert_eq!(
        delta % PROBE_CALLS,
        0,
        "the instruction must reserve the same block on every call: {delta} words over \
         {PROBE_CALLS} calls does not divide"
    );
    verify(route, n, source, last, atoms);
    delta / PROBE_CALLS
}

// ---------------------------------------------------------------------------
// The host load
// ---------------------------------------------------------------------------

/// One `/proc/loadavg` line, or an honest note when the file is unreadable.
fn loadavg() -> String {
    match std::fs::read_to_string("/proc/loadavg") {
        Ok(text) => text.trim_end().to_string(),
        Err(err) => format!("unavailable ({err})"),
    }
}

// ---------------------------------------------------------------------------
// The semantic pin (independent of every figure above)
// ---------------------------------------------------------------------------

/// A map of `n` keys `1..=n` with value `3 * key` — built by the engine's own general
/// builder, so it is a HAMT once `n > MAP_SMALL_MAP_LIMIT` (32).
fn source_map(ctx: &mut MinimalContext, module: &Module, n: usize) -> Eterm {
    let pairs: Vec<(Eterm, Eterm)> = (1..=n as i64)
        .map(|key| (make_small(key), make_small(key * 3)))
        .collect();
    let map = terms::make_map(ctx, &pairs, &module.atom_table).expect("build the source map");
    assert!(
        terms::is_hashmap(map),
        "n = {n}: the source map must be a hash map"
    );
    assert_eq!(
        beam_term::map::map_size(map, &module.atom_table).unwrap(),
        n,
        "n = {n}: the source map holds every key"
    );
    map
}

/// Asserts the semantic outcome of one measured run at `n` source keys.
///
/// `result` is the term the measured instruction handed back and it is read **here**,
/// while the arena that owns it is still alive; `atoms` is the source map's own atom
/// namespace (the keys are small integers, so this only keeps the read consistent).
fn verify(route: Route, n: usize, source: Eterm, result: Eterm, atoms: &AtomTable) {
    match route {
        Route::AssocReplace | Route::ExactReplace => {
            let produced = terms::map_entries(result).expect("the update produced a map");
            assert_eq!(produced.len(), n, "n = {n}: a replacement loses no key");
            assert_eq!(
                beam_term::map::map_get(result, make_small(n as i64), atoms).unwrap(),
                Some(make_small(REPLACED_VALUE)),
                "n = {n}: the present key was replaced"
            );
            assert_eq!(
                beam_term::map::map_get(result, make_small(1), atoms).unwrap(),
                Some(make_small(3)),
                "n = {n}: an untouched key keeps its value"
            );
        }
        Route::AssocInsert => {
            let produced = terms::map_entries(result).expect("the update produced a map");
            assert_eq!(produced.len(), n + 1, "n = {n}: an insert adds one key");
            assert_eq!(
                beam_term::map::map_get(result, make_small(ABSENT_KEY), atoms).unwrap(),
                Some(make_small(1)),
                "n = {n}: the absent key was inserted"
            );
            assert_eq!(
                beam_term::map::map_get(result, make_small(n as i64), atoms).unwrap(),
                Some(make_small(3 * n as i64)),
                "n = {n}: an untouched key keeps its value"
            );
        }
        Route::HasFields => {
            // The instruction neither builds nor rewrites the map: its answer is the
            // source word it was given (`x(0)` is untouched).
            assert_eq!(
                result, source,
                "n = {n}: a passing has_map_fields leaves x(0) alone"
            );
            // ... and both arms are exercised by the branch-shaped twin, over the same
            // source: a pair of present keys falls through, an absent key jumps to `fail`.
            assert_eq!(
                branch_answer(
                    source,
                    vec![Operand::Integer(1), Operand::Integer(n as i64)]
                ),
                make_small(1),
                "n = {n}: both keys are present, so the guard passes"
            );
            assert_eq!(
                branch_answer(source, vec![Operand::Integer(ABSENT_KEY)]),
                make_small(0),
                "n = {n}: an absent key takes the fail branch"
            );
        }
        Route::GetElements => {
            // The instruction's destinations must hold exactly the map's values. Its own
            // body returns `x(0)` (the source), so the two fetched elements are read
            // through the tuple-building twin.
            check_elements_tuple(n, source);
        }
    }
}

/// Runs a `has_map_fields` module whose fail branch answers `0` and whose fall-through
/// answers `1`, over `source`.
///
/// The answer is a small integer, so it stays readable after the run's arena is dropped.
fn branch_answer(source: Eterm, keys: Vec<Operand>) -> Eterm {
    let (module, entry) = module_with(
        || {
            vec![
                Instr::HasMapFields {
                    fail: FAIL_LABEL,
                    source: Operand::X(0),
                    keys,
                },
                Instr::Move {
                    src: Operand::Integer(1),
                    dst: Operand::X(0),
                },
                Instr::Return,
                Instr::Label(FAIL_LABEL),
                Instr::Move {
                    src: Operand::Integer(0),
                    dst: Operand::X(0),
                },
                Instr::Return,
            ]
        },
        1,
    );
    let mut ctx = MinimalContext::new();
    let mut machine = Machine::new();
    machine
        .run_label(&module, entry, 1, &[source], &mut ctx)
        .expect("a has_map_fields branch run")
}

/// Runs a `get_map_elements` module that builds `{x(1), x(2)}` into `x(0)` and asserts
/// the tuple's elements, over `source`.
///
/// The assertions happen here because the produced tuple lives in the run's arena: a
/// term naming an arena word is gone when the context drops.
fn check_elements_tuple(n: usize, source: Eterm) {
    let (module, entry) = module_with(
        || {
            vec![
                Instr::GetMapElements {
                    fail: NO_FAIL,
                    source: Operand::X(0),
                    pairs: vec![
                        (Operand::Integer(1), Operand::X(1)),
                        (Operand::Integer(n as i64), Operand::X(2)),
                    ],
                },
                Instr::PutTuple2 {
                    dst: Operand::X(0),
                    elements: vec![Operand::X(1), Operand::X(2)],
                },
                Instr::Return,
            ]
        },
        1,
    );
    let mut ctx = MinimalContext::new();
    let mut machine = Machine::new();
    let values = machine
        .run_label(&module, entry, 1, &[source], &mut ctx)
        .expect("a get_map_elements run");
    assert_eq!(
        terms::tuple_arity(values).expect("a 2-tuple"),
        2,
        "n = {n}: two destinations"
    );
    assert_eq!(
        terms::tuple_element(values, 0).unwrap(),
        make_small(3),
        "n = {n}: the first key's value"
    );
    assert_eq!(
        terms::tuple_element(values, 1).unwrap(),
        make_small(3 * n as i64),
        "n = {n}: the last key's value"
    );
}

// ---------------------------------------------------------------------------
// The measurement
// ---------------------------------------------------------------------------

/// The map-instruction measurement: five rows at three source sizes, with the
/// **words reserved per call** (load-independent) beside the **median ns/call**.
///
/// `#[ignore]`d because it is a measurement, not a gate: over a whole run it hands out
/// gigabytes of arena (in fresh contexts, so the peak is one sample's ~200 MiB) and
/// takes seconds. The semantic pins inside [`verify`] run with it, so a produced map, a
/// fetched element or a `has_map_fields` branch that is wrong fails the run.
#[test]
#[ignore = "measurement harness: run with --ignored --nocapture"]
fn map_instructions_measured() {
    let before_load = loadavg();
    eprintln!();
    eprintln!("loadavg before: {before_load}");
    eprintln!();
    eprintln!(
        "{:<14} {:>6} {:>10} {:>12} {:>12} {:>12} {:>6}",
        "inst", "n", "calls", "median ns/c", "words/call", "hamt model", "match"
    );
    eprintln!("{}", "-".repeat(80));

    for n in SIZES {
        // The source map, its atom namespace and the fixture context all outlive the
        // samples: the source is only ever read (a map term is immutable), so one build
        // per size serves every row.
        let (fixture_module, _) = measured_module(Route::AssocReplace, n);
        let mut fixture_ctx = MinimalContext::new();
        let source = source_map(&mut fixture_ctx, &fixture_module, n);
        let atoms = &fixture_module.atom_table;

        for route in Route::ALL {
            let (module, entry) = measured_module(route, n);
            let calls = calls_per_sample(n);
            let ns = ns_per_call(&module, entry, source, calls);
            let words = words_per_call(route, n, source, &module, entry, atoms);

            // The known "before" model of the write rows: the whole map block is
            // re-reserved from the source's key count plus the new pairs.
            let model = if route.writes() {
                beam_term::map::map_words_capacity(n + route.pairs(n).len())
            } else {
                0
            };
            let matches = words == model;
            eprintln!(
                "{:<14} {:>6} {:>10} {:>12.1} {:>12} {:>12} {:>6}",
                route.name(),
                n,
                calls,
                ns,
                words,
                if route.writes() {
                    model.to_string()
                } else {
                    "-".to_string()
                },
                if route.writes() {
                    if matches {
                        "yes"
                    } else {
                        "NO"
                    }
                } else {
                    "-"
                }
            );
            // The machine-readable line: one per row, for copying into a report.
            println!(
                "perf inst={} n={} ns_per_call={:.1} words_per_call={}",
                route.name(),
                n,
                ns,
                words
            );
        }
        eprintln!("{}", "-".repeat(80));
    }

    let after_load = loadavg();
    eprintln!();
    eprintln!("loadavg after:  {after_load}");
    eprintln!();
}
