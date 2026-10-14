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

//! The in-crate memory characterisation of the `copy_bound/ets_copy` loop shape.
//!
//! `tools/bench`'s `ets_copy` case performs, per op, `T = payload()`,
//! `ets:insert(Tab, {k, T})`, `ets:lookup(Tab, k)` against **one** `set` table with
//! **one** key, and its rust `beam_rt` peak RSS reaches 90.30 MiB at `N = 100 000`
//! against the oracle's 66.26 MiB (`tools/bench/RESULTS.md` §6.7 / §8 item 5). This
//! file isolates the part of that loop this crate owns — the ETS adopt/copy
//! (`TermStore::copy` / `free`) and read paths — from the caller-side BIF-result
//! copy-out seam the siblings (`beam_bifs`, `beam_rt`) own.
//!
//! It drives the **exact shape** (the same payload term the workload's
//! `payload/0` builds, `{k, Payload}` as the object, `lookup(k)` on every op) twice:
//!
//! * **directly through [`TermStore`]** — the pure copy boundary: `copy` the object,
//!   `free` the copy it replaces. [`TermStore::words`] and [`TermStore::blocks`]
//!   must return to the same value on every checkpoint (the arena reuses the freed
//!   runs instead of bumping new blocks).
//! * **through the public [`Table`] API** — the full insert/lookup loop (index
//!   included), where [`Table::memory`] must stay flat and `size` must stay `1`.
//!
//! Both phases also sample the **process** peak RSS (`/proc/self/status` `VmHWM`)
//! and the current RSS (`VmRSS`) at each checkpoint, so the file answers "is the
//! growth in this crate or in a sibling?" with numbers rather than an assumption.
//!
//! # One test per binary, deliberately
//!
//! `VmHWM` is a *process-wide* high-water mark, so a second test running on another
//! thread could raise it under this one. The whole measurement therefore lives in
//! the single test below, and the crate's `CONTEXT.md` records the run command.
//!
//! Run it with output:
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_ets \
//!     --test ets_memory -- --nocapture
//! ```
//! `BEAM_ETS_MEMORY_N` overrides the loop count (default 100 000, the bench's rust
//! `N` for `ets_copy`), so the trajectory can be sampled at several sizes.
//!
//! The figures of every run are recorded in the crate's `PERF.md`.

mod common;

use std::env;
use std::fs;

use beam_ets::{Table, TableId, TableKind, TableOptions, TermStore};
use beam_term::prelude::*;

// ---------------------------------------------------------------------------
// The bench workload's payload (`tools/bench/src/workloads/copy_bound.rs`,
// `module_source`). Mirrored here so the two sides cannot drift silently: the
// Erlang `payload/0` is generated from those same constants.
// ---------------------------------------------------------------------------

/// The number of list elements in the copied payload.
const PAYLOAD_LIST_LEN: i64 = 10;
/// The number of bytes in the copied payload's binary.
const PAYLOAD_BIN_BYTES: u8 = 12;
/// The copied payload's float.
const PAYLOAD_FLOAT: f64 = 3.5;
/// The copied payload's char-list string.
const PAYLOAD_STRING: &str = "a char list";
/// The copied payload's negative small integer.
const PAYLOAD_NEG: i64 = -42;
/// The atom naming the single key the loop writes and reads.
const KEY_ATOM: &str = "k";

/// The loop count (the bench's rust `ets_copy` `N` unless overridden).
fn loop_count() -> usize {
    match env::var("BEAM_ETS_MEMORY_N") {
        Ok(value) => value
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("BEAM_ETS_MEMORY_N is not a number: {value:?}")),
        Err(_) => 100_000,
    }
}

/// A `/proc/self/status` field in KiB (`None` off Linux or if the field is absent).
fn status_kib(field: &str) -> Option<u64> {
    let text = fs::read_to_string("/proc/self/status").ok()?;
    let prefix = format!("{field}:");
    text.lines().find_map(|line| {
        let rest = line.strip_prefix(&prefix)?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

/// `(VmRSS, VmHWM)`, both in KiB — the current and peak resident set size.
fn rss() -> (Option<u64>, Option<u64>) {
    (status_kib("VmRSS"), status_kib("VmHWM"))
}

/// The MiB rendering of a KiB figure.
fn mib(kib: u64) -> f64 {
    kib as f64 / 1024.0
}

/// One checkpoint: the op index plus everything measured there.
struct Sample {
    op: usize,
    /// [`Table::memory`] — table-owned words plus the interior's index words.
    table_memory: usize,
    /// [`TermStore::words`] — live (non-freed) table-owned words.
    store_words: usize,
    /// [`TermStore::blocks`] — the arena blocks the store owns.
    store_blocks: usize,
    /// Current resident set size, KiB.
    rss_kib: Option<u64>,
    /// Peak resident set size, KiB.
    hwm_kib: Option<u64>,
}

/// The source arena the payload term is built on.
///
/// The workload builds a **fresh** payload per op, so the arena is rewound per op:
/// the source is the caller's (out of this crate's scope) and is flat by
/// construction, which is what keeps the measurement attributable to the table.
struct Source {
    heap: Heap,
    atoms: AtomTable,
}

impl Source {
    fn new() -> Source {
        // 4096 words is ample for the payload (~70 words) and never reallocates.
        Source {
            heap: Heap::new(4096),
            atoms: AtomTable::new(),
        }
    }

    /// An on-heap binary (`ErlHeapBits`) on the source arena.
    fn binary(&mut self, bytes: &[u8]) -> Eterm {
        // SAFETY: `alloc_words` hands back exactly `heap_bits_size_words(n_bits)`
        // uninitialised words for the block `write_binary` fills, and the arena
        // outlives the term it returns.
        unsafe {
            let hp = self
                .heap
                .alloc_words(heap_bits_size_words(bytes.len() * 8))
                .expect("the source arena has room for the binary");
            write_binary(hp, bytes).expect("a byte-aligned binary")
        }
    }

    /// A float term on the source arena.
    fn float(&mut self, value: f64) -> Eterm {
        // SAFETY: `FLOAT_SIZE_WORDS` is exactly what `[HEADER_FLONUM, bits]` needs
        // and `write_float` fills both words before the term escapes.
        unsafe {
            let hp = self
                .heap
                .alloc_words(FLOAT_SIZE_WORDS)
                .expect("the source arena has room for the float");
            write_float(hp, value)
        }
    }

    /// A flat map on the source arena, built with the frozen
    /// `beam_term::map::write_map` the runtime itself uses.
    fn map(&mut self, pairs: &[(Eterm, Eterm)]) -> Eterm {
        let capacity = map_words_capacity(pairs.len());
        // SAFETY: `alloc_words(capacity)` hands back exactly the `capacity`
        // exclusively-owned, writable words `write_map` requires and initialises in
        // full; the arena outlives the map.
        unsafe {
            let hp = self
                .heap
                .alloc_words(capacity)
                .expect("the source arena has room for the map");
            write_map(hp, pairs, &self.atoms).expect("a well-formed flat map")
        }
    }

    /// The workload's `payload/0`: `#{a => […], b => {ok, <<…>>, Float},
    /// c => <<…>>, d => "…", e => -42}`.
    fn payload(&mut self) -> Eterm {
        let mut list = NIL;
        for n in (1..=PAYLOAD_LIST_LEN).rev() {
            list = self
                .heap
                .cons(make_small(n), list)
                .expect("the source arena has room for the list");
        }
        let ok = make_atom(self.atoms.intern("ok"));
        let binary = {
            let bytes: Vec<u8> = (1..=PAYLOAD_BIN_BYTES).collect();
            self.binary(&bytes)
        };
        let float = self.float(PAYLOAD_FLOAT);
        let inner = self
            .heap
            .tuple(&[ok, binary, float])
            .expect("the source arena has room for the tuple");
        let c = {
            let bytes: Vec<u8> = (1..=PAYLOAD_BIN_BYTES).collect();
            self.binary(&bytes)
        };
        let mut string = NIL;
        for ch in PAYLOAD_STRING.bytes().rev() {
            string = self
                .heap
                .cons(make_small(i64::from(ch)), string)
                .expect("the source arena has room for the string");
        }
        let pairs: [(Eterm, Eterm); 5] = [
            (make_atom(self.atoms.intern("a")), list),
            (make_atom(self.atoms.intern("b")), inner),
            (make_atom(self.atoms.intern("c")), c),
            (make_atom(self.atoms.intern("d")), string),
            (make_atom(self.atoms.intern("e")), make_small(PAYLOAD_NEG)),
        ];
        self.map(&pairs)
    }

    /// The workload's object: `{k, payload()}` — a `keypos` 1 `set` object whose
    /// key is the atom `k` and whose value is the payload map.
    fn object(&mut self, key: Eterm) -> Eterm {
        self.heap.reset();
        let payload = self.payload();
        self.heap
            .tuple(&[key, payload])
            .expect("the source arena has room for the object")
    }

    /// The `lookup/2` key of the workload's object.
    fn key(&mut self) -> Eterm {
        make_atom(self.atoms.intern(KEY_ATOM))
    }
}

/// The op indices at which a sample is taken: roughly quarter-way through the loop,
/// plus the first and last op, so a *rise* is distinguishable from a constant.
fn checkpoints(n: usize) -> Vec<usize> {
    let mut points = vec![1];
    for divisor in [4, 2] {
        points.push((n / divisor).max(1));
    }
    points.push((n * 3 / 4).max(1));
    points.push(n);
    points.sort_unstable();
    points.dedup();
    points
}

/// The whole measurement — see the module docs for why it is a single test.
#[test]
fn ets_copy_shaped_loop_keeps_the_in_crate_footprint_flat_and_its_rss_growth_at_zero() {
    let n = loop_count();
    let loadavg = fs::read_to_string("/proc/loadavg").unwrap_or_default();
    let (rss_start, hwm_start) = rss();

    let mut source = Source::new();
    let key = source.key();

    println!(
        "[ets_memory] N={n} loadavg={} start: VmRSS={:?} KiB VmHWM={:?} KiB",
        loadavg.trim(),
        rss_start,
        hwm_start
    );

    // ---------------------------------------------------------------------
    // Phase A — the copy boundary alone (`TermStore::copy` + `TermStore::free`)
    // ---------------------------------------------------------------------
    let mut store = TermStore::with_default_allocator();
    let mut samples: Vec<Sample> = Vec::new();
    let points = checkpoints(n);

    // The op order mirrors `Table::insert` -> `insert_stored`: the replacement is
    // copied **before** the object it replaces is freed, so the store can never
    // reuse the outgoing object's words for the incoming one.
    let mut current = store
        .copy(source.object(key))
        .expect("the payload object is within the copied subset");
    for op in 1..=n {
        let next = store
            .copy(source.object(key))
            .expect("the payload object is within the copied subset");
        let reclaimed = store.free(current);
        assert!(
            reclaimed > 0,
            "op {op}: freeing the replaced copy reclaimed nothing"
        );
        current = next;
        if points.contains(&op) {
            let (rss_kib, hwm_kib) = rss();
            samples.push(Sample {
                op,
                table_memory: 0,
                store_words: store.words(),
                store_blocks: store.blocks(),
                rss_kib,
                hwm_kib,
            });
        }
    }
    println!("[ets_memory] phase A — TermStore copy/free, one key upserted {n} times");
    for sample in &samples {
        println!(
            "  op={:>7} store.words={:>6} store.blocks={:>3} VmRSS={:?} KiB VmHWM={:?} KiB",
            sample.op, sample.store_words, sample.store_blocks, sample.rss_kib, sample.hwm_kib
        );
    }
    let first = &samples[0];
    let last = samples.last().expect("at least one checkpoint");
    assert_eq!(
        last.store_words, first.store_words,
        "the store's live-word count grew over the loop ({} -> {})",
        first.store_words, last.store_words
    );
    assert_eq!(
        last.store_blocks, first.store_blocks,
        "the store mapped a fresh arena block over the loop ({} -> {})",
        first.store_blocks, last.store_blocks
    );

    // ---------------------------------------------------------------------
    // Phase B — the public `Table` API (insert + lookup per op, index included)
    // ---------------------------------------------------------------------
    let mut opts = TableOptions::new();
    opts.set_kind(TableKind::Set);
    let mut table = Table::new(TableId::next(), opts).expect("a fresh table is constructible");

    // Warm up so the arena's block chain and the free lists have reached their
    // steady state before the first checkpoint (the first upserts bump).
    for _ in 0..1000 {
        table
            .insert(source.object(key), &source.atoms)
            .expect("the object is insertable");
    }
    let floor = table.memory();

    let mut samples: Vec<Sample> = Vec::new();
    for op in 1..=n {
        let object = source.object(key);
        table
            .insert(object, &source.atoms)
            .expect("the object is insertable");
        let found = table.lookup(key, &source.atoms);
        assert_eq!(found.len(), 1, "op {op}: the single key must read back as one object");
        // The workload's own validation: a bound-variable pattern is a real
        // equality test, so a corrupted copy fails here instead of being measured.
        assert!(
            eq(found[0], object, &source.atoms),
            "op {op}: the read-back object is not equal to the inserted one"
        );
        assert_eq!(table.size(), 1, "op {op}: the table must hold exactly one key");
        if points.contains(&op) {
            let (rss_kib, hwm_kib) = rss();
            samples.push(Sample {
                op,
                table_memory: table.memory(),
                store_words: 0,
                store_blocks: 0,
                rss_kib,
                hwm_kib,
            });
        }
    }
    println!(
        "[ets_memory] phase B — Table insert+lookup, one key upserted {n} times (memory floor {floor} words)"
    );
    for sample in &samples {
        println!(
            "  op={:>7} table.memory={:>6} VmRSS={:?} KiB VmHWM={:?} KiB",
            sample.op, sample.table_memory, sample.rss_kib, sample.hwm_kib
        );
    }
    let first = &samples[0];
    let last = samples.last().expect("at least one checkpoint");
    assert_eq!(
        last.table_memory, first.table_memory,
        "the table's own footprint grew over the loop ({} -> {})",
        first.table_memory, last.table_memory
    );
    assert_eq!(
        first.table_memory, floor,
        "the table's footprint left its warm-up floor"
    );

    // ---------------------------------------------------------------------
    // The process-level reading: reported, and bounded.
    // ---------------------------------------------------------------------
    let (rss_end, hwm_end) = rss();
    let rss_growth = rss_end
        .zip(rss_start)
        .map(|(end, start)| end.saturating_sub(start));
    let hwm_growth = hwm_end
        .zip(hwm_start)
        .map(|(end, start)| end.saturating_sub(start));
    println!(
        "[ets_memory] summary N={n} table.memory={} words flat; process VmRSS {rss_start:?} -> {rss_end:?} KiB, \
         VmHWM {hwm_start:?} -> {hwm_end:?} KiB (growth {rss_growth:?} / {hwm_growth:?} KiB)",
        first.table_memory
    );
    if let Some(growth) = rss_growth {
        println!(
            "[ets_memory] process VmRSS growth over {n} ops = {growth} KiB = {:.2} MiB",
            mib(growth)
        );
    }
    // The measured RSS growth is ~0 for this crate's path (PERF.md records the
    // figures). The bound is deliberately loose: it is a *regression guard* that
    // catches an in-crate per-op leak, not a precision claim about the allocator.
    if let Some(growth) = rss_growth {
        assert!(
            growth < 16 * 1024,
            "the ets_copy-shaped loop grew the process by {growth} KiB over {n} ops; \
             an in-crate per-op leak is the only explanation for a step of that size"
        );
    }
}
