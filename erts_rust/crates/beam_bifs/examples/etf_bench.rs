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
//! A reproducible micro-benchmark for the ETF round trip
//! (`term_to_binary/1` → `binary_to_term/1`) over the **registered BIFs** — the exact
//! crate-level path `beam_rt` drives when it dispatches
//! `erlang:term_to_binary/1,2` / `erlang:binary_to_term/1,2`.
//!
//! Absolute latencies on this shared host are meaningless on their own; run it before and
//! after a change on the same host and read the **ratio**, and always report
//! `/proc/loadavg` alongside the numbers (printed in the first output line).
//!
//! ```text
//! erts_rust/scripts/cargo.sh run --release --manifest-path erts_rust/Cargo.toml \
//!     -p beam_bifs --example etf_bench -- [atoms] [iters]
//! ```
//!
//! * `atoms` — the number of synthetic atoms the shared table is seeded with (default
//!   3000, standing in for the union of the preloaded modules' atoms, which is what makes
//!   a whole-table clone expensive). The payload's own atoms are seeded on top.
//! * `iters` — the number of round trips timed per phase (default 20000).
//!
//! Phases (all ns/op):
//! * `clone`  — one `AtomTable::clone()` of that table, i.e. the decode path's per-call
//!   rebuild measured in isolation;
//! * `verify` — the index-by-index revalidation of a memoized table image, isolated;
//! * `encode` — `erlang:term_to_binary/1` on the payload term;
//! * `memo`   — `binary_to_term/1` the way it ran *before* the O(1) witness guard: a
//!   memoized image of the live table revalidated one name at a time on every call;
//! * `decode` — `erlang:binary_to_term/1` of the image (memo + O(1) witness);
//! * `round`  — both, back to back (the `etf_roundtrip` shape).
//!
//! Every destination heap is `reset()` between iterations, so a metric is the crate's own
//! per-op work (allocation included) and not GC — which `beam_rt` performs outside the BIF.

use std::hint::black_box;
use std::time::Instant;

use beam_alloc::Heap;
use beam_bifs::{default_registry, AtomInterner, BifContext};
use beam_process::ProcessId;
use beam_term::{Atom, AtomTable, Eterm};

/// Seed table size: the number of synthetic atoms interned before the payload's own. The
/// preloaded modules' `AtU8` counts sum to ≈2720 before dedup, so 3000 is representative
/// of the runtime's `AtomTable` when `etf_roundtrip` runs.
const DEFAULT_ATOMS: usize = 3000;

/// Round trips timed per phase.
const DEFAULT_ITERS: usize = 20_000;

/// A minimal `AtomInterner`: records novel names and predicts the shared table's next
/// indices exactly as `beam_rt`'s `PendingInterner` does.
struct CollectorInterner {
    base: usize,
    names: Vec<String>,
}

impl AtomInterner for CollectorInterner {
    fn intern(&mut self, name: &str) -> Atom {
        if let Some(offset) = self.names.iter().position(|pending| pending == name) {
            return Atom::from_raw((self.base + offset) as u32);
        }
        let index = self.base + self.names.len();
        self.names.push(name.to_string());
        Atom::from_raw(index as u32)
    }
}

/// The `payload/0` term of the `etf_roundtrip` bench case as an ETF image: a map with five
/// keys holding a 10-element list, a `{ok, <<"hello world">>, 3.5}` tuple, a 12-byte
/// binary, a char-list string and `-42`.
fn payload_image() -> Vec<u8> {
    const SMALL_ATOM_UTF8: u8 = 119;
    const SMALL_INTEGER: u8 = 97;
    const INTEGER: u8 = 98;
    const SMALL_TUPLE: u8 = 104;
    const BINARY: u8 = 109;
    const NEW_FLOAT: u8 = 70;
    const STRING: u8 = 107;
    const LIST: u8 = 108;
    const NIL: u8 = 106;
    const MAP: u8 = 116;

    fn key(out: &mut Vec<u8>, name: &str) {
        out.push(SMALL_ATOM_UTF8);
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());
    }

    let mut out = vec![131u8, MAP];
    out.extend_from_slice(&5u32.to_be_bytes());
    // a => [1, ..., 10]
    key(&mut out, "a");
    out.push(LIST);
    out.extend_from_slice(&10u32.to_be_bytes());
    for n in 1..=10u8 {
        out.push(SMALL_INTEGER);
        out.push(n);
    }
    out.push(NIL);
    // b => {ok, <<"hello world">>, 3.5}
    key(&mut out, "b");
    out.push(SMALL_TUPLE);
    out.push(3);
    key(&mut out, "ok");
    out.push(BINARY);
    out.extend_from_slice(&11u32.to_be_bytes());
    out.extend_from_slice(b"hello world");
    out.push(NEW_FLOAT);
    out.extend_from_slice(&3.5f64.to_be_bytes());
    // c => <<1..12>>
    key(&mut out, "c");
    out.push(BINARY);
    out.extend_from_slice(&12u32.to_be_bytes());
    out.extend(1..=12u8);
    // d => "a char list"
    key(&mut out, "d");
    out.push(STRING);
    out.extend_from_slice(&11u16.to_be_bytes());
    out.extend_from_slice(b"a char list");
    // e => -42
    key(&mut out, "e");
    out.push(INTEGER);
    out.extend_from_slice(&(-42i32).to_be_bytes());
    out
}

/// Times `body` `iters` times and returns ns/op (one untimed warm-up first, so lazily
/// initialized state is not charged to the first sample).
fn bench<F: FnMut()>(iters: usize, mut body: F) -> f64 {
    body();
    let start = Instant::now();
    for _ in 0..iters {
        body();
    }
    start.elapsed().as_nanos() as f64 / iters as f64
}

/// The image bytes behind a heap-resident byte-aligned binary term.
fn binary_bytes(term: Eterm) -> Vec<u8> {
    let (bytes, offset_bits, size_bits) = beam_codec::term::bitstring_bytes(term).expect("binary");
    assert_eq!((offset_bits, size_bits) , (0, bytes.len() * 8), "byte-aligned binary");
    bytes.to_vec()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let atoms_n: usize = args
        .next()
        .and_then(|a| a.parse().ok())
        .unwrap_or(DEFAULT_ATOMS);
    let iters: usize = args
        .next()
        .and_then(|a| a.parse().ok())
        .unwrap_or(DEFAULT_ITERS);

    let loadavg = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    let loadavg = loadavg.split_whitespace().next().unwrap_or("?").to_string();

    // The shared table: the runtime's own atoms, then the payload's.
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "nonode@nohost", "a", "b", "c", "d", "e"]);
    let synthetic: Vec<String> = (0..atoms_n).map(|n| format!("synthetic_atom_{n}")).collect();
    atoms.seed(synthetic.iter().map(String::as_str));
    let seeded = atoms.len();

    // The source term lives on its own heap for the whole run.
    let mut src_heap = Heap::new(1 << 16).unwrap();
    let payload = beam_codec::decode_term(&payload_image(), &mut src_heap, &mut atoms).unwrap();

    // One canonical image, for the decode phase, decoded and held on its own heap. This
    // mirrors `beam_rt`: `term_to_binary` returns a binary term on the BIF heap, which
    // `binary_to_term` then reads.
    let registry = default_registry();
    let pid = ProcessId::new(1, 0);
    let mut enc_heap = Heap::new(1 << 16).unwrap();
    let mut img_heap = Heap::new(1 << 10).unwrap();
    let mut dst_heap = Heap::new(1 << 16).unwrap();
    let mut interner = CollectorInterner {
        base: atoms.len(),
        names: Vec::new(),
    };
    let canonical = {
        let mut ctx = BifContext::new(&mut enc_heap, &atoms, pid);
        let out = registry
            .call("erlang", "term_to_binary", 1, &mut ctx, &[payload])
            .expect("term_to_binary/1");
        binary_bytes(out)
    };
    let img_bytes = beam_codec::term::make_binary(&mut img_heap, &canonical).unwrap();

    // --- clone: the whole-table rebuild, isolated ------------------------------------
    let clone_ns = bench(iters, || {
        black_box(atoms.clone());
    });

    // --- verify: a *correct* full-prefix validation of a cached copy, isolated -------
    // (the cheap alternative to a rebuild: comparing every index of a memoized copy
    // against the live table is O(n) but allocation-free)
    let scratch = atoms.clone();
    let verify_ns = bench(iters, || {
        let mut same = scratch.len() == atoms.len();
        for i in 0..scratch.len() {
            let a = Atom::from_raw(i as u32);
            if scratch.name(a) != atoms.name(a) {
                same = false;
                break;
            }
        }
        black_box(same);
    });
    drop(scratch);

    // --- encode ----------------------------------------------------------------------
    let encode_ns = bench(iters, || {
        enc_heap.reset();
        let mut ctx = BifContext::new(&mut enc_heap, &atoms, pid);
        let out = registry
            .call("erlang", "term_to_binary", 1, &mut ctx, &[payload])
            .expect("term_to_binary/1");
        black_box(binary_bytes(out));
    });

    // --- decode ----------------------------------------------------------------------
    let decode_ns = bench(iters, || {
        dst_heap.reset();
        let mut ctx = BifContext::new(&mut dst_heap, &atoms, pid)
            .with_atom_interner(&mut interner);
        let term = registry
            .call("erlang", "binary_to_term", 1, &mut ctx, &[img_bytes])
            .expect("binary_to_term/1");
        black_box(term);
    });
    assert_eq!(interner.names.len(), 0, "the payload image interned a novel atom");

    // --- legacy decode: the pre-fix path, a full `AtomTable` clone per call -----------
    // (kept in the bench so "before" and "after" are measured in the same run on the
    // same host — the ratio, not the absolute latency, is the signal)
    let legacy_ns = bench(iters, || {
        dst_heap.reset();
        let mut local = atoms.clone();
        let term = beam_codec::decode_term_used(&canonical, &mut dst_heap, &mut local)
            .expect("binary_to_term/1");
        black_box(term);
    });
    // --- memoized decode: the previous `with_decode_scratch` shape ---------------------
    // The per-thread *image* of the live table was reused, but every call compared it
    // against that table one name at a time (`prefix_matches`) before decoding into it.
    // The payload interns nothing, so the image stays exact and all `n` comparisons run —
    // this is the cost the O(1) witness guard removes.
    let mut memo = atoms.clone();
    let memo_ns = bench(iters, || {
        dst_heap.reset();
        let mut same = memo.len() == atoms.len();
        for i in 0..memo.len() {
            let a = Atom::from_raw(i as u32);
            if memo.name(a) != atoms.name(a) {
                same = false;
                break;
            }
        }
        black_box(same);
        let term = beam_codec::decode_term_used(&canonical, &mut dst_heap, &mut memo)
            .expect("binary_to_term/1");
        black_box(term);
    });
    // --- round -----------------------------------------------------------------------
    let round_ns = bench(iters, || {
        enc_heap.reset();
        let mut ctx = BifContext::new(&mut enc_heap, &atoms, pid);
        let out = registry
            .call("erlang", "term_to_binary", 1, &mut ctx, &[payload])
            .expect("term_to_binary/1");
        let bytes = binary_bytes(out);
        img_heap.reset();
        let img = beam_codec::term::make_binary(&mut img_heap, &bytes).unwrap();
        dst_heap.reset();
        let mut ctx = BifContext::new(&mut dst_heap, &atoms, pid)
            .with_atom_interner(&mut interner);
        let term = registry
            .call("erlang", "binary_to_term", 1, &mut ctx, &[img])
            .expect("binary_to_term/1");
        black_box(term);
    });

    println!("atoms={seeded} iters={iters} loadavg={loadavg}");
    println!("clone   {clone_ns:>10.1} ns/op");
    println!("verify  {verify_ns:>10.1} ns/op");
    println!("encode  {encode_ns:>10.1} ns/op");
    println!("legacy  {legacy_ns:>10.1} ns/op  (pre-fix decode: full clone + decode)");
    println!("memo    {memo_ns:>10.1} ns/op  (previous decode: memoized image, index-by-index)");
    println!("decode  {decode_ns:>10.1} ns/op  (current decode: memoized image + O(1) witness)");
    println!("round   {round_ns:>10.1} ns/op  = {:.3} us/op", round_ns / 1000.0);
    println!(
        "witness speedup {:.1}x  (previous decode {:.3} us/op -> {:.3} us/op)",
        memo_ns / decode_ns,
        memo_ns / 1000.0,
        decode_ns / 1000.0
    );
    println!(
        "decode speedup {:.1}x   (old round ~ encode+legacy = {:.3} us/op -> {:.1}x)",
        legacy_ns / decode_ns,
        (legacy_ns + encode_ns) / 1000.0,
        (legacy_ns + encode_ns) / round_ns
    );
    println!("image   {:<10} bytes", canonical.len());
}
