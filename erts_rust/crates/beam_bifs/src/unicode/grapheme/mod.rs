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
//! Extended grapheme cluster segmentation — the oracle's `unicode_util:gc/1`.
//!
//! `string:next_grapheme/1` is `unicode_util:gc(CD)` (`lib/stdlib/src/string.erl`
//! @1119) and `string:length/1` counts clusters by repeatedly splitting with the
//! same walk (`string.erl` @1145-1199). Both are *pure Erlang* in OTP, generated
//! from `lib/stdlib/uc_spec/gen_unicode_mod.escript` (`gen_gc/2` @787, `gen_cp/1`
//! @708) into `unicode_util.erl`. This module is a faithful port of that generated
//! code — **not** an independent implementation of UAX #29 — so that the
//! extension/ruleset quirks the generator bakes in (the exact `gc_1/2` clause
//! order, the "losing work done on `T`" roll-backs, the byte-tail preservation)
//! are reproduced rather than re-derived.
//!
//! Layout:
//!
//! * [`props`] — the generated Unicode property tables and their predicates
//!   (`is_control/1`, `is_extend/1`, `is_ext_pict/1`, the Hangul classes, …).
//! * [`chardata`] — `cp/1` and its `cpl*` / `cp_no_bin*` helpers: the code-point
//!   **normalisation front end** (it flattens just enough of a chardata term's
//!   front to expose one code point, preserving the tail representation verbatim).
//! * [`api`] — the two entry points (`next_grapheme/1` ≡ `gc/1`,
//!   `grapheme_count/1` ≡ `string:length/1`).
//!
//! Everything below is the port of `gc/1` and the `gc_*` family. The clause
//! *order* of `gc_1/2` is load-bearing and is preserved exactly: the whole
//! `cr ∪ lf ∪ control` set is one early group (so U+00AD, a control singleton, is
//! never shadowed by the Latin-1 fast path), then the two `Extended_Pictographic`
//! code points below 256, then the Latin-1 byte-tail fast path, then the
//! out-of-range raise, then prepend / Hangul / regional / default.
//!
//! `gc/1`'s accumulators (`Acc`, `CPs`) are Erlang lists; they are held here as a
//! `Vec<Eterm>` in **Erlang order** (most recently consed first), so `[CP|Acc]` is
//! `acc.insert(0, cp)`, `lists:reverse(Acc)` is `acc.iter()`, `Acc =:= []` is
//! `acc.is_empty()` and `Acc =:= [CP]` is `acc.len() == 1`.
//!
//! Failure is return-based (workspace `CONTEXT.md` §3): the port returns the
//! oracle's own raise as a typed [`beam_common::BeamError`] — the bare
//! `error:function_clause`, the bare `error:badarg` of `gc_h_lv_lvt/3` and of the
//! `++` in `gc_ext_pict/3`, and the structured `{badarg, CP}` of `gc_1/2` — never a
//! wrong result.

// The two entry points this module exposes are consumed through the `pub(crate)`
// re-export below by the `string:next_grapheme/1` / `string:length/1` BIF wiring
// (and by the `~ts`-under-`{chars_limit, N}` path in `../../io.rs`) that lives in
// sibling modules of this crate. Until that wiring is present in a given checkout,
// the only in-crate caller of the port is the oracle corpus in `tests`, so rustc's
// dead-code lint flags the whole faithful-port surface — every `gc_*` clause, the
// `cp`/`cpl` normalisation front end in `chardata` and the generated property
// tables in `props.rs`. Each of those items exists to reproduce the oracle's
// generated `unicode_util:gc/1` clause-for-clause (the `gc_1/2` clause *order* is
// load-bearing), so they are this module's API, not leftovers: keep them.
#![allow(dead_code)]

use beam_common::Result;
use beam_term::{atom_val, Eterm, NIL};

use crate::access;
use crate::bits;
use crate::context::BifContext;
use crate::error::{badarg, function_clause, structured_error};
use crate::unicode::{decode_one, Decode};

mod api;
mod chardata;
mod props;

// The module's frozen `pub(crate)` surface —
// `crate::unicode::grapheme::{next_grapheme, grapheme_count}` — and the exact path
// its consumers resolve. The re-export is what makes `api`'s two entry points
// reachable, so it must stay even while no non-test code in this crate names it
// (see the `dead_code` note above); the corpus in `tests` reaches both through
// this path.
#[allow(unused_imports)]
pub(crate) use api::{grapheme_count, next_grapheme};

use chardata::{cp, cp_no_bin, NoBin};
use props::{
    in_l, in_lv, in_lvt, in_prepend, in_regional, in_t, in_v, is_control, is_ext_pict,
    is_indic_consonant, is_indic_linker, Extend,
};

// ---------------------------------------------------------------------------
// Term predicates
// ---------------------------------------------------------------------------

/// The value of a **small** integer term.
///
/// `None` for a bignum, a float or anything else. This is deliberately
/// small-integer-only: every range check in the walk is `is_integer(X, Lo, Hi)`
/// with `Hi <= 16#10FFFF`, and a normalised bignum never holds a value below
/// `2^59`, so a bignum can never satisfy one.
fn int_value(term: Eterm) -> Option<i64> {
    if access::is_small(term) {
        Some(beam_term::signed_value(term))
    } else {
        None
    }
}

/// `IS_CP(X)` — `is_integer(X, 0, 16#10FFFF)`.
pub(super) fn is_cp(term: Eterm) -> bool {
    cp_value(term).is_some()
}

/// `X` as a code point, when it is an integer in `0..=16#10FFFF`.
fn cp_value(term: Eterm) -> Option<u32> {
    match int_value(term) {
        Some(value) if (0..=0x10FFFF).contains(&value) => Some(value as u32),
        _ => None,
    }
}

/// `IS_LATIN1(X)` — `is_integer(X, 0, 255)`.
fn latin1_value(term: Eterm) -> Option<u8> {
    match int_value(term) {
        Some(value) if (0..=0xFF).contains(&value) => Some(value as u8),
        _ => None,
    }
}

/// `[H | T]` — `Some((H, T))` for a cons cell, `None` otherwise.
///
/// Note the oracle's list clauses match a **cons cell** (`[H|T]`) and `[]`
/// separately, so this deliberately answers `None` for `NIL`.
pub(super) fn head_tail(term: Eterm) -> Option<(Eterm, Eterm)> {
    if access::is_list(term) {
        Some((access::car(term), access::cdr(term)))
    } else {
        None
    }
}

/// The bytes of `term` when it is a **byte-aligned binary** (`is_binary/1`),
/// `None` otherwise (a non-byte-aligned bitstring is not a binary in Erlang, and
/// `cp/1` has no clause for one).
pub(super) fn binary_opt(term: Eterm) -> Option<Vec<u8>> {
    if !bits::is_binary(term) {
        return None;
    }
    // `is_binary/1` is exactly `bitstring_bytes`'s precondition.
    bits::bytes(term).ok()
}

/// The atom `error` — the tag of the `{error, Rest}` shape `cp/1` builds.
pub(super) fn error_atom(ctx: &BifContext<'_>) -> Result<Eterm> {
    ctx.atom("error")
}

/// Whether `term` is the `{error, Rest}` shape produced by `cp/1`/`cpl/2`.
///
/// Detected structurally (a 2-tuple whose first element is the atom `error`)
/// because only `cp`/`cpl` produce it internally.
fn is_error_tuple(ctx: &BifContext<'_>, term: Eterm) -> bool {
    access::is_tuple(term)
        && access::tuple_size(term) == 2
        && is_atom_named(ctx, access::tuple_element(term, 0), "error")
}

fn is_atom_named(ctx: &BifContext<'_>, term: Eterm, name: &str) -> bool {
    access::is_atom(term) && ctx.atoms().name(atom_val(term)) == Some(name)
}

/// The `Rest` of an `{error, Rest}` shape.
fn error_payload(term: Eterm) -> Eterm {
    access::tuple_element(term, 1)
}

// ---------------------------------------------------------------------------
// Property queries over terms (the oracle's catch-all clauses answer `false`)
// ---------------------------------------------------------------------------

fn is_control_term(term: Eterm) -> bool {
    cp_value(term).is_some_and(is_control)
}

fn is_extend_term(term: Eterm) -> Extend {
    match cp_value(term) {
        Some(cp) => props::is_extend(cp),
        None => Extend::No,
    }
}

fn is_ext_pict_term(term: Eterm) -> bool {
    cp_value(term).is_some_and(is_ext_pict)
}

fn is_indic_consonant_term(term: Eterm) -> bool {
    cp_value(term).is_some_and(is_indic_consonant)
}

fn is_indic_linker_term(term: Eterm) -> bool {
    cp_value(term).is_some_and(is_indic_linker)
}

fn in_prepend_term(term: Eterm) -> bool {
    cp_value(term).is_some_and(in_prepend)
}

fn in_l_term(term: Eterm) -> bool {
    cp_value(term).is_some_and(in_l)
}

fn in_v_term(term: Eterm) -> bool {
    cp_value(term).is_some_and(in_v)
}

fn in_t_term(term: Eterm) -> bool {
    cp_value(term).is_some_and(in_t)
}

fn in_lv_term(term: Eterm) -> bool {
    cp_value(term).is_some_and(in_lv)
}

fn in_lvt_term(term: Eterm) -> bool {
    cp_value(term).is_some_and(in_lvt)
}

fn in_regional_term(term: Eterm) -> bool {
    cp_value(term).is_some_and(in_regional)
}

// ---------------------------------------------------------------------------
// List building blocks
// ---------------------------------------------------------------------------

/// `lists:reverse(Acc)` — `acc` is held most-recent-first.
fn reversed_list(ctx: &mut BifContext<'_>, acc: &[Eterm]) -> Result<Eterm> {
    let mut out = NIL;
    for &item in acc {
        out = ctx.cons(item, out)?;
    }
    Ok(out)
}

/// `add_acc(Acc, R)` — `[CP|R]` for a one-element accumulator, else
/// `[lists:reverse(Acc)|R]`.
fn add_acc(ctx: &mut BifContext<'_>, acc: &[Eterm], r: Eterm) -> Result<Eterm> {
    if acc.len() == 1 {
        return ctx.cons(acc[0], r);
    }
    let reversed = reversed_list(ctx, acc)?;
    ctx.cons(reversed, r)
}

/// `Lhs ++ Rhs`.
///
/// `erlang:'++'/2` copies the **left** spine and uses the right operand verbatim
/// with no check; a left operand that is not a proper list (an improper tail, a
/// binary, any other term) is `error:badarg`. `gc_ext_pict/3` and
/// `gc_ext_pict_zwj/3` reach that failure when the run's saved prefix is a binary,
/// and the oracle raises the bare `badarg` — so does this port.
fn append_2(ctx: &mut BifContext<'_>, lhs: Eterm, rhs: Eterm) -> Result<Eterm> {
    let mut items: Vec<Eterm> = Vec::new();
    let mut cursor = lhs;
    while let Some((head, tail)) = head_tail(cursor) {
        items.push(head);
        cursor = tail;
    }
    if !access::is_nil(cursor) {
        return Err(badarg());
    }
    let mut out = rhs;
    for &item in items.iter().rev() {
        out = ctx.cons(item, out)?;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// gc/1
// ---------------------------------------------------------------------------

/// The `[[$\r,$\n]|R1]` shared by `gc/1` and `gc_1/2`: when `cp(r0)` starts with
/// `$\n`, emit the CRLF pair, otherwise `fallback`.
fn crlf_pair(ctx: &mut BifContext<'_>, r0: Eterm, fallback: Eterm) -> Result<Eterm> {
    let rest = cp(ctx, r0)?;
    if let Some((cp2, r1)) = head_tail(rest) {
        if cp_value(cp2) == Some(10) {
            let cr = ctx.int(13)?;
            let lf = ctx.int(10)?;
            let pair = ctx.list(&[cr, lf])?;
            return ctx.cons(pair, r1);
        }
    }
    Ok(fallback)
}

/// `gc(Str)` — one extended grapheme cluster off the front of a chardata.
///
/// Returns `[]` for exhausted input, `[Cluster | Rest]` for one cluster
/// (`Rest` keeps the input's tail representation: a binary tail stays a binary,
/// an improper list tail stays improper), or `{error, Rest}` for a binary whose
/// first byte does not start a valid UTF-8 sequence.
pub(crate) fn gc(ctx: &mut BifContext<'_>, str: Eterm) -> Result<Eterm> {
    // gc([]=R) -> R
    if access::is_nil(str) {
        return Ok(str);
    }
    if let Some((cp1, t1)) = head_tail(str) {
        // gc([CP]=R) when ?IS_CP(CP) -> R
        if access::is_nil(t1) && is_cp(cp1) {
            return Ok(str);
        }
        // gc([$\r=CP|R0] = Str) -> ... keep the tail binary
        if cp_value(cp1) == Some(13) {
            return crlf_pair(ctx, t1, str);
        }
        // gc([CP1|T1]=T) when ?IS_LATIN1(CP1)
        if latin1_value(cp1).is_some() {
            // Ascii fast path: `[CP2|_]` with a Latin-1 `CP2` keeps the tail as is.
            if let Some((cp2, _)) = head_tail(t1) {
                if latin1_value(cp2).is_some() {
                    return Ok(str);
                }
            }
            return match cp_no_bin(ctx, t1)? {
                NoBin::Term(t3) => {
                    // Second Ascii fast path: `[CP2|_] = T3` with a Latin-1 `CP2`.
                    if let Some((cp2, _)) = head_tail(t3) {
                        if latin1_value(cp2).is_some() {
                            return ctx.cons(cp1, t3);
                        }
                    }
                    gc_1(ctx, t3, cp1)
                }
                NoBin::BinaryFound => gc_1(ctx, t1, cp1),
            };
        }
        // gc([CP|T]) when ?IS_CP(CP) -> gc_1(T,CP)
        if is_cp(cp1) {
            return gc_1(ctx, t1, cp1);
        }
    }
    // gc(<<>>) -> []  /  gc(<<CP1/utf8, Rest/binary>>) -> ...
    if let Some(bytes) = binary_opt(str) {
        if bytes.is_empty() {
            return Ok(NIL);
        }
        if let Decode::Ok(value, len) = decode_one(&bytes) {
            let cp1 = ctx.int(i64::from(value))?;
            let rest_bytes = &bytes[len..];
            if value < 256 && value != 13 {
                if let Decode::Ok(cp2, _) = decode_one_prefix(rest_bytes) {
                    if cp2 < 256 {
                        // Ascii fast path: `[CP1|Rest]`.
                        let tail = bits::make_binary(ctx.heap(), rest_bytes)?;
                        return ctx.cons(cp1, tail);
                    }
                }
            }
            let rest = bits::make_binary(ctx.heap(), rest_bytes)?;
            return gc_1(ctx, rest, cp1);
        }
    }
    // gc(Str) -> case cp(Str) of {error,_}=Error -> Error; CPs -> gc(CPs) end
    let cps = cp(ctx, str)?;
    if is_error_tuple(ctx, cps) {
        return Ok(cps);
    }
    gc(ctx, cps)
}

/// `decode_one/1` for a possibly-empty slice (`<<CP2/utf8, _/binary>>` requires a
/// byte).
fn decode_one_prefix(bytes: &[u8]) -> Decode {
    if bytes.is_empty() {
        Decode::Incomplete(1)
    } else {
        decode_one(bytes)
    }
}

// ---------------------------------------------------------------------------
// gc_1/2 and the clusters it dispatches to
// ---------------------------------------------------------------------------

/// `gc_1(R0, CP)` — continue a cluster started by the code point `CP`.
///
/// The clause order is the generated oracle's and is load-bearing; see the module
/// docs.
fn gc_1(ctx: &mut BifContext<'_>, r0: Eterm, cp1: Eterm) -> Result<Eterm> {
    // gc_1(R0, $\r) -> case cp(R0) of [$\n|R1] -> [[$\r,$\n]|R1]; _ -> [$\r|R0] end
    if cp_value(cp1) == Some(13) {
        let fallback = ctx.cons(cp1, r0)?;
        return crlf_pair(ctx, r0, fallback);
    }
    if let Some(value) = cp_value(cp1) {
        // %% Handle control — `cr ∪ lf ∪ control`, minus the `$\r` clause above.
        if is_control(value) {
            return ctx.cons(cp1, r0);
        }
        // The two `Extended_Pictographic` code points below 256.
        if value == 169 || value == 174 {
            return gc_ext_pict(ctx, r0, vec![cp1]);
        }
        // Latin-1 byte-tail fast path.
        if value <= 0xFF {
            if let Some((cp2, _)) = head_tail(r0) {
                if latin1_value(cp2).is_some() {
                    return ctx.cons(cp1, r0);
                }
            }
            let rest = cp(ctx, r0)?;
            return gc_extend(ctx, rest, r0, cp1);
        }
    }
    // gc_1(_, CP) when not ?IS_CP(CP) -> error({badarg, CP})
    let Some(value) = cp_value(cp1) else {
        return Err(structured_error(ctx, "badarg", &[cp1])?);
    };
    // %% Handle Prepend
    if in_prepend(value) {
        return gc_prepend(ctx, r0, cp1);
    }
    // %% Handle Hangul L / V / T
    if in_l(value) {
        return gc_h_l(ctx, r0, vec![cp1]);
    }
    if in_v(value) {
        return gc_h_v(ctx, r0, vec![cp1]);
    }
    if in_t(value) {
        return gc_h_t(ctx, r0, vec![cp1]);
    }
    // %% Handle Hangul LV and LVT special, since they are large
    if (44000..=56000).contains(&value) {
        let head = ctx.cons(cp1, r0)?;
        return gc_h_lv_lvt(ctx, head, head, Vec::new());
    }
    // %% Handle Regional
    if in_regional(value) {
        return gc_regional(ctx, r0, cp1);
    }
    // %% default clauses
    if is_ext_pict(value) {
        return gc_ext_pict(ctx, r0, vec![cp1]);
    }
    if is_indic_consonant(value) {
        let rest = cp(ctx, r0)?;
        return gc_indic(ctx, rest, r0, false, vec![cp1]);
    }
    let rest = cp(ctx, r0)?;
    gc_extend(ctx, rest, r0, cp1)
}

/// `gc_prepend(R00, CP0)`.
fn gc_prepend(ctx: &mut BifContext<'_>, r00: Eterm, cp0: Eterm) -> Result<Eterm> {
    let rest = cp(ctx, r00)?;
    if let Some((cp1, r0)) = head_tail(rest) {
        if is_control_term(cp1) {
            return ctx.cons(cp0, r00);
        }
        let cluster = gc_1(ctx, r0, cp1)?;
        let Some((head, tail)) = head_tail(cluster) else {
            return Err(structured_error(ctx, "case_clause", &[cluster])?);
        };
        let inner = if access::is_integer(head) {
            ctx.list(&[cp0, head])?
        } else {
            ctx.cons(cp0, head)?
        };
        return ctx.cons(inner, tail);
    }
    if access::is_nil(rest) {
        return ctx.cons(cp0, NIL);
    }
    if is_error_tuple(ctx, rest) {
        return ctx.cons(cp0, error_payload(rest));
    }
    Err(structured_error(ctx, "case_clause", &[rest])?)
}

/// `gc_extend(T, T0, CP0)` — `T` is `cp(T0)`'s result.
fn gc_extend(
    ctx: &mut BifContext<'_>,
    normalized: Eterm,
    t0: Eterm,
    cp0: Eterm,
) -> Result<Eterm> {
    if let Some((cp1, t)) = head_tail(normalized) {
        if is_extend_term(cp1) == Extend::No {
            // Losing work done on T.
            return ctx.cons(cp0, t0);
        }
        let next = cp(ctx, t)?;
        return gc_extend2(ctx, next, t, vec![cp1, cp0]);
    }
    if access::is_nil(normalized) {
        return ctx.cons(cp0, t0);
    }
    if is_error_tuple(ctx, normalized) {
        return ctx.cons(cp0, error_payload(normalized));
    }
    Err(function_clause())
}

/// `gc_extend2(T, T0, Acc)` — `T` is `cp(T0)`'s result.
fn gc_extend2(
    ctx: &mut BifContext<'_>,
    normalized: Eterm,
    t0: Eterm,
    mut acc: Vec<Eterm>,
) -> Result<Eterm> {
    if let Some((cp1, t)) = head_tail(normalized) {
        if is_extend_term(cp1) == Extend::No {
            let reversed = reversed_list(ctx, &acc)?;
            return ctx.cons(reversed, t0);
        }
        acc.insert(0, cp1);
        let next = cp(ctx, t)?;
        return gc_extend2(ctx, next, t, acc);
    }
    if access::is_nil(normalized) {
        let reversed = reversed_list(ctx, &acc)?;
        return ctx.cons(reversed, t0);
    }
    if is_error_tuple(ctx, normalized) {
        // [lists:reverse(Acc)] ++ [R]
        let reversed = reversed_list(ctx, &acc)?;
        return ctx.list(&[reversed, error_payload(normalized)]);
    }
    Err(function_clause())
}

/// `gc_ext_pict(T, Acc)`.
fn gc_ext_pict(ctx: &mut BifContext<'_>, t: Eterm, acc: Vec<Eterm>) -> Result<Eterm> {
    let normalized = cp(ctx, t)?;
    gc_ext_pict_norm(ctx, normalized, t, acc)
}

/// The `gc_ext_pict/3` clauses; `normalized` is `cp(t0)`'s result and `t0` the
/// term `normalized` was derived from (the roll-back target).
fn gc_ext_pict_norm(
    ctx: &mut BifContext<'_>,
    normalized: Eterm,
    t0: Eterm,
    mut acc: Vec<Eterm>,
) -> Result<Eterm> {
    if let Some((cp1, r1)) = head_tail(normalized) {
        match is_extend_term(cp1) {
            Extend::Zwj => {
                acc.insert(0, cp1);
                let next = cp(ctx, r1)?;
                return gc_ext_pict_zwj(ctx, next, r1, acc);
            }
            Extend::Yes => {
                acc.insert(0, cp1);
                return gc_ext_pict(ctx, r1, acc);
            }
            Extend::No => return add_acc(ctx, &acc, t0),
        }
    }
    if access::is_nil(normalized) {
        return add_acc(ctx, &acc, t0);
    }
    if is_error_tuple(ctx, normalized) {
        // gc_ext_pict([], T, Acc) ++ [R]
        let r = error_payload(normalized);
        let partial = add_acc(ctx, &acc, t0)?;
        let singleton = ctx.list(&[r])?;
        return append_2(ctx, partial, singleton);
    }
    Err(function_clause())
}

/// `gc_ext_pict_zwj(T, T0, Acc)`; `normalized` is `cp(T0)`'s result.
fn gc_ext_pict_zwj(
    ctx: &mut BifContext<'_>,
    normalized: Eterm,
    t0: Eterm,
    mut acc: Vec<Eterm>,
) -> Result<Eterm> {
    if let Some((cp1, r1)) = head_tail(normalized) {
        if is_ext_pict_term(cp1) {
            acc.insert(0, cp1);
            return gc_ext_pict(ctx, r1, acc);
        }
        return add_acc(ctx, &acc, t0);
    }
    if access::is_nil(normalized) {
        return add_acc(ctx, &acc, t0);
    }
    if is_error_tuple(ctx, normalized) {
        // gc_ext_pict_zwj([], T, Acc) ++ [R]
        let r = error_payload(normalized);
        let partial = add_acc(ctx, &acc, t0)?;
        let singleton = ctx.list(&[r])?;
        return append_2(ctx, partial, singleton);
    }
    Err(function_clause())
}

/// The oracle's `gc_h_L(R0, Acc)` clause (Hangul `L` run, then optionally `V`/`T`).
fn gc_h_l(ctx: &mut BifContext<'_>, r0: Eterm, mut acc: Vec<Eterm>) -> Result<Eterm> {
    let normalized = cp(ctx, r0)?;
    if let Some((cp1, r1)) = head_tail(normalized) {
        if in_l_term(cp1) {
            acc.insert(0, cp1);
            return gc_h_l(ctx, r1, acc);
        }
        if in_v_term(cp1) {
            acc.insert(0, cp1);
            return gc_h_v(ctx, r1, acc);
        }
    }
    gc_h_lv_lvt(ctx, normalized, r0, acc)
}

/// The oracle's `gc_h_V(R0, Acc)` clause (Hangul `V` run, then optionally `T`).
fn gc_h_v(ctx: &mut BifContext<'_>, r0: Eterm, mut acc: Vec<Eterm>) -> Result<Eterm> {
    let normalized = cp(ctx, r0)?;
    if let Some((cp1, r1)) = head_tail(normalized) {
        if in_v_term(cp1) {
            acc.insert(0, cp1);
            return gc_h_v(ctx, r1, acc);
        }
        if in_t_term(cp1) {
            acc.insert(0, cp1);
            return gc_h_t(ctx, r1, acc);
        }
    }
    if acc.len() == 1 {
        return gc_extend(ctx, normalized, r0, acc[0]);
    }
    gc_extend2(ctx, normalized, r0, acc)
}

/// The oracle's `gc_h_T(R0, Acc)` clause (Hangul `T` run).
fn gc_h_t(ctx: &mut BifContext<'_>, r0: Eterm, mut acc: Vec<Eterm>) -> Result<Eterm> {
    let normalized = cp(ctx, r0)?;
    if let Some((cp1, r1)) = head_tail(normalized) {
        if in_t_term(cp1) {
            acc.insert(0, cp1);
            return gc_h_t(ctx, r1, acc);
        }
    }
    if acc.len() == 1 {
        return gc_extend(ctx, normalized, r0, acc[0]);
    }
    gc_extend2(ctx, normalized, r0, acc)
}

/// `gc_h_lv_lvt(First, R0, Acc)` — `First` is the (already normalised) head of
/// `R0` for the `[]` accumulator case, and `cp(R0)`'s result otherwise.
fn gc_h_lv_lvt(
    ctx: &mut BifContext<'_>,
    first: Eterm,
    r0: Eterm,
    mut acc: Vec<Eterm>,
) -> Result<Eterm> {
    if let Some((cp1, r1)) = head_tail(first) {
        // gc_h_lv_lvt([CP|_], _R0, _Acc) when not ?IS_CP(CP) -> error(badarg)
        if cp_value(cp1).is_none() {
            return Err(badarg());
        }
        if in_lv_term(cp1) {
            acc.insert(0, cp1);
            return gc_h_v(ctx, r1, acc);
        }
        if in_lvt_term(cp1) {
            acc.insert(0, cp1);
            return gc_h_t(ctx, r1, acc);
        }
        if acc.is_empty() {
            let next = cp(ctx, r1)?;
            return gc_extend(ctx, next, r1, cp1);
        }
    }
    if acc.len() == 1 {
        return gc_extend(ctx, first, r0, acc[0]);
    }
    gc_extend2(ctx, first, r0, acc)
}

/// `gc_regional(R0, CP0)`.
fn gc_regional(ctx: &mut BifContext<'_>, r0: Eterm, cp0: Eterm) -> Result<Eterm> {
    let normalized = cp(ctx, r0)?;
    if let Some((cp1, r1)) = head_tail(normalized) {
        if in_regional_term(cp1) {
            let next = cp(ctx, r1)?;
            return gc_extend2(ctx, next, r1, vec![cp1, cp0]);
        }
    }
    gc_extend(ctx, normalized, r0, cp0)
}

/// `gc_indic(T, R0, FetchedLinker, CPs)` — `T` is `cp(R0)`'s result.
fn gc_indic(
    ctx: &mut BifContext<'_>,
    normalized: Eterm,
    r0: Eterm,
    fetched_linker: bool,
    mut cps: Vec<Eterm>,
) -> Result<Eterm> {
    if let Some((cp1, r1)) = head_tail(normalized) {
        if is_indic_linker_term(cp1) {
            cps.insert(0, cp1);
            let next = cp(ctx, r1)?;
            return gc_indic(ctx, next, r1, true, cps);
        }
        if is_extend_term(cp1) == Extend::No {
            if fetched_linker && is_indic_consonant_term(cp1) {
                cps.insert(0, cp1);
                let next = cp(ctx, r1)?;
                return gc_indic(ctx, next, r1, false, cps);
            }
            return add_acc(ctx, &cps, r0);
        }
        cps.insert(0, cp1);
        let next = cp(ctx, r1)?;
        return gc_indic(ctx, next, r1, fetched_linker, cps);
    }
    if access::is_nil(normalized) {
        return add_acc(ctx, &cps, r0);
    }
    if is_error_tuple(ctx, normalized) {
        // NB: the error payload, not the second argument.
        return add_acc(ctx, &cps, error_payload(normalized));
    }
    Err(function_clause())
}

// The oracle corpus lives in its own file: it is data plus a harness (the verbatim
// `~w` rendering of the system oracle's answers, the term builders it is expressed
// with, and the live-oracle cross-check), not part of the walk. It belongs to this
// module all the same — `use super::*;` reaches the port's private helpers and the
// frozen `pub(crate)` re-export above, which the integration-test crate cannot see.
#[cfg(test)]
mod tests;

