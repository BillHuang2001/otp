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
//! A reproducible micro-benchmark for the `ets` **match-spec / traversal** BIFs over
//! the **registered BIFs** — the exact crate-level path `beam_rt` drives when it
//! dispatches `ets:select/2`, `ets:match/2`, `ets:select_reverse/2`,
//! `ets:select_count/2` and `erlang:match_spec_test/3`.
//!
//! Those five BIFs (and the rest of the `ets` select family) used to deep-clone the
//! whole shared `AtomTable` on **every call** — the borrow conflict between the
//! immutable table and the mutable caller heap. This bench measures that drop: the
//! `legacy` phase performs the old work (`ctx.atoms().clone()` followed by the very
//! same registry call) while the named phases run the current call, so "before" and
//! "after" are measured in the **same run** on the same host. Read the **ratio**, not
//! the absolute latency, and always report `/proc/loadavg` (printed on the first
//! output line).
//!
//! ```text
//! erts_rust/scripts/cargo.sh run --release --manifest-path erts_rust/Cargo.toml \
//!     -p beam_bifs --bin ets_bench -- [atoms] [iters] [objects]
//! ```
//!
//! * `atoms` — the number of synthetic atoms the shared table is seeded with
//!   (default 3000, standing in for the union of the preloaded modules' atoms, which
//!   is what makes a whole-table clone expensive);
//! * `iters` — the number of calls timed per phase (default 20000);
//! * `objects` — the number of `{Key, Value}` objects the benchmark table holds
//!   (default 100), i.e. how much real traversal work each call does on top of the
//!   (removed) clone.
//!
//! Phases (all ns/op, with the us/op equivalent printed for the before/after pair):
//! * `info`         — `ets:info(T, size)`: a **control** with no atom-table clone
//!   anywhere (it took the non-cloning path before *and* after), the floor the other
//!   phases are read against;
//! * `select`       — `ets:select/2` (whole-object projection match spec);
//! * `match`        — `ets:match/2` (`{'$1','$2'}` pattern);
//! * `select_rev`   — `ets:select_reverse/2`;
//! * `select_count` — `ets:select_count/2`;
//! * `ms_test`      — `erlang:match_spec_test/3` (`table` dialect);
//! * `legacy`       — `black_box(ctx.atoms().clone())` + `ets:select/2`, i.e. the
//!   pre-refactor cost, kept in-run so the speed-up ratio is load-independent.
//!
//! Every destination heap is `reset()` between iterations, so a metric is the crate's
//! own per-op work (allocation included) and not GC — which `beam_rt` performs
//! outside the BIF. The table's own terms live on a dedicated heap that is never
//! reset (the registry's tables alias it).

use std::hint::black_box;
use std::time::Instant;

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, TableRegistry};
use beam_common::Result;
use beam_process::ProcessId;
use beam_term::{AtomTable, Eterm};

/// Seed table size: the number of synthetic atoms interned before the payload's own.
const DEFAULT_ATOMS: usize = 3000;

/// Calls timed per phase.
const DEFAULT_ITERS: usize = 20_000;

/// Objects the benchmark table holds (the traversal work per call).
const DEFAULT_OBJECTS: usize = 100;

/// Times `body` `iters` times and returns ns/op (one untimed warm-up first, so
/// lazily initialized state is not charged to the first sample).
fn bench<F: FnMut()>(iters: usize, mut body: F) -> f64 {
    body();
    let start = Instant::now();
    for _ in 0..iters {
        body();
    }
    start.elapsed().as_nanos() as f64 / iters as f64
}

/// Dispatches one registered BIF over a fresh [`BifContext`] bound to `tables`,
/// `atoms` and `heap` — exactly the way `beam_rt` drives the registry.
#[allow(clippy::too_many_arguments)]
fn run(
    registry: &BifRegistry,
    tables: &mut TableRegistry,
    atoms: &AtomTable,
    heap: &mut Heap,
    pid: ProcessId,
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<Eterm> {
    let mut ctx = BifContext::new(heap, atoms, pid).with_tables(tables);
    registry.call(module, function, arity, &mut ctx, args)
}

fn main() {
    let mut argv = std::env::args().skip(1);
    let atoms_n: usize = argv
        .next()
        .and_then(|a| a.parse().ok())
        .unwrap_or(DEFAULT_ATOMS);
    let iters: usize = argv
        .next()
        .and_then(|a| a.parse().ok())
        .unwrap_or(DEFAULT_ITERS);
    let objects: usize = argv
        .next()
        .and_then(|a| a.parse().ok())
        .unwrap_or(DEFAULT_OBJECTS);

    let loadavg = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    let loadavg = loadavg.split_whitespace().next().unwrap_or("?").to_string();

    let registry = default_registry();
    let pid = ProcessId::new(1, 0);

    // --- the shared atom table (the thing every site used to clone) ---------------
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true",
        "false",
        "ok",
        "set",
        "ordered_set",
        "$1",
        "$2",
        "size",
        "table",
        "bench_tab",
    ]);
    let synthetic: Vec<String> = (0..atoms_n).map(|n| format!("synthetic_atom_{n}")).collect();
    atoms.seed(synthetic.iter().map(String::as_str));
    let seeded = atoms.len();
    drop(synthetic);

    // The heap the table's terms live on for the whole run — never reset (the
    // registry's tables alias it).
    let mut table_heap = Heap::new(1 << 20).unwrap();
    let mut tables = TableRegistry::new();

    // One `ordered_set` table with `objects` entries, plus the terms every phase
    // dispatches: the whole-object projection spec, the `[true]` counting spec, the
    // `{'$1','$2'}` pattern and the `{3, 30}` subject of `match_spec_test/3`.
    let (handle, select_spec, count_spec, pattern, against, size_atom, table_atom) = {
        let mut ctx = BifContext::new(&mut table_heap, &atoms, pid).with_tables(&mut tables);
        let name = ctx.atom("bench_tab").unwrap();
        let kind = ctx.atom("ordered_set").unwrap();
        let opts = ctx.list(&[kind]).unwrap();
        let handle = registry
            .call("ets", "new", 2, &mut ctx, &[name, opts])
            .expect("ets:new/2");
        for i in 1..=objects as i64 {
            let key = ctx.int(i).unwrap();
            let value = ctx.int(i * 10).unwrap();
            let object = ctx.tuple(&[key, value]).unwrap();
            registry
                .call("ets", "insert", 2, &mut ctx, &[handle, object])
                .expect("ets:insert/2");
        }

        let v1 = ctx.atom("$1").unwrap();
        let v2 = ctx.atom("$2").unwrap();
        let head = ctx.tuple(&[v1, v2]).unwrap();
        let guards = ctx.list(&[]).unwrap();
        // `[{{'$1','$2'}, [], [{{'$1','$2'}}]}]`
        let projected = ctx.tuple(&[v1, v2]).unwrap();
        let body_item = ctx.tuple(&[projected]).unwrap();
        let body = ctx.list(&[body_item]).unwrap();
        let clause = ctx.tuple(&[head, guards, body]).unwrap();
        let select_spec = ctx.list(&[clause]).unwrap();
        // `[{{'$1','$2'}, [], [true]}]`
        let t = ctx.atom("true").unwrap();
        let cbody = ctx.list(&[t]).unwrap();
        let cclause = ctx.tuple(&[head, guards, cbody]).unwrap();
        let count_spec = ctx.list(&[cclause]).unwrap();
        // The `{3, 30}` term `erlang:match_spec_test/3` matches the head against.
        let k3 = ctx.int(3).unwrap();
        let v30 = ctx.int(30).unwrap();
        let against = ctx.tuple(&[k3, v30]).unwrap();

        let size_atom = ctx.atom("size").unwrap();
        let table_atom = ctx.atom("table").unwrap();
        (
            handle,
            select_spec,
            count_spec,
            head,
            against,
            size_atom,
            table_atom,
        )
    };

    let mut dst_heap = Heap::new(1 << 18).unwrap();

    // Sanity: the table really holds `objects` matching entries, so each phase below
    // exercises the traversal (not an empty table that would mask a regression).
    {
        let count = run(
            &registry,
            &mut tables,
            &atoms,
            &mut dst_heap,
            pid,
            "ets",
            "select_count",
            2,
            &[handle, count_spec],
        )
        .expect("ets:select_count/2");
        dst_heap.reset();
        assert_eq!(
            beam_term::signed_value(count),
            objects as i64,
            "the benchmark table must hold {objects} matching objects"
        );
    }

    // --- info: the control (no atom-table clone before or after) -------------------
    let info_ns = bench(iters, || {
        dst_heap.reset();
        let out = run(
            &registry,
            &mut tables,
            &atoms,
            &mut dst_heap,
            pid,
            "ets",
            "info",
            2,
            &[handle, size_atom],
        )
        .expect("ets:info/2");
        black_box(out);
    });

    // --- select/2 ------------------------------------------------------------------
    let select_ns = bench(iters, || {
        dst_heap.reset();
        let out = run(
            &registry,
            &mut tables,
            &atoms,
            &mut dst_heap,
            pid,
            "ets",
            "select",
            2,
            &[handle, select_spec],
        )
        .expect("ets:select/2");
        black_box(out);
    });

    // --- match/2 -------------------------------------------------------------------
    let match_ns = bench(iters, || {
        dst_heap.reset();
        let out = run(
            &registry,
            &mut tables,
            &atoms,
            &mut dst_heap,
            pid,
            "ets",
            "match",
            2,
            &[handle, pattern],
        )
        .expect("ets:match/2");
        black_box(out);
    });

    // --- select_reverse/2 ----------------------------------------------------------
    let reverse_ns = bench(iters, || {
        dst_heap.reset();
        let out = run(
            &registry,
            &mut tables,
            &atoms,
            &mut dst_heap,
            pid,
            "ets",
            "select_reverse",
            2,
            &[handle, select_spec],
        )
        .expect("ets:select_reverse/2");
        black_box(out);
    });

    // --- select_count/2 ------------------------------------------------------------
    let count_ns = bench(iters, || {
        dst_heap.reset();
        let out = run(
            &registry,
            &mut tables,
            &atoms,
            &mut dst_heap,
            pid,
            "ets",
            "select_count",
            2,
            &[handle, count_spec],
        )
        .expect("ets:select_count/2");
        black_box(out);
    });

    // --- erlang:match_spec_test/3 --------------------------------------------------
    let ms_test_ns = bench(iters, || {
        dst_heap.reset();
        let out = run(
            &registry,
            &mut tables,
            &atoms,
            &mut dst_heap,
            pid,
            "erlang",
            "match_spec_test",
            3,
            &[against, select_spec, table_atom],
        )
        .expect("erlang:match_spec_test/3");
        black_box(out);
    });

    // --- legacy: the pre-fix cost — a full AtomTable clone + the same call ---------
    // (kept in the bench so "before" and "after" are measured in the same run on the
    // same host — the ratio, not the absolute latency, is the signal)
    let legacy_ns = bench(iters, || {
        black_box(atoms.clone());
        dst_heap.reset();
        let out = run(
            &registry,
            &mut tables,
            &atoms,
            &mut dst_heap,
            pid,
            "ets",
            "select",
            2,
            &[handle, select_spec],
        )
        .expect("ets:select/2");
        black_box(out);
    });

    println!("atoms={seeded} objects={objects} iters={iters} loadavg={loadavg}");
    println!("info         {info_ns:>10.1} ns/op  = {:>8.3} us/op  (control: no clone, before or after)", info_ns / 1000.0);
    println!("select       {select_ns:>10.1} ns/op  = {:>8.3} us/op", select_ns / 1000.0);
    println!("match        {match_ns:>10.1} ns/op  = {:>8.3} us/op", match_ns / 1000.0);
    println!("select_rev   {reverse_ns:>10.1} ns/op  = {:>8.3} us/op", reverse_ns / 1000.0);
    println!("select_count {count_ns:>10.1} ns/op  = {:>8.3} us/op", count_ns / 1000.0);
    println!("ms_test      {ms_test_ns:>10.1} ns/op  = {:>8.3} us/op", ms_test_ns / 1000.0);
    println!(
        "legacy       {legacy_ns:>10.1} ns/op  = {:>8.3} us/op  (pre-fix: ctx.atoms().clone() + select/2)",
        legacy_ns / 1000.0
    );
    println!(
        "select speedup {:.1}x   (legacy {:.3} us/op -> select {:.3} us/op)",
        legacy_ns / select_ns,
        legacy_ns / 1000.0,
        select_ns / 1000.0
    );
}
