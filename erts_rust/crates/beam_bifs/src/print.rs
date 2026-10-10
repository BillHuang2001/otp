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

//! The `~w`/`~p`/`~W`/`~P` term printer used by `io:format`/`io_lib:format`.
//!
//! A safe helper module for [`crate::io`] (it registers no BIFs). It renders the
//! Milestone-1 term set the way `io_lib:write/…` and the `~p` pretty printer do,
//! and pins every case against the OTP oracle (erts-17.1): small integers, atoms
//! (bare vs quoted), `NIL`, floats, proper/improper lists, tuples, maps, binaries,
//! bignums, internal pids and internal refs.
//!
//! This module owns the **leaf renderer** ([`term_chunk`]/[`render_chunk`], the
//! `io_lib:write1/4` tree) and the shared `io_lib_format` chunk helpers; the
//! `io_lib_pretty` `~p`/`~P` line re-flow lives in the private [`pretty`] submodule
//! and is re-exported here as [`pretty_chunk`], its `cind/7` probe in
//! `pretty::cind`.
//!
//! [`format_term`] is the depth-unlimited entry point `~w`/`~p` use;
//! [`format_term_depth`] is the depth-limited one `~W`/`~P` use. The depth
//! semantics reproduce `io_lib:write1/4` (`lib/stdlib/src/io_lib.erl`): depth `0`
//! renders `...`, depth `1` elides the *contents* of a container (`{...}`,
//! `[...]`, `<<...>>`, `#{}`), and every nesting step consumes one level; a
//! negative depth is unlimited (`~w` passes `-1`).
//!
//! [`crate::io`] renders every directive to a [`Chunk`] and materializes those for
//! `io_lib:format` (the deep list) or flattens them for `io:format` (the text).
//! [`format_term`] is a test-only text shortcut over the same renderer.
//!
//! # Nested chunks
//!
//! Rendering builds a tree of [`Chunk`]s mirroring the **exact nested-list
//! structure** the oracle's `io_lib`/`io_lib_pretty` produce (`write1/4`,
//! `write_tuple/6`, `write_tail/4`, `write_map/4`, `write_binary/2`): a container
//! is `[open, [e1, sep, e2, …], close]`, each element its own sub-list, each
//! separator an individual integer. [`format_term`]/[`format_term_depth`] flatten
//! the tree to text; `crate::io` materializes it as the deep list `io_lib:format`
//! returns.
//!
//! # The `~p` printable-string rule
//!
//! With `printable = true` (the `~p`/`~P` directives) a **non-empty proper list
//! whose every element is a printable latin1 character** (`16#20..=16#7E`,
//! `16#A0..=16#FF`, or one of `\t \n \r \v \b \f \e`) renders as a `"…"` string,
//! escaping `"`, `\` and those control characters (and leaving `16#A0..=16#FF`
//! verbatim); `~w` (`printable = false`) never applies the rule. So `~p` of
//! `"abc"` is `"abc"` while `~w` of `"abc"` is `[97,98,99]`, and `~p` of `[]` is
//! `[]` (the rule requires a *non-empty* list). The same rule is applied to
//! **binaries**, which then render as `<<"…">>`.
//!
//! # Maps
//!
//! A map renders `#{k => v,…}` (note ` => ` between a key and its value and a
//! bare `,` between pairs — the oracle's rendering). Pairs are printed in the
//! map's **storage order** ([`beam_term::map::map_pairs_storage_order`]) — the
//! order `maps:keys/1` yields: a flat map (≤32 pairs) stores its keys in the
//! oracle's *flatmap key order* (`erts_cmp_flatmap_keys`, `erl_utils.h:267`: two
//! atom keys by their raw term word, i.e. atom index / intern order; anything else
//! by the exact term order), and a hash map (>32 pairs) stores them in its HAMT slot
//! traversal order. Both match the oracle's `~w`/`~p` byte for byte.
//!
//! # `~p`/`~P` re-flow (`io_lib_pretty`)
//!
//! The oracle runs `io_lib_pretty`, which breaks a container across lines when its
//! single-line rendering is at least `line_length - column` characters (80 columns
//! and column 1 by default). [`pretty_chunk`] reproduces that break for a
//! proper/improper list, a tagged/untagged tuple and a map each of whose elements
//! renders single-line (the break separator `,`, the improper tail after `|`, a
//! `tag_len + 2` indent after a tuple tag, a two-space indent after `#{`), and for a
//! **non-printable byte-aligned binary**, whose `integer_to_list/1` byte elements
//! `pp_binary/7` packs `N = max(8, min(Ll - Col, M - 4))` per line. For the forms it
//! does not model — a nested container, an element too long to fit on a line of its
//! own, an all-printable binary (the oracle's string leaf), a non-byte-aligned
//! bitstring, a long tuple tag, a depth-limited `~P` — it raises `error:badarg`
//! rather than emit a line the oracle would have broken differently. See the
//! `crate::io` module docs for the precise rule, the term classes and the narrowest
//! repro.
//!
//! # Deviations from the oracle (documented)
//!
//! * **Floats** use Rust's shortest round-trip with a `.0` appended when integral
//!   (`1.0`, `1.5`, `0.1`, `3.14159`), not OTP's `float_to_list(F, [short])`:
//!   large/small magnitudes differ (`1.0e10` renders `10000000000`, the oracle
//!   `1.0e10`).
//! * **Quoted atoms are not re-escaped** (a `'` inside a name is printed verbatim)
//!   — such atoms are not produced by Milestone-1.
//! * **Non-byte-aligned bitstrings** render as the oracle's `<<X:L>>` (whole leading
//!   bytes, then the trailing `L` bits as `Value:L`), e.g. `<<1:1>>`, `<<23:5>>`,
//!   `<<0,1:1>>`. The `~p` printable-string rule never applies to them.
//! * **Internal refs** render as `#Ref<Channel.Node.Serial.Id>` with
//!   `Channel`/`Node`/`Serial` always `0` in Milestone-1 (the frozen
//!   `[header, node, serial, id]` thing of [`crate::access::make_ref_thing`]), so
//!   only `Id` varies. The oracle's numbers are the same shape.
//! * External pids/ports/refs, funs and records are outside the Milestone-1 term
//!   set and are rejected with `BeamError::BadArg` rather than guessed — never a
//!   *silently wrong* result.

use beam_codec::term::big_parts;
use beam_common::Result;
use beam_term::{atom_val, pid_number, pid_serial, signed_value, AtomTable, Eterm};

use crate::access::{
    bitstring_bit_size, bitstring_bits, car, cdr, float_value, is_atom, is_big, is_float,
    is_internal_pid, is_internal_ref, is_list, is_map, is_nil, is_small, is_tuple, ref_thing_id,
    tuple_element, tuple_size,
};
use crate::bits;
use crate::context::BifContext;
use crate::error::badarg;

mod pretty;
pub(crate) use pretty::pretty_chunk;

/// The depth value that disables depth limiting — `io_lib` passes `-1` for `~w`.
pub(crate) const UNLIMITED_DEPTH: i64 = -1;

/// A fragment of a rendering: the exact nested character-list structure the
/// oracle's `io_lib`/`io_lib_pretty` build.
///
/// An `io_lib` "chars" value is a list whose elements are either single
/// characters (small integers) or further lists: `Char` is one such element and
/// `List` a sub-list element. `io:format` flattens the tree to text, while
/// `io_lib:format` materializes it as the corresponding nested term (see
/// `crate::io`).
#[derive(Clone)]
pub(crate) enum Chunk {
    /// One character element — a single small integer in the output list.
    Char(char),
    /// A sub-list element.
    List(Vec<Chunk>),
}

impl Chunk {
    /// A flat character list (`"abc"` ⇒ the one element `[a, b, c]`).
    pub(crate) fn text(text: &str) -> Chunk {
        Chunk::List(text.chars().map(Chunk::Char).collect())
    }

    /// Appends the flattened text to `out`.
    pub(crate) fn flatten_into(&self, out: &mut String) {
        match self {
            Chunk::Char(c) => out.push(*c),
            Chunk::List(items) => {
                for item in items {
                    item.flatten_into(out);
                }
            }
        }
    }

    /// The flat character count (`io_lib:chars_length/1`).
    pub(crate) fn flat_len(&self) -> usize {
        match self {
            Chunk::Char(_) => 1,
            Chunk::List(items) => items.iter().map(Chunk::flat_len).sum(),
        }
    }

    /// The element list of a `Chunk::List`.
    fn into_items(self) -> Vec<Chunk> {
        match self {
            Chunk::List(items) => items,
            char => vec![char],
        }
    }

    /// The first `n` characters as one flat sub-list (`flat_trunc/3`).
    fn flat_trunc(&self, n: usize) -> Chunk {
        let mut text = String::new();
        self.flatten_into(&mut text);
        Chunk::List(text.chars().take(n).map(Chunk::Char).collect())
    }
}

/// Flattens a chunk sequence to its text.
pub(crate) fn flatten(chunks: &[Chunk]) -> String {
    let mut out = String::new();
    for chunk in chunks {
        chunk.flatten_into(&mut out);
    }
    out
}

/// Renders `term` the way the `~w` (`printable = false`) or `~p`
/// (`printable = true`) directive does, with no depth limit — the **text** form.
///
/// Test-only: `crate::io` renders its directives to [`Chunk`]s and flattens those
/// itself, so this text shortcut is used by the assertions of this module and of
/// `crate::lists` alone.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a term outside the
/// Milestone-1 printable set (see the module docs), or an atom whose index is not
/// interned in `ctx`'s atom table.
#[cfg(test)]
pub(crate) fn format_term(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    printable: bool,
) -> Result<String> {
    format_term_depth(ctx, term, printable, UNLIMITED_DEPTH)
}

/// Renders `term` with an `io_lib:write/2` **depth** limit (`~W`/`~P`); see
/// [`format_term`].
///
/// A negative `depth` is unlimited (see [`UNLIMITED_DEPTH`]); `0` renders `...`
/// and `1` elides a container's contents.
///
/// # Errors
///
/// As [`format_term`].
#[cfg(test)]
pub(crate) fn format_term_depth(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    printable: bool,
    depth: i64,
) -> Result<String> {
    Ok(flatten(&[term_chunk(ctx, term, printable, depth)?]))
}

/// Renders `term` as one element chunk (`io_lib:write1/4`, or
/// `io_lib_pretty:write/1` when `printable`, the `~p` string rule).
///
/// # Errors
///
/// As [`format_term`].
pub(crate) fn term_chunk(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    printable: bool,
    depth: i64,
) -> Result<Chunk> {
    render_chunk(ctx.atoms(), term, printable, depth)
}
/// Collects `term` into its element vector iff it is a proper list.
///
/// `NIL` yields `Some(vec![])`; a cons chain ending in anything but `NIL`, or a
/// non-list term, yields `None`. Shared with [`crate::io`] and [`crate::ets`] so
/// the "is this a proper list?" rule lives in exactly one place.
pub(crate) fn proper_list(term: Eterm) -> Option<Vec<Eterm>> {
    let mut elems = Vec::new();
    let mut cur = term;
    while is_list(cur) {
        elems.push(car(cur));
        cur = cdr(cur);
    }
    if is_nil(cur) {
        Some(elems)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// The `~p` printable-character rule, shared with the `pretty` sub-module
// ---------------------------------------------------------------------------

/// `io_lib:printable_character(C, latin1)`: `16#20..=16#7E`, `16#A0..=16#FF`, or
/// one of `\t \n \r \v \b \f \e` (`io_lib.erl:1514-1523`).
fn latin1_printable_char(c: u8) -> bool {
    (0x20..=0x7E).contains(&c)
        || (0xA0..=0xFF).contains(&c)
        || matches!(c, 0x09 | 0x0A | 0x0D | 0x0B | 0x08 | 0x0C | 0x1B)
}

/// [`latin1_printable_char`] for a small integer's value: the value must be a byte
/// (`0..=255`) that is a printable latin1 character. A value outside the byte
/// range is never printable.
fn char_printable(value: i64) -> bool {
    (0..=255).contains(&value) && latin1_printable_char(value as u8)
}

/// Renders `term` as one element chunk (`io_lib:write1/4` for `~w`, or
/// `io_lib_pretty:write/1` for `~p`).
fn render_chunk(atoms: &AtomTable, term: Eterm, printable: bool, depth: i64) -> Result<Chunk> {
    if depth == 0 {
        return Ok(Chunk::text("..."));
    }
    if is_small(term) {
        return Ok(Chunk::text(&signed_value(term).to_string()));
    }
    if is_big(term) {
        return Ok(Chunk::text(&format_big(term)?));
    }
    if is_nil(term) {
        return Ok(Chunk::text("[]"));
    }
    if is_list(term) {
        if depth == 1 {
            return Ok(Chunk::text("[...]"));
        }
        if printable {
            if let Some(bytes) = printable_string(term) {
                return Ok(Chunk::text(&quoted_string(&bytes)));
            }
        }
        return render_list_chunk(atoms, term, printable, depth);
    }
    if is_tuple(term) {
        let arity = tuple_size(term);
        if arity == 0 {
            return Ok(Chunk::text("{}"));
        }
        if depth == 1 {
            return Ok(Chunk::text("{...}"));
        }
        let mut body: Vec<Chunk> = Vec::with_capacity(arity);
        body.push(render_chunk(atoms, tuple_element(term, 0), printable, depth - 1)?);
        render_tuple_rest_chunk(atoms, term, 1, depth - 1, printable, &mut body)?;
        return Ok(Chunk::List(vec![
            Chunk::Char('{'),
            Chunk::List(body),
            Chunk::Char('}'),
        ]));
    }
    if is_atom(term) {
        let name = atoms.name(atom_val(term)).ok_or_else(badarg)?;
        return Ok(Chunk::text(&format_atom(name)));
    }
    if is_float(term) {
        return Ok(Chunk::text(&format_float(float_value(term))));
    }
    if bits::is_binary(term) {
        return render_binary_chunk(term, printable, depth);
    }
    if bits::is_bitstring(term) {
        return render_bitstring_chunk(term, printable, depth);
    }
    if is_map(term) {
        return render_map_chunk(atoms, term, printable, depth);
    }
    if is_internal_pid(term) {
        return Ok(Chunk::text(&format!(
            "<0.{}.{}>",
            pid_number(term),
            pid_serial(term)
        )));
    }
    if is_internal_ref(term) {
        return Ok(Chunk::text(&format_ref(term)?));
    }
    Err(badarg())
}
/// Renders a list (`[e1,e2]` / `[e1|tail]`), the caller having already handled
/// depth `0`/`1` and the `~p` printable-string rule (`io_lib:write1/4`).
fn render_list_chunk(
    atoms: &AtomTable,
    term: Eterm,
    printable: bool,
    depth: i64,
) -> Result<Chunk> {
    let mut body: Vec<Chunk> = Vec::new();
    body.push(render_chunk(atoms, car(term), printable, depth - 1)?);
    render_tail_chunk(atoms, cdr(term), printable, depth - 1, &mut body)?;
    Ok(Chunk::List(vec![
        Chunk::Char('['),
        Chunk::List(body),
        Chunk::Char(']'),
    ]))
}

/// Appends a list tail (`""`, `,e…` or `|tail`, `io_lib:write_tail/4`); the
/// closing `]` is the caller's.
fn render_tail_chunk(
    atoms: &AtomTable,
    term: Eterm,
    printable: bool,
    depth: i64,
    out: &mut Vec<Chunk>,
) -> Result<()> {
    if is_nil(term) {
        return Ok(());
    }
    if depth == 1 {
        // `[$| | "..."]` — the four characters are spliced individually.
        out.extend(['|', '.', '.', '.'].map(Chunk::Char));
        return Ok(());
    }
    if is_list(term) {
        out.push(Chunk::Char(','));
        out.push(render_chunk(atoms, car(term), printable, depth - 1)?);
        return render_tail_chunk(atoms, cdr(term), printable, depth - 1, out);
    }
    out.push(Chunk::Char('|'));
    out.push(render_chunk(atoms, term, printable, depth - 1)?);
    Ok(())
}

/// Appends the remaining tuple elements (`,e…` / `,...`), `io_lib:write_tuple/6`.
fn render_tuple_rest_chunk(
    atoms: &AtomTable,
    term: Eterm,
    index: usize,
    depth: i64,
    printable: bool,
    out: &mut Vec<Chunk>,
) -> Result<()> {
    if index >= tuple_size(term) {
        return Ok(());
    }
    if depth == 1 {
        out.extend([',', '.', '.', '.'].map(Chunk::Char));
        return Ok(());
    }
    out.push(Chunk::Char(','));
    out.push(render_chunk(atoms, tuple_element(term, index), printable, depth - 1)?);
    render_tuple_rest_chunk(atoms, term, index + 1, depth - 1, printable, out)
}
/// Renders a map (`#{k => v,…}`), `io_lib:write_map/4` /
/// `io_lib_pretty:write/1` (both build `[#, {, [pair, …, pair], }]`).
///
/// The pairs come from the map's storage order (flat-map key order for a flat map,
/// HAMT slot order for a hash map) — the order `maps:keys/1` yields and the oracle
/// prints — so no re-sort is needed and a >32-pair hash map renders like the oracle.
fn render_map_chunk(
    atoms: &AtomTable,
    term: Eterm,
    printable: bool,
    depth: i64,
) -> Result<Chunk> {
    if depth == 1 {
        return Ok(Chunk::text("#{}"));
    }
    let pairs: Vec<(Eterm, Eterm)> = beam_term::map::map_pairs_storage_order(term)?;
    if pairs.is_empty() {
        return Ok(Chunk::text("#{}"));
    }
    let inner = depth - 1;
    let mut body: Vec<Chunk> = Vec::new();
    body.push(map_pair_chunk(atoms, pairs[0], printable, inner)?);
    render_map_body_chunk(atoms, &pairs, 1, inner, inner, printable, &mut body)?;
    Ok(Chunk::List(vec![
        Chunk::Char('#'),
        Chunk::Char('{'),
        Chunk::List(body),
        Chunk::Char('}'),
    ]))
}

/// Appends the remaining map pairs (`,k => v…` / `,...`),
/// `io_lib:write_map_body/5`.
///
/// `depth` starts at the first pair's inner depth and decreases per pair; `inner`
/// is the (fixed) depth every key and value is rendered at.
fn render_map_body_chunk(
    atoms: &AtomTable,
    pairs: &[(Eterm, Eterm)],
    index: usize,
    depth: i64,
    inner: i64,
    printable: bool,
    out: &mut Vec<Chunk>,
) -> Result<()> {
    if depth == 1 {
        out.extend([',', '.', '.', '.'].map(Chunk::Char));
        return Ok(());
    }
    if index >= pairs.len() {
        return Ok(());
    }
    out.push(Chunk::Char(','));
    out.push(map_pair_chunk(atoms, pairs[index], printable, inner)?);
    render_map_body_chunk(atoms, pairs, index + 1, depth - 1, inner, printable, out)
}

/// Renders one map pair (`io_lib:write_map_assoc/4`): `[Key, " => ", Value]` —
/// the four ` => ` characters are one sub-list element.
fn map_pair_chunk(
    atoms: &AtomTable,
    pair: (Eterm, Eterm),
    printable: bool,
    depth: i64,
) -> Result<Chunk> {
    Ok(Chunk::List(vec![
        render_chunk(atoms, pair.0, printable, depth)?,
        Chunk::text(" => "),
        render_chunk(atoms, pair.1, printable, depth)?,
    ]))
}
/// `io_lib_format:chars/2` — a deeply nested `lists:duplicate/2`.
///
/// `chars(C, N)` is `[]`/`[C]`/`[C,C]`/`[C,C,C]` for `N` in `0..=3`, else `[S|S]`
/// (`S` = `chars(C, N/2)`) for even `N` and `[C,S|S]` for odd `N`, so a long run
/// is a *tree* of sub-lists, not a flat list.
pub(crate) fn dup_chars(c: char, n: usize) -> Chunk {
    match n {
        0 => Chunk::List(Vec::new()),
        1 => Chunk::List(vec![Chunk::Char(c)]),
        2 => Chunk::List(vec![Chunk::Char(c), Chunk::Char(c)]),
        3 => {
            Chunk::List(vec![Chunk::Char(c), Chunk::Char(c), Chunk::Char(c)])
        }
        n if n % 2 == 0 => {
            let half = dup_chars(c, n / 2);
            let mut items = vec![half.clone()];
            items.extend(half.into_items());
            Chunk::List(items)
        }
        n => {
            let half = dup_chars(c, n / 2);
            let mut items = vec![Chunk::Char(c), half.clone()];
            items.extend(half.into_items());
            Chunk::List(items)
        }
    }
}

/// `io_lib_format:adjust/3` — `[Data|Pad]` (left) / `[Pad|Data]` (right), or the
/// data unchanged when the pad is the empty list.
pub(crate) fn adjust_chunk(data: Chunk, pad: Chunk, left: bool) -> Chunk {
    let pad_items = pad.into_items();
    if pad_items.is_empty() {
        return data;
    }
    let data_items = data.into_items();
    if left {
        let mut items = vec![Chunk::List(data_items)];
        items.extend(pad_items);
        Chunk::List(items)
    } else {
        let mut items = vec![Chunk::List(pad_items)];
        items.extend(data_items);
        Chunk::List(items)
    }
}

/// `io_lib_format:term/6` — the field-width / truncation rule as a chunk.
pub(crate) fn field_chunk(
    body: Chunk,
    width: Option<usize>,
    left: bool,
    precision: Option<usize>,
    pad: u8,
) -> Chunk {
    let (field, precision) = match (width, precision) {
        (None, None) => return body,
        (None, Some(p)) => (p, p),
        (Some(f), None) => (f, f),
        (Some(f), Some(p)) => (f, p),
    };
    let length = body.flat_len();
    let effective = length.min(precision.min(field));
    if length > effective {
        adjust_chunk(
            dup_chars('*', effective),
            dup_chars(pad as char, field - effective),
            left,
        )
    } else {
        adjust_chunk(body, dup_chars(pad as char, field - length), left)
    }
}

/// `io_lib_format:string/6` — the `~s` field rule as a chunk.
pub(crate) fn string_field_chunk(
    s: Chunk,
    width: Option<usize>,
    left: bool,
    precision: Option<usize>,
    pad: u8,
) -> Result<Chunk> {
    match (width, precision) {
        (None, None) => Ok(s),
        (Some(f), None) => Ok(string_field(s, f, left, pad)),
        (None, Some(p)) => Ok(string_field(s, p, true, pad)),
        (Some(f), Some(p)) if f >= p => {
            let n = s.flat_len();
            if f > p {
                if n > p {
                    Ok(adjust_chunk(
                        s.flat_trunc(p),
                        dup_chars(pad as char, f - p),
                        left,
                    ))
                } else if n < p {
                    // `[S | chars(Pad, P-N)]`.
                    let mut items = vec![s];
                    items.extend(dup_chars(pad as char, p - n).into_items());
                    Ok(adjust_chunk(
                        Chunk::List(items),
                        dup_chars(pad as char, f - p),
                        left,
                    ))
                } else {
                    Ok(adjust_chunk(s, dup_chars(pad as char, f - p), left))
                }
            } else {
                Ok(string_field(s, f, left, pad))
            }
        }
        _ => Err(badarg()),
    }
}

/// `io_lib_format:string_field/6`.
fn string_field(s: Chunk, f: usize, left: bool, pad: u8) -> Chunk {
    let n = s.flat_len();
    if n > f {
        s.flat_trunc(f)
    } else if n < f {
        adjust_chunk(s, dup_chars(pad as char, f - n), left)
    } else {
        s
    }
}
/// Renders a binary (`io_lib:write_binary/2` for `~w`,
/// `io_lib_pretty:write/1` for `~p`).
///
/// The `~w` form is `[<, <, [digits, 44, digits, …], >, >]` (each byte its own
/// sub-list, the separators individual integers; `[<, <, [], >, >]` for an empty
/// binary, `[<, <, ["..."], >, >]` at depth 1). The `~p` form keeps the string
/// leaf flat: `<<>>` / `<<...>>` / `<<"…">>`.
fn render_binary_chunk(term: Eterm, printable: bool, depth: i64) -> Result<Chunk> {
    let bytes = bits::bytes(term)?;
    if printable {
        if bytes.is_empty() {
            return Ok(Chunk::text("<<>>"));
        }
        if depth == 1 {
            return Ok(Chunk::text("<<...>>"));
        }
        if printable_bytes(&bytes) {
            return Ok(Chunk::List(vec![
                Chunk::Char('<'),
                Chunk::Char('<'),
                Chunk::text(&quoted_string(&bytes)),
                Chunk::Char('>'),
                Chunk::Char('>'),
            ]));
        }
    } else if bytes.is_empty() {
        return Ok(Chunk::List(vec![
            Chunk::Char('<'),
            Chunk::Char('<'),
            Chunk::List(Vec::new()),
            Chunk::Char('>'),
            Chunk::Char('>'),
        ]));
    }
    let mut body: Vec<Chunk> = Vec::new();
    if depth == 1 {
        body.push(Chunk::text("..."));
    } else {
        for (i, byte) in bytes.iter().enumerate() {
            if i != 0 {
                body.push(Chunk::Char(','));
            }
            body.push(Chunk::text(&byte.to_string()));
        }
    }
    Ok(Chunk::List(vec![
        Chunk::Char('<'),
        Chunk::Char('<'),
        Chunk::List(body),
        Chunk::Char('>'),
        Chunk::Char('>'),
    ]))
}

/// Renders a **non-byte-aligned** bitstring (`io_lib:write_binary/2` /
/// `io_lib_pretty:write/1`).
///
/// The oracle prints `<<` + the whole leading bytes as comma-separated decimals +
/// (when bits remain) a comma and the trailing bit field `Value:BitCount` + `>>`;
/// erts-17.1 renders e.g. `<<1:1>>` ⇒ `<<1:1>>`, `<<5:3,3:2>>` ⇒ `<<23:5>>`,
/// `<<1:9>>` ⇒ `<<0,1:1>>`, `<<255:12>>` ⇒ `<<15,15:4>>`. The nested-list
/// structure matches [`render_binary_chunk`]: the container is
/// `[<, <, body, >, >]`, each rendered number its own sub-list and every `,`/`:`
/// an individual integer.
///
/// A byte-aligned bitstring is never routed here (the binary branch of
/// [`render_chunk`] handles it), but the delegation keeps the helper total. The
/// `~p` printable-string rule does **not** apply — a non-byte-aligned bitstring is
/// never a string — yet `~p`/`~P` still collapse a depth-1 (`<<...>>`) render
/// exactly like a printable binary (the printable branch of
/// [`render_binary_chunk`]).
fn render_bitstring_chunk(term: Eterm, printable: bool, depth: i64) -> Result<Chunk> {
    let bit_len = bitstring_bit_size(term)?;
    if bit_len % 8 == 0 {
        return render_binary_chunk(term, printable, depth);
    }
    if printable && depth == 1 {
        // `io_lib_pretty` truncates a printable-bitstring render to the flat
        // `<<...>>` (mirrors the depth-1 case of `render_binary_chunk`).
        return Ok(Chunk::text("<<...>>"));
    }
    let data = bitstring_bits(term)?;
    let full_bytes = bit_len / 8;
    let trailing_bits = bit_len % 8;
    let mut body: Vec<Chunk> = Vec::new();
    if depth == 1 {
        body.push(Chunk::text("..."));
    } else {
        for (i, byte) in data[..full_bytes].iter().enumerate() {
            if i != 0 {
                body.push(Chunk::Char(','));
            }
            body.push(Chunk::text(&byte.to_string()));
        }
        if full_bytes != 0 {
            body.push(Chunk::Char(','));
        }
        // `data[full_bytes]` holds the trailing `trailing_bits` bits in its most
        // significant positions (the packing is MSB-first), so shift them down.
        let trailing_value = data[full_bytes] >> (8 - trailing_bits);
        body.push(Chunk::text(&trailing_value.to_string()));
        body.push(Chunk::Char(':'));
        body.push(Chunk::text(&trailing_bits.to_string()));
    }
    Ok(Chunk::List(vec![
        Chunk::Char('<'),
        Chunk::Char('<'),
        Chunk::List(body),
        Chunk::Char('>'),
        Chunk::Char('>'),
    ]))
}
/// The byte values of `term` iff it is a non-empty proper list of printable latin1
/// characters (the `~p` printable-string rule; see [`char_printable`]).
fn printable_string(term: Eterm) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut cur = term;
    while is_list(cur) {
        let head = car(cur);
        if !is_small(head) {
            return None;
        }
        let value = signed_value(head);
        if !char_printable(value) {
            return None;
        }
        bytes.push(value as u8);
        cur = cdr(cur);
    }
    if !is_nil(cur) || bytes.is_empty() {
        return None;
    }
    Some(bytes)
}

/// Whether every byte is printable latin1 and the binary is non-empty (the `~p`
/// printable-binary rule, mirroring [`printable_string`]).
fn printable_bytes(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().all(|&b| latin1_printable_char(b))
}

/// Renders `bytes` as a double-quoted string, escaping `"`, `\` and the latin1
/// control characters the oracle's `io_lib:string_char/4` escapes (`\b \t \n \v \f
/// \r \e`). Bytes `16#A0..=16#FF` are emitted verbatim as their Unicode code point
/// (the enclosing [`Chunk`] is a Rust `String`, so the oracle's UTF-8 output — e.g.
/// `c2 a0` for `16#A0` — comes out byte-for-byte).
fn quoted_string(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() + 2);
    out.push('"');
    for &byte in bytes {
        match byte {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            0x08 => out.push_str("\\b"),
            0x09 => out.push_str("\\t"),
            0x0A => out.push_str("\\n"),
            0x0B => out.push_str("\\v"),
            0x0C => out.push_str("\\f"),
            0x0D => out.push_str("\\r"),
            0x1B => out.push_str("\\e"),
            _ => out.push(char::from(byte)),
        }
    }
    out.push('"');
    out
}
/// Renders a bignum in decimal (`erts_big_to_string(wobj, 10, …)`).
///
/// The digits are read with the safe [`beam_codec::term::big_parts`] wrapper (the
/// crate's `access.rs` exposes no bignum digit accessor and is the crate's only
/// `#[allow(unsafe_code)]` module; `beam_codec` is already a declared dependency).
fn format_big(term: Eterm) -> Result<String> {
    let (negative, digits) = big_parts(term)?;
    Ok(format_big_decimal(negative, digits))
}

/// Converts a least-significant-first `u64` digit vector to decimal.
fn format_big_decimal(negative: bool, digits: &[u64]) -> String {
    let mut limbs: Vec<u64> = digits.to_vec();
    while limbs.len() > 1 && limbs.last() == Some(&0) {
        limbs.pop();
    }
    let mut reversed: Vec<char> = Vec::new();
    loop {
        // One base-2^64 long division by 10 (the remainder is the next decimal
        // digit, least significant first).
        let mut rem: u128 = 0;
        let mut nonzero = false;
        for i in (0..limbs.len()).rev() {
            let current = (rem << 64) | u128::from(limbs[i]);
            let quotient = (current / 10) as u64;
            limbs[i] = quotient;
            nonzero |= quotient != 0;
            rem = current % 10;
        }
        reversed.push(char::from(b'0' + rem as u8));
        if !nonzero {
            break;
        }
    }
    let mut out: String = reversed.iter().rev().collect();
    if negative {
        out.insert(0, '-');
    }
    out
}

/// Renders an atom name: bare when it matches `[a-z][A-Za-z0-9_@]*`, else quoted.
fn format_atom(name: &str) -> String {
    if is_bare_atom(name) {
        name.to_string()
    } else {
        format!("'{name}'")
    }
}

/// Whether `name` is a bare atom name (`[a-z][A-Za-z0-9_@]*`).
fn is_bare_atom(name: &str) -> bool {
    let bytes = name.as_bytes();
    match bytes.first() {
        Some(&first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    bytes[1..]
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'_' || b == b'@')
}

/// Renders a float: Rust's shortest round-trip, with a `.0` appended when the
/// rendering is integral (deviation from `float_to_list(F, [short])`; see the
/// module docs).
fn format_float(value: f64) -> String {
    let mut s = format!("{value}");
    if value.is_finite() && !s.contains('.') && !s.contains('e') && !s.contains('E') {
        s.push_str(".0");
    }
    s
}

/// Renders an internal ref as `#Ref<0.0.0.Id>` (Milestone-1 refs carry no channel,
/// node or serial — see the module docs).
fn format_ref(term: Eterm) -> Result<String> {
    let id = ref_thing_id(term)?;
    Ok(format!("#Ref<0.0.0.{id}>"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable, NIL};

    /// Runs `f` over a fresh context whose atom table is seeded with the names the
    /// tests build terms from.
    fn with_ctx<R>(f: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
        let mut heap = Heap::new(1024).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok", "hello", "Hello", "a", "b", "c", "abc", "hi"]);
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&mut ctx)
    }

    /// Renders `term` with `printable`, asserting success.
    fn show(ctx: &mut BifContext<'_>, term: Eterm, printable: bool) -> String {
        format_term(ctx, term, printable).expect("term is printable")
    }

    /// Renders `term` at `depth`.
    fn show_at(ctx: &mut BifContext<'_>, term: Eterm, printable: bool, depth: i64) -> String {
        format_term_depth(ctx, term, printable, depth).expect("term is printable")
    }

    /// A flat map whose keys are stored in the canonical flat-map key order
    /// ([`beam_term::compare::cmp_flatmap_keys`]).
    ///
    /// [`crate::access::make_flatmap`] preserves the pair order it is handed, so the
    /// writer canonicalises first — exactly the step `maps.rs::build_map` performs (and
    /// what `beam_term::map::write_map` sorts for). A real flat map always stores its
    /// keys in that order, which is why the printer renders the map's *storage* order
    /// without re-sorting.
    fn canonical_flatmap(ctx: &mut BifContext<'_>, pairs: &[(Eterm, Eterm)]) -> Eterm {
        let mut pairs = pairs.to_vec();
        pairs.sort_by(|a, b| beam_term::compare::cmp_flatmap_keys(a.0, b.0, ctx.atoms()));
        crate::access::make_flatmap(ctx.heap(), &pairs).expect("the flat map fits")
    }

    #[test]
    fn small_ints_and_atoms() {
        with_ctx(|ctx| {
            let positive = ctx.int(7).expect("fits");
            assert_eq!(show(ctx, positive, false), "7");
            let negative = ctx.int(-5).expect("fits");
            assert_eq!(show(ctx, negative, false), "-5");

            let hello = ctx.atom("hello").expect("seeded");
            assert_eq!(show(ctx, hello, false), "hello");
            // `Hello` is not a bare name (`[a-z]…`), so it is quoted.
            let quoted = ctx.atom("Hello").expect("seeded");
            assert_eq!(show(ctx, quoted, false), "'Hello'");
        });
    }

    #[test]
    fn tuple_and_lists() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").expect("seeded");
            let one = ctx.int(1).expect("fits");
            let tuple = ctx.tuple(&[a, one]).expect("fits");
            assert_eq!(show(ctx, tuple, false), "{a,1}");

            let two = ctx.int(2).expect("fits");
            let tail = ctx.cons(two, NIL).expect("fits");
            let list = ctx.cons(one, tail).expect("fits");
            assert_eq!(show(ctx, list, false), "[1,2]");

            // `[]` is `NIL`, `{}` the empty tuple.
            assert_eq!(show(ctx, NIL, false), "[]");
            assert_eq!(show(ctx, NIL, true), "[]");
            let empty = ctx.tuple(&[]).expect("fits");
            assert_eq!(show(ctx, empty, false), "{}");
            assert_eq!(show_at(ctx, empty, false, 1), "{}");

            // An improper list renders `[head|tail]`.
            let b = ctx.atom("b").expect("seeded");
            let improper = ctx.cons(a, b).expect("fits");
            assert_eq!(show(ctx, improper, false), "[a|b]");
            assert_eq!(show(ctx, improper, true), "[a|b]");
        });
    }

    #[test]
    fn floats_use_shortest_round_trip_with_dot_zero() {
        with_ctx(|ctx| {
            for (value, expected) in [
                (1.0f64, "1.0"),
                (1.5, "1.5"),
                (0.1, "0.1"),
                (3.14159, "3.14159"),
                (-2.5, "-2.5"),
            ] {
                let term = ctx.make_float(value).expect("fits");
                assert_eq!(show(ctx, term, false), expected, "float {value}");
            }
        });
    }

    #[test]
    fn printable_string_rule() {
        with_ctx(|ctx| {
            // "abc" == [97,98,99].
            let abc = ctx
                .list(&[
                    ctx.int(97).expect("fits"),
                    ctx.int(98).expect("fits"),
                    ctx.int(99).expect("fits"),
                ])
                .expect("fits");
            assert_eq!(show(ctx, abc, true), "\"abc\"");
            assert_eq!(show(ctx, abc, false), "[97,98,99]");

            // A byte below 32 disqualifies the whole list (even with `~p`).
            let controls = ctx
                .list(&[ctx.int(31).expect("fits"), ctx.int(32).expect("fits")])
                .expect("fits");
            assert_eq!(show(ctx, controls, true), "[31,32]");

            // 127 is outside `32..=126`.
            let high = ctx.list(&[ctx.int(127).expect("fits")]).expect("fits");
            assert_eq!(show(ctx, high, true), "[127]");

            // Escaping: `"` (34) and `\` (92).
            let quote = ctx.list(&[ctx.int(34).expect("fits")]).expect("fits");
            assert_eq!(show(ctx, quote, true), "\"\\\"\"");
            let backslash = ctx.list(&[ctx.int(92).expect("fits")]).expect("fits");
            assert_eq!(show(ctx, backslash, true), "\"\\\\\"");
        });
    }

    #[test]
    fn printable_rule_propagates_into_tuples() {
        with_ctx(|ctx| {
            let ok = ctx.atom("ok").expect("seeded");
            let hi = ctx
                .list(&[ctx.int(104).expect("fits"), ctx.int(105).expect("fits")])
                .expect("fits");
            let float = ctx.make_float(1.5).expect("fits");
            let tuple = ctx.tuple(&[ok, hi, float]).expect("fits");
            assert_eq!(show(ctx, tuple, true), "{ok,\"hi\",1.5}");
        });
    }

    #[test]
    fn nested_lists_respect_the_rule() {
        with_ctx(|ctx| {
            let inner1 = ctx
                .list(&[ctx.int(1).expect("fits"), ctx.int(2).expect("fits")])
                .expect("fits");
            let inner2 = ctx.list(&[ctx.int(3).expect("fits")]).expect("fits");
            let outer = ctx.list(&[inner1, inner2]).expect("fits");
            assert_eq!(show(ctx, outer, true), "[[1,2],[3]]");
        });
    }

    #[test]
    fn binaries_render_in_the_oracle_form() {
        with_ctx(|ctx| {
            // `~w` of `<<"abc">>` is `<<97,98,99>>`; `~p` is `<<"abc">>`.
            // (`erl -noshell -eval 'io:format("~p~n",[io_lib:format("~w",[<<"abc">>])])'`)
            let abc = bits::make_binary(ctx.heap(), b"abc").expect("fits");
            assert_eq!(show(ctx, abc, false), "<<97,98,99>>");
            assert_eq!(show(ctx, abc, true), "<<\"abc\">>");

            // Non-printable bytes stay in the `~w` form under `~p` too.
            let low = bits::make_binary(ctx.heap(), &[1, 2, 3]).expect("fits");
            assert_eq!(show(ctx, low, false), "<<1,2,3>>");
            assert_eq!(show(ctx, low, true), "<<1,2,3>>");

            // An empty binary is `<<>>` in both forms (oracle).
            let empty = bits::make_binary(ctx.heap(), &[]).expect("fits");
            assert_eq!(show(ctx, empty, false), "<<>>");
            assert_eq!(show(ctx, empty, true), "<<>>");
            assert_eq!(show_at(ctx, empty, true, 1), "<<>>");

            // `"`/`\` escape inside the `~p` string form.
            let esc = bits::make_binary(ctx.heap(), b"\"\\").expect("fits");
            assert_eq!(show(ctx, esc, true), "<<\"\\\"\\\\\">>");
        });
    }

    #[test]
    fn non_byte_aligned_bitstrings_render_in_the_oracle_form() {
        with_ctx(|ctx| {
            // All pinned to erts-17.1, e.g.
            // `erl -noshell -eval 'io:format("~w~n",[<<1:1>>]).'`.
            // `<<1:1>>` — one bit, no whole leading byte.
            let one = bits::make_bitstring(ctx.heap(), &[0b1000_0000], 1).expect("fits");
            assert_eq!(show(ctx, one, false), "<<1:1>>");
            assert_eq!(show(ctx, one, true), "<<1:1>>");
            // `<<5:3,3:2>>` = `10111` ⇒ `<<23:5>>`.
            let five = bits::make_bitstring(ctx.heap(), &[0b1011_1000], 5).expect("fits");
            assert_eq!(show(ctx, five, false), "<<23:5>>");
            // `<<1:9>>` — one whole leading byte then one trailing bit.
            let nine = bits::make_bitstring(ctx.heap(), &[0x00, 0b1000_0000], 9).expect("fits");
            assert_eq!(show(ctx, nine, false), "<<0,1:1>>");
            // `<<255:12>>` = `00001111 1111` ⇒ `<<15,15:4>>`.
            let twelve = bits::make_bitstring(ctx.heap(), &[0x0F, 0b1111_0000], 12).expect("fits");
            assert_eq!(show(ctx, twelve, false), "<<15,15:4>>");
            // `<<0:3>>` — every bit zero.
            let zero = bits::make_bitstring(ctx.heap(), &[0], 3).expect("fits");
            assert_eq!(show(ctx, zero, false), "<<0:3>>");

            // Nested in a tuple and a list (oracle).
            let a = ctx.atom("a").expect("seeded");
            let tuple = ctx.tuple(&[one, a]).expect("fits");
            assert_eq!(show(ctx, tuple, false), "{<<1:1>>,a}");
            let two = ctx.int(2).expect("fits");
            let list = ctx.list(&[one, two]).expect("fits");
            assert_eq!(show(ctx, list, false), "[<<1:1>>,2]");

            // Depth: `~W`/`~P` elide a bitstring's contents at depth 1.
            assert_eq!(show_at(ctx, one, false, 1), "<<...>>");
            assert_eq!(show_at(ctx, one, true, 1), "<<...>>");
            assert_eq!(show_at(ctx, one, false, 0), "...");
            assert_eq!(show_at(ctx, one, false, 2), "<<1:1>>");
        });
    }

    #[test]
    fn maps_render_with_spaces_around_the_arrow() {
        with_ctx(|ctx| {
            // `~w` of `#{1 => 2, 3 => 4}` is `#{1 => 2,3 => 4}` (oracle).
            let one = ctx.int(1).expect("fits");
            let two = ctx.int(2).expect("fits");
            let three = ctx.int(3).expect("fits");
            let four = ctx.int(4).expect("fits");
            let map = canonical_flatmap(ctx, &[(three, four), (one, two)]);
            assert_eq!(show(ctx, map, false), "#{1 => 2,3 => 4}");
            assert_eq!(show(ctx, map, true), "#{1 => 2,3 => 4}");

            // `~p` of `#{<<"k">> => 1}` is `#{<<"k">> => 1}` (printable propagates).
            let key = bits::make_binary(ctx.heap(), b"k").expect("fits");
            let map = crate::access::make_flatmap(ctx.heap(), &[(key, one)]).expect("fits");
            assert_eq!(show(ctx, map, false), "#{<<107>> => 1}");
            assert_eq!(show(ctx, map, true), "#{<<\"k\">> => 1}");

            // An empty map is `#{}`.
            let empty = ctx.atom("hi").expect("seeded");
            let map = crate::access::make_flatmap(ctx.heap(), &[]).expect("fits");
            assert_eq!(show(ctx, map, false), "#{}");
            assert_eq!(show(ctx, empty, false), "hi");
        });
    }

    #[test]
    fn atom_keys_sort_by_atom_index_and_others_by_term_order() {
        with_ctx(|ctx| {
            // Atoms `b` then `a`: `a` was seeded after `b`? The seeded order is
            // ["true","false","ok","hello","Hello","a","b","c","abc","hi"], so `a`
            // has a lower index than `b` and sorts first (oracle rule
            // `erts_cmp_flatmap_keys`: atom keys by raw term word).
            let a = ctx.atom("a").expect("seeded");
            let b = ctx.atom("b").expect("seeded");
            let one = ctx.int(1).expect("fits");
            let two = ctx.int(2).expect("fits");
            let map = canonical_flatmap(ctx, &[(b, one), (a, two)]);
            assert_eq!(show(ctx, map, false), "#{a => 2,b => 1}");

            // Mixed classes use the term order (`1` < `a` < `{a}`).
            let tuple = ctx.tuple(&[a]).expect("fits");
            let map = canonical_flatmap(ctx, &[(tuple, one), (a, two), (one, one)]);
            assert_eq!(show(ctx, map, false), "#{1 => 1,a => 2,{a} => 1}");
        });
    }

    #[test]
    fn depth_limit_matches_io_lib_write() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").expect("seeded");
            let b = ctx.atom("b").expect("seeded");
            let c = ctx.atom("c").expect("seeded");
            let inner = ctx.tuple(&[c, make_small(1)]).expect("fits");
            let mid = ctx.tuple(&[b, inner]).expect("fits");
            let outer = ctx.tuple(&[a, mid]).expect("fits");
            // `~W` of `{a,{b,{c,1}}}`: -1 ⇒ full, 4 ⇒ `{a,{b,...}}`, 3 ⇒ `{a,{...}}`,
            // 2 ⇒ `{a,...}`, 1 ⇒ `{...}`, 0 ⇒ `...`.
            assert_eq!(show_at(ctx, outer, false, -1), "{a,{b,{c,1}}}");
            assert_eq!(show_at(ctx, outer, false, 4), "{a,{b,...}}");
            assert_eq!(show_at(ctx, outer, false, 3), "{a,{...}}");
            assert_eq!(show_at(ctx, outer, false, 2), "{a,...}");
            assert_eq!(show_at(ctx, outer, false, 1), "{...}");
            assert_eq!(show_at(ctx, outer, false, 0), "...");

            // `~W` of `[1,2,3,4,5]`.
            let list = ctx
                .list(&[
                    make_small(1),
                    make_small(2),
                    make_small(3),
                    make_small(4),
                    make_small(5),
                ])
                .expect("fits");
            assert_eq!(show_at(ctx, list, false, 3), "[1,2|...]");
            assert_eq!(show_at(ctx, list, false, 2), "[1|...]");
            assert_eq!(show_at(ctx, list, false, 1), "[...]");
            assert_eq!(show_at(ctx, list, false, 6), "[1,2,3,4,5]");
            assert_eq!(show_at(ctx, NIL, false, 1), "[]");
        });
    }

    #[test]
    fn binary_and_map_depth_limit() {
        with_ctx(|ctx| {
            let bin = bits::make_binary(ctx.heap(), &[1, 2, 3]).expect("fits");
            assert_eq!(show_at(ctx, bin, false, 1), "<<...>>");
            assert_eq!(show_at(ctx, bin, false, 2), "<<1,2,3>>");
            // A printable binary at depth 1 is still elided (oracle).
            let abc = bits::make_binary(ctx.heap(), b"abc").expect("fits");
            assert_eq!(show_at(ctx, abc, true, 1), "<<...>>");

            // `~W` of `#{a=>1,b=>2,c=>3}`: 1 ⇒ `#{}`, 2 ⇒ first pair then `,...`,
            // 3 ⇒ two pairs then `,...`.
            let a = ctx.atom("a").expect("seeded");
            let b = ctx.atom("b").expect("seeded");
            let c = ctx.atom("c").expect("seeded");
            let map = crate::access::make_flatmap(
                ctx.heap(),
                &[(a, make_small(1)), (b, make_small(2)), (c, make_small(3))],
            )
            .expect("fits");
            assert_eq!(show_at(ctx, map, false, 1), "#{}");
            assert_eq!(show_at(ctx, map, false, 2), "#{a => 1,...}");
            assert_eq!(show_at(ctx, map, false, 3), "#{a => 1,b => 2,...}");
        });
    }

    #[test]
    fn bignums_render_in_decimal() {
        with_ctx(|ctx| {
            // 123456789012345678901234567890 = 0x18EE90FF6C373E0EE4E3F0AD2.
            // `erl -noshell -eval 'io:format("~w~n",[123456789012345678901234567890])'`
            let value = 123456789012345678901234567890u128;
            let low = value as u64;
            let high = (value >> 64) as u64;
            let big = beam_codec::term::make_big(ctx.heap(), false, &[low, high]).expect("fits");
            assert_eq!(show(ctx, big, false), "123456789012345678901234567890");
            assert_eq!(show(ctx, big, true), "123456789012345678901234567890");

            // A negative bignum keeps the sign.
            let neg = beam_codec::term::make_big(ctx.heap(), true, &[low, high]).expect("fits");
            assert_eq!(show(ctx, neg, false), "-123456789012345678901234567890");

            // 2**128 (a single high digit) — `erl` prints `340282366920938463463374607431768211456`.
            let pow = beam_codec::term::make_big(ctx.heap(), false, &[0, 0, 1]).expect("fits");
            assert_eq!(show(ctx, pow, false), "340282366920938463463374607431768211456");
        });
    }

    #[test]
    fn big_decimal_conversion_edge_cases() {
        assert_eq!(format_big_decimal(false, &[0]), "0");
        assert_eq!(format_big_decimal(false, &[9]), "9");
        assert_eq!(format_big_decimal(false, &[10, 0]), "10");
        assert_eq!(format_big_decimal(true, &[255]), "-255");
        // 2**64 = 18446744073709551616.
        assert_eq!(format_big_decimal(false, &[0, 1]), "18446744073709551616");
    }

    #[test]
    fn internal_refs_render_with_the_ref_shape() {
        with_ctx(|ctx| {
            let reference = crate::access::make_ref_thing(ctx.heap(), 42).expect("fits");
            assert_eq!(show(ctx, reference, false), "#Ref<0.0.0.42>");
            assert_eq!(show(ctx, reference, true), "#Ref<0.0.0.42>");

            let pid = ProcessId::new(10, 0).to_eterm();
            assert_eq!(show(ctx, pid, false), "<0.10.0>");
        });
    }

    #[test]
    fn bare_atom_name_rule() {
        assert!(is_bare_atom("a"));
        assert!(is_bare_atom("hello"));
        assert!(is_bare_atom("x1_@z"));
        assert!(is_bare_atom("foo@bar"));
        assert!(!is_bare_atom(""));
        assert!(!is_bare_atom("Foo"));
        assert!(!is_bare_atom("_foo"));
        assert!(!is_bare_atom("1x"));
        assert!(!is_bare_atom("a-b"));
        assert_eq!(format_atom("_foo"), "'_foo'");
        assert_eq!(format_atom(""), "''");
    }

    #[test]
    fn proper_list_collects_or_rejects() {
        with_ctx(|ctx| {
            assert_eq!(proper_list(NIL).expect("nil is a proper list").len(), 0);
            let list = ctx
                .list(&[make_small(1), make_small(2)])
                .expect("fits");
            assert_eq!(proper_list(list).expect("proper").len(), 2);
            // An improper list and a non-list are both rejected.
            let improper = ctx.cons(make_small(1), make_small(2)).expect("fits");
            assert!(proper_list(improper).is_none());
            assert!(proper_list(make_small(3)).is_none());
        });
    }

    #[test]
    fn unsupported_terms_are_badarg() {
        with_ctx(|ctx| {
            // An external pid is outside the M1 printable set.
            let node = ctx.atom("hi").expect("seeded");
            let external = beam_codec::term::make_external_pid(ctx.heap(), node, 1, 2, 3)
                .expect("fits");
            assert!(matches!(
                format_term(ctx, external, false),
                Err(beam_common::BeamError::BadArg)
            ));
            // Sanity: the atom itself still renders.
            assert_eq!(show(ctx, node, false), "hi");
        });
    }
}
