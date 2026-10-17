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

//! Integration test — the **full cross-class Erlang term order** (`cmp`/`cmp_arith`,
//! contract A.1).
//!
//! `src/compare.rs`'s [`class_rank`] assigns every [`TermClass`] a rank and
//! `cmp_impl` compares two *different* classes by it, so the class ladder is what
//! `lists:sort/1`, `ordered_set` tables and the `<`/`>` BIFs see for mixed-class
//! operands. The pin here is the complete ladder of
//! `erts/emulator/beam/erl_term.h:1441-1458`'s `*_DEF` virtual tags (term order
//! ascends as `_DEF` descends, `rank = 0x10 - _DEF`), with the C's
//! `BIN_REF_DEF` (`0x11`, documented there as "not a real term") last:
//!
//! ```text
//! small < big < float < atom < external_ref < ref < fun < external_port < port <
//! external_pid < pid < tuple < record < map < nil < list < bitstring < bin_ref
//! ```
//!
//! What this file adds over `tests/compare_terms.rs`'s
//! `ordering_matches_the_documented_class_order` (which covers only
//! `number < atom < ref < port < pid < tuple < map < nil < list < bitstring`):
//!
//! 1. [`the_full_cross_class_ladder_is_pinned`] — every rung, adjacency **and** the
//!    whole `i<j ⇒ Less`, `i==j ⇒ Equal`, `i>j ⇒ Greater` matrix.
//! 2. [`the_numeric_rung_is_rank_ordered_in_exact_mode_and_numeric_in_arithmetic_mode`]
//!    — the three numeric classes are three *ranks* for `=:=` ordering and one
//!    *numeric* rung for `==` ordering (the C's `exact_fall_through:` vs the
//!    `mixed_types` number cases).
//! 3. [`every_fun_kind_sits_in_the_fun_rung`] — `Local`/`Imported`/`External` funs
//!    all occupy the `fun` rung (`Local` < `External` is a *within-class* rule).
//! 4. [`a_refc_binary_is_a_bitstring_and_the_raw_bin_ref_word_sorts_last`] — an
//!    off-heap refc binary roots at an `ErlSubBits`, which the C's `tag_val_def`
//!    classifies as `BITSTRING_DEF` (`erl_term.h:1535-1536`), so it sits in the
//!    `bitstring` rung; only the *inner* `BinRef` word carries `BIN_REF_DEF`.
//! 5. [`internal_and_external_boundary_pairs`] — the six mixed internal/external
//!    pairings (`utils.c`'s `pid_common`/`ref_common` arms and its `mixed_types`
//!    fallbacks).
//! 6. **The local-node-aware mixed arms** (sections 6 below) — the whole reason
//!    [`cmp_with_local_node`]/[`cmp_arith_with_local_node`] exist: a mixed
//!    internal/external `pid`/`port`/`ref` pair compared **field by field** with the
//!    local node supplied (serial → number → node; port node only; ref node → ids),
//!    both operand orders, plus the proof that the context changes nothing else.
//! 7. [`oracle_agrees_on_the_mixed_internal_external_pairs`] and its siblings — the
//!    same field precedences pinned on the real runtime, plus what the oracle does
//!    with a crafted **local-node** external pid/ref (`binary_to_term/1` demotes it
//!    to the internal form — detected through `erts_debug:size/1` — while a differing
//!    `creation` keeps it external).
//! 8. [`oracle_agrees_on_the_cross_class_ladder`] — the crate-local **system-`erl`
//!    oracle cross-check (the same text-protocol convention as `tests/oracle.rs` and
//!    `tests/compare_external.rs`): the real runtime builds one term per rung
//!    (external things by decoding hand-written ETF with `binary_to_term/1`, the
//!    record with `records:create/4`) and its `=:=`/`<`/`>` are compared against this
//!    crate's `eq`/`cmp_arith`. If `erl` is not on `PATH` (or does not answer within
//!    [`ERL_TIMEOUT`]) that check **skips loudly** and returns; it never fails the
//!    suite on absence. The `bin_ref` rung has no oracle sample (a `BinRef` is not a
//!    constructible term).
//!
//! ## Note on mixed internal/external pairs
//!
//! `pid`, `port` and `ref` each have an internal and an external representation. For
//! a pair of the **same** category the C does not use the `_DEF` ladder: its
//! `_TAG_HEADER_EXTERNAL_PID`/`_TAG_HEADER_EXTERNAL_PORT`/`_TAG_HEADER_(EXTERNAL_)REF`
//! cases all jump to `pid_common` (`utils.c:1777`) or `ref_common` (`:2262`),
//! reached from the external cases at `:2212`/`:2235`/`:2288`; both compare the
//! fields — `serial`, `number`, then `CMP_NODES` (sysname atom, then `creation`)
//! — so the *node names decide*, not the ranks.
//!
//! Section 5 below pins those six pairings under the **context-free** `cmp`, which
//! compares them by [`class_rank`] (`external_*` below `*`); with [`ANODE`] sorting
//! before the local node the two rules happen to agree, which is why those samples
//! pass either way. `cmp` keeps the ladder deliberately: an internal word carries no
//! node, so without one there is nothing to substitute for `erts_this_node`.
//! [`cmp_with_local_node`] is the entry point that *does* have the node; sections 6
//! and 7 pin its field-by-field arms, and [`the_local_node_context_changes_nothing_else`]
//! proves it agrees with `cmp` on every other pair.
use std::cmp::Ordering;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use beam_term::prelude::*;

/// Wall-clock budget for a single `erl` invocation before the oracle is declared
/// unusable and the affected check skips.
const ERL_TIMEOUT: Duration = Duration::from_secs(30);

/// The sysname of the "local" foreign node used by every external thing. It must
/// **differ** from the oracle's own `nonode@nohost` (otherwise the oracle would
/// decode the bytes into an internal pid/port/ref) and it sorts **before** it
/// (`a` < `n`), so a mixed internal/external pair orders the same way under the
/// C's node-driven `pid_common`/`ref_common` arms and under this crate's class
/// rank — see the module docs.
const ANODE: &str = "anode@ahost";

/// The real runtime's own sysname (`node()` under `erl -noshell`), i.e. the node a
/// [`LocalNodeId`] names when the mixed arms are pinned on the oracle.
const LOCAL_SYSNAME: &str = "nonode@nohost";

/// A foreign sysname sorting **after** [`LOCAL_SYSNAME`] (`z` > `n`), so a mixed pair
/// whose node is the only difference orders *against* the class ladder.
const ZNODE: &str = "zzz@zzz";

/// Every atom name the [`AtomTable`] must know.
const ATOM_NAMES: &[&str] = &[
    "a",
    "b",
    "mymod",
    "myrec",
    ANODE,
    LOCAL_SYSNAME,
    ZNODE,
];
// ETF tags — `erts/emulator/beam/external.h`.
/// `VERSION_MAGIC`, the byte `binary_to_term/1` requires first.
const VERSION_MAGIC: u8 = 131;
/// `NEW_PID_EXT` (`'X'`, 88).
const NEW_PID_EXT: u8 = b'X';
/// `NEWER_REFERENCE_EXT` (`'Z'`, 90).
const NEWER_REFERENCE_EXT: u8 = b'Z';
/// `V4_PORT_EXT` (`'x'`, 120).
const V4_PORT_EXT: u8 = b'x';
/// `ATOM_EXT` (`'d'`, 100) — a latin1 atom with a 16-bit length. Every name used
/// here is ASCII, so latin1 is exact.
const ATOM_EXT: u8 = b'd';

// ---------------------------------------------------------------------------
// Rust-side builders
// ---------------------------------------------------------------------------

/// The [`AtomTable`] shared by every Rust-side comparison.
fn atoms() -> AtomTable {
    let mut t = AtomTable::new();
    t.seed(ATOM_NAMES.iter().copied());
    t
}

/// The off-heap payload behind the refc-binary sample: one word whose **first two
/// bytes** are `0x01`, `0x02` whatever the host's endianness (the payload's bytes
/// are read as stored), so its bits equal the on-heap `<<1,2>>` binary's.
fn refc_payload() -> &'static mut [u64; 1] {
    Box::leak(Box::new([u64::from_le_bytes([1, 2, 0, 0, 0, 0, 0, 0])]))
}

/// Builds one term per rung of the documented class ladder, in ascending `cmp`
/// order, and returns it with the rung's name.
///
/// The heap is a fixed-capacity bump arena that never reallocates, so every
/// returned term stays valid (and unmoved) for as long as `heap` is alive.
fn ladder(heap: &mut Heap, atoms: &AtomTable) -> Vec<(&'static str, Eterm)> {
    let atom = |name: &str| make_atom(atoms.lookup(name).expect("atom name is seeded"));
    let mut rungs: Vec<(&'static str, Eterm)> = Vec::new();

    // --- small < big < float (three ranks for `cmp`, one numeric rung for
    // `cmp_arith` — see the dedicated test) ---
    rungs.push(("small", make_small(1)));
    // 0x5678_0000_0000_0000_0000 + 0x1234: two limbs, far outside the small range.
    // SAFETY: `heap` hands out `big_size_words(2)` exclusively-owned, writable words;
    // `write_big` writes all of them and they stay live with `heap`.
    let bignum = unsafe {
        let hp = heap.alloc_words(big_size_words(2)).expect("heap room");
        write_big(hp, false, &[0x1234, 0x5678])
    };
    rungs.push(("big", bignum));
    // SAFETY: `heap` hands out `FLOAT_SIZE_WORDS` (2) writable words; `write_float`
    // writes both.
    let float = unsafe {
        let hp = heap.alloc_words(FLOAT_SIZE_WORDS).expect("heap room");
        write_float(hp, 1.5)
    };
    rungs.push(("float", float));

    // --- atom ---
    rungs.push(("atom", atom("a")));

    // --- external_ref < ref ---
    // SAFETY: `heap` hands out `external_ref_size_words(3)` writable words;
    // `write_external_ref` writes all of them.
    let external_ref = unsafe {
        let hp = heap
            .alloc_words(external_ref_size_words(3))
            .expect("heap room");
        write_external_ref(hp, atom(ANODE), &[0, 0, 1], 3)
    };
    rungs.push(("external_ref", external_ref));
    // SAFETY: `heap` hands out `INTERNAL_REF_WORDS` (4) writable words; the sanctioned
    // M1 writer fills them (see the crate `Handoffs`: never hand-write these words).
    let internal_ref = unsafe {
        let hp = heap.alloc_words(INTERNAL_REF_WORDS).expect("heap room");
        write_internal_ref(hp, 7).expect("reference id fits a small")
    };
    rungs.push(("ref", internal_ref));

    // --- fun (the `Local` kind; the other two kinds are pinned separately) ---
    // SAFETY: `heap` hands out `fun_words(Local, 0)` (3) writable words;
    // `write_fun_local` writes all of them.
    let fun = unsafe {
        let hp = heap
            .alloc_words(fun_words(FunKind::Local, 0))
            .expect("heap room");
        write_fun_local(hp, 0, 3, 1, &[]).expect("arity/num_free fit")
    };
    rungs.push(("fun", fun));

    // --- external_port < port ---
    // SAFETY: `heap` hands out `external_port_size_words()` writable words;
    // `write_external_port` writes all of them.
    let external_port = unsafe {
        let hp = heap
            .alloc_words(external_port_size_words())
            .expect("heap room");
        write_external_port(hp, atom(ANODE), 0, 0)
    };
    rungs.push(("external_port", external_port));
    rungs.push(("port", make_internal_port(make_port_data(0))));

    // --- external_pid < pid ---
    // SAFETY: `heap` hands out `external_pid_size_words()` writable words;
    // `write_external_pid` writes all of them.
    let external_pid = unsafe {
        let hp = heap
            .alloc_words(external_pid_size_words())
            .expect("heap room");
        write_external_pid(hp, atom(ANODE), 0, 0, 0)
    };
    rungs.push(("external_pid", external_pid));
    rungs.push(("pid", make_internal_pid(make_pid_data(0, 0))));

    // --- tuple < record < map < nil < list < bitstring ---
    rungs.push(("tuple", heap.tuple(&[atom("a")]).expect("heap room")));
    // SAFETY: `heap` hands out `record_block_words(1)` writable words; `write_record`
    // writes every one of them. The `definition_hash` word is not compared, so the
    // placeholder is fine here.
    let record = unsafe {
        let hp = heap.alloc_words(record_block_words(1)).expect("heap room");
        write_record(
            hp,
            atoms.lookup("mymod").expect("module atom"),
            atoms.lookup("myrec").expect("record name atom"),
            true,
            &[(atom("a"), make_small(1))],
            make_small(0),
        )
        .expect("write_record")
    };
    rungs.push(("record", record));
    // SAFETY: `heap` hands out `map_words_capacity(1)` writable words; `write_map`
    // writes exactly the produced flat map.
    let map = unsafe {
        let hp = heap.alloc_words(map_words_capacity(1)).expect("heap room");
        write_map(hp, &[(atom("a"), make_small(1))], atoms).expect("write_map")
    };
    rungs.push(("map", map));
    rungs.push(("nil", NIL));
    rungs.push(("list", heap.cons(make_small(1), NIL).expect("heap room")));
    // SAFETY: `heap` hands out `heap_bits_size_words(16)` (3) writable words;
    // `write_binary` writes all of them.
    let bitstring = unsafe {
        let hp = heap
            .alloc_words(heap_bits_size_words(16))
            .expect("heap room");
        write_binary(hp, &[1, 2]).expect("no more bytes than `n_bits`")
    };
    rungs.push(("bitstring", bitstring));

    // --- bin_ref (the C's `BIN_REF_DEF`, "not a real term") ---
    // The full 8-word refc pair, whose root is the `ErlSubBits` at `hp + 3`; the bare
    // `BinRef` word is `make_boxed(hp)` — the only thing that classifies as
    // `TermClass::BinRef` (see `a_refc_binary_is_a_bitstring_...`).
    let payload = refc_payload();
    let base = payload.as_ptr().cast::<u8>();
    // SAFETY: `heap` hands out `ERL_REFC_BITS_SIZE` (8) writable words; the caller
    // contract of `write_refc_binary` is that `base`/`val` address a live, word-aligned
    // off-heap payload (`refc_payload`'s one leaked word covers the 16 bits used here).
    let bin_ref = unsafe {
        let hp = heap.alloc_words(ERL_REFC_BITS_SIZE).expect("heap room");
        write_refc_binary(
            hp,
            base.cast_mut().cast::<Eterm>(),
            std::ptr::null_mut(),
            base,
            0,
            16,
        );
        make_boxed(hp)
    };
    rungs.push(("bin_ref", bin_ref));

    rungs
}

// ---------------------------------------------------------------------------
// 1. the whole ladder
// ---------------------------------------------------------------------------

/// Every rung of the documented class order is strictly ascending under `cmp`
/// (exact): adjacent pairs, and the full `i<j ⇒ Less` / `i==j ⇒ Equal` / `i>j ⇒
/// Greater` matrix. `bin_ref` — the C's "not a real term" class — is last.
#[test]
fn the_full_cross_class_ladder_is_pinned() {
    let table = atoms();
    let mut heap = Heap::new(256);
    let rungs = ladder(&mut heap, &table);
    assert_eq!(
        rungs.len(),
        18,
        "the ladder changed; update the module docs"
    );

    // Adjacent rungs are strictly ascending (a `Less` also rules out a tie).
    for w in rungs.windows(2) {
        let (lo, hi) = (&w[0], &w[1]);
        assert_eq!(
            cmp(lo.1, hi.1, &table),
            Ordering::Less,
            "expected {} < {}",
            lo.0,
            hi.0,
        );
        assert_ne!(
            cmp(hi.1, lo.1, &table),
            Ordering::Less,
            "expected {} > {}",
            hi.0,
            lo.0,
        );
    }

    // The whole matrix, both directions.
    for i in 0..rungs.len() {
        for j in 0..rungs.len() {
            let (x, y) = (rungs[i], rungs[j]);
            let expected = i.cmp(&j);
            assert_eq!(
                cmp(x.1, y.1, &table),
                expected,
                "cmp({}, {}) should be {expected:?}",
                x.0,
                y.0,
            );
            assert_eq!(
                eq(x.1, y.1, &table),
                i == j,
                "eq({}, {}) should be {}",
                x.0,
                y.0,
                i == j,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 2. the numeric rung: three ranks for `=:=`, one numeric rung for `==`
// ---------------------------------------------------------------------------

/// The three numeric classes are **three ranks** in exact mode (the C's
/// `exact_fall_through:` → `j = b_tag - a_tag`: `SMALL_DEF` `0x10`, `BIG_DEF`
/// `0x0f`, `FLOAT_DEF` `0x0e`) and **one numeric rung** in arithmetic mode (the
/// `mixed_types` `SMALL_FLOAT`/`BIG_FLOAT` cases), where the exact integer ↔ float
/// algorithms of `src/compare/numbers.rs` decide. Both sit below every non-number
/// class.
#[test]
fn the_numeric_rung_is_rank_ordered_in_exact_mode_and_numeric_in_arithmetic_mode() {
    let table = atoms();
    let mut heap = Heap::new(64);
    let rungs = ladder(&mut heap, &table);
    let (small, big, float) = (rungs[0].1, rungs[1].1, rungs[2].1);
    let atom = rungs[3].1;
    // The samples: 1, 0x5678_0000_0000_0000_0000 + 0x1234, and 1.5.
    assert_eq!(signed_value(small), 1);

    // Exact (`=:=` order): small < big < float, whatever the values are.
    assert_eq!(cmp(small, big, &table), Ordering::Less);
    assert_eq!(cmp(big, float, &table), Ordering::Less);
    assert_eq!(cmp(float, big, &table), Ordering::Greater);
    assert_eq!(cmp(small, float, &table), Ordering::Less);
    assert!(!eq(big, float, &table));

    // Arithmetic (`==` order): numeric — 1 < 1.5 < the bignum.
    assert_eq!(cmp_arith(small, float, &table), Ordering::Less);
    assert_eq!(cmp_arith(float, big, &table), Ordering::Less);
    assert_eq!(cmp_arith(big, float, &table), Ordering::Greater);

    // Every numeric class sorts below the atom rung in both modes.
    for n in [small, big, float] {
        assert_eq!(cmp(n, atom, &table), Ordering::Less);
        assert_eq!(cmp_arith(n, atom, &table), Ordering::Less);
    }
}

// ---------------------------------------------------------------------------
// 3. every fun kind occupies the `fun` rung
// ---------------------------------------------------------------------------

/// `Local` (`make_fun3`), `Imported` (`NEW_FUN_EXT`) and `External`
/// (`EXPORT_EXT`) funs are all one class: each sorts after the `ref` rung and
/// before the `external_port` rung. `Local` < `External` is the C's *within-class*
/// rule ("External funs compare greater than local ones", `utils.c:2208`), not a
/// cross-class one.
#[test]
fn every_fun_kind_sits_in_the_fun_rung() {
    let table = atoms();
    let mut heap = Heap::new(128);
    let rungs = ladder(&mut heap, &table);
    let internal_ref = rungs[5].1;
    let external_port = rungs[7].1;
    let local = rungs[6].1;
    // SAFETY: `heap` hands out exclusively-owned writable words for each block and
    // outlives every returned term.
    let (external, imported) = unsafe {
        let hp = heap
            .alloc_words(fun_words(FunKind::External, 0))
            .expect("heap room");
        let external = write_fun_external(
            hp,
            1,
            table.lookup("mymod").expect("module atom"),
            table.lookup("b").expect("function atom"),
        )
        .expect("arity fits");
        let hp = heap
            .alloc_words(fun_words(FunKind::Imported, 1))
            .expect("heap room");
        let imported = write_fun_imported(
            hp,
            1,
            table.lookup("mymod").expect("module atom"),
            &[0u8; 16],
            0,
            0,
            7,
            FunPid {
                node: table.lookup(ANODE).expect("sysname atom"),
                number: 0,
                serial: 0,
                creation: 0,
            },
            &[make_small(0)],
        )
        .expect("arity/num_free fit");
        (external, imported)
    };

    for (name, fun) in [
        ("local", local),
        ("imported", imported),
        ("external", external),
    ] {
        assert_eq!(
            cmp(internal_ref, fun, &table),
            Ordering::Less,
            "ref < {name} fun"
        );
        assert_eq!(
            cmp(fun, external_port, &table),
            Ordering::Less,
            "{name} fun < external_port"
        );
    }
    // The documented within-class rule, unchanged by this test.
    assert_eq!(cmp(local, external, &table), Ordering::Less);
}

// ---------------------------------------------------------------------------
// 4. a refc binary is a bitstring; only the raw `BinRef` word sorts last
// ---------------------------------------------------------------------------

/// An off-heap (refc) binary roots at an `ErlSubBits`, which `tag_val_def`
/// classifies as `BITSTRING_DEF` (`erl_term.h:1535-1536`) — so a refc binary lives
/// in the `bitstring` rung and compares `=:=` to an on-heap binary with the same
/// bits. Only the *inner* `BinRef` word is the C's `BIN_REF_DEF` (`0x11`, "not a
/// real term"), and `class_rank` places that after every real class.
#[test]
fn a_refc_binary_is_a_bitstring_and_the_raw_bin_ref_word_sorts_last() {
    let table = atoms();
    let mut heap = Heap::new(128);
    let rungs = ladder(&mut heap, &table);
    let (nil, list, on_heap) = (rungs[14].1, rungs[15].1, rungs[16].1);
    let bin_ref = rungs[17].1;

    // Rebuild the same pair so both halves are reachable: the root (the
    // `ErlSubBits`) and the raw `BinRef` word at its head.
    let payload = refc_payload();
    let base = payload.as_ptr().cast::<u8>();
    // SAFETY: as in `ladder` — 8 exclusively-owned, writable words plus a live,
    // word-aligned off-heap payload.
    let (refc_binary, raw_bin_ref) = unsafe {
        let hp = heap.alloc_words(ERL_REFC_BITS_SIZE).expect("heap room");
        let root = write_refc_binary(
            hp,
            base.cast_mut().cast::<Eterm>(),
            std::ptr::null_mut(),
            base,
            0,
            16,
        );
        // SAFETY: `root` is the pair's live `ErlSubBits`.
        (root, make_boxed(hp))
    };

    // The class the C assigns: BITSTRING_DEF, not BIN_REF_DEF.
    // SAFETY: both terms are live boxes owned by `heap`.
    assert_eq!(unsafe { class(refc_binary) }, TermClass::Bitstring);
    assert_eq!(unsafe { class(raw_bin_ref) }, TermClass::BinRef);

    // It sits in the bitstring rung: above `nil` and `list`, and `=:=` an on-heap
    // binary carrying the same bits (the C's `erts_cmp_bits`).
    assert_eq!(cmp(nil, refc_binary, &table), Ordering::Less);
    assert_eq!(cmp(list, refc_binary, &table), Ordering::Less);
    assert_eq!(cmp(refc_binary, on_heap, &table), Ordering::Equal);
    assert!(eq(refc_binary, on_heap, &table));

    // The raw `BinRef` word sorts after *every* real class — including the
    // bitstring rung and the `list`/`nil` rungs.
    for (name, rung) in &rungs[..17] {
        assert_eq!(
            cmp(*rung, raw_bin_ref, &table),
            Ordering::Less,
            "{name} < raw BinRef"
        );
    }
    // The ladder's own last rung is that same class.
    // SAFETY: `bin_ref` is a live box owned by `heap`.
    assert_eq!(unsafe { class(bin_ref) }, TermClass::BinRef);
}

// ---------------------------------------------------------------------------
// 5. the six internal/external boundary pairings
// ---------------------------------------------------------------------------

/// The six mixed internal/external pairings, each asserted one way. Three of them
/// (`ref < fun`, `fun < external_port`, `port < external_pid`) are pure
/// `mixed_types` fallbacks — node-independent. The other three pair two
/// representations of the *same* category; the C takes `pid_common`/`ref_common`
/// there and compares the node (and `serial`/`number`) first, while `cmp_impl`
/// uses [`class_rank`] — see the module docs' note; with [`ANODE`] sorting before
/// the local node both rules give the same verdict, which is what these samples
/// assert. The node-aware entry point that *does* compare those fields is pinned by
/// sections 6 and 7.
#[test]
fn internal_and_external_boundary_pairs() {    let table = atoms();
    let mut heap = Heap::new(256);
    let rungs = ladder(&mut heap, &table);
    let find = |name: &str| {
        rungs
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, t)| *t)
            .unwrap_or_else(|| panic!("no {name} rung"))
    };
    let (external_ref, internal_ref) = (find("external_ref"), find("ref"));
    let fun = find("fun");
    let (external_port, port) = (find("external_port"), find("port"));
    let (external_pid, pid) = (find("external_pid"), find("pid"));

    // EXTERNAL_REF_DEF (0x0c) < REF_DEF (0x0b).
    assert_eq!(cmp(external_ref, internal_ref, &table), Ordering::Less);
    // REF_DEF (0x0b) < FUN_DEF (0x0a) — the C's mixed_types fallback.
    assert_eq!(cmp(internal_ref, fun, &table), Ordering::Less);
    // FUN_DEF (0x0a) < EXTERNAL_PORT_DEF (0x09) — mixed_types.
    assert_eq!(cmp(fun, external_port, &table), Ordering::Less);
    // EXTERNAL_PORT_DEF (0x09) < PORT_DEF (0x08).
    assert_eq!(cmp(external_port, port, &table), Ordering::Less);
    // PORT_DEF (0x08) < EXTERNAL_PID_DEF (0x07) — mixed_types.
    assert_eq!(cmp(port, external_pid, &table), Ordering::Less);
    // EXTERNAL_PID_DEF (0x07) < PID_DEF (0x06).
    assert_eq!(cmp(external_pid, pid, &table), Ordering::Less);

    // The reverse direction of each, so a rank inversion cannot slip through.
    for (lo, hi) in [
        (external_ref, internal_ref),
        (internal_ref, fun),
        (fun, external_port),
        (external_port, port),
        (port, external_pid),
        (external_pid, pid),
    ] {
        assert_eq!(cmp(hi, lo, &table), Ordering::Greater);
        assert!(!eq(lo, hi, &table));
    }
}

// ---------------------------------------------------------------------------
// 6. the local-node-aware mixed internal/external arms
// ---------------------------------------------------------------------------

/// The local node every mixed-arm pin supplies: the oracle's own sysname with
/// `creation` 0 (what `term_to_binary/1` of a local pid carries under `-noshell`, see
/// [`the_oracle_demotes_a_local_node_pid_and_honours_a_foreign_creation`]).
fn local_node(table: &AtomTable) -> LocalNodeId {
    LocalNodeId::new(node_atom(table, LOCAL_SYSNAME), 0)
}

/// The atom term of the sysname `name` (a seeded atom).
fn node_atom(table: &AtomTable, name: &str) -> Eterm {
    make_atom(table.lookup(name).expect("seeded sysname atom"))
}

/// An external pid term for `node` — `write_external_pid` over `heap`.
fn ext_pid(
    heap: &mut Heap,
    table: &AtomTable,
    node: &str,
    number: u32,
    serial: u32,
    creation: u32,
) -> Eterm {
    // SAFETY: `heap` hands out `external_pid_size_words()` (4) exclusively-owned,
    // writable words and `write_external_pid` writes all of them. The heap never
    // reallocates, so the returned term stays live (and unmoved) with it.
    unsafe {
        let hp = heap
            .alloc_words(external_pid_size_words())
            .expect("heap room");
        write_external_pid(hp, node_atom(table, node), number, serial, creation)
    }
}

/// An external port term for `node` — `write_external_port` over `heap`.
fn ext_port(
    heap: &mut Heap,
    table: &AtomTable,
    node: &str,
    number: u64,
    creation: u32,
) -> Eterm {
    // SAFETY: as `ext_pid` — `external_port_size_words()` (4) writable words, all
    // written by `write_external_port`.
    unsafe {
        let hp = heap
            .alloc_words(external_port_size_words())
            .expect("heap room");
        write_external_port(hp, node_atom(table, node), number, creation)
    }
}

/// An external reference term for `node` with the given ids — `write_external_ref`
/// over `heap`.
fn ext_ref(heap: &mut Heap, table: &AtomTable, node: &str, ids: &[u32], creation: u32) -> Eterm {
    // SAFETY: as `ext_pid` — `external_ref_size_words(ids.len())` writable words, all
    // written by `write_external_ref`.
    unsafe {
        let hp = heap
            .alloc_words(external_ref_size_words(ids.len()))
            .expect("heap room");
        write_external_ref(hp, node_atom(table, node), ids, creation)
    }
}

/// An internal pid for `<num.ser>` — `make_pid_data` + `make_internal_pid`.
fn int_pid(serial: u64, number: u64) -> Eterm {
    make_internal_pid(make_pid_data(serial, number))
}

/// An internal port for `#Port<num>`.
fn int_port(number: u64) -> Eterm {
    make_internal_port(make_port_data(number))
}

/// An internal (M1) reference carrying the single 60-bit `id` — the sanctioned
/// [`write_internal_ref`] writer, never hand-written words.
fn int_ref(heap: &mut Heap, id: u64) -> Eterm {
    // SAFETY: `heap` hands out `INTERNAL_REF_WORDS` (4) exclusively-owned, writable
    // words; the writer fills all of them.
    unsafe {
        let hp = heap.alloc_words(INTERNAL_REF_WORDS).expect("heap room");
        write_internal_ref(hp, id).expect("the id fits a small")
    }
}

/// A mixed pid pair is decided by `serial`, then `number`, then the node — the C's
/// `pid_common` (`utils.c:1777-1811`), reached from the internal/external cases at
/// `:1766-1775`/`:2212-2217`. Each step is pinned with a node that would decide the
/// other way, so a wrong field order cannot slip through.
#[test]
fn mixed_pids_compare_serial_then_number_then_the_node() {
    let table = atoms();
    let mut heap = Heap::new(64);
    let local = local_node(&table);
    // <0.5.1>: number 5, serial 1.
    let pid = int_pid(1, 5);

    // (1) serial first: serial 1 > 0 decides, although 'anode@ahost' < 'nonode@nohost'.
    let serial_differs = ext_pid(&mut heap, &table, ANODE, 0, 0, 0);
    assert_eq!(
        cmp_with_local_node(pid, serial_differs, local, &table),
        Ordering::Greater,
        "serial is compared before the node",
    );
    // (2) number next: 5 < 9 decides, although 'anode@ahost' < 'nonode@nohost'.
    let number_differs = ext_pid(&mut heap, &table, ANODE, 9, 1, 0);
    assert_eq!(
        cmp_with_local_node(pid, number_differs, local, &table),
        Ordering::Less,
        "number is compared before the node",
    );
    // (3) then the node: with serial and number equal, the sysname decides.
    let node_before = ext_pid(&mut heap, &table, ANODE, 5, 1, 0);
    let node_after = ext_pid(&mut heap, &table, ZNODE, 5, 1, 0);
    assert_eq!(
        cmp_with_local_node(pid, node_before, local, &table),
        Ordering::Greater,
        "equal fields, foreign sysname sorting first ⇒ external pid < internal pid",
    );
    assert_eq!(
        cmp_with_local_node(pid, node_after, local, &table),
        Ordering::Less,
        "equal fields, foreign sysname sorting last ⇒ internal pid < external pid",
    );
    // (4) the node's `creation` is the node's last field (`CMP_NODES`).
    let creation_after = ext_pid(&mut heap, &table, LOCAL_SYSNAME, 5, 1, 1);
    assert_eq!(
        cmp_with_local_node(pid, creation_after, local, &table),
        Ordering::Less,
        "same sysname, later creation ⇒ internal pid < external pid",
    );
    // (5) same node, same serial, same number ⇒ `Equal` (the C's `CMP_NODES` no-op
    // followed by `pop_next`).
    let same_node = ext_pid(&mut heap, &table, LOCAL_SYSNAME, 5, 1, 0);
    assert_eq!(
        cmp_with_local_node(pid, same_node, local, &table),
        Ordering::Equal,
    );
    assert!(cmp_arith_with_local_node(pid, same_node, local, &table).is_eq());

    // Every arm is antisymmetric — the reversed operand order is the reversed verdict.
    for (ext, expected) in [
        (serial_differs, Ordering::Less),
        (number_differs, Ordering::Greater),
        (node_before, Ordering::Less),
        (node_after, Ordering::Greater),
        (creation_after, Ordering::Greater),
        (same_node, Ordering::Equal),
    ] {
        assert_eq!(
            cmp_with_local_node(ext, pid, local, &table),
            expected,
            "reversed operand order",
        );
        assert_eq!(
            cmp_arith_with_local_node(ext, pid, local, &table),
            expected,
            "reversed operand order (arithmetic)",
        );
    }
}/// A mixed port pair is decided by the **node alone**: the C never compares the port
/// numbers (`utils.c:1751-1764` internal-first, `:2219-2233` external-first), so a
/// foreign port with a *larger* number still sorts below a local one whose node sorts
/// after it. Pinned on the oracle by
/// [`oracle_agrees_on_the_mixed_internal_external_pairs`].
///
/// The same-node case is unreachable in the C (it raises
/// `ERTS_INTERNAL_ERROR("different nodes compared equal")`); this crate's documented
/// convention compares the two port numbers there, so the relation stays total.
#[test]
fn mixed_ports_compare_the_node_only() {
    let table = atoms();
    let mut heap = Heap::new(64);
    let local = local_node(&table);
    // #Port<5>.
    let port = int_port(5);

    // The number 999 is *ignored*: 'anode@ahost' < 'nonode@nohost' decides alone.
    let big_number_before = ext_port(&mut heap, &table, ANODE, 999, 0);
    let small_number_before = ext_port(&mut heap, &table, ANODE, 0, 0);
    for ext in [big_number_before, small_number_before] {
        assert_eq!(
            cmp_with_local_node(port, ext, local, &table),
            Ordering::Greater,
            "a foreign port before the local node sorts first, whatever its number",
        );
        assert_eq!(
            cmp_with_local_node(ext, port, local, &table),
            Ordering::Less,
        );
    }
    // The last node: the local port sorts first even against number 0.
    let after = ext_port(&mut heap, &table, ZNODE, 0, 0);
    assert_eq!(
        cmp_with_local_node(port, after, local, &table),
        Ordering::Less,
    );
    // A later creation at the same sysname is a later node.
    let later_creation = ext_port(&mut heap, &table, LOCAL_SYSNAME, 0, 2);
    assert_eq!(
        cmp_with_local_node(port, later_creation, local, &table),
        Ordering::Less,
    );

    // The documented same-node convention: the port numbers decide, and only then.
    let same_node_bigger = ext_port(&mut heap, &table, LOCAL_SYSNAME, 999, 0);
    let same_node_smaller = ext_port(&mut heap, &table, LOCAL_SYSNAME, 3, 0);
    assert_eq!(
        cmp_with_local_node(port, same_node_bigger, local, &table),
        Ordering::Less,
        "same node: 5 < 999 by the documented convention",
    );
    assert_eq!(
        cmp_with_local_node(port, same_node_smaller, local, &table),
        Ordering::Greater,
    );
    let same = ext_port(&mut heap, &table, LOCAL_SYSNAME, 5, 0);
    assert_eq!(cmp_with_local_node(port, same, local, &table), Ordering::Equal);
}

/// A mixed reference pair is decided by the node first, then by the ids as one big
/// integer — the C's `ref_common` (`utils.c:2239-2287`), reached from the
/// internal-first case at `:2239-2260` and the external-first one at `:2288-2308`.
///
/// This tree's M1 internal reference carries its whole identity as one 60-bit id
/// ([`write_internal_ref`]/[`internal_ref_id`]), expressed as the id array
/// `[id as u32, (id >> 32) as u32]` — low word at index 0, high word at index 1, i.e.
/// the id's numeric value in the little-index order ERTS stores its own ref numbers in
/// (index `len - 1` most significant).
#[test]
fn mixed_refs_compare_the_node_then_the_ids() {
    let table = atoms();
    let mut heap = Heap::new(128);
    let local = local_node(&table);
    let reference = int_ref(&mut heap, 5); // ids [5, 0]

    // The node decides first — even against maximal foreign ids.
    let max_ids_before = ext_ref(&mut heap, &table, ANODE, &[u32::MAX; 3], 0);
    assert_eq!(
        cmp_with_local_node(reference, max_ids_before, local, &table),
        Ordering::Greater,
        "the node is compared before the ids",
    );
    let zero_ids_after = ext_ref(&mut heap, &table, ZNODE, &[0, 0, 0], 0);
    assert_eq!(
        cmp_with_local_node(reference, zero_ids_after, local, &table),
        Ordering::Less,
    );
    assert_eq!(
        cmp_with_local_node(max_ids_before, reference, local, &table),
        Ordering::Less,
    );

    // At the same node the ids decide, highest index (most significant) first.
    let same_ids = ext_ref(&mut heap, &table, LOCAL_SYSNAME, &[5, 0], 0);
    assert_eq!(
        cmp_with_local_node(reference, same_ids, local, &table),
        Ordering::Equal,
        "same node, same ids ⇒ Equal",
    );
    let low_word_after = ext_ref(&mut heap, &table, LOCAL_SYSNAME, &[6, 0], 0);
    assert_eq!(
        cmp_with_local_node(reference, low_word_after, local, &table),
        Ordering::Less,
    );
    let high_word_after = ext_ref(&mut heap, &table, LOCAL_SYSNAME, &[5, 1], 0);
    assert_eq!(
        cmp_with_local_node(reference, high_word_after, local, &table),
        Ordering::Less,
        "the high id word is the more significant one",
    );
    let low_word_before = ext_ref(&mut heap, &table, LOCAL_SYSNAME, &[4], 0);
    assert_eq!(
        cmp_with_local_node(reference, low_word_before, local, &table),
        Ordering::Greater,
    );
    // Zero-trimming: a longer foreign slice whose leading (highest-index) ids are zero
    // is the *same* integer as `[5]`, so it compares Equal to the internal `[5, 0]`.
    let trimmed = ext_ref(&mut heap, &table, LOCAL_SYSNAME, &[5, 0, 0, 0], 0);
    assert_eq!(
        cmp_with_local_node(reference, trimmed, local, &table),
        Ordering::Equal,
        "leading zero ids are trimmed before comparing",
    );
    // …while a non-zero leading id on the longer side decides immediately.
    let longer_nonzero = ext_ref(&mut heap, &table, LOCAL_SYSNAME, &[5, 0, 1], 0);
    assert_eq!(
        cmp_with_local_node(reference, longer_nonzero, local, &table),
        Ordering::Less,
        "a non-zero leading id makes the longer slice the greater one",
    );
}

/// Without a [`LocalNodeId`] there is nothing to substitute for `erts_this_node`, so
/// the **context-free** [`cmp`]/[`cmp_arith`] keep the class-ladder convention for a
/// mixed internal/external pair — [`class_rank`] (`external_*` below `*`), regardless of
/// nodes, serials or numbers. This is the documented no-context behaviour, and the
/// reason the node-aware entry points exist.
#[test]
fn mixed_pairs_without_a_local_node_keep_the_class_ladder() {
    let table = atoms();
    let mut heap = Heap::new(128);
    let pid = int_pid(1, 5);
    let port = int_port(5);
    let reference = int_ref(&mut heap, 5);

    // The very samples a local node would decide the *other* way (ZNODE sorts after
    // `nonode@nohost`, and the ports' numbers are ignored): rank says `external_*` <
    // `*` in every one of them, in both modes and both operand orders.
    let after_pid = ext_pid(&mut heap, &table, ZNODE, 5, 1, 0);
    let after_port = ext_port(&mut heap, &table, ZNODE, 0, 0);
    let before_ref = ext_ref(&mut heap, &table, ANODE, &[u32::MAX; 3], 0);
    for (ext, internal) in [
        (after_pid, pid),
        (after_port, port),
        (before_ref, reference),
    ] {
        assert_eq!(cmp(ext, internal, &table), Ordering::Less);
        assert_eq!(cmp(internal, ext, &table), Ordering::Greater);
        assert_eq!(cmp_arith(ext, internal, &table), Ordering::Less);
        assert!(!eq(ext, internal, &table));
    }
}

/// The local-node context is **additive**: it changes nothing outside the three mixed
/// arms. Every pair of the 18-rung ladder — including the ladder's own mixed
/// internal/external pid/port/ref pairs, whose node (`ANODE`) sorts before the local
/// one so both rules agree — gets exactly the same verdict from
/// [`cmp_with_local_node`]/[`cmp_arith_with_local_node`] as from [`cmp`]/[`cmp_arith`].
#[test]
fn the_local_node_context_changes_nothing_else() {
    let table = atoms();
    let mut heap = Heap::new(256);
    let rungs = ladder(&mut heap, &table);
    let local = local_node(&table);
    for i in 0..rungs.len() {
        for j in 0..rungs.len() {
            let (x, y) = (rungs[i], rungs[j]);
            assert_eq!(
                cmp_with_local_node(x.1, y.1, local, &table),
                cmp(x.1, y.1, &table),
                "cmp({}, {}) must not depend on the local node",
                x.0,
                y.0,
            );
            assert_eq!(
                cmp_arith_with_local_node(x.1, y.1, local, &table),
                cmp_arith(x.1, y.1, &table),
                "cmp_arith({}, {}) must not depend on the local node",
                x.0,
                y.0,
            );
        }
    }
}

/// A non-term word is handled exactly as before on the node-aware entry points too
/// (the [`is_term_word`] guard runs first, so nothing is dereferenced and the relation
/// stays total): a non-term sorts after every term and two non-terms order by word.
#[test]
fn the_local_node_entry_points_still_guard_non_terms() {
    let table = atoms();
    let local = local_node(&table);
    let non_value = THE_NON_VALUE;
    let pid = int_pid(0, 0);
    assert_eq!(
        cmp_with_local_node(non_value, pid, local, &table),
        Ordering::Greater,
    );
    assert_eq!(
        cmp_with_local_node(pid, non_value, local, &table),
        Ordering::Less,
    );
    assert_eq!(
        cmp_with_local_node(non_value, non_value, local, &table),
        Ordering::Equal,
    );
    assert_eq!(
        cmp_arith_with_local_node(non_value, pid, local, &table),
        Ordering::Greater,
    );
}

// ---------------------------------------------------------------------------
// 7. oracle cross-check
// ---------------------------------------------------------------------------
/// Runs `<expr>, halt(0).` under the system `erl` and returns its stdout.
///
/// Returns `None` — after printing a clear skip message — when `erl` cannot be
/// spawned or does not answer within [`ERL_TIMEOUT`]. A *spawned* `erl` that exits
/// non-zero is a bug in the embedded expression, so that is asserted rather than
/// skipped.
///
/// The expression must be a **single line** (the oracle harness passes it verbatim
/// to `-eval`).
fn erl_stdout(expr: &str) -> Option<String> {
    let code = format!("{expr}, halt(0).");
    debug_assert!(
        !code.contains('\n'),
        "the -eval expression must be one line"
    );
    let (tx, rx) = mpsc::channel();
    let code_for_thread = code.clone();
    thread::spawn(move || {
        let out = Command::new("erl")
            .arg("-noshell")
            .arg("-eval")
            .arg(&code_for_thread)
            .output();
        // The receiver may have timed out and gone away; that is fine.
        let _ = tx.send(out);
    });

    match rx.recv_timeout(ERL_TIMEOUT) {
        Err(_) => {
            eprintln!(
                "skipping class-order oracle check: `erl` did not answer within {ERL_TIMEOUT:?}"
            );
            None
        }
        Ok(Err(e)) => {
            eprintln!(
                "skipping class-order oracle check: could not spawn `erl` ({e}); is Erlang on PATH?"
            );
            None
        }
        Ok(Ok(out)) => {
            assert!(
                out.status.success(),
                "`erl` exited with {} for expression:\n{code}\nstderr:\n{}",
                out.status,
                String::from_utf8_lossy(&out.stderr),
            );
            Some(String::from_utf8_lossy(&out.stdout).into_owned())
        }
    }
}

/// Parses a flat Erlang list of booleans printed with `~w`, e.g.
/// `[true,false,true]`, into a `Vec<bool>`.
fn parse_bools(s: &str) -> Vec<bool> {
    let s = s.trim();
    let inner = s
        .strip_prefix('[')
        .and_then(|x| x.strip_suffix(']'))
        .unwrap_or_else(|| panic!("expected an Erlang boolean list, got {s:?}"));
    if inner.trim().is_empty() {
        return Vec::new();
    }
    inner
        .split(',')
        .map(|tok| match tok.trim() {
            "true" => true,
            "false" => false,
            other => panic!("expected an oracle boolean, got {other:?}"),
        })
        .collect()
}

/// Parses a flat Erlang list of non-negative integers printed with `~w`, e.g.
/// `[0,4]`, into a `Vec<u64>`.
fn parse_ints(s: &str) -> Vec<u64> {
    let s = s.trim();
    let inner = s
        .strip_prefix('[')
        .and_then(|x| x.strip_suffix(']'))
        .unwrap_or_else(|| panic!("expected an Erlang integer list, got {s:?}"));
    if inner.trim().is_empty() {
        return Vec::new();
    }
    inner
        .split(',')
        .map(|tok| {
            tok.trim()
                .parse()
                .unwrap_or_else(|_| panic!("expected an oracle integer, got {tok:?}"))
        })
        .collect()
}

/// Appends an `ATOM_EXT` (latin1, 16-bit length) atom to `out`.
fn etf_atom(out: &mut Vec<u8>, name: &str) {
    let bytes = name.as_bytes();    assert!(bytes.len() <= u16::MAX as usize, "atom name too long");
    out.push(ATOM_EXT);
    out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(bytes);
}

/// `NEWER_REFERENCE_EXT`: tag, `ref_words:16`, sysname atom, `creation:32`, then
/// `ref_words` u32 ids (`external.c:3610-3624`). Decoding it yields an **external**
/// reference because [`ANODE`] is not the oracle's own node.
fn external_ref_etf() -> Vec<u8> {
    external_ref_etf_for(ANODE, &[0, 0, 1], 3)
}

/// [`external_ref_etf`] for an arbitrary node, id array and `creation`.
///
/// A ref whose sysname **and** creation are the oracle's own is *demoted* to the
/// internal form by `binary_to_term/1` (`dec_is_this_node`, `external.c:3124-3129`,
/// `:3170-3175`) — see [`the_oracle_compares_local_node_refs_by_their_ids`].
fn external_ref_etf_for(node: &str, ids: &[u32], creation: u32) -> Vec<u8> {
    let mut out = vec![VERSION_MAGIC, NEWER_REFERENCE_EXT];
    out.extend_from_slice(&(ids.len() as u16).to_be_bytes());
    etf_atom(&mut out, node);
    out.extend_from_slice(&creation.to_be_bytes());
    for id in ids {
        out.extend_from_slice(&id.to_be_bytes());
    }
    out
}

/// `V4_PORT_EXT`: tag, sysname atom, `num:64`, `creation:32`
/// (`external.c:3670-3701`).
fn external_port_etf() -> Vec<u8> {
    external_port_etf_for(ANODE, 0, 0)
}

/// [`external_port_etf`] for an arbitrary node, number and `creation`.
fn external_port_etf_for(node: &str, number: u64, creation: u32) -> Vec<u8> {
    let mut out = vec![VERSION_MAGIC, V4_PORT_EXT];
    etf_atom(&mut out, node);
    out.extend_from_slice(&number.to_be_bytes());
    out.extend_from_slice(&creation.to_be_bytes());
    out
}

/// `NEW_PID_EXT`: tag, sysname atom, `num:32`, `serial:32`, `creation:32`
/// (`external.c:2998-3021`).
fn external_pid_etf() -> Vec<u8> {
    external_pid_etf_for(ANODE, 0, 0, 0)
}

/// [`external_pid_etf`] for an arbitrary node, number, serial and `creation`.
///
/// A pid whose sysname **and** creation are the oracle's own is *demoted* to the
/// internal form by `binary_to_term/1` (`dec_pid`, `external.c:3164-3175`) — see
/// [`the_oracle_demotes_a_local_node_pid_and_honours_a_foreign_creation`].
fn external_pid_etf_for(node: &str, number: u32, serial: u32, creation: u32) -> Vec<u8> {
    let mut out = vec![VERSION_MAGIC, NEW_PID_EXT];
    etf_atom(&mut out, node);
    out.extend_from_slice(&number.to_be_bytes());
    out.extend_from_slice(&serial.to_be_bytes());
    out.extend_from_slice(&creation.to_be_bytes());
    out
}
/// `binary_to_term(<<…>>)` over `bytes`.
fn binary_to_term_expr(bytes: &[u8]) -> String {
    let list: Vec<String> = bytes.iter().map(u8::to_string).collect();
    format!("binary_to_term(<<{}>>)", list.join(","))
}

/// The `-eval` expression that materializes the rung named `name` on the oracle
/// side, or `None` for a rung the real runtime cannot construct (the `bin_ref`
/// word: a `BinRef` is not a term).
///
/// The values match the Rust samples of [`ladder`]: `1`, `1 bsl 70`, `1.5`, `a`,
/// an external ref/port/pid for [`ANODE`] (decoded from hand-written ETF), an
/// internal ref, a local fun, an internal port, an internal pid, `{a}`,
/// `records:create(mymod, myrec, [{a,1}], #{is_exported => true})`, `#{a => 1}`,
/// `[]`, `[1]` and `<<1,2>>`.
fn oracle_expr(name: &str) -> Option<String> {
    Some(match name {
        "small" => "1".to_string(),
        "big" => "1 bsl 70".to_string(),
        "float" => "1.5".to_string(),
        "atom" => "a".to_string(),
        "external_ref" => binary_to_term_expr(&external_ref_etf()),
        "ref" => "make_ref()".to_string(),
        "fun" => "fun(X) -> X end".to_string(),
        "external_port" => binary_to_term_expr(&external_port_etf()),
        "port" => "erlang:list_to_port(\"#Port<0.0>\")".to_string(),
        "external_pid" => binary_to_term_expr(&external_pid_etf()),
        "pid" => "erlang:list_to_pid(\"<0.0.0>\")".to_string(),
        "tuple" => "{a}".to_string(),
        "record" => "records:create(mymod, myrec, [{a,1}], #{is_exported => true})".to_string(),
        "map" => "#{a => 1}".to_string(),
        "nil" => "[]".to_string(),
        "list" => "[1]".to_string(),
        "bitstring" => "<<1,2>>".to_string(),
        _ => return None,
    })
}

/// One mixed-pair pin: the two Rust terms, the `-eval` expressions that build the same
/// two terms in the real runtime, and the verdict both sides must reach.
struct MixedCase {
    /// The case's name, for assertion messages.
    label: &'static str,
    /// The Rust-side `a` operand.
    a: Eterm,
    /// The Rust-side `b` operand.
    b: Eterm,
    /// The oracle expression that builds `a`.
    expr_a: String,
    /// The oracle expression that builds `b`.
    expr_b: String,
    /// The verdict `cmp_with_local_node(a, b, local)` must return.
    expected: Ordering,
}

/// The real runtime agrees with [`cmp_with_local_node`]/
/// [`cmp_arith_with_local_node`] on every mixed internal/external `pid`/`port`/`ref`
/// arm: `serial` before `number` before the node for a pid, the node **only** for a
/// port, the node before the ids for a reference, in both operand orders. Every case is
/// built so a wrong field order gives the opposite verdict. Skipped loudly when `erl` is
/// unavailable.
#[test]
fn oracle_agrees_on_the_mixed_internal_external_pairs() {
    let table = atoms();
    let mut heap = Heap::new(256);
    let local = local_node(&table);
    // The internal samples, matching the oracle's `<0.5.1>` and `#Port<0.5>`/`make_ref()`.
    let pid = int_pid(1, 5);
    let port = int_port(5);
    let reference = int_ref(&mut heap, 7);
    let list_to_pid = "erlang:list_to_pid(\"<0.5.1>\")".to_string();
    let list_to_port = "erlang:list_to_port(\"#Port<0.5>\")".to_string();
    let make_ref = "make_ref()".to_string();

    let mut cases = Vec::new();
    // (serial decides before the node: serial 1 > 0 although 'anode@ahost' < local.)
    // Number decides before the node: 5 < 9 although 'anode@ahost' < local. Equal
    // serial and number: the sysname decides, both ways round the local node. And the
    // node's own `creation` is its last field.
    for (label, node, number, serial, creation, expected) in [
        (
            "pid/serial-before-node",
            ANODE,
            0u32,
            0u32,
            0u32,
            Ordering::Greater,
        ),
        ("pid/number-before-node", ANODE, 9, 1, 0, Ordering::Less),
        ("pid/node-before", ANODE, 5, 1, 0, Ordering::Greater),
        ("pid/node-after", ZNODE, 5, 1, 0, Ordering::Less),
        ("pid/creation-after", LOCAL_SYSNAME, 5, 1, 1, Ordering::Less),
    ] {
        cases.push(MixedCase {
            label,
            a: pid,
            b: ext_pid(&mut heap, &table, node, number, serial, creation),
            expr_a: list_to_pid.clone(),
            expr_b: binary_to_term_expr(&external_pid_etf_for(node, number, serial, creation)),
            expected,
        });
    }    // The node is the *only* field a mixed port pair compares: number 999 is ignored.
    for (label, node, number, expected) in [
        ("port/node-before", ANODE, 999u64, Ordering::Greater),
        ("port/node-after", ZNODE, 0, Ordering::Less),
    ] {
        cases.push(MixedCase {
            label,
            a: port,
            b: ext_port(&mut heap, &table, node, number, 0),
            expr_a: list_to_port.clone(),
            expr_b: binary_to_term_expr(&external_port_etf_for(node, number, 0)),
            expected,
        });
    }
    // The node decides before the ids — even against maximal foreign ones.
    for (label, node, ids, expected) in [
        ("ref/node-before", ANODE, vec![u32::MAX; 3], Ordering::Greater),
        ("ref/node-after", ZNODE, vec![0, 0, 0], Ordering::Less),
    ] {
        cases.push(MixedCase {
            label,
            a: reference,
            b: ext_ref(&mut heap, &table, node, &ids, 0),
            expr_a: make_ref.clone(),
            expr_b: binary_to_term_expr(&external_ref_etf_for(node, &ids, 0)),
            expected,
        });
    }

    // One binding pair per case plus four booleans: `=:=`, `<`, `>`, and `<` reversed.
    let assigns: Vec<String> = cases
        .iter()
        .enumerate()
        .flat_map(|(i, c)| [format!("A{i} = {}", c.expr_a), format!("B{i} = {}", c.expr_b)])
        .collect();
    let checks: Vec<String> = cases
        .iter()
        .enumerate()
        .map(|(i, _)| format!("[A{i} =:= B{i}, A{i} < B{i}, A{i} > B{i}, B{i} < A{i}]"))
        .collect();
    let expr = format!(
        "{}, io:format(\"~w\", [lists:flatten([{}])])",
        assigns.join(", "),
        checks.join(", "),
    );

    let Some(out) = erl_stdout(&expr) else {
        return; // skipped loudly
    };
    let bools = parse_bools(&out);
    assert_eq!(
        bools.len(),
        cases.len() * 4,
        "unexpected oracle output: {out:?}"
    );
    for (i, case) in cases.iter().enumerate() {
        let exact = cmp_with_local_node(case.a, case.b, local, &table);
        let arith = cmp_arith_with_local_node(case.a, case.b, local, &table);
        assert_eq!(
            exact, case.expected,
            "{}: rust cmp_with_local_node gave {exact:?}",
            case.label,
        );
        assert_eq!(
            arith, case.expected,
            "{}: rust cmp_arith_with_local_node gave {arith:?}",
            case.label,
        );
        let oracle = [
            bools[4 * i],
            bools[4 * i + 1],
            bools[4 * i + 2],
            bools[4 * i + 3],
        ];
        assert_eq!(
            oracle,
            [
                exact.is_eq(),
                arith.is_lt(),
                arith.is_gt(),
                arith.is_gt(),
            ],
            "{}: oracle {oracle:?} vs rust {arith:?}",
            case.label,
        );
    }
}

/// What the oracle does with a crafted **local-node** external pid: `binary_to_term/1`
/// **demotes** it to the internal form (`dec_is_this_node`, `external.c:3124-3129`,
/// `:3164-3175`), so it is `=:=` the internal pid with the same number and serial — and
/// an internal pid is an immediate, which `erts_debug:size/1` reports as **0** words
/// while the kept-external variant (a differing `creation`) is a 4-word thing.
///
/// That is why the same-node mixed arm is unreachable in ERTS: an external pid for
/// *this* node cannot exist in its memory, so `pid_common` never has to compare equal
/// nodes (and this crate's same-node ⇒ `Equal` is the C's `CMP_NODES` no-op falling
/// through to `pop_next`).
#[test]
fn the_oracle_demotes_a_local_node_pid_and_honours_a_foreign_creation() {
    let local_image = external_pid_etf_for(LOCAL_SYSNAME, 5, 1, 0);
    let later_creation = external_pid_etf_for(LOCAL_SYSNAME, 5, 1, 7);
    let setup = format!(
        "L = erlang:list_to_pid(\"<0.5.1>\"), D = {}, E7 = {}",
        binary_to_term_expr(&local_image),
        binary_to_term_expr(&later_creation),
    );

    let Some(out) = erl_stdout(&format!(
        "{setup}, io:format(\"~w\", [[D =:= L, E7 =:= L, E7 < L, L < E7]])",
    )) else {
        return; // skipped loudly
    };
    let bools = parse_bools(&out);
    assert_eq!(bools.len(), 4, "unexpected oracle output: {out:?}");
    // Demoted ⇒ exactly the internal pid; a later creation is a *later* node, so the
    // local pid sorts first.
    assert_eq!(bools, [true, false, false, true]);

    let Some(out) = erl_stdout(&format!(
        "{setup}, io:format(\"~w\", [[erts_debug:size(D), erts_debug:size(E7)]])",
    )) else {
        return; // skipped loudly
    };
    // 0 words = an internal pid *immediate* (demoted); 4 words = the external thing.
    assert_eq!(parse_ints(&out), [0, 4], "unexpected oracle output: {out:?}");
}

/// A crafted external reference at the **local** node is demoted to the internal form
/// too (`dec_is_this_node`, `external.c:3124-3129`), so its ids are compared by the same
/// big-integer rule at the same node — index `len - 1` most significant. The ids
/// `[1,2,3]` and `[1,2,4]` differ only in the **highest** index, which decides.
#[test]
fn the_oracle_compares_local_node_refs_by_their_ids() {
    let r1 = external_ref_etf_for(LOCAL_SYSNAME, &[1, 2, 3], 0);
    let r2 = external_ref_etf_for(LOCAL_SYSNAME, &[1, 2, 4], 0);
    let expr = format!(
        "R1 = {a}, R2 = {b}, R3 = {c}, io:format(\"~w\", [[R1 =:= R2, R1 < R2, R1 > R2, R1 =:= R3]])",
        a = binary_to_term_expr(&r1),
        b = binary_to_term_expr(&r2),
        c = binary_to_term_expr(&r1),
    );
    let Some(out) = erl_stdout(&expr) else {
        return; // skipped loudly
    };
    let bools = parse_bools(&out);
    // Not equal, and the highest id index decides the order; a re-decode of the same
    // bytes is `=:=` the first term.
    assert_eq!(bools, [false, true, false, true], "unexpected: {out:?}");
}
/// The real runtime agrees with this crate on the whole class ladder: for every
/// pair of rungs, `=:=` decides `eq` (the exact `cmp`-is-`Equal` predicate) and
/// `<`/`>` decide `cmp_arith`. Skipped loudly when `erl` is unavailable.
#[test]
fn oracle_agrees_on_the_cross_class_ladder() {
    let table = atoms();
    let mut heap = Heap::new(256);
    let rungs = ladder(&mut heap, &table);
    let oracled: Vec<(usize, String)> = rungs
        .iter()
        .enumerate()
        .filter_map(|(i, (name, _))| oracle_expr(name).map(|expr| (i, expr)))
        .collect();
    assert_eq!(
        oracled.len(),
        rungs.len() - 1,
        "the bin_ref rung is the only one without an oracle sample",
    );

    // Every pair (including each rung against itself), as `T<i> = <expr>` bindings
    // plus one `[=:=, <, >]` boolean triple per pair.
    let assigns: Vec<String> = oracled
        .iter()
        .enumerate()
        .map(|(var, (_, expr))| format!("T{var} = {expr}"))
        .collect();
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for a in 0..oracled.len() {
        for b in a..oracled.len() {
            pairs.push((a, b));
        }
    }
    let checks: Vec<String> = pairs
        .iter()
        .map(|&(a, b)| format!("[T{a} =:= T{b}, T{a} < T{b}, T{a} > T{b}]"))
        .collect();
    let expr = format!(
        "{}, io:format(\"~w\", [lists:flatten([{}])])",
        assigns.join(", "),
        checks.join(", "),
    );

    let Some(out) = erl_stdout(&expr) else {
        return; // skipped loudly
    };
    let bools = parse_bools(&out);
    assert_eq!(
        bools.len(),
        pairs.len() * 3,
        "unexpected oracle output: {out:?}"
    );

    for (k, &(a, b)) in pairs.iter().enumerate() {
        let (x, y) = (rungs[oracled[a].0], rungs[oracled[b].0]);
        let exact = cmp(x.1, y.1, &table);
        let arith = cmp_arith(x.1, y.1, &table);
        let (o_eq, o_lt, o_gt) = (bools[3 * k], bools[3 * k + 1], bools[3 * k + 2]);
        assert_eq!(
            o_eq,
            exact.is_eq(),
            "({}, {}) =:= : oracle {o_eq} vs rust {}",
            x.0,
            y.0,
            exact.is_eq(),
        );
        assert_eq!(
            o_lt,
            arith.is_lt(),
            "({}, {}) < : oracle {o_lt} vs rust {arith:?}",
            x.0,
            y.0,
        );
        assert_eq!(
            o_gt,
            arith.is_gt(),
            "({}, {}) > : oracle {o_gt} vs rust {arith:?}",
            x.0,
            y.0,
        );
    }
}
