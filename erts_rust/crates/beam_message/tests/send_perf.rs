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

//! The **real per-send cost** of this crate's copy boundary: `beam_message::send`.
//!
//! [`tests/perf.rs`](perf) measures the copy engine in isolation (`copy_struct` /
//! `copy_struct_simple`); this file measures what a host actually calls for a process-to-process
//! message — the whole [`send`] path, copy **plus** the destination off-heap table drain and the
//! [`Message`] envelope build (`enqueue_copied`) — for the shapes a runtime really sends.
//!
//! Run it with output shown:
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_message \
//!     --test send_perf -- --nocapture
//! ```
//!
//! # The two destination kinds (the one axis that matters here)
//!
//! Every shape is sent into **two** destinations with **identical free head-room**
//! (`size_of_copy(term) + HEADROOM` words still available in both):
//!
//! * **`clean`** — a freshly constructed heap, so `used() == 0`;
//! * **`dirty`** — one word handed out first, so `used() > 0`.
//!
//! Both get a fresh, **empty** staging [`OffHeapTable`] (which is what
//! `beam_process::Process::copy_message` passes). A dirty receiver heap is the *real* case: the
//! destination is `Process`'s long-lived young heap, which is non-empty after the first message, so
//! the copier's single-pass fast path only ever fires for a dirty destination in practice.
//!
//! The printed `clean → dirty` delta is therefore exactly the Pass-1 (sizing/preflight)
//! traversal's share when the fast path is gated on `used() == 0`, and ~0 when the gate is only the
//! off-heap table's emptiness.
//!
//! # Correctness only, never a timing threshold
//!
//! `std::time::Instant` only (no `criterion`, no new dependency). The warm-up send of each row
//! asserts the enqueued payload is structurally `=:=` the original, that exactly one message was
//! enqueued, and that `used()` advanced by exactly `size_of_copy(term)`; the timed loop re-asserts
//! the word count and re-checks equality on its first iteration. **No** assertion here is a timing
//! threshold, so the measurement can never fail spuriously on a loaded host. The printed numbers
//! (and `/proc/loadavg`) are the deliverable.
//!
//! # The shapes
//!
//! * `small tuple` — a 3-element tuple, the smallest realistic message;
//! * `10-int list` — 10 cons cells of immediates;
//! * `8 x 32-byte binaries` — a list of four **refc** binaries (payload off-heap, shared by
//!   reference through the destination table) and four **on-heap** small binaries, so both binary
//!   shapes cross the boundary;
//! * `HAMT map (64 keys)` — past `MAP_SMALL_MAP_LIMIT`, so a trie of many small nodes;
//! * `send_recv payload` — the **exact** payload of the `copy_bound` benchmark workload
//!   (`tools/bench/src/workloads/copy_bound.rs`): a 5-key flat map holding a 10-element int list,
//!   an `{ok, <<"hello world">>, 3.5}` tuple, a 12-byte binary, a char list and `-42`.

use std::time::{Duration, Instant};

use beam_alloc::{Heap, RefcBinary};
use beam_message::off_heap::OffHeapTable;
use beam_message::{send, size_of_copy, CopyContext, Queue};
use beam_term::compare;
use beam_term::prelude::*;

/// The source process: the heap the shapes live on, its atom table, and the off-heap table that
/// owns the payloads the refc binaries share.
struct Src {
    heap: Heap,
    atoms: AtomTable,
    off: OffHeapTable,
}

impl Src {
    fn new() -> Src {
        Src {
            heap: Heap::new(1 << 16).expect("source heap"),
            atoms: AtomTable::new(),
            off: OffHeapTable::new(),
        }
    }
}

/// The free head-room every destination keeps beyond the payload's own words, so the `clean` and
/// `dirty` rows are measured on equal terms: both have exactly `words + HEADROOM` words available.
const HEADROOM: usize = 64;

/// How the destination heap is staged before the timed [`send`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum DestKind {
    /// `used() == 0`: the heap has handed out no word.
    Clean,
    /// One word already handed out, so `used() > 0` — the shape a long-lived receiver heap has.
    Dirty,
}

impl DestKind {
    /// The words consumed before the copy, so `used() > 0` for a dirty destination.
    fn precharge(self) -> usize {
        match self {
            DestKind::Clean => 0,
            DestKind::Dirty => 1,
        }
    }

    fn name(self) -> &'static str {
        match self {
            DestKind::Clean => "clean",
            DestKind::Dirty => "dirty",
        }
    }
}

// ---------------------------------------------------------------------------
// Shapes
// ---------------------------------------------------------------------------

/// Builds the list `[e0, e1, ...]` on `heap`, tail-first.
fn build_list(heap: &mut Heap, elems: &[Eterm]) -> Eterm {
    let mut acc = NIL;
    for &elem in elems.iter().rev() {
        acc = heap.cons(elem, acc).expect("the cons fits");
    }
    acc
}

/// Writes a flat map (or HAMT map, past `MAP_SMALL_MAP_LIMIT`) on `heap`.
fn write_map_term(heap: &mut Heap, pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> Eterm {
    // SAFETY: `map_words_capacity` is a sound upper bound on the words `write_map` writes.
    unsafe {
        let hp = heap
            .alloc_words(map_words_capacity(pairs.len()))
            .expect("the map fits");
        write_map(hp, pairs, atoms).expect("the map builds")
    }
}

/// Writes a byte-aligned on-heap bitstring of `bytes` (the shape ERTS keeps on the heap).
fn heap_bits(heap: &mut Heap, bytes: &[u8]) -> Eterm {
    let bits = bytes.len() * 8;
    // SAFETY: `heap_bits_size_words(bits)` words are reserved and `bytes` covers `bits` bits.
    unsafe {
        let hp = heap
            .alloc_words(heap_bits_size_words(bits))
            .expect("the bitstring fits");
        write_heap_bits(hp, bytes, bits).expect("the payload fits")
    }
}

/// Writes a boxed float (`[HEADER_FLONUM, bits]`, 2 words).
fn boxed_float(heap: &mut Heap, value: f64) -> Eterm {
    // SAFETY: `FLOAT_SIZE_WORDS` (2) words are reserved for the block.
    unsafe {
        let hp = heap.alloc_words(FLOAT_SIZE_WORDS).expect("the float fits");
        write_float(hp, value)
    }
}

/// Registers a payload in `off` and writes the 8-word `[BinRef][ErlSubBits]` refc-binary pair over
/// it on `heap` (a large binary: the payload lives off-heap and the copy shares it by reference).
fn refc_binary(off: &mut OffHeapTable, heap: &mut Heap, bytes: &[u8]) -> Eterm {
    off.push_binary(RefcBinary::new(bytes).expect("the payload allocates"));
    let data_ptr = off.binaries()[off.binaries().len() - 1].data_ptr();
    // SAFETY: `ERL_REFC_BITS_SIZE` (8) words are reserved for the pair; `off` owns the payload,
    // which outlives it.
    unsafe {
        let hp = heap.alloc_words(ERL_REFC_BITS_SIZE).expect("the pair fits");
        write_refc_binary(
            hp,
            data_ptr as *mut Eterm,
            core::ptr::null_mut(),
            data_ptr,
            0,
            bytes.len() * 8,
        )
    }
}

/// The smallest realistic message: a 3-element tuple (header + 3 immediate elements).
fn small_tuple(s: &mut Src) -> Eterm {
    s.heap
        .tuple(&[make_small(1), make_small(2), make_small(3)])
        .expect("the tuple fits")
}

/// A 10-element integer list: 10 cons cells.
fn int_list(s: &mut Src) -> Eterm {
    build_list(&mut s.heap, &(1..=10).map(make_small).collect::<Vec<_>>())
}

/// A list of eight 32-byte binaries — four **refc** (off-heap payload, shared) and four **on-heap**
/// — so both binary shapes cross the boundary.
fn binary_list(s: &mut Src) -> Eterm {
    let mut elems = Vec::with_capacity(8);
    for i in 0..8u8 {
        let bytes: Vec<u8> = (0..32u8).map(|j| i.wrapping_mul(31).wrapping_add(j)).collect();
        if i % 2 == 0 {
            elems.push(refc_binary(&mut s.off, &mut s.heap, &bytes));
        } else {
            elems.push(heap_bits(&mut s.heap, &bytes));
        }
    }
    build_list(&mut s.heap, &elems)
}

/// A 64-key map: past `MAP_SMALL_MAP_LIMIT` (32), so a HAMT trie of many small nodes.
fn hamt_map(s: &mut Src) -> Eterm {
    let pairs: Vec<(Eterm, Eterm)> = (0..64)
        .map(|i| (make_small(i * 3), make_small(i * 1000 + 7)))
        .collect();
    write_map_term(&mut s.heap, &pairs, &s.atoms)
}

/// The **exact** `send_recv` benchmark payload (`tools/bench/src/workloads/copy_bound.rs`): a
/// 5-key flat map holding a 10-element int list, an `{ok, <<"hello world">>, 3.5}` tuple, a
/// 12-byte binary, a char list and `-42`.
fn send_recv_payload(s: &mut Src) -> Eterm {
    let ints = build_list(&mut s.heap, &(1..=10).map(make_small).collect::<Vec<_>>());
    let hello = heap_bits(&mut s.heap, b"hello world");
    let float = boxed_float(&mut s.heap, 3.5);
    let ok = make_atom(s.atoms.intern("ok"));
    let b = s.heap.tuple(&[ok, hello, float]).expect("the tuple fits");
    let c = heap_bits(&mut s.heap, &(1..=12u8).collect::<Vec<u8>>());
    let d = build_list(
        &mut s.heap,
        &"a char list"
            .bytes()
            .map(|ch| make_small(ch as i64))
            .collect::<Vec<_>>(),
    );

    let ka = make_atom(s.atoms.intern("a"));
    let kb = make_atom(s.atoms.intern("b"));
    let kc = make_atom(s.atoms.intern("c"));
    let kd = make_atom(s.atoms.intern("d"));
    let ke = make_atom(s.atoms.intern("e"));
    let pairs = [
        (ka, ints),
        (kb, b),
        (kc, c),
        (kd, d),
        (ke, make_small(-42)),
    ];
    write_map_term(&mut s.heap, &pairs, &s.atoms)
}

/// A shape to send: how to build it, its structural object count, and how many copies to time.
struct Shape {
    name: &'static str,
    build: fn(&mut Src) -> Eterm,
    /// The number of compound destination objects the copy materialises (cons cells, tuples,
    /// binaries, map nodes). For the HAMT map this counts the keys' leaf cells and is therefore an
    /// approximation of the trie's node count.
    objects: usize,
    iterations: u32,
}

/// The measured shapes (see the module docs).
const SHAPES: &[Shape] = &[
    Shape {
        name: "small tuple",
        build: small_tuple,
        objects: 1,
        iterations: 20_000,
    },
    Shape {
        name: "10-int list",
        build: int_list,
        objects: 10,
        iterations: 20_000,
    },
    Shape {
        name: "8 x 32-byte binaries",
        build: binary_list,
        objects: 16,
        iterations: 20_000,
    },
    Shape {
        name: "HAMT map (64 keys)",
        build: hamt_map,
        objects: 65,
        iterations: 5_000,
    },
    Shape {
        name: "send_recv payload",
        build: send_recv_payload,
        objects: 26,
        iterations: 20_000,
    },
];

// ---------------------------------------------------------------------------
// The measurement
// ---------------------------------------------------------------------------

/// A fresh destination of `capacity` words, pre-charged with `precharge` words (0 for a clean
/// heap, 1 for a dirty one), plus the per-send staging off-heap table and message queue.
fn stage(capacity: usize, precharge: usize) -> (Heap, OffHeapTable, Queue) {
    let mut heap = Heap::new(capacity).expect("destination heap");
    if precharge > 0 {
        // SAFETY: the precharge word is inside this heap's own region and is never read; it exists
        // only to make `used() > 0`, i.e. the destination dirty.
        unsafe { heap.alloc_words(precharge).expect("the precharge fits") };
    }
    (heap, OffHeapTable::new(), Queue::new())
}

/// Times `iterations` real [`send`]s of `term` into a destination of `dest`'s kind, asserting
/// correctness (never a timing threshold) and returning `(mean_ns, best_ns)` per send.
fn measure(
    label: &str,
    dest: DestKind,
    term: Eterm,
    src: &Src,
    iterations: u32,
) -> (f64, u128) {
    let words = size_of_copy(term);
    let pre = dest.precharge();
    // Identical free head-room in both kinds: `capacity - pre == words + HEADROOM`.
    let capacity = words + HEADROOM + pre;
    let from = make_small(1);

    // Warm up — and validate the payload the send actually enqueued. This is the one correctness
    // assertion the timing may never be allowed to game: a fast-but-wrong send fails it.
    {
        let (mut heap, mut dst_off, mut queue) = stage(capacity, pre);
        {
            let mut ctx = CopyContext::new(&src.off, &mut dst_off);
            send(from, term, &mut heap, &mut queue, &mut ctx).expect("the send succeeds");
        }
        let payload = queue.peek().expect("one queued message").payload();
        assert!(
            compare::eq(term, payload, &src.atoms),
            "{label}: the enqueued payload must be =:= the original"
        );
        assert_eq!(queue.len(), 1, "{label}: exactly one message is enqueued");
        assert_eq!(
            heap.used(),
            pre + words,
            "{label}: the send advances `used` by exactly size_of_copy(term)"
        );
    }

    let mut total = Duration::ZERO;
    let mut best = Duration::MAX;
    for i in 0..iterations {
        let (mut heap, mut dst_off, mut queue) = stage(capacity, pre);
        let start = Instant::now();
        {
            let mut ctx = CopyContext::new(&src.off, &mut dst_off);
            send(from, term, &mut heap, &mut queue, &mut ctx).expect("the send succeeds");
        }
        let elapsed = start.elapsed();
        total += elapsed;
        if elapsed < best {
            best = elapsed;
        }

        let payload = queue.peek().expect("one queued message").payload();
        if i == 0 {
            assert!(
                compare::eq(term, payload, &src.atoms),
                "{label}: the timed copy must be correct"
            );
        }
        assert_eq!(
            heap.used(),
            pre + words,
            "{label}: the copy writes exactly the sized count after the pre-charge"
        );
        // Keep the copied payload observable so the send cannot be optimised away.
        std::hint::black_box(payload);
    }

    (total.as_nanos() as f64 / f64::from(iterations), best.as_nanos())
}

/// `/proc/loadavg`, or a marker when the host does not expose it (the numbers are meaningless
/// without it, so the report names it).
fn loadavg() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "<unavailable>".to_string())
}

#[test]
fn report_send_cost_per_shape() {
    println!("send_perf: /proc/loadavg at start = {}", loadavg());
    println!(
        "{:<26} {:<6} {:>7} {:>8} {:>12} {:>12} {:>8}",
        "shape", "dest", "words", "objects", "mean_ns/op", "best_ns/op", "ns/word"
    );

    for shape in SHAPES {
        let mut src = Src::new();
        let term = (shape.build)(&mut src);
        let words = size_of_copy(term);
        assert!(words > 0, "{}: every shape is a compound term", shape.name);

        let mut means = [0f64; 2];
        for (index, dest) in [DestKind::Clean, DestKind::Dirty].into_iter().enumerate() {
            let label = format!("{} [{}]", shape.name, dest.name());
            let (mean, best) = measure(&label, dest, term, &src, shape.iterations);
            means[index] = mean;
            println!(
                "{:<26} {:<6} {:>7} {:>8} {:>12.1} {:>12} {:>8.2}",
                shape.name,
                dest.name(),
                words,
                shape.objects,
                mean,
                best,
                mean / words as f64
            );
        }

        // The clean->dirty delta: the Pass-1 (sizing) traversal's share. It is the cost the copy
        // engine adds on top of the writing traversal *only* when its fast path is gated on
        // `used() == 0`; with the off-heap table as the only gate it collapses to measurement noise.
        let delta = means[1] - means[0];
        println!(
            "{:<26} {:<6} {:>7} {:>8} {:>12.1} {:>12} {:>8}",
            shape.name,
            "delta",
            words,
            shape.objects,
            delta,
            "-",
            format!("{:>5.0}%", 100.0 * delta / means[0])
        );
    }

    println!("send_perf: /proc/loadavg at end = {}", loadavg());
}
