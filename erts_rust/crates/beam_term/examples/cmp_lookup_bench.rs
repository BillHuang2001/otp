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

//! A single-workload micro-benchmark for the **term-comparison / map-lookup** path
//! (`compare::{cmp, cmp_arith, cmp_flatmap_keys, eq}`, `map::map_get`,
//! `hashing::make_hash2`, `ihash::erts_map_hash`).
//!
//! One bucket per run, selected on the command line, so the binary can be wrapped in
//! `perf stat -e cpu_core/instructions/u` (or `perf record`) and attributed to the
//! crate's own frames. Absolute latencies on a shared host are meaningless on their
//! own; what is comparable is the **retired-instruction count** of one build against
//! another for the same bucket, which is load-independent.
//!
//! ```text
//! erts_rust/scripts/cargo.sh run --release --manifest-path erts_rust/Cargo.toml \
//!     -p beam_term --example cmp_lookup_bench -- <bucket> [ops]
//! ```
//!
//! Buckets (each prints one `bucket ops ns/op` row):
//!
//! * `flat32_atom` — a 32-pair *flat* map with atom keys, one `map_get` per op (the
//!   linear `eq` scan, i.e. up to 32 structural comparisons);
//! * `flat32_int` — the same shape with small-integer keys (the scan without atoms);
//! * `hamt512_atom` — a 512-key hash map with atom keys (`erts_map_hash` + trie walk);
//! * `hamt512_int` — the same with small-integer keys;
//! * `payload_eq` — `eq` of two structurally equal **copies** of the `copy_bound/ets_copy`
//!   payload (~27 objects: 5-pair map, 10-element list, tuple, two binaries, char list);
//! * `payload_hash` — `make_hash2` of that payload (the ETS `set` CHAMP's hash);
//! * `bin_eq` — `eq` of two equal 11-byte binaries (the `memcmp` arm of `cmp_bitstrings`);
//! * `tuple_eq` — `eq` of two equal 2-tuples `{k, payload}`, the ETS object shape;
//! * `cmp_atoms` — `cmp` (ordering, not equality) of two distinct atom names, which is
//!   the name-memcmp arm `erts_cmp_atoms` *requires* for ordering.
use std::hint::black_box;
use std::time::Instant;

use beam_term::atom::{Atom, AtomTable};
use beam_term::compare::{cmp, eq};
use beam_term::eterm::Eterm;
use beam_term::hashing::make_hash2;
use beam_term::boxed::make_arityval;
use beam_term::immediate::{make_atom, make_small, NIL};
use beam_term::list::make_list;
use beam_term::map::{map_get, map_words_capacity, write_map};
use beam_term::thing::write_flatmap;
use beam_term::boxed::make_boxed;

/// A bump arena over one owned word buffer: the term builders in this crate are
/// heap-agnostic and take a caller-provided `*mut Eterm`, which is exactly this.
struct Arena {
    buf: Vec<Eterm>,
    off: usize,
}

impl Arena {
    fn new(words: usize) -> Self {
        Arena {
            buf: vec![Eterm::from_raw(0); words],
            off: 0,
        }
    }

    /// Reserve `n` words and hand back their base pointer.
    fn alloc(&mut self, n: usize) -> *mut Eterm {
        assert!(self.off + n <= self.buf.len(), "arena exhausted");
        let p = unsafe { self.buf.as_mut_ptr().add(self.off) };
        self.off += n;
        p
    }
}

/// A proper list of `items`.
fn build_list(a: &mut Arena, items: &[Eterm]) -> Eterm {
    let n = items.len();
    if n == 0 {
        return NIL;
    }
    let base = a.alloc(n * 2);
    for (i, item) in items.iter().enumerate() {
        // SAFETY: `base` covers `2n` freshly reserved words; cell `i` occupies words
        // `2i` (car) and `2i + 1` (cdr), both in bounds.
        unsafe {
            *base.add(i * 2) = *item;
            *base.add(i * 2 + 1) = if i + 1 == n {
                NIL
            } else {
                make_list(base.add((i + 1) * 2))
            };
        }
    }
    make_list(base)
}

/// A tuple of `items`.
fn build_tuple(a: &mut Arena, items: &[Eterm]) -> Eterm {
    let n = items.len();
    let base = a.alloc(n + 1);
    // SAFETY: `base` covers `n + 1` reserved words; word 0 is the header and words
    // `1..=n` the payload.
    unsafe {
        *base = make_arityval(n);
        for (i, item) in items.iter().enumerate() {
            *base.add(1 + i) = *item;
        }
    }
    make_boxed(base as *const Eterm)
}

/// A binary of `bytes`.
fn build_binary(a: &mut Arena, bytes: &[u8]) -> Eterm {
    let words = 2 + bytes.len().div_ceil(8);
    let hp = a.alloc(words);
    // SAFETY: `hp` covers `heap_bits_size_words(bytes.len() * 8) == words` words.
    unsafe { beam_term::bitstring::write_binary(hp, bytes) }.expect("binary")
}

/// A map keyed by each of `keys`, binding key `i` to the small integer `i`.
fn build_map(a: &mut Arena, keys: &[Eterm], atoms: &AtomTable) -> Eterm {
    let pairs: Vec<(Eterm, Eterm)> = keys
        .iter()
        .enumerate()
        .map(|(i, k)| (*k, make_small(i as i64)))
        .collect();
    let hp = a.alloc(map_words_capacity(pairs.len()));
    // SAFETY: `hp` covers `map_words_capacity(pairs.len())` exclusively-owned words.
    unsafe { write_map(hp, &pairs, atoms) }.expect("map")
}

/// The `copy_bound/ets_copy` payload: a 5-pair map whose values are a 10-element list, a
/// tuple `{ok, <<"hello world">>, 3.5}`, a 12-byte binary, a char-list string and `-42`.
fn build_payload(a: &mut Arena, atoms: &AtomTable) -> Eterm {
    let atom = |a: &AtomTable, n: &str| make_atom(a.lookup(n).expect("atom"));
    let list = {
        let items: Vec<Eterm> = (1..=10).map(make_small).collect();
        build_list(a, &items)
    };
    let hello = build_binary(a, b"hello world");
    let float = {
        let hp = a.alloc(2);
        // SAFETY: `hp` covers the 2-word float block.
        unsafe { beam_term::thing::write_float(hp, 3.5) }
    };
    let tuple = build_tuple(a, &[atom(atoms, "ok"), hello, float]);
    let bin = {
        let bytes: Vec<u8> = (1..=12).collect();
        build_binary(a, &bytes)
    };
    let string = {
        let items: Vec<Eterm> = "a char list".bytes().map(|b| make_small(b as i64)).collect();
        build_list(a, &items)
    };
    let pairs = vec![
        (atom(atoms, "a"), list),
        (atom(atoms, "b"), tuple),
        (atom(atoms, "c"), bin),
        (atom(atoms, "d"), string),
        (atom(atoms, "e"), make_small(-42)),
    ];
    // `write_flatmap` is order-preserving; the pair order above is not sorted, so use the
    // canonical builder (which sorts), exactly as every in-tree producer does.
    let hp = a.alloc(map_words_capacity(pairs.len()));
    // SAFETY: `hp` covers `map_words_capacity(pairs.len())` exclusively-owned words.
    unsafe { write_map(hp, &pairs, atoms) }.expect("payload")
}

/// An atom table holding the payload's names plus `n` synthetic `key<i>` atoms.
fn table(n: usize) -> (AtomTable, Vec<Atom>) {
    let mut atoms = AtomTable::new();
    atoms.seed(["a", "b", "c", "d", "e", "ok"]);
    let keys: Vec<Atom> = (0..n)
        .map(|i| atoms.intern(Box::leak(format!("key{i}").into_boxed_str())))
        .collect();
    (atoms, keys)
}

/// Run `body` `ops` times and print the per-op wall time.
fn run<F: FnMut(usize)>(bucket: &str, ops: usize, mut body: F) {
    // Warm up (page-in, branch predictor, the first-touch of the arena).
    for i in 0..ops.min(10_000) {
        body(i);
    }
    let start = Instant::now();
    for i in 0..ops {
        body(i);
    }
    let elapsed = start.elapsed();
    let ns = elapsed.as_secs_f64() * 1e9 / ops as f64;
    println!("{bucket} {ops} {ns:.3}");
}

fn main() {
    let mut args = std::env::args().skip(1);
    let bucket = args.next().unwrap_or_else(|| "flat32_atom".to_string());
    let ops: usize = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_000_000);

    match bucket.as_str() {
        "flat32_atom" => {
            let (atoms, keys) = table(32);
            let mut arena = Arena::new(1 << 14);
            let keys: Vec<Eterm> = keys.iter().map(|a| make_atom(*a)).collect();
            let map = build_map(&mut arena, &keys, &atoms);
            run(&bucket, ops, |i| {
                let key = keys[i % keys.len()];
                black_box(map_get(map, key, &atoms).expect("get"));
            });
        }
        "flat32_int" => {
            let (atoms, _) = table(4);
            let mut arena = Arena::new(1 << 14);
            let keys: Vec<Eterm> = (0..32).map(make_small).collect();
            let map = build_map(&mut arena, &keys, &atoms);
            run(&bucket, ops, |i| {
                let key = keys[i % keys.len()];
                black_box(map_get(map, key, &atoms).expect("get"));
            });
        }
        "hamt512_atom" => {
            let (atoms, keys) = table(512);
            let mut arena = Arena::new(1 << 18);
            let keys: Vec<Eterm> = keys.iter().map(|a| make_atom(*a)).collect();
            let map = build_map(&mut arena, &keys, &atoms);
            run(&bucket, ops, |i| {
                let key = keys[i % keys.len()];
                black_box(map_get(map, key, &atoms).expect("get"));
            });
        }
        "hamt512_int" => {
            let (atoms, _) = table(4);
            let mut arena = Arena::new(1 << 18);
            let keys: Vec<Eterm> = (0..512).map(make_small).collect();
            let map = build_map(&mut arena, &keys, &atoms);
            run(&bucket, ops, |i| {
                let key = keys[i % keys.len()];
                black_box(map_get(map, key, &atoms).expect("get"));
            });
        }
        "payload_eq" => {
            let (atoms, _) = table(4);
            let mut arena = Arena::new(1 << 14);
            let a = build_payload(&mut arena, &atoms);
            let b = build_payload(&mut arena, &atoms);
            assert!(eq(a, b, &atoms), "the two payload copies must be equal");
            run(&bucket, ops, |_| {
                black_box(eq(a, b, &atoms));
            });
        }
        "payload_hash" => {
            let (atoms, _) = table(4);
            let mut arena = Arena::new(1 << 14);
            let a = build_payload(&mut arena, &atoms);
            run(&bucket, ops, |_| {
                black_box(make_hash2(a, &atoms).expect("hash"));
            });
        }
        "bin_eq" => {
            let (atoms, _) = table(4);
            let mut arena = Arena::new(1 << 10);
            let a = build_binary(&mut arena, b"hello world");
            let b = build_binary(&mut arena, b"hello world");
            assert!(eq(a, b, &atoms));
            run(&bucket, ops, |_| {
                black_box(eq(a, b, &atoms));
            });
        }
        "tuple_eq" => {
            let (atoms, _) = table(4);
            let mut arena = Arena::new(1 << 15);
            let k = make_atom(atoms.lookup("k").expect("k"));
            let pa = build_payload(&mut arena, &atoms);
            let pb = build_payload(&mut arena, &atoms);
            let a = build_tuple(&mut arena, &[k, pa]);
            let b = build_tuple(&mut arena, &[k, pb]);
            assert!(eq(a, b, &atoms));
            run(&bucket, ops, |_| {
                black_box(eq(a, b, &atoms));
            });
        }
        "cmp_atoms" => {
            let (atoms, keys) = table(512);
            let a = make_atom(keys[0]);
            let b = make_atom(keys[511]);
            run(&bucket, ops, |_| {
                black_box(cmp(a, b, &atoms));
            });
        }
        "flat32_unused" => {
            // Compile-check that the order-preserving writer stays reachable for callers
            // that need an image-preserving emit (not timed here).
            let (atoms, _) = table(4);
            let mut arena = Arena::new(1 << 6);
            let hp = arena.alloc(map_words_capacity(1));
            let m = unsafe { write_flatmap(hp, &[(make_small(1), make_small(2))]) };
            black_box(map_get(m, make_small(1), &atoms).expect("get"));
        }
        other => {
            eprintln!("unknown bucket `{other}`");
            std::process::exit(2);
        }
    }
}
