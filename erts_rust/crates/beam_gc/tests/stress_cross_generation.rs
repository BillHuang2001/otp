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

//! **Cross-generation correctness gate** -- one rich term graph mixing *every* term family the
//! copier knows about, driven through many minor collections and several forced fullsweeps,
//! under **both** mature strategies ([`AnyCollector::Copying`] and [`AnyCollector::Immix`]).
//!
//! The existing tests each cover a slice (`map_copy.rs`/`fun_copy.rs`/`refc_binary_copy.rs`
//! one interior in isolation, `gc_pressure_structures.rs` sharing and cycles, `immix_parity.rs`
//! tuples/lists/maps/funs/a refc binary), but **no single test mixes a `>32`-key HAMT map, all
//! three fun kinds, an on-heap binary, a bitstring, a refc binary, an external pid and an
//! off-heap MSO across generations**. External pids are never exercised as a standalone heap
//! term anywhere else, and an MSO is never driven through a process-heap collection.
//!
//! After **every** collection it asserts:
//!
//! * (a) the reachable graph is structurally unchanged -- the canonical form (below) equals the
//!   form captured *before* the collection, the round-0 subgraph equals a persistent,
//!   never-collected **canonical baseline** built in a private heap, and every baseline root
//!   compares `Equal` under [`compare::eq`] (structural for these shapes, including the
//!   external pid; the hand-made cyclic graph instead goes through its own accessor because
//!   `compare::eq` would recurse forever on a cycle);
//! * (b) every root is *relocated* -- out of the nursery and inside the mature generation;
//! * (c) the shared subterm is a **single** object after every collection and the two-cell
//!   cycle closes on the *copies*; a dedicated test pins the exact
//!   [`GcStats::objects_copied`] for those corpora;
//! * (d) the unreferenced garbage sentinel was dropped (unreachable and never promoted);
//! * and the off-heap objects (a [`Mso`] and the refc binary's payload) are untouched: the
//!   MSO's refcount/payload are unchanged, its slot still resolves, and the refc binary's
//!   on-heap reference word is copied **verbatim** (the off-heap pointer is left alone).
//!
//! The same run under the two strategies must produce the **same collection-kind sequence**,
//! the same root count and the same canonical snapshots.
//!
//! `unsafe` below only dereferences pointers this test allocated and wrote itself (or that a
//! collection just wrote); every block carries a `// SAFETY:` rationale.

use beam_alloc::{Heap, RefcBinary};
use beam_gc::prelude::{
    AnyCollector, CollectionKind, GcConfig, HeapPolicy, MatureKind, Mso, MsoHandle, OffHeap,
    OffHeapEntry,
};
use beam_term::compare;
use beam_term::prelude::{
    arityval, bin_ref_parts, bit_count_to_bytes, bitstring_parts, boxed_val, external_pid_parts,
    external_pid_size_words, fun_arity, fun_creator_pid, fun_env, fun_function, fun_index,
    fun_kind_of_header, fun_module, fun_num_free, fun_uniq, fun_uniq_bytes, fun_words,
    heap_bits_size_words, is_arity_value, is_atom, is_bitstring_header, is_boxed, is_external_pid,
    is_external_pid_header, is_fun_header, is_list, is_map_header, is_refc_binary, is_small,
    list_val, make_atom, make_list, make_small, map_pairs, map_words_capacity, refc_bytes,
    refc_size, signed_value, sub_bits_parts, write_external_pid, write_fun_external,
    write_fun_imported, write_fun_local, write_heap_bits, write_map, write_refc_binary, Atom,
    AtomTable, Eterm, FunKind, FunPid, ERL_REFC_BITS_SIZE, NIL,
};

// Corpus geometry (deterministic; no timing, no randomness)

/// Rounds of "allocate a corpus slice, then collect". With `fullsweep_after = 3` the kind
/// sequence is Minor, Minor, Minor, Major, ... so both strategies are driven through **six
/// minors and two forced fullsweeps**.
const ROUNDS: usize = 8;

/// A flat map (`<= MAP_SMALL_MAP_LIMIT`) and a HAMT map (`> MAP_SMALL_MAP_LIMIT`), so both
/// map interiors are exercised.
const FLAT_KEYS: usize = 12;
const HAMT_KEYS: usize = 40;

/// The on-heap binary and the non-byte-aligned bitstring payloads.
const BIN_BYTES: &[u8] = b"heap binary payload";
const BITS_LEN: usize = 13;
const BITS_BYTES: &[u8] = &[0b1010_1100, 0b0101_1000];

/// The off-heap refc-binary payload and the MSO's apparent size.
const REFC_PAYLOAD: &[u8] = b"stress cross-generation refc payload 0123456789";
const MSO_APPARENT: usize = 4096;

/// The number of unreferenced garbage tuples allocated in round 0.
const GARBAGE: usize = 8;

/// The sentinel marker: the smallest integer appearing in a garbage tuple (`i900`).
const SENTINEL: i64 = 900;

/// The policy for both strategies. Identical sizes mean neither path is capacity-tight, so
/// the collection *kinds* are driven purely by the fullsweep cadence.
///
/// `min_heap_words` is pinned because the C-faithful post-collection resize
/// (`HeapPolicy::young_after_collection`) shrinks a nursery a round leaves mostly empty; the
/// round-0 corpus needs a large, stable nursery.
fn policy() -> HeapPolicy {
    HeapPolicy {
        fullsweep_after: 3,
        initial_young_words: 4096,
        initial_mature_words: 8192,
        min_heap_words: 4096,
        ..HeapPolicy::default()
    }
}

// Off-heap objects (held entirely outside every process heap)

/// The MSO payload. `Mso` is type-erased, so this concrete type is what we downcast back to;
/// `Vec<u32>` keeps it `Any + Send`.
struct MsoPayload {
    tag: u32,
    words: Vec<u32>,
}

impl MsoPayload {
    fn new() -> MsoPayload {
        MsoPayload {
            tag: 0xABCD,
            words: vec![10, 20, 30, 40],
        }
    }
}

// The corpus

/// Which post-collection check a root needs. `compare::eq` is *structural* for tuples,
/// lists, maps, funs, bitstrings, the primitives **and external pids** (ordered through
/// their parts), but a hand-made cyclic graph would make it recurse forever -- that one gets
/// its own accessor, and the canonical walker (which is cycle-safe) covers everything.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RootKind {
    /// Compare structurally with `compare::eq` against the same-index baseline root.
    Eq,
    /// Two tuples sharing one subterm: assert the copied shared node is a single object.
    Shared,
    /// A hand-made two-cell cycle: assert it closes on the copies.
    Cycle,
    /// A tuple whose first element is the off-heap MSO's slot index.
    MsoHolder,
    /// An external pid: `compare` orders it structurally, but this checks its parts directly too.
    Pid,
    /// A root with no baseline (added after round 0): only the canonical pre/post check.
    Extra,
}

/// The shared, immutable corpus context (atoms, off-heap payload, MSO slot).
struct Ctx<'a> {
    table: &'a AtomTable,
    module: Atom,
    function: Atom,
    node: Atom,
    payload: &'a RefcBinary,
    mso_slot: usize,
}

/// Round 0's corpus plus the garbage sentinel it deliberately allocates.
struct Round0 {
    roots: Vec<Eterm>,
    kinds: Vec<RootKind>,
    sentinel: Eterm,
}

/// Reserves [`map_words_capacity`]`(n)` words (the sound upper bound) and writes the `i ->
/// 3*i` map there. The reservation's trailing slack stays uninitialised and is never
/// referenced by any term.
fn write_map_n(heap: &mut Heap, atoms: &AtomTable, n: usize) -> Eterm {
    let cap = map_words_capacity(n);
    let input: Vec<(Eterm, Eterm)> = (0..n as i64)
        .map(|i| (make_small(i), make_small(i * 3)))
        .collect();
    // SAFETY: `cap` exclusively-owned, writable words are reserved; `write_map` writes a
    // contiguous prefix of them with well-formed terms and returns a root into that prefix.
    unsafe {
        let hp = heap.alloc_words(cap).expect("reserve the map block");
        write_map(hp, &input, atoms).expect("build the map")
    }
}

/// Writes a `Local` fun (`[header][uniq][index][env...]`) into `heap`.
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
    // storage kept alive by `payload` (held by the caller for the whole run), and `null` is
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

/// Writes an on-heap bitstring of `n_bits` bits drawn from `bytes`.
fn write_heap_bits_n(heap: &mut Heap, bytes: &[u8], n_bits: usize) -> Eterm {
    let words = heap_bits_size_words(n_bits);
    // SAFETY: `words` exclusively-owned, writable words are reserved and all of them are
    // written by `write_heap_bits`.
    unsafe {
        let hp = heap.alloc_words(words).expect("reserve heap bits");
        write_heap_bits(hp, bytes, n_bits).expect("write heap bits")
    }
}

/// Writes a hand-made two-cell cycle (Erlang cannot build one; raw words prove the copier's
/// forwarding markers close the loop on the *copies* and terminate).
fn write_cycle(heap: &mut Heap) -> Eterm {
    // SAFETY: two 2-word cons cells are reserved and fully written; each cell's cdr is the
    // other's LIST-tagged pointer (a deliberate cycle).
    unsafe {
        let px = heap.alloc_words(2).expect("cell X");
        let py = heap.alloc_words(2).expect("cell Y");
        *px = make_small(1);
        *px.add(1) = make_list(py as *const Eterm);
        *py = make_small(2);
        *py.add(1) = make_list(px as *const Eterm);
        make_list(px as *const Eterm)
    }
}

/// Writes an external pid with chosen `(number, serial, creation)` into `heap` (the only public
/// pid-heap-term constructor).
fn write_pid(heap: &mut Heap, node: Atom, number: u32, serial: u32, creation: u32) -> Eterm {
    // SAFETY: `external_pid_size_words()` (4) exclusively-owned words are reserved and all of
    // them are written by `write_external_pid`; the `node` slot holds a live atom term.
    unsafe {
        let hp = heap
            .alloc_words(external_pid_size_words())
            .expect("reserve pid");
        write_external_pid(hp, make_atom(node), number, serial, creation)
    }
}

/// Builds round 0: every term family, plus deliberate garbage.
fn build_round0(heap: &mut Heap, ctx: &Ctx) -> Round0 {
    let mut roots: Vec<Eterm> = Vec::new();
    let mut kinds: Vec<RootKind> = Vec::new();

    // (1) nested tuples + a cons list + a boxed leaf.
    let atom = make_atom(Atom::from_raw(7));
    let cell3 = heap.cons(make_small(3), NIL).expect("cons cell");
    let cell2 = heap.cons(make_small(2), cell3).expect("cons cell");
    let list = heap.cons(make_small(1), cell2).expect("cons cell");
    let inner = heap
        .tuple(&[make_small(4), make_small(5)])
        .expect("inner tuple");
    let outer = heap.tuple(&[atom, list, inner]).expect("outer tuple");
    roots.push(outer);
    kinds.push(RootKind::Eq);

    // (2) a shared subterm: `A = {S, 10}` and `B = {S, 20}` in one list.
    let s = heap
        .tuple(&[make_small(41), make_small(42)])
        .expect("shared subterm");
    let a = heap.tuple(&[s, make_small(10)]).expect("A");
    let b = heap.tuple(&[s, make_small(20)]).expect("B");
    let tail = heap.cons(b, NIL).expect("tail");
    roots.push(heap.cons(a, tail).expect("shared root"));
    kinds.push(RootKind::Shared);

    // (3) a two-cell cycle.
    roots.push(write_cycle(heap));
    kinds.push(RootKind::Cycle);

    // (4) a flat map and (5) a HAMT map.
    roots.push(write_map_n(heap, ctx.table, FLAT_KEYS));
    kinds.push(RootKind::Eq);
    roots.push(write_map_n(heap, ctx.table, HAMT_KEYS));
    kinds.push(RootKind::Eq);

    // (6) an on-heap binary and (7) a non-byte-aligned on-heap bitstring.
    roots.push(write_heap_bits_n(heap, BIN_BYTES, BIN_BYTES.len() * 8));
    kinds.push(RootKind::Eq);
    roots.push(write_heap_bits_n(heap, BITS_BYTES, BITS_LEN));
    kinds.push(RootKind::Eq);

    // (8) the 8-word refc-binary pair (its off-heap pointer must be copied verbatim).
    roots.push(write_refc(heap, ctx.payload));
    kinds.push(RootKind::Eq);

    // (9) all three fun kinds, with compound free variables.
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
    kinds.push(RootKind::Eq);
    roots.push(write_external(heap, 2, ctx.module, ctx.function));
    kinds.push(RootKind::Eq);
    let imported_env = heap
        .tuple(&[make_small(6), make_small(7), make_small(8)])
        .expect("imported env tuple");
    let creator = FunPid {
        node: ctx.node,
        number: 4,
        serial: 5,
        creation: 6,
    };
    roots.push(write_imported(
        heap,
        1,
        ctx.module,
        creator,
        &[imported_env],
    ));
    kinds.push(RootKind::Eq);

    // (10) an external pid (the shape no other test exercises).
    roots.push(write_pid(heap, ctx.node, 7, 8, 9));
    kinds.push(RootKind::Pid);

    // (11) the MSO holder: the on-heap reference word is the off-heap slot index (the
    // documented `OffHeap::insert -> slot` contract). The collector must copy that word and
    // never follow it into the off-heap object.
    let holder = heap
        .tuple(&[
            make_small(ctx.mso_slot as i64),
            make_atom(Atom::from_raw(9)),
        ])
        .expect("mso holder");
    roots.push(holder);
    kinds.push(RootKind::MsoHolder);

    // (12) deliberate garbage that no root reaches; keep the first as the sentinel.
    let mut sentinel = NIL;
    for i in 0..GARBAGE as i64 {
        let t = heap
            .tuple(&[make_small(SENTINEL + i)])
            .expect("garbage tuple");
        if i == 0 {
            sentinel = t;
        }
    }

    Round0 {
        roots,
        kinds,
        sentinel,
    }
}

/// Builds a later round's light graph: a tuple, a two-cell list and a `Local` fun holding
/// both, plus one dropped garbage tuple.
fn build_round_n(heap: &mut Heap, round: usize) -> Vec<Eterm> {
    let r = round as i64;
    let t = heap
        .tuple(&[make_small(r), make_small(100 + r)])
        .expect("round tuple");
    let tail = heap.cons(make_small(r), NIL).expect("round cons");
    let list = heap.cons(tail, NIL).expect("round cons");
    let f = write_local(heap, 1, round as u64, round as u32, &[t, list]);
    let _ = heap.tuple(&[make_small(500 + r)]).expect("garbage tuple");
    vec![t, list, f]
}

// The canonical structural walker (one walker for both strategies)

/// The recursion stack of container pointers, so a cyclic graph terminates with `<cycle>`
/// instead of looping. Shared (non-cyclic) subterms are rendered in full every time, exactly
/// as `immix_parity.rs` does.
type Path = Vec<usize>;

/// Appends the canonical, **address-free** structural form of the graph reachable from
/// `term`. Address-free is what makes it a valid baseline: it is unchanged by a relocation.
fn walk(term: Eterm, atoms: &AtomTable, out: &mut String, path: &mut Path) {
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
        let key = cell as usize;
        if path.contains(&key) {
            out.push_str("<cycle>");
            return;
        }
        let (car, cdr) = unsafe { (*cell, *cell.add(1)) };
        path.push(key);
        out.push('[');
        walk(car, atoms, out, path);
        out.push('|');
        walk(cdr, atoms, out, path);
        out.push(']');
        path.pop();
        return;
    }
    if is_boxed(term) {
        let p = boxed_val(term);
        let key = p as usize;
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
            walk_fun(term, header, atoms, out, path);
            return;
        }
        if is_map_header(header) {
            let pairs = map_pairs(term, atoms).expect("map_pairs");
            out.push_str("map{");
            for (k, v) in pairs {
                walk(k, atoms, out, path);
                out.push_str("=>");
                walk(v, atoms, out, path);
                out.push(',');
            }
            out.push('}');
            return;
        }
        if is_bitstring_header(header) {
            // SAFETY: `term` is a live heap bitstring, and the heap that owns it is alive
            // and unmoved for the duration of this call.
            let (base, offset, size_bits) = unsafe { bitstring_parts(term) };
            let first = offset / 8;
            let last = bit_count_to_bytes(offset + size_bits);
            let bytes: &[u8] = if last > first {
                // SAFETY: `[first, last)` lies inside the bitstring's storage.
                unsafe { core::slice::from_raw_parts(base.add(first), last - first) }
            } else {
                &[]
            };
            let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            out.push_str(&format!("bin[{size_bits}b:{hex}]"));
            return;
        }
        if is_external_pid_header(header) {
            // SAFETY: `term` is a live external pid written by `write_external_pid` (and, if
            // it moved, copied verbatim by the collector).
            let (node, number, serial, creation) = unsafe { external_pid_parts(term) };
            out.push_str(&format!(
                "pid{{n{},num{number},ser{serial},cre{creation}}}",
                node.raw()
            ));
            return;
        }
        if is_arity_value(header) {
            let n = arityval(header);
            if path.contains(&key) {
                out.push_str("<cycle>");
                return;
            }
            path.push(key);
            out.push('{');
            for i in 0..n {
                // SAFETY: the tuple has `n` elements at words `1..=n` of a live object.
                walk(unsafe { *p.add(1 + i) }, atoms, out, path);
                out.push(',');
            }
            out.push('}');
            path.pop();
            return;
        }
        out.push_str(&format!("<thing:{:x}>", header.raw()));
        return;
    }
    out.push_str(&format!("<imm:{:x}>", term.raw()));
}

/// Appends the canonical structural form of a fun: its kind, metadata and environment.
fn walk_fun(term: Eterm, header: Eterm, atoms: &AtomTable, out: &mut String, path: &mut Path) {
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
            out.push_str(&format!(
                ",mod{},uniq{:?},idx{index},pid{:?}",
                m.raw(),
                u,
                pid
            ));
        }
    }
    // SAFETY: `term` is a live fun, so its environment words are readable.
    let env = unsafe { fun_env(term) }.expect("fun env");
    out.push_str(",env[");
    for e in env {
        walk(*e, atoms, out, path);
        out.push(',');
    }
    out.push_str("])");
}

/// The canonical form of a whole root slice.
fn canonical(roots: &[Eterm], atoms: &AtomTable) -> String {
    let mut out = String::new();
    let mut path: Path = Vec::new();
    for root in roots {
        walk(*root, atoms, &mut out, &mut path);
        out.push(';');
    }
    out
}

// Per-root structural checks (including the cyclic graph `compare::eq` cannot order)

/// Asserts the two tuples still share **one** subterm object (the copied-once evidence) and
/// that the path values are intact.
fn assert_shared(root: Eterm) {
    // SAFETY: `root` is the live, copied `[A | [B | []]]` list this test built.
    let (a, rest) = unsafe {
        let px = list_val(root);
        (*px, *px.add(1))
    };
    assert!(is_list(rest), "the shared root's tail is a list");
    // SAFETY: `rest` is a live cons cell.
    let b = unsafe { *list_val(rest) };
    // SAFETY: `a` and `b` are live tuples `{S, _}`, so element 0 is at word 1.
    let (sa, sb) = unsafe { (*boxed_val(a).add(1), *boxed_val(b).add(1)) };
    assert_eq!(
        sa, sb,
        "the shared subterm must be copied ONCE and stay a single object"
    );
    // SAFETY: `sa` is the live, copied `{41, 42}` tuple's header pointer.
    let (s0, s1) = unsafe { (*boxed_val(sa).add(1), *boxed_val(sa).add(2)) };
    assert_eq!((s0, s1), (make_small(41), make_small(42)), "shared values");
    // SAFETY: `a`/`b` are the live tuples; element 1 holds their distinct path values.
    let (va, vb) = unsafe { (*boxed_val(a).add(2), *boxed_val(b).add(2)) };
    assert_eq!((va, vb), (make_small(10), make_small(20)), "path values");
}

/// Asserts the two-cell cycle is still closed on the *copies*.
fn assert_cycle_closed(root: Eterm) {
    // SAFETY: `root` is the live, copied first cell of the two-cycle.
    let px = list_val(root);
    assert_eq!(unsafe { *px }, make_small(1), "cycle cell X");
    let cy = unsafe { *px.add(1) };
    assert!(is_list(cy), "the cycle's second cell is a list");
    // SAFETY: `cy` is the live, copied second cell.
    let py = list_val(cy);
    assert_eq!(unsafe { *py }, make_small(2), "cycle cell Y");
    assert_eq!(
        unsafe { *py.add(1) },
        root,
        "the two-cycle must close on the copies"
    );
}

/// Asserts the external pid's parts round-trip (its `node` is the sysname atom).
fn assert_pid(root: Eterm, node: Atom) {
    // SAFETY: `root` is a live external pid written by `write_external_pid`.
    assert!(unsafe { is_external_pid(root) }, "root is an external pid");
    // SAFETY: as above.
    let (n, number, serial, creation) = unsafe { external_pid_parts(root) };
    assert_eq!(n, make_atom(node), "pid node");
    assert_eq!((number, serial, creation), (7, 8, 9), "pid payload");
}

/// The `BinRef`'s `val` word of a refc-binary root: the on-heap reference word that must be
/// copied verbatim while the off-heap pointer is left alone.
///
/// The `BinRef` is reached through the `ErlSubBits`'s `orig` slot (the frozen forward
/// pointer) rather than by assuming the two blocks stay adjacent -- the collector may place
/// them anywhere in the to-space.
fn refc_val_word(root: Eterm) -> *const u8 {
    // SAFETY: `root` is a live refc binary (an `ErlSubBits`), so its `orig` slot holds the
    // live `BinRef` this collection just relocated.
    let orig = unsafe { sub_bits_parts(root).3 };
    // SAFETY: `orig` is a live `BinRef` term, so its `val` word is initialised.
    let (val, _next) = unsafe { bin_ref_parts(orig) };
    val as *const u8
}

// The driver

/// Whether `term` is resident in the strategy's mature generation.
fn in_mature(c: &AnyCollector, term: Eterm) -> bool {
    match c {
        AnyCollector::Copying(x) => x.mature().contains_term(term),
        AnyCollector::Immix(x) => x.mature().contains_term(term),
    }
}

/// The observable result of one run.
struct Observation {
    kinds: Vec<CollectionKind>,
    root_count: usize,
    snapshots: Vec<String>,
}

/// Builds the corpus round by round against the `kind` strategy and collects after each
/// round, asserting the whole correctness gate and recording the observable results.
fn observe(kind: MatureKind) -> Observation {
    let config = GcConfig::copying(policy()).with_mature(kind);
    let mut c = AnyCollector::from_config(&config).expect("collector");
    assert_eq!(c.kind(), kind, "the config selected the requested strategy");

    let mut table = AtomTable::new();
    table.seed(["mod", "call", "node"]);
    let module = table.intern("mod");
    let function = table.intern("call");
    let node = table.intern("node");

    // --- off-heap objects: held entirely outside every process heap -------------------
    let payload = RefcBinary::new(REFC_PAYLOAD).expect("refc payload");
    let mso: MsoHandle = Mso::new(MsoPayload::new(), MSO_APPARENT);
    let mut off_heap = OffHeap::new();
    let mso_slot = off_heap.insert(mso.clone());
    off_heap.push(payload.clone());
    let mso_refcount = mso.refcount();
    assert_eq!(
        mso_refcount, 2,
        "the test handle plus the off-heap list's clone"
    );
    assert_eq!(off_heap.len(), 2, "the MSO and the refc binary payload");
    assert_eq!(
        off_heap.overhead_bytes(),
        MSO_APPARENT + payload.len(),
        "the off-heap accounting is the summed payload bytes"
    );

    let ctx = Ctx {
        table: &table,
        module,
        function,
        node,
        payload: &payload,
        mso_slot,
    };

    // --- the persistent, never-collected canonical baseline ---------------------------
    // A second copy of round 0 in a private heap that is never collected, so `compare::eq`
    // can compare each live root against a stable baseline after every collection.
    let mut baseline_heap = Heap::new(8192).expect("baseline heap");
    let baseline = build_round0(&mut baseline_heap, &ctx);
    let baseline_len = baseline.roots.len();
    let baseline_canonical = canonical(&baseline.roots, &table);

    // --- the rounds -------------------------------------------------------------------
    let mut roots: Vec<Eterm> = Vec::new();
    let mut root_kinds: Vec<RootKind> = Vec::new();
    let mut kinds: Vec<CollectionKind> = Vec::new();
    let mut snapshots: Vec<String> = Vec::new();
    let mut sentinel = NIL;

    for round in 0..ROUNDS {
        if round == 0 {
            let r0 = build_round0(c.young_mut().heap_mut(), &ctx);
            sentinel = r0.sentinel;
            roots.extend(r0.roots);
            root_kinds.extend(r0.kinds);
        } else {
            let fresh = build_round_n(c.young_mut().heap_mut(), round);
            root_kinds.extend(std::iter::repeat(RootKind::Extra).take(fresh.len()));
            roots.extend(fresh);
        }

        let before = canonical(&roots, &table);
        let collection = c.collect(&mut roots).expect("collect");
        kinds.push(collection.kind);
        let after = canonical(&roots, &table);

        // (a) the graph is structurally unchanged across the collection.
        assert_eq!(
            after, before,
            "{kind:?}: round {round}: the reachable graph changed across the collection"
        );
        assert_eq!(
            canonical(&roots[..baseline_len], &table),
            baseline_canonical,
            "{kind:?}: round {round}: the round-0 subgraph drifted from the canonical baseline"
        );

        // (b) every root is relocated out of the nursery into the mature generation.
        for (i, root) in roots.iter().enumerate() {
            assert!(
                !c.young().contains_term(*root),
                "{kind:?}: round {round}: root {i} stayed in the nursery"
            );
            assert!(
                in_mature(&c, *root),
                "{kind:?}: round {round}: root {i} was not relocated into the mature generation"
            );
        }

        // (a)/(c) per-root structural checks against the baseline (including the cyclic
        // graph `compare::eq` cannot order).
        for i in 0..baseline_len {
            match root_kinds[i] {
                RootKind::Eq => assert!(
                    compare::eq(roots[i], baseline.roots[i], &table),
                    "{kind:?}: round {round}: root {i} is not structurally equal to its baseline"
                ),
                RootKind::MsoHolder => {
                    assert!(
                        compare::eq(roots[i], baseline.roots[i], &table),
                        "{kind:?}: round {round}: the MSO holder is not structurally equal"
                    );
                    assert_mso_holder(roots[i], &off_heap, mso_slot);
                }
                RootKind::Shared => assert_shared(roots[i]),
                RootKind::Cycle => assert_cycle_closed(roots[i]),
                RootKind::Pid => assert_pid(roots[i], node),
                RootKind::Extra => {}
            }
        }

        // (c) the refc binary's on-heap reference word is copied verbatim: the off-heap
        // pointer is left exactly as written (the collector never follows it).
        assert_eq!(
            refc_val_word(roots[REFC_ROOT]),
            payload.data_ptr() as *const u8,
            "{kind:?}: round {round}: the BinRef's off-heap pointer must be verbatim"
        );

        // The off-heap objects themselves are untouched by the collection.
        assert_eq!(
            mso.refcount(),
            mso_refcount,
            "{kind:?}: round {round}: the collector must not change the MSO refcount"
        );
        assert_eq!(off_heap.len(), 2, "off-heap entries survive");
        assert_eq!(
            off_heap.sweep(|_| true),
            0,
            "{kind:?}: round {round}: nothing is reclaimed while the caller reports all live"
        );
        assert_eq!(off_heap.len(), 2, "off-heap entries survive the sweep");
        let mso_payload = mso
            .downcast_ref::<MsoPayload>()
            .expect("the MSO payload is intact");
        assert_eq!(
            (mso_payload.tag, mso_payload.words.as_slice()),
            (0xABCD, &[10, 20, 30, 40][..]),
            "{kind:?}: round {round}: the MSO payload is unchanged"
        );

        // (d) the unreferenced sentinel was dropped and never promoted.
        assert!(
            !after.contains(&format!("i{SENTINEL}")),
            "{kind:?}: round {round}: the garbage sentinel must be unreachable"
        );
        assert!(
            !c.young().contains_term(sentinel),
            "{kind:?}: round {round}: the sentinel must not survive in the nursery"
        );
        assert!(
            !in_mature(&c, sentinel),
            "{kind:?}: round {round}: the unreferenced sentinel must not be promoted"
        );

        snapshots.push(after);
    }

    assert!(
        c.stats().minor_collections > 0 && c.stats().major_collections > 0,
        "{kind:?}: both collection kinds must have run"
    );

    Observation {
        kinds,
        root_count: roots.len(),
        snapshots,
    }
}

/// The index of the refc-binary root inside round 0's corpus (see `build_round0`).
const REFC_ROOT: usize = 7;

/// Asserts the MSO holder's on-heap reference word still resolves the off-heap MSO.
fn assert_mso_holder(holder: Eterm, off_heap: &OffHeap, expected_slot: usize) {
    // SAFETY: `holder` is the live `{slot, atom}` tuple this test built.
    let slot_term = unsafe { *boxed_val(holder).add(1) };
    assert!(is_small(slot_term), "the MSO slot is a small int");
    let slot = signed_value(slot_term) as usize;
    assert_eq!(slot, expected_slot, "the on-heap MSO slot is stable");
    match off_heap.get(slot) {
        Some(OffHeapEntry::Mso(h)) => {
            assert_eq!(h.apparent_size(), MSO_APPARENT, "MSO apparent size");
            let p = h.downcast_ref::<MsoPayload>().expect("MSO payload type");
            assert_eq!(p.tag, 0xABCD, "MSO payload tag");
        }
        _ => panic!("the MSO slot must resolve to the off-heap MSO"),
    }
}

// Tests

/// The core gate: the rich corpus survives every collection under both strategies and the
/// two strategies agree on the collection kinds, the root count and every snapshot.
#[test]
fn rich_corpus_survives_cross_generation_collections_under_both_strategies() {
    let copying = observe(MatureKind::Copying);
    let immix = observe(MatureKind::Immix);

    // Six minors and two forced fullsweeps, driven purely by the cadence.
    let expected: Vec<CollectionKind> = (0..ROUNDS)
        .map(|r| {
            if r % 4 == 3 {
                CollectionKind::Major
            } else {
                CollectionKind::Minor
            }
        })
        .collect();
    assert_eq!(copying.kinds, expected, "copying collection kinds");
    assert_eq!(
        immix.kinds, expected,
        "immix collection kinds (capacity must not force an escalation)"
    );

    // The same corpus and the same cadence => identical observables.
    assert_eq!(
        copying.root_count, immix.root_count,
        "identical root count across strategies"
    );
    assert_eq!(
        copying.snapshots, immix.snapshots,
        "the reachable graph must be structurally identical across strategies"
    );

    // The corpus really does exercise every family (so the equality above is not vacuous).
    let last = copying.snapshots.last().expect("at least one snapshot");
    assert!(last.contains("map{"), "the flat/HAMT maps are in the graph");
    assert!(last.contains("fun(Local"), "the local fun is in the graph");
    assert!(
        last.contains("fun(External"),
        "the external fun is in the graph"
    );
    assert!(
        last.contains("fun(Imported"),
        "the imported fun is in the graph"
    );
    assert!(last.contains("refc["), "the refc binary is in the graph");
    assert!(
        last.contains("bin["),
        "the on-heap binary/bitstring is in the graph"
    );
    assert!(last.contains("pid{"), "the external pid is in the graph");
    assert!(last.contains("<cycle>"), "the cyclic graph is in the graph");
    assert!(
        last.contains("i0"),
        "the MSO holder's slot word is in the graph"
    );
    assert!(
        !last.contains(&format!("i{SENTINEL}")),
        "the deliberate garbage was reclaimed, so it must NOT appear"
    );
}

/// (c) the shared-once evidence: an exact object count for a small shared corpus.
#[test]
fn a_shared_subterm_is_copied_exactly_once() {
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let config = GcConfig::copying(policy()).with_mature(kind);
        let mut c = AnyCollector::from_config(&config).expect("collector");

        let mut roots: Vec<Eterm> = Vec::new();
        {
            let heap = c.young_mut().heap_mut();
            let s = heap.tuple(&[make_small(41), make_small(42)]).expect("S");
            let a = heap.tuple(&[s, make_small(10)]).expect("A");
            let b = heap.tuple(&[s, make_small(20)]).expect("B");
            let tail = heap.cons(b, NIL).expect("tail");
            roots.push(heap.cons(a, tail).expect("shared root"));
        }

        // Exactly five objects are live: S, A, B, the tail cons and the root cons. A
        // duplicated shared object would inflate this to six.
        let stats = c.collect_minor(&mut roots).expect("minor");
        assert_eq!(
            stats.objects_copied, 5,
            "{kind:?}: the shared subterm must be copied ONCE"
        );
        assert_eq!(c.young().used_words(), 0, "{kind:?}: nursery emptied");

        assert_shared(roots[0]);
    }
}

/// (c) the cycle evidence with an exact count too: the two cells are copied once each and
/// the loop closes on the copies.
#[test]
fn a_two_cell_cycle_closes_on_the_copies() {
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let config = GcConfig::copying(policy()).with_mature(kind);
        let mut c = AnyCollector::from_config(&config).expect("collector");

        let mut roots: Vec<Eterm> = vec![write_cycle(c.young_mut().heap_mut())];
        let stats = c.collect_minor(&mut roots).expect("minor");
        assert_eq!(
            stats.objects_copied, 2,
            "{kind:?}: the two cycle cells are copied once each"
        );
        assert!(in_mature(&c, roots[0]), "{kind:?}: the cycle moved");
        assert_cycle_closed(roots[0]);
    }
}

/// The off-heap MSO is never interpreted by the collector: only the on-heap slot word is
/// copied, and the object survives a minor, a promotion and several fullsweeps untouched.
#[test]
fn an_off_heap_mso_is_untouched_by_every_collection() {
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let config = GcConfig::copying(policy()).with_mature(kind);
        let mut c = AnyCollector::from_config(&config).expect("collector");

        let mut off_heap = OffHeap::new();
        let payload = RefcBinary::new(REFC_PAYLOAD).expect("payload");
        let mso = Mso::new(MsoPayload::new(), MSO_APPARENT);
        let slot = off_heap.insert(mso.clone());
        off_heap.push(payload.clone());

        let mut roots = Vec::new();
        {
            let heap = c.young_mut().heap_mut();
            roots.push(write_refc(heap, &payload));
            roots.push(
                heap.tuple(&[make_small(slot as i64), make_atom(Atom::from_raw(9))])
                    .expect("holder"),
            );
        }

        for round in 0..ROUNDS {
            c.collect(&mut roots).expect("collect");
            assert_eq!(mso.refcount(), 2, "{kind:?}: round {round}: refcount");
            assert_eq!(off_heap.len(), 2, "{kind:?}: round {round}: entries");
            assert_eq!(
                off_heap.sweep(|_| true),
                0,
                "{kind:?}: round {round}: sweep"
            );
            assert_mso_holder(roots[1], &off_heap, slot);
            assert_eq!(
                refc_val_word(roots[0]),
                payload.data_ptr() as *const u8,
                "{kind:?}: round {round}: verbatim off-heap pointer"
            );
        }
    }
}

/// `compare::eq` orders an external pid **structurally through its parts** (the C `erts_cmp`
/// field order: node, then number/serial/creation), so a pid is genuinely compared through
/// its parts and a copy compares `=:=` its original. This pins equality for identical parts,
/// inequality for a differing number or node, and that the parts stay readable through the
/// `external_pid_parts` accessor.
#[test]
fn an_external_pid_compares_through_its_parts() {
    let mut table = AtomTable::new();
    table.seed(["node", "other_node"]);
    let node = table.intern("node");
    let other_node = table.intern("other_node");

    let mut heap = Heap::new(64).expect("heap");
    let p1 = write_pid(&mut heap, node, 7, 8, 9);
    let p2 = write_pid(&mut heap, node, 7, 8, 9);
    assert!(is_boxed(p1) && is_boxed(p2));
    assert!(
        compare::eq(p1, p2, &table),
        "two pids with identical parts compare structurally equal"
    );
    assert_pid(p1, node);
    assert_pid(p2, node);

    // A different `number` (every other part equal) compares unequal.
    let other_number = write_pid(&mut heap, node, 42, 8, 9);
    assert!(
        !compare::eq(p1, other_number, &table),
        "a pid with a different number must compare unequal"
    );

    // A different `node` (every other part equal) compares unequal.
    let other_node_pid = write_pid(&mut heap, other_node, 7, 8, 9);
    assert!(
        !compare::eq(p1, other_node_pid, &table),
        "a pid with a different node must compare unequal"
    );
}
