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
//! **Micro-benchmark: the heap-fragment route a control signal takes when the receiver's heap must
//! not move — the per-signal *sizing + fill* cost of both of its sizing routes.**
//!
//! When a control signal (`{'EXIT', From, Reason}` / `{'DOWN', …}`) cannot be materialised on the
//! receiver's own heap — the receiver is `Running` (its machine's root snapshot is stale),
//! relocation-exempt, or the host declines the relocating collection — `src/ptab.rs`'s
//! `enqueue_fragment_payload` builds the whole payload into a `beam_message::HeapFragment`. That
//! function's **sizing** step is what this harness measures, because it changed:
//!
//! * **retired** — `HeapFragment::new(beam_term::size_object(payload) + 8)`: one *full graph walk*
//!   (which allocates its own walk buffers) and a single `copy_root`; an under-sized reservation
//!   propagated an `Err`.
//! * **current** — the reservation starts from a caller-held **lower bound** on the payload's
//!   destination price (the `AllocFailure.requested` figure the copier itself reported when it
//!   refused to put the same graph on the receiver's heap), plus a documented envelope/slack
//!   term, or from the floor constant `SIGNAL_FRAGMENT_FLOOR_WORDS` when the caller holds no
//!   price; the copy's own refusal then re-reserves the fragment **monotonically** to the exact
//!   price and retries. No `size_object` walk remains on the path: one preflight traversal
//!   (inside the copy) instead of two.
//!
//! Two facts fix the shape of a fragment signal, and this file reproduces both call sites rather
//! than one of them:
//!
//! * the **apply-time** route (`enqueue_fragment_signal`, `build_signal_message`'s `src: None`
//!   arm) holds **no** price — its payload build failed on a single bumped allocation of the
//!   receiver's own heap, which prices that allocation, not the payload — so it starts at the
//!   floor ([`Route::CurrentFloor`]). This is the shape a real abnormal process death produces:
//!   temporary instrumentation of
//!   `beam_rt -noshell -eval 'Ps = [spawn(fun() -> exit(boom) end) || _ <- lists:seq(1, 200)],
//!   timer:sleep(500), halt(0).'` saw the route fire ~276 times for 200 abnormal deaths (~1.4
//!   fragment signals per death), every one of them a **4-word** `{'EXIT', From, boom}` payload
//!   with a `32`-byte `AllocKind::Heap` refusal and a `Running` receiver.
//! * the **enqueue-time** route (`deliver_exit_fragment`/`deliver_down_fragment`, the reason still
//!   living in the signaller's heap) **does** hold the reason sub-graph's price — the copy that
//!   refused *was* a copy of that sub-graph — and adds the envelope's own words
//!   (`EXIT_ENVELOPE_WORDS`) plus the same slack ([`Route::CurrentHeld`]).
//!
//! # Model caveat (read this before quoting a number)
//!
//! `enqueue_fragment_payload` is **private** to `beam_process`, so this example cannot call it; it
//! reproduces both routes out of exactly the public primitives that function uses — a
//! `beam_alloc::Heap` scratch arena plus the crate's own payload builder
//! (`build_exit_message_term`), `beam_message::HeapFragment::new` + `copy_root`,
//! `Fragments::new`/`push`, `Message::new(..).with_fragments(..)` — with the route's constants
//! (scratch words, slack, floor, envelope) and its `refused_words` conversion **copied by hand**
//! from `src/ptab.rs` and named after their originals. The figures are therefore *per signal*
//! sizing + fill (plus the message's own drop, identically for every route), not an end-to-end
//! signal delivery: the receiver, its mailbox and its collection are out of scope.
//!
//! # Method
//!
//! * **Exact, load-immune counters** — a counting `#[global_allocator]` wraps
//!   `std::alloc::System`, so `allocs_per_signal` / `reallocs_per_signal` / `bytes_per_signal`
//!   are exact integers, reproducible on any host. That is what makes the claim *checkable*: the
//!   retired route pays the sizing walk's own buffers, the current routes pay none.
//! * **Paired, alternating wall clock** — each round times every route on the same shape
//!   back-to-back, and consecutive rounds reverse the order, so a drifting host biases all routes
//!   alike; the report quotes the **median** of the per-round `ns/signal`, its min–max spread,
//!   and the **median of the per-round paired ratios and differences** (route B vs route A on the
//!   same round, never two independently scheduled runs). Absolute nanoseconds mean nothing on a
//!   shared host: read the ratios and the exact counters, and check `/proc/loadavg` (printed
//!   before and after).
//! * **Attribution** — the per-signal wall clock is dominated by `beam_alloc`, not by the sizing
//!   route, so the report closes with the fixed costs every route pays: the scratch arena's
//!   allocate-and-release, a fragment's allocate-and-release at the reservations used here, and
//!   `size_object`'s own price per payload. Without that block, a reader would attribute an
//!   allocator block-size effect to the sizing route.
//!
//! # Reading the numbers
//!
//! On the reference host an allocate-and-release of a 512-byte fragment costs ~2.3 µs, of a
//! ~100-byte one ~7.8 µs, of a 64-byte one ~13 µs and of a 2 KiB one ~0.18 µs: `beam_alloc`'s
//! small-block paths get *more* expensive as the block gets smaller (an observed `beam_alloc`
//! property, not this harness's and not the route's). Two consequences:
//!
//! * `retired` and `current (enqueue-time)` reserve the **same** capacity for these shapes — pinned
//!   by an assert in [`main`] — so their paired ratio is the walk's marginal cost alone, which is
//!   ~30 ns / ~105 ns / ~465 ns for the three shapes (it tracks `size_object`'s own price, which
//!   the attribution block prints per shape). That is the actual size of the win the change bought.
//! * `current (apply-time)`'s 64-word floor lands on a *different* block-size path than the tight
//!   `size_object`-derived reservation the retired route made of a 4-word payload, so for shape 1 it
//!   is microseconds cheaper for an allocator reason — and for shape 3 it pays a second fragment
//!   because the floor is under-priced.
//!
//! The three payload shapes are the point of the exercise — the shape a real workload emits, and
//! the two shapes where a *walk* could plausibly pay for itself:
//!
//! | shape | payload | why |
//! |---|---|---|
//! | 1 | `{'EXIT', From, boom}` — 4 words | what an abnormal process death actually produces (measured above) |
//! | 2 | `{'EXIT', From, {40-element tuple}}` — 45 words | a `{badmatch, …}`/`{case_clause, …}`-sized reason; the shape the crate's own sizing test pins |
//! | 3 | `{'EXIT', From, {2 000-element tuple}}` — 2 005 words | a large reason, where the floor is under-priced and the optimistic route must re-reserve once |
//!
//! ```text
//! erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml --release \
//!     -p beam_process --example perf_signal_fragment -- [scale] [rounds] [shape:route]
//! ```
//!
//! `scale` (default `1`) multiplies every route's iteration count, `rounds` (default
//! [`DEFAULT_ROUNDS`]) is the number of paired rounds per shape, and an optional third argument
//! (`1:retired`, `3:held`, …) runs **one** block and exits — the form to wrap in `perf stat`, which
//! needs a single well-defined workload ([`run_only`]).

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use beam_alloc::Heap;
use beam_common::{AllocKind, BeamError};
use beam_message::{Fragments, HeapFragment, Message, OffHeapTable};
use beam_process::prelude::*;
use beam_term::prelude::*;

// ---------------------------------------------------------------------------
// The route's constants, copied from `src/ptab.rs` (private there)
// ---------------------------------------------------------------------------

/// Reserved words for the scratch arena a fragment-backed signal payload is built on
/// (`src/ptab.rs`'s `SIGNAL_SCRATCH_WORDS`).
const SIGNAL_SCRATCH_WORDS: usize = 64;

/// Term-word slack added on top of a caller-held refusal price (`FRAGMENT_SLACK_WORDS`). An
/// **optimisation**, never a correctness requirement: an under-sized reservation is re-reserved
/// from the copy's own refusal.
const FRAGMENT_SLACK_WORDS: usize = 8;

/// The reservation a fragment starts from when the caller holds **no** price for it
/// (`SIGNAL_FRAGMENT_FLOOR_WORDS`) — the apply-time route.
const SIGNAL_FRAGMENT_FLOOR_WORDS: usize = 64;

/// The words the `{'EXIT', From, Reason}` envelope adds to the `reason` sub-graph it wraps
/// (`EXIT_ENVELOPE_WORDS`): a tuple header plus three elements.
const EXIT_ENVELOPE_WORDS: usize = 4;

/// The number of paired rounds per shape (consecutive rounds reverse the route order).
const DEFAULT_ROUNDS: usize = 7;

/// The iteration count of one exact-allocation pass — the counters are integer-exact, so a small
/// pass reproduces the steady-state per-signal figures.
const ALLOC_ITERS: u64 = 200;

/// The iteration count of one attribution row (an allocate-and-release pair costs microseconds).
const ATTR_ITERS: u64 = 5_000;

// ---------------------------------------------------------------------------
// The counting allocator: exact per-signal allocation figures
// ---------------------------------------------------------------------------

/// Allocations performed so far (every `alloc`).
static ALLOCS: AtomicU64 = AtomicU64::new(0);
/// `realloc` calls so far (a growing walk buffer shows up here rather than in [`ALLOCS`]).
static REALLOCS: AtomicU64 = AtomicU64::new(0);
/// Bytes requested so far, by `alloc` and by `realloc`.
static BYTES: AtomicU64 = AtomicU64::new(0);

/// The counting allocator: `System` plus three counters, so a run reports exact figures.
struct Counting;

// SAFETY: every method forwards to `System` unchanged (the same layout, the same pointer) and
// only adds relaxed counter increments on the side, so the `GlobalAlloc` contract — the returned
// block satisfies the layout and is freed by the matching `dealloc`/`realloc` — is exactly
// `System`'s. No `unsafe` escapes this impl.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: forwarded unchanged — the caller's layout is `System`'s precondition.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: as `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        REALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        // SAFETY: forwarded unchanged — the caller guarantees `ptr` came from this allocator with
        // `layout`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// `/proc/loadavg`'s first three fields — the host caveat every wall-clock figure carries.
fn loadavg() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
        .unwrap_or_else(|_| "unknown".to_string())
}

// ---------------------------------------------------------------------------
// The two routes, plus the current route's two call-site flavours
// ---------------------------------------------------------------------------

/// One way of sizing (+ filling) a signal fragment.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Route {
    /// The **retired** route: `HeapFragment::new(size_object(payload) + FRAGMENT_SLACK_WORDS)`,
    /// one full graph walk (with its own walk buffers) and a single `copy_root`. It had no retry:
    /// an under-sized reservation propagated the `Err`.
    Retired,
    /// The **current** route as the apply-time call site drives it: no held price, so the
    /// reservation starts at [`SIGNAL_FRAGMENT_FLOOR_WORDS`] and the copy's refusal re-reserves
    /// it monotonically to the exact price.
    CurrentFloor,
    /// The **current** route as the enqueue-time call sites drive it: the held figure prices the
    /// `reason` sub-graph only, so the envelope's words and the slack are added on top. One copy
    /// attempt for these shapes, because the price is a lower bound the copy cannot exceed for a
    /// binary-free graph.
    CurrentHeld,
}

impl Route {
    /// Every route, in the order a round times them on even rounds.
    const ALL: [Route; 3] = [Route::Retired, Route::CurrentFloor, Route::CurrentHeld];

    fn label(self) -> &'static str {
        match self {
            Route::Retired => "retired  (size_object walk)",
            Route::CurrentFloor => "current  (apply-time, floor 64)",
            Route::CurrentHeld => "current  (enqueue-time, held price)",
        }
    }

    /// The reservation this route ends up with for a payload of `payload_words` built around a
    /// `reason` of `reason_words`.
    ///
    /// * retired: the walk's count plus the slack;
    /// * apply-time: the floor, re-reserved by the copy when the payload is larger;
    /// * enqueue-time: the held reason price + the envelope + the slack.
    fn reservation(self, reason_words: usize, payload_words: usize) -> usize {
        match self {
            Route::Retired => payload_words + FRAGMENT_SLACK_WORDS,
            Route::CurrentFloor => SIGNAL_FRAGMENT_FLOOR_WORDS.max(payload_words),
            Route::CurrentHeld => reason_words + EXIT_ENVELOPE_WORDS + FRAGMENT_SLACK_WORDS,
        }
    }
}

/// The word count a `Message`-kind refusal reports, when its byte figure is a whole (nonzero)
/// number of term words — `src/ptab.rs`'s private `refused_words`, verbatim.
///
/// The `AllocKind::Message` figure is the copier's **exact destination price** in bytes: the copy
/// preflights the whole graph in one walk and refuses with that count.
fn refused_words(requested_bytes: usize) -> Option<usize> {
    let word = core::mem::size_of::<Eterm>();
    let words = requested_bytes / word;
    (words > 0 && requested_bytes % word == 0).then_some(words)
}

// ---------------------------------------------------------------------------
// The payload shapes
// ---------------------------------------------------------------------------

/// The three `reason` graphs a fragment signal wraps — see the module docs' table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Shape {
    /// A 1-word atom reason: the 4-word `{'EXIT', From, boom}` a real abnormal death produces.
    Death,
    /// A 41-word reason (a 40-element tuple), the size of a `{badmatch, …}`/`{case_clause, …}`
    /// graph — the shape the crate's own sizing test pins.
    MidReason,
    /// A 2 001-word reason (a 2 000-element tuple).
    LargeReason,
}

impl Shape {
    const ALL: [Shape; 3] = [Shape::Death, Shape::MidReason, Shape::LargeReason];

    fn name(self) -> &'static str {
        match self {
            Shape::Death => "shape 1",
            Shape::MidReason => "shape 2",
            Shape::LargeReason => "shape 3",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Shape::Death => "{'EXIT', From, boom} — the real abnormal-death payload",
            Shape::MidReason => "{'EXIT', From, {40-element tuple}} — a case_clause-sized reason",
            Shape::LargeReason => "{'EXIT', From, {2000-element tuple}} — a large reason",
        }
    }

    /// The element count of the tuple `reason` this shape builds (0 for the atom reason).
    fn reason_elements(self) -> usize {
        match self {
            Shape::Death => 0,
            Shape::MidReason => 40,
            Shape::LargeReason => 2_000,
        }
    }

    /// The iteration count of one timed block at `scale` 1, sized so a block is a handful of
    /// milliseconds on the reference host.
    fn iterations(self) -> u64 {
        match self {
            Shape::Death => 100_000,
            Shape::MidReason => 20_000,
            Shape::LargeReason => 2_000,
        }
    }
}

// ---------------------------------------------------------------------------
// The fixture: the signaller's stable heap, its off-heap table and the two cached prices
// ---------------------------------------------------------------------------

/// One shape's stable input.
///
/// The `reason` graph lives on the **signaller's** heap for the whole run, exactly as it does in a
/// real signalling process, so the copy's source never moves and the harness never pays to rebuild
/// the graph inside a timed block. `off_heap` is that process's (empty) off-heap table — the table
/// `copy_struct` resolves refc binaries and MSO through.
struct Source {
    /// The shared atom space (the `'EXIT'` head and, for shape 1, the `boom` reason).
    atoms: AtomTable,
    /// The signal atoms, so the payload is built by the crate's own builder.
    signals: SignalAtoms,
    /// The signaller's heap. Never read after construction; held only so the `reason` graph's
    /// words stay mapped.
    _signaller_heap: Heap,
    /// The signaller's off-heap table (the copy's `src_off_heap`).
    off_heap: OffHeapTable,
    /// The signalling pid, as the immediate it is (the `From` slot).
    from: Eterm,
    /// The `reason` term, on [`Source::_signaller_heap`].
    reason: Eterm,
    /// The reason sub-graph's own flat word count — the figure an enqueue-time refusal reports.
    reason_words: usize,
    /// The whole payload's flat word count, i.e. the copy's destination price. Equal to
    /// `reason_words + EXIT_ENVELOPE_WORDS`, pinned below.
    payload_words: usize,
}

impl Source {
    /// Builds the shape's reason graph and prices both it and the payload the route will build
    /// around it.
    fn new(shape: Shape) -> Source {
        let mut atoms = AtomTable::new();
        let signals = SignalAtoms::from_table(&mut atoms);
        let from = ProcessId::new(1, 0).to_eterm();
        let elements = shape.reason_elements();
        // One arena for the reason graph, built once and never moved.
        let mut heap = Heap::new(elements + 8).expect("the signaller's arena allocates");
        let reason = if elements == 0 {
            make_atom(atoms.intern("boom"))
        } else {
            let elems: Vec<Eterm> = (0..elements).map(|n| make_small(n as i64)).collect();
            heap.tuple(&elems).expect("the reason tuple fits the arena")
        };
        let reason_words = size_object(reason);
        // Price the payload exactly the way the route builds it, on a throwaway scratch arena, so
        // no figure in this file is derived from a hand-written word count.
        let mut probe = Heap::new(SIGNAL_SCRATCH_WORDS).expect("the probe arena allocates");
        let payload = build_exit_message_term(&mut probe, &signals, from, reason)
            .expect("the 4-word envelope fits the scratch arena");
        let payload_words = size_object(payload);
        assert_eq!(
            payload_words,
            reason_words + EXIT_ENVELOPE_WORDS,
            "the envelope constant must match the builder's actual shape"
        );
        Source {
            atoms,
            signals,
            _signaller_heap: heap,
            off_heap: OffHeapTable::new(),
            from,
            reason,
            reason_words,
            payload_words,
        }
    }
}

// ---------------------------------------------------------------------------
// The measured operation: one signal's fragment sizing + fill
// ---------------------------------------------------------------------------

/// Performs one signal's sizing + fill exactly as `enqueue_fragment_payload` does it for `route`,
/// and hands back the message the real function would enqueue.
///
/// The three steps that are **not** the route's business are identical everywhere: the scratch
/// arena, the payload build, and the enqueue (`Fragments`/`Message`).
fn enqueue_signal(src: &Source, route: Route) -> Message {
    // The real first step: build the payload on a fresh scratch arena, so the payload never
    // depends on a heap that can move. The reason itself is only *referenced* from here — the copy
    // walks into the signaller's heap through that word.
    let mut scratch = Heap::new(SIGNAL_SCRATCH_WORDS).expect("the scratch arena allocates");
    let payload = build_exit_message_term(&mut scratch, &src.signals, src.from, src.reason)
        .expect("the 4-word envelope fits the scratch arena");
    let mut capacity = match route {
        // RETIRED: a full graph walk (which allocates its own walk buffers) for the reservation.
        Route::Retired => size_object(payload) + FRAGMENT_SLACK_WORDS,
        // CURRENT, apply-time: no held price, so the floor.
        Route::CurrentFloor => SIGNAL_FRAGMENT_FLOOR_WORDS,
        // CURRENT, enqueue-time: the held reason price, the envelope's words, the slack.
        Route::CurrentHeld => src.reason_words + EXIT_ENVELOPE_WORDS + FRAGMENT_SLACK_WORDS,
    };
    // `copy_struct` walks the graph by pointer, so the heap-resident `reason` subtree is copied in
    // too; the signaller's persistent off-heap table resolves its refc binaries/MSO.
    let (fragment, root) = if route == Route::Retired {
        let mut fragment = HeapFragment::new(capacity).expect("the fragment allocates");
        // The retired route had **no** retry: an under-sized reservation was an `Err`. Its
        // `size_object` count is exact for a binary-free graph (no refc binary for the copier to
        // flatten into more destination words than the source occupies).
        let root = fragment
            .copy_root(payload, &src.off_heap)
            .expect("a walk-derived reservation is exact for a binary-free payload");
        (fragment, root)
    } else {
        loop {
            let mut candidate = HeapFragment::new(capacity).expect("the fragment allocates");
            match candidate.copy_root(payload, &src.off_heap) {
                Ok(root) => break (candidate, root),
                // The copy priced the payload exactly: reserve that and retry (strictly larger,
                // so the loop terminates at the true price).
                Err(BeamError::Alloc(failure)) if failure.kind == AllocKind::Message => {
                    match refused_words(failure.requested) {
                        Some(words) if words > capacity => capacity = words,
                        _ => panic!("a copy refusal without a usable price: {failure:?}"),
                    }
                }
                Err(other) => panic!("a binary-free payload cannot fail to copy: {other:?}"),
            }
        }
    };
    let mut fragments = Fragments::new();
    fragments.push(fragment);
    Message::new(root, src.from).with_fragments(fragments)
}

/// Structurally verifies one delivered message (outside every timed block): the payload is
/// fragment-resident, sized exactly, and is the `{'EXIT', From, Reason}` graph that went in.
fn verify(src: &Source, route: Route, message: &Message) {
    assert_eq!(
        message.from(),
        src.from,
        "the envelope's `from` stays the signaller's pid immediate"
    );
    let payload = message.payload();
    let fragments = message
        .fragments()
        .expect("the fragment route's message owns a fragment chain");
    assert_eq!(fragments.len(), 1, "one fragment holds the whole payload");
    let fragment = fragments.iter().next().expect("the chain is non-empty");
    assert!(
        fragment.contains(payload),
        "the payload root must point into the fragment's own region"
    );
    assert_eq!(
        fragment.used(),
        src.payload_words,
        "the copy writes exactly the payload's flat words"
    );
    assert_eq!(
        fragment.capacity(),
        route.reservation(src.reason_words, src.payload_words),
        "{} reserves what its sizing route says it does",
        route.label()
    );
    // SAFETY: `payload` is the live 3-tuple the copy engine wrote into the fragment, so its size
    // word and elements 0..3 are in bounds.
    unsafe {
        assert!(is_tuple(payload), "the payload is the `{{'EXIT', …}}` tuple");
        assert_eq!(tuple_size(payload), 3, "a header plus three elements");
        assert_eq!(
            tuple_element(payload, 0),
            make_atom(src.signals.exit()),
            "the head is the `'EXIT'` atom"
        );
        assert_eq!(tuple_element(payload, 1), src.from, "the `From` slot");
        let delivered = tuple_element(payload, 2);
        assert!(
            beam_term::compare::eq(delivered, src.reason, &src.atoms),
            "the whole reason graph arrived (compared structurally: it now lives in the fragment)"
        );
    }
}

// ---------------------------------------------------------------------------
// The harness: exact allocations, paired alternating rounds, attribution
// ---------------------------------------------------------------------------

/// One timed block of `iterations` signals: `ns` per signal.
fn block_ns(src: &Source, route: Route, iterations: u64) -> f64 {
    assert!(iterations > 0, "a block must divide by a positive count");
    let started = Instant::now();
    for _ in 0..iterations {
        let message = enqueue_signal(src, route);
        // Keep the whole chain observable, so the sizing + fill cannot be optimised away.
        black_box(message.payload());
    }
    started.elapsed().as_secs_f64() * 1e9 / iterations as f64
}

/// One `ns/op` block around an arbitrary operation (the attribution rows).
fn time_block(iterations: u64, mut op: impl FnMut()) -> f64 {
    for _ in 0..(iterations / 10).max(1) {
        op();
    }
    let started = Instant::now();
    for _ in 0..iterations {
        op();
    }
    started.elapsed().as_secs_f64() * 1e9 / iterations as f64
}

/// Exact per-signal allocation figures for one route (integer counts: load-immune).
struct AllocRow {
    allocs: u64,
    reallocs: u64,
    bytes: u64,
}

/// Counts the allocations of `iterations` signals and divides by them (asserting the division is
/// exact — every iteration performs the same allocations in steady state).
fn allocations_per_signal(src: &Source, route: Route, iterations: u64) -> AllocRow {
    // Warm up first: the copier's preflight runs on a warm per-thread scratch, so its one-off
    // growth is not a per-signal cost.
    for _ in 0..8 {
        black_box(enqueue_signal(src, route));
    }
    let allocs_before = ALLOCS.load(Ordering::Relaxed);
    let reallocs_before = REALLOCS.load(Ordering::Relaxed);
    let bytes_before = BYTES.load(Ordering::Relaxed);
    for _ in 0..iterations {
        black_box(enqueue_signal(src, route));
    }
    let allocs = ALLOCS.load(Ordering::Relaxed) - allocs_before;
    let reallocs = REALLOCS.load(Ordering::Relaxed) - reallocs_before;
    let bytes = BYTES.load(Ordering::Relaxed) - bytes_before;
    assert_eq!(
        allocs % iterations,
        0,
        "every iteration allocates the same number of times"
    );
    assert_eq!(
        reallocs % iterations,
        0,
        "every iteration reallocates the same number of times"
    );
    assert_eq!(
        bytes % iterations,
        0,
        "every iteration requests the same number of bytes"
    );
    AllocRow {
        allocs: allocs / iterations,
        reallocs: reallocs / iterations,
        bytes: bytes / iterations,
    }
}

/// One route's measurement for one shape: the per-round `ns/signal`, in round order.
struct Row {
    shape: &'static str,
    route: Route,
    ns: Vec<f64>,
}

impl Row {
    /// The median of the per-round figures.
    fn median_ns(&self) -> f64 {
        median(&self.ns)
    }
}

/// The median of `values` (the upper middle value for an even count).
fn median(values: &[f64]) -> f64 {
    assert!(!values.is_empty(), "a statistic needs at least one value");
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("no NaN in a timing"));
    sorted[sorted.len() / 2]
}

/// The smallest value.
fn min_of(values: &[f64]) -> f64 {
    values.iter().copied().fold(f64::INFINITY, f64::min)
}

/// The largest value.
fn max_of(values: &[f64]) -> f64 {
    values.iter().copied().fold(f64::NEG_INFINITY, f64::max)
}

/// The row for `(shape, route)` (a missing row is a bug in this file).
fn row_mut<'a>(rows: &'a mut [Row], shape: &'static str, route: Route) -> &'a mut Row {
    rows.iter_mut()
        .find(|row| row.shape == shape && row.route == route)
        .unwrap_or_else(|| panic!("the row `{shape} / {}` must be measured", route.label()))
}

/// The per-round **paired ratio** `b[i] / a[i]` and the **paired difference** `b[i] - a[i]` — the
/// paired estimator: every ratio and difference compares two routes timed on the *same* round, so
/// a drifting host biases both sides alike.
///
/// Returns `(ratio median, ratio min, ratio max, difference median, difference min, difference max)`.
fn paired(a: &[f64], b: &[f64]) -> (f64, f64, f64, f64, f64, f64) {
    assert_eq!(a.len(), b.len(), "a paired comparison needs equal round counts");
    let ratios: Vec<f64> = a.iter().zip(b).map(|(a, b)| b / a).collect();
    let diffs: Vec<f64> = a.iter().zip(b).map(|(a, b)| b - a).collect();
    (
        median(&ratios),
        min_of(&ratios),
        max_of(&ratios),
        median(&diffs),
        min_of(&diffs),
        max_of(&diffs),
    )
}

/// Measures **one** `(shape, route)` block and exits — the `only` mode.
///
/// It exists so the example can be wrapped in an instruction counter, which needs a single
/// well-defined workload:
///
/// ```text
/// perf stat -e cpu_core/instructions/u taskset -c 1 \
///     erts_rust/target/release/examples/perf_signal_fragment 1 1 1:retired
/// ```
fn run_only(shape: Shape, route: Route, scale: u64) {
    let src = Source::new(shape);
    verify(&src, route, &enqueue_signal(&src, route));
    let iterations = shape.iterations() * scale;
    // Warm up, so the counted window is steady state.
    block_ns(&src, route, (iterations / 10).max(1));
    let ns = block_ns(&src, route, iterations);
    println!(
        "perf_signal_fragment | only {:<8} {:<34} {iterations} signals  {ns:.1} ns/signal",
        shape.name(),
        route.label()
    );
}

/// Parses the `only` mode's `shape:route` selector (`1:retired`, `3:held`, …).
fn parse_only(selector: &str) -> (Shape, Route) {
    let (shape, route) = selector
        .split_once(':')
        .unwrap_or_else(|| panic!("`{selector}` is not a `shape:route` selector"));
    let shape = match shape {
        "1" => Shape::Death,
        "2" => Shape::MidReason,
        "3" => Shape::LargeReason,
        other => panic!("`{other}` is not a shape (1, 2 or 3)"),
    };
    let route = match route {
        "retired" => Route::Retired,
        "floor" => Route::CurrentFloor,
        "held" => Route::CurrentHeld,
        other => panic!("`{other}` is not a route (retired, floor or held)"),
    };
    (shape, route)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let scale: u64 = args
        .next()
        .map(|a| a.parse().expect("a scale factor"))
        .unwrap_or(1);
    let rounds: usize = args
        .next()
        .map(|a| a.parse().expect("a round count"))
        .unwrap_or(DEFAULT_ROUNDS);
    let only = args.next().map(|selector| parse_only(&selector));
    assert!(scale > 0, "the scale factor must be positive");
    assert!(rounds > 0, "at least one round is needed");
    if let Some((shape, route)) = only {
        return run_only(shape, route, scale);
    }

    println!("perf_signal_fragment | loadavg(before) = {}", loadavg());
    println!(
        "perf_signal_fragment | rounds = {rounds} (alternating order), allocation pass = \
         {ALLOC_ITERS} signals/route"
    );
    println!(
        "perf_signal_fragment | model: `enqueue_fragment_payload` is private, so both sizing \
         routes are reproduced from its public primitives"
    );
    println!();

    let mut rows: Vec<Row> = Shape::ALL
        .iter()
        .flat_map(|shape| {
            Route::ALL.iter().map(move |route| Row {
                shape: shape.name(),
                route: *route,
                ns: Vec::with_capacity(rounds),
            })
        })
        .collect();
    let mut allocs: Vec<(Shape, Route, AllocRow, usize)> = Vec::new();
    let mut prices: Vec<(Shape, usize, usize)> = Vec::new();
    let mut attribution: Vec<(String, f64)> = Vec::new();

    for shape in Shape::ALL {
        let src = Source::new(shape);
        // Correctness first, outside every timed block: every route delivers the same graph.
        for route in Route::ALL {
            let message = enqueue_signal(&src, route);
            verify(&src, route, &message);
        }
        // The enqueue-time route must restore exactly the reservation the retired *walk* derived,
        // for a payload whose copy price equals its source occupancy (no refc binary) — i.e. the
        // walk was pure overhead for these shapes, and its two routes are compared at equal
        // capacity.
        assert_eq!(
            Route::CurrentHeld.reservation(src.reason_words, src.payload_words),
            Route::Retired.reservation(src.reason_words, src.payload_words),
            "the held price plus the envelope plus the slack is the walk's own figure"
        );
        prices.push((shape, src.reason_words, src.payload_words));
        for route in Route::ALL {
            let row = allocations_per_signal(&src, route, ALLOC_ITERS);
            let reserved = route.reservation(src.reason_words, src.payload_words);
            allocs.push((shape, route, row, reserved));
        }
        // What the walk itself costs, for the attribution block: the same payload the route sizes.
        let mut probe = Heap::new(SIGNAL_SCRATCH_WORDS).expect("the probe arena allocates");
        let payload = build_exit_message_term(&mut probe, &src.signals, src.from, src.reason)
            .expect("the payload fits the probe arena");
        attribution.push((
            format!("size_object(payload), {} — the retired route's walk", shape.name()),
            time_block(shape.iterations(), || {
                black_box(size_object(payload));
            }),
        ));
        // Warm up every route, so the measured window is steady state (warm copier scratch, warm
        // allocator free lists) for each of them.
        let iterations = shape.iterations() * scale;
        for route in Route::ALL {
            block_ns(&src, route, (iterations / 10).max(1));
        }
        for round in 0..rounds {
            // Alternate the order every round, so a monotone drift on the host cannot favour one
            // route: even rounds run retired → floor → held, odd rounds the reverse.
            for index in 0..Route::ALL.len() {
                let route = if round % 2 == 0 {
                    Route::ALL[index]
                } else {
                    Route::ALL[Route::ALL.len() - 1 - index]
                };
                let ns = block_ns(&src, route, iterations);
                row_mut(&mut rows, shape.name(), route).ns.push(ns);
            }
        }
    }

    // The fixed per-signal costs, measured once: an allocate-and-release pair, which is what one
    // signal pays whatever the sizing route.
    attribution.insert(
        0,
        (
            format!("Heap::new({SIGNAL_SCRATCH_WORDS}) — the scratch arena every route builds on"),
            time_block(ATTR_ITERS, || {
                black_box(Heap::new(SIGNAL_SCRATCH_WORDS).expect("the arena allocates"));
            }),
        ),
    );
    // The reservations the three routes actually take (13 = shape 1's tight walk-derived one, 53 =
    // shape 2's, 64 = the apply-time floor, 2 005 = shape 3's), plus the block sizes the block-size
    // curve turns on.
    for capacity in [8usize, 13, 53, 64, 128, 256, 512, 2_005] {
        attribution.push((
            format!("HeapFragment::new({capacity}) — a fragment's allocate+release"),
            time_block(ATTR_ITERS, || {
                black_box(HeapFragment::new(capacity).expect("the fragment allocates"));
            }),
        ));
    }

    // --- the payloads ------------------------------------------------------
    println!("perf_signal_fragment | payloads under test");
    for (shape, reason_words, payload_words) in &prices {
        println!(
            "perf_signal_fragment |   {:<8} {:<62} reason {:>5} w, payload {:>5} w",
            shape.name(),
            shape.description(),
            reason_words,
            payload_words
        );
    }
    println!();

    // --- exact allocations -------------------------------------------------
    println!(
        "perf_signal_fragment | EXACT per signal (counting global allocator: integers, load-immune)"
    );
    println!(
        "perf_signal_fragment | {:<8} {:<34} {:>7} {:>9} {:>10} {:>10}",
        "shape", "route", "allocs", "reallocs", "bytes", "reserved"
    );
    for (shape, route, row, reserved) in &allocs {
        println!(
            "perf_signal_fragment | {:<8} {:<34} {:>7} {:>9} {:>10} {:>7} w",
            shape.name(),
            route.label(),
            row.allocs,
            row.reallocs,
            row.bytes,
            reserved
        );
    }
    println!();

    // --- paired wall clock -------------------------------------------------
    println!(
        "perf_signal_fragment | ns/signal, median [min..max] over {rounds} paired rounds"
    );
    println!(
        "perf_signal_fragment | {:<8} {:<34} {:>12} {:>12} {:>12}",
        "shape", "route", "median", "min", "max"
    );
    for row in &rows {
        println!(
            "perf_signal_fragment | {:<8} {:<34} {:>12.1} {:>12.1} {:>12.1}",
            row.shape,
            row.route.label(),
            row.median_ns(),
            min_of(&row.ns),
            max_of(&row.ns)
        );
    }
    println!();

    // --- the current/retired ratios ---------------------------------------
    println!(
        "perf_signal_fragment | current vs retired, paired per round: ratio (median [min..max]) \
         and difference (median [min..max] ns)"
    );
    for shape in Shape::ALL {
        let retired = rows
            .iter()
            .find(|row| row.shape == shape.name() && row.route == Route::Retired)
            .expect("the retired row is measured")
            .ns
            .clone();
        for route in [Route::CurrentFloor, Route::CurrentHeld] {
            let current = rows
                .iter()
                .find(|row| row.shape == shape.name() && row.route == route)
                .expect("every current row is measured")
                .ns
                .clone();
            let (ratio, ratio_min, ratio_max, diff, diff_min, diff_max) = paired(&retired, &current);
            println!(
                "perf_signal_fragment | {:<8} {:<34} {:.3}x [{:.3}..{:.3}]  {:+.1} ns \
                 [{:+.1}..{:+.1}]",
                shape.name(),
                route.label(),
                ratio,
                ratio_min,
                ratio_max,
                diff,
                diff_min,
                diff_max
            );
        }
    }
    println!();

    // --- attribution -------------------------------------------------------
    println!(
        "perf_signal_fragment | attribution: the fixed per-signal costs, ns per allocate+release \
         (these bound what a sizing route can be blamed for)"
    );
    for (label, ns) in &attribution {
        println!("perf_signal_fragment |   {label:<62} {ns:>10.1} ns");
    }
    println!();

    println!("perf_signal_fragment | loadavg(after) = {}", loadavg());

    // -----------------------------------------------------------------------
    // Structural invariants only — never a wall-clock threshold (that would be
    // flaky on a shared, loaded host; the printed numbers are the deliverable).
    // -----------------------------------------------------------------------
    assert_eq!(rows.len(), Shape::ALL.len() * Route::ALL.len());
    for row in &rows {
        assert_eq!(row.ns.len(), rounds, "every row has one figure per round");
        let ns = row.median_ns();
        assert!(
            ns.is_finite() && ns > 0.0,
            "{} / {} must report a positive finite ns/signal, got {ns}",
            row.shape,
            row.route.label()
        );
        assert!(
            (1.0..1.0e8).contains(&ns),
            "{} / {} reported an implausible {ns} ns/signal",
            row.shape,
            row.route.label()
        );
    }
    // The claim this whole exercise rests on: the retired route's `size_object` walk allocates its
    // own walk buffers, which the current routes never pay. An integer assertion about
    // allocations, not a timing one, so it holds on any host.
    for shape in Shape::ALL {
        let allocs_of = |route: Route| {
            allocs
                .iter()
                .find(|(s, r, _, _)| *s == shape && *r == route)
                .map(|(_, _, row, _)| row.allocs)
                .expect("every route's allocations are measured")
        };
        assert!(
            allocs_of(Route::Retired) > allocs_of(Route::CurrentFloor),
            "{}: the retired walk's buffers must cost allocations the current routes do not pay \
             ({} vs {})",
            shape.name(),
            allocs_of(Route::Retired),
            allocs_of(Route::CurrentFloor)
        );
        assert!(
            allocs_of(Route::Retired) > allocs_of(Route::CurrentHeld),
            "{}: the same, for the enqueue-time route ({} vs {})",
            shape.name(),
            allocs_of(Route::Retired),
            allocs_of(Route::CurrentHeld)
        );
    }
}
