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

//! Term construction/inspection BIFs for `erlang`: `element/2`, `setelement/3`,
//! `tuple_size/1`, `tuple_to_list/1`, `list_to_tuple/1`, `length/1`, `hd/1`,
//! `tl/1` and `size/1`.
//!
//! Ground truth (read-only): the `bif.tab` roster (`ubif erlang:element/2`,
//! `ubif erlang:length/1`, `ubif erlang:tuple_size/1`, `ubif erlang:size/1`,
//! `ubif erlang:hd/1`, `ubif erlang:tl/1`, `bif erlang:setelement/3`,
//! `bif erlang:list_to_tuple/1`, `bif erlang:tuple_to_list/1`) and the
//! implementations in `../../erts/emulator/beam/erl_bif_tuple.c` /
//! `erl_bif_lists.c`.
//!
//! # Pinned against the OTP 29 oracle
//!
//! * `element/2` is 1-based: `element(1, {a})` ⇒ `a`; `element(0, {a})`,
//!   `element(2, {a})`, `element(1.0, {a})` and `element(1, [1])` ⇒ `badarg`.
//! * `setelement(1, {a,b}, x)` ⇒ `{x,b}`; an out-of-range index ⇒ `badarg`.
//! * `tuple_size({1,2})` ⇒ `2`; a non-tuple ⇒ `badarg`.
//! * `tuple_to_list({1,2})` ⇒ `[1,2]`; `list_to_tuple([1,2])` ⇒ `{1,2}`;
//!   `list_to_tuple([1|2])` (improper) ⇒ `badarg`.
//! * `length([a|b])` (improper) ⇒ `badarg`; `hd([])`/`tl([])` ⇒ `badarg`.
//! * `size({1,2})` ⇒ `2`; `size(<<1,2,3>>)` ⇒ `3`; `size(<<1:1>>)` ⇒ `0`;
//!   `size([1,2])` ⇒ `badarg`.
//!
//! # `size/1` on a bitstring
//!
//! `size/1` is `tuple_size/1` for tuples and the *byte* size for a binary or
//! bitstring: the oracle truncates the bit size (`size(<<1:1>>)` ⇒ `0`,
//! `size(<<1:9>>)` ⇒ `1`, both verified against erts-17.1). The bitstrings
//! themselves are built and read through [`crate::bits`].

use beam_common::Result;
use beam_term::{signed_value, Eterm};

use crate::access;
use crate::bits;
use crate::context::BifContext;
use crate::error::{badarg, system_limit};
use crate::registry::BifRegistry;

/// `element(N, Tuple)` — 0-based element index, validated against the tuple.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `n` is not a small
/// integer in `1..=tuple_size`, or `tuple` is not a tuple.
fn element_index(n: Eterm, tuple: Eterm) -> Result<usize> {
    if !access::is_small(n) || !access::is_tuple(tuple) {
        return Err(badarg());
    }
    let index = signed_value(n);
    let size = access::tuple_size(tuple);
    if index < 1 || index as u64 > size as u64 {
        return Err(badarg());
    }
    Ok(index as usize - 1)
}

/// Counts a proper list, rejecting a non-list or an improper tail.
///
/// The count is the whole of `length/1`, so this walk allocates nothing and never
/// materializes the elements.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `t` is not a proper
/// list.
fn proper_list_len(t: Eterm) -> Result<usize> {
    let mut len = 0usize;
    let mut tail = t;
    while access::is_list(tail) {
        len += 1;
        tail = access::cdr(tail);
    }
    if access::is_nil(tail) {
        Ok(len)
    } else {
        Err(badarg())
    }
}

/// Walks a proper list into a `Vec`, rejecting a non-list or an improper tail.
///
/// The count pass reserves the element buffer at the list's *exact* length, so the
/// fill is a single allocation instead of a doubling sequence, and a rejected list
/// allocates nothing at all (the failure is found before the buffer exists).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `t` is not a proper
/// list.
fn proper_list(t: Eterm) -> Result<Vec<Eterm>> {
    let len = proper_list_len(t)?;
    let mut elems = Vec::with_capacity(len);
    let mut tail = t;
    while access::is_list(tail) {
        elems.push(access::car(tail));
        tail = access::cdr(tail);
    }
    Ok(elems)
}

/// `element(N, Tuple)`.
fn element(_ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let index = element_index(args[0], args[1])?;
    Ok(access::tuple_element(args[1], index))
}

/// `setelement(N, Tuple, Value)` — a *new* tuple with element `N` replaced.
fn setelement(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let tuple = args[1];
    let index = element_index(args[0], tuple)?;
    let mut elems: Vec<Eterm> = (0..access::tuple_size(tuple))
        .map(|i| access::tuple_element(tuple, i))
        .collect();
    elems[index] = args[2];
    ctx.tuple(&elems)
}

/// `tuple_size(Tuple)`.
fn tuple_size(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if !access::is_tuple(args[0]) {
        return Err(badarg());
    }
    ctx.int(access::tuple_size(args[0]) as i64)
}

/// `tuple_to_list(Tuple)`.
fn tuple_to_list(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let tuple = args[0];
    if !access::is_tuple(tuple) {
        return Err(badarg());
    }
    let elems: Vec<Eterm> = (0..access::tuple_size(tuple))
        .map(|i| access::tuple_element(tuple, i))
        .collect();
    ctx.list(&elems)
}

/// `list_to_tuple(List)`.
fn list_to_tuple(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let elems = proper_list(args[0])?;
    ctx.tuple(&elems)
}

/// `length(List)`.
fn length(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let len = proper_list_len(args[0])?;
    ctx.int(i64::try_from(len).map_err(|_| system_limit())?)
}

/// `hd(List)`.
fn hd(_ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if !access::is_list(args[0]) {
        return Err(badarg());
    }
    Ok(access::car(args[0]))
}

/// `tl(List)`.
fn tl(_ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if !access::is_list(args[0]) {
        return Err(badarg());
    }
    Ok(access::cdr(args[0]))
}

/// `size(Term)` — the tuple size, or a bitstring's byte size.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for anything that is
/// neither a tuple nor a bitstring (`size([1,2])` ⇒ `badarg`);
/// [`BeamError::SystemLimit`](beam_common::BeamError::SystemLimit) if the byte size
/// does not fit a small integer.
fn size(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let term = args[0];
    if access::is_tuple(term) {
        return ctx.int(access::tuple_size(term) as i64);
    }
    if bits::is_bitstring(term) {
        // The oracle truncates: `size(<<1,2,3>>)` ⇒ `3`, `size(<<1:1>>)` ⇒ `0`.
        let bytes = bits::bit_size(term)? / 8;
        return ctx.int(i64::try_from(bytes).map_err(|_| system_limit())?);
    }
    Err(badarg())
}
/// Registers the `erlang` term-operation BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "element", 2, element);
    reg.register("erlang", "setelement", 3, setelement);
    reg.register("erlang", "tuple_size", 1, tuple_size);
    reg.register("erlang", "tuple_to_list", 1, tuple_to_list);
    reg.register("erlang", "list_to_tuple", 1, list_to_tuple);
    reg.register("erlang", "length", 1, length);
    reg.register("erlang", "hd", 1, hd);
    reg.register("erlang", "tl", 1, tl);
    reg.register("erlang", "size", 1, size);
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable, NIL};

    fn atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok", "a", "b"]);
        atoms
    }

    fn with_ctx<R>(f: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
        let mut heap = Heap::new(256).expect("test heap");
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&mut ctx)
    }

    #[test]
    fn element_is_one_based_and_checked() {
        with_ctx(|ctx| {
            let tup = ctx.tuple(&[make_small(10), make_small(20)]).unwrap();
            assert_eq!(element(ctx, &[make_small(1), tup]).unwrap(), make_small(10));
            assert_eq!(element(ctx, &[make_small(2), tup]).unwrap(), make_small(20));

            // Out of range and non-integer indices are `badarg`.
            assert!(matches!(element(ctx, &[make_small(0), tup]), Err(BeamError::BadArg)));
            assert!(matches!(element(ctx, &[make_small(3), tup]), Err(BeamError::BadArg)));
            let bad = ctx.make_float(1.0).unwrap();
            assert!(matches!(element(ctx, &[bad, tup]), Err(BeamError::BadArg)));
            let a = ctx.atom("a").unwrap();
            assert!(matches!(element(ctx, &[a, tup]), Err(BeamError::BadArg)));
            // A non-tuple second argument is `badarg`.
            assert!(matches!(element(ctx, &[make_small(1), NIL]), Err(BeamError::BadArg)));
        });
    }

    #[test]
    fn setelement_replaces_in_a_new_tuple() {
        with_ctx(|ctx| {
            let tup = ctx.tuple(&[make_small(1), make_small(2)]).unwrap();
            let replaced = setelement(ctx, &[make_small(1), tup, make_small(9)]).unwrap();
            assert!(access::is_tuple(replaced));
            assert_eq!(access::tuple_size(replaced), 2);
            assert_eq!(access::tuple_element(replaced, 0), make_small(9));
            assert_eq!(access::tuple_element(replaced, 1), make_small(2));
            // The original is unchanged.
            assert_eq!(access::tuple_element(tup, 0), make_small(1));

            assert!(matches!(
                setelement(ctx, &[make_small(3), tup, make_small(9)]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                setelement(ctx, &[make_small(0), tup, make_small(9)]),
                Err(BeamError::BadArg)
            ));
        });
    }

    #[test]
    fn tuple_size_and_size() {
        with_ctx(|ctx| {
            let tup = ctx.tuple(&[make_small(1), make_small(2), make_small(3)]).unwrap();
            assert_eq!(tuple_size(ctx, &[tup]).unwrap(), make_small(3));
            assert_eq!(size(ctx, &[tup]).unwrap(), make_small(3));
            // `size([])` and `size([1,2])` are `badarg`.
            assert!(matches!(size(ctx, &[NIL]), Err(BeamError::BadArg)));
            let list = ctx.list(&[make_small(1), make_small(2)]).unwrap();
            assert!(matches!(size(ctx, &[list]), Err(BeamError::BadArg)));
            assert!(matches!(size(ctx, &[make_small(1)]), Err(BeamError::BadArg)));
            assert!(matches!(tuple_size(ctx, &[make_small(1)]), Err(BeamError::BadArg)));
        });
    }

    #[test]
    fn size_of_a_bitstring_is_its_byte_size() {
        with_ctx(|ctx| {
            // A three-byte binary and the empty binary.
            let bin = bits::make_binary(ctx.heap(), &[1, 2, 3]).unwrap();
            assert_eq!(size(ctx, &[bin]).unwrap(), make_small(3));
            let empty = bits::make_binary(ctx.heap(), &[]).unwrap();
            assert_eq!(size(ctx, &[empty]).unwrap(), make_small(0));
            // The oracle truncates the bit size: `<<1:1>>` ⇒ 0, `<<1:9>>` ⇒ 1.
            let one_bit = bits::make_bitstring(ctx.heap(), &[0b1000_0000], 1).unwrap();
            assert_eq!(size(ctx, &[one_bit]).unwrap(), make_small(0));
            let nine_bits = bits::make_bitstring(ctx.heap(), &[0, 0b1000_0000], 9).unwrap();
            assert_eq!(size(ctx, &[nine_bits]).unwrap(), make_small(1));
            let eight_bits = bits::make_bitstring(ctx.heap(), &[0xFF], 8).unwrap();
            assert_eq!(size(ctx, &[eight_bits]).unwrap(), make_small(1));
        });
    }
    #[test]
    fn tuple_and_list_conversions() {
        with_ctx(|ctx| {
            let tup = ctx.tuple(&[make_small(1), make_small(2)]).unwrap();
            let list = tuple_to_list(ctx, &[tup]).unwrap();
            assert_eq!(length(ctx, &[list]).unwrap(), make_small(2));
            assert_eq!(hd(ctx, &[list]).unwrap(), make_small(1));
            let rest = tl(ctx, &[list]).unwrap();
            assert_eq!(hd(ctx, &[rest]).unwrap(), make_small(2));
            assert_eq!(tl(ctx, &[rest]).unwrap(), NIL);

            let back = list_to_tuple(ctx, &[list]).unwrap();
            assert!(access::is_tuple(back));
            assert_eq!(access::tuple_size(back), 2);
            assert_eq!(access::tuple_element(back, 1), make_small(2));

            // Empty conversions.
            assert!(matches!(tuple_to_list(ctx, &[NIL]), Err(BeamError::BadArg)));
            let empty = list_to_tuple(ctx, &[NIL]).unwrap();
            assert!(access::is_tuple(empty));
            assert_eq!(access::tuple_size(empty), 0);
            assert_eq!(tuple_to_list(ctx, &[empty]).unwrap(), NIL);
        });
    }

    #[test]
    fn length_hd_tl_reject_improper_and_empty_lists() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").unwrap();
            let b = ctx.atom("b").unwrap();
            let proper = ctx.list(&[a, b]).unwrap();
            assert_eq!(length(ctx, &[proper]).unwrap(), make_small(2));
            assert_eq!(hd(ctx, &[proper]).unwrap(), a);
            let rest = tl(ctx, &[proper]).unwrap();
            assert_eq!(hd(ctx, &[rest]).unwrap(), b);
            assert_eq!(tl(ctx, &[rest]).unwrap(), NIL);

            let improper = ctx.cons(a, b).unwrap();
            assert!(matches!(length(ctx, &[improper]), Err(BeamError::BadArg)));
            assert!(matches!(hd(ctx, &[NIL]), Err(BeamError::BadArg)));
            assert!(matches!(tl(ctx, &[NIL]), Err(BeamError::BadArg)));
            assert!(matches!(length(ctx, &[make_small(1)]), Err(BeamError::BadArg)));
            assert!(matches!(list_to_tuple(ctx, &[improper]), Err(BeamError::BadArg)));
        });
    }

    #[test]
    fn register_installs_every_term_op() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        for (name, arity) in [
            ("element", 2),
            ("setelement", 3),
            ("tuple_size", 1),
            ("tuple_to_list", 1),
            ("list_to_tuple", 1),
            ("length", 1),
            ("hd", 1),
            ("tl", 1),
            ("size", 1),
        ] {
            assert!(reg.contains("erlang", name, arity), "missing erlang:{name}/{arity}");
        }
        assert_eq!(reg.len(), 9);
    }
}
