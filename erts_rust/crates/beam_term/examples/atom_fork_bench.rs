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
//! A paired A/B for the two whole-table copies of [`AtomTable`]: the deep [`Clone`] and the
//! handle-sharing [`AtomTable::fork`].
//!
//! The runtime's per-module load takes a private copy of the whole live atom table, so the cost of
//! that copy is paid once per module loaded, at whatever size the table happens to have. This
//! example measures one such copy, at a size the boot actually reaches, in **retired
//! instructions** — the only trustworthy figure on this host (a wall time on a shared, hybrid-CPU
//! machine is noise at these effect sizes).
//!
//! ```text
//! erts_rust/scripts/cargo.sh build --release --manifest-path erts_rust/Cargo.toml \
//!     -p beam_term --example atom_fork_bench
//! for mode in none clone fork; do
//!   taskset -c 1 perf stat -e cpu_core/instructions/u \
//!     ./erts_rust/target/release/examples/atom_fork_bench $mode 3009 2000
//! done
//! ```
//!
//! `none` is the control: it builds the table and runs the same iteration loop with **no** copy,
//! so `instructions/op = (mode - none) / iters` cancels the table construction, the loop and the
//! process. Alternate the modes inside every round and record `/proc/loadavg` (printed here).
//!
//! Arguments: `<none|clone|fork> [size] [iters]`.
use std::hint::black_box;
use std::time::Instant;
use beam_term::AtomTable;
/// The live atom table's size once the boot has loaded the bulk of its modules — the size the
/// per-module copy is measured at by default (and roughly `atom.names`' own count).
const DEFAULT_SIZE: usize = 3_009;
/// Copies timed by default. `clone` allocates one string per atom, so a huge count would turn the
/// run into an allocator benchmark; 2 000 × 3 009 is ~6 M strings, a few tenths of a second.
const DEFAULT_ITERS: usize = 2_000;
/// A table of `n` realistically shaped atoms.
fn table_with(n: usize) -> AtomTable {
    let mut t = AtomTable::new();
    t.seed(["true", "false", "ok", "error"]);
    let rest: Vec<String> = (4..n).map(|i| format!("atom_fork_atom_{i:05}")).collect();
    t.seed(rest.iter().map(String::as_str));
    assert_eq!(t.len(), n.max(4));
    t
}
/// Copies the table once, in the way the named mode does, and returns something the caller can
/// consume so the copy cannot be optimized away.
fn copy_once(mode: &str, table: &AtomTable) -> usize {
    match mode {
        "clone" => black_box(table.clone()).len(),
        "fork" => black_box(table.fork()).len(),
        _ => 0,
    }
}
fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "fork".to_string());
    assert!(
        matches!(mode.as_str(), "none" | "clone" | "fork"),
        "usage: atom_fork_bench <none|clone|fork> [size] [iters]"
    );
    let size = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_SIZE);
    let iters: usize = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_ITERS);

    let load = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    println!("loadavg before: {}", load.trim());
    let table = table_with(size);
    assert_eq!(table.len(), size.max(4));

    // One untimed warm-up (the allocator's arenas and the table's pages), then the timed loop.
    let mut sink = black_box(copy_once(&mode, &table));
    let start = Instant::now();
    for _ in 0..iters {
        sink = sink.wrapping_add(black_box(copy_once(&mode, &table)));
    }
    let elapsed = start.elapsed();
    black_box(sink);

    let ns_per_op = elapsed.as_nanos() as f64 / iters as f64;
    println!(
        "atom_fork_bench: mode={mode} size={size} iters={iters} => {ns_per_op:.1} ns/op \
         ({:.3} ms total, sink={sink})",
        elapsed.as_secs_f64() * 1e3
    );
    let load = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    println!("loadavg after: {}", load.trim());
}
