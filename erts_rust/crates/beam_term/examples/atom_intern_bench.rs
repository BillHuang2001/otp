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
//! The two atom-table directions the ETF round trip spends its time in: name → index
//! ([`AtomTable::lookup`], and a *hitting* [`AtomTable::intern`]) and index → name
//! ([`AtomTable::name`], the encode direction).
//!
//! `copy_bound/etf_roundtrip` decodes a payload whose atom set is `a`, `b`, `c`, `d`, `e`, `ok`,
//! so the name→index probe runs six times per payload — every one of them a **hit** on a name the
//! table already holds. That makes the probe, not `intern`'s append, the thing to measure: the
//! table is sized like the booted runtime's (~9 000 atoms, so a shard holds ~9 names) and the loop
//! asks for the same six payload-shaped names over and over.
//!
//! ```text
//! erts_rust/scripts/cargo.sh run --release --manifest-path erts_rust/Cargo.toml \
//!     -p beam_term --example atom_intern_bench [size] [iters]
//! ```
//!
//! Host discipline: this machine is shared, so read these figures as a *ratio* against the same
//! run of the other implementation, never as an absolute. `/proc/loadavg` is printed before and
//! after, the three phases interleave nothing but their own loop, and nothing here asserts — a
//! measurement, not a test.
//!
//! Arguments: `[size] [iters]`.
use std::hint::black_box;
use std::time::Instant;

use beam_term::{Atom, AtomTable};

/// The live atom table's size once a boot has loaded the bulk of its modules — roughly the size
/// the ETF round trip's probes run against (the booted shared table is ≈9 000 atoms).
const DEFAULT_SIZE: usize = 9_000;
/// Probes per phase. The phases are independent loops over the same six names, so 1 M each keeps
/// the run at a few tens of milliseconds while still averaging over scheduler noise.
const DEFAULT_ITERS: usize = 1_000_000;
/// The atom set of the `copy_bound/etf_roundtrip` payload — the names a decode interns (warm: all
/// present) and an encode looks up by index.
const PAYLOAD: [&str; 6] = ["a", "b", "c", "d", "e", "ok"];

/// A table of `size` realistically shaped atoms, with [`PAYLOAD`] interned **first**.
///
/// The payload names must be present for every probe to be a hit; interning them first also puts
/// them at indices 0..6, which is the layout a decode of that payload produces.
fn table_with(size: usize) -> AtomTable {
    let mut table = AtomTable::new();
    table.seed(PAYLOAD);
    let rest: Vec<String> = (0..size.saturating_sub(PAYLOAD.len()))
        .map(|i| format!("atom_intern_bench_{i:05}"))
        .collect();
    table.seed(rest.iter().map(String::as_str));
    table
}

/// `iters` repetitions of `f` (called with the iteration number and the rotating payload name),
/// returning nanoseconds per call.
///
/// Rotating through [`PAYLOAD`] is the shape of the decode loop (it alternates among the payload's
/// atoms), and it keeps the probe from hammering one cache line for a whole run. `i % 6` on a
/// six-element array is a multiply-shift, not a division, so the rotation costs no measurable time.
#[inline(always)]
fn time_per_call<F: FnMut(usize, &str) -> usize>(iters: usize, mut f: F) -> f64 {
    let start = Instant::now();
    let mut sink = 0usize;
    for i in 0..iters {
        sink = sink.wrapping_add(black_box(f(i, black_box(PAYLOAD[i % PAYLOAD.len()]))));
    }
    let elapsed = start.elapsed();
    black_box(sink);
    elapsed.as_nanos() as f64 / iters as f64
}

fn main() {
    let mut args = std::env::args().skip(1);
    let size = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_SIZE)
        .max(PAYLOAD.len());
    let iters: usize = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_ITERS);

    let load = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    println!("loadavg before: {}", load.trim());

    let mut table = table_with(size);
    println!("atom_intern_bench: size={} iters={iters}", table.len());

    // Resolve the six payload atoms once, outside every timed window.
    let atoms: [Atom; PAYLOAD.len()] = std::array::from_fn(|i| {
        table
            .lookup(PAYLOAD[i])
            .expect("the payload is seeded first")
    });

    // One untimed warm-up of each phase (the allocator's arenas, the table's pages), then the
    // timed loops.
    black_box(table.lookup(black_box(PAYLOAD[0])));
    black_box(table.name(black_box(atoms[0])));

    let lookup_ns = time_per_call(iters, |_, name| {
        table.lookup(name).map_or(0, |a| a.raw() as usize)
    });
    // A *hitting* `intern` is the same probe plus the early return — the path a warm decode takes
    // for every one of its six atom references.
    let intern_ns = time_per_call(iters, |_, name| table.intern(name).raw() as usize);
    // The other direction: index → name, what an encode walks. It rotates over the resolved atoms
    // rather than over the names, so the phase measures only the index→name accessor.
    let name_ns = time_per_call(iters, |i, _| {
        table
            .name(black_box(atoms[i % atoms.len()]))
            .map_or(0, str::len)
    });

    println!("  lookup (hit)      : {lookup_ns:>7.2} ns/call");
    println!("  intern (hit)      : {intern_ns:>7.2} ns/call");
    println!("  name (index->name): {name_ns:>7.2} ns/call");
    println!(
        "  a decode's 6 atom probes => {:.1} ns/payload",
        lookup_ns * PAYLOAD.len() as f64
    );

    let load = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    println!("loadavg after: {}", load.trim());
}
