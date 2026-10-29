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
//! A reproducible micro-benchmark for the **single-key map writers** —
//! `maps:put/3`, `maps:update/3`, `maps:remove/2` and `maps:take/2`, the four BIFs
//! that `crate::maps` drives through its `cow_put`/`cow_remove` helpers — against the
//! `maps:from_list/1` **rebuild**, i.e. the Θ(n log n) builder the update BIFs no
//! longer use.
//!
//! Absolute latencies on this shared host are meaningless on their own: read the
//! **ratio** between rows of the same run, and always report `/proc/loadavg`
//! alongside the numbers (printed before and after the run).
//!
//! ```text
//! erts_rust/scripts/cargo.sh run --release --manifest-path erts_rust/Cargo.toml \
//!     -p beam_bifs --example maps_write_cost --offline
//! ```
//!
//! # What is measured (all through the BIF surface — `registry.call(...)`)
//!
//! Every fixture is a `{0 => 0, 1 => 1, …, n-1 => n-1}` map built **once** per `n`
//! with `maps:from_list/1` on its own heap, which is never rewound: the source map is
//! the same term for every sample, so every call starts from it (nothing compounds,
//! and the writer's "same value word ⇒ return the input map" arm can never be entered
//! by accident — the replacement value is one the source map does not hold).
//!
//! | row | call |
//! |---|---|
//! | `put replace` | `maps:put(KeyPresent, FreshValue, M)` — a write, not the word-identity no-op |
//! | `put insert` | `maps:put(n, Value, M)` — `n` is absent (the keys are `0 .. n-1`) |
//! | `remove present` | `maps:remove(KeyPresent, M)` |
//! | `remove absent` | `maps:remove(n, M)` — the `Ok(None)` arm: nothing is written |
//! | `from_list rebuild` | `maps:from_list([{n, Value} \| maps:to_list(M)])`, the comparator |
//! | `update replace` | `maps:update(KeyPresent, FreshValue, M)` — a write (extra row) |
//! | `take present` | `maps:take(KeyPresent, M)` — a lookup plus the same path copy (extra row) |
//! | `control` | `maps:size(M)`, a two-load O(1) read: the zero-work sample the pair subtracts |
//!
//! # The rebuild comparator, and why it is an honest "before"
//!
//! `from_list rebuild` is exactly the shape the four update BIFs used to run: take
//! the map's pairs, change one key, hand the whole pair list to
//! [`beam_term::map::write_map`] (`crate::access::make_map`) — which hashes *every*
//! key, sorts, builds the trie and serializes it, for one changed pair. The old
//! `put_3`/`update_3`/`remove_2` also performed an **O(n) pair walk** on top of that
//! (`map_pairs` → `collect_pairs` → `Vec<(Eterm, Eterm)>`) to splice the key in, so
//! the true before-figure was strictly larger than this row; the `maps:to_list/1`
//! walk and the list construction are performed **outside** the timed region here so
//! the row prices the builder alone.
//!
//! # Method (this host is shared, so the discipline matters)
//!
//! Each (operation, `n`) is measured as `ROUNDS` **paired, interleaved** rounds: one
//! sample of the operation and one sample of the `maps:size/1` control back to back,
//! and the reported figure is the **median of the paired differences**. Every sample
//! rewinds the BIF working arena and rebuilds the [`BifContext`], so the constant
//! overhead a real dispatch pays (arena rewind + context + registry lookup) is
//! present in *both* halves of a pair and cancels in the difference. Two runs at
//! different times are never compared.
//!
//! The source maps and the rebuild comparator's pair list live on a separate heap
//! (`src`), so rewinding the arena cannot invalidate them — the path copy shares the
//! source map's off-path subtrees by word, and the term that must outlive the arena
//! is the source map itself.

use std::hint::black_box;
use std::time::Instant;

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_process::ProcessId;
use beam_term::prelude::signed_value;
use beam_term::{make_small, AtomTable, Eterm};

/// Map sizes measured, in distinct small-integer keys. All three are well above
/// `MAP_SMALL_MAP_LIMIT` (32), so every fixture is a **HAMT** hash map — the
/// representation the update BIFs' path copy is about (a flat map's path copy is a
/// keys-tuple `memcpy`, a different and much smaller shape).
const SIZES: [usize; 3] = [250, 2_500, 6_250];

/// Paired (operation, control) rounds per (row, `n`). Odd, so the median is the middle
/// sample; a round is a few hundred microseconds, so a handful more costs nothing and
/// buys a median that one stalled round on this shared host cannot move.
const ROUNDS: usize = 21;

/// Calls per sample for an update BIF: ~50-100 µs of work per sample, far above the
/// `Instant::now()` resolution, while keeping the whole run well under a minute.
const UPDATE_ITERS: usize = 128;

/// The heap the source maps and the rebuild comparator's pair list live on: it is
/// never rewound, so a HAMT built by `write_map`'s `21·n + 18`-word reservation for
/// `n = 6250` (≈131 K words) plus both lists fit with room to spare.
const SOURCE_HEAP_WORDS: usize = 1 << 20;

/// The BIF working arena — the `BifContext::heap()` destination, rewound before every
/// single call exactly as `beam_rt` rewinds its pooled BIF heap. Large enough for the
/// `from_list` rebuild of the largest fixture (≈131 K words).
const ARENA_WORDS: usize = 1 << 20;

/// The value `put replace`/`update replace` write: never a value the source map
/// holds, so the writer's "same value word ⇒ answer the input map" arm is not taken.
fn replace_value(n: usize) -> Eterm {
    make_small(1_000_000 + n as i64)
}

/// The value `put insert` and the rebuild comparator's appended pair write. The two
/// use the same pair, so the comparator prices building exactly the map the insert
/// would have produced.
fn insert_value(n: usize) -> Eterm {
    make_small(2_000_000 + n as i64)
}

/// One `registry.call(...)`, panicking on a typed failure — a BIF that cannot serve
/// its documented shape is a bench-harness bug, never a number to report.
fn call(
    registry: &BifRegistry,
    ctx: &mut BifContext<'_>,
    module: &str,
    function: &str,
    args: &[Eterm],
) -> Eterm {
    registry
        .call(module, function, args.len() as u32, ctx, args)
        .unwrap_or_else(|e| panic!("{module}:{function}/{} failed: {e:?}", args.len()))
}

/// `maps:size/1` as a `usize` (a verification helper, never on the timed path).
fn size_of(registry: &BifRegistry, ctx: &mut BifContext<'_>, map: Eterm) -> usize {
    let size = call(registry, ctx, "maps", "size", &[map]);
    usize::try_from(signed_value(size)).expect("a non-negative map size")
}

/// The median of `values` (in place): the estimator's reducer. The sample count is
/// `ROUNDS`, odd, so this is the middle element.
fn median(values: &mut [f64]) -> f64 {
    assert!(!values.is_empty(), "the estimator needs at least one round");
    values.sort_by(|a, b| a.partial_cmp(b).expect("elapsed times are finite"));
    let mid = values.len() / 2;
    if values.len() % 2 == 0 {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    }
}

/// One sample: `iters` calls, each on a freshly rewound arena and a fresh
/// [`BifContext`], returned as ns/call. The rewind and the context are inside the
/// timed region on purpose — the paired control pays them too, so the difference is
/// the map work alone.
fn sample<F>(iters: usize, arena: &mut Heap, atoms: &AtomTable, pid: ProcessId, mut body: F) -> f64
where
    F: FnMut(&mut BifContext<'_>) -> Eterm,
{
    let start = Instant::now();
    for _ in 0..iters {
        arena.reset();
        let mut ctx = BifContext::new(&mut *arena, atoms, pid);
        black_box(body(&mut ctx));
    }
    start.elapsed().as_nanos() as f64 / iters as f64
}

/// The estimator: `rounds` paired rounds of (operation, control) back to back, the
/// median of the per-round differences (ns/call).
fn paired<F, G>(
    arena: &mut Heap,
    atoms: &AtomTable,
    pid: ProcessId,
    rounds: usize,
    iters: usize,
    mut op: F,
    mut ctl: G,
) -> f64
where
    F: FnMut(&mut BifContext<'_>) -> Eterm,
    G: FnMut(&mut BifContext<'_>) -> Eterm,
{
    let mut diffs = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let op_ns = sample(iters, arena, atoms, pid, &mut op);
        let ctl_ns = sample(iters, arena, atoms, pid, &mut ctl);
        diffs.push(op_ns - ctl_ns);
    }
    median(&mut diffs)
}

/// `/proc/loadavg`'s three load figures, so a reader can judge the host.
fn loadavg() -> String {
    let line = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    let fields: Vec<&str> = line.split_whitespace().take(3).collect();
    if fields.is_empty() {
        "?".to_string()
    } else {
        fields.join(" ")
    }
}

/// One measured row, ns/op.
struct Row {
    n: usize,
    put_replace: f64,
    put_insert: f64,
    remove_present: f64,
    remove_absent: f64,
    rebuild: f64,
    update_replace: f64,
    take_present: f64,
    control: f64,
}

/// The fixture for one `n`: the source map and the rebuild comparator's pair list,
/// both on the never-rewound `source` heap.
struct Fixture {
    map: Eterm,
    rebuild_list: Eterm,
}

/// Builds `#{0 => 0, …, n-1 => n-1}` with `maps:from_list/1` and, on the same heap,
/// the comparator's input: `[{n, Insert} | maps:to_list(Map)]`.
///
/// Both the walk and the list construction are untimed setup: the row prices
/// `maps:from_list/1` alone.
fn build_source(
    n: usize,
    registry: &BifRegistry,
    source: &mut Heap,
    atoms: &AtomTable,
    pid: ProcessId,
) -> Fixture {
    let mut ctx = BifContext::new(&mut *source, atoms, pid);
    let mut pairs = Vec::with_capacity(n);
    for i in 0..n {
        let key = make_small(i as i64);
        pairs.push(
            ctx.tuple(&[key, key])
                .expect("a 2-tuple fits the source heap"),
        );
    }
    let list = ctx
        .list(&pairs)
        .expect("the pair list fits the source heap");
    let map = call(registry, &mut ctx, "maps", "from_list", &[list]);
    assert_eq!(
        size_of(registry, &mut ctx, map),
        n,
        "from_list built n keys"
    );

    let existing = call(registry, &mut ctx, "maps", "to_list", &[map]);
    let appended = ctx
        .tuple(&[make_small(n as i64), insert_value(n)])
        .expect("a 2-tuple fits the source heap");
    let rebuild_list = ctx.cons(appended, existing).expect("a cons cell fits");
    Fixture { map, rebuild_list }
}

/// Cheap correctness pass — the rows must be measuring the writers, so the shapes are
/// pinned before they are timed (including the two no-write arms: an absent `remove`
/// hands back the **input map word**, which is what `erts_debug:same/2` observes).
fn verify(
    n: usize,
    fx: &Fixture,
    registry: &BifRegistry,
    arena: &mut Heap,
    atoms: &AtomTable,
    pid: ProcessId,
) {
    arena.reset();
    let mut ctx = BifContext::new(&mut *arena, atoms, pid);
    let mid = make_small((n / 2) as i64);
    let third = make_small((n / 3) as i64);
    let absent = make_small(n as i64);

    let replaced = call(
        registry,
        &mut ctx,
        "maps",
        "put",
        &[mid, replace_value(n), fx.map],
    );
    assert_eq!(
        size_of(registry, &mut ctx, replaced),
        n,
        "put replace kept the size"
    );
    let got = call(registry, &mut ctx, "maps", "get", &[mid, replaced]);
    assert_eq!(got, replace_value(n), "put replace stored the new value");

    let inserted = call(
        registry,
        &mut ctx,
        "maps",
        "put",
        &[absent, insert_value(n), fx.map],
    );
    assert_eq!(
        size_of(registry, &mut ctx, inserted),
        n + 1,
        "put insert grew the map"
    );
    let got = call(registry, &mut ctx, "maps", "get", &[absent, inserted]);
    assert_eq!(got, insert_value(n), "put insert stored the new pair");

    let removed = call(registry, &mut ctx, "maps", "remove", &[third, fx.map]);
    assert_eq!(
        size_of(registry, &mut ctx, removed),
        n - 1,
        "remove present shrank the map"
    );

    let untouched = call(registry, &mut ctx, "maps", "remove", &[absent, fx.map]);
    assert!(
        untouched == fx.map,
        "remove of an absent key returns the input map word"
    );

    let updated = call(
        registry,
        &mut ctx,
        "maps",
        "update",
        &[mid, replace_value(n), fx.map],
    );
    assert_eq!(
        size_of(registry, &mut ctx, updated),
        n,
        "update replace kept the size"
    );

    let taken = call(registry, &mut ctx, "maps", "take", &[mid, fx.map]);
    let value = call(
        registry,
        &mut ctx,
        "erlang",
        "element",
        &[make_small(1), taken],
    );
    assert_eq!(
        value,
        make_small((n / 2) as i64),
        "take returned the stored value"
    );
    let rest = call(
        registry,
        &mut ctx,
        "erlang",
        "element",
        &[make_small(2), taken],
    );
    assert_eq!(
        size_of(registry, &mut ctx, rest),
        n - 1,
        "take shrank the map"
    );

    let rebuilt = call(registry, &mut ctx, "maps", "from_list", &[fx.rebuild_list]);
    assert_eq!(
        size_of(registry, &mut ctx, rebuilt),
        n + 1,
        "the rebuild list holds n + 1 pairs"
    );
    arena.reset();
}

fn main() {
    let load_before = loadavg();

    let registry = default_registry();
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true",
        "false",
        "ok",
        "none",
        "error",
        "undefined",
        "nonode@nohost",
    ]);
    let pid = ProcessId::new(1, 0);

    let mut source = Heap::new(SOURCE_HEAP_WORDS).expect("source heap");
    let mut arena = Heap::new(ARENA_WORDS).expect("BIF working arena");

    let mut rows: Vec<Row> = Vec::with_capacity(SIZES.len());
    for n in SIZES {
        let fx = build_source(n, &registry, &mut source, &atoms, pid);
        verify(n, &fx, &registry, &mut arena, &atoms, pid);

        let map = fx.map;
        let rebuild_list = fx.rebuild_list;
        let mid = make_small((n / 2) as i64);
        let third = make_small((n / 3) as i64);
        let absent = make_small(n as i64);
        let repl = replace_value(n);
        let ins = insert_value(n);

        // The zero-work control: two loads inside `map_size` (plus the dispatch).
        let control = |ctx: &mut BifContext<'_>| call(&registry, ctx, "maps", "size", &[map]);

        let put_replace = paired(
            &mut arena,
            &atoms,
            pid,
            ROUNDS,
            UPDATE_ITERS,
            |ctx| call(&registry, ctx, "maps", "put", &[mid, repl, map]),
            control,
        );
        let put_insert = paired(
            &mut arena,
            &atoms,
            pid,
            ROUNDS,
            UPDATE_ITERS,
            |ctx| call(&registry, ctx, "maps", "put", &[absent, ins, map]),
            control,
        );
        let remove_present = paired(
            &mut arena,
            &atoms,
            pid,
            ROUNDS,
            UPDATE_ITERS,
            |ctx| call(&registry, ctx, "maps", "remove", &[third, map]),
            control,
        );
        let remove_absent = paired(
            &mut arena,
            &atoms,
            pid,
            ROUNDS,
            UPDATE_ITERS,
            |ctx| call(&registry, ctx, "maps", "remove", &[absent, map]),
            control,
        );
        let update_replace = paired(
            &mut arena,
            &atoms,
            pid,
            ROUNDS,
            UPDATE_ITERS,
            |ctx| call(&registry, ctx, "maps", "update", &[mid, repl, map]),
            control,
        );
        let take_present = paired(
            &mut arena,
            &atoms,
            pid,
            ROUNDS,
            UPDATE_ITERS,
            |ctx| call(&registry, ctx, "maps", "take", &[mid, map]),
            control,
        );

        // The Θ(n log n) comparator: one `from_list` per sample, so a sample is a few
        // tens of microseconds without making the run long.
        let rebuild_iters = (2_000 / n).max(1);
        let rebuild = paired(
            &mut arena,
            &atoms,
            pid,
            ROUNDS,
            rebuild_iters,
            |ctx| call(&registry, ctx, "maps", "from_list", &[rebuild_list]),
            control,
        );
        // The control figure itself, for transparency (ns/call): the dispatch + arena
        // rewind + context build that every pair above subtracts.
        let control_ns = sample(UPDATE_ITERS, &mut arena, &atoms, pid, control);

        rows.push(Row {
            n,
            put_replace,
            put_insert,
            remove_present,
            remove_absent,
            rebuild,
            update_replace,
            take_present,
            control: control_ns,
        });
    }

    let load_after = loadavg();

    println!(
        "maps single-key writers — path copy vs. the from_list rebuild\n\
         estimator: median of {ROUNDS} paired rounds; each round samples the operation and a\n\
         \x20          maps:size/1 control back to back (arena rewound + BifContext rebuilt per\n\
         \x20          call in both halves), reported as the median paired difference, ns/call.\n\
         update rows: {UPDATE_ITERS} calls/sample; rebuild rows: max(1, 2000/n) calls/sample.\n\
         loadavg before: {load_before}\n\
         loadavg after:  {load_after}\n"
    );

    println!("| n | put replace ns | put insert ns | remove present ns | remove absent ns | from_list rebuild ns |");
    println!("|---:|---:|---:|---:|---:|---:|");
    for r in &rows {
        println!(
            "| {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.0} |",
            r.n, r.put_replace, r.put_insert, r.remove_present, r.remove_absent, r.rebuild
        );
    }

    println!();
    println!("| n | rebuild / put-replace |");
    println!("|---:|---:|");
    for r in &rows {
        println!("| {} | {:.1}x |", r.n, r.rebuild / r.put_replace);
    }

    println!();
    println!("| n | update replace ns | take present ns | control (maps:size) ns |");
    println!("|---:|---:|---:|---:|");
    for r in &rows {
        println!(
            "| {} | {:.1} | {:.1} | {:.1} |",
            r.n, r.update_replace, r.take_present, r.control
        );
    }

    // Flatness: the algorithmic claim is a *relation*, so state it as one — the last
    // size is 25x the first, and a Θ(log n) writer's ratio must stay far below that.
    let first = &rows[0];
    let last = &rows[rows.len() - 1];
    println!();
    println!(
        "flatness (n = {} / n = {}, i.e. 25x the entries):\n\
         \x20 put replace    {:.2}x\n\
         \x20 put insert     {:.2}x\n\
         \x20 remove present {:.2}x\n\
         \x20 remove absent  {:.2}x\n\
         \x20 update replace {:.2}x\n\
         \x20 take present   {:.2}x\n\
         \x20 from_list      {:.2}x",
        last.n,
        first.n,
        last.put_replace / first.put_replace,
        last.put_insert / first.put_insert,
        last.remove_present / first.remove_present,
        last.remove_absent / first.remove_absent,
        last.update_replace / first.update_replace,
        last.take_present / first.take_present,
        last.rebuild / first.rebuild,
    );
}
