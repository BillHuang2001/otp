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
//! A reproducible micro-benchmark for the O(1) append-only witness API of [`AtomTable`]
//! — `snapshot()` / `generation()` / `appended_since()` — against the O(table) block it
//! replaces, `AtomTable::clone()`.
//!
//! Absolute latencies on this shared host are meaningless on their own; run it before and
//! after a change on the same host and read the **shape across sizes** plus the ratio, and
//! always report `/proc/loadavg` alongside the numbers (printed in the first output line).
//! The signal is that the witness columns are *flat* in the table size while `clone`
//! grows linearly with it — that is the whole point of the API.
//!
//! ```text
//! erts_rust/scripts/cargo.sh run --release --manifest-path erts_rust/Cargo.toml \
//!     -p beam_term --example atom_witness -- [iters]
//! ```
//!
//! * `iters` — iterations timed per phase per size (default 2 000 000). `clone` is timed
//!   with a much smaller, iteration-derived count (it is O(table), so a full run on the
//!   largest table would dominate the benchmark); the count used is printed.
//!
//! Columns (all ns/op, except `clone`, which is printed in µs/op):
//! * `snapshot`  — `AtomTable::snapshot()`: three scalar reads into an `AtomSnapshot`;
//! * `generation`— `AtomTable::generation()`;
//! * `append(hit)`  — `appended_since()` against a prefix snapshot of the same table
//!   (what a memoized table image checks on every decode);
//! * `append(miss)` — `appended_since()` against a foreign table's snapshot;
//! * `clone`     — `AtomTable::clone()`: the per-call whole-table rebuild (every `String`
//!   deep-copied, the whole `HashMap` rehashed) that the witness lets a caller skip.

use std::hint::black_box;
use std::time::Instant;

use beam_term::{AtomSnapshot, AtomTable};

/// Iterations timed per phase per table size.
const DEFAULT_ITERS: usize = 2_000_000;

/// The table sizes measured: tiny, and the four sizes the ETF decode path's shared table
/// actually takes (the preloaded modules' atoms, plus synthetic ones).
const SIZES: [usize; 5] = [4, 1009, 3009, 12009, 30009];

/// A table holding `n` atoms.
fn table_with(n: usize) -> AtomTable {
    let mut t = AtomTable::new();
    t.seed(["true", "false", "ok", "error"][..n.min(4)].iter().copied());
    if n > 4 {
        let rest: Vec<String> = (4..n).map(|i| format!("synthetic_atom_{i}")).collect();
        t.seed(rest.iter().map(String::as_str));
    }
    assert_eq!(t.len(), n);
    t
}

/// Times `body` `iters` times and returns ns/op (one untimed warm-up first, so lazily
/// initialized state is not charged to the first sample).
fn bench(iters: usize, mut body: impl FnMut()) -> f64 {
    body();
    let start = Instant::now();
    for _ in 0..iters {
        body();
    }
    start.elapsed().as_nanos() as f64 / iters as f64
}

fn main() {
    let iters: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(DEFAULT_ITERS);
    let clone_iters = (iters / 4_000).clamp(20, 500);

    let loadavg = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    let loadavg = loadavg.split_whitespace().next().unwrap_or("?").to_string();

    // A table that is *not* the measured one, for the miss case.
    let foreign: AtomSnapshot = {
        let mut f = AtomTable::new();
        f.seed(["totally", "unrelated"]);
        black_box(f.snapshot())
    };

    println!("atom_witness: iters={iters} clone_iters={clone_iters} loadavg={loadavg}");
    println!(
        "{:>8}  {:>10}  {:>10}  {:>11}  {:>12}  {:>12}",
        "atoms", "snapshot", "generation", "append(hit)", "append(miss)", "clone"
    );
    println!(
        "{:>8}  {:>10}  {:>10}  {:>11}  {:>12}  {:>12}",
        "", "ns/op", "ns/op", "ns/op", "ns/op", "us/op"
    );

    for &size in &SIZES {
        let mut table = table_with(size);
        // A prefix snapshot, then one append: the hit case checks a table that has grown
        // since its witness was taken (the steady state of the memo protocol).
        let snap = table.snapshot();
        table.intern("appended_after_snapshot");
        assert!(table.appended_since(snap), "hit case must be a hit");

        let snapshot_ns = bench(iters, || {
            let t = black_box(&table);
            black_box(t.snapshot());
        });
        let generation_ns = bench(iters, || {
            let t = black_box(&table);
            black_box(t.generation());
        });
        let hit_ns = bench(iters, || {
            let t = black_box(&table);
            black_box(t.appended_since(black_box(snap)));
        });
        let miss_ns = bench(iters, || {
            let t = black_box(&table);
            black_box(t.appended_since(black_box(foreign)));
        });
        let clone_ns = bench(clone_iters, || {
            let t = black_box(&table);
            black_box(t.clone());
        });

        println!(
            "{size:>8}  {snapshot_ns:>10.3}  {generation_ns:>10.3}  {hit_ns:>11.3}  \
             {miss_ns:>12.3}  {:>12.3}",
            clone_ns / 1000.0
        );
    }
}
