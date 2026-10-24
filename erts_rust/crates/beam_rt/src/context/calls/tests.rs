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

//! Units for the copy-out's **retry policy** (see
//! [`RuntimeContext::move_result_into_process_heap`]): the growth hint recovered from the
//! copier's own refusal, and the gate that keeps a *permanent* refusal from growing the
//! destination at all. The copy itself is exercised end to end by `tests/perf_call.rs`
//! (path B copies a container result on every iteration) and the `src/context/tests.rs`
//! registered-BIF suites.

use beam_common::{AllocFailure, AllocKind};
use beam_engine::ExecutionContext;
use beam_process::SignalAtoms;
use beam_term::make_small;
use beam_term::prelude::make_header;
use super::*;

/// The number of `Eterm`s a byte figure converts back to.
const WORD: usize = core::mem::size_of::<Eterm>();

/// A `runtime` + a caller-owned `ProcessTable` holding one default process, plus the pid —
/// the `for_process` shape, i.e. the one where the copy-out (and its retry) is ON.
fn process_table(rt: &mut crate::runtime::Runtime) -> (ProcessTable, beam_process::ProcessId) {
    let signals = SignalAtoms::from_table(rt.code_server_atoms_mut());
    let mut table = ProcessTable::new(signals);
    let pid = table.spawn_default().expect("a default process");
    (table, pid)
}

#[test]
fn the_growth_hint_is_the_copiers_own_shortfall_in_words() {
    // `beam_message`'s exhaustion error carries `words * size_of::<Eterm>()` in
    // `AllocFailure::requested` (its documented contract), so dividing recovers exactly the
    // flat word count the refused copy wanted — with no second traversal of the graph.
    let exhaustion = BeamError::Alloc(AllocFailure::new(AllocKind::Message, 37 * WORD));
    assert_eq!(
        copy_shortfall_words(&exhaustion),
        37,
        "the copier's own word count is recovered verbatim"
    );
    // A refusal of any other class reports no room request: the conservative `0` (the
    // caller's `max(256)` floor turns it into a legal growth hint).
    assert_eq!(copy_shortfall_words(&BeamError::BadArg), 0);
    assert_eq!(copy_shortfall_words(&BeamError::BadArity), 0);
    assert_eq!(
        copy_shortfall_words(&BeamError::Alloc(AllocFailure::new(AllocKind::Heap, 9 * WORD))),
        9,
        "any `Alloc` refusal carries its own requested byte count"
    );
}

#[test]
fn a_permanent_copy_refusal_grows_no_nursery_and_writes_nothing() {
    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    let spaces_before = table
        .get(pid)
        .expect("the spawned process")
        .collector()
        .young_space_count();
    let used_before = table.get(pid).expect("the spawned process").heap().used();
    let copy = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        // A bare header word is not a standing term at all (`make_header(0, 0)` is the C
        // ABI's `THE_NON_VALUE`): `copy_struct` answers the copier's typed malformed-graph
        // `badarg` — `BeamError::BadArg`, **not** a message-heap exhaustion. Growing the
        // destination cannot make it succeed, so the retry must not run: no space is
        // retired, no word is written, and the caller gets `None` (so `call_registered`
        // leaves both the result and the pooled BIF heap exactly as they were).
        ctx.move_result_into_process_heap(make_header(0, 0))
    };
    assert!(
        copy.is_none(),
        "a malformed graph cannot be re-homed; the caller must leave the result untouched"
    );
    let process = table.get(pid).expect("the spawned process");
    assert_eq!(
        process.collector().young_space_count(),
        spaces_before,
        "a permanent refusal must not retire a nursery space (the item-4 growth is per-call)"
    );
    assert_eq!(
        process.heap().used(),
        used_before,
        "a refused copy writes no destination word"
    );
}

#[test]
fn an_exhaustion_refusal_grows_the_nursery_and_the_retry_copies() {
    // The positive control for the gate above: a **valid** container the nursery cannot hold
    // still grows the destination by the copier's own word count and is copied on the retry.
    // `lists:seq/2`'s 4000-odd-word result does not fit the default 233-word nursery.
    use beam_engine::ExecutionContext;

    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    let spaces_before = table
        .get(pid)
        .expect("the spawned process")
        .collector()
        .young_space_count();
    let copied = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        notes::reset();
        ctx.call_bif("lists", "seq", 2, &[make_small(1), make_small(2000)])
            .expect("lists:seq(1, 2000)")
    };
    assert!(
        beam_term::prelude::is_list(copied) && !beam_term::literal::is_literal(copied),
        "the result is a real list built on the process's own heap"
    );
    assert!(
        table
            .get(pid)
            .expect("the spawned process")
            .collector()
            .young_space_count()
            > spaces_before,
        "an exhaustion refusal grows the destination (the retry then fits)"
    );
    // The successful retry's destination was **empty** as handed to the copier (fast path
    // eligible) — `lists:seq/2` carries no off-heap reference, and the retry builds its own
    // fresh table, so nothing from the refused attempt can have poisoned it.
    let (attempts, grows, empties) = notes::snapshot();
    assert_eq!(
        (attempts, grows),
        (2, 1),
        "one refused attempt, one growth, one retry"
    );
    assert_eq!(
        empties, attempts,
        "every attempt — including the retry — was offered a fully empty destination"
    );
}

/// Builds an `n`-cell list of small ints on a heap the copier will walk — the same shape both
/// the `copy_bound` payload and [`bench_shaped_result`] are built from.
fn int_list(heap: &mut beam_alloc::Heap, n: i64) -> Result<Eterm> {
    let mut list = beam_term::prelude::NIL;
    for i in (1..=n).rev() {
        list = heap.cons(make_small(i), list)?;
    }
    Ok(list)
}

/// A registered BIF whose result is a **container shaped like the `copy_bound/ets_copy` payload**:
/// 73 flat words across 32 objects, built on the pooled BIF heap, so [`RuntimeContext::call_bif`]
/// must re-home it onto the running process's own nursery on every call.
///
/// The shape is what makes the growth policy observable: at 73 words the copy cannot fit the
/// default 233-word nursery beyond the third call, and the copier's shortfall (`73`) is far below
/// the 1024-word headroom floor, so every refusal is the same refusal the benchmark case pays.
fn bench_shaped_result(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let heap = ctx.heap();
    let ten = int_list(heap, 10)?; // 20 words, 10 objects
    let eleven = int_list(heap, 11)?; // 22 words, 11 objects
    let eight = int_list(heap, 8)?; // 16 words,  8 objects
    let triple = heap.tuple(&[make_small(7), make_small(8), make_small(9)])?; // 4 words
    let quad = heap.tuple(&[make_small(1), make_small(2), make_small(3), make_small(4)])?; // 5
    // 6 words: 20 + 22 + 16 + 4 + 5 + 6 = 73.
    heap.tuple(&[ten, triple, eleven, quad, eight, make_small(-42)])
}

/// **The copy-out grows the nursery with HEADROOM, not by the bare shortfall.**
///
/// This is the counter pin for the `copy_bound/ets_copy` regression. A growth sized by the
/// copier's exact shortfall (`needed.max(256)`) leaves the fresh destination only one table step
/// ahead of the graph that forced it — 376 words for the 73-word payload — so the next calls of
/// the same size refuse again within a handful of ops, and every refusal pays the whole sizing
/// machinery of a refused copy (`try_copy_fast` → `preflight_words` → the bounded sizing walk →
/// `size_object_with`) before the retry.
///
/// The loop below re-homes the [`bench_shaped_result`] container into a **default** (233-word)
/// nursery and reads the copy-out's structural counters:
///
/// * `grows` — one per `Process::grow_nursery` call, i.e. one per refused-destination call;
/// * `attempts` — one per `beam_message::copy_struct` call, so `attempts == copies + grows` when
///   nothing else goes wrong.
///
/// Two properties are pinned (the figures are this suite's own measurement):
///
/// 1. **The first growth's installed capacity** carries headroom — `1598` words, the growth
///    table's step above the 1024-word request, against `376` for a bare-shortfall growth. That is
///    the discriminating assertion, and it is what makes one growth serve ~21 further copies of
///    the payload instead of ~5. The headroom lives until a **collection** reclaims it
///    (`Nursery::collapse` only drops retired spaces, and `young_after_collection` shrinks the
///    nursery only when the round's demand is under a quarter of it), so the growth is not undone
///    by the next allocation.
/// 2. **The growth count stays rare over a long run** — `grows * 8 < copies` (measured: 5 growths
///    over 1004 copies with the headroom, 6 with the bare shortfall; the long-run figure converges
///    because `Nursery::grow` at least doubles a space it cannot enlarge, but 1 growth per 8
///    copies would mean the growth is again sized to the shortfall).
#[test]
fn the_copy_out_grows_the_nursery_with_headroom_not_by_the_bare_shortfall() {
    /// Copies before the run's counters are read.
    const OPS: usize = 1000;
    /// How many copies the first-growth probe may need: three 73-word payloads fill a 233-word
    /// nursery, so the fourth refuses.
    const PROBE_OPS: usize = 8;
    /// The smallest installed nursery the headroom request can land on: 1024 rounds up through
    /// the growth table to 1598, and a bare-shortfall growth installs 376.
    const HEADROOM_FLOOR_WORDS: usize = 1024;

    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    {
        let registry = Rc::get_mut(&mut rt.registry)
            .expect("the runtime holds the only handle to its registry");
        let _ = registry.register("copy_out_fixture", "bench_shaped", 0, bench_shaped_result);
    }
    let nursery_before = table
        .get(pid)
        .expect("the spawned process")
        .collector()
        .young()
        .capacity_words();
    assert_eq!(
        nursery_before,
        beam_gc::DEFAULT_MIN_HEAP_WORDS,
        "the pin is about the DEFAULT nursery: a larger one would hide the growth policy"
    );
    notes::reset();
    let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
    // Phase 1 — copy until the destination refuses once, then read what that growth installed.
    let mut first_growth_words = None;
    let mut copies = 0usize;
    for op in 0..PROBE_OPS {
        let copied = ctx
            .call_bif("copy_out_fixture", "bench_shaped", 0, &[])
            .expect("the fixture BIF returns");
        copies += 1;
        assert!(
            beam_term::prelude::is_boxed(copied) && !beam_term::literal::is_literal(copied),
            "op {op}: the bench-shaped container is re-homed onto the process's own heap"
        );
        if notes::snapshot().1 > 0 {
            first_growth_words = Some(
                ctx.process_table()
                    .get(pid)
                    .expect("the spawned process")
                    .collector()
                    .young()
                    .capacity_words(),
            );
            break;
        }
    }
    let first_growth_words = first_growth_words.expect("the 73-word payload outgrows a 233-word nursery");
    assert!(
        first_growth_words >= HEADROOM_FLOOR_WORDS,
        "the copy-out must grow the nursery with HEADROOM: the first growth installed \
         {first_growth_words} words, below the {HEADROOM_FLOOR_WORDS}-word request floor — i.e. \
         the growth is again sized by the copier's bare shortfall (which installs 376 words), so \
         the very next copies of the same size refuse and re-walk the graph"
    );
    // Phase 2 — the long run the growth count is asserted over.
    for op in 0..OPS {
        let copied = ctx
            .call_bif("copy_out_fixture", "bench_shaped", 0, &[])
            .expect("the fixture BIF returns");
        copies += 1;
        assert!(
            beam_term::prelude::is_boxed(copied) && !beam_term::literal::is_literal(copied),
            "op {op}: the bench-shaped container is re-homed onto the process's own heap"
        );
    }
    drop(ctx);
    let (attempts, grows, empties) = notes::snapshot();
    assert_eq!(
        empties, attempts,
        "every attempt — refused or not — is handed a fully empty destination table"
    );
    assert_eq!(
        attempts,
        copies + grows,
        "exactly one refused attempt per growth: {attempts} attempts over {copies} copies with \
         {grows} growths"
    );
    assert!(
        grows * 8 < copies,
        "the copy-out must grow the nursery with HEADROOM: {grows} growths (and so {grows} \
         refused-and-re-walked copies) over {copies} copies exceeds 1 per 8, i.e. the growth is \
         again sized by the bare shortfall instead of a table step with room to spare"
    );
    let nursery_after = table
        .get(pid)
        .expect("the spawned process")
        .collector()
        .young()
        .capacity_words();
    assert!(
        nursery_after > first_growth_words,
        "the long run keeps stepping the nursery up ({first_growth_words} -> {nursery_after} words)"
    );
}

/// **A `max_heap_size` ceiling that refuses the headroom request still grows by the bare
/// shortfall, so the copy is re-homed instead of failing.**
///
/// `Nursery::grow` (`crates/beam_gc/src/nursery.rs:339-345`) bounds the nursery chain's
/// **summed** capacity by [`HeapPolicy::max_heap_words`] and refuses a request the remaining
/// budget cannot reach. A growth request with headroom is therefore *more* likely to be refused
/// than the bare shortfall the retry used to ask for, so the copy-out must fall back to exactly
/// that on a refusal — otherwise a configured `+hmax` would turn a growth that always succeeded
/// into a hard failure (the result left on the pooled BIF heap, un-rewound).
///
/// The policy below leaves a budget of 367 words: 1024 is unreachable, 256 is not.
#[test]
fn a_ceiling_that_refuses_the_headroom_request_still_grows_by_the_shortfall() {
    /// The nursery chain's ceiling: 233 words installed + 367 of budget.
    const CAP_WORDS: usize = 600;
    /// Copies the first-growth probe may need (three 73-word payloads fill a 233-word nursery).
    const PROBE_OPS: usize = 8;

    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let signals = SignalAtoms::from_table(rt.code_server_atoms_mut());
    let mut table = ProcessTable::new(signals);
    let pid = table
        .spawn(beam_gc::HeapPolicy::from_min_max(0, CAP_WORDS))
        .expect("a capped process");
    {
        let registry = Rc::get_mut(&mut rt.registry)
            .expect("the runtime holds the only handle to its registry");
        let _ = registry.register("copy_out_fixture", "bench_shaped", 0, bench_shaped_result);
    }
    let capacity_before = table
        .get(pid)
        .expect("the spawned process")
        .collector()
        .young()
        .capacity_words();
    notes::reset();
    let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
    let mut copies = 0usize;
    for op in 0..PROBE_OPS {
        let copied = ctx
            .call_bif("copy_out_fixture", "bench_shaped", 0, &[])
            .expect("the fixture BIF returns");
        copies += 1;
        assert!(
            beam_term::prelude::is_boxed(copied) && !beam_term::literal::is_literal(copied),
            "op {op}: the copy is re-homed even under the ceiling (copies so far: {copies})"
        );
        if notes::snapshot().1 > 0 {
            break;
        }
    }
    let (_, grows, _) = notes::snapshot();
    assert_eq!(
        grows, 2,
        "one refused-destination copy under the ceiling makes TWO `grow_nursery` calls — the \
         headroom request (refused: {CAP_WORDS} words of ceiling leave {CAP_WORDS} - \
         {capacity_before} of budget, short of the 1024-word request) and the bare-shortfall \
         fallback (accepted)"
    );
    let capacity_after = ctx
        .process_table()
        .get(pid)
        .expect("the spawned process")
        .collector()
        .young()
        .capacity_words();
    assert!(
        capacity_after > capacity_before && capacity_after <= CAP_WORDS,
        "the fallback growth installed {capacity_after} words ({capacity_before} before, ceiling \
         {CAP_WORDS}): it must cover the copier's shortfall and stay inside the ceiling"
    );
}

/// The structural outcome of [`drive_admission_cadence`], one field per counter the pin reads.
struct Cadence {
    /// Copy-outs that ran (== the loop count).
    copies: usize,
    /// Copy attempts the copier made: `copies + refusals`, since every refusal buys exactly one
    /// retry.
    attempts: usize,
    /// Expensive first attempts: `attempts - copies`.
    refusals: usize,
    /// `Process::grow_nursery` calls that **answered** a refusal (the fallback path).
    refusal_grows: usize,
    /// `Process::grow_nursery` calls the learned-hint gate made **before** an attempt.
    hint_grows: usize,
    /// Real collections the modelled admission door ran.
    collections: usize,
}

impl Cadence {
    /// Refusals per copy-out — the figure the pin bounds.
    fn refusals_per_op(&self) -> f64 {
        self.refusals as f64 / self.copies as f64
    }
}

/// Drives `ops` copy-outs of the [`bench_shaped_result`] container against a **default-policy**
/// process while modelling the runtime's own heap-admission **cadence** between them.
///
/// The cadence is the /point/ of this helper: the synthetic long-run tests above never collect,
/// so the nursery they grow is never collapsed and they pass even when the copy-out refuses on
/// every op. Here, after each copy-out, the process's own admission predicate
/// ([`beam_process::Process::collect_if_needed`] — the OR of the occupancy test and the
/// collector's `collection_debt`, exactly what the engine's `ensure_heap`/`ensure_heap_full`
/// doors consult) runs with a payload-sized request, so the nursery the loop's own construction
/// filled is **collected** and the next copy-out meets a destination at the barely-fits boundary
/// again. That is the measured `copy_bound/ets_copy` regime: one collection per op, and the
/// destination barely large enough.
///
/// `clear_hint_every_op` zeroes the context's [`RuntimeContext::copy_out_hint`] before each copy,
/// i.e. it reproduces what the copy-out did **before** the hint existed. It is the control that
/// keeps this pin honest: the same cadence must then refuse on a large fraction of the ops.
fn drive_admission_cadence(clear_hint_every_op: bool, ops: usize) -> Cadence {
    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    {
        let registry = Rc::get_mut(&mut rt.registry)
            .expect("the runtime holds the only handle to its registry");
        let _ = registry.register("copy_out_fixture", "bench_shaped", 0, bench_shaped_result);
    }
    assert_eq!(
        table
            .get(pid)
            .expect("the spawned process")
            .collector()
            .young()
            .capacity_words(),
        beam_gc::DEFAULT_MIN_HEAP_WORDS,
        "the cadence is about the DEFAULT nursery"
    );
    // The 73-word payload is what the next admission must make room for — the same figure the
    // engine's `test_heap` carries into the door.
    const ADMISSION_NEED_WORDS: usize = 73;
    // The loop's **own** term construction between copies: it fills the destination nursery, which
    // is what puts the next copy-out at the barely-fits boundary the `copy_bound/ets_copy` case is
    // measured in — the `Err` arm below is the terminator, and the budget is only a runaway guard.
    // A fixed build cannot express the regime: the admission cadence leaves the nursery at a
    // growth/shrink equilibrium (a collection hands back a `~90`-word nursery, each growth request
    // steps it up by ~1 KiB), so the room the loop has to cover differs from op to op, and a build
    // sized for one capacity leaves the next copy a destination it fits by a wide margin.
    const PAYLOAD_BUILD_CELLS: usize = 1 << 16;
    let mut collections = 0usize;
    let mut copies = 0usize;
    {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        for op in 0..ops {
            if clear_hint_every_op {
                // Reproduce the pre-hint copy-out: nothing is ever learned, so the gate never
                // runs.
                ctx.copy_out_hint = 0;
            }
            {
                let process = ctx
                    .process_table_mut()
                    .get_mut(pid)
                    .expect("the spawned process");
                let mut built = beam_term::prelude::NIL;
                for _ in 0..PAYLOAD_BUILD_CELLS {
                    match process.heap_mut().cons(beam_term::prelude::NIL, built) {
                        Ok(list) => built = list,
                        // The nursery filled mid-construction: the engine's own admission door
                        // would have collected first, and the collection below is the same door.
                        Err(_) => break,
                    }
                }
            }
            let copied = ctx
                .call_bif("copy_out_fixture", "bench_shaped", 0, &[])
                .expect("the fixture BIF returns");
            copies += 1;
            assert!(
                beam_term::prelude::is_boxed(copied) && !beam_term::literal::is_literal(copied),
                "op {op}: the bench-shaped container is re-homed onto the process's own heap"
            );
            let collected = ctx
                .process_table_mut()
                .get_mut(pid)
                .expect("the spawned process")
                .collect_if_needed(ADMISSION_NEED_WORDS)
                .expect("the admission collection");
            if collected {
                collections += 1;
            }
        }
    }
    let (attempts, refusal_grows, empties) = notes::snapshot();
    assert_eq!(
        empties, attempts,
        "every attempt — refused or not — is handed a fully empty destination table"
    );
    assert!(
        attempts >= copies,
        "the copier cannot make fewer attempts than there are copy-outs"
    );
    Cadence {
        copies,
        attempts,
        refusals: attempts - copies,
        refusal_grows,
        hint_grows: notes::hint_grows(),
        collections,
    }
}
/// **The copy-out must not refuse against a default-policy process that really collects.**
///
/// This is the `copy_bound/ets_copy` regression pin that the synthetic no-collect loops above
/// cannot make: with the loop's own construction filling the nursery between copies and a 73-word
/// result to re-home, the destination is under the result's size often enough that the pre-hint
/// copy-out refused on ≈1 of every 3 ops (measured: 0.332 refusals/op over 250 000 ops). Every
/// refusal pays the copier's whole sizing path (`try_copy_fast` → `preflight_words` → the bounded
/// `size_object_bounded_with` walk → the acyclicity check → `size_object_with`), so the loop was
/// paying ~4.2 µs per op for nothing.
///
/// The pin: the **learned hint** (`RuntimeContext::copy_out_hint` + `hint_grow_gate`) makes the
/// first attempt fit, so refusals collapse to at most one per context (the call that teaches the
/// hint) — asserted as `refusals * 8 <= copies`, i.e. 1 in 8, well inside the measured ≤1 per
/// run. A regression that removes the gate (or stops learning the hint) puts the refusal rate
/// back to ≈1 in 3 and fails here.
#[test]
fn a_collecting_default_process_copies_every_result_without_refusing() {
    /// The cadence collects on every op, so this many copy-outs are this many modelled
    /// collections — enough for the refusal *rate* to mean something, and still a unit test.
    const OPS: usize = 1200;

    notes::reset();
    let cadence = drive_admission_cadence(false, OPS);
    assert!(
        cadence.collections * 4 > OPS,
        "the cadence must really collect ({} collections over {OPS} ops), else this pin measures \
         the no-collect regime the synthetic loops already cover",
        cadence.collections
    );
    assert!(
        cadence.hint_grows > 0,
        "the learned-hint gate must be what serves the copies (0 hint growths over {} ops)",
        cadence.copies
    );
    assert!(
        cadence.refusals * 8 <= cadence.copies,
        "the copy-out refused {} times over {} copies ({:.3} per op, {} attempts): the learned \
         hint is not pre-growing the destination, so the copy-bound loop is back to paying the \
         copier's whole sizing walk on a large fraction of its ops",
        cadence.refusals,
        cadence.copies,
        cadence.refusals_per_op(),
        cadence.attempts
    );
    assert_eq!(
        cadence.refusal_grows, cadence.refusals,
        "every refusal is answered by exactly one growth (the fallback), and no more"
    );
}

/// **The hint is what keeps the refusal rate down — the control for the pin above.**
///
/// The same cadence with [`RuntimeContext::copy_out_hint`] zeroed before every copy (i.e. the
/// pre-hint behaviour) must refuse on a large fraction of the ops: each collection hands the
/// nursery back at the collector's own retained size and the 73-word result then meets a
/// destination that is often too small. That is what the measured `0.332` refusals/op looked
/// like, and asserting it here means the pin above cannot be satisfied by anything other than the
/// hint: if the gate ever stops being the mechanism (e.g. the growth policy alone starts sizing
/// the nursery large enough to survive the collection), this control fails and the claim is
/// re-examined.
#[test]
fn without_the_learned_hint_the_same_cadence_refuses_again() {
    /// Same cadence as the pin above, so the two are directly comparable.
    const OPS: usize = 1200;

    notes::reset();
    let cadence = drive_admission_cadence(true, OPS);
    assert!(
        cadence.collections * 4 > OPS,
        "the control runs the same collecting cadence ({} collections)",
        cadence.collections
    );
    assert_eq!(
        cadence.hint_grows, 0,
        "with no hint ever learned the gate must never grow the destination"
    );
    assert!(
        cadence.refusals * 2 >= cadence.copies,
        "the pre-hint copy-out must refuse on a large fraction of the ops: only {} refusals over \
         {} copies ({:.3} per op) — the cadence no longer reproduces the copy-bound regime, so \
         the pin above asserts nothing",
        cadence.refusals,
        cadence.copies,
        cadence.refusals_per_op()
    );
    assert_eq!(
        cadence.refusal_grows, cadence.refusals,
        "each refusal is answered by exactly one growth (or two under a ceiling)"
    );
}

/// The copy-out's destination off-heap table is **fresh and empty per attempt**, so
/// `beam_message`'s copier takes its single-traversal fast path even when the runtime's
/// escaped-payload store ([`crate::runtime::Runtime::eval_off_heap`]) is already populated by an
/// unrelated nested run or transport copy.
///
/// **This test fails on the pre-fix code**: with the shared `eval_off_heap` as the copier's
/// destination, the node entry pushed below makes the table non-empty for *both* attempts, so
/// `empty_destination_attempts` reads `0` instead of the `2` asserted here — exactly the
/// permanent poisoning the fresh local table removes.
#[test]
fn the_copy_out_always_hands_the_copier_an_empty_destination_table() {
    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    // Model a runtime whose store a prior nested run / transport copy has already populated:
    // one node entry, exactly what a copy that passed an external pid would have registered.
    let node = {
        let atoms = rt.code_server_atoms_mut();
        beam_term::prelude::make_atom(atoms.intern("nonode@nohost"))
    };
    rt.eval_off_heap
        .push_node(beam_message::NodeEntry::new(node, 0));
    // A **foreign** container — a term built on a heap the process's collector does not scan —
    // that fits the default nursery by a wide margin (a 2-tuple is 3 words), held in a local so
    // it outlives both copies.
    let mut foreign = beam_alloc::Heap::new(16).expect("a local heap");
    let term = foreign
        .tuple(&[make_small(1), make_small(2)])
        .expect("the tuple fits");
    let used_before = table.get(pid).expect("the spawned process").heap().used();
    notes::reset();
    let (first, second) = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        (
            ctx.move_result_into_process_heap(term),
            ctx.move_result_into_process_heap(term),
        )
    };
    assert!(
        first.is_some() && second.is_some(),
        "both copies fit the nursery"
    );
    let (attempts, grows, empties) = notes::snapshot();
    assert_eq!(
        empties, 2,
        "each copy was handed a fully empty destination table (the fast path was eligible)"
    );
    assert_eq!(attempts, 2, "one `copy_struct` call per copy-out");
    assert_eq!(grows, 0, "a 3-word tuple never has to grow the nursery");
    let copied_words = table.get(pid).expect("the spawned process").heap().used() - used_before;
    assert_eq!(
        copied_words, 6,
        "two 3-word tuples land on the process's own heap"
    );
}

/// A **refused** copy adopts nothing into the runtime-lifetime store: a malformed-graph refusal
/// leaves [`crate::runtime::Runtime::eval_off_heap`] exactly as it was — its pre-existing entry
/// intact, no entry added — because the destination table is local and is merely dropped.
#[test]
fn a_refused_copy_adopts_nothing_into_the_runtime_store() {
    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    // A pre-existing entry models a store a prior nested run populated: the refusal must not
    // disturb it, and must add nothing of its own.
    let node = {
        let atoms = rt.code_server_atoms_mut();
        beam_term::prelude::make_atom(atoms.intern("nonode@nohost"))
    };
    rt.eval_off_heap
        .push_node(beam_message::NodeEntry::new(node, 0));
    let nodes_before = rt.eval_off_heap.nodes().len();
    let binaries_before = rt.eval_off_heap.binaries().len();
    notes::reset();
    let copy = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        // The `make_header(0, 0)` malformed graph is refused **permanently** (see
        // `a_permanent_copy_refusal_grows_no_nursery_and_writes_nothing`).
        ctx.move_result_into_process_heap(make_header(0, 0))
    };
    assert!(copy.is_none(), "the malformed graph is not re-homed");
    assert_eq!(
        rt.eval_off_heap.nodes().len(),
        nodes_before,
        "a refused copy adopts nothing (it also drops its own node references)"
    );
    assert_eq!(
        rt.eval_off_heap.binaries().len(),
        binaries_before,
        "a refused copy adopts no binary"
    );
    let (attempts, grows, _) = notes::snapshot();
    assert_eq!(attempts, 1, "the malformed graph is refused on the first attempt");
    assert_eq!(grows, 0, "a permanent refusal never grows the nursery");
}

// -----------------------------------------------------------------------
// What the copier registers, and what the copy-out therefore adopts
// -----------------------------------------------------------------------
/// The off-heap payload owner a [`beam_codec::term::make_refc_binary`] call needs — exactly the
/// role `beam_engine`'s `Machine::refc_binaries` plays for a real run.
struct Payloads(Vec<beam_alloc::RefcBinary>);
impl beam_codec::OffHeapBinaries for Payloads {
    fn alloc_refc_payload(&mut self, bytes: &[u8]) -> Result<*mut Eterm> {
        let handle = beam_alloc::RefcBinary::new(bytes)?;
        let data = handle.data_ptr();
        self.0.push(handle);
        Ok(data.cast::<Eterm>())
    }
}

/// A 200-byte payload — above [`beam_term::refc::ERL_ONHEAP_BINARY_LIMIT`], so the term built from
/// it is the frozen 8-word `[BinRef][ErlSubBits]` off-heap pair rather than an on-heap binary.
fn payload_bytes() -> Vec<u8> {
    let bytes: Vec<u8> = (0..200u32).map(|i| (i % 251) as u8).collect();
    assert!(bytes.len() > beam_term::refc::ERL_ONHEAP_BINARY_LIMIT);
    bytes
}

/// The off-heap **MSO** reference container `[HEADER_BIN_REF, slot, next]` (the frozen `BinRef`
/// thing, `beam_term::refc`) for slot `slot` on `heap`.
///
/// Written through [`beam_alloc::Heap::used_words_mut`] — the safe, initialised-prefix view — so
/// this crate's `#![deny(unsafe_code)]` holds: no production path builds one (nothing in the
/// workspace produces an MSO yet), and the raw layout owner is `beam_term::refc`, whose
/// on-heap writer is an `unsafe fn`.
fn off_heap_container(heap: &mut beam_alloc::Heap, slot: usize) -> Eterm {
    // A 2-tuple occupies exactly `ERL_BIN_REF_SIZE` words: the header plus its two elements.
    let words = beam_term::prelude::ERL_BIN_REF_SIZE;
    heap.tuple(&vec![beam_term::prelude::NIL; words - 1])
        .expect("the container's words fit");
    let base = {
        let slot_words = heap.used_words_mut();
        assert_eq!(slot_words.len(), words, "the tuple is the whole used prefix");
        slot_words[0] = beam_term::prelude::HEADER_BIN_REF;
        slot_words[1] = Eterm::from_raw(slot);
        slot_words[2] = Eterm::from_raw(0);
        slot_words.as_mut_ptr()
    };
    beam_term::prelude::make_boxed(base)
}

/// **A container holding an off-heap refc binary is copied out byte-faithfully.**
///
/// The copy's source off-heap table is empty (the pooled BIF heap owns no handle), so the copier
/// takes its documented fallback and **flattens** the refc binary's window into a fresh on-heap
/// bitstring: the result is byte-identical, self-contained, and registers **nothing** in the
/// runtime store — the copy-out succeeds where a dangling payload reference would have been the
/// alternative.
#[test]
fn a_container_holding_an_off_heap_refc_binary_is_copied_out_as_an_on_heap_binary() {
    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    // The BIF result: a 2-tuple whose first element is an off-heap refc binary — the shape
    // `bs_create_bin`/`base64:encode/1` hands back above the on-heap limit.
    let bytes = payload_bytes();
    let mut source = beam_alloc::Heap::new(64).expect("the source heap");
    let mut payloads = Payloads(Vec::new());
    let binary = beam_codec::term::make_refc_binary(
        &mut source,
        &mut payloads,
        &bytes,
        0,
        bytes.len() * 8,
    )
    .expect("the source refc binary");
    let term = source
        .tuple(&[binary, make_small(1)])
        .expect("the source tuple");
    // What the flattened copy must read back as: the same bytes as an on-heap binary.
    let mut reference_heap = beam_alloc::Heap::new(64).expect("the reference heap");
    let reference_binary =
        beam_codec::term::make_binary(&mut reference_heap, &bytes).expect("the reference binary");
    let reference = reference_heap
        .tuple(&[reference_binary, make_small(1)])
        .expect("the reference tuple");

    let used_before = table
        .get(pid)
        .expect("the spawned process")
        .heap()
        .used();
    notes::reset();
    let copied = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.move_result_into_process_heap(term)
            .expect("the container is re-homed")
    };
    assert!(
        beam_term::prelude::is_boxed(copied) && !beam_term::literal::is_literal(copied),
        "the result is a real tuple built on the process's own heap"
    );
    assert!(
        beam_term::compare::eq(copied, reference, rt.atoms()),
        "the copied binary reads back byte-identical to the payload it was flattened from"
    );
    assert!(
        rt.eval_off_heap.binaries().is_empty(),
        "a flattened payload registers no off-heap handle in the runtime store"
    );
    let copied_words = table.get(pid).expect("the spawned process").heap().used() - used_before;
    assert_eq!(
        copied_words,
        beam_term::size_object(copied),
        "the whole copied graph — the flattened bitstring included — lives on the process heap"
    );
    let (attempts, grows, empties) = notes::snapshot();
    assert_eq!(
        (attempts, grows, empties),
        (1, 0, 1),
        "one attempt, on the empty-destination fast path, with nothing to grow"
    );
}

/// **An off-heap reference with no resolvable owner is refused, never copied into a dangling one.**
///
/// The copy's source table is empty, so an off-heap MSO container's slot does not resolve
/// (`clone_slot` is `None`) and the copier answers its typed `bad_term` — the pool-owning caller
/// then gets `None` (so it neither rewinds the pool nor adopts anything) rather than a term whose
/// `val` word names a slot the runtime store does not hold.
///
/// The refusal has **no root**: the copier's sized fall-back path may already have materialised the
/// container's ancestors (the two words of the tuple below) before it reached the unresolvable
/// slot, and those words are unreachable — nothing in the process's root set points at them — so
/// the invariant asserted here is that no *result* exists, never that the bump pointer is where it
/// was (only the exhaustion preflight, and a refusal of a root that is itself malformed, promise
/// that; see `a_permanent_copy_refusal_grows_no_nursery_and_writes_nothing`).
#[test]
fn an_off_heap_reference_with_no_owner_is_refused_and_adopts_nothing() {
    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    let mut source = beam_alloc::Heap::new(64).expect("the source heap");
    let container = off_heap_container(&mut source, 0);
    let term = source
        .tuple(&[container, make_small(1)])
        .expect("the source tuple");
    notes::reset();
    let copy = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.move_result_into_process_heap(term)
    };
    assert!(
        copy.is_none(),
        "the unresolvable container is refused (never re-homed with a dangling slot)"
    );
    assert!(
        rt.eval_off_heap.nodes().is_empty() && rt.eval_off_heap.binaries().is_empty(),
        "a refused copy adopts nothing into the runtime store"
    );
    let (attempts, grows, empties) = notes::snapshot();
    assert_eq!(
        (attempts, grows, empties),
        (1, 0, 1),
        "one attempt, refused on the empty-destination fast path, with nothing to grow"
    );
}

/// A registered BIF whose result is a **container the copier refuses** (`make_header(0, 0)` is the
/// C ABI's non-value, a bare header word), after allocating words on the pooled heap.
fn refused_result(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let _pooled = ctx.heap().tuple(&[make_small(1), make_small(2)])?;
    Ok(make_header(0, 0))
}

/// A registered BIF that allocates on the pooled heap and returns a **copyable** container.
fn copyable_result(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let _pooled = ctx.heap().tuple(&[make_small(1), make_small(2)])?;
    ctx.heap().tuple(&[make_small(9)])
}

/// **The pool rewind is a decision of its own, and a refused copy-out leaves the pool alone.**
///
/// `call_registered` rewinds the pooled BIF heap to this call's `mark` **only** when the copy-out
/// reported a container result it could re-home. A refused result is returned to the caller
/// **as it was built** — still on the pool — so rewinding would recycle the very words the result
/// points at. The positive control is the same call shape with a copyable result, whose words
/// *are* given back (to its own `mark`, which is exactly where the refused call left the pool).
#[test]
fn a_refused_copy_out_leaves_the_pooled_bif_heap_unrewound() {
    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    {
        let registry = Rc::get_mut(&mut rt.registry)
            .expect("the runtime holds the only handle to its registry");
        let _ = registry.register("copy_out_fixture", "refused", 0, refused_result);
        let _ = registry.register("copy_out_fixture", "copied", 0, copyable_result);
    }
    let pool_used = |rt: &crate::runtime::Runtime| {
        rt.bif_heaps
            .last()
            .expect("the pool keeps its region")
            .used()
    };
    assert_eq!(pool_used(&rt), 0, "the pool starts clean");
    // The refused result: the BIF's own three words are on the pooled heap when the call returns
    // them, so the pool's bump pointer must still be where the call left it.
    let pool_after_refusal = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        notes::reset();
        let result = ctx
            .call_bif("copy_out_fixture", "refused", 0, &[])
            .expect("the fixture BIF returns");
        assert_eq!(
            result.raw(),
            make_header(0, 0).raw(),
            "the un-re-homed result is handed to the caller unchanged"
        );
        assert_eq!(
            notes::snapshot(),
            (1, 0, 1),
            "the copy-out did run — one attempt, on the empty-destination fast path — and refused"
        );
        pool_used(ctx.runtime)
    };
    assert_eq!(
        pool_after_refusal, 3,
        "a refused copy-out must not rewind: the result still points into those words"
    );
    // The positive control: the same call shape, a result the copier can re-home — the call's own
    // words are given back, leaving the pool exactly where the previous call left it.
    let pool_after_copy = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        notes::reset();
        let result = ctx
            .call_bif("copy_out_fixture", "copied", 0, &[])
            .expect("the fixture BIF returns");
        assert!(
            beam_term::prelude::is_boxed(result) && !beam_term::literal::is_literal(result),
            "the copyable result is a real container on the process heap"
        );
        assert_eq!(
            notes::snapshot(),
            (1, 0, 1),
            "the copy-out ran once on the empty-destination fast path"
        );
        pool_used(ctx.runtime)
    };
    assert_eq!(
        pool_after_copy, pool_after_refusal,
        "a successful copy-out rewinds the pool to its own mark"
    );
}

/// The frozen M1 internal-reference block (`[make_header(3, REF_SUBTAG), 0, 0, id]` — exactly what
/// [`beam_term::boxed::write_internal_ref`] writes) for `id`, through the safe initialised-prefix
/// view ([`beam_alloc::Heap::used_words_mut`]) the way [`off_heap_container`] writes its header, so
/// this crate's `#![deny(unsafe_code)]` holds.
fn internal_reference(heap: &mut beam_alloc::Heap, id: i64) -> Eterm {
    let words = beam_term::prelude::INTERNAL_REF_WORDS;
    heap.tuple(&vec![beam_term::prelude::NIL; words - 1])
        .expect("the reference's words fit");
    let base = {
        let block = heap.used_words_mut();
        assert_eq!(
            block.len(),
            words,
            "the reference block is the whole used prefix"
        );
        block[0] = beam_term::prelude::make_header(words - 1, beam_term::prelude::REF_SUBTAG);
        block[1] = make_small(0); // node: the local node
        block[2] = make_small(0); // serial: one id space in M1
        block[3] = make_small(id); // id
        block.as_mut_ptr()
    };
    beam_term::prelude::make_boxed(base)
}

/// **The copy-out's reference walk never dereferences a word that is not a readable term.**
///
/// The frozen tag predicates are the C's own bit tricks: `is_boxed` is `(x & 0x1) == 0` and
/// `is_list` is `(x & 0x2) == 0`, so **both** accept every `HEADER`-tagged word — the words that
/// are not terms at all. The C ABI's `THE_NON_VALUE` (`make_header(0, 0)`) is one, and its pointer
/// is `NULL`, so an unguarded walk aborts the whole test binary with `null pointer dereference
/// occurred`; [`refused_result`] above is a registered BIF that returns exactly that word. Every
/// other unreadable word is skipped the same way: a bare header naming a non-zero aligned
/// "address", an immediate, `NIL`, and a `LIST`-tagged word whose cell pointer is `NULL`.
///
/// The positive controls keep the reference finding itself honest: the walk still **finds** a
/// reference at the root, one level down a tuple, in a cons cell's car and through a nested
/// container, so the guards above reject only words that are not readable terms.
#[test]
fn the_reference_walk_skips_words_that_are_not_readable_terms() {
    let mut heap = beam_alloc::Heap::new(64).expect("the term heap");
    // `THE_NON_VALUE`: not a term word, yet both frozen container tricks accept it.
    let the_non_value = make_header(0, 0);
    assert!(
        beam_term::prelude::is_boxed(the_non_value) && beam_term::prelude::is_list(the_non_value),
        "the frozen bit tricks cannot tell the non-value from a container"
    );
    assert!(!beam_term::tag::is_term_word(the_non_value));
    assert!(
        !RuntimeContext::graph_holds_reference(the_non_value),
        "the non-value is skipped, never dereferenced"
    );
    // A bare header word: `HEADER`-tagged, so `boxed_val` names a plausible non-zero, aligned
    // address that belongs to nothing.
    assert!(
        !RuntimeContext::graph_holds_reference(make_header(3, 0)),
        "a bare header word is skipped, never dereferenced"
    );
    // A `LIST`-tagged word whose cell pointer is `NULL`.
    assert_eq!(beam_term::list::ptr_val(Eterm::from_raw(1)) as usize, 0);
    assert!(
        !RuntimeContext::graph_holds_reference(Eterm::from_raw(1)),
        "a list word with a NULL cell pointer is skipped, never dereferenced"
    );
    // An immediate and `NIL` are neither boxed nor cons cells.
    assert!(!RuntimeContext::graph_holds_reference(make_small(7)));
    assert!(!RuntimeContext::graph_holds_reference(
        beam_term::prelude::NIL
    ));
    // A real container of immediates holds no reference at all …
    let tuple = heap
        .tuple(&[make_small(1), make_small(2)])
        .expect("a 2-tuple of immediates");
    assert!(!RuntimeContext::graph_holds_reference(tuple));
    // … and a non-term word *inside* a real container is skipped when the walk reaches it.
    let cons = heap
        .cons(make_small(1), the_non_value)
        .expect("a cons cell over the non-value");
    assert!(
        !RuntimeContext::graph_holds_reference(cons),
        "an unreadable child contributes no reference finding"
    );
    // The positive controls: the findings the walk exists for are unchanged.
    let mut ref_heap = beam_alloc::Heap::new(64).expect("the reference heap");
    let reference = internal_reference(&mut ref_heap, 7);
    assert!(
        RuntimeContext::graph_holds_reference(reference),
        "the reference block itself is a reference"
    );
    let tuple_over_ref = ref_heap
        .tuple(&[make_small(4), reference])
        .expect("a tuple over the reference");
    assert!(
        RuntimeContext::graph_holds_reference(tuple_over_ref),
        "a reference one level down a tuple is found"
    );
    let cons_over_ref = ref_heap
        .cons(reference, beam_term::prelude::NIL)
        .expect("a cons over the reference");
    assert!(
        RuntimeContext::graph_holds_reference(cons_over_ref),
        "a reference in a cons cell's car is found"
    );
    let nested = ref_heap
        .tuple(&[tuple_over_ref])
        .expect("a tuple over the reference-holding tuple");
    assert!(
        RuntimeContext::graph_holds_reference(nested),
        "a reference found through a nested container is found"
    );
}

// ---------------------------------------------------------------------------------------
// The reference walk's cost, pinned by deterministic counters (never a wall time)
// ---------------------------------------------------------------------------------------

/// The `copy_bound` benchmark's payload — the 5-key map `payload/0` builds in
/// `tools/bench/src/workloads/copy_bound.rs`, reconstructed here from that file's constants: a
/// 10-element list, `{ok, <<"hello world">>, 3.5}`, a 12-byte binary, `"a char list"` and `-42`.
///
/// An 11- and a 12-byte binary both stay **on** the heap
/// ([`beam_term::refc::ERL_ONHEAP_BINARY_LIMIT`] = 64 bytes), so the graph is exactly the
/// `etf_roundtrip` case's copy boundary: 27 dereferenceable objects.
fn bench_payload(
    ctx: &mut RuntimeContext<'_>,
    keys: [Eterm; 5],
    ok: Eterm,
    atoms: &beam_term::AtomTable,
) -> Result<Eterm> {
    let cells: Vec<Eterm> = (1..=10).map(make_small).collect();
    let list = beam_engine::terms::make_list(ctx, &cells)?;
    let hello = beam_engine::terms::make_heap_binary(ctx, b"hello world")?;
    let float = beam_engine::terms::make_float(ctx, 3.5)?;
    let tuple = beam_engine::terms::make_tuple(ctx, &[ok, hello, float])?;
    let bytes: Vec<u8> = (1..=12).collect();
    let binary = beam_engine::terms::make_heap_binary(ctx, &bytes)?;
    let chars: Vec<Eterm> = "a char list".chars().map(|c| make_small(c as i64)).collect();
    let chunk = beam_engine::terms::make_list(ctx, &chars)?;
    let pairs = [
        (keys[0], list),
        (keys[1], tuple),
        (keys[2], binary),
        (keys[3], chunk),
        (keys[4], make_small(-42)),
    ];
    beam_engine::terms::make_map(ctx, &pairs, atoms)
}

/// **One registered-BIF container result costs one walk call, 27 visits, no hashing, no spill.**
///
/// The graph walked is the one `binary_to_term/1` **rebuilds** on the pooled BIF heap from
/// `term_to_binary/1`'s image — the `etf_roundtrip` benchmark case's own round-trip — so the
/// counted objects are the ones the copy boundary really moves: the flat-map block, the key tuple
/// the flat map wraps its five keys in, the ten cons cells of `[1..10]`, the `{ok, …}` tuple, its
/// on-heap binary and its float, the 12-byte binary, and the eleven cons cells of `"a char list"`
/// (`1 + 1 + 10 + 1 + 1 + 1 + 1 + 11 = 27`).
///
/// The three bookkeeping counters are the point of the test: the walk must dereference each object
/// once (`visits`), must do **no** hashed bookkeeping (`hash_inserts == 0` — the walk keeps no
/// visited set) and must never leave its inline worklist (`spills == 0`), so an ordinary BIF call
/// allocates nothing for it.
#[test]
fn the_reference_walk_visits_a_bench_shaped_payload_without_hashing_or_spilling() {
    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    let (mut table, pid) = process_table(&mut rt);
    let keys = ["a", "b", "c", "d", "e"].map(|name| rt.intern(name));
    let ok = rt.intern("ok");
    let atoms = rt.code_server().atoms_handle();
    let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
    let payload = bench_payload(&mut ctx, keys, ok, &atoms).expect("the bench payload");
    let image = ctx
        .call_bif("erlang", "term_to_binary", 1, &[payload])
        .expect("term_to_binary/1 of the payload");
    // Only the `binary_to_term/1` call is measured: its result is the container the copy-out
    // re-homes, and therefore the graph the walk sees.
    notes::reset();
    let rebuilt = ctx
        .call_bif("erlang", "binary_to_term", 1, &[image])
        .expect("binary_to_term/1 of the image");
    let counts = notes::walk_snapshot();
    assert_eq!(
        counts,
        notes::WalkCounts {
            calls: 1,
            visits: 27,
            hash_inserts: 0,
            spills: 0
        },
        "one container result: one walk, one visit per object, hash-free, spill-free"
    );
    // The walk saw the real payload: re-encoding the rebuilt graph reproduces the image byte for
    // byte, i.e. every object the walk visited is the payload's own object.
    let re_encoded = ctx
        .call_bif("erlang", "term_to_binary", 1, &[rebuilt])
        .expect("term_to_binary/1 of the rebuilt payload");
    assert_eq!(
        beam_engine::terms::binary_bytes(re_encoded).expect("a binary"),
        beam_engine::terms::binary_bytes(image).expect("a binary"),
        "the round-trip rebuilt the payload unchanged"
    );
}
// ---------------------------------------------------------------------------------------
// The table-less (`-eval`) route's pooled BIF heap — counters first.
//
// The rewind above needs a destination for a container result. With no process
// (`RuntimeContext::new`, i.e. `-eval`/`-s`/`-run`/the single-pass `run_*` entries) the re-home
// arm is `move_result_into_runtime_store` instead, so the result leaves the pool for the runtime's
// long-lived store and the pool is rewound to the call's own mark. These helpers read the pool's
// state and the process's minor-page-fault counter, so the cost is *measured* (never estimated)
// and the fix is pinned by counters, not by a wall time. Run the whole sweep with
// `--ignored --exact context::calls::tests::eval_route_pool_costs`; the measured table is in
// `../IMPLEMENTATION.md#the-table-less-arm-and-its-store`.
// ---------------------------------------------------------------------------------------

/// Every pooled BIF-heap region as `(used, peak_used, capacity)` words, in pool order.
fn pool_snapshot(rt: &crate::runtime::Runtime) -> Vec<(usize, usize, usize)> {
    rt.bif_heaps
        .iter()
        .map(|heap| (heap.used(), heap.peak_used(), heap.capacity()))
        .collect()
}

/// [`pool_snapshot`] in a whitespace-free wire form (`used/peak/capacity` per region, `;`-joined),
/// so the child's single printed line can be split on spaces.
fn compact_regions(rt: &crate::runtime::Runtime) -> String {
    pool_snapshot(rt)
        .iter()
        .map(|(used, peak, capacity)| format!("{used}/{peak}/{capacity}"))
        .collect::<Vec<_>>()
        .join(";")
}

/// The **cumulative high-water** of the pool: the words every region has ever handed out.
///
/// `peak_used` survives a [`beam_alloc::Heap::reset_to`] (it is a property of the live-word
/// history, not of the current offset), so this is exactly "how many pool words have been
/// first-touched", and it is deterministic — no timing, no counter that depends on the host.
fn pool_words_touched(rt: &crate::runtime::Runtime) -> usize {
    rt.bif_heaps.iter().map(|heap| heap.peak_used()).sum()
}

/// This process's minor-page-fault count (`/proc/self/stat` field 10), or `None` where the file
/// does not exist (non-Linux).
fn minflt() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/stat").ok()?;
    // `comm` (field 2) is parenthesized and may itself contain spaces, so the tail is taken
    // after the LAST `)`: it begins at field 3 (`state`), so minflt (field 10) is index 7.
    let tail: Vec<&str> = text.get(text.rfind(')')? + 1..)?.split_whitespace().collect();
    tail.get(7)?.parse().ok()
}

/// One measured row: `N` calls of `body`'s comprehension on the **table-less** route.
struct Row {
    /// Calls (= comprehension elements) in the row.
    calls: usize,
    /// Minor faults the whole child process took (`None` where `/proc/self/stat` is unavailable).
    faults: Option<u64>,
    /// The child's total wall time for the measured expression, in nanoseconds, divided by
    /// `calls` — reported only as a **ratio** between two builds on the same host.
    ns_per_call: f64,
    /// The pool's cumulative high-water ([`pool_words_touched`]) in the child.
    pool_words: usize,
    /// The committed `eval_heap` words in the child.
    store_words: usize,
    /// `pool_snapshot` in the child.
    regions: Vec<(usize, usize, usize)>,
    /// The child's `/proc/loadavg` after the measured expression.
    loadavg: String,
}

impl Row {
    /// This row's minor faults per call (never a wall time).
    fn faults_per_call(&self) -> Option<f64> {
        self.faults.map(|faults| faults as f64 / self.calls as f64)
    }
}

/// The workloads: `(label, body)` — the comprehension body the child evaluates `N` times.
const POOL_WORKLOADS: [(&str, &str); 3] = [
    // The control: an **immediate** result already rewinds the pool on every call.
    ("immediate", "element(1, {I})"),
    // A **container** result: the case the store arm exists for — it leaves the pool for
    // `eval_heap` and the pool is rewound to the call's mark.
    ("container", "list_to_tuple([I])"),
    // A container result with a large temporary: the bulk shape, where the pool's marching
    // dominates the call.
    ("bulk", "list_to_tuple(lists:seq(1,20))"),
];

/// The env var that turns this test binary into a **single-row child**.
const POOL_CHILD_ENV: &str = "BEAM_RT_EVAL_POOL_CHILD";

/// `body:calls` → `(body, calls)`.
fn parse_child_spec(spec: &str) -> (String, usize) {
    let (body, calls) = spec.rsplit_once(':').expect("a `<body>:<calls>` spec");
    (body.to_string(), calls.parse().expect("a decimal call count"))
}

/// The **child** half: one fresh process, one measured expression, one printed row.
///
/// A fresh OS process is what makes the fault count mean anything: a pooled region that a
/// previous row already wrote is fully resident, so a same-process measurement would report the
/// *carry-over* rather than the first-touch cost. Every page this child touches is cold.
fn pool_child_row() -> String {
    let spec = std::env::var(POOL_CHILD_ENV).expect("the child spec");
    let (body, calls) = parse_child_spec(&spec);
    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    // Warm-up: pre-faults the runtime's own one-time regions (boot, the code server, the
    // evaluator's own first allocations) so the measured window is the workload's own cost.
    rt.eval(&format!("[{body} || I <- lists:seq(1,64)]"))
        .expect("the warm-up expression evaluates");
    let expr = format!("[{body} || I <- lists:seq(1,{calls})]");
    let before = minflt();
    let started = std::time::Instant::now();
    rt.eval(&expr).expect("the measured expression evaluates");
    let elapsed = started.elapsed();
    let after = minflt();
    format!(
        "calls={calls} faults={} ns={} pool_words={} store_words={} regions={} loadavg={}",
        before
            .zip(after)
            .map(|(before, after)| (after - before).to_string())
            .unwrap_or_else(|| "-".to_string()),
        elapsed.as_nanos(),
        pool_words_touched(&rt),
        rt.eval_heap.used(),
        compact_regions(&rt),
        std::fs::read_to_string("/proc/loadavg")
            .map(|load| load.trim().replace(' ', "|"))
            .unwrap_or_else(|_| "unavailable".to_string()),
    )
}

/// Runs the child for `(body, calls)` and parses its row.
fn run_pool_child(body: &str, calls: usize) -> Row {
    let output = std::process::Command::new(std::env::current_exe().expect("the test binary"))
        .args([
            "--ignored",
            "--exact",
            "--nocapture",
            "--test-threads=1",
            "context::calls::tests::eval_route_pool_costs",
        ])
        .env(POOL_CHILD_ENV, format!("{body}:{calls}"))
        .output()
        .expect("the child test process runs");
    assert!(
        output.status.success(),
        "the child row failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The harness prints the test header and the row on the same line (`test … ... calls=…`),
    // so the row is located by its first key rather than by a line start.
    let line = stdout
        .lines()
        .find_map(|line| line.find("calls=").map(|at| &line[at..]))
        .unwrap_or_else(|| panic!("the child printed no row:\n{stdout}"));
    let mut row = Row {
        calls,
        faults: None,
        ns_per_call: 0.0,
        pool_words: 0,
        store_words: 0,
        regions: Vec::new(),
        loadavg: String::new(),
    };
    for field in line.split_whitespace() {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        match key {
            "faults" => row.faults = value.parse().ok(),
            "ns" => {
                let total: f64 = value.parse().expect("a decimal nanosecond count");
                row.ns_per_call = total / calls as f64;
            }
            "pool_words" => row.pool_words = value.parse().expect("a decimal word count"),
            "store_words" => row.store_words = value.parse().expect("a decimal word count"),
            "regions" => {
                row.regions = value
                    .split(';')
                    .filter(|region| !region.is_empty())
                    .map(|region| {
                        let mut words = region.split('/').map(|word| {
                            word.parse::<usize>().expect("a decimal word count")
                        });
                        (
                            words.next().expect("used"),
                            words.next().expect("peak"),
                            words.next().expect("capacity"),
                        )
                    })
                    .collect();
            }
            "loadavg" => row.loadavg = value.replace('|', " "),
            _ => {}
        }
    }
    row
}

/// Prints the measured rows as a table (measurement harness, not an assertion).
fn report(label: &str, rows: &[Row]) {
    for row in rows {
        println!(
            "[{label}] N={:<7} faults={:<7} faults/call={:<8} ns/call={:<9} pool_words={:<8} store_words={:<8} regions={:?} loadavg={}",
            row.calls,
            match row.faults {
                Some(faults) => faults.to_string(),
                None => "-".to_string(),
            },
            match row.faults_per_call() {
                Some(ratio) => format!("{ratio:.5}"),
                None => "-".to_string(),
            },
            format!("{:.1}", row.ns_per_call),
            row.pool_words,
            row.store_words,
            row.regions,
            row.loadavg,
        );
    }
}

/// **The measurement harness** for the table-less route's pooled BIF heap: minor faults per call,
/// the pool's cumulative high-water and (as a *ratio* only) the per-call wall time, over three
/// `N` so the per-call slope separates from the runtime's constant, for an immediate-result
/// control and two container-result workloads.
///
/// Every row runs in a **fresh child process** (`--ignored --exact` on this very test with
/// [`POOL_CHILD_ENV`] set), because a pooled region written by an earlier row is already
/// resident and would hide the first-touch cost this harness exists to measure.
///
/// ```text
/// taskset -c 3 erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt \
///     --lib eval_route_pool_costs -- --ignored --nocapture --test-threads=1
/// ```
#[test]
#[ignore = "measurement harness (fresh child process per row, reads /proc/self/stat)"]
fn eval_route_pool_costs() {
    if std::env::var(POOL_CHILD_ENV).is_ok() {
        // Child: print one row and finish (the harness's unit of measurement).
        println!("{}", pool_child_row());
        return;
    }
    const NS: [usize; 3] = [25_000, 50_000, 100_000];
    for (label, body) in POOL_WORKLOADS {
        let rows: Vec<Row> = NS.iter().map(|n| run_pool_child(body, *n)).collect();
        report(label, &rows);
    }
}

// ---------------------------------------------------------------------------------------
// The reference walk's `move_container` gate — the walk runs only where it is observable
// ---------------------------------------------------------------------------------------
//
// `RuntimeContext::call_registered`'s tail walks the result graph for a reference **only** when
// `move_container` is true (a process route, or a table-less call no escaping arm can have leaked
// a heap word from). When `move_container == false` the walk's answer is unobservable: its
// `reference` arm would only set `rewind = false`, which the gate already implies, and the copy
// arm — the walk's second consumer — is gated on the same flag. The suite below pins both sides of
// that gate plus the exactness of the hoisted visit counter.

thread_local! {
    /// The term the fixture BIF built on the pooled heap, so a test can assert the result came back
    /// **word-identical** (a copy would be a different address — and for a reference inside it, a
    /// different term).
    static FIXTURE_BUILT: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

/// A registered BIF that allocates on the pooled heap and returns `{Ref}` — a container holding an
/// **internal reference**, the one shape the copy-out's reference walk exists for.
fn reference_holding_result(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let reference = internal_reference(ctx.heap(), 4242);
    let container = ctx.heap().tuple(&[reference])?;
    FIXTURE_BUILT.with(|c| c.set(container.raw()));
    Ok(container)
}

/// A registered BIF whose result is the **accumulator shape**: a cons spine of immediates ending in
/// an internal reference (what a `lists:foldl/3` accumulator looks like to the walk — the measured
/// 236.5 M + 76.1 M of the 408.5 M visits a `lib/stdlib/src/string.erl` compile used to pay).
fn accumulator_result(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let reference = internal_reference(ctx.heap(), 777);
    let mut list = reference;
    for index in (0..ACCUMULATOR_CELLS).rev() {
        list = ctx.heap().cons(make_small(index), list)?;
    }
    FIXTURE_BUILT.with(|c| c.set(list.raw()));
    Ok(list)
}

/// The accumulator's spine length (long enough that a whole-AST walk would be visible, short enough
/// to stay a unit test).
const ACCUMULATOR_CELLS: i64 = 64;

/// A fun term built on the caller's `heap`, used purely as a **reentrancy witness**: the dispatch
/// prologue's `reentrancy_capable` asks only whether some argument is a fun, which is what makes a
/// real `lists:foldl/3` call reentrant. The raw atom indices are placeholders because nothing here
/// applies the fun.
///
/// The heap is passed in rather than built here on purpose: [`beam_engine::terms::is_fun`] reads
/// the term's header word, so the fun must still have a live region when the call runs (a heap
/// dropped at this function's exit would leave the header read pointing at recycled memory and the
/// call would look *non*-reentrant).
fn reentrancy_witness(heap: &mut beam_alloc::Heap) -> Eterm {
    beam_codec::term::make_fun_external(
        heap,
        1,
        beam_term::Atom::from_raw(1),
        beam_term::Atom::from_raw(2),
    )
    .expect("the external fun builds")
}

/// The runtime a table-less fixture call runs against, with the two fixture BIFs registered.
fn fixture_runtime() -> crate::runtime::Runtime {
    let mut rt = crate::runtime::Runtime::new(crate::config::RuntimeConfig::default())
        .expect("the default runtime");
    {
        let registry = Rc::get_mut(&mut rt.registry)
            .expect("the runtime holds the only handle to its registry");
        let _ = registry.register(
            "copy_out_fixture",
            "reference_holding",
            2,
            reference_holding_result,
        );
        let _ = registry.register("copy_out_fixture", "accumulator", 2, accumulator_result);
    }
    rt
}

/// **A table-less reentrant call never walks its result.**
///
/// `RuntimeContext::new` (the `-eval`/`-s`/`-run` route and the in-process `compile:file/2` route of
/// contract A.9) has `mailbox_table == None`, and a **reentrant** call — any call carrying a fun
/// argument, i.e. every `lists:foldl/3`/`lists:map/2`/`maps:map/2` accumulator — is one conjunct of
/// `!safe_to_rewind`, so `move_container` is false and the walk is unobservable. Three things are
/// pinned: the walk is **not invoked** (its call counter does not move), the result is handed back
/// **word-identical** to what the BIF built on the pool (a copy would be a different address, and a
/// copied reference is a different term), and the pool is **not rewound** (its words are live).
#[test]
fn a_table_less_reentrant_call_never_walks_its_result() {
    let mut rt = fixture_runtime();
    let mut witness = beam_alloc::Heap::new(16).expect("a scratch heap for the reentrancy witness");
    let fun = reentrancy_witness(&mut witness);
    FIXTURE_BUILT.with(|c| c.set(0));
    let built = {
        let mut ctx = RuntimeContext::new(&mut rt);
        notes::reset();
        let result = ctx
            .call_bif("copy_out_fixture", "reference_holding", 2, &[fun, make_small(0)])
            .expect("the fixture BIF returns");
        assert_eq!(
            notes::walk_snapshot().calls,
            0,
            "a table-less reentrant call must not walk its result: the answer is unobservable"
        );
        assert_eq!(
            notes::walk_snapshot().visits,
            0,
            "the walk's whole visit count stays at zero for the gated path"
        );
        assert!(
            rt_note(&notes::snapshot()),
            "the copy-out must not even be attempted on this route"
        );
        result.raw()
    };
    assert_eq!(
        built,
        FIXTURE_BUILT.with(core::cell::Cell::get),
        "the result is word-identical to the term the BIF built — nothing was copied"
    );
    assert_eq!(
        rt.bif_heaps.last().expect("the pool keeps its region").used() > 0,
        true,
        "the pool must NOT be rewound: the result still points into those words"
    );
}

/// The no-copy-out half of the assertion above, spelled out (`snapshot()`'s three-tuple is
/// `(copy_attempts, grow_retries, empty_destination_attempts)`).
fn rt_note(snapshot: &(usize, usize, usize)) -> bool {
    *snapshot == (0, 0, 0)
}

/// **The same result on a process route still walks, still finds the reference, and still declines
/// the move.**
///
/// `RuntimeContext::for_process` supplies a `mailbox_table`, so `move_container` is true whatever
/// `safe_to_rewind` answers: this is the case the gate must leave **exactly** as it was. The walk
/// runs, answers "holds a reference", and the result is returned word-identical (not copied onto
/// the process heap) with the pool un-rewound — a copied reference would stop being the term it
/// names.
#[test]
fn a_process_route_still_walks_its_result_and_still_declines_the_move() {
    let mut rt = fixture_runtime();
    let (mut table, pid) = process_table(&mut rt);
    let mut witness = beam_alloc::Heap::new(16).expect("a scratch heap for the reentrancy witness");
    let fun = reentrancy_witness(&mut witness);
    FIXTURE_BUILT.with(|c| c.set(0));
    let built = {
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        notes::reset();
        let result = ctx
            .call_bif("copy_out_fixture", "reference_holding", 2, &[fun, make_small(0)])
            .expect("the fixture BIF returns");
        assert_eq!(
            notes::walk_snapshot().calls,
            1,
            "a process route moves its container result, so the walk's answer is observable — it runs"
        );
        assert_eq!(
            notes::walk_snapshot().visits,
            2,
            "the `{{Ref}}` container is two dereferenced objects: the tuple, then the reference block"
        );
        result.raw()
    };
    assert_eq!(
        built,
        FIXTURE_BUILT.with(core::cell::Cell::get),
        "a reference-holding result is NOT copied — the walk's answer kept the term where it was"
    );
    assert!(
        rt.bif_heaps.last().expect("the pool keeps its region").used() > 0,
        "and the pool is not rewound, exactly as before the gate"
    );
}

/// **The accumulator shape through the gate: a table-less reentrant call whose result is a long
/// cons spine ending in a reference.**
///
/// The shape pinned here is a spine of 64 immediate cons cells with an internal reference in the
/// tail — what `lists:foldl/3` hands back to `call_registered` when the fold's accumulator is the
/// intermediate compiler AST (measured: `lists:foldl/3` 236.5 M + `lists:map/2` 76.1 M of
/// `string.erl`'s 408.5 M walk visits). The real bridge is not driven because the walk's gate reads
/// only `reentrancy_capable(args)` — "some argument is a fun" — which the fixture supplies; an
/// applied fun would additionally need a compiled, resolvable closure, and the property under test
/// does not depend on what the fun does. The call still returns the fold's value, cell for cell.
#[test]
fn a_table_less_reentrant_accumulator_is_returned_unwalked() {
    let mut rt = fixture_runtime();
    let mut witness = beam_alloc::Heap::new(16).expect("a scratch heap for the reentrancy witness");
    let fun = reentrancy_witness(&mut witness);
    FIXTURE_BUILT.with(|c| c.set(0));
    let built = {
        let mut ctx = RuntimeContext::new(&mut rt);
        notes::reset();
        let result = ctx
            .call_bif("copy_out_fixture", "accumulator", 2, &[fun, make_small(0)])
            .expect("the fixture BIF returns");
        assert_eq!(
            notes::walk_snapshot(),
            notes::WalkCounts {
                calls: 0,
                visits: 0,
                hash_inserts: 0,
                spills: 0,
            },
            "a 64-cell accumulator is handed back without a single visit on this route"
        );
        result.raw()
    };
    assert_eq!(
        built,
        FIXTURE_BUILT.with(core::cell::Cell::get),
        "the accumulator comes back as the fold left it"
    );
    assert_eq!(
        rt.bif_heaps.last().expect("the pool keeps its region").used() > 0,
        true,
        "and its spine stays live on the pool (no rewind)"
    );
}

/// **The hoisted visit counter is exact on every exit.**
///
/// The walk accumulates its visits in a local `u64` and folds them in once, at its single exit
/// (two early answers `break 'walk` into it). This pins that the folded total equals the per-visit
/// counter for both shapes — the ordinary terminal exit (a spine of immediates, 8 cells) and the
/// reference-header early exit (`{Ref}`, 2 objects) — so the hoist cannot silently under-count.
#[test]
fn the_folded_visit_total_matches_the_per_visit_counter_on_every_exit() {
    let mut heap = beam_alloc::Heap::new(256).expect("the term heap");
    let mut list = beam_term::prelude::NIL;
    for index in (0..8i64).rev() {
        list = heap.cons(make_small(index), list).expect("a cell fits");
    }
    notes::reset();
    assert!(!RuntimeContext::graph_holds_reference(list));
    let visits = notes::walk_snapshot().visits;
    assert_eq!(visits, 8, "one visit per cons cell, and `NIL` is never pushed");
    assert_eq!(
        notes::walk_fold_snapshot(),
        (1, visits),
        "the terminal exit folds the local counter exactly once"
    );

    // The reference-header exit needs a **fresh** heap: `internal_reference` asserts the block is
    // the heap's whole used prefix (it writes the header over `used_words_mut`).
    let mut heap = beam_alloc::Heap::new(256).expect("the term heap");
    let reference = internal_reference(&mut heap, 7);
    let tuple = heap.tuple(&[reference]).expect("the tuple fits");
    notes::reset();
    assert!(RuntimeContext::graph_holds_reference(tuple));
    let visits = notes::walk_snapshot().visits;
    assert_eq!(visits, 2, "the tuple and the reference block it holds");
    assert_eq!(
        notes::walk_fold_snapshot(),
        (1, visits),
        "the reference-header early exit folds the local counter exactly once too"
    );
}
