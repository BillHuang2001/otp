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

//! **Manual wall-clock + allocation attribution for the ETF codec.**
//!
//! Run it with
//!
//! ```text
//! erts_rust/scripts/cargo.sh build --release --manifest-path erts_rust/Cargo.toml \
//!     -p beam_codec --examples
//! erts_rust/target/release/examples/etf_attrib
//! ```
//!
//! It measures the codec **alone** — no runtime, no BIF dispatch, no scheduler —
//! on the exact payload of the benchmark's worst `copy_bound` case,
//! `etf_roundtrip` (`erts_rust/tools/bench/src/workloads/copy_bound.rs::module_source`).
//! The point is to separate this crate's share of that case from everything the
//! surrounding runtime owns (`payload/0`'s construction, the BIF dispatch, the
//! bound-variable match), so the ratio published for the whole Erlang-level case is
//! not silently attributed to the codec.
//!
//! What it prints, in order:
//!
//! * `/proc/loadavg` before and after (so a contended machine is visible);
//! * the image size, its term census and the exact allocation counts (the same
//!   numbers `tests/alloc_count.rs` pins) for the `term_to_binary`- and
//!   `binary_to_term`-shaped calls;
//! * `ns/op` for encode and decode over [`ITERS`] iterations after a warm-up;
//! * a **phase breakdown**: the raw [`Writer`] append of the same byte count (the
//!   sink's floor), the `AtomTable` name lookup and intern costs, and a warm vs cold
//!   decode (the delta is the `AtomTable` share of a cold decode);
//! * a comparison against the **same payload built on a heap** with no wire bytes at
//!   all, which is what the runtime does per iteration *besides* calling the codec.
//!
//! Nothing here is asserted and no timing is a test: it is a reporting tool. The
//! deterministic, asserted counterpart is `tests/alloc_count.rs`.
//!
//! Licence note: the module is included from `tests/` (a directory module, so cargo
//! does not treat it as its own test target) instead of being duplicated here.

#[path = "../tests/attrib_support/mod.rs"]
mod attrib_support;

use std::hint::black_box;
use std::time::Instant;

use beam_alloc::Heap;
use beam_term::prelude::AtomTable;

use beam_codec::Writer;

use attrib_support::{
    allocs, build_payload, census, counted, decode_image, encode_image, Census, HEAP_WORDS,
    PAYLOAD_ATOM_NAMES, PAYLOAD_KEY_NAMES,
};

/// Measured iterations per phase (≥ 1e6 as the attribution contract requires).
const ITERS: u32 = 1_000_000;
/// Iterations discarded before each measurement.
const WARMUP: u32 = 20_000;

/// `/proc/loadavg`, or a note when it is unreadable.
fn loadavg() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .map(|s| s.trim_end().to_string())
        .unwrap_or_else(|e| format!("<unreadable: {e}>"))
}

/// `ns/op` for a timed loop of [`ITERS`] iterations.
fn ns_per_op(mut body: impl FnMut()) -> f64 {
    for _ in 0..WARMUP {
        body();
    }
    let start = Instant::now();
    for _ in 0..ITERS {
        body();
    }
    let elapsed = start.elapsed();
    elapsed.as_nanos() as f64 / f64::from(ITERS)
}

fn main() {
    println!("== beam_codec ETF attribution ==");
    println!("loadavg before: {}", loadavg());

    // ---- the payload (atoms pre-interned, so the encoder never interns) ----
    let mut heap = Heap::new(HEAP_WORDS).expect("a payload heap");
    let mut atoms = AtomTable::new();
    let term = build_payload(&mut heap, &mut atoms);

    let (image, encode_allocs) = counted(|| encode_image(term, &atoms).expect("encode"));
    let mut c = Census::default();
    census(term, &mut c);
    println!(
        "payload: image {} bytes, census {} nodes {:?}, atoms {}",
        image.len(),
        c.total(),
        c,
        atoms.len()
    );
    assert_eq!(c, Census::payload(), "the fixture is the benchmark payload");

    // ---- allocation counts (the same three the test pins) ----
    let mut dec_heap = Heap::new(HEAP_WORDS).expect("a payload heap");
    let mut dec_atoms = AtomTable::new();
    let (_, decode_fresh_allocs) = counted(|| {
        decode_image(&image, &mut dec_heap, &mut dec_atoms).expect("decode cold")
    });
    let mut warm_atoms = AtomTable::new();
    for name in PAYLOAD_KEY_NAMES.iter().copied().chain(["ok"]) {
        warm_atoms.intern(name);
    }
    let mut warm_heap = Heap::new(HEAP_WORDS).expect("a payload heap");
    let (_, decode_warm_allocs) = counted(|| {
        decode_image(&image, &mut warm_heap, &mut warm_atoms).expect("decode warm")
    });
    println!(
        "allocs: encode {encode_allocs}; decode cold {decode_fresh_allocs}; \
         decode warm {decode_warm_allocs}; AtomTable share of a cold decode {}",
        decode_fresh_allocs - decode_warm_allocs
    );

    // ---- encode: the `term_to_binary`-shaped call ----
    let encode_ns = ns_per_op(|| {
        let bytes = encode_image(black_box(term), &atoms).expect("encode");
        black_box(bytes.len());
    });

    // ---- the raw sink's floor: the same byte count through `Writer` ----
    let mut sink = Writer::with_capacity(image.len());
    let sink_ns = ns_per_op(|| {
        sink.clear();
        sink.bytes(black_box(&image));
        black_box(sink.len());
    });

    // ---- decode: warm and cold ----
    let decode_ns = ns_per_op(|| {
        warm_heap.reset();
        let decoded = decode_image(black_box(&image), &mut warm_heap, &mut warm_atoms)
            .expect("decode warm");
        black_box(decoded);
    });
    let decode_cold_ns = ns_per_op(|| {
        warm_heap.reset();
        let mut table = AtomTable::new();
        for name in PAYLOAD_ATOM_NAMES {
            table.intern(name);
        }
        let decoded =
            decode_image(black_box(&image), &mut warm_heap, &mut table).expect("decode cold");
        black_box(decoded);
    });

    // ---- `AtomTable` interaction, isolated ----
    let atoms_ref = &atoms;
    let name_ns = ns_per_op(|| {
        for name in PAYLOAD_ATOM_NAMES {
            let atom = atoms_ref.lookup(name).expect("interned");
            black_box(atoms_ref.name(atom).expect("named"));
        }
    });

    // ---- the payload built on a heap with no wire bytes (the runtime's own
    //      per-iteration `payload/0` cost, which is NOT the codec's) ----
    let mut build_heap = Heap::new(HEAP_WORDS).expect("a payload heap");
    let mut build_atoms = AtomTable::new();
    let (_, build_allocs) = counted(|| {
        build_heap.reset();
        black_box(build_payload(&mut build_heap, &mut build_atoms));
    });
    let build_ns = ns_per_op(|| {
        build_heap.reset();
        black_box(build_payload(&mut build_heap, &mut build_atoms));
    });

    println!("ns/op over {ITERS} iterations:");
    println!("  encode (Encoder::new + encode + finish) : {encode_ns:9.1}");
    println!("  Writer append of the same byte count     : {sink_ns:9.1}");
    println!("  decode (warm AtomTable)                  : {decode_ns:9.1}");
    println!("  decode (cold AtomTable, incl. interning) : {decode_cold_ns:9.1}");
    println!("  AtomTable::intern + name (6 names)       : {name_ns:9.1}");
    println!("  payload built on a heap, no wire bytes   : {build_ns:9.1}  ({build_allocs} allocs)");
    println!(
        "  codec share of one Erlang-level round trip (encode+decode, warm): {:.1} ns",
        encode_ns + decode_ns
    );
    println!(
        "  whole-op baseline from tools/bench/RESULTS.md (m14, rust): 109917.8 ns; \
         the rest is the runtime's payload/0, BIF dispatch and the match"
    );

    // The measurements above must not be optimised away; touch them once more.
    black_box((allocs(), image.len(), build_allocs));
    println!("loadavg after:  {}", loadavg());
}
