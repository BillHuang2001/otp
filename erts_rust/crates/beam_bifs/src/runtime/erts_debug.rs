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

//! The `erts_debug`/`erl_debugger` runtime-coupled BIFs (the internal debugger tools).
//!
//! Every key below is a declared `bif.tab` entry.
//!
//! `unaligned_bitstring/2` and `map_info/1` are implemented here **for real** — the
//! former a pure term transformation over the shared [`crate::access`] bitstring
//! surface, the latter a walk of a hash map's HAMT node interiors over the
//! [`crate::access`] node accessors (see `REAL_BIFS`).
//!
//! Every other key requires the *running* runtime (VM introspection, the distribution
//! codec, lock-counter bookkeeping, the internal-state knobs). Each is registered
//! through the single [`RuntimeHooks`](crate::context::RuntimeHooks) seam, so with no
//! hook installed a call answers the typed `unsupported_bif` capability gap — never
//! `undef`, never a fabricated value (e.g. a constant `0` for
//! `interpreter_size/0`).
//!
//! `size_shared/1` stays such a gap, and not for want of a hook: the oracle's
//! `size_shared` (`../../erts/emulator/beam/copy.c:319`) counts the words a term
//! **shares** with other terms — the literal-area subterms handed out to a module's
//! code. This runtime models no shared/literal-area subterms at all (every term a BIF
//! sees is built fresh on a process heap), so no term here can share storage with
//! another and the oracle's **non-zero** results are unreachable: it could only ever
//! answer `0`, which for a sharing-measuring BIF is a fabricated value rather than an
//! answer. `copy_shared/2` is blocked on the same missing model.

use beam_common::{BeamError, Result};
use beam_term::prelude::signed_value;
use beam_term::{Eterm, NIL};

use crate::access;
use crate::arith;
use crate::context::BifContext;
use crate::error::{badarg, structured_error};
use crate::registry::BifRegistry;

/// The `(module, function, arity)` keys this module implements **for real** (rather
/// than forwarding to the [`RuntimeHooks`](crate::context::RuntimeHooks) seam).
///
/// Sorted by `(module, function, arity)`. A follow-up task (the `hook_sets` pass)
/// consumes this list to drop the matching `runtime_bif!` key from the
/// `RUNTIME_HOOKS_BIFS` roster of `src/hook_sets.rs`; until then it is the single
/// source of truth for which keys the real implementations own, and [`register`]
/// asserts every one of them is actually present after registration.
pub(crate) const REAL_BIFS: &[(&str, &str, u32)] = &[
    ("erts_debug", "map_info", 1),
    ("erts_debug", "unaligned_bitstring", 2),
];

/// Registers this family's BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    crate::runtime::runtime_bif!(reg, "erts_debug", breakpoint, 2);
    crate::runtime::runtime_bif!(reg, "erts_debug", copy_shared, 2);
    crate::runtime::runtime_bif!(reg, "erts_debug", dirty, 3);
    crate::runtime::runtime_bif!(reg, "erts_debug", dirty_cpu, 2);
    crate::runtime::runtime_bif!(reg, "erts_debug", dirty_io, 2);
    crate::runtime::runtime_bif!(reg, "erts_debug", disassemble, 1);
    crate::runtime::runtime_bif!(reg, "erts_debug", dist_ext_to_term, 2);
    crate::runtime::runtime_bif!(reg, "erts_debug", get_internal_state, 1);
    crate::runtime::runtime_bif!(reg, "erts_debug", instructions, 0);
    crate::runtime::runtime_bif!(reg, "erts_debug", interpreter_size, 0);
    crate::runtime::runtime_bif!(reg, "erts_debug", lcnt_clear, 0);
    crate::runtime::runtime_bif!(reg, "erts_debug", lcnt_collect, 0);
    crate::runtime::runtime_bif!(reg, "erts_debug", lcnt_control, 1);
    crate::runtime::runtime_bif!(reg, "erts_debug", lcnt_control, 2);
    // `map_info/1` summarises a HAMT map's node interior (depth, arrays, collisions,
    // bitmaps, leafs); the walk is implemented below over the `crate::access` HAMT
    // node accessors.
    crate::runtime::runtime_bif!(reg, "erts_debug", map_info, 1);
    crate::runtime::runtime_bif!(reg, "erts_debug", set_internal_state, 2);
    // A gap on purpose — the sharing it measures is unrepresentable here (see the
    // module docs), so a value would be fabricated.
    crate::runtime::runtime_bif!(reg, "erts_debug", size_shared, 1);
    // Implemented for real (a pure bitstring-to-bitstring transformation).
    reg.register("erts_debug", "unaligned_bitstring", 2, unaligned_bitstring);
    // Implemented for real (a read-only walk of the frozen HAMT node interiors).
    reg.register("erts_debug", "map_info", 1, map_info_1);
    // `REAL_BIFS` is the declared roster; keep it honest against what was registered.
    debug_assert!(
        REAL_BIFS
            .iter()
            .all(|(module, function, arity)| reg.contains(module, function, *arity)),
        "REAL_BIFS lists a key that this module does not register"
    );
}

/// `erts_debug:unaligned_bitstring(Bitstring, Offset)`.
///
/// Ground truth: `../../erts/emulator/beam/beam_debug.c:100-167`
/// (`erts_debug_unaligned_bitstring_2`).
///
/// `Bitstring` must be a bitstring and `Offset` a **small** integer with
/// `0 < Offset < 8`; anything else is `badarg` — the C's `is_not_bitstring` /
/// `is_not_small` / `!(0 < offset && offset < 8)` guards, checked in that order.
/// Oracle-verified: `unaligned_bitstring(<<1,2,3>>, 0)` / `(_, 8)` / `(_, 9)` /
/// `(_, -1)` / `(_, 1.0)` / `(_, foo)` / `(_, 1 bsl 60)` ⇒ `badarg`, and
/// `unaligned_bitstring(foo, 1)` / `([1,2], 1)` ⇒ `badarg`.
///
/// The result carries **exactly the source's bits**: the C copies `source_size` bits
/// from the source into a fresh buffer at bit `offset` and then takes a sub-bit view
/// **starting at that same `offset`** (`erl_sub_bits_init(sb, …, base, offset,
/// source_size)`), so the `offset` leading bits it left uninitialised are never
/// observable — directly, or through the sub-bit view's own size. Hence
/// `unaligned_bitstring(<<1,2,3>>, 1) =:= <<1,2,3>>` and
/// `unaligned_bitstring(<<1:3>>, 1) =:= <<1:3>>` (both oracle-verified); an empty
/// source is **not** an error — `unaligned_bitstring(<<>>, 1)` ⇒ `<<>>`, because
/// `<<>>` *is* a bitstring (`is_not_bitstring` is false for it).
///
/// # Representational deviation (Milestone-1)
///
/// What the C returns is a `refc` bitstring: a heap `ErlSubBits` whose bit pointer
/// sits `offset` bits **below** the byte start of a freshly minted off-heap refc
/// binary — i.e. a deliberately **unaligned** internal representation, which is the
/// whole point of the BIF (it exists to exercise consumers of an unaligned bit
/// pointer). This runtime builds a plain on-heap `ErlHeapBits` instead (every M1
/// binary is on-heap, see [`crate::bits`]), so the below-byte-start placement is not
/// reproducible: the **bit pattern and shape agree** (same bit size, same bits, the
/// result `=:=` the source) while the internal representation differs. The observable
/// consequence is `binary:referenced_byte_size/1`, which reports the C's oversized
/// refc buffer (65 for `unaligned_bitstring(<<1,2,3>>, 1)`) where this runtime reports
/// the on-heap binary's own size.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-bitstring source
/// or a non-small / out-of-range offset; otherwise propagates the heap-profile failure
/// when the result bitstring does not fit the destination heap.
fn unaligned_bitstring(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let source = args[0];
    if !access::is_bitstring(source) {
        return Err(badarg());
    }
    let offset = args[1];
    if !access::is_small(offset) {
        return Err(badarg());
    }
    // C reads the raw word (`unsigned_val`), so a negative small int becomes a huge
    // unsigned one; comparing the signed value against the same open interval is
    // equivalent for every small int in it (`0 < offset < 8`).
    let offset = signed_value(offset);
    if !(0 < offset && offset < 8) {
        return Err(badarg());
    }
    // The result's bits are the source's bits, at the source's bit size. Reading them
    // bit by bit (`bitstring_bits`) accepts a non-byte-aligned source and never
    // follows a `BinRef`/match-context indirection.
    let bit_len = access::bitstring_bit_size(source)?;
    let bytes = access::bitstring_bits(source)?;
    access::make_heap_bitstring(ctx.heap(), &bytes, bit_len)
}

/// `erts_debug:map_info/1` — `hashmap_info/2`'s traversal statistics
/// (`erl_map.c:3464-3559`).
///
/// Three cases, decided by the argument exactly as the C does:
///
/// * a **hash map** (a HAMT head node) is walked node by node and summarised as
///   `[{depth, MaxDepth}, {arrays, N, []}, {collisions, N, {16 bins}},
///   {bitmaps, N, {16 bins}}, {leafs, N, {18 bins}}]`;
/// * a **flat map** (and *only* a flat map) ⇒ `error:badarg`
///   (`BIF_ERROR(BIF_P, BADARG)`, `erl_map.c:3304-3305`);
/// * anything else — including a HAMT *inner* node, which `is_hashmap` rejects but
///   `is_map` accepts ⇒ `error:{badmap, Term}`.
///
/// The bins are indexed exactly as the C's: `arrays` counts head nodes with all 16
/// slots present; `collisions` counts collision nodes by arity, every arity above 16
/// folded onto the last bin; `bitmaps` counts bitmap nodes by `popcount(bitmap)`, bin
/// `i` holding nodes with `i + 1` slots; `leafs` counts the leaf cons cells by the
/// node level they hang at, where the head node's own level is `0` (so a leaf
/// directly below the head lands in bin `1`). `depth` is the deepest level any *node*
/// was reached at — `1` for a head whose children are all leaves.
///
/// Oracle-pinned (OTP 29.1.1 / erts-17.1; see the module tests):
/// `map_info(maps:from_list([{N, N} || N <- lists:seq(1, 40)]))` ⇒
/// `[{depth,4}, {arrays,0,[]}, {collisions,0,{0,…}}, {bitmaps,14,{1,5,3,2,1,1,0,…,1,0,0}},
/// {leafs,40,{0,4,32,2,2,0,…}}]`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a flat map, and the
/// `{badmap, Term}` structured raise for a non-map — both the C's `BIF_ERROR`s.
fn map_info_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let term = args[0];
    // The C's test order and classification: `is_hashmap` first (a HAMT head), then
    // `is_flatmap` (which is what raises the plain `badarg`), else `badmap`.
    if !access::is_hashmap(term) {
        if access::is_flatmap(term) {
            return Err(badarg());
        }
        return Err(structured_error(ctx, "badmap", &[term])?);
    }
    let info = walk_hashmap(term)?;

    let leafs = int_bins(ctx, &info.leafs)?;
    let bitmaps = int_bins(ctx, &info.bitmaps)?;
    let collisions = int_bins(ctx, &info.collisions)?;

    // The C builds the result back to front, so the list reads
    // `[depth, arrays, collisions, bitmaps, leafs]`.
    let leafs_atom = ctx.intern_atom("leafs")?;
    let value = arith::int_from_i64(ctx, info.nleaf as i64)?;
    let entry = ctx.tuple(&[leafs_atom, value, leafs])?;
    let mut result = ctx.cons(entry, NIL)?;

    let bitmaps_atom = ctx.intern_atom("bitmaps")?;
    let value = arith::int_from_i64(ctx, info.nbitmap as i64)?;
    let entry = ctx.tuple(&[bitmaps_atom, value, bitmaps])?;
    result = ctx.cons(entry, result)?;

    let collisions_atom = ctx.intern_atom("collisions")?;
    let value = arith::int_from_i64(ctx, info.ncollision as i64)?;
    let entry = ctx.tuple(&[collisions_atom, value, collisions])?;
    result = ctx.cons(entry, result)?;

    let arrays_atom = ctx.intern_atom("arrays")?;
    let value = arith::int_from_i64(ctx, info.narray as i64)?;
    let entry = ctx.tuple(&[arrays_atom, value, NIL])?;
    result = ctx.cons(entry, result)?;

    let depth_atom = ctx.intern_atom("depth")?;
    let value = arith::int_from_i64(ctx, info.depth as i64)?;
    let entry = ctx.tuple(&[depth_atom, value])?;
    result = ctx.cons(entry, result)?;
    Ok(result)
}

/// Builds the tuple of the small/bignum integers `values` — the C's
/// `hashmap_bld_tuple_uint` (`erl_map.c:3560-3572`), which materialises each count
/// through `erts_bld_uint` (a small int when it fits, else a bignum).
fn int_bins(ctx: &mut BifContext<'_>, values: &[u64]) -> Result<Eterm> {
    let mut terms = Vec::with_capacity(values.len());
    for &value in values {
        terms.push(arith::int_from_i64(ctx, value as i64)?);
    }
    ctx.tuple(&terms)
}

/// The `hashmap_info/2` traversal accumulators (`erl_map.c:3475-3483`): the four
/// per-class node counts and their bin histograms.
#[derive(Default)]
struct HashMapInfo {
    /// `leaf_usage` — `HAMT_MAX_LEVEL + 2` bins, one per level a leaf hangs at.
    leafs: [u64; beam_term::HAMT_MAX_LEVEL + 2],
    /// `bitmap_usage` — 16 bins, `bitmaps[i]` = bitmap nodes with `i + 1` slots.
    bitmaps: [u64; beam_term::HAMT_MAX_LEVEL],
    /// `collision_usage` — 16 bins, `collisions[i]` = collision nodes of arity
    /// `i + 1`, every larger arity folded onto the last bin.
    collisions: [u64; beam_term::HAMT_MAX_LEVEL],
    /// `nleaf` — the number of leaf cons cells reached.
    nleaf: u64,
    /// `nbitmap` — the number of bitmap nodes (head or inner).
    nbitmap: u64,
    /// `narray` — the number of head nodes with all 16 slots present.
    narray: u64,
    /// `ncollision` — the number of collision nodes.
    ncollision: u64,
    /// `max_depth` — the deepest level a node was reached at.
    depth: u64,
}

/// Walks the HAMT rooted at `map` (a hash-map head node) and accumulates the
/// `hashmap_info/2` statistics (`erl_map.c:3486-3536`).
///
/// The C's explicit `ESTACK` of `(node, level)` pairs is reproduced as a `Vec`; its
/// `clvl` is the level a node sits at, the head at `0`, and every node's children are
/// visited one level deeper.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a slot that is neither a
/// leaf nor a well-formed node, and for a level outside the frozen
/// `HAMT_MAX_LEVEL + 2` leaf bins (a map this runtime cannot produce).
fn walk_hashmap(map: Eterm) -> Result<HashMapInfo> {
    let mut info = HashMapInfo::default();
    let mut stack: Vec<(usize, Eterm)> = vec![(0, map)];
    while let Some((level, node)) = stack.pop() {
        // `TAG_PRIMARY_LIST`: a leaf is a two-word `[Key, Value]` cons cell.
        if access::is_list(node) {
            info.nleaf += 1;
            let bin = info.leafs.get_mut(level).ok_or(BeamError::BadArg)?;
            *bin += 1;
            continue;
        }
        let view = access::hamt_node(node)?;
        let slots = view.slots.len();
        match view.kind {
            access::HamtNode::HeadArray => info.narray += 1,
            access::HamtNode::HeadBitmap | access::HamtNode::InnerBitmap => {
                info.nbitmap += 1;
                let bin = info.bitmaps.get_mut(slots - 1).ok_or(BeamError::BadArg)?;
                *bin += 1;
            }
            access::HamtNode::Collision => {
                info.ncollision += 1;
                let bin = slots.min(beam_term::HAMT_MAX_LEVEL) - 1;
                let bin = info.collisions.get_mut(bin).ok_or(BeamError::BadArg)?;
                *bin += 1;
            }
        }
        let child_level = level + 1;
        info.depth = info.depth.max(child_level as u64);
        for slot in view.slots {
            stack.push((child_level, slot));
        }
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::{BeamError, ExitReason};
    use beam_process::ProcessId;
    use beam_term::prelude::make_small;
    use beam_term::{make_atom, AtomTable, NIL};

    /// The whole oracle probe this module's expectations were taken from
    /// (OTP 29.1.1 / erts-17.1, `erl` on `PATH`):
    ///
    /// ```text
    /// erl -noshell -eval '
    ///   R = (try EXPR catch C:E -> {C,E} end), io:format("~p~n",[R]), halt(0).'
    /// ```
    ///
    /// with `EXPR` and the pinned output:
    ///
    /// ```text
    /// erts_debug:unaligned_bitstring(<<1,2,3>>,1)               => <<1,2,3>>
    /// erts_debug:unaligned_bitstring(<<1,2,3>>,2)               => <<1,2,3>>
    /// erts_debug:unaligned_bitstring(<<1,2,3>>,7)               => <<1,2,3>>
    /// erts_debug:unaligned_bitstring(<<1,2,3>>,0)               => {error,badarg}
    /// erts_debug:unaligned_bitstring(<<1,2,3>>,8)               => {error,badarg}
    /// erts_debug:unaligned_bitstring(<<1,2,3>>,9)               => {error,badarg}
    /// erts_debug:unaligned_bitstring(<<1,2,3>>,-1)              => {error,badarg}
    /// erts_debug:unaligned_bitstring(<<1,2,3>>,1.0)             => {error,badarg}
    /// erts_debug:unaligned_bitstring(<<1,2,3>>,foo)             => {error,badarg}
    /// erts_debug:unaligned_bitstring(<<1,2,3>>,1 bsl 60)        => {error,badarg}
    /// erts_debug:unaligned_bitstring(<<1,2,3>>,<<>>)            => {error,badarg}
    /// erts_debug:unaligned_bitstring(<<1,2,3>>,<<"x">>)         => {error,badarg}
    /// erts_debug:unaligned_bitstring(<<>>,1)                    => <<>>
    /// erts_debug:unaligned_bitstring(<<>>,7)                    => <<>>
    /// erts_debug:unaligned_bitstring(foo,1)                     => {error,badarg}
    /// erts_debug:unaligned_bitstring([1,2],1)                   => {error,badarg}
    /// erts_debug:unaligned_bitstring(1,1)                       => {error,badarg}
    /// erts_debug:unaligned_bitstring(<<1:3>>,1)                 => <<1:3>>
    /// erts_debug:unaligned_bitstring(<<17:5>>,3)                => <<17:5>>
    /// erts_debug:unaligned_bitstring(<<255:12>>,4)              => <<15,15:4>>
    /// bit_size(erts_debug:unaligned_bitstring(<<1,2,3>>,1))     => 24
    /// bit_size(erts_debug:unaligned_bitstring(<<1:3>>,1))       => 3
    /// erts_debug:unaligned_bitstring(<<1,2,3>>,1) =:= <<1,2,3>> => true
    /// erts_debug:unaligned_bitstring(<<1:3>>,1) =:= <<1:3>>     => true
    /// binary:referenced_byte_size(…(<<1,2,3>>,1))               => 65
    /// erts_debug:size_shared(<<1,2,3>>)                         => 0
    /// ```
    ///
    /// Note `<<>>` is **not** `badarg` (it *is* a bitstring); the `badarg` sources are
    /// `foo`, `[1,2]` and `1`.

    /// Calls `erts_debug:unaligned_bitstring(Source, Offset)` on its own heap and
    /// returns the observable result shape `(bit_size, bits)`.
    ///
    /// The result term points into the call's heap, so it is read **inside** that
    /// heap's scope and only the copied `(bit_size, bits)` escapes.
    fn probe(source: Eterm, offset: Eterm) -> Result<(usize, Vec<u8>)> {
        let mut heap = Heap::new(256).expect("test heap");
        let atoms = AtomTable::new();
        let mut reg = BifRegistry::new();
        super::register(&mut reg);
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let out = reg.call("erts_debug", "unaligned_bitstring", 2, &mut ctx, &[source, offset])?;
        Ok((access::bitstring_bit_size(out)?, access::bitstring_bits(out)?))
    }

    /// Builds the on-heap bitstring `<<bytes:bits>>` for a test source.
    fn bitstring(heap: &mut Heap, bytes: &[u8], bits: usize) -> Eterm {
        access::make_heap_bitstring(heap, bytes, bits).expect("the source fits")
    }

    /// The pinned `{error, badarg}` cases.
    #[track_caller]
    fn assert_badarg(result: Result<(usize, Vec<u8>)>, label: impl std::fmt::Debug) {
        match result {
            Err(BeamError::BadArg) => {}
            other => panic!("{label:?}: expected BadArg, got {other:?}"),
        }
    }

    /// The types this module owns are exactly [`REAL_BIFS`], and every one of them is
    /// actually registered by [`register`].
    #[test]
    fn real_bifs_is_the_single_key_and_is_registered() {
        assert_eq!(
            REAL_BIFS,
            &[
                ("erts_debug", "map_info", 1),
                ("erts_debug", "unaligned_bitstring", 2)
            ]
        );
        let mut reg = BifRegistry::new();
        super::register(&mut reg);
        for &(module, function, arity) in REAL_BIFS {
            assert!(
                reg.contains(module, function, arity),
                "{module}:{function}/{arity} must be registered by this module"
            );
        }
        // Swapping a seam key for a real one must not change the family's key count.
        assert_eq!(reg.len(), 18, "the erts_debug key count");
    }
    /// Every valid call returns the source's own bits at the source's own bit size —
    /// the C's sub-bit view starts at the very `offset` the bits were copied to, so
    /// the `offset` leading bits are unobservable (oracle: `=:= the source`).
    #[test]
    fn unaligned_bitstring_reproduces_the_source_bits() {
        let mut heap = Heap::new(256).expect("test heap");
        // (source bytes, source bit size, in-range offset)
        let cases: [(Vec<u8>, usize, i64); 9] = [
            (vec![1, 2, 3], 24, 1),
            (vec![1, 2, 3], 24, 2),
            (vec![1, 2, 3], 24, 7),
            (vec![0x20], 3, 1),        // <<1:3>>
            (vec![0x88], 5, 3),        // <<17:5>>
            (vec![0x0F, 0xF0], 12, 4), // <<255:12>> (prints <<15,15:4>>)
            (vec![0xFF, 0xFF], 16, 5),
            (vec![], 0, 1), // <<>> is a bitstring, not an error
            (vec![], 0, 7),
        ];
        for (bytes, bits, offset) in cases {
            let source = bitstring(&mut heap, &bytes, bits);
            let (got_bits, got_bytes) = probe(source, make_small(offset))
                .unwrap_or_else(|e| panic!("({bytes:?}/{bits}, {offset}) errored: {e:?}"));
            assert_eq!(got_bits, bits, "bit size for {bytes:?}/{bits} at offset {offset}");
            assert_eq!(got_bytes, bytes, "bits for {bytes:?}/{bits} at offset {offset}");
        }
    }

    /// Offsets outside the open interval `(0, 8)` are `badarg` (the C's
    /// `!(0 < offset && offset < 8)` guard; `0` and `8` are both rejected).
    #[test]
    fn unaligned_bitstring_rejects_offsets_outside_one_to_seven() {
        let mut heap = Heap::new(256).expect("test heap");
        let source = bitstring(&mut heap, &[1, 2, 3], 24);
        for offset in [0i64, 8, 9, -1, 1000, -(1 << 58)] {
            assert_badarg(probe(source, make_small(offset)), offset);
        }
    }

    /// A non-small offset (a float, a bignum, an atom) is `badarg` — the C checks
    /// `is_not_small` before it looks at the value.
    #[test]
    fn unaligned_bitstring_rejects_a_non_small_offset() {
        let mut heap = Heap::new(256).expect("test heap");
        let source = bitstring(&mut heap, &[1, 2, 3], 24);
        let float = access::make_float(&mut heap, 1.0).expect("the float fits");
        // 2^60 > the 60-bit small-integer range.
        let big = beam_codec::term::make_big(&mut heap, false, &[1 << 60]).expect("the bignum fits");
        let mut atoms = AtomTable::new();
        atoms.seed(["foo"]);
        let atom = make_atom(atoms.lookup("foo").expect("seeded"));
        assert_badarg(probe(source, float), "float offset");
        assert_badarg(probe(source, big), "bignum offset");
        assert_badarg(probe(source, atom), "atom offset");
    }

    /// A non-bitstring source is `badarg` (the C's `is_not_bitstring` guard runs
    /// before the offset is looked at).
    #[test]
    fn unaligned_bitstring_rejects_a_non_bitstring_source() {
        let mut heap = Heap::new(256).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(["foo"]);
        let atom = make_atom(atoms.lookup("foo").expect("seeded"));
        let list = heap.cons(make_small(1), NIL).expect("the cons fits");
        let tuple = heap.tuple(&[make_small(1)]).expect("the tuple fits");
        for (label, source) in [
            ("atom", atom),
            ("integer", make_small(7)),
            ("nil", NIL),
            ("list", list),
            ("tuple", tuple),
        ] {
            assert_badarg(probe(source, make_small(1)), label);
        }
    }

    /// Every other key stays seam-backed: with no `RuntimeHooks` installed a call
    /// answers the typed capability gap rather than a fabricated value.
    #[test]
    fn registers_and_gaps_without_a_hook() {
        let mut reg = BifRegistry::new();
        super::register(&mut reg);
        assert!(reg.contains("erts_debug", "interpreter_size", 0));
        assert!(reg.contains("erts_debug", "set_internal_state", 2));

        let mut heap = Heap::new(8).expect("test heap");
        let atoms = AtomTable::new();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        match reg.call("erts_debug", "interpreter_size", 0, &mut ctx, &[]) {
            Err(BeamError::Raise(r)) => {
                assert_eq!(r.reason, ExitReason::atom("unsupported_bif"));
            }
            other => panic!("expected unsupported_bif, got {other:?}"),
        }
        // `size_shared/1` stays a gap on purpose: the sharing it measures is
        // unrepresentable here, so any answer would be a fabricated value (in
        // particular never a constant `0`).
        match reg.call("erts_debug", "size_shared", 1, &mut ctx, &[make_small(7)]) {
            Err(BeamError::Raise(r)) => {
                assert_eq!(r.reason, ExitReason::atom("unsupported_bif"));
            }
            other => panic!("expected unsupported_bif, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // map_info/1
    //
    // The whole oracle probe these expectations were taken from (OTP 29.1.1 /
    // erts-17.1, `erl` on `PATH`):
    //
    // ```text
    // erl -noshell -eval '
    //   R = (try EXPR catch C:E -> {C,E} end), io:format("~p~n",[R]), halt(0).'
    // ```
    //
    // ```text
    // erts_debug:map_info(#{a=>1,b=>2})                       => {error,badarg}
    // erts_debug:map_info(#{})                                => {error,badarg}
    // erts_debug:map_info(maps:from_list([{I,I} || I <- lists:seq(1,32)]))
    //                                                         => {error,badarg}
    // erts_debug:map_info(1)                                  => {error,{badmap,1}}
    // erts_debug:map_info("abc")                              => {error,{badmap,"abc"}}
    // erts_debug:map_info(maps:from_list([{I,I} || I <- lists:seq(1,33)]))
    //   => [{depth,4},
    //       {arrays,0,[]},
    //       {collisions,0,{0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0}},
    //       {bitmaps,12,{1,5,1,3,1,0,0,0,0,0,0,0,1,0,0,0}},
    //       {leafs,33,{0,4,27,0,2,0,0,0,0,0,0,0,0,0,0,0,0,0}}]
    // erts_debug:map_info(maps:from_list([{I,I} || I <- lists:seq(1,40)]))
    //   => [{depth,4},
    //       {arrays,0,[]},
    //       {collisions,0,{0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0}},
    //       {bitmaps,14,{1,5,3,2,1,1,0,0,0,0,0,0,0,1,0,0}},
    //       {leafs,40,{0,4,32,2,2,0,0,0,0,0,0,0,0,0,0,0,0,0}}]
    // erts_debug:map_info(maps:put(100, M40, maps:from_list([{I,I} || I <- lists:seq(50,90)])))
    //   => [{depth,3},
    //       {arrays,1,[]},
    //       {collisions,0,{0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0}},
    //       {bitmaps,16,{0,9,4,3,0,0,0,0,0,0,0,0,0,0,0,0}},
    //       {leafs,42,{0,5,27,10,0,0,0,0,0,0,0,0,0,0,0,0,0,0}}]
    // ```
    //
    // The maps are built from **integer** keys on purpose: an integer key's
    // `erts_map_hash` is a pure function of the term (no runtime atom index), so the
    // HAMT the oracle builds for `maps:from_list/1` has the same trie shape as the one
    // `beam_term::map::write_map` builds here — which is what makes the summary
    // comparable at all. An atom-keyed map's trie shape is driven by the runtime atom
    // index and legitimately differs (see `beam_term::ihash`).

    /// Builds `maps:from_list([{K, K} || K <- keys])` through this crate's map
    /// producer ([`access::make_map`] → `beam_term::map::write_map`).
    fn int_map(heap: &mut Heap, keys: impl IntoIterator<Item = i64>) -> Eterm {
        let pairs: Vec<(Eterm, Eterm)> = keys
            .into_iter()
            .map(|key| (make_small(key), make_small(key)))
            .collect();
        access::make_map(heap, &pairs).expect("the map fits")
    }

    /// Calls `map_info/1` on `map` (which must already live on `heap`).
    fn probe_info(heap: &mut Heap, atoms: &AtomTable, map: Eterm) -> Result<Eterm> {
        let mut reg = BifRegistry::new();
        super::register(&mut reg);
        let mut ctx = BifContext::new(heap, atoms, ProcessId::new(1, 0));
        reg.call("erts_debug", "map_info", 1, &mut ctx, &[map])
    }

    /// The elements of the proper list `term`.
    fn list_of(term: Eterm) -> Vec<Eterm> {
        let mut out = Vec::new();
        let mut cur = term;
        while access::is_list(cur) {
            out.push(access::car(cur));
            cur = access::cdr(cur);
        }
        assert!(access::is_nil(cur), "the result must be a proper list");
        out
    }

    /// The small integers of the tuple `term`.
    fn ints_of(term: Eterm) -> Vec<i64> {
        (0..access::tuple_size(term))
            .map(|i| signed_value(access::tuple_element(term, i)))
            .collect()
    }

    /// Reads the `[{depth, D}, {arrays, N, []}, {collisions, N, Bins},
    /// {bitmaps, N, Bins}, {leafs, N, Bins}]` summary into Rust values, checking every
    /// tag atom and the arrays entry's `[]` tail against `atoms`.
    #[allow(clippy::type_complexity)]
    fn read_info(
        info: Eterm,
        atoms: &AtomTable,
    ) -> (
        i64,
        i64,
        (i64, Vec<i64>),
        (i64, Vec<i64>),
        (i64, Vec<i64>),
    ) {
        let entries = list_of(info);
        assert_eq!(entries.len(), 5, "the five summary entries");
        let tag = |entry: Eterm| {
            let atom = access::tuple_element(entry, 0);
            atoms
                .name(beam_term::atom_val(atom))
                .expect("a seeded tag atom")
                .to_string()
        };
        for (index, name) in ["depth", "arrays", "collisions", "bitmaps", "leafs"]
            .into_iter()
            .enumerate()
        {
            assert_eq!(tag(entries[index]), name, "entry {index}");
        }
        assert!(
            access::is_nil(access::tuple_element(entries[1], 2)),
            "{{arrays, N, []}}"
        );
        let count = |entry: Eterm| signed_value(access::tuple_element(entry, 1));
        let bins = |entry: Eterm| ints_of(access::tuple_element(entry, 2));
        (
            count(entries[0]),
            count(entries[1]),
            (count(entries[2]), bins(entries[2])),
            (count(entries[3]), bins(entries[3])),
            (count(entries[4]), bins(entries[4])),
        )
    }

    /// A flat map (any size at or below `MAP_SMALL_MAP_LIMIT`) is `error:badarg`; any
    /// non-map is the structured `error:{badmap, Term}` — the C's test order
    /// (`erl_map.c:3300-3308`).
    #[test]
    fn map_info_rejects_flat_maps_and_non_maps() {
        let mut atoms = AtomTable::new();
        atoms.seed(["badmap", "depth", "arrays", "collisions", "bitmaps", "leafs"]);
        let mut heap = Heap::new(1 << 12).expect("test heap");
        let empty = access::make_map(&mut heap, &[]).expect("the empty map fits");
        let flat = int_map(&mut heap, 1..=32);
        let tuple = heap.tuple(&[make_small(1)]).expect("the tuple fits");
        for (label, term) in [("empty map", empty), ("32-key flat map", flat)] {
            match probe_info(&mut heap, &atoms, term) {
                Err(BeamError::BadArg) => {}
                other => panic!("{label}: expected BadArg, got {other:?}"),
            }
        }
        for (label, term) in [
            ("integer", make_small(1)),
            ("nil", NIL),
            ("tuple", tuple),
        ] {
            match probe_info(&mut heap, &atoms, term) {
                Err(BeamError::Raise(raise)) => {
                    assert_eq!(raise.reason, ExitReason::Structured, "{label}");
                    // The live reason term is `{badmap, Term}`.
                    let reason = raise
                        .live_reason()
                        .and_then(|live| live.downcast_ref::<Eterm>())
                        .copied()
                        .unwrap_or_else(|| panic!("{label}: no live reason term"));
                    assert!(access::is_tuple(reason), "{label}: a 2-tuple");
                    assert_eq!(access::tuple_size(reason), 2, "{label}");
                    assert_eq!(
                        atoms.name(beam_term::atom_val(access::tuple_element(reason, 0))),
                        Some("badmap"),
                        "{label}"
                    );
                    assert_eq!(access::tuple_element(reason, 1), term, "{label}");
                }
                other => panic!("{label}: expected a raised {{badmap, Term}}, got {other:?}"),
            }
        }
    }

    /// The traversal statistics for the HAMT maps the oracle pins.
    #[test]
    fn map_info_summarises_a_hamt_like_the_oracle() {
        let mut atoms = AtomTable::new();
        atoms.seed(["badmap", "depth", "arrays", "collisions", "bitmaps", "leafs"]);
        let mut heap = Heap::new(1 << 16).expect("test heap");

        // 33 keys: a bitmap head with 13 slots, 4 of them leaves.
        let m33 = int_map(&mut heap, 1..=33);
        let info = probe_info(&mut heap, &atoms, m33).expect("map_info runs");
        assert_eq!(
            read_info(info, &atoms),
            (
                4,
                0,
                (0, vec![0; 16]),
                (12, vec![1, 5, 1, 3, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0]),
                (
                    33,
                    vec![0, 4, 27, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
                ),
            ),
            "the 33-key map"
        );

        // 40 keys: a deeper bitmap head (14 nodes, one of them with 14 slots).
        let m40 = int_map(&mut heap, 1..=40);
        let info = probe_info(&mut heap, &atoms, m40).expect("map_info runs");
        assert_eq!(
            read_info(info, &atoms),
            (
                4,
                0,
                (0, vec![0; 16]),
                (14, vec![1, 5, 3, 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0]),
                (
                    40,
                    vec![0, 4, 32, 2, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
                ),
            ),
            "the 40-key map"
        );
    }

    /// The head node is a 16-slot **array** node once every nibble is present, and the
    /// walk counts it as `arrays`, not as a bitmap node (`erl_map.c:3509-3512`).
    #[test]
    fn map_info_counts_a_full_head_as_an_array_node() {
        let mut atoms = AtomTable::new();
        atoms.seed(["badmap", "depth", "arrays", "collisions", "bitmaps", "leafs"]);
        let mut heap = Heap::new(1 << 16).expect("test heap");
        let inner = int_map(&mut heap, 1..=40);
        let mut pairs: Vec<(Eterm, Eterm)> = (50..=90)
            .map(|key| (make_small(key), make_small(key)))
            .collect();
        pairs.push((make_small(100), inner));
        let map = access::make_map(&mut heap, &pairs).expect("the map fits");

        let info = probe_info(&mut heap, &atoms, map).expect("map_info runs");
        assert_eq!(
            read_info(info, &atoms),
            (
                3,
                1,
                (0, vec![0; 16]),
                (16, vec![0, 9, 4, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
                (
                    42,
                    vec![0, 5, 27, 10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
                ),
            ),
            "the nested 42-key map"
        );
    }

    /// A HAMT *inner* node is not `is_hashmap`, so it is `{badmap, Node}` — not a
    /// summary (the C's `is_hashmap`/`is_flatmap`/`BADMAP` test order).
    #[test]
    fn map_info_rejects_a_hamt_inner_node_as_badmap() {
        let mut atoms = AtomTable::new();
        atoms.seed(["badmap", "depth", "arrays", "collisions", "bitmaps", "leafs"]);
        let mut heap = Heap::new(1 << 16).expect("test heap");
        let map = int_map(&mut heap, 1..=33);
        let node = {
            let view = access::hamt_node(map).expect("a head node");
            // A sub-node slot: a boxed HAMT node.
            let sub = view
                .slots
                .iter()
                .copied()
                .find(|slot| access::hamt_node_kind(*slot) == Some(access::HamtNode::InnerBitmap))
                .expect("the 33-key head has sub-nodes");
            assert!(access::is_map(sub), "an inner node is still is_map");
            assert!(!access::is_hashmap(sub), "but not is_hashmap");
            sub
        };
        match probe_info(&mut heap, &atoms, node) {
            Err(BeamError::Raise(raise)) => {
                assert_eq!(raise.reason, ExitReason::Structured);
                let reason = raise
                    .live_reason()
                    .and_then(|live| live.downcast_ref::<Eterm>())
                    .copied()
                    .expect("a live reason term");
                assert_eq!(access::tuple_element(reason, 1), node);
            }
            other => panic!("expected a raised {{badmap, Node}}, got {other:?}"),
        }
    }
}
