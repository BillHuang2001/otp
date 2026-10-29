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

//! An in-process benchmark for the **incremental** map writers
//! ([`beam_term::map::map_put_cow`] / [`beam_term::map::map_remove_cow`]) against the
//! rebuild routes they replace ([`beam_term::map::map_put`] / [`beam_term::map::map_remove`],
//! each of which collects the pairs and re-runs [`beam_term::map::write_map`]).
//!
//! The rebuild route is Θ(n) per operation (a collect plus a sort plus a full write); the
//! path-copying route copies only the nodes on the key's hash path, which is a *bounded*
//! number of words. That bound is this bench's load-independent evidence: the exact
//! [`beam_term::map::map_put_cow_words_required`] /
//! [`beam_term::map::map_remove_cow_words_required`] figure per operation, printed next to
//! the reservation each route asks for.
//!
//! Three operations are measured at each size: replacing an existing key's value,
//! inserting an absent key, and removing a present key (the remove route rotates through
//! the base map's keys, so every round removes a key that is there).
//!
//! ```text
//! erts_rust/scripts/cargo.sh run --release --manifest-path erts_rust/Cargo.toml \
//!     -p beam_term --example map_cow_bench -- [budget_ms]
//! ```
//!
//! * `budget_ms` — measurement budget per configuration (default 100 ms).
//!
//! **The host is shared, so the absolute times below are indicative only.** The evidence is
//! the ratio between two same-run, same-host columns and the deterministic word counters.
//! A round is a *batch* of operations: the batch grows until it lasts at least
//! [`TARGET_BATCH_NS`], so the clock's resolution cannot dominate a fast route, and a
//! round's sample is `batch_ns / batch_ops`. Nothing here asserts on wall time — the binary
//! exits non-zero only when a *correctness* check fails.

use std::hint::black_box;
use std::time::Instant;

use beam_term::atom::{Atom, AtomTable};
use beam_term::eterm::Eterm;
use beam_term::immediate::{make_atom, make_small};
use beam_term::map;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// The measured map sizes: past the flat-map limit, up to a compiler-sized map.
const SIZES: [usize; 3] = [250, 2_500, 6_250];

/// Measurement budget per configuration, in nanoseconds, unless overridden on the command
/// line (`budget_ms`).
const DEFAULT_BUDGET_NS: u128 = 100_000_000;

/// Floor on the number of rounds (batches) per configuration, so a median means something.
const MIN_ROUNDS: usize = 20;

/// Ceiling on the number of rounds per configuration, so the binary cannot run away.
const MAX_ROUNDS: usize = 200_000;

/// A round is a *batch* of operations whose measured duration should reach this figure, so a
/// fast route's per-op number is not dominated by `Instant::now()`.
const TARGET_BATCH_NS: u128 = 50_000;

/// How much a batch may grow in one step while chasing [`TARGET_BATCH_NS`].
const MAX_BATCH_GROWTH: usize = 64;

/// Sentinel words placed immediately after the output reservation, so a write past the
/// documented bound is caught rather than silently corrupting the process.
const GUARD_WORDS: usize = 64;
const SENTINEL: usize = 0xDEAD_BEEF_5EED_5EED;

/// The atom fixture's size — deliberately larger than any measured map, so the insert route
/// always has an absent atom key to hand.
const N_ATOMS: usize = 8_192;

/// The value every put route stores. It must differ from the value the base map holds under
/// the replaced key: a path-copying put that finds the stored value word already identical
/// writes nothing (`erl_map.c:2523-2525`), which would time an empty operation.
const NEW_VALUE: i64 = -1_000_000;

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// One measured (size, operation) configuration.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Route {
    /// `map::map_put` — rebuild — replacing an existing key's value.
    PutReplaceRebuild,
    /// `map::map_put_cow` — path copy — replacing an existing key's value.
    PutReplaceCow,
    /// `map::map_put` — rebuild — inserting an absent key.
    PutInsertRebuild,
    /// `map::map_put_cow` — path copy — inserting an absent key.
    PutInsertCow,
    /// `map::map_remove` — rebuild — removing a present key.
    RemoveRebuild,
    /// `map::map_remove_cow` — path copy — removing a present key.
    RemoveCow,
}

/// The operation group a route belongs to: `0` replace, `1` insert, `2` remove.
const GROUP_REPLACE: usize = 0;
const GROUP_INSERT: usize = 1;
const GROUP_REMOVE: usize = 2;

impl Route {
    /// Every route, in report order (a rebuild before its path-copying twin).
    const ALL: [Route; 6] = [
        Route::PutReplaceRebuild,
        Route::PutReplaceCow,
        Route::PutInsertRebuild,
        Route::PutInsertCow,
        Route::RemoveRebuild,
        Route::RemoveCow,
    ];

    fn label(self) -> &'static str {
        match self {
            Route::PutReplaceRebuild => "put_replace_rebuild",
            Route::PutReplaceCow => "put_replace_cow",
            Route::PutInsertRebuild => "put_insert_rebuild",
            Route::PutInsertCow => "put_insert_cow",
            Route::RemoveRebuild => "remove_rebuild",
            Route::RemoveCow => "remove_cow",
        }
    }

    fn is_cow(self) -> bool {
        matches!(
            self,
            Route::PutReplaceCow | Route::PutInsertCow | Route::RemoveCow
        )
    }

    fn is_remove(self) -> bool {
        matches!(self, Route::RemoveRebuild | Route::RemoveCow)
    }

    fn group(self) -> usize {
        match self {
            Route::PutReplaceRebuild | Route::PutReplaceCow => GROUP_REPLACE,
            Route::PutInsertRebuild | Route::PutInsertCow => GROUP_INSERT,
            Route::RemoveRebuild | Route::RemoveCow => GROUP_REMOVE,
        }
    }
}

/// The words a route asks its caller to reserve for a map of `n` pairs.
fn reserve_words(route: Route, n: usize) -> usize {
    if route.is_remove() {
        if route.is_cow() {
            map::map_remove_cow_words_capacity(n)
        } else {
            map::map_words_capacity(n)
        }
    } else if route.is_cow() {
        map::map_put_cow_words_capacity(n)
    } else {
        map::map_words_capacity(n + 1)
    }
}

/// Performs one operation of `route` into `out` and returns the map it produced.
///
/// # Safety
///
/// `out` must address at least [`reserve_words`]`(route, n)` writable words (for a
/// path-copying route, at least the exact `*_words_required` figure), in a region that is not
/// the source map's own storage and that outlives every use of the result.
unsafe fn apply_route(
    route: Route,
    out: *mut Eterm,
    base: Eterm,
    key: Eterm,
    value: Eterm,
    atoms: &AtomTable,
) -> Eterm {
    match route {
        Route::PutReplaceRebuild | Route::PutInsertRebuild => {
            // SAFETY: the caller upholds the reservation above.
            unsafe { map::map_put(out, base, key, value, atoms) }.unwrap()
        }
        Route::PutReplaceCow | Route::PutInsertCow => {
            // SAFETY: as above.
            unsafe { map::map_put_cow(out, base, key, value, atoms) }.unwrap()
        }
        Route::RemoveRebuild => {
            // SAFETY: as above.
            unsafe { map::map_remove(out, base, key, atoms) }.unwrap()
        }
        Route::RemoveCow => {
            // SAFETY: as above. The rotated key is present, so `None` is unreachable — and
            // the fixture's correctness pass proves that before any timing starts.
            unsafe { map::map_remove_cow(out, base, key, atoms) }
                .unwrap()
                .unwrap_or(base)
        }
    }
}

// ---------------------------------------------------------------------------
// Fixture: the base map, its keys and the word figures
// ---------------------------------------------------------------------------

/// Everything one size needs: the base map (built once by `write_map`), the keys each route
/// works on, and the deterministic word counters.
struct Fixture {
    n: usize,
    /// The base map, living in the caller-held base buffer.
    base: Eterm,
    /// The base map's keys, in the order the remove route rotates through them.
    present: Vec<Eterm>,
    /// A key the base map does not hold (the insert route's target).
    absent: Eterm,
    /// The words each operation writes, per the crate's exact `*_words_required` figures.
    put_replace_words: usize,
    put_insert_words: usize,
    remove_words_min: usize,
    remove_words_max: usize,
    /// The words the timed output buffer holds (the widest reservation any route asks for).
    out_words: usize,
}

impl Fixture {
    /// The key one operation of `route` works on; `op` is the operation's index within the
    /// configuration, which only the remove route consumes (it rotates, so every operation
    /// removes a key that is present).
    fn key_for(&self, route: Route, op: usize) -> Eterm {
        match route.group() {
            GROUP_REMOVE => self.present[op % self.n],
            GROUP_INSERT => self.absent,
            _ => self.present[0],
        }
    }

    /// The exact words the path-copying writer writes for this configuration's operation,
    /// rendered for the report (the remove route rotates, so it can report a range).
    fn op_words(&self, route: Route) -> String {
        match route.group() {
            GROUP_REMOVE => {
                if self.remove_words_min == self.remove_words_max {
                    format!("{}", self.remove_words_min)
                } else {
                    format!("{}-{}", self.remove_words_min, self.remove_words_max)
                }
            }
            GROUP_INSERT => format!("{}", self.put_insert_words),
            _ => format!("{}", self.put_replace_words),
        }
    }

    /// The exact words figure for this configuration's operation, as a number.
    fn op_words_exact(&self, route: Route) -> usize {
        match route.group() {
            GROUP_REMOVE => self.remove_words_max,
            GROUP_INSERT => self.put_insert_words,
            _ => self.put_replace_words,
        }
    }
}

/// Builds the fixture for a base map already written into `base_buf`.
fn fixture(
    n: usize,
    base: Eterm,
    pairs: &[(Eterm, Eterm)],
    atoms: &AtomTable,
) -> Result<Fixture, String> {
    let present: Vec<Eterm> = pairs.iter().map(|p| p.0).collect();
    if present.len() != n {
        return Err(format!("fixture n={n}: the base map lost keys"));
    }
    let absent = absent_key(base, atoms)
        .map_err(|e| format!("fixture n={n}: no absent key available ({e:?})"))?;

    let mut remove_words_min = usize::MAX;
    let mut remove_words_max = 0usize;
    for &key in &present {
        let words = map::map_remove_cow_words_required(base, key)
            .map_err(|e| format!("fixture n={n}: remove words for a present key ({e:?})"))?;
        remove_words_min = remove_words_min.min(words);
        remove_words_max = remove_words_max.max(words);
    }

    let put_replace_words = map::map_put_cow_words_required(base, present[0])
        .map_err(|e| format!("fixture n={n}: put words for a present key ({e:?})"))?;
    let put_insert_words = map::map_put_cow_words_required(base, absent)
        .map_err(|e| format!("fixture n={n}: put words for an absent key ({e:?})"))?;

    let out_words = Route::ALL
        .iter()
        .map(|&r| reserve_words(r, n))
        .chain([
            put_replace_words,
            put_insert_words,
            remove_words_min,
            remove_words_max,
        ])
        .max()
        .unwrap_or(0);

    Ok(Fixture {
        n,
        base,
        present,
        absent,
        put_replace_words,
        put_insert_words,
        remove_words_min,
        remove_words_max,
        out_words,
    })
}

/// The first candidate the scan reaches that `base` does not hold.
fn absent_key(base: Eterm, atoms: &AtomTable) -> Result<Eterm, beam_term::error::TermError> {
    // Atom keys from the top of the fixture table downwards, then small ints above the pool
    // the base maps are generated from.
    let candidates = (0..N_ATOMS)
        .map(|j| make_atom(Atom::from_raw((N_ATOMS - 1 - j) as u32)))
        .chain((0..=4_096).map(|j| make_small(1_000_000 + j)));
    for candidate in candidates {
        if map::map_get(base, candidate, atoms)?.is_none() {
            return Ok(candidate);
        }
    }
    // Unreachable for the fixture's key pools: a small int far outside every pool.
    Ok(make_small(9_000_000))
}

// ---------------------------------------------------------------------------
// The base corpus
// ---------------------------------------------------------------------------

/// A deterministic xorshift64 generator, so every run builds the same corpus.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// `n` distinct-key pairs — a mix of atom and small-int keys, so the hash nibbles spread the
/// way a compiler's maps spread them — with small-int values.
fn make_pairs(rng: &mut Rng, n: usize) -> Vec<(Eterm, Eterm)> {
    let mut distinct: Vec<Eterm> = Vec::with_capacity(n);
    let mut pairs = Vec::with_capacity(n);
    while pairs.len() < n {
        let pick = rng.next() as usize;
        let key = if pick & 1 == 0 {
            make_atom(Atom::from_raw(((pick >> 1) % N_ATOMS) as u32))
        } else {
            make_small(((pick >> 1) % 100_000) as i64)
        };
        if distinct.contains(&key) {
            continue;
        }
        distinct.push(key);
        pairs.push((key, make_small(pairs.len() as i64)));
    }
    pairs
}

// ---------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------

/// One configuration's samples: a round is a batch, its sample the batch's ns/op.
struct Stats {
    rounds: usize,
    ops: usize,
    nanos: u128,
    samples: Vec<f64>,
}

impl Stats {
    fn mean(&self) -> f64 {
        if self.ops == 0 {
            f64::NAN
        } else {
            self.nanos as f64 / self.ops as f64
        }
    }

    fn median(&self) -> f64 {
        if self.samples.is_empty() {
            return f64::NAN;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mid = sorted.len() / 2;
        if sorted.len() % 2 == 0 {
            (sorted[mid - 1] + sorted[mid]) / 2.0
        } else {
            sorted[mid]
        }
    }
}

/// One reported configuration: its size, its route, the samples and the word figures.
struct Row {
    n: usize,
    route: Route,
    stats: Stats,
    words_op: String,
    reserve: usize,
}

/// Measures one configuration: warm up, then run rounds (batches) until the budget is spent,
/// growing the batch so a round outlasts the clock's resolution.
fn measure(route: Route, fx: &Fixture, out: *mut Eterm, atoms: &AtomTable, budget_ns: u128) -> Stats {
    let value = make_small(NEW_VALUE);
    // Warm up outside the window: page in the buffer and let the branches settle.
    for op in 0..8 {
        let key = fx.key_for(route, op);
        // SAFETY: `out` is the fixture's reservation for every route.
        unsafe { black_box(apply_route(route, out, fx.base, key, value, atoms)) };
    }

    let mut stats = Stats {
        rounds: 0,
        ops: 0,
        nanos: 0,
        samples: Vec::new(),
    };
    let mut batch = 1usize;
    let started = Instant::now();
    loop {
        let t0 = Instant::now();
        for j in 0..batch {
            let key = fx.key_for(route, stats.ops + j);
            // SAFETY: as above; each call fully overwrites the block it needs.
            unsafe { black_box(apply_route(route, out, fx.base, key, value, atoms)) };
        }
        let dt = t0.elapsed().as_nanos().max(1);

        stats.samples.push(dt as f64 / batch as f64);
        stats.nanos += dt;
        stats.ops += batch;
        stats.rounds += 1;

        if dt < TARGET_BATCH_NS {
            let factor = (TARGET_BATCH_NS / dt).clamp(2, MAX_BATCH_GROWTH as u128);
            batch = batch.saturating_mul(factor as usize).min(MAX_ROUNDS);
        }
        if stats.rounds >= MAX_ROUNDS {
            break;
        }
        if stats.rounds >= MIN_ROUNDS && started.elapsed().as_nanos() >= budget_ns {
            break;
        }
    }
    stats
}

fn fmt_ratio(a: f64, b: f64) -> String {
    if a.is_nan() || b.is_nan() || b == 0.0 {
        "n/a".to_string()
    } else {
        format!("{:.2}x", a / b)
    }
}

/// The median ns/op of one (size, route) configuration.
fn median_of(rows: &[Row], n: usize, route: Route) -> f64 {
    rows.iter()
        .find(|r| r.n == n && r.route == route)
        .map(|r| r.stats.median())
        .unwrap_or(f64::NAN)
}

// ---------------------------------------------------------------------------
// Correctness — one pass per route, outside every timed window
// ---------------------------------------------------------------------------

/// Verifies one route's produced map, and that the operation left the base map alone.
fn check_route(
    route: Route,
    fx: &Fixture,
    check_buf: *mut Eterm,
    atoms: &AtomTable,
) -> Result<(), String> {
    let n = fx.n;
    let value = make_small(NEW_VALUE);
    let key = fx.key_for(route, 0);
    // SAFETY: `check_buf` holds the widest reservation any route asks for.
    let out = unsafe { apply_route(route, check_buf, fx.base, key, value, atoms) };

    let out_size = map::map_size(out, atoms).map_err(|e| format!("{}: {e:?}", route.label()))?;
    let base_size = map::map_size(fx.base, atoms)
        .map_err(|e| format!("{}: base size {e:?}", route.label()))?;
    let got = map::map_get(out, key, atoms).map_err(|e| format!("{}: {e:?}", route.label()))?;
    let base_got =
        map::map_get(fx.base, key, atoms).map_err(|e| format!("{}: base lookup {e:?}", route.label()))?;

    let want_size = match route.group() {
        GROUP_INSERT => n + 1,
        GROUP_REMOVE => n - 1,
        _ => n,
    };
    let want_out = if route.is_remove() {
        None
    } else {
        Some(value)
    };

    if out_size != want_size {
        return Err(format!(
            "{}: the produced map holds {out_size} pairs, expected {want_size}",
            route.label()
        ));
    }
    if got != want_out {
        return Err(format!(
            "{}: the lookup of the operated key answered {got:?}, expected {want_out:?}",
            route.label()
        ));
    }
    if base_size != n {
        return Err(format!(
            "{}: the base map's size moved to {base_size} — the operation must be functional",
            route.label()
        ));
    }
    if route.is_remove() {
        if base_got.is_none() {
            return Err(format!(
                "{}: the removed key was not present in the base map",
                route.label()
            ));
        }
    } else if base_got == Some(value) {
        return Err(format!(
            "{}: the base map's stored value was overwritten",
            route.label()
        ));
    }

    // The reservation invariant: the exact figure must fit the route's capacity bound, and
    // the timed buffer must therefore hold both.
    let exact = fx.op_words_exact(route);
    let reserve = reserve_words(route, n);
    if route.is_cow() && exact > reserve {
        return Err(format!(
            "{}: the exact figure {exact} exceeds the capacity bound {reserve}",
            route.label()
        ));
    }
    if exact > fx.out_words {
        return Err(format!(
            "{}: the exact figure {exact} exceeds the timed buffer's {} words",
            route.label(),
            fx.out_words
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn loadavg() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "<unavailable>".to_string())
}

fn main() {
    let budget_ms: u128 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(DEFAULT_BUDGET_NS / 1_000_000);
    let budget_ns = budget_ms.max(1) * 1_000_000;

    let mut atoms = AtomTable::new();
    let names: Vec<String> = (0..N_ATOMS).map(|i| format!("map_key_{i}")).collect();
    atoms.seed(names.iter().map(String::as_str));
    let atoms = atoms;

    println!(
        "map_cow_bench: sizes={SIZES:?} budget={budget_ms} ms/config rounds={MIN_ROUNDS}..={MAX_ROUNDS} \
         batch>={TARGET_BATCH_NS} ns stored_value_delta={NEW_VALUE}"
    );
    println!("loadavg BEFORE: {}", loadavg());

    let mut failures: Vec<String> = Vec::new();
    let mut rows: Vec<Row> = Vec::new();
    let mut rng = Rng(0x243F_6A88_85A3_08D3);

    let block_started = Instant::now();
    for &n in &SIZES {
        let pairs = make_pairs(&mut rng, n);

        // The base map, built once per size by the from-scratch writer, then read-only.
        let mut base_buf = vec![Eterm::from_raw(0); map::map_words_capacity(n)];
        // SAFETY: `base_buf` holds `map_words_capacity(pairs.len())` words.
        let base = match unsafe { map::write_map(base_buf.as_mut_ptr(), &pairs, &atoms) } {
            Ok(base) => base,
            Err(e) => {
                eprintln!("map_cow_bench: the base map for n={n} failed: {e:?}");
                std::process::exit(1);
            }
        };
        let fx = match fixture(n, base, &pairs, &atoms) {
            Ok(fx) => fx,
            Err(e) => {
                eprintln!("map_cow_bench: {e}");
                std::process::exit(1);
            }
        };

        // Correctness first, in a buffer of its own, so no timed buffer is involved.
        let mut check_buf = vec![Eterm::from_raw(SENTINEL); fx.out_words + GUARD_WORDS];
        for &route in &Route::ALL {
            let check_ptr = check_buf.as_mut_ptr();
            if let Err(e) = check_route(route, &fx, check_ptr, &atoms) {
                failures.push(format!("n={n} {e}"));
            }
        }
        drop(check_buf);

        // One over-sized output buffer per size, reused by every route and every round: the
        // measurement is the writer, not the reservation (or the allocation) of its output.
        let mut out_buf = vec![Eterm::from_raw(SENTINEL); fx.out_words + GUARD_WORDS];
        let out = out_buf.as_mut_ptr();
        for &route in &Route::ALL {
            let stats = measure(route, &fx, out, &atoms, budget_ns);
            rows.push(Row {
                n,
                route,
                stats,
                words_op: fx.op_words(route),
                reserve: reserve_words(route, n),
            });
        }
        // Keep the buffer observably live, then check nothing wrote past the reservation.
        black_box(&out_buf[..]);
        let guard_hits = out_buf[fx.out_words..]
            .iter()
            .filter(|w| w.raw() != SENTINEL)
            .count();
        if guard_hits != 0 {
            failures.push(format!(
                "n={n}: {guard_hits} guard word(s) past the {}-word reservation were written",
                fx.out_words
            ));
        }
    }
    let block_wall = block_started.elapsed();

    println!();
    println!(
        "{:>6}  {:<20}  {:>7}  {:>10}  {:>12}  {:>12}  {:>10}  {:>9}",
        "n", "route", "rounds", "ops", "ns/op mean", "ns/op med", "words_op", "reserve"
    );
    for row in &rows {
        println!(
            "{:>6}  {:<20}  {:>7}  {:>10}  {:>12.1}  {:>12.1}  {:>10}  {:>9}",
            row.n,
            row.route.label(),
            row.stats.rounds,
            row.stats.ops,
            row.stats.mean(),
            row.stats.median(),
            row.words_op,
            row.reserve,
        );
    }
    println!("measurement block wall: {block_wall:?}");
    println!("loadavg AFTER: {}", loadavg());

    // Ratios: the rebuild route over its path-copying twin, per size.
    println!();
    println!("ratios, median ns/op (rebuild / cow — > 1.00x means the incremental writer wins):");
    println!(
        "{:>6}  {:>13}  {:>13}  {:>13}",
        "n", "put_replace", "put_insert", "remove"
    );
    for &n in &SIZES {
        println!(
            "{:>6}  {:>13}  {:>13}  {:>13}",
            n,
            fmt_ratio(
                median_of(&rows, n, Route::PutReplaceRebuild),
                median_of(&rows, n, Route::PutReplaceCow)
            ),
            fmt_ratio(
                median_of(&rows, n, Route::PutInsertRebuild),
                median_of(&rows, n, Route::PutInsertCow)
            ),
            fmt_ratio(
                median_of(&rows, n, Route::RemoveRebuild),
                median_of(&rows, n, Route::RemoveCow)
            ),
        );
    }

    // Scaling: how each route's median moves with the map size.
    println!();
    println!(
        "scaling, median ns/op by size ({:.0}x larger map from n={} to n={}):",
        SIZES[SIZES.len() - 1] as f64 / SIZES[0] as f64,
        SIZES[0],
        SIZES[SIZES.len() - 1]
    );
    println!(
        "{:<20}  {:>12}  {:>12}  {:>12}  {:>12}",
        "route", "n=250", "n=2500", "n=6250", "6250/250"
    );
    for &route in &Route::ALL {
        let per_size: Vec<f64> = SIZES.iter().map(|&n| median_of(&rows, n, route)).collect();
        println!(
            "{:<20}  {:>12.1}  {:>12.1}  {:>12.1}  {:>12}",
            route.label(),
            per_size[0],
            per_size[1],
            per_size[2],
            fmt_ratio(per_size[2], per_size[0]),
        );
    }

    println!();
    println!("note: this host is shared, so the absolute ns/op above are indicative only.");
    println!(
        "      the evidence is the rebuild/cow ratios and the deterministic `words_op` column,"
    );
    println!(
        "      which is the crate's exact `*_words_required` figure for the operation (and, for the"
    );
    println!(
        "      remove route, the range over the keys its rotation visits). `reserve` is the"
    );
    println!("      reservation each route asks its caller for.");
    println!(
        "      expectation: the rebuild's ns/op grows ~with n (it collects, sorts and rewrites the"
    );
    println!(
        "      whole map); the cow route's grows ~with the hash-path depth, i.e. ~log n, at a fixed"
    );
    println!("      word count. The numbers above are reported as measured, not tuned.");

    println!();
    let checks = SIZES.len() * Route::ALL.len();
    if failures.is_empty() {
        println!(
            "correctness: {checks}/{checks} route checks passed (map_size + map_get on the \
             produced map, the base map untouched, the reservation invariant, no write past the \
             reserved words)"
        );
    } else {
        for failure in &failures {
            println!("FAIL {failure}");
        }
        println!("correctness: {} of {checks} route check(s) failed", failures.len());
        std::process::exit(1);
    }
}
