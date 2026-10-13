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

//! The **unicode** arm of `~s` under an active `{chars_limit, N}` — the
//! `control_limited($s, …, unicode, …)` clause of `io_lib_format.erl`.
//!
//! ```text
//! control_limited($s, [L0], F, Adj, P, Pad, unicode, _Str, _Ord, CL, _I) ->
//!     L = cdata_to_chars(L0, F, CL),
//!     uniconv(string(L, limit_field(F, CL), Adj, P, Pad, unicode)).
//! ```
//!
//! `uniconv/1` is the **identity** unless the `UNICODE_AS_BINARIES` compile flag is
//! set, so the whole arm is `cdata_to_chars/3` followed by `string/6`.
//!
//! # Why this is not the latin1 arm
//!
//! Both arms share `string/6`, but their element lists are *not* interchangeable:
//!
//! * `cdata_to_chars/3` limits the argument by **`string:next_grapheme/1`**, not by
//!   characters — so the element list is a list of *grapheme clusters*, each an
//!   integer (a singleton cluster) or a **list** of code points (a multi-codepoint
//!   cluster such as `[97,769]`). A binary argument is *decoded* as UTF-8
//!   (`cdata_to_chars/1`'s binary clause) and a binary that is not valid UTF-8
//!   falls back to its raw bytes.
//! * the elision probe counts with **`string:length/1`** (grapheme clusters) while
//!   `string/6` measures with **`io_lib:chars_length/1`** — `try iolist_size(S)
//!   catch _:_ -> string:length(S) end` — and the two **disagree** for a cluster
//!   list (`[97,769]` measures 1 as an iolist element but 1 cluster too) and for a
//!   list whose element is a binary or a code point above 255.
//! * `string/6`'s truncation is `string:slice/3`, whose list clauses splice a
//!   cluster **flat** (`append(GC, Str)`'s `GC ++ Str` clause) and whose ASCII fast
//!   path needs a **two-element** prefix.
//!
//! # Errors
//!
//! `io_lib:format/3` folds **every** raise inside `io_lib_format` into
//! `error:badarg`, so every failure here — an unwalkable argument, a
//! `string:length/1` that raises on a binary element, the `case_clause` of a probe
//! whose cluster count exceeds 4, the missing `string/6` clause for `F < P` — is the
//! plain [`badarg`], never a wrong rendering and never a leaked internal reason.

use beam_common::Result;
use beam_term::{atom_val, is_nil, is_small, signed_value, Eterm};

use crate::access::{car, cdr, is_atom, is_list, is_tuple, tuple_element, tuple_size};
use crate::bits;
use crate::context::BifContext;
use crate::error::badarg;
use crate::io::Spec;
use crate::print::{adjust_chunk, dup_chars, tsub, Chunk};

/// The walked source of a `limit_cdata_to_chars/3` recursion.
///
/// `string:next_grapheme/1` keeps the input's tail representation, so a walk of a
/// **binary** argument continues over a byte slice (`{error, <<C,Cs1/binary>>}` and
/// `[26085 | <<Rest/binary>>]`) while a walk of a list continues over the list.
enum TsSource {
    /// A live Erlang term (a list, or the improper tail of one).
    Term(Eterm),
    /// Raw bytes — the tail of a binary whose head cluster was consumed.
    Bytes(Vec<u8>),
}

/// `io_lib_format:control_limited($s, …, unicode, …)` — `~ts` of a **list or
/// binary** under the per-directive budget `chars_limit`.
///
/// The single entry point `crate::io::render_limited_s` calls for the unicode arm;
/// the result is the directive's one sub-list element of the output.
pub(crate) fn render_limited_ts(
    ctx: &mut BifContext<'_>,
    spec: &Spec,
    term: Eterm,
    chars_limit: i64,
) -> Result<Chunk> {
    let items = cdata_to_chars_limited(ctx, term, spec.width, chars_limit)?;
    ts_string(
        ctx,
        items,
        crate::io::limit_field(spec.width, chars_limit),
        spec.left,
        spec.precision,
        spec.pad,
    )
}

/// `io_lib_format:cdata_to_chars/3` — the element list of the `~ts` argument, its
/// tail limited by `CharsLimit - 3` characters unless the budget already covers the
/// whole field.
fn cdata_to_chars_limited(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    width: Option<usize>,
    chars_limit: i64,
) -> Result<Vec<Chunk>> {
    // `cdata_to_chars(Cs, F, CL) when CL < 0; CL >= F` — note that a *missing*
    // width is the atom `none`, which no integer is `>=`.
    if chars_limit < 0 || width.is_some_and(|f| chars_limit >= f as i64) {
        return cdata_to_chars_plain(term);
    }
    limit_cdata(ctx, TsSource::Term(term), tsub(chars_limit, 3), false)
}

/// `io_lib_format:cdata_to_chars/1` — the *unlimited* element list, whose nesting
/// the [`crate::io::cdata_chunk`] walk already reproduces (a binary is decoded as
/// UTF-8, a non-integer element recurses into a sub-list).
fn cdata_to_chars_plain(term: Eterm) -> Result<Vec<Chunk>> {
    match crate::io::cdata_chunk(term)? {
        Chunk::List(items) => Ok(items),
        // `cdata_chunk` always answers a list; a non-list would be a bug, and a
        // wrong element list must never be produced.
        _ => Err(badarg()),
    }
}

/// `io_lib_format:limit_cdata_to_chars/3` — one grapheme cluster per recursion,
/// with the `"..."` elision probe once the budget is spent.
///
/// `final` is the probe mode (`limit_cdata_to_chars(Cs, 0, final) -> []`); `normal`
/// re-probes with a budget of 4 and decides from `string:length/1` of the probe: a
/// count below 4 keeps the probe verbatim, exactly 4 elides to `"..."`, and any
/// other count is the oracle's `case_clause` (an observable `badarg`).
///
/// The oracle's tail recursion is a loop here, so a large `{chars_limit, N}` cannot
/// exhaust the native stack.
fn limit_cdata(
    ctx: &mut BifContext<'_>,
    mut src: TsSource,
    limit: i64,
    final_mode: bool,
) -> Result<Vec<Chunk>> {
    let mut out: Vec<Chunk> = Vec::new();
    let mut remaining = limit;
    loop {
        if remaining <= 0 {
            if final_mode {
                return Ok(out);
            }
            let probe = limit_cdata(ctx, src, 4, true)?;
            let count = items_string_length(ctx, &probe)?;
            return match count {
                4 => {
                    out.extend([Chunk::Char('.'), Chunk::Char('.'), Chunk::Char('.')]);
                    Ok(out)
                }
                n if n < 4 => {
                    out.extend(probe);
                    Ok(out)
                }
                // The oracle's `case string:length(L) of 1..3 / 4` has no clause
                // for a longer probe.
                _ => Err(badarg()),
            };
        }
        match step(ctx, &src)? {
            None => return Ok(out),
            Some((item, next)) => {
                out.push(item);
                src = next;
                remaining -= 1;
            }
        }
    }
}

/// One `string:next_grapheme/1` step of [`limit_cdata`] — the oracle's four
/// `limit_cdata_to_chars/3` dispatch clauses.
fn step(ctx: &mut BifContext<'_>, src: &TsSource) -> Result<Option<(Chunk, TsSource)>> {
    let term = materialize(ctx, src)?;
    let res = crate::unicode::grapheme::next_grapheme(ctx, term).map_err(|_| badarg())?;
    if is_nil(res) {
        // `[]` — exhausted.
        return Ok(None);
    }
    if is_list(res) {
        // `[GC|Cs1]` — one cluster, and the tail keeps the input's representation.
        return Ok(Some((item_of_term(car(res))?, source_of_term(cdr(res))?)));
    }
    // `{error, Err}` — `cp/1` could not decode the front of a binary.
    if let Some(payload) = error_payload(ctx, res) {
        if bits::is_binary(payload) {
            // `{error, <<C,Cs1/binary>>}` — emit the **byte** and continue on the
            // byte slice. An empty payload (unreachable: `next_grapheme/1` answers
            // `[]` for an empty binary) contributes nothing.
            let bytes = bits::bytes(payload)?;
            return match bytes.split_first() {
                None => Ok(None),
                Some((&first, rest)) => Ok(Some((
                    Chunk::Char(first as char),
                    TsSource::Bytes(rest.to_vec()),
                ))),
            };
        }
        if is_list(payload) {
            // `{error, [C|Cs1]}` — emit the head verbatim (a nested list, a binary
            // or an integer) and continue on the tail.
            return Ok(Some((item_of_term(car(payload))?, TsSource::Term(cdr(payload)))));
        }
        return Err(badarg());
    }
    Err(badarg())
}

/// The `Rest` of the `{error, Rest}` shape [`crate::unicode::grapheme::next_grapheme`]
/// builds, `None` when `term` is not that shape.
fn error_payload(ctx: &BifContext<'_>, term: Eterm) -> Option<Eterm> {
    if is_tuple(term) && tuple_size(term) == 2 {
        let tag = tuple_element(term, 0);
        if is_atom(tag) && ctx.atoms().name(atom_val(tag)) == Some("error") {
            return Some(tuple_element(term, 1));
        }
    }
    None
}

/// A [`TsSource`] as a live term (a binary payload is materialized).
fn materialize(ctx: &mut BifContext<'_>, src: &TsSource) -> Result<Eterm> {
    match src {
        TsSource::Term(term) => Ok(*term),
        TsSource::Bytes(bytes) => bits::make_binary(ctx.heap(), bytes),
    }
}

/// The continuation of a walk: a byte-aligned binary tail becomes a
/// [`TsSource::Bytes`] (so the next step continues over its bytes), anything else
/// stays a term.
fn source_of_term(term: Eterm) -> Result<TsSource> {
    if bits::is_binary(term) {
        Ok(TsSource::Bytes(bits::bytes(term)?.to_vec()))
    } else {
        Ok(TsSource::Term(term))
    }
}

/// One element of the walked list.
///
/// * a non-negative integer that is a Unicode scalar becomes [`Chunk::Char`], any
///   other non-negative integer (a surrogate, past `16#10FFFF`, a bignum) is kept
///   verbatim as [`Chunk::Int`] — the `cdata_integer_chunk/1` rule both arms share;
/// * a byte-aligned binary is a **binary element** ([`Chunk::Bin`]): the `~ts`
///   element list is the one place `io_lib:format` returns a real binary inside its
///   deep list (`[[<<200,201>>],97]` walks to `<<200,201>>` verbatim);
/// * a cons is a sub-list element — a grapheme cluster (`[97,769]`, `[13,10]`) or a
///   nested chardata element;
/// * `[]` is the empty sub-list.
///
/// Anything else (a float, an atom, a tuple, a negative integer, a non-byte-aligned
/// bitstring) has no representation in the element list, so it is the oracle's
/// `function_clause` — the observable `badarg` — never a substituted term.
fn item_of_term(term: Eterm) -> Result<Chunk> {
    if is_nil(term) {
        return Ok(Chunk::List(Vec::new()));
    }
    if let Some(chunk) = crate::io::cdata_integer_chunk(term) {
        return Ok(chunk);
    }
    if bits::is_bitstring(term) {
        if !bits::is_binary(term) {
            return Err(badarg());
        }
        return Ok(Chunk::Bin(bits::bytes(term)?.to_vec()));
    }
    if is_list(term) {
        let mut items = Vec::new();
        let mut cursor = term;
        loop {
            if is_nil(cursor) {
                return Ok(Chunk::List(items));
            }
            if !is_list(cursor) {
                return Err(badarg());
            }
            items.push(item_of_term(car(cursor))?);
            cursor = cdr(cursor);
        }
    }
    Err(badarg())
}

/// `io_lib:chars_length(S)` — `try iolist_size(S) catch _:_ -> string:length(S) end`.
///
/// The **first** branch wins whenever the element list measures as an iolist (every
/// element a byte-sized integer or a binary, recursively), so a cluster list of
/// code points above 255 (`[97,769]`) falls through to the grapheme count — the
/// distinction that decides how far `string/6` pads.
fn chars_length(ctx: &mut BifContext<'_>, items: &[Chunk]) -> Result<i64> {
    match iolist_size(items) {
        Some(size) => Ok(size as i64),
        None => items_string_length(ctx, items),
    }
}

/// The `iolist_size/1` branch: `None` when some element is not a valid iolist leaf
/// (an integer outside `0..=255`), which is exactly the oracle's raise.
fn iolist_size(items: &[Chunk]) -> Option<usize> {
    let mut total = 0usize;
    for item in items {
        total += iolist_size_item(item)?;
    }
    Some(total)
}

fn iolist_size_item(item: &Chunk) -> Option<usize> {
    match item {
        Chunk::Char(c) => {
            let value = *c as u32;
            (value <= 0xFF).then_some(1)
        }
        Chunk::Int(term) => {
            if is_small(*term) && (0..=255).contains(&signed_value(*term)) {
                Some(1)
            } else {
                None
            }
        }
        Chunk::Bin(bytes) => Some(bytes.len()),
        Chunk::List(inner) => iolist_size(inner),
    }
}

/// `string:length(List)` of an element list — the fallback of
/// [`chars_length`] and the elision probe's counter.
///
/// A binary element makes the oracle's `string:length/1` raise (`{badarg, …}`),
/// which [`crate::unicode::grapheme::grapheme_count`] reproduces; the raise surfaces
/// here as the plain `badarg`.
fn items_string_length(ctx: &mut BifContext<'_>, items: &[Chunk]) -> Result<i64> {
    let term = crate::io::chunk_to_term(ctx, &Chunk::List(items.to_vec()))?;
    let count = crate::unicode::grapheme::grapheme_count(ctx, term).map_err(|_| badarg())?;
    if is_small(count) {
        Ok(signed_value(count))
    } else {
        Err(badarg())
    }
}

/// `io_lib_format:string/6` over the element list.
///
/// `width` is already `limit_field(F, CL)`. The `(Some(f), Some(p))` clause with
/// `f < p` has **no** definition in the oracle (`error:function_clause`), so it is
/// the plain `badarg`.
fn ts_string(
    ctx: &mut BifContext<'_>,
    items: Vec<Chunk>,
    width: Option<usize>,
    left: bool,
    precision: Option<usize>,
    pad: u8,
) -> Result<Chunk> {
    match (width, precision) {
        (None, None) => Ok(Chunk::List(items)),
        (Some(f), None) => string_field_items(ctx, items, f, left, pad),
        (None, Some(p)) => string_field_items(ctx, items, p, true, pad),
        (Some(f), Some(p)) if f >= p => {
            let n = chars_length(ctx, &items)? as usize;
            if f > p {
                if n > p {
                    let trunc = flat_trunc(ctx, &items, p)?;
                    Ok(adjust_chunk(
                        Chunk::List(trunc),
                        dup_chars(pad as char, f - p),
                        left,
                    ))
                } else if n < p {
                    // `[S | chars(Pad, P-N)]` — `S` as **one** element.
                    let mut body = vec![Chunk::List(items)];
                    body.extend(items_of(dup_chars(pad as char, p - n)));
                    Ok(adjust_chunk(
                        Chunk::List(body),
                        dup_chars(pad as char, f - p),
                        left,
                    ))
                } else {
                    Ok(adjust_chunk(
                        Chunk::List(items),
                        dup_chars(pad as char, f - p),
                        left,
                    ))
                }
            } else {
                string_field_items(ctx, items, f, left, pad)
            }
        }
        _ => Err(badarg()),
    }
}

/// `io_lib_format:string_field/6` over the element list.
fn string_field_items(
    ctx: &mut BifContext<'_>,
    items: Vec<Chunk>,
    f: usize,
    left: bool,
    pad: u8,
) -> Result<Chunk> {
    let n = chars_length(ctx, &items)? as usize;
    if n > f {
        Ok(Chunk::List(flat_trunc(ctx, &items, f)?))
    } else if n < f {
        Ok(adjust_chunk(
            Chunk::List(items),
            dup_chars(pad as char, f - n),
            left,
        ))
    } else {
        Ok(Chunk::List(items))
    }
}

/// `io_lib_format:flat_trunc(Str, N, unicode)` — `string:slice(Str, 0, N)`.
///
/// The argument is always the *element list* (`cdata_to_chars/3` has already
/// decoded a binary argument), so only `string.erl`'s list clause can fire.
fn flat_trunc(ctx: &mut BifContext<'_>, items: &[Chunk], n: usize) -> Result<Vec<Chunk>> {
    let cd = crate::io::chunk_to_term(ctx, &Chunk::List(items.to_vec()))?;
    slice_list(ctx, cd, n)
}

/// `string:slice_list/2` — `slice_trail/2`'s list clause.
///
/// ```text
/// slice_list([CP1|[CP2|_]=Cont], N) when ?ASCII_LIST(CP1,CP2), N > 0 ->
///     [CP1|slice_list(Cont, N-1)];
/// slice_list(CD, N) when N > 0 ->
///     case unicode_util:gc(CD) of
///         [GC|Cont] -> append(GC, slice_list(Cont, N-1));
///         [] -> [];
///         {error, Err} -> error({badarg, Err})
///     end;
/// slice_list(_, 0) -> [].
/// ```
///
/// `append(GC, Str)` **splices** a cluster list (`GC ++ Str`) and conses a bare
/// integer — so a multi-code-point cluster is flattened into the truncated result
/// (`string:slice([[97,769],[98]], 0, 2)` = `[97,769,98]`).
fn slice_list(ctx: &mut BifContext<'_>, cd: Eterm, n: usize) -> Result<Vec<Chunk>> {
    if n == 0 {
        return Ok(Vec::new());
    }
    // The ASCII fast path needs a **two-element** prefix whose first element is
    // Latin-1 and not `\r`.
    if is_list(cd) {
        if let Some(first) = ascii_head(car(cd)) {
            let cont = cdr(cd);
            if is_list(cont) && ascii_head(car(cont)).is_some() {
                let mut out = vec![Chunk::Char(first)];
                out.extend(slice_list(ctx, cont, n - 1)?);
                return Ok(out);
            }
        }
    }
    let res = crate::unicode::grapheme::next_grapheme(ctx, cd).map_err(|_| badarg())?;
    if is_nil(res) {
        return Ok(Vec::new());
    }
    if is_list(res) {
        let mut out = items_of(item_of_term(car(res))?);
        out.extend(slice_list(ctx, cdr(res), n - 1)?);
        return Ok(out);
    }
    Err(badarg())
}

/// `?IS_LATIN1(CP)` and `CP =/= $\r` — the oracle's ASCII fast-path guard.
fn ascii_head(term: Eterm) -> Option<char> {
    if !is_small(term) {
        return None;
    }
    let value = signed_value(term);
    if (0..=255).contains(&value) && value != 13 {
        char::from_u32(value as u32)
    } else {
        None
    }
}

/// A chunk as the element vector the oracle's `[S | Pad]`/`[Pad | S]` splices:
/// a list keeps its elements, a single element is wrapped.
fn items_of(chunk: Chunk) -> Vec<Chunk> {
    match chunk {
        Chunk::List(items) => items,
        single => vec![single],
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::AtomTable;

    /// The `~ts` subjects the oracle matrix uses (name, a builder over `ctx`).
    ///
    /// The builders are restricted to what `BifContext` can build on its own
    /// (integers, lists, conses and atoms); the binaries are built through
    /// `beam_codec::term::make_binary`, the sibling safe constructor.
    fn subject(ctx: &mut BifContext<'_>, name: &str) -> Eterm {
        let int = |ctx: &mut BifContext<'_>, v: i64| ctx.int(v).expect("a small int fits");
        let bin = |ctx: &mut BifContext<'_>, bytes: &[u8]| {
            beam_codec::term::make_binary(ctx.heap(), bytes).expect("the binary fits")
        };
        match name {
            "U" => {
                let items: Vec<Eterm> = [26085, 26412, 35486, 26085, 26412, 35486]
                    .iter()
                    .map(|&v| int(ctx, v))
                    .collect();
                ctx.list(&items).expect("the list fits")
            }
            "A" => {
                let items: Vec<Eterm> = [97, 769, 98, 99].iter().map(|&v| int(ctx, v)).collect();
                ctx.list(&items).expect("the list fits")
            }
            "KOU2" => bin(ctx, &[230, 151, 165, 230, 151, 165]),
            "KOU" => bin(ctx, &[230, 151, 165]),
            "CRLF" => {
                let items: Vec<Eterm> = [13, 10, 97].iter().map(|&v| int(ctx, v)).collect();
                ctx.list(&items).expect("the list fits")
            }
            "NESTBAD" => {
                let inner = bin(ctx, &[200, 201]);
                let sub = ctx.list(&[inner]).expect("the list fits");
                let items = vec![sub, int(ctx, 97)];
                ctx.list(&items).expect("the list fits")
            }
            "NESTBAD2" => {
                let inner = bin(ctx, &[200, 201]);
                let sub = ctx.list(&[inner]).expect("the list fits");
                ctx.list(&[sub]).expect("the list fits")
            }
            "FLATBAD" => {
                let items = vec![bin(ctx, &[200, 201]), int(ctx, 97)];
                ctx.list(&items).expect("the list fits")
            }
            "FLATBAD2" => {
                let b = bin(ctx, &[200, 201]);
                ctx.list(&[b]).expect("the list fits")
            }
            "EMPTY" => ctx.list(&[]).expect("the empty list fits"),
            "EMPTYBIN" => bin(ctx, &[]),
            "IMP" => {
                let head = int(ctx, 97);
                let tail = int(ctx, 98);
                ctx.cons(head, tail).expect("the cons fits")
            }
            "INT" => int(ctx, 97),
            "NEG" => {
                let items: Vec<Eterm> = [97, -1, 98].iter().map(|&v| int(ctx, v)).collect();
                ctx.list(&items).expect("the list fits")
            }
            "FLOAT" => {
                let f = ctx.make_float(1.5).expect("the float fits");
                ctx.list(&[f]).expect("the list fits")
            }
            "ATOM" => ctx.atom("abc").expect("the atom is seeded"),
            "BAD" => bin(ctx, &[200, 201]),
            other => panic!("unknown subject {other}"),
        }
    }

    fn seeded() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "chars_limit",
            "error",
            "badarg",
            "function_clause",
            "case_clause",
            "abc",
            "true",
            "false",
        ]);
        atoms
    }

    /// Runs `io_lib:format(Fmt, [Subject], [{chars_limit, CL}])`.
    fn format_ts(
        ctx: &mut BifContext<'_>,
        fmt: &str,
        subject: Eterm,
        limit: i64,
    ) -> Result<Eterm> {
        let reg = crate::registry::default_registry();
        let bytes: Vec<Eterm> = fmt.bytes().map(|b| ctx.int(i64::from(b)).unwrap()).collect();
        let format = ctx.list(&bytes).expect("a short format list fits");
        let args = ctx.list(&[subject]).expect("a short argument list fits");
        let tag = ctx.atom("chars_limit").expect("the atom is seeded");
        let value = ctx.int(limit).expect("a small int fits");
        let option = ctx.tuple(&[tag, value]).expect("a short tuple fits");
        let options = ctx.list(&[option]).expect("a short options list fits");
        reg.call("io_lib", "format", 3, ctx, &[format, args, options])
    }

    /// The decisive rows of the oracle matrix, checked structurally (a nested
    /// sub-list difference is a failure even when the flattened text agrees).
    ///
    /// Each expectation is `Err(())` for the oracle's `{error,badarg}`, else the
    /// `~w` image of the oracle's deep list (`~ts` of `[[<<200,201>>],97]` at
    /// `CL = -1` is `[[[[200,201]],97]]`, `>= 4` is `[[<<200,201>>,97]]`).
    #[test]
    fn the_decisive_ts_rows_match_the_oracle() {
        let cases: &[(&str, &str, i64, std::result::Result<&str, ()>)] = &[
            ("~ts", "NESTBAD", -1, Ok("[[[[200,201]],97]]")),
            ("~ts", "NESTBAD", 0, Err(())),
            ("~ts", "NESTBAD", 3, Err(())),
            ("~ts", "NESTBAD", 4, Ok("[[<<200,201>>,97]]")),
            ("~ts", "NESTBAD2", 0, Ok("[[200,201]]")),
            ("~ts", "NESTBAD2", -1, Ok("[[[[200,201]]]]")),
            ("~ts", "FLATBAD", -1, Ok("[[[200,201],97]]")),
            ("~ts", "BAD", -1, Ok("[[200,201]]")),
            ("~ts", "U", -1, Ok("[[26085,26412,35486,26085,26412,35486]]")),
            ("~ts", "U", 0, Ok("[[46,46,46]]")),
            ("~ts", "U", 4, Ok("[[26085,46,46,46]]")),
            ("~ts", "U", 5, Ok("[[26085,26412,46,46,46]]")),
            ("~ts", "U", 6, Ok("[[26085,26412,35486,26085,26412,35486]]")),
            ("~ts", "A", -1, Ok("[[97,769,98,99]]")),
            ("~ts", "A", 0, Ok("[[[97,769],98,99]]")),
            ("~ts", "A", 100, Ok("[[[97,769],98,99]]")),
            ("~ts", "KOU", -1, Ok("[[26085]]")),
            ("~ts", "KOU2", -1, Ok("[[26085,26085]]")),
            ("~ts", "EMPTY", -1, Ok("[[]]")),
            ("~ts", "EMPTYBIN", -1, Ok("[[]]")),
            ("~ts", "IMP", -1, Err(())),
            ("~ts", "INT", -1, Err(())),
            ("~ts", "NEG", -1, Err(())),
            ("~ts", "FLOAT", -1, Err(())),
            ("~ts", "ATOM", -1, Ok("[97,98,99]")),
            ("a~tsb", "NESTBAD", -1, Ok("[97,[[[200,201]],97],98]")),
            ("a~tsb", "NESTBAD", 4, Err(())),
            ("a~tsb", "NESTBAD", 6, Ok("[97,[<<200,201>>,97],98]")),
            ("~10ts", "NESTBAD", -1, Ok("[[[32,[32,32,32],32,32,32],[[200,201]],97]]")),
            ("~10ts", "NESTBAD", 4, Ok("[[[32],<<200,201>>,97]]")),
            ("~10ts", "NESTBAD", 10, Ok("[[[32,[32,32,32],32,32,32],[[200,201]],97]]")),
            ("~10ts", "NESTBAD2", 0, Ok("[[[32],200,201]]")),
            ("~-10ts", "NESTBAD", -1, Ok("[[[[[200,201]],97],32,[32,32,32],32,32,32]]")),
            ("~-10ts", "NESTBAD", 4, Ok("[[[<<200,201>>,97],32]]")),
            ("~10.4ts", "NESTBAD", -1, Ok("[[[[32,32,32],32,32,32],[[[200,201]],97],32]]")),
            ("~10.4ts", "NESTBAD", 4, Ok("[[[32],<<200,201>>,97]]")),
            ("~10.4ts", "NESTBAD", 5, Ok("[[[32],[<<200,201>>,97],32]]")),
            ("~10.4ts", "A", 4, Ok("[[[32],[97,769],98,99]]")),
            ("~10.4ts", "A", 5, Ok("[[[32],[[97,769],98,99],32]]")),
            ("~10.4ts", "A", 9, Ok("[[[32,[32,32],32,32],[[97,769],98,99],32]]")),
            ("~10.4ts", "U", 4, Ok("[[26085,46,46,46]]")),
            ("~10.4ts", "U", 5, Ok("[[[32],26085,26412,46,46]]")),
            ("~1ts", "KOU2", -1, Ok("[[26085]]")),
            ("~1ts", "KOU2", 0, Ok("[[[32],26085,26085]]")),
            ("~2ts", "KOU2", 2, Ok("[[[32],26085,26085]]")),
            ("~8ts", "KOU2", -1, Ok("[[[[32,32,32],32,32,32],26085,26085]]")),
            ("~8ts", "KOU2", 6, Ok("[[[[32,32],32,32],26085,26085]]")),
            ("~8ts", "KOU2", 5, Ok("[[[32,32,32],26085,26085]]")),
            ("~.3ts", "KOU2", -1, Ok("[[[26085,26085],32]]")),
            ("~10ts", "ATOM", 0, Ok("[32,32,32,32,32,32,32,97,98,99]")),
            ("~10.3ts", "ATOM", 4, Ok("[32,32,32,32,32,32,32,97,98,99]")),
            ("~10.4ts", "ATOM", 100, Ok("[32,32,32,32,32,32,97,98,99,32]")),
            ("~-ts", "U", -1, Err(())),
            ("~-ts", "KOU2", 100, Err(())),
        ];
        for (fmt, name, limit, expect) in cases {
            let mut heap = Heap::new(1 << 20).expect("test heap");
            let atoms = seeded();
            let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
            let subject = subject(&mut ctx, name);
            let got = format_ts(&mut ctx, fmt, subject, *limit);
            match expect {
                Err(()) => match got {
                    Err(BeamError::BadArg) => {}
                    other => panic!("{fmt} of {name} at CL {limit}: expected badarg, got {other:?}"),
                },
                Ok(expected) => {
                    let got = got.unwrap_or_else(|e| {
                        panic!("{fmt} of {name} at CL {limit}: expected {expected}, got {e:?}")
                    });
                    let want = parse(&mut ctx, expected);
                    assert!(
                        beam_term::compare::eq(got, want, ctx.atoms()),
                        "{fmt} of {name} at CL {limit}: does not match {expected}"
                    );
                }
            }
        }
    }

    /// A minimal parser for the matrix's value grammar: nested lists of small
    /// integers, byte binaries (`<<200,201>>`) and the empty list.
    fn parse(ctx: &mut BifContext<'_>, text: &str) -> Eterm {
        let mut chars = text.chars().peekable();
        let term = parse_value(ctx, &mut chars);
        assert!(chars.next().is_none(), "trailing text in {text:?}");
        term
    }

    fn parse_value(ctx: &mut BifContext<'_>, chars: &mut std::iter::Peekable<std::str::Chars>) -> Eterm {
        match chars.peek() {
            Some('[') => {
                chars.next();
                let mut items = Vec::new();
                if chars.peek() == Some(&']') {
                    chars.next();
                    return ctx.list(&items).expect("the empty list fits");
                }
                loop {
                    items.push(parse_value(ctx, chars));
                    match chars.next() {
                        Some(',') => {}
                        Some(']') => break,
                        other => panic!("expected `,`/`]`, got {other:?}"),
                    }
                }
                ctx.list(&items).expect("the list fits")
            }
            Some('<') => {
                chars.next();
                chars.next(); // the second `<`
                let mut bytes = Vec::new();
                if chars.peek() == Some(&'>') {
                    chars.next();
                    chars.next();
                } else {
                    loop {
                        bytes.push(parse_number(chars) as u8);
                        match chars.next() {
                            Some(',') => {}
                            Some('>') => {
                                assert_eq!(chars.next(), Some('>'));
                                break;
                            }
                            other => panic!("expected `,`/`>>`, got {other:?}"),
                        }
                    }
                }
                beam_codec::term::make_binary(ctx.heap(), &bytes).expect("the binary fits")
            }
            _ => {
                let value = parse_number(chars);
                ctx.int(value).expect("a small int fits")
            }
        }
    }

    fn parse_number(chars: &mut std::iter::Peekable<std::str::Chars>) -> i64 {
        let mut text = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_ascii_digit() || c == '-' {
                text.push(c);
                chars.next();
            } else {
                break;
            }
        }
        text.parse().unwrap_or_else(|_| panic!("expected a number, got {text:?}"))
    }
}
