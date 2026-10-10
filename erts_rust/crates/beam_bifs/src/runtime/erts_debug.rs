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
//! `unaligned_bitstring/2` is implemented here **for real** — a pure term
//! transformation over the shared [`crate::access`] bitstring surface (see
//! `REAL_BIFS`).
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

use beam_common::Result;
use beam_term::prelude::signed_value;
use beam_term::Eterm;

use crate::access;
use crate::context::BifContext;
use crate::error::badarg;
use crate::registry::BifRegistry;

/// The `(module, function, arity)` keys this module implements **for real** (rather
/// than forwarding to the [`RuntimeHooks`](crate::context::RuntimeHooks) seam).
///
/// Sorted by `(module, function, arity)`. A follow-up task (the `hook_sets` pass)
/// consumes this list to drop the matching `runtime_bif!` key from the
/// `RUNTIME_HOOKS_BIFS` roster of `src/hook_sets.rs`; until then it is the single
/// source of truth for which keys the real implementations own, and [`register`]
/// asserts every one of them is actually present after registration.
pub(crate) const REAL_BIFS: &[(&str, &str, u32)] = &[("erts_debug", "unaligned_bitstring", 2)];

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
    // bitmaps, leafs); walking that interior is confined to `crate::access`, this
    // crate's `#[allow(unsafe_code)]` term-access module, and no such accessor exists
    // yet — so the key is a typed gap rather than a fabricated summary.
    crate::runtime::runtime_bif!(reg, "erts_debug", map_info, 1);
    crate::runtime::runtime_bif!(reg, "erts_debug", set_internal_state, 2);
    // A gap on purpose — the sharing it measures is unrepresentable here (see the
    // module docs), so a value would be fabricated.
    crate::runtime::runtime_bif!(reg, "erts_debug", size_shared, 1);
    // Implemented for real (a pure bitstring-to-bitstring transformation).
    reg.register("erts_debug", "unaligned_bitstring", 2, unaligned_bitstring);
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
        assert_eq!(REAL_BIFS, &[("erts_debug", "unaligned_bitstring", 2)]);
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
}
