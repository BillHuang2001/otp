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

//! Parity evidence for the **GenImmix** mature generation: the same corpus, built with the
//! same allocation sequence, must produce **identical observable results** through
//! [`AnyCollector::Copying`] (the default, Cheney-copying mature space) and
//! [`AnyCollector::Immix`] (the mark-region mature space).
//!
//! This is the core requirement of the GenImmix option: switching the mature-generation
//! *strategy* may change *where* the data lives and *how* it is reclaimed, but it must never
//! change what the mutator observes. The test therefore reuses the crate's existing corpora —
//! nested tuples + cons lists + boxed leaves + deliberate garbage (`copy_correctness.rs`),
//! a flat map and a >32-key HAMT map (`map_copy.rs`), `Local`/`External`/`Imported` funs with
//! compound free variables (`fun_copy.rs`) and the 8-word `[BinRef][ErlSubBits]` refc-binary
//! pair (`refc_binary_copy.rs`) — and asserts, after *every* collection:
//!
//! * the sequence of [`CollectionKind`]s is identical for both strategies;
//! * every root is relocated out of the nursery into that strategy's mature generation;
//! * the canonical structural form of the whole reachable graph (one walker, below) is
//!   byte-identical between the two strategies.
//!
//! The canonical form is produced by a single structural walker over the copied graph —
//! `beam_term::size::size_object` is deliberately **not** used as the oracle (it has a known
//! HAMT under-count, see the crate `CONTEXT.md`).
//!
//! `unsafe` below only dereferences pointers this test allocated and wrote itself (or that a
//! collection just wrote); every block carries a `// SAFETY:` rationale.

use beam_alloc::{Heap, RefcBinary};
use beam_gc::prelude::{AnyCollector, CollectionKind, GcConfig, HeapPolicy, MatureKind};
use beam_term::prelude::{
    arityval, boxed_val, fun_arity, fun_creator_pid, fun_env, fun_function, fun_index,
    fun_kind_of_header, fun_module, fun_num_free, fun_uniq, fun_uniq_bytes, fun_words,
    is_arity_value, is_atom, is_boxed, is_fun_header, is_list, is_map_header, is_refc_binary,
    is_small, list_val, make_atom, make_small, map_pairs, map_words_capacity, refc_bytes, refc_size,
    signed_value, write_fun_external, write_fun_imported, write_fun_local, write_map,
    write_refc_binary, Atom, AtomTable, Eterm, FunKind, FunPid, NIL, ERL_REFC_BITS_SIZE,
};

// ---------------------------------------------------------------------------
// Corpus geometry (kept deterministic; no timing, no randomness)
// ---------------------------------------------------------------------------

/// Rounds of "allocate a corpus, then collect". With `fullsweep_after = 1` the collection
/// sequence is Minor, Major, Minor, Major, … so both strategies are driven through several
/// minors and several forced fullsweeps.
const ROUNDS: usize = 6;

/// A flat map (`<= MAP_SMALL_MAP_LIMIT`) and a HAMT map (`> MAP_SMALL_MAP_LIMIT`) so both
/// map interiors are exercised.
const FLAT_KEYS: usize = 12;
const HAMT_KEYS: usize = 40;

/// The heap policy for both strategies (identical sizes, so neither path is capacity-tight
/// and the collection *kinds* are driven purely by the fullsweep cadence).
fn policy() -> HeapPolicy {
    HeapPolicy {
        // Force a fullsweep after every minor: the sequence is Minor, Major, Minor, …
        fullsweep_after: 1,
        initial_young_words: 4096,
        initial_mature_words: 8192,
        // Keep the copying collector's freshly-sized fullsweep space comfortably large, so
        // the copying path never escalates by capacity either.
        min_heap_words: 4096,
        ..HeapPolicy::default()
    }
}

// ---------------------------------------------------------------------------
// The corpus: one deterministic allocation sequence, heap-agnostic
// ---------------------------------------------------------------------------

/// The `i -> 3*i` pair set (distinct integer keys ⇒ no collisions).
fn pairs(n: usize) -> Vec<(Eterm, Eterm)> {
    (0..n as i64)
        .map(|i| (make_small(i), make_small(i * 3)))
        .collect()
}

/// Reserves [`map_words_capacity`]`(n)` words — the sound upper bound — and writes the
/// `i -> 3*i` map there. The reservation's trailing slack stays uninitialised and is never
/// referenced by any term.
fn write_map_n(heap: &mut Heap, atoms: &AtomTable, n: usize) -> Eterm {
    let cap = map_words_capacity(n);
    let input = pairs(n);
    // SAFETY: `cap` exclusively-owned, writable words are reserved; `write_map` writes a
    // contiguous prefix of them with well-formed terms and returns a root into that prefix.
    unsafe {
        let hp = heap.alloc_words(cap).expect("reserve the map block");
        write_map(hp, &input, atoms).expect("build the map")
    }
}

/// Writes a `Local` fun (`[header][uniq][index][env…]`) into `heap`.
fn write_local(heap: &mut Heap, arity: usize, uniq: u64, index: u32, env: &[Eterm]) -> Eterm {
    let words = fun_words(FunKind::Local, env.len());
    // SAFETY: `words` exclusively-owned, writable words are reserved and all of them are
    // written by `write_fun_local`; every `env` entry is a live term of this heap.
    unsafe {
        let hp = heap.alloc_words(words).expect("reserve local fun");
        write_fun_local(hp, arity, uniq, index, env).expect("write local fun")
    }
}

/// Writes an `External` fun (`[header][module][function][arity]`) into `heap`.
fn write_external(heap: &mut Heap, arity: usize, module: Atom, function: Atom) -> Eterm {
    let words = fun_words(FunKind::External, 0);
    // SAFETY: the 4 words `write_fun_external` writes are exclusively owned and reserved.
    unsafe {
        let hp = heap.alloc_words(words).expect("reserve external fun");
        write_fun_external(hp, arity, module, function).expect("write external fun")
    }
}

/// Writes an `Imported` fun (the whole `NEW_FUN_EXT` descriptor) into `heap`.
fn write_imported(
    heap: &mut Heap,
    arity: usize,
    module: Atom,
    creator: FunPid,
    env: &[Eterm],
) -> Eterm {
    let words = fun_words(FunKind::Imported, env.len());
    let uniq = [
        0x11u8, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF,
        0x00,
    ];
    // SAFETY: `words` exclusively-owned, writable words are reserved and all of them are
    // written by `write_fun_imported`; every `env` entry is a live term of this heap.
    unsafe {
        let hp = heap.alloc_words(words).expect("reserve imported fun");
        write_fun_imported(hp, arity, module, &uniq, 21, 0, 0, creator, env)
            .expect("write imported fun")
    }
}

/// Writes the 8-word `[BinRef][ErlSubBits]` refc-binary pair for `payload` into `heap`.
fn write_refc(heap: &mut Heap, payload: &RefcBinary) -> Eterm {
    let size_bits = payload.len() * 8;
    // SAFETY: exactly `ERL_REFC_BITS_SIZE` (8) exclusively-owned words are reserved;
    // `payload.data_ptr()` is a live, word-aligned pointer to `size_bits` bits of off-heap
    // storage kept alive by `payload` (held by the caller for the whole test), and `null` is
    // the (empty) off-heap chain link.
    unsafe {
        let hp = heap
            .alloc_words(ERL_REFC_BITS_SIZE)
            .expect("8 words for the pair");
        write_refc_binary(
            hp,
            payload.data_ptr() as *mut Eterm,
            core::ptr::null_mut(),
            payload.data_ptr(),
            0,
            size_bits,
        )
    }
}

/// Builds one round's corpus with a fixed allocation sequence and returns its roots.
///
/// Round 0 is the heavy round (all four term-shape families + deliberate garbage); every
/// later round is a light graph so that the live set grows while the nursery is repeatedly
/// emptied. The off-heap refc-binary payload is pushed onto `payloads` so it stays alive.
fn build_round(
    heap: &mut Heap,
    atoms: &AtomTable,
    module: Atom,
    function: Atom,
    node: Atom,
    payloads: &mut Vec<RefcBinary>,
    round: usize,
) -> Vec<Eterm> {
    let mut roots: Vec<Eterm> = Vec::new();

    if round == 0 {
        // (1) nested tuples + cons lists + boxed leaves, exactly as `copy_correctness.rs`.
        let atom = make_atom(Atom::from_raw(7));
        let cell3 = heap.cons(make_small(3), NIL).expect("cons cell");
        let cell2 = heap.cons(make_small(2), cell3).expect("cons cell");
        let list = heap.cons(make_small(1), cell2).expect("cons cell");
        let inner = heap
            .tuple(&[make_small(4), make_small(5)])
            .expect("inner tuple");
        let outer = heap.tuple(&[atom, list, inner]).expect("outer tuple");
        roots.push(outer);

        // (2) a flat map and (3) a HAMT map.
        roots.push(write_map_n(heap, atoms, FLAT_KEYS));
        roots.push(write_map_n(heap, atoms, HAMT_KEYS));

        // (4) all three fun kinds, with compound free variables.
        let free_tuple = heap
            .tuple(&[make_small(1), make_small(2)])
            .expect("free tuple");
        let free_cons = heap.cons(make_small(3), NIL).expect("free cons");
        roots.push(write_local(
            heap,
            3,
            0xDEAD_BEEF,
            11,
            &[free_tuple, free_cons, make_small(5)],
        ));
        roots.push(write_external(heap, 2, module, function));
        let imported_env = heap
            .tuple(&[make_small(6), make_small(7), make_small(8)])
            .expect("imported env tuple");
        let creator = FunPid {
            node,
            number: 4,
            serial: 5,
            creation: 6,
        };
        roots.push(write_imported(heap, 1, module, creator, &[imported_env]));

        // (5) the 8-word refc-binary pair.
        let payload =
            RefcBinary::new(b"gen-immix parity payload 0123456789").expect("refc payload");
        roots.push(write_refc(heap, &payload));
        payloads.push(payload);

        // (6) deliberate garbage that no root reaches.
        for i in 0..8i64 {
            let _ = heap
                .tuple(&[make_small(900 + i)])
                .expect("garbage tuple");
        }
    } else {
        // A light graph: a tuple, a two-cell list and a `Local` fun holding both.
        let r = round as i64;
        let t = heap
            .tuple(&[make_small(r), make_small(100 + r)])
            .expect("round tuple");
        let tail = heap.cons(make_small(r), NIL).expect("round cons");
        let list = heap.cons(tail, NIL).expect("round cons");
        roots.push(t);
        roots.push(list);
        roots.push(write_local(heap, 1, round as u64, round as u32, &[t, list]));
        // Deliberate garbage.
        let _ = heap.tuple(&[make_small(500 + r)]).expect("garbage tuple");
    }

    roots
}

// ---------------------------------------------------------------------------
// The canonical structural walker (one walker for both strategies)
// ---------------------------------------------------------------------------

/// Appends the canonical structural form of the graph reachable from `term`.
fn walk(term: Eterm, atoms: &AtomTable, out: &mut String) {
    if term == NIL {
        out.push_str("[]");
        return;
    }
    if is_small(term) {
        out.push_str(&format!("i{}", signed_value(term)));
        return;
    }
    if is_atom(term) {
        out.push_str(&format!("a{}", term.raw()));
        return;
    }
    if is_list(term) {
        // SAFETY: `term` is a live cons cell (`is_list`), so both words are readable.
        let cell = list_val(term);
        let (car, cdr) = unsafe { (*cell, *cell.add(1)) };
        out.push('[');
        walk(car, atoms, out);
        out.push('|');
        walk(cdr, atoms, out);
        out.push(']');
        return;
    }
    if is_boxed(term) {
        let p = boxed_val(term);
        // SAFETY: `term` is a live boxed term (`is_boxed`), so its header word is readable.
        let header = unsafe { *p };
        // SAFETY: `term` is a live boxed term.
        if unsafe { is_refc_binary(term) } {
            // SAFETY: `is_refc_binary` just held, so `term` is a live refc binary whose
            // off-heap payload the test keeps alive.
            let (size_bits, bytes) = unsafe { (refc_size(term), refc_bytes(term)) };
            let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            out.push_str(&format!("refc[{size_bits}b:{hex}]"));
            return;
        }
        if is_fun_header(header) {
            walk_fun(term, header, atoms, out);
            return;
        }
        if is_map_header(header) {
            let pairs = map_pairs(term, atoms).expect("map_pairs");
            out.push_str("map{");
            for (k, v) in pairs {
                walk(k, atoms, out);
                out.push_str("=>");
                walk(v, atoms, out);
                out.push(',');
            }
            out.push('}');
            return;
        }
        if is_arity_value(header) {
            let n = arityval(header);
            out.push('{');
            for i in 0..n {
                // SAFETY: the tuple has `n` elements at words `1..=n` of a live object.
                walk(unsafe { *p.add(1 + i) }, atoms, out);
                out.push(',');
            }
            out.push('}');
            return;
        }
        out.push_str(&format!("<thing:{:x}>", header.raw()));
        return;
    }
    out.push_str(&format!("<imm:{:x}>", term.raw()));
}

/// Appends the canonical structural form of a fun: its kind, metadata and environment.
fn walk_fun(term: Eterm, header: Eterm, atoms: &AtomTable, out: &mut String) {
    let kind = fun_kind_of_header(header);
    out.push_str(&format!(
        "fun({kind:?},a{},nf{}",
        fun_arity(header),
        fun_num_free(header)
    ));
    match kind {
        FunKind::Local => {
            // SAFETY: `term` is a live fun of this kind, so its descriptor is readable.
            let (uniq, index) = unsafe {
                (
                    fun_uniq(term).expect("fun uniq"),
                    fun_index(term).expect("fun index"),
                )
            };
            out.push_str(&format!(",uniq{uniq:x},idx{index}"));
        }
        FunKind::External => {
            // SAFETY: `term` is a live fun of this kind, so its descriptor is readable.
            let (m, f) = unsafe {
                (
                    fun_module(term).expect("fun module"),
                    fun_function(term).expect("fun function"),
                )
            };
            out.push_str(&format!(",mod{},fun{}", m.raw(), f.raw()));
        }
        FunKind::Imported => {
            // SAFETY: `term` is a live fun of this kind, so its descriptor is readable.
            let (m, u, index, pid) = unsafe {
                (
                    fun_module(term).expect("fun module"),
                    fun_uniq_bytes(term).expect("fun uniq"),
                    fun_index(term).expect("fun index"),
                    fun_creator_pid(term).expect("fun pid"),
                )
            };
            out.push_str(&format!(",mod{},uniq{:?},idx{index},pid{:?}", m.raw(), u, pid));
        }
    }
    // SAFETY: `term` is a live fun, so its environment words are readable.
    let env = unsafe { fun_env(term) }.expect("fun env");
    out.push_str(",env[");
    for e in env {
        walk(*e, atoms, out);
        out.push(',');
    }
    out.push_str("])");
}

/// The canonical form of a whole root slice.
fn canonical(roots: &[Eterm], atoms: &AtomTable) -> String {
    let mut out = String::new();
    for root in roots {
        walk(*root, atoms, &mut out);
        out.push(';');
    }
    out
}

// ---------------------------------------------------------------------------
// The driver
// ---------------------------------------------------------------------------

/// Whether `term` is resident in the strategy's mature generation.
fn in_mature(c: &AnyCollector, term: Eterm) -> bool {
    match c {
        AnyCollector::Copying(x) => x.mature().contains_term(term),
        AnyCollector::Immix(x) => x.mature().contains_term(term),
    }
}

/// The observable result of one run: the collection kinds and the canonical graph after each
/// collection.
struct Observation {
    kinds: Vec<CollectionKind>,
    snapshots: Vec<String>,
}

/// Builds the corpus round by round against the `kind` strategy and collects after each
/// round, recording the observable results.
fn observe(kind: MatureKind) -> Observation {
    let config = GcConfig::copying(policy()).with_mature(kind);
    let mut c = AnyCollector::from_config(&config).expect("collector");
    assert_eq!(c.kind(), kind, "the config selected the requested strategy");

    let mut table = AtomTable::new();
    table.seed(["mod", "call", "node"]);
    let module = table.intern("mod");
    let function = table.intern("call");
    let node = table.intern("node");

    // Keep the off-heap refc-binary payloads alive for the whole run.
    let mut payloads: Vec<RefcBinary> = Vec::new();
    let mut roots: Vec<Eterm> = Vec::new();
    let mut kinds = Vec::new();
    let mut snapshots = Vec::new();

    for round in 0..ROUNDS {
        // `young_mut().heap_mut()` is the same allocator API for both strategies, so the
        // allocation sequence is identical.
        let fresh = build_round(
            c.young_mut().heap_mut(),
            &table,
            module,
            function,
            node,
            &mut payloads,
            round,
        );
        roots.extend(fresh);

        let collection = c.collect(&mut roots).expect("collect");
        kinds.push(collection.kind);

        // Every root must be relocated out of the nursery into this strategy's mature space.
        for (i, root) in roots.iter().enumerate() {
            assert!(
                !c.young().contains_term(*root),
                "root {i} stayed in the nursery after round {round}"
            );
            assert!(
                in_mature(&c, *root),
                "root {i} was not relocated into the mature generation after round {round}"
            );
        }

        snapshots.push(canonical(&roots, &table));
    }

    assert_eq!(
        payloads.len(),
        1,
        "the refc payload stays alive for the whole run"
    );
    Observation { kinds, snapshots }
}

#[test]
fn both_strategies_produce_identical_observable_results() {
    let copying = observe(MatureKind::Copying);
    let immix = observe(MatureKind::Immix);

    // The fullsweep cadence drives an alternating minor/major sequence for both strategies —
    // several collections, several forced fullsweeps.
    let expected: Vec<CollectionKind> = (0..ROUNDS)
        .map(|r| {
            if r % 2 == 0 {
                CollectionKind::Minor
            } else {
                CollectionKind::Major
            }
        })
        .collect();
    assert_eq!(copying.kinds, expected, "copying collection kinds");
    assert_eq!(
        immix.kinds, expected,
        "immix collection kinds (capacity must not force an escalation)"
    );

    // The core requirement: identical observable results after every collection.
    assert_eq!(
        copying.snapshots, immix.snapshots,
        "the reachable graph must be structurally identical across strategies"
    );

    // Sanity: the corpus really does exercise the term shapes (so the equality above is not
    // vacuous).
    let last = copying.snapshots.last().expect("at least one snapshot");
    assert!(last.contains("map{"), "the flat/HAMT maps are in the graph");
    assert!(last.contains("fun(Local"), "the local fun is in the graph");
    assert!(last.contains("fun(External"), "the external fun is in the graph");
    assert!(last.contains("fun(Imported"), "the imported fun is in the graph");
    assert!(last.contains("refc["), "the refc binary is in the graph");
    assert!(
        !last.contains("i900"),
        "the deliberate round-0 garbage was reclaimed, so it must NOT appear"
    );
}

#[test]
fn default_config_selects_the_copying_path_and_immix_config_the_mark_region_one() {
    // The parity corpus runs through `AnyCollector::from_config`; pin the dispatch itself.
    let copying = AnyCollector::from_config(&GcConfig::default()).expect("collector");
    assert_eq!(copying.kind(), MatureKind::Copying);
    assert!(matches!(copying, AnyCollector::Copying(_)));

    let immix = AnyCollector::from_config(&GcConfig::immix(policy())).expect("collector");
    assert_eq!(immix.kind(), MatureKind::Immix);
    assert!(matches!(immix, AnyCollector::Immix(_)));
}

#[test]
fn the_nursery_is_emptied_after_every_collection_on_both_strategies() {
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let config = GcConfig::copying(policy()).with_mature(kind);
        let mut c = AnyCollector::from_config(&config).expect("collector");
        let mut table = AtomTable::new();
        table.seed(["m", "f"]);
        let module = table.intern("m");
        let function = table.intern("f");
        let node = module;
        let mut payloads = Vec::new();

        let mut roots: Vec<Eterm> = Vec::new();
        for round in 0..ROUNDS {
            let fresh = build_round(
                c.young_mut().heap_mut(),
                &table,
                module,
                function,
                node,
                &mut payloads,
                round,
            );
            roots.extend(fresh);
            c.collect(&mut roots).expect("collect");
            assert_eq!(
                c.young().used_words(),
                0,
                "{kind:?}: the nursery must be empty after round {round}"
            );
        }
        assert!(
            c.stats().minor_collections > 0 && c.stats().major_collections > 0,
            "{kind:?}: both collection kinds must have run"
        );
    }
}
