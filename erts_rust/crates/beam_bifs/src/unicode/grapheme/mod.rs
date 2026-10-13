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
        return gc_h_L(ctx, r0, vec![cp1]);
    }
    if in_v(value) {
        return gc_h_V(ctx, r0, vec![cp1]);
    }
    if in_t(value) {
        return gc_h_T(ctx, r0, vec![cp1]);
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

/// `gc_h_L(R0, Acc)`.
fn gc_h_L(ctx: &mut BifContext<'_>, r0: Eterm, mut acc: Vec<Eterm>) -> Result<Eterm> {
    let normalized = cp(ctx, r0)?;
    if let Some((cp1, r1)) = head_tail(normalized) {
        if in_l_term(cp1) {
            acc.insert(0, cp1);
            return gc_h_L(ctx, r1, acc);
        }
        if in_v_term(cp1) {
            acc.insert(0, cp1);
            return gc_h_V(ctx, r1, acc);
        }
    }
    gc_h_lv_lvt(ctx, normalized, r0, acc)
}

/// `gc_h_V(R0, Acc)`.
fn gc_h_V(ctx: &mut BifContext<'_>, r0: Eterm, mut acc: Vec<Eterm>) -> Result<Eterm> {
    let normalized = cp(ctx, r0)?;
    if let Some((cp1, r1)) = head_tail(normalized) {
        if in_v_term(cp1) {
            acc.insert(0, cp1);
            return gc_h_V(ctx, r1, acc);
        }
        if in_t_term(cp1) {
            acc.insert(0, cp1);
            return gc_h_T(ctx, r1, acc);
        }
    }
    if acc.len() == 1 {
        return gc_extend(ctx, normalized, r0, acc[0]);
    }
    gc_extend2(ctx, normalized, r0, acc)
}

/// `gc_h_T(R0, Acc)`.
fn gc_h_T(ctx: &mut BifContext<'_>, r0: Eterm, mut acc: Vec<Eterm>) -> Result<Eterm> {
    let normalized = cp(ctx, r0)?;
    if let Some((cp1, r1)) = head_tail(normalized) {
        if in_t_term(cp1) {
            acc.insert(0, cp1);
            return gc_h_T(ctx, r1, acc);
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
        let Some(value) = cp_value(cp1) else {
            return Err(badarg());
        };
        if in_lv_term(cp1) {
            acc.insert(0, cp1);
            return gc_h_V(ctx, r1, acc);
        }
        if in_lvt_term(cp1) {
            acc.insert(0, cp1);
            return gc_h_T(ctx, r1, acc);
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

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_codec::term::{make_big, make_binary, make_bitstring};
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::compare::eq;
    use beam_term::AtomTable;

    /// A term in the corpus, either as input or as an expected value.
    #[derive(Clone, Debug)]
    enum Expr {
        Int(i64),
        /// `1 bsl k` — a bignum.
        BigPow2(u32),
        Float(f64),
        Bin(Vec<u8>),
        /// `<<bytes:bits>>`.
        Bits(Vec<u8>, usize),
        Nil,
        List(Vec<Expr>),
        Cons(Box<Expr>, Box<Expr>),
        Tuple(Vec<Expr>),
        /// `{error, X}`.
        ErrorTuple(Box<Expr>),
    }

    /// What the oracle does for a case.
    #[derive(Clone, Debug)]
    enum Outcome {
        Value(Expr),
        /// A bare `error` reason atom (`function_clause`, `badarg`).
        AtomRaise(&'static str),
        /// A structured `{badarg, X}` raise.
        BadArgRaise(Expr),
    }

    struct Case {
        name: &'static str,
        input: Expr,
        next: Outcome,
        len: Outcome,
    }

    fn seeded_atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(["error", "true", "false", "badarg", "function_clause"]);
        atoms
    }

    fn pow2_limbs(k: u32) -> Vec<u64> {
        let mut limbs = vec![0u64; (k / 36) as usize + 1];
        limbs[(k / 36) as usize] = 1u64 << (k % 36);
        limbs
    }

    fn build(ctx: &mut BifContext<'_>, expr: &Expr) -> Eterm {
        match expr {
            Expr::Int(value) => ctx.int(*value).expect("a small int fits"),
            Expr::BigPow2(k) => make_big(ctx.heap(), false, &pow2_limbs(*k)).expect("a bignum"),
            Expr::Float(value) => ctx.make_float(*value).expect("a float"),
            Expr::Bin(bytes) => make_binary(ctx.heap(), bytes).expect("a binary"),
            Expr::Bits(bytes, bits) => {
                make_bitstring(ctx.heap(), bytes, *bits).expect("a bitstring")
            }
            Expr::Nil => NIL,
            Expr::List(items) => {
                let items: Vec<Eterm> = items.iter().map(|item| build(ctx, item)).collect();
                ctx.list(&items).expect("a short list fits")
            }
            Expr::Cons(head, tail) => {
                let head = build(ctx, head);
                let tail = build(ctx, tail);
                ctx.cons(head, tail).expect("a cons cell fits")
            }
            Expr::Tuple(items) => {
                let items: Vec<Eterm> = items.iter().map(|item| build(ctx, item)).collect();
                ctx.tuple(&items).expect("a short tuple fits")
            }
            Expr::ErrorTuple(payload) => {
                let tag = ctx.atom("error").expect("the atom `error` is seeded");
                let payload = build(ctx, payload);
                ctx.tuple(&[tag, payload]).expect("a tuple fits")
            }
        }
    }

    /// The live reason term of a structured raise.
    fn live_reason(error: &BeamError) -> Eterm {
        match error {
            BeamError::Raise(raise) => raise
                .live_reason()
                .and_then(|live| live.downcast_ref::<Eterm>().copied())
                .expect("a structured raise carries its reason term"),
            other => panic!("expected a raise, got {other:?}"),
        }
    }

    fn check(
        ctx: &mut BifContext<'_>,
        atoms: &AtomTable,
        case: &Case,
        what: &str,
        got: Result<Eterm>,
        want: &Outcome,
    ) {
        match (got, want) {
            (Ok(got), Outcome::Value(want)) => {
                let want = build(ctx, want);
                assert!(
                    eq(got, want, atoms),
                    "case `{}` {what}: got a different term",
                    case.name
                );
            }
            (Err(error), Outcome::AtomRaise(atom)) => {
                assert_eq!(
                    error.as_atom(),
                    Some(*atom),
                    "case `{}` {what}: wrong raise",
                    case.name
                );
            }
            (Err(error), Outcome::BadArgRaise(reason)) => {
                let want = build(ctx, reason);
                assert_eq!(error.as_atom(), None, "case `{}` {what}", case.name);
                let got = live_reason(&error);
                assert!(
                    eq(got, want, atoms),
                    "case `{}` {what}: wrong `{{badarg, _}}` payload",
                    case.name
                );
            }
            (Ok(_), want) => panic!("case `{}` {what}: expected {want:?}, got a value", case.name),
            (Err(error), want) => panic!(
                "case `{}` {what}: expected {want:?}, got the raise {error:?}",
                case.name
            ),
        }
    }

    /// `nil` — a helper so the table reads like the oracle's `~w` output.
    fn ints(values: &[i64]) -> Expr {
        Expr::List(values.iter().copied().map(Expr::Int).collect())
    }

    fn bytes(bytes: &[u8]) -> Expr {
        Expr::Bin(bytes.to_vec())
    }

    /// Every corpus case: `(input, next_grapheme, string:length)`.
    ///
    /// Each row is the verbatim `~w` rendering of the system oracle's answer
    /// (OTP 29.1.1 / erts-17.1); a row is a `Value`, a bare `badarg`, or a bare
    /// `function_clause`/`{badarg, X}` raise.
    fn cases() -> Vec<Case> {
        fn case(
            name: &'static str,
            input: Expr,
            next: Outcome,
            len: Outcome,
        ) -> Case {
            Case {
                name,
                input,
                next,
                len,
            }
        }
        fn value(expr: Expr) -> Outcome {
            Outcome::Value(expr)
        }
        fn err(payload: Expr) -> Outcome {
            Outcome::BadArgRaise(payload)
        }
        let fc = Outcome::AtomRaise("function_clause");
        let ba = Outcome::AtomRaise("badarg");
        vec![
            case(
                "combining-mark list",
                ints(&[97, 769, 98]),
                value(Expr::List(vec![ints_raw(&[97, 769]), Expr::Int(98)])),
                value(Expr::Int(2)),
            ),
            case(
                "combining-mark binary",
                bytes(&[97, 204, 129, 98]),
                value(Expr::Cons(
                    Box::new(ints_raw(&[97, 769])),
                    Box::new(bytes(&[98])),
                )),
                value(Expr::Int(2)),
            ),
            case(
                "cjk binary tail",
                bytes(&[230, 151, 165, 226, 130, 172]),
                value(Expr::Cons(
                    Box::new(Expr::Int(26085)),
                    Box::new(bytes(&[226, 130, 172])),
                )),
                value(Expr::Int(2)),
            ),
            case("two latin1", ints(&[255, 254]), value(ints(&[255, 254])), value(Expr::Int(2))),
            case(
                "binary inside a list",
                Expr::List(vec![bytes(b"abc"), Expr::Int(100)]),
                value(Expr::List(vec![
                    Expr::Int(97),
                    bytes(&[98, 99]),
                    Expr::Int(100),
                ])),
                value(Expr::Int(4)),
            ),
            case(
                "nested lists",
                Expr::List(vec![ints(&[97, 98]), ints(&[99, 100])]),
                value(ints(&[97, 98, 99, 100])),
                value(Expr::Int(4)),
            ),
            case(
                "empty binary then a cp",
                Expr::List(vec![bytes(b""), Expr::Int(97)]),
                value(ints(&[97])),
                value(Expr::Int(1)),
            ),
            case(
                "improper cons with a binary tail",
                Expr::Cons(Box::new(Expr::Int(97)), Box::new(bytes(&[98, 99]))),
                value(Expr::Cons(Box::new(Expr::Int(97)), Box::new(bytes(&[98, 99])))),
                value(Expr::Int(3)),
            ),
            case(
                "improper cons with an integer tail",
                Expr::Cons(Box::new(Expr::Int(97)), Box::new(Expr::Int(98))),
                value(Expr::Cons(Box::new(Expr::Int(97)), Box::new(Expr::Int(98)))),
                fc,
            ),
            case(
                "truncated utf-8 tail",
                bytes(&[97, 204, 129, 200]),
                value(Expr::List(vec![ints_raw(&[97, 769]), bytes(&[200])])),
                err(bytes(&[200])),
            ),
            // A stray continuation byte at the head of a binary: `cp/1` has no
            // clause for it, so the whole (unconsumed) binary comes back.
            case(
                "invalid utf-8 head",
                bytes(&[169, 200]),
                value(Expr::ErrorTuple(Box::new(bytes(&[169, 200])))),
                err(bytes(&[169, 200])),
            ),
            case(
                "invalid utf-8 head (cont)",
                bytes(&[169, 199]),
                value(Expr::ErrorTuple(Box::new(bytes(&[169, 199])))),
                err(bytes(&[169, 199])),
            ),
            case(
                "invalid utf-8 head (ascii)",
                bytes(&[169, 97]),
                value(Expr::ErrorTuple(Box::new(bytes(&[169, 97])))),
                err(bytes(&[169, 97])),
            ),
            case(
                "invalid utf-8 head (two lead bytes)",
                bytes(&[200, 201]),
                value(Expr::ErrorTuple(Box::new(bytes(&[200, 201])))),
                err(bytes(&[200, 201])),
            ),
            case(
                "invalid utf-8 head (nul)",
                bytes(&[169, 0]),
                value(Expr::ErrorTuple(Box::new(bytes(&[169, 0])))),
                err(bytes(&[169, 0])),
            ),
            case(
                "invalid utf-8 head (single byte)",
                bytes(&[169]),
                value(Expr::ErrorTuple(Box::new(bytes(&[169])))),
                err(bytes(&[169])),
            ),
            case(
                "invalid utf-8 head (single continuation)",
                bytes(&[200]),
                value(Expr::ErrorTuple(Box::new(bytes(&[200])))),
                err(bytes(&[200])),
            ),
            // 173 (`SOFT HYPHEN`) is a control singleton: it must be caught before
            // the Latin-1 fast path, so it does not absorb the following extend.
            case(
                "soft hyphen is a control",
                ints(&[173, 769, 97]),
                value(ints(&[173, 769, 97])),
                value(Expr::Int(3)),
            ),
            case(
                "prepend then an ascii cp",
                ints(&[1536, 97]),
                value(Expr::List(vec![ints_raw(&[1536, 97])])),
                value(Expr::Int(1)),
            ),
            case(
                "prepend then a combining mark",
                ints(&[1536, 769, 97]),
                value(Expr::List(vec![ints_raw(&[1536, 769]), Expr::Int(97)])),
                value(Expr::Int(2)),
            ),
            case(
                "prepend then a control",
                ints(&[1536, 10, 97]),
                value(ints(&[1536, 10, 97])),
                value(Expr::Int(3)),
            ),
            case(
                "regional indicator pair",
                ints(&[127462, 127463, 127464, 127465, 97]),
                value(Expr::List(vec![
                    ints_raw(&[127462, 127463]),
                    Expr::Int(127464),
                    Expr::Int(127465),
                    Expr::Int(97),
                ])),
                value(Expr::Int(3)),
            ),
            case(
                "hangul L+V then a control",
                ints(&[1100, 1161, 11, 97]),
                value(Expr::List(vec![
                    ints_raw(&[1100, 1161]),
                    Expr::Int(11),
                    Expr::Int(97),
                ])),
                value(Expr::Int(3)),
            ),
            case(
                "hangul L+V+T",
                ints(&[4352, 4449, 4520, 97]),
                value(Expr::List(vec![ints_raw(&[4352, 4449, 4520]), Expr::Int(97)])),
                value(Expr::Int(2)),
            ),
            case(
                "hangul LV+T",
                ints(&[44032, 4520, 97]),
                value(Expr::List(vec![ints_raw(&[44032, 4520]), Expr::Int(97)])),
                value(Expr::Int(2)),
            ),
            case(
                "hangul L+L",
                ints(&[4352, 4352, 97]),
                value(Expr::List(vec![ints_raw(&[4352, 4352]), Expr::Int(97)])),
                value(Expr::Int(2)),
            ),
            case(
                "indic consonant + zwj",
                ints(&[2325, 8205, 2326, 97]),
                value(Expr::List(vec![ints_raw(&[2325, 8205]), Expr::Int(2326), Expr::Int(97)])),
                value(Expr::Int(3)),
            ),
            case(
                "indic consonant + vowel sign",
                ints(&[2325, 2366, 97]),
                value(Expr::List(vec![ints_raw(&[2325, 2366]), Expr::Int(97)])),
                value(Expr::Int(2)),
            ),
            case(
                "indic conjunct cluster",
                ints(&[2325, 2366, 2381, 2326, 97]),
                value(Expr::List(vec![
                    ints_raw(&[2325, 2366, 2381, 2326]),
                    Expr::Int(97),
                ])),
                value(Expr::Int(2)),
            ),
            case(
                "lone indic linker",
                ints(&[2381, 97]),
                value(ints(&[2381, 97])),
                value(Expr::Int(2)),
            ),
            case(
                "emoji zwj sequence",
                ints(&[128104, 8205, 128105, 97]),
                value(Expr::List(vec![
                    ints_raw(&[128104, 8205, 128105]),
                    Expr::Int(97),
                ])),
                value(Expr::Int(2)),
            ),
            case("empty list", Expr::Nil, value(Expr::Nil), value(Expr::Int(0))),
            case("empty binary", bytes(b""), value(Expr::Nil), value(Expr::Int(0))),
            case("single cp", ints(&[400]), value(ints(&[400])), value(Expr::Int(1))),
            case(
                "single cp then ascii",
                ints(&[400, 97]),
                value(ints(&[400, 97])),
                value(Expr::Int(2)),
            ),
            case("out-of-range cp", ints(&[1114112]), fc.clone(), fc.clone()),
            case(
                "negative then ascii",
                Expr::List(vec![Expr::Int(-1), Expr::Int(97)]),
                fc.clone(),
                fc.clone(),
            ),
            case(
                "bignum",
                Expr::List(vec![Expr::BigPow2(70)]),
                fc.clone(),
                fc.clone(),
            ),
            case("float", Expr::Float(1.5), fc.clone(), fc.clone()),
            case(
                "tuple",
                Expr::Tuple(vec![Expr::Int(1), Expr::Int(2)]),
                fc.clone(),
                fc.clone(),
            ),
            case(
                "non-byte-aligned bitstring",
                Expr::Bits(vec![1], 1),
                fc.clone(),
                fc.clone(),
            ),
            case(
                "one nested list",
                Expr::List(vec![ints(&[97, 98])]),
                value(ints(&[97, 98])),
                value(Expr::Int(2)),
            ),
            case(
                "improper tail inside a list",
                Expr::List(vec![Expr::Int(97), Expr::Cons(Box::new(Expr::Int(98)), Box::new(Expr::Int(99)))]),
                value(Expr::Cons(Box::new(Expr::Int(97)), Box::new(Expr::Int(98)))),
                fc.clone(),
            ),
            case(
                "binary then an empty binary",
                Expr::List(vec![bytes(&[97]), bytes(b"")]),
                value(Expr::List(vec![Expr::Int(97), bytes(b""), bytes(b"")])),
                value(Expr::Int(1)),
            ),
            case("nul", ints(&[0]), value(ints(&[0])), value(Expr::Int(1))),
            case(
                "crlf then ascii",
                ints(&[13, 10, 97]),
                value(Expr::List(vec![ints_raw(&[13, 10]), Expr::Int(97)])),
                value(Expr::Int(2)),
            ),
            case(
                "ascii then crlf",
                ints(&[97, 13, 10, 98]),
                value(ints(&[97, 13, 10, 98])),
                value(Expr::Int(3)),
            ),
            case(
                "two combining marks",
                ints(&[776, 769]),
                value(Expr::List(vec![ints_raw(&[776, 769])])),
                value(Expr::Int(1)),
            ),
            case(
                "two prepends",
                ints(&[1536, 1537]),
                value(Expr::List(vec![ints_raw(&[1536, 1537])])),
                value(Expr::Int(1)),
            ),
            case("zwj alone", ints(&[8205]), value(ints(&[8205])), value(Expr::Int(1))),
            case(
                "ascii then crlf (no trailing)",
                ints(&[97, 13, 10]),
                value(ints(&[97, 13, 10])),
                value(Expr::Int(2)),
            ),
            case(
                "three nested lists",
                Expr::List(vec![ints(&[97, 98]), ints(&[99, 100]), ints(&[101, 102])]),
                value(Expr::List(vec![
                    Expr::Int(97),
                    Expr::Int(98),
                    Expr::Int(99),
                    Expr::Int(100),
                    ints_raw(&[101, 102]),
                ])),
                value(Expr::Int(6)),
            ),
            case(
                "surrogate-range integer",
                ints(&[97, 204, 129, 200, 201]),
                value(ints(&[97, 204, 129, 200, 201])),
                value(Expr::Int(5)),
            ),
            case(
                "extended pictographic below 256, binary roll-back",
                Expr::List(vec![Expr::Int(169), bytes(&[200])]),
                value(Expr::List(vec![
                    Expr::Int(169),
                    bytes(&[200]),
                    bytes(&[200]),
                ])),
                err(Expr::List(vec![bytes(&[200]), bytes(&[200])])),
            ),
            case(
                "extended pictographic then a suffix",
                Expr::List(vec![Expr::Int(169), bytes(&[200]), Expr::Int(97)]),
                value(Expr::List(vec![
                    Expr::Int(169),
                    bytes(&[200]),
                    Expr::Int(97),
                    Expr::List(vec![bytes(&[200]), Expr::Int(97)]),
                ])),
                err(Expr::List(vec![
                    bytes(&[200]),
                    Expr::Int(97),
                    Expr::List(vec![bytes(&[200]), Expr::Int(97)]),
                ])),
            ),
            case(
                "extended pictographic 174, binary roll-back",
                Expr::List(vec![Expr::Int(174), bytes(&[200])]),
                value(Expr::List(vec![
                    Expr::Int(174),
                    bytes(&[200]),
                    bytes(&[200]),
                ])),
                err(Expr::List(vec![bytes(&[200]), bytes(&[200])])),
            ),
            // The saved prefix is a binary here, so the `++` in `gc_ext_pict/3`
            // raises the bare `badarg` (never `{badarg, _}`).
            case(
                "extended pictographic with a binary prefix",
                Expr::List(vec![
                    Expr::List(vec![Expr::Int(169)]),
                    bytes(&[200]),
                ]),
                ba.clone(),
                ba.clone(),
            ),
            case(
                "one nested list of one cp",
                Expr::List(vec![ints(&[169, 200])]),
                value(ints(&[169, 200])),
                value(Expr::Int(2)),
            ),
            case(
                "soft hyphen then a cp",
                ints(&[169, 200]),
                value(ints(&[169, 200])),
                value(Expr::Int(2)),
            ),
            case("control 8225", ints(&[8225, 97]), value(ints(&[8225, 97])), value(Expr::Int(2))),
            case("ascii 111", ints(&[111]), value(ints(&[111])), value(Expr::Int(1))),
            case("ascii 128", ints(&[128]), value(ints(&[128])), value(Expr::Int(1))),
            case(
                "ascii 128 twice",
                ints(&[128, 128]),
                value(ints(&[128, 128])),
                value(Expr::Int(2)),
            ),
            case("indic linker+consonant", ints(&[2381, 2326, 97]), value(ints(&[2381, 2326, 97])), value(Expr::Int(3))),
            case(
                "two picTOgraphics",
                ints(&[128104, 128105]),
                value(ints(&[128104, 128105])),
                value(Expr::Int(2)),
            ),
            case("soft hyphen alone (169)", ints(&[169]), value(ints(&[169])), value(Expr::Int(1))),
            case("two combining marks (2)", ints(&[771, 775]), value(Expr::List(vec![ints_raw(&[771, 775])])), value(Expr::Int(1))),
            case("cr alone", ints(&[13]), value(ints(&[13])), value(Expr::Int(1))),
            case("two cr", ints(&[13, 13]), value(ints(&[13, 13])), value(Expr::Int(2))),
            case("lf then cr", ints(&[10, 13]), value(ints(&[10, 13])), value(Expr::Int(2))),
            case("prepend then ascii (2)", ints(&[1757, 97]), value(Expr::List(vec![ints_raw(&[1757, 97])])), value(Expr::Int(1))),
            case(
                "prepend then soft hyphen",
                ints(&[1757, 173, 97]),
                value(ints(&[1757, 173, 97])),
                value(Expr::Int(3)),
            ),
            case("prepend then crlf", ints(&[1536, 13, 10]), value(ints(&[1536, 13, 10])), value(Expr::Int(2))),
            case("ascii prepend ascii", ints(&[97, 1536, 97]), value(ints(&[97, 1536, 97])), value(Expr::Int(2))),
            case("regional alone", ints(&[127462, 97]), value(ints(&[127462, 97])), value(Expr::Int(2))),
            case(
                "three regionals",
                ints(&[127462, 127463, 127463]),
                value(Expr::List(vec![ints_raw(&[127462, 127463]), Expr::Int(127463)])),
                value(Expr::Int(2)),
            ),
            case("LV then L", ints(&[44032, 4352, 97]), value(ints(&[44032, 4352, 97])), value(Expr::Int(3))),
            case(
                "L then LV",
                ints(&[4352, 44032, 97]),
                value(Expr::List(vec![ints_raw(&[4352, 44032]), Expr::Int(97)])),
                value(Expr::Int(2)),
            ),
            case("T then LV", ints(&[4520, 44032, 97]), value(ints(&[4520, 44032, 97])), value(Expr::Int(3))),
            case("max cp", ints(&[1114111]), value(ints(&[1114111])), value(Expr::Int(1))),
            case("max cp then ascii", ints(&[1114111, 97]), value(ints(&[1114111, 97])), value(Expr::Int(2))),
            case(
                "variation selectors",
                ints(&[65024, 65024, 97]),
                value(Expr::List(vec![ints_raw(&[65024, 65024]), Expr::Int(97)])),
                value(Expr::Int(2)),
            ),
            case("control 8400", ints(&[8400, 97]), value(ints(&[8400, 97])), value(Expr::Int(2))),
            case("control 8288", ints(&[8288, 97]), value(ints(&[8288, 97])), value(Expr::Int(2))),
            case("169 then ascii", ints(&[169, 97]), value(ints(&[169, 97])), value(Expr::Int(2))),
            case("174 then ascii", ints(&[174, 97]), value(ints(&[174, 97])), value(Expr::Int(2))),
            case(
                "169 and 174 then ascii",
                ints(&[169, 174, 97]),
                value(ints(&[169, 174, 97])),
                value(Expr::Int(3)),
            ),
            case(
                "hangul run",
                ints(&[1100, 1161, 1161, 1162, 97]),
                value(Expr::List(vec![
                    ints_raw(&[1100, 1161, 1161]),
                    Expr::Int(1162),
                    Expr::Int(97),
                ])),
                value(Expr::Int(3)),
            ),
        ]
    }

    /// A list built from an owned slice of small integers.
    fn ints_raw(values: &[i64]) -> Expr {
        Expr::List(values.iter().copied().map(Expr::Int).collect())
    }

    #[test]
    fn the_corpus_matches_the_oracle_bit_for_bit() {
        let cases = cases();
        assert!(!cases.is_empty(), "the grapheme corpus must not be empty");
        let mut heap = Heap::new(1 << 22).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        for case in &cases {
            let input = build(&mut ctx, &case.input);
            let next = gc(&mut ctx, input);
            check(&mut ctx, &atoms, case, "next_grapheme", next, &case.next);
            let len = crate::unicode::grapheme::grapheme_count(&mut ctx, input);
            check(&mut ctx, &atoms, case, "string:length", len, &case.len);
        }
    }
}
