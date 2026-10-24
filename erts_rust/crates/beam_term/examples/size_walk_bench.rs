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

//! An in-process A/B benchmark for the sizing walk (`beam_term::size`), three ways over
//! the **same** term graph:
//!
//! * **`retired`** — a frozen copy of the *pre-change* walk ([`mod retired`]): every call
//!   builds its own `Vec<Step>` DFS stack and a SipHash-1-3 `HashSet<usize>` path set.
//!   This is the baseline the change is measured against;
//! * **`fresh`** — the live walk through the scratch-free entry point
//!   ([`size_object_with`]), i.e. the live code with a per-call scratch. The difference
//!   against `retired` is the hasher alone;
//! * **`scratch`** — the live walk on one **warm** [`SizingScratch`]
//!   ([`size_object_with_scratch`]), i.e. the live code with reused buffers. The
//!   difference against `fresh` is the allocation alone, and `retired`/`scratch` is the
//!   whole change.
//!
//! All three count the same graph — the guard below aborts the run if any pair disagrees —
//! and they are interleaved **per call** with the order rotated every iteration, so no
//! side systematically runs first on a warming cache.
//!
//! ```text
//! erts_rust/scripts/cargo.sh run --release --manifest-path erts_rust/Cargo.toml \
//!     -p beam_term --example size_walk_bench -- [scale] [all|retired|fresh|scratch] [bucket]
//! ```
//!
//! * `scale`  — multiplies every bucket's call count (default 1; an integer ≥ 1);
//! * `mode`   — which side(s) to time. `all` (the default) interleaves the three; a single
//!   side is the form to use under `perf stat`, where the instruction count must be
//!   attributable to one walk:
//!
//! ```text
//! taskset -c 1 perf stat -e cpu_core/instructions/u ./target/release/examples/size_walk_bench 1 retired
//! taskset -c 1 perf stat -e cpu_core/instructions/u ./target/release/examples/size_walk_bench 1 scratch
//! ```
//!
//!   (Note: the `:u`-as-suffix form `cpu_core/instructions:u` can silently land on
//!   `cpu_atom` on a hybrid host — spell it `cpu_core/instructions/u`.)
//! * `bucket` — an optional bucket label (e.g. `message-40k`), so a `perf stat` run can be
//!   attributed to one graph shape.
//!
//! Columns:
//! * `bucket`/`words`/`calls` — the graph shape, its flat size, and the calls timed per
//!   side;
//! * `retired`/`fresh`/`scratch` — ns per call for each side;
//! * `retired/scratch` — the change's total speed-up (> 1.0 means the scratch-backed walk
//!   is faster than the pre-change one);
//! * `fresh/scratch` — the allocation half of it (> 1.0 means reusing the buffers beats
//!   rebuilding them);
//! * `allocs r/f/s` — **deterministic** allocations per call for the three sides, measured
//!   with the counting `#[global_allocator]` below. Host-load independent, and the number
//!   that makes the ns figures readable: `scratch` is `0` by construction.
//!
//! Absolute latencies on a shared host are meaningless on their own; the ratios are
//! same-run same-host comparisons. `/proc/loadavg` is printed before and after every run
//! for exactly that reason, and the instruction counts from `perf stat` are the
//! load-independent cross-check.
//!
//! The timing reads include two `Instant::now()` calls (tens of nanoseconds), which is why
//! the `small` bucket — where a whole walk costs about that much — is a *"no worse"*
//! control rather than a speed-up claim.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use beam_term::prelude::*;

// ---------------------------------------------------------------------------
// The counting allocator (per-thread armed, `tests/size_scratch.rs`'s pattern)
// ---------------------------------------------------------------------------

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
}

#[inline]
fn record(bytes: usize) {
    let _ = ARMED.try_with(|armed| {
        if armed.get() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(bytes, Ordering::Relaxed);
        }
    });
}

struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > layout.size() {
            record(new_size - layout.size());
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Runs `body` with the counters armed on this thread; returns (allocations, bytes, value).
fn allocations_in<T>(body: impl FnOnce() -> T) -> (usize, usize, T) {
    ARMED.with(|armed| armed.set(false));
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let before_bytes = BYTES.load(Ordering::Relaxed);
    ARMED.with(|armed| armed.set(true));
    let result = body();
    ARMED.with(|armed| armed.set(false));
    (
        ALLOCATIONS.load(Ordering::Relaxed) - before,
        BYTES.load(Ordering::Relaxed) - before_bytes,
        result,
    )
}

// ---------------------------------------------------------------------------
// The retired walk — the A/B baseline (a frozen copy of the pre-change algorithm)
// ---------------------------------------------------------------------------

/// A **frozen, verbatim copy** of the sizing walk as it stood before `SizingScratch` and
/// the address-mix hasher: every call builds `vec![Step::Enter(root)]` and a
/// `HashSet<usize>` path set with the standard library's default (SipHash-1-3) hasher.
///
/// It is the benchmark baseline and the equivalence oracle for this file — never used by
/// the crate, so do not "improve" it (the prose docs and the `SizingPolicy` definition
/// stay with the live code in `src/size.rs`; only the walk is copied here). Only the
/// unbounded form is kept: that is the mode the copier's preflight runs
/// (`limit == usize::MAX`, where the over-limit arm is unreachable), so the acyclicity
/// re-pass has no baseline to compare against.
mod retired {
        use std::collections::HashSet;

        use beam_term::prelude::*;

    /// Whether a word can contribute heap words (a term word that is not an immediate).
    #[inline]
    fn needs_visit(t: Eterm) -> bool {
        is_term_word(t) && !is_immediate(t)
    }

    /// One compound object a walk enters (address on the DFS path + its child slots).
    enum Frame {
        Cons {
            addr: usize,
            head: Eterm,
            tail: Eterm,
        },
        Box {
            addr: usize,
            words: usize,
            base: *mut Eterm,
            count: usize,
        },
    }

    impl Frame {
        fn addr(&self) -> usize {
            match self {
                Frame::Cons { addr, .. } | Frame::Box { addr, .. } => *addr,
            }
        }

        fn word_count(&self) -> usize {
            match self {
                Frame::Cons { .. } => 2,
                Frame::Box { words, .. } => *words,
            }
        }

        fn has_children(&self) -> bool {
            match self {
                Frame::Cons { .. } => true,
                Frame::Box { count, .. } => *count > 0,
            }
        }
    }

    /// One entry of the explicit DFS stack.
    enum Step {
        Enter(Eterm),
        Exit(usize),
    }

    /// Classifies a word into a [`Frame`] (the pre-change `classify`).
    ///
    /// # Safety
    ///
    /// `t` must be a live term word that is not an immediate.
    unsafe fn classify(t: Eterm, policy: &dyn SizingPolicy) -> Option<Frame> {
        if is_list(t) {
            // A cons cell is a header-less 2-word object.
            let p = list_val(t);
            // SAFETY: `t` is a live, non-`NIL` cons cell, so both words are readable.
            let (head, tail) = unsafe { (*p, *p.add(1)) };
            return Some(Frame::Cons {
                addr: p as usize,
                head,
                tail,
            });
        }
        let p = boxed_val(t);
        // SAFETY: `t` is a live boxed term, so `p` addresses its header word.
        let header = unsafe { *p };
        if header.raw() & TAG_HEADER_MASK == ARITYVAL_SUBTAG
            && header_arity(header) == 0
            && is_literal(t)
        {
            // The global empty-tuple literal (`tuple0()`): 0 words (`copy.c:125-140`).
            return None;
        }
        // The policy prices the object; `None` means it contributes nothing and exposes no
        // child (so the walk neither counts it nor descends into it).
        let words = policy.object_words(header, p)?;
        let mut base: *mut Eterm = p;
        let mut count = 0usize;
        // SAFETY: `header`/`p` head a live object whose sizing child slots are live. The
        // enumerator yields one contiguous ascending run, so the first offset pins `base`.
        unsafe {
            for_each_sizing_child_slot(header, p, |offset| {
                if count == 0 {
                    base = p.add(offset);
                }
                count += 1;
            });
        }
        Some(Frame::Box {
            addr: p as usize,
            words,
            base,
            count,
        })
    }
    /// Pushes every navigable sizing child of `frame` onto the DFS `stack`.
    fn push_frame_children(frame: &Frame, stack: &mut Vec<Step>) {
        match frame {
            Frame::Cons { head, tail, .. } => {
                if needs_visit(*tail) {
                    stack.push(Step::Enter(*tail));
                }
                if needs_visit(*head) {
                    stack.push(Step::Enter(*head));
                }
            }
            Frame::Box { base, count, .. } => {
                for i in 0..*count {
                    // SAFETY: `base` addresses `count` live child slots of the object the
                    // `Frame` was derived from.
                    let child = unsafe { *base.add(i) };
                    if needs_visit(child) {
                        stack.push(Step::Enter(child));
                    }
                }
            }
        }
    }

    /// Where the walk stopped, and the words it had counted by then.
    enum Sizing {
        Complete(usize),
        OverLimit(usize),
        Cyclic(usize),
    }

    /// The pre-change cycle-safe, `limit`-bounded sizing walk.
    fn sizing_walk(root: Eterm, limit: usize, policy: &dyn SizingPolicy) -> Sizing {
        let mut total = 0usize;
        let mut on_path: HashSet<usize> = HashSet::new();
        let mut stack: Vec<Step> = vec![Step::Enter(root)];

        while let Some(step) = stack.pop() {
            match step {
                Step::Exit(addr) => {
                    on_path.remove(&addr);
                }
                Step::Enter(t) => {
                    if !needs_visit(t) {
                        // An immediate / non-term: 0 words.
                        continue;
                    }
                    if policy.is_shared(t) {
                        // The consumer reuses this subterm by identity: 0 words, no descent.
                        continue;
                    }
                    // SAFETY: `t` is a live word that is not an immediate.
                    let Some(frame) = (unsafe { classify(t, policy) }) else {
                        // The global empty-tuple literal, or a policy pricing it at nothing.
                        continue;
                    };
                    let has_children = frame.has_children();
                    let addr = frame.addr();
                    if has_children && !on_path.insert(addr) {
                        // A back-edge: the object is one of its own ancestors.
                        return Sizing::Cyclic(total);
                    }
                    total = total.saturating_add(frame.word_count());
                    if total > limit {
                        return Sizing::OverLimit(total);
                    }
                    if has_children {
                        stack.push(Step::Exit(addr));
                        push_frame_children(&frame, &mut stack);
                    }
                }
            }
        }
        Sizing::Complete(total)
    }

        /// `size_object_x(a, NULL)`: the pre-change unbounded walk.
        pub fn size_object(a: Eterm) -> usize {
            let policy = DefaultSizingPolicy;
            match sizing_walk(a, usize::MAX, &policy) {
                Sizing::Complete(words) | Sizing::OverLimit(words) | Sizing::Cyclic(words) => words,
            }
        }
    }

// ---------------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------------

/// One payload shape, and how many calls per side it is timed for at `scale == 1` (chosen
/// so every bucket costs a comparable wall time whatever its graph size).
struct Bucket {
    label: &'static str,
    term: Eterm,
    calls: usize,
}

/// Owns everything the corpus' interior pointers point into. The fields are never read:
/// the value exists so the heap blocks, the map layouts and the refc payloads stay alive
/// and addressable for as long as the terms that point into them.
#[allow(
    dead_code,
    reason = "an ownership holder: the fields keep the corpus' interior pointers valid"
)]
struct Arena {
    heap: Heap,
    buffers: Vec<Vec<Eterm>>,
    payload: Box<[u8; 64 * 64]>,
}

fn corpus() -> (Arena, Vec<Bucket>) {
    // 20 000 cells x (2 + 5) words for the message lists, plus the deep list's 200 000.
    let mut heap = Heap::new(1 << 20);
    let mut buffers: Vec<Vec<Eterm>> = Vec::new();
    let mut buckets: Vec<Bucket> = Vec::new();
    let payload = Box::new([0xA5u8; 64 * 64]);

    // --- small: per-call overhead (a 2-word tuple) --------------------------
    let small = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
    buckets.push(Bucket {
        label: "small",
        term: small,
        calls: 200_000,
    });

    // --- message: a tuple of two long lists of 4-word tuples ---------------
    // The shape a large `send` moves: cons cells over small tuples, so nearly every object
    // on the DFS path is compound and the path set has to hold the whole spine.
    fn message_list(heap: &mut Heap, cells: usize) -> Eterm {
        let mut list = NIL;
        for i in 0..cells {
            let tuple = heap
                .tuple(&[
                    make_small(i as i64),
                    make_small((i * 3) as i64),
                    make_small((i * 5) as i64),
                    make_small((i * 7) as i64),
                ])
                .unwrap();
            list = heap.cons(tuple, list).unwrap();
        }
        list
    }
    let list_a = message_list(&mut heap, 20_000);
    let list_b = message_list(&mut heap, 20_000);
    let message = heap.tuple(&[list_a, list_b]).unwrap();
    buckets.push(Bucket {
        label: "message-40k",
        term: message,
        calls: 500,
    });

    // --- deep: a 100 000-cell list of small ints (immediate-heavy) ----------
    let mut deep = NIL;
    for i in 0..100_000i64 {
        deep = heap.cons(make_small(i), deep).unwrap();
    }
    buckets.push(Bucket {
        label: "deep-list-100k",
        term: deep,
        calls: 500,
    });

    // --- hamt: a 1 000-key map (the copy path's other common payload) ------
    {
        let pairs: Vec<(Eterm, Eterm)> = (1..=1_000i64)
            .map(|k| (make_small(k), make_small(k * 7)))
            .collect();
        let atoms = AtomTable::new();
        let mut buf = vec![Eterm::from_raw(0); map_words_capacity(pairs.len())];
        // SAFETY: `buf` owns exactly the `map_words_capacity` words `write_map` needs and
        // outlives the term (kept in `Arena::buffers`).
        let map = unsafe { write_map(buf.as_mut_ptr(), &pairs, &atoms) }.unwrap();
        buckets.push(Bucket {
            label: "hamt-1000",
            term: map,
            calls: 20_000,
        });
        buffers.push(buf);
    }

    // --- refc: a list of tuples each holding an 8-word refc binary pair ----
    {
        let mut list = NIL;
        for i in 0..1_000usize {
            // Each binary points at its own 64-byte window of the shared payload.
            let window = payload.as_ptr().wrapping_add(i * 64);
            // SAFETY: 8 words are reserved for the `[BinRef][ErlSubBits]` pair; `window`
            // addresses `payload`, which outlives every term (it is in `Arena`).
            let binary = unsafe {
                let hp = heap.alloc_words(ERL_REFC_BITS_SIZE).unwrap();
                write_refc_binary(hp, window as *mut Eterm, core::ptr::null_mut(), window, 0, 64)
            };
            let tuple = heap.tuple(&[make_small(i as i64), binary]).unwrap();
            list = heap.cons(tuple, list).unwrap();
        }
        buckets.push(Bucket {
            label: "refc-1000",
            term: list,
            calls: 20_000,
        });
    }

    (
        Arena {
            heap,
            buffers,
            payload,
        },
        buckets,
    )
}

// ---------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------

/// Which walk(s) a run times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    All,
    Retired,
    Fresh,
    Scratch,
}

impl Mode {
    fn wants_retired(self) -> bool {
        matches!(self, Mode::All | Mode::Retired)
    }

    fn wants_fresh(self) -> bool {
        matches!(self, Mode::All | Mode::Fresh)
    }

    fn wants_scratch(self) -> bool {
        matches!(self, Mode::All | Mode::Scratch)
    }
}

#[derive(Default, Clone, Copy)]
struct Side {
    nanos: u128,
    calls: usize,
}

impl Side {
    fn add(&mut self, nanos: u128) {
        self.nanos += nanos;
        self.calls += 1;
    }

    /// `NaN` when the side was not timed (a single-mode run).
    fn ns_per_call(&self) -> f64 {
        if self.calls == 0 {
            f64::NAN
        } else {
            self.nanos as f64 / self.calls as f64
        }
    }

    fn show(&self) -> String {
        let ns = self.ns_per_call();
        if ns.is_nan() {
            "n/a".to_string()
        } else {
            format!("{ns:.1}")
        }
    }
}

/// The three sides of one bucket.
#[derive(Default, Clone, Copy)]
struct Triple {
    retired: Side,
    fresh: Side,
    scratch: Side,
}

/// Times `bucket` `calls` times per side, interleaved per call with a rotating order (so
/// no side always runs on a freshly warmed cache), and warm at the start.
fn bench_bucket(bucket: &Bucket, calls: usize, mode: Mode) -> Triple {
    let policy = DefaultSizingPolicy;
    let mut scratch = SizingScratch::new();
    let mut triple = Triple::default();

    // Warm-up outside the windows.
    black_box(retired::size_object(bucket.term));
    black_box(size_object_with(bucket.term, &policy));
    black_box(size_object_with_scratch(
        bucket.term,
        &policy,
        &mut scratch,
    ));

    // The six cyclic rotations of `(retired, fresh, scratch)`, and we walk through the
    // first three of them: `i % 3` picks a rotation, so every side is first, second and
    // third equally often.
    const ORDERS: [[u8; 3]; 3] = [[0, 1, 2], [1, 2, 0], [2, 0, 1]];
    for i in 0..calls {
        for which in ORDERS[i % 3] {
            match which {
                0 if mode.wants_retired() => {
                    let t = Instant::now();
                    black_box(retired::size_object(bucket.term));
                    triple.retired.add(t.elapsed().as_nanos());
                }
                1 if mode.wants_fresh() => {
                    let t = Instant::now();
                    black_box(size_object_with(bucket.term, &policy));
                    triple.fresh.add(t.elapsed().as_nanos());
                }
                2 if mode.wants_scratch() => {
                    let t = Instant::now();
                    black_box(size_object_with_scratch(
                        bucket.term,
                        &policy,
                        &mut scratch,
                    ));
                    triple.scratch.add(t.elapsed().as_nanos());
                }
                _ => {}
            }
        }
    }
    // Keep the scratch observably used so nothing above can be elided.
    black_box(&scratch);
    triple
}

/// The deterministic per-call allocation counts `(retired, fresh, scratch)`.
fn alloc_counts(term: Eterm) -> (usize, usize, usize) {
    let policy = DefaultSizingPolicy;
    let mut scratch = SizingScratch::new();
    // Warm the scratch first, so its figure is the *steady state* one.
    black_box(size_object_with_scratch(term, &policy, &mut scratch));
    let (retired, _, _) = allocations_in(|| black_box(retired::size_object(term)));
    let (fresh, _, _) = allocations_in(|| black_box(size_object_with(term, &policy)));
    let (scratch, _, _) = allocations_in(|| {
        black_box(size_object_with_scratch(term, &policy, &mut scratch))
    });
    (retired, fresh, scratch)
}

fn load() -> String {
    let s = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    s.split_whitespace().next().unwrap_or("?").to_string()
}

fn fmt_ratio(a: f64, b: f64) -> String {
    if b == 0.0 || a.is_nan() || b.is_nan() {
        "n/a".to_string()
    } else {
        format!("{:.3}x", a / b)
    }
}

fn usage() -> ! {
    eprintln!("usage: size_walk_bench [scale] [all|retired|fresh|scratch] [bucket]");
    std::process::exit(2);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let scale: usize = args
        .next()
        .and_then(|a| a.parse().ok())
        .unwrap_or(1)
        .max(1);
    let mode = match args.next().as_deref() {
        None | Some("all") => Mode::All,
        Some("retired") => Mode::Retired,
        Some("fresh") => Mode::Fresh,
        Some("scratch") => Mode::Scratch,
        Some(_) => usage(),
    };
    let only: Option<String> = args.next();
    if args.next().is_some() {
        usage();
    }

    let load_before = load();
    let (_arena, all_buckets) = corpus();
    let buckets: Vec<&Bucket> = all_buckets
        .iter()
        .filter(|b| match &only {
            None => true,
            Some(label) => {
                if b.label == label {
                    true
                } else {
                    false
                }
            }
        })
        .collect();
    if buckets.is_empty() {
        eprintln!(
            "no bucket matches `{}` (have: {})",
            only.unwrap_or_default(),
            all_buckets
                .iter()
                .map(|b| b.label)
                .collect::<Vec<_>>()
                .join(", ")
        );
        std::process::exit(2);
    }

    // A correctness guard before anything is timed: all three sides must count the same
    // graph. A mismatch means the timing would compare different work, so it aborts.
    {
        let policy = DefaultSizingPolicy;
        let mut scratch = SizingScratch::new();
        for bucket in &buckets {
            let r = retired::size_object(bucket.term);
            let f = size_object_with(bucket.term, &policy);
            let s = size_object_with_scratch(bucket.term, &policy, &mut scratch);
            assert_eq!(
                (r, f),
                (s, s),
                "{}: all three walks must count the same graph",
                bucket.label
            );
        }
    }

    println!("size_walk_bench: mode={mode:?} scale={scale} loadavg_before={load_before}");
    println!(
        "{:>15}  {:>9}  {:>7}  {:>11}  {:>11}  {:>11}  {:>15}  {:>13}  {:>12}",
        "bucket",
        "words",
        "calls",
        "retired",
        "fresh",
        "scratch",
        "retired/scratch",
        "fresh/scratch",
        "allocs r/f/s"
    );

    // Totals over the timed corpus, weighted by calls.
    let mut total = Triple::default();
    for bucket in &buckets {
        let calls = bucket.calls.saturating_mul(scale);
        let (alloc_r, alloc_f, alloc_s) = alloc_counts(bucket.term);
        let triple = bench_bucket(bucket, calls, mode);
        let words = size_object(bucket.term);
        println!(
            "{:>15}  {:>9}  {:>7}  {:>11}  {:>11}  {:>11}  {:>15}  {:>13}  {:>12}",
            bucket.label,
            words,
            triple.retired.calls.max(triple.fresh.calls).max(triple.scratch.calls),
            triple.retired.show(),
            triple.fresh.show(),
            triple.scratch.show(),
            fmt_ratio(triple.retired.ns_per_call(), triple.scratch.ns_per_call()),
            fmt_ratio(triple.fresh.ns_per_call(), triple.scratch.ns_per_call()),
            format!("{alloc_r}/{alloc_f}/{alloc_s}"),
        );
        total.retired.nanos += triple.retired.nanos;
        total.retired.calls += triple.retired.calls;
        total.fresh.nanos += triple.fresh.nanos;
        total.fresh.calls += triple.fresh.calls;
        total.scratch.nanos += triple.scratch.nanos;
        total.scratch.calls += triple.scratch.calls;
    }
    println!(
        "{:>15}  {:>9}  {:>7}  {:>11}  {:>11}  {:>11}  {:>15}  {:>13}  {:>12}",
        "ALL",
        "",
        total.retired.calls.max(total.fresh.calls).max(total.scratch.calls),
        total.retired.show(),
        total.fresh.show(),
        total.scratch.show(),
        fmt_ratio(total.retired.ns_per_call(), total.scratch.ns_per_call()),
        fmt_ratio(total.fresh.ns_per_call(), total.scratch.ns_per_call()),
        "",
    );
    println!("size_walk_bench: loadavg_after={}", load());
}
