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

//! Test-only microbenchmark: the two competing interiors of each selectable kind.
//!
//! Declared `#[cfg(test)]` in `lib.rs`. It times the operations ERTS optimizes an
//! index for — **insert**, **lookup hit**, **lookup miss**, **iteration** and
//! **delete** — for the four key shapes a real table stores (small integers,
//! atoms, binaries, tuples), across the sizes `10`, `1e3`, `1e5` (and `1e6` on
//! demand), and reports the interior's own **bytes per entry**
//! ([`Interior::memory`], the index footprint only — key terms are shared by both
//! interiors, so they cancel out).
//!
//! Two families are measured:
//! * `interior_microbenchmark` — the `set` interiors, chained
//!   ([`HashInterior`]) vs CHAMP ([`ChampInterior`]);
//! * `ordered_interior_microbenchmark` — the `ordered_set` interiors, the B+-tree
//!   ([`OrderedSetInterior`], the default) vs the sorted `Vec`
//!   ([`SortedOrderedInterior`]).
//!
//! Both families time **`insert_new`** (the "add a key that is not there" path) on
//! a disjoint key set in addition to `insert`, because `insert_new` is a distinct
//! code path in every interior (the B+-tree's is a separate descent) and is the
//! bulk-loading entry point (`ets:insert_new/2`).
//!
//! Run either with output:
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_ets \
//!     --lib interior_microbenchmark -- --nocapture
//! ```
//! Scale it with `BEAM_ETS_BENCH_SIZES=10,1000,100000,1000000` and restrict the
//! shapes with `BEAM_ETS_BENCH_SHAPES=smallint,binary,tuple`.
//!
//! **Comparing two implementations** across processes needs
//! `BEAM_ETS_BENCH_REPEATS=<passes>` (the reported figure is then the per-metric
//! minimum over those passes, see [`repeats`]) and, for the ordered family, usually
//! `BEAM_ETS_BENCH_INDEX=bplus` to leave the `O(n)`-per-op sorted `Vec` out of the
//! run. A single pass is not comparable between runs on a shared machine.
//!
//! Each measurement also **asserts** the results (`size`, hit/miss answers,
//! iteration count, `insert_new`'s `true`/`false`), so a wrong-but-fast index fails
//! the run rather than winning the benchmark.

use std::time::Instant;
use beam_term::bitstring::{heap_bits_size_words, write_binary};
use beam_term::heap::Heap;
use beam_term::prelude::*;

use crate::champ_interior::ChampInterior;
use crate::hash_interior::HashInterior;
use crate::kind::TableKind;
use crate::ordered_interior::{OrderedSetInterior, SortedOrderedInterior};
use crate::table::Interior;

/// The measured key shapes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// 60-bit small integers (no heap term).
    SmallInt,
    /// Interned atoms (`bench_<salt>_<i>`).
    Atom,
    /// 16-byte binaries (`i` as a little-endian `u128`).
    Binary,
    /// Two-element `{Int, Int}` tuples.
    Tuple,
}

impl Shape {
    fn name(self) -> &'static str {
        match self {
            Shape::SmallInt => "smallint",
            Shape::Atom => "atom",
            Shape::Binary => "binary",
            Shape::Tuple => "tuple",
        }
    }

    fn parse(name: &str) -> Option<Shape> {
        match name.trim() {
            "smallint" | "int" => Some(Shape::SmallInt),
            "atom" => Some(Shape::Atom),
            "binary" | "bin" => Some(Shape::Binary),
            "tuple" => Some(Shape::Tuple),
            _ => None,
        }
    }
}

/// A set of keys that keeps its backing term arena alive.
struct Keys {
    /// Owns the heap storage `keys` point into (empty for the immediate shapes).
    _heap: Heap,
    keys: Vec<Eterm>,
}

/// The measured sizes. Default keeps the crate gate fast; override with
/// `BEAM_ETS_BENCH_SIZES=10,1000,100000,1000000`.
fn sizes() -> Vec<usize> {
    match std::env::var("BEAM_ETS_BENCH_SIZES") {
        Ok(spec) => {
            let parsed: Vec<usize> = spec
                .split(',')
                .filter_map(|part| part.trim().parse().ok())
                .collect();
            assert!(!parsed.is_empty(), "BEAM_ETS_BENCH_SIZES held no sizes");
            parsed
        }
        Err(_) => vec![10, 1_000, 100_000],
    }
}

/// The measured shapes. Override with `BEAM_ETS_BENCH_SHAPES=smallint,binary`.
fn shapes() -> Vec<Shape> {
    match std::env::var("BEAM_ETS_BENCH_SHAPES") {
        Ok(spec) => {
            let parsed: Vec<Shape> = spec.split(',').filter_map(Shape::parse).collect();
            assert!(!parsed.is_empty(), "BEAM_ETS_BENCH_SHAPES held no shapes");
            parsed
        }
        Err(_) => vec![Shape::SmallInt, Shape::Atom, Shape::Binary, Shape::Tuple],
    }
}

/// How many full measurement passes to run per cell. Default `1`.
///
/// The reported figure per cell is the **minimum** over the passes ([`Metrics::min`]),
/// because a machine shared with other work can only ever make a pass *slower*: the
/// minimum converges on the interference-free cost, while a mean (or a single pass)
/// reports whatever contention happened to be present. Bump it with
/// `BEAM_ETS_BENCH_REPEATS=7` when comparing two implementations across processes —
/// a single pass is not comparable between runs.
fn repeats() -> usize {
    match std::env::var("BEAM_ETS_BENCH_REPEATS") {
        Ok(spec) => {
            let n: usize = spec
                .trim()
                .parse()
                .expect("BEAM_ETS_BENCH_REPEATS is a pass count");
            assert!(n >= 1, "BEAM_ETS_BENCH_REPEATS must be at least 1");
            n
        }
        Err(_) => 1,
    }
}

/// Which ordered interiors the `ordered_set` benchmark drives. Default `both`.
///
/// The sorted-`Vec` interior is `O(n)` per insert/delete, so it dominates a
/// large-`n` run; `BEAM_ETS_BENCH_INDEX=bplus` (or `sorted`) measures one interior
/// on its own, which is what an implementation A/B needs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Indexes {
    Both,
    Bplus,
    Sorted,
}

impl Indexes {
    fn parse(spec: &str) -> Indexes {
        match spec.trim() {
            "both" => Indexes::Both,
            "bplus" | "tree" => Indexes::Bplus,
            "sorted" | "vec" => Indexes::Sorted,
            other => panic!("BEAM_ETS_BENCH_INDEX: unknown interior {other:?}"),
        }
    }

    fn measures_bplus(self) -> bool {
        self != Indexes::Sorted
    }

    fn measures_sorted(self) -> bool {
        self != Indexes::Bplus
    }
}

fn indexes() -> Indexes {
    match std::env::var("BEAM_ETS_BENCH_INDEX") {
        Ok(spec) => Indexes::parse(&spec),
        Err(_) => Indexes::Both,
    }
}

/// Build `n` distinct keys of `shape`; `salt` shifts the key space so a second
/// call with `salt = n` yields keys absent from the first.
fn build_keys(shape: Shape, n: usize, atoms: &mut AtomTable, salt: u128) -> Keys {
    match shape {
        Shape::SmallInt => Keys {
            _heap: Heap::new(0),
            keys: (0..n)
                .map(|i| make_small(i as i64 + salt as i64))
                .collect(),
        },
        Shape::Atom => Keys {
            _heap: Heap::new(0),
            keys: (0..n)
                .map(|i| make_atom(atoms.intern(&format!("bench_{salt}_{i}"))))
                .collect(),
        },
        Shape::Binary => {
            const BITS: usize = 128;
            let per = heap_bits_size_words(BITS);
            let mut heap = Heap::new(n * per + 8);
            let mut keys = Vec::with_capacity(n);
            for i in 0..n {
                let bytes = (i as u128 + salt).to_le_bytes();
                // SAFETY: `per == heap_bits_size_words(128)` words are reserved
                // per key and `bytes` is exactly 16 bytes (128 bits).
                let term = unsafe {
                    let hp = heap
                        .alloc_words(per)
                        .expect("the arena was sized for every key");
                    write_binary(hp, &bytes).expect("16 bytes is a 128-bit binary")
                };
                keys.push(term);
            }
            Keys { _heap: heap, keys }
        }
        Shape::Tuple => {
            let mut heap = Heap::new(n * 3 + 8);
            let mut keys = Vec::with_capacity(n);
            for i in 0..n {
                let first = make_small(i as i64 + salt as i64);
                let second = make_small((i as i64).wrapping_mul(2_654_435_761));
                keys.push(
                    heap.tuple(&[first, second])
                        .expect("the arena was sized for every key"),
                );
            }
            Keys { _heap: heap, keys }
        }
    }
}

/// One interior's measurements for one (shape, size).
struct Metrics {
    insert_ns: f64,
    insert_new_ns: f64,
    hit_ns: f64,
    miss_ns: f64,
    iter_ns: f64,
    delete_ns: f64,
    bytes_per_entry: f64,
}

impl Metrics {
    /// The elementwise minimum of two passes over the same cell.
    ///
    /// Deliberate: a contended machine can only ever make a pass *slower*, so the
    /// minimum over several passes is the interference-free estimate, whereas an
    /// average reports the competition. (A *sub*-linear minimum would be a bug in
    /// the interior, which the per-pass assertions catch.)
    fn min(self, other: Metrics) -> Metrics {
        Metrics {
            insert_ns: self.insert_ns.min(other.insert_ns),
            insert_new_ns: self.insert_new_ns.min(other.insert_new_ns),
            hit_ns: self.hit_ns.min(other.hit_ns),
            miss_ns: self.miss_ns.min(other.miss_ns),
            iter_ns: self.iter_ns.min(other.iter_ns),
            delete_ns: self.delete_ns.min(other.delete_ns),
            bytes_per_entry: self.bytes_per_entry.min(other.bytes_per_entry),
        }
    }
}

/// Run [`measure`] on `repeats()` freshly built interiors and keep the per-metric
/// minimum ([`Metrics::min`]).
fn measure_best<I: Interior, F: FnMut() -> I>(
    mut make: F,
    present: &[Eterm],
    missing: &[Eterm],
    fresh: &[Eterm],
    atoms: &AtomTable,
) -> Metrics {
    let mut best: Option<Metrics> = None;
    for _ in 0..repeats() {
        let mut interior = make();
        let pass = measure(&mut interior, present, missing, fresh, atoms);
        best = Some(match best {
            Some(best) => best.min(pass),
            None => pass,
        });
    }
    best.expect("at least one measurement pass")
}

/// Time `insert`, `insert_new`, hit-lookup, miss-lookup, iteration and delete on
/// `interior`, asserting the answers as it goes. The object stored for every key is
/// the key itself, so the measurement is the *index* (key → object) cost only.
///
/// `present` are the keys the upsert/hit/miss/iterate/delete phases use; `fresh` is
/// a **disjoint** key set the `insert_new` phase loads into the (by then emptied)
/// interior, so every `insert_new` call takes the "key is new" path.
fn measure<I: Interior>(
    interior: &mut I,
    present: &[Eterm],
    missing: &[Eterm],
    fresh: &[Eterm],
    atoms: &AtomTable,
) -> Metrics {
    let n = present.len();
    let nf = n as f64;

    let start = Instant::now();
    for &key in present {
        interior.insert(key, key, atoms).unwrap();
    }
    let insert_ns = start.elapsed().as_nanos() as f64 / nf;
    assert_eq!(interior.size(), n, "every key must be stored");

    let start = Instant::now();
    for &key in present {
        assert_eq!(interior.lookup(key, atoms).len(), 1, "a present key must hit");
    }
    let hit_ns = start.elapsed().as_nanos() as f64 / nf;

    let start = Instant::now();
    for &key in missing {
        assert!(!interior.member(key, atoms), "an absent key must miss");
    }
    let miss_ns = start.elapsed().as_nanos() as f64 / nf;

    let start = Instant::now();
    let iterated = interior.iter_objects().count();
    let iter_ns = start.elapsed().as_nanos() as f64 / nf;
    assert_eq!(iterated, n, "iteration must visit every stored object");

    let bytes_per_entry = interior.memory() as f64 * 8.0 / nf;

    let start = Instant::now();
    for &key in present {
        interior.delete(key, atoms).unwrap();
    }
    let delete_ns = start.elapsed().as_nanos() as f64 / nf;
    assert_eq!(interior.size(), 0, "every key must be removed");

    // `insert_new` on an empty interior with a disjoint key set: every call adds a
    // key (`true`), so the figure is the pure "insert a key that is absent" path.
    assert_eq!(fresh.len(), n, "the insert_new key set matches the others");
    let start = Instant::now();
    for &key in fresh {
        assert!(
            interior.insert_new(key, key, atoms).unwrap(),
            "a disjoint key must be new"
        );
    }
    let insert_new_ns = start.elapsed().as_nanos() as f64 / nf;
    assert_eq!(interior.size(), n, "every fresh key must be stored");
    // …and a second pass over the same keys is refused (`false`) and stores nothing.
    for &key in fresh {
        assert!(
            !interior.insert_new(key, key, atoms).unwrap(),
            "an already-stored key must be refused"
        );
    }
    assert_eq!(interior.size(), n, "a refused insert_new stores nothing");
    for &key in fresh {
        interior.delete(key, atoms).unwrap();
    }
    assert_eq!(interior.size(), 0, "the fresh keys must be removed again");

    Metrics {
        insert_ns,
        insert_new_ns,
        hit_ns,
        miss_ns,
        iter_ns,
        delete_ns,
        bytes_per_entry,
    }
}

/// Format a per-op nanosecond figure.
fn ns(v: f64) -> String {
    format!("{v:.1}")
}

fn print_row(shape: Shape, index: &str, n: usize, m: &Metrics) {
    println!(
        "{:<9} {:>9} {:<8} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10.1}",
        shape.name(),
        n,
        index,
        ns(m.insert_ns),
        ns(m.insert_new_ns),
        ns(m.hit_ns),
        ns(m.miss_ns),
        ns(m.iter_ns),
        ns(m.delete_ns),
        m.bytes_per_entry,
    );
}

#[test]
fn interior_microbenchmark() {
    let sizes = sizes();
    let shapes = shapes();
    let mut atoms = AtomTable::new();

    println!();
    println!("# beam_ets set-interior microbenchmark: chained (HashInterior) vs CHAMP (ChampInterior)");
    println!("# cells: ns per operation; last column: interior bytes per entry");
    println!(
        "{:<9} {:>9} {:<8} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "shape", "n", "index", "insert", "insert_new", "hit", "miss", "iter", "delete", "B/entry"
    );

    for shape in shapes {
        for &n in &sizes {
            let present = build_keys(shape, n, &mut atoms, 0);
            let missing = build_keys(shape, n, &mut atoms, n as u128);
            let fresh = build_keys(shape, n, &mut atoms, 2 * n as u128);

            let chained = measure_best(
                || HashInterior::new(TableKind::Set),
                &present.keys,
                &missing.keys,
                &fresh.keys,
                &atoms,
            );
            let champ = measure_best(
                ChampInterior::new,
                &present.keys,
                &missing.keys,
                &fresh.keys,
                &atoms,
            );

            print_row(shape, "chained", n, &chained);
            print_row(shape, "champ", n, &champ);
            println!(
                "{:<9} {:>9} {:<8} {:>10.2} {:>10.2} {:>10.2} {:>10.2} {:>10.2} {:>10.2} {:>10.2}",
                shape.name(),
                n,
                "champ/old",
                ratio(champ.insert_ns, chained.insert_ns),
                ratio(champ.insert_new_ns, chained.insert_new_ns),
                ratio(champ.hit_ns, chained.hit_ns),
                ratio(champ.miss_ns, chained.miss_ns),
                ratio(champ.iter_ns, chained.iter_ns),
                ratio(champ.delete_ns, chained.delete_ns),
                ratio(champ.bytes_per_entry, chained.bytes_per_entry),
            );
        }
    }
    println!();
}

/// A deterministic pseudo-random permutation of `keys` (an LCG-driven
/// Fisher-Yates shuffle, so no `rand` dependency), used only by the *ordered*
/// benchmark.
///
/// The sorted-`Vec` interior's cost depends on the position it inserts/deletes at:
/// appending keys in ascending order is its O(1) fast path, while deleting them in
/// ascending order is its shift-the-whole-tail worst case. Measuring in that
/// order would flatter it (and the hash interiors do not care), so the ordered
/// benchmark drives a shuffled key order instead.
fn shuffled(keys: &[Eterm], seed: u64) -> Vec<Eterm> {
    let mut out = keys.to_vec();
    let mut state = seed | 1;
    for i in (1..out.len()).rev() {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let j = ((state >> 33) as usize) % (i + 1);
        out.swap(i, j);
    }
    out
}

/// The `ordered_set` benchmark: the B+-tree default vs the sorted `Vec`.
///
/// It uses the same sizes as the `set` benchmark (`10`, `1e3`, `1e5` by default).
/// It is the crate's *slow* benchmark because the sorted-`Vec` interior shifts the
/// whole tail on every insert/delete (`O(n)`); iterate with
/// `BEAM_ETS_BENCH_SIZES=10,1000,10000` to keep it quick, extend the list for a
/// scale curve, or measure one interior at a time with
/// `BEAM_ETS_BENCH_INDEX=bplus`/`sorted` (see [`Indexes`]).
#[test]
fn ordered_interior_microbenchmark() {
    let sizes = sizes();
    let shapes = shapes();
    let indexes = indexes();
    let mut atoms = AtomTable::new();

    println!();
    println!("# beam_ets ordered_set-interior microbenchmark: B+-tree (OrderedSetInterior) vs sorted Vec (SortedOrderedInterior)");
    println!("# keys are driven in a shuffled order (see `shuffled`); cells: ns per operation");
    println!(
        "{:<9} {:>9} {:<8} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "shape", "n", "index", "insert", "insert_new", "hit", "miss", "iter", "delete", "B/entry"
    );

    for shape in shapes {
        for &n in &sizes {
            let present = build_keys(shape, n, &mut atoms, 0);
            let missing = build_keys(shape, n, &mut atoms, n as u128);
            let fresh = build_keys(shape, n, &mut atoms, 2 * n as u128);
            let present_keys = shuffled(&present.keys, 0x9E37_79B9_7F4A_7C15);
            let missing_keys = shuffled(&missing.keys, 0x2545_F491_4F6C_DD1D);
            let fresh_keys = shuffled(&fresh.keys, 0x6C07_8965_CA92_A6ED);

            let tree = indexes.measures_bplus().then(|| {
                measure_best(
                    OrderedSetInterior::new,
                    &present_keys,
                    &missing_keys,
                    &fresh_keys,
                    &atoms,
                )
            });
            let sorted = indexes.measures_sorted().then(|| {
                measure_best(
                    SortedOrderedInterior::new,
                    &present_keys,
                    &missing_keys,
                    &fresh_keys,
                    &atoms,
                )
            });

            if let Some(tree) = &tree {
                print_row(shape, "bplus", n, tree);
            }
            if let Some(sorted) = &sorted {
                print_row(shape, "sorted", n, sorted);
            }
            if let (Some(tree), Some(sorted)) = (&tree, &sorted) {
                println!(
                    "{:<9} {:>9} {:<8} {:>10.2} {:>10.2} {:>10.2} {:>10.2} {:>10.2} {:>10.2} {:>10.2}",
                    shape.name(),
                    n,
                    "sorted/tree",
                    ratio(sorted.insert_ns, tree.insert_ns),
                    ratio(sorted.insert_new_ns, tree.insert_new_ns),
                    ratio(sorted.hit_ns, tree.hit_ns),
                    ratio(sorted.miss_ns, tree.miss_ns),
                    ratio(sorted.iter_ns, tree.iter_ns),
                    ratio(sorted.delete_ns, tree.delete_ns),
                    ratio(sorted.bytes_per_entry, tree.bytes_per_entry),
                );
            }
        }
    }
    println!();
}

/// The `numerator`/`denominator` ratio for one column (`< 1.0` favours the
/// numerator's interior — CHAMP for the `set` table, the B+-tree for the ordered
/// table).
fn ratio(numerator: f64, denominator: f64) -> f64 {
    if denominator == 0.0 {
        f64::NAN
    } else {
        numerator / denominator
    }
}