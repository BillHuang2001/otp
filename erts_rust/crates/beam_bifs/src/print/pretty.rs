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

//! The `io_lib_pretty` line re-flow used by the `~p`/`~P` directives.
//!
//! A safe submodule of `crate::print` (it registers no BIFs) that ports
//! `io_lib_pretty`'s template model — `print_length/7` (`build`), `write/1`
//! (`write`) and `pp/8` (`pp`) — so a container whose single-line rendering is at
//! least `line_length - column` characters is broken across lines exactly like the
//! oracle. The leaf term renderer it re-flows stays in the parent module;
//! [`pretty_chunk`] is the entry point `crate::io` calls, re-exported from
//! `crate::print` so the rest of the crate is unaffected. The `cind/7` indentation
//! probe lives in the [`cind`] submodule.
//!
//! See the parent module docs (`crate::print`) for the precise rule, the term
//! classes and the narrowest repro.

use beam_common::Result;
use beam_term::{signed_value, Eterm};

use crate::access::{
    car, cdr, flatmap_pairs, is_atom, is_list, is_map, is_nil, is_small, is_tuple, tuple_element,
    tuple_size,
};
use crate::bits;
use crate::context::BifContext;

use super::{
    char_printable, dup_chars, flatmap_key_cmp, latin1_printable_char, printable_string,
    proper_list, quoted_string, term_chunk, Chunk, UNLIMITED_DEPTH,
};

mod cind;
use cind::while_fail_indent;
/// Whether the oracle's `io_lib_pretty` may break `term` across lines: a tuple, a
/// map, a **non-printable** list, or a **non-printable** bitstring (`print_length/7`
/// gives those a container intermediate; a printable string or binary is written on
/// one line however long).
pub(crate) fn is_reflowable(term: Eterm) -> bool {
    if is_tuple(term) || is_map(term) {
        return true;
    }
    if bits::is_bitstring(term) {
        return !binary_printable(term);
    }
    if is_list(term) {
        return !list_printable(term);
    }
    false
}

/// Whether `term` is a non-empty proper list of printable latin1 characters (the
/// printable-string rule; see [`char_printable`]).
fn list_printable(term: Eterm) -> bool {
    match proper_list(term) {
        Some(elems) if !elems.is_empty() => {
            elems.iter().all(|&e| is_small(e) && char_printable(signed_value(e)))
        }
        _ => false,
    }
}

/// Whether `term` is a non-empty binary whose every byte is printable latin1.
fn binary_printable(term: Eterm) -> bool {
    match bits::bytes(term) {
        Ok(bytes) if !bytes.is_empty() => bytes.iter().all(|&b| latin1_printable_char(b)),
        _ => false,
    }
}

/// Renders `term` for a `~p`/`~P` directive: the flat single-line form when it fits
/// `line_length - column`, else the `io_lib_pretty:print/10` line break.
///
/// This is a faithful port of `io_lib_pretty`'s template model: [`build`] is
/// `print_length/7` (it computes each node's `Len`/`Dots` without rendering),
/// [`write`] is `write/1` (the single-line form), [`pp`] is `pp/8` (the broken
/// form) and [`cind`] is `cind/7` (the tagged-tuple indentation probe). Nested
/// containers and a depth-limited `~P` are therefore rendered by the same code
/// path as the oracle; no form is refused.
///
/// # Errors
///
/// As [`term_chunk`].
pub(crate) fn pretty_chunk(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    printable: bool,
    depth: i64,
    line_length: usize,
    column: usize,
) -> Result<Chunk> {
    let flat = term_chunk(ctx, term, printable, depth)?;
    let ll = line_length as i64;
    let col = column.max(1) as i64;
    if ll == 0 || depth == 0 {
        // `print/10`'s `Ll =:= 0` and `D =:= 0` clauses.
        return Ok(flat);
    }
    if depth < 0 && !is_reflowable(term) {
        // A printable string/binary leaf has no line break of its own — but only
        // while the depth is unlimited: a depth-limited `~P` truncates the leaf
        // (`~P <<"abcdef">> 2` is `<<"abcd"...>>`), which only [`build`] knows.
        return Ok(flat);
    }
    let node = build(ctx, term, printable, depth, -1)?;
    // `M = max_cs(-1, Len) = Len`, so the oracle's `Len =< M` always holds.
    if node.len < ll - col {
        return Ok(write(&node));
    }
    let m = node.len;
    let t_ind = while_fail_indent(&node, col, ll, m);
    Ok(pp(&node, col, ll, m, t_ind, &indent_base(column), 0, 0))
}

/// `io_lib_pretty:indent/1` — the base indentation of a term starting at column
/// `col` (`chars($\s, col - 1)`; empty for column 1).
fn indent_base(col: usize) -> Chunk {
    dup_chars(' ', col.saturating_sub(1))
}
/// `io_lib_pretty:indent/2` — prepend `n` spaces to `base`.
fn indent_chunk(n: usize, base: &Chunk) -> Chunk {
    let prefix = match n {
        1 => Chunk::List(vec![Chunk::Char(' ')]),
        4 => Chunk::List(vec![dup_chars(' ', 2), dup_chars(' ', 2)]),
        _ => Chunk::List(vec![dup_chars(' ', n)]),
    };
    let mut items = prefix.into_items();
    items.extend(base.clone().into_items());
    Chunk::List(items)
}
/// The container a [`Tpl::Container`] template renders — `io_lib_pretty`'s
/// `{list, L}`, `{tuple, IsTagged, L}` and `{map, Pairs}` templates.
#[derive(Clone, Copy)]
enum Kind {
    /// `[ … ]`: elements separated by `,`, an improper tail after `|`.
    List,
    /// `{ … }`; the flag is the oracle's `IsTagged` (an atom first element of a
    /// tuple of size > 1, which `pp_tag_tuple/10` indents differently).
    Tuple(bool),
    /// `#{ … }`: `K => V` pairs separated by `,`.
    Map,
}
/// A template's "S": what the value is, without the surrounding `Len`/`Dots`
/// (`io_lib_pretty`'s `{S, Len, Dots, More}`).
enum Tpl {
    /// A chars leaf — anything `io_lib:write1/4`/`write_string/1` renders flat
    /// (atoms, numbers, pids/refs, a printable string, the printable-binary leaf).
    Text(Chunk),
    /// A container and its element sequence.
    Container(Kind, Seq),
    /// A `K => V` pair (the oracle's `{map_pair, K, V}`).
    Pair(Box<Node>, Box<Node>),
    /// A binary whose contents `pp_binary/7` repacks: the **element body** of the
    /// oracle's `{bin, S}` (`S` = `io_lib:write_binary/3`'s
    /// `[$<, $<, Body, $>, $>]`, so the `<<`/`>>` are added by [`write`] and
    /// [`pp_binary`]).
    Bin(Vec<Chunk>),
}
/// A container's element sequence: the oracle's `many` list, whose end is either
/// proper (`[]`), `{dots,3,3}` (the contents were elided) or a non-list tail
/// (`[1,2|x]`).
struct Seq {
    elems: Vec<Node>,
    tail: SeqTail,
}
/// How a [`Seq`] ends.
enum SeqTail {
    /// A proper end.
    None,
    /// `{dots,3,3}` — the elements were elided by the depth limit.
    Dots,
    /// An improper list tail: the oracle's `print_length_list1/7` last clause,
    /// whose value is the tail term's own template.
    Improper(Box<Node>),
}
impl Seq {
    /// The `[]` sequence.
    fn empty() -> Seq {
        Seq {
            elems: Vec::new(),
            tail: SeqTail::None,
        }
    }
    /// The `{dots,3,3}` sequence (an elided container's contents).
    fn dots() -> Seq {
        Seq {
            elems: Vec::new(),
            tail: SeqTail::Dots,
        }
    }
}
/// A built template — `io_lib_pretty`'s `{S, Len, Dots, More}`. The `More`
/// closure (retry with more depth, used by the `intermediate/7` chars-limit path)
/// is unreachable from this printer, which always passes `T = -1`, so it is not
/// modelled.
struct Node {
    tpl: Tpl,
    /// `Len` — the flat character count of the template's rendering.
    len: i64,
    /// `Dots` — non-zero once the rendering was truncated.
    dots: i64,
}
impl Node {
    /// A flat chars leaf.
    fn text(chunk: Chunk) -> Node {
        Node {
            len: chunk.flat_len() as i64,
            dots: 0,
            tpl: Tpl::Text(chunk),
        }
    }
    /// A flat chars leaf of literal text.
    fn leaf(text: &str) -> Node {
        Node::text(Chunk::text(text))
    }
    /// A depth-elided container leaf (`[...]`, `{...}`, `#{...}`, `<<...>>`) —
    /// the oracle's templates with `Dots = 3`.
    fn elided(text: &str) -> Node {
        Node {
            tpl: Tpl::Text(Chunk::text(text)),
            len: text.chars().count() as i64,
            dots: 3,
        }
    }
}
/// `io_lib_pretty:tsub/2` — `t` minus `e`, never crossing zero; a negative `t`
/// (the unlimited chars limit) is preserved.
fn tsub(t: i64, e: i64) -> i64 {
    if t < 0 {
        t
    } else if t >= e {
        t - e
    } else {
        0
    }
}
/// `io_lib_pretty:last_depth/2` — `0` when more elements follow, else `ld + 1`.
fn last_depth(more_follow: bool, ld: i64) -> i64 {
    if more_follow {
        0
    } else {
        ld + 1
    }
}
/// `io_lib_pretty:map_value_indent/1` — a map value indents by the tagged-tuple
/// indent when the caller found one, else by four.
fn map_value_indent(t_ind: i64) -> i64 {
    if t_ind > 0 {
        t_ind
    } else {
        4
    }
}
/// `io_lib_pretty:list_length/3` — the flat length and dots count of a sequence;
/// `acc` is what the caller already emits around it (`2` for `[`/`{` with the
/// closing bracket, `3` for `#{` and `}`).
fn list_length(seq: &Seq, acc: i64, dots: i64) -> (i64, i64) {
    let mut len = acc;
    let mut dots = dots;
    for (i, e) in seq.elems.iter().enumerate() {
        if i > 0 {
            len += 1;
        }
        len += e.len;
        dots += e.dots;
    }
    match &seq.tail {
        SeqTail::None => {}
        SeqTail::Dots => {
            // As the whole sequence the dots replace an element (`[...]`), after
            // one they follow its separator (`[1|...]`).
            len += if seq.elems.is_empty() { 3 } else { 1 + 3 };
            dots += 3;
        }
        SeqTail::Improper(t) => {
            len += 1 + t.len;
            dots += t.dots;
        }
    }
    (len, dots)
}
/// `io_lib_pretty:print_length/7` (`RecDefFun = no_fun`, `Str = true`, `T = -1`):
/// the template of `term` at depth `d` — the *shape* of the render, with its flat
/// length and dots count, and no rendering at all.
///
/// # Errors
///
/// As [`term_chunk`]: a term outside the Milestone-1 printable set, or a hash map
/// (only flat maps are modelled).
fn build(ctx: &mut BifContext<'_>, term: Eterm, printable: bool, d: i64, t: i64) -> Result<Node> {
    if is_nil(term) {
        return Ok(Node::leaf("[]"));
    }
    if is_tuple(term) {
        if tuple_size(term) == 0 {
            return Ok(Node::leaf("{}"));
        }
        if d == 1 {
            return Ok(Node::elided("{...}"));
        }
        return build_tuple(ctx, term, printable, d, t);
    }
    if is_map(term) {
        let (keys, _values) = flatmap_pairs(term)?;
        if keys.is_empty() {
            return Ok(Node::leaf("#{}"));
        }
        if d == 1 {
            return Ok(Node::elided("#{...}"));
        }
        return build_map(ctx, term, printable, d, t);
    }
    if is_list(term) {
        // `printable_list/4` is `false` at `D =:= 1`, so only a deeper printable
        // list becomes the escaped-string leaf.
        if printable && d != 1 {
            if let Some(bytes) = printable_string(term) {
                return Ok(Node::text(Chunk::text(&quoted_string(&bytes))));
            }
        }
        if d == 1 {
            return Ok(Node {
                tpl: Tpl::Container(Kind::List, Seq::dots()),
                len: 5,
                dots: 3,
            });
        }
        return build_list(ctx, term, printable, d, t);
    }
    if bits::is_bitstring(term) {
        if bits::bit_size(term)? == 0 {
            return Ok(Node::leaf("<<>>"));
        }
        if d == 1 {
            return Ok(Node::elided("<<...>>"));
        }
        return build_binary(ctx, term, printable, d, t);
    }
    // `print_length/7`'s catch-all: `io_lib:write/1`, never truncated.
    Ok(Node::text(term_chunk(ctx, term, false, UNLIMITED_DEPTH)?))
}
/// `io_lib_pretty:print_length_list/7`.
fn build_list(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    printable: bool,
    d: i64,
    t: i64,
) -> Result<Node> {
    let seq = list1(ctx, term, printable, d, tsub(t, 2))?;
    let (len, dots) = list_length(&seq, 2, 0);
    Ok(Node {
        tpl: Tpl::Container(Kind::List, seq),
        len,
        dots,
    })
}
/// `io_lib_pretty:print_length_list1/7` — the elements of a list, ending in `[]`,
/// `{dots,3,3}` or the non-list tail term's own template.
fn list1(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    printable: bool,
    d: i64,
    t: i64,
) -> Result<Seq> {
    if is_nil(term) {
        return Ok(Seq::empty());
    }
    if d == 1 || t == 0 {
        return Ok(Seq::dots());
    }
    if !is_list(term) {
        // The oracle's last clause: the recursion's value *is* the tail's
        // template.
        return Ok(Seq {
            elems: Vec::new(),
            tail: SeqTail::Improper(Box::new(build(ctx, term, printable, d - 1, t)?)),
        });
    }
    let rest = cdr(term);
    let t1 = if is_nil(rest) { t } else { tsub(t, 1) };
    let elem = build(ctx, car(term), printable, d - 1, t1)?;
    let inner = list1(ctx, rest, printable, d - 1, tsub(t1, elem.len))?;
    Ok(prepend(elem, inner))
}
/// `io_lib_pretty:print_length_tuple/7` (the `D =:= 1` elision is [`build`]'s).
fn build_tuple(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    printable: bool,
    d: i64,
    t: i64,
) -> Result<Node> {
    let seq = tuple1(ctx, term, printable, 1, d, tsub(t, 2))?;
    let (len, dots) = list_length(&seq, 2, 0);
    let tagged = tuple_size(term) > 1 && is_atom(tuple_element(term, 0));
    Ok(Node {
        tpl: Tpl::Container(Kind::Tuple(tagged), seq),
        len,
        dots,
    })
}
/// `io_lib_pretty:print_length_tuple1/7` — the tuple's elements from index `i`
/// (1-based, like `element/2`).
fn tuple1(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    printable: bool,
    i: usize,
    d: i64,
    t: i64,
) -> Result<Seq> {
    if i > tuple_size(term) {
        return Ok(Seq::empty());
    }
    if d == 1 || t == 0 {
        return Ok(Seq::dots());
    }
    let t1 = if i == tuple_size(term) { t } else { tsub(t, 1) };
    let elem = build(ctx, tuple_element(term, i - 1), printable, d - 1, t1)?;
    let inner = tuple1(ctx, term, printable, i + 1, d - 1, tsub(t1, elem.len))?;
    Ok(prepend(elem, inner))
}
/// `io_lib_pretty:print_length_map/7` (the `D =:= 1` elision is [`build`]'s):
/// the pairs in this crate's flatmap key order (see the module docs).
fn build_map(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    printable: bool,
    d: i64,
    t: i64,
) -> Result<Node> {
    let (keys, values) = flatmap_pairs(term)?;
    let mut pairs: Vec<(Eterm, Eterm)> = keys.into_iter().zip(values).collect();
    pairs.sort_by(|a, b| flatmap_key_cmp(a.0, b.0, ctx.atoms()));
    let seq = map1(ctx, &pairs, printable, 0, d, d - 1, tsub(t, 3))?;
    let (len, dots) = list_length(&seq, 3, 0);
    Ok(Node {
        tpl: Tpl::Container(Kind::Map, seq),
        len,
        dots,
    })
}
/// `io_lib_pretty:print_length_map_pairs/8` — the pairs from index `i`. The
/// oracle threads two depths: `d` (the remaining pair budget, one pair per level)
/// and `d0` (the depth the keys/values themselves are rendered at).
fn map1(
    ctx: &mut BifContext<'_>,
    pairs: &[(Eterm, Eterm)],
    printable: bool,
    i: usize,
    d: i64,
    d0: i64,
    t: i64,
) -> Result<Seq> {
    if i >= pairs.len() {
        return Ok(Seq::empty());
    }
    if d == 1 || t == 0 {
        return Ok(Seq::dots());
    }
    let t1 = if i + 1 == pairs.len() { t } else { tsub(t, 1) };
    let (key, value) = pairs[i];
    let pair = map_pair(ctx, key, value, printable, d0, t1)?;
    let inner = map1(ctx, pairs, printable, i + 1, d - 1, d0, tsub(t1, pair.len))?;
    Ok(prepend(pair, inner))
}
/// `io_lib_pretty:print_length_map_pair/8` — `K => V` (four characters of
/// separator between the halves).
fn map_pair(
    ctx: &mut BifContext<'_>,
    key: Eterm,
    value: Eterm,
    printable: bool,
    d0: i64,
    t: i64,
) -> Result<Node> {
    let k = build(ctx, key, printable, d0, t)?;
    let sep_len = k.len + 4;
    let v = build(ctx, value, printable, d0, tsub(t, sep_len))?;
    Ok(Node {
        len: sep_len + v.len,
        dots: k.dots + v.dots,
        tpl: Tpl::Pair(Box::new(k), Box::new(v)),
    })
}
/// `io_lib_pretty:print_length_binary/7` with a latin1 encoding: a printable
/// byte-aligned binary keeps the oracle's quoted-string leaf (a chars template,
/// never `{bin, S}` — which is exactly why a long printable binary is *not*
/// re-flowed), anything else becomes [`Tpl::Bin`] for `pp_binary/7` to repack.
fn build_binary(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    printable: bool,
    d: i64,
    t: i64,
) -> Result<Node> {
    if bits::bit_size(term)? % 8 != 0 {
        // A non-byte-aligned bitstring is never printable, so the oracle goes
        // straight to `io_lib:write_binary/3`.
        return Ok(Node::text(term_chunk(ctx, term, false, d)?));
    }
    let bytes = bits::bytes(term)?;
    let n = bytes.len() as i64;
    // `printable_bin0/5`: four characters per depth level, capped by the binary
    // and (when non-negative) by the chars limit.
    let d1 = d - 1;
    let mut len = if d1 >= 0 { (4 * d1).min(n) } else { n };
    if t >= 0 {
        len = len.min(t);
    }
    if printable && len > 0 {
        let k = len.min(n) as usize;
        if bytes[..k].iter().all(|&b| latin1_printable_char(b)) {
            let prefix = quoted_string(&bytes[..k]);
            let chars = prefix.chars().count() as i64;
            // `{true, List}` (the whole binary) or `{true, true, Prefix}` (a
            // printable prefix and an ellipsis). Both are chars templates — so
            // neither is ever re-flowed (a long printable binary stays on one
            // line, exactly like the oracle).
            return Ok(if len >= n {
                Node::text(Chunk::List(vec![
                    Chunk::Char('<'),
                    Chunk::Char('<'),
                    Chunk::text(&prefix),
                    Chunk::Char('>'),
                    Chunk::Char('>'),
                ]))
            } else {
                Node {
                    tpl: Tpl::Text(Chunk::List(vec![
                        Chunk::Char('<'),
                        Chunk::Char('<'),
                        Chunk::text(&prefix),
                        Chunk::text("...>>"),
                    ])),
                    len: 7 + chars,
                    dots: 3,
                }
            });
        }
    }
    // `false`: `io_lib:write_binary(Bin, D, T)`'s `{bin, S}`, `Dots = 3` when the
    // depth truncated the body (the oracle's non-empty `Rest`).
    let (body, truncated) = write_binary_body(&bytes, d);
    let len = 4 + body.iter().map(|c| c.flat_len() as i64).sum::<i64>();
    Ok(Node {
        tpl: Tpl::Bin(body),
        len,
        dots: if truncated { 3 } else { 0 },
    })
}
/// `io_lib:write_binary/3`'s body with the oracle's **element-count** depth
/// budget (`D - 1` elements at depth `D`, then `"..."`), and whether the body was
/// truncated.
fn write_binary_body(bytes: &[u8], d: i64) -> (Vec<Chunk>, bool) {
    let n = bytes.len();
    let budget = if d < 0 { n as i64 } else { (d - 1).max(0) };
    let k = (budget as usize).min(n);
    let mut body: Vec<Chunk> = Vec::with_capacity(2 * k + 2);
    for (i, byte) in bytes[..k].iter().enumerate() {
        if i > 0 {
            body.push(Chunk::Char(','));
        }
        body.push(Chunk::text(&byte.to_string()));
    }
    if k < n {
        if k > 0 {
            body.push(Chunk::Char(','));
        }
        body.push(Chunk::text("..."));
        return (body, true);
    }
    (body, false)
}
/// `prepend` an element to a sequence (the oracle's `[Elem | Rest]`).
fn prepend(elem: Node, rest: Seq) -> Seq {
    let mut elems = Vec::with_capacity(rest.elems.len() + 1);
    elems.push(elem);
    elems.extend(rest.elems);
    Seq {
        elems,
        tail: rest.tail,
    }
}
/// `io_lib_pretty:write/1` — the single-line (flat) rendering of a template.
fn write(node: &Node) -> Chunk {
    match &node.tpl {
        Tpl::Text(s) => s.clone(),
        Tpl::Bin(body) => Chunk::List(vec![
            Chunk::Char('<'),
            Chunk::Char('<'),
            Chunk::List(body.clone()),
            Chunk::Char('>'),
            Chunk::Char('>'),
        ]),
        Tpl::Pair(_, _) => write_pair(node),
        Tpl::Container(kind, seq) => match kind {
            Kind::List => Chunk::List(vec![
                Chunk::Char('['),
                write_seq(seq, '|'),
                Chunk::Char(']'),
            ]),
            Kind::Tuple(_) => Chunk::List(vec![
                Chunk::Char('{'),
                write_seq(seq, ','),
                Chunk::Char('}'),
            ]),
            // `write({{map, Pairs}, …}) -> [$#, ${, write_list(Pairs, $,), $}]`.
            Kind::Map => Chunk::List(vec![
                Chunk::Char('#'),
                Chunk::Char('{'),
                write_seq(seq, ','),
                Chunk::Char('}'),
            ]),
        },
    }
}
/// `io_lib_pretty:write_list/2` — a sequence's elements (always separated by
/// `,`), then `sep` before a terminating `...` or an improper tail.
///
/// The oracle's trailing clauses are *improper* conses — `write_tail({dots,…}, S)
/// -> [S | "..."]` and `write_tail(E, S) -> [S | write(E)]` — so the dots are three
/// individual characters and an improper tail's own rendering is spliced into the
/// sequence rather than nested (`[1,2|x]`, not `[1,2|[x]]`). Only the whole-sequence
/// `write_list({dots,…}, _S) -> "..."` is a nested chars leaf.
fn write_seq(seq: &Seq, sep: char) -> Chunk {
    if seq.elems.is_empty() && matches!(seq.tail, SeqTail::Dots) {
        // `write_list({dots,…}, _S) -> "..."` — the whole-sequence clause, whose
        // value is the nested `[46,46,46]` leaf itself (not a list wrapping it).
        return Chunk::text("...");
    }
    let mut items: Vec<Chunk> = Vec::with_capacity(2 * seq.elems.len() + 2);
    for (i, e) in seq.elems.iter().enumerate() {
        if i > 0 {
            items.push(Chunk::Char(','));
        }
        items.push(write(e));
    }
    match &seq.tail {
        SeqTail::None => {}
        SeqTail::Dots => items.extend([sep, '.', '.', '.'].map(Chunk::Char)),
        SeqTail::Improper(t) => {
            if !seq.elems.is_empty() {
                items.push(Chunk::Char(sep));
            }
            items.extend(write(t).into_items());
        }
    }
    Chunk::List(items)
}
/// `io_lib_pretty:write_pair/1`.
fn write_pair(node: &Node) -> Chunk {
    match &node.tpl {
        Tpl::Pair(k, v) => Chunk::List(vec![write(k), Chunk::text(" => "), write(v)]),
        _ => write(node),
    }
}
/// `io_lib_pretty`'s `?ATM/1` — a template that renders as a plain char list. A
/// `{bin, S}` template is **not** atomic, so a binary element is repacked by
/// `pp_binary/7` on its own line rather than written inline.
fn atomic(node: &Node) -> bool {
    matches!(node.tpl, Tpl::Text(_))
}
/// `io_lib_pretty`'s `?ATM_PAIR/1` — both halves of a `K => V` pair atomic.
fn atomic_pair(node: &Node) -> bool {
    match &node.tpl {
        Tpl::Pair(k, v) => atomic(k) && atomic(v),
        _ => false,
    }
}
/// `io_lib_pretty:pp/8` — the line-broken rendering of a template.
fn pp(node: &Node, col: i64, ll: i64, m: i64, t_ind: i64, ind: &Chunk, ld: i64, w: i64) -> Chunk {
    if node.len < ll - col - ld && node.len + w + ld <= m {
        return write(node);
    }
    match &node.tpl {
        Tpl::Text(s) => s.clone(),
        Tpl::Bin(body) => pp_binary(body, col + 2, ll, m, &indent_chunk(2, ind), ld, w),
        Tpl::Pair(_, _) => write_pair(node),
        Tpl::Container(kind, seq) => match kind {
            Kind::List => Chunk::List(vec![
                Chunk::Char('['),
                pp_list(seq, col + 1, ll, m, t_ind, &indent_chunk(1, ind), ld, '|', w + 1),
                Chunk::Char(']'),
            ]),
            Kind::Tuple(true) => Chunk::List(vec![
                Chunk::Char('{'),
                pp_tag_tuple(seq, col, ll, m, t_ind, ind, ld, w + 1),
                Chunk::Char('}'),
            ]),
            Kind::Tuple(false) => Chunk::List(vec![
                Chunk::Char('{'),
                pp_list(seq, col + 1, ll, m, t_ind, &indent_chunk(1, ind), ld, ',', w + 1),
                Chunk::Char('}'),
            ]),
            Kind::Map => Chunk::List(vec![
                Chunk::Char('#'),
                Chunk::Char('{'),
                pp_map(seq, col + 2, ll, m, t_ind, &indent_chunk(2, ind), ld, w + 1),
                Chunk::Char('}'),
            ]),
        },
    }
}
/// `[Head… | Tail]` — the oracle *conses* a recursive `pp` result onto the elements
/// it has already emitted (`[ES | pp_tail(…)]`, `[$,, write(E) | …]`,
/// `[pp(K), … | pp(V)]`), which splices that result's own elements into the list
/// instead of nesting them under one sub-list element.
fn cons(head: Vec<Chunk>, rest: Chunk) -> Chunk {
    let mut items = head;
    items.extend(rest.into_items());
    Chunk::List(items)
}
/// `io_lib_pretty:pp_list/9`.
fn pp_list(
    seq: &Seq,
    col0: i64,
    ll: i64,
    m: i64,
    t_ind: i64,
    ind: &Chunk,
    ld: i64,
    sep: char,
    w: i64,
) -> Chunk {
    pp_elems(&seq.elems, &seq.tail, col0, ll, m, t_ind, ind, ld, sep, w)
}
/// `io_lib_pretty:pp_list/9` over an already-split sequence.
#[allow(clippy::too_many_arguments)]
fn pp_elems(
    elems: &[Node],
    tail: &SeqTail,
    col0: i64,
    ll: i64,
    m: i64,
    t_ind: i64,
    ind: &Chunk,
    ld: i64,
    sep: char,
    w: i64,
) -> Chunk {
    match elems.split_first() {
        None => match tail {
            SeqTail::None => Chunk::List(Vec::new()),
            SeqTail::Dots => Chunk::text("..."),
            SeqTail::Improper(t) => pp_improper(t, col0, col0, ll, m, t_ind, ind, ld, sep, w),
        },
        Some((first, rest)) => {
            let ld_first = last_depth(!rest.is_empty() || !matches!(tail, SeqTail::None), ld);
            let (es, we) = pp_element(first, col0, ll, m, t_ind, ind, ld_first, w);
            let tail_chunk = pp_tail(
                rest,
                tail,
                col0,
                col0 + we,
                ll,
                m,
                t_ind,
                ind,
                ld,
                sep,
                w + we,
            );
            cons(vec![es], tail_chunk)
        }
    }
}
/// `io_lib_pretty:pp_tail/10` — everything after a list's (or tuple's) first
/// element, up to and including a terminating `...` or an improper tail.
#[allow(clippy::too_many_arguments)]
fn pp_tail(
    elems: &[Node],
    tail: &SeqTail,
    col0: i64,
    col: i64,
    ll: i64,
    m: i64,
    t_ind: i64,
    ind: &Chunk,
    ld: i64,
    sep: char,
    w: i64,
) -> Chunk {
    match elems.split_first() {
        None => match tail {
            SeqTail::None => Chunk::List(Vec::new()),
            // `pp_tail({dots,…}, …, S, _W) -> [S | "..."]` — a splice.
            SeqTail::Dots => Chunk::List([sep, '.', '.', '.'].map(Chunk::Char).into()),
            SeqTail::Improper(t) => pp_improper(t, col0, col, ll, m, t_ind, ind, ld, sep, w),
        },
        Some((e, rest)) => {
            let ld1 = last_depth(!rest.is_empty(), ld);
            let e_len = 1 + e.len;
            let flat = if ld1 == 0 {
                e_len + 1 < ll - col && w + e_len + 1 <= m && atomic(e)
            } else {
                e_len < ll - col - ld1 && w + e_len + ld1 <= m && atomic(e)
            };
            if flat {
                let rest_chunk = pp_tail(
                    rest,
                    tail,
                    col0,
                    col + e_len,
                    ll,
                    m,
                    t_ind,
                    ind,
                    ld,
                    sep,
                    w + e_len,
                );
                cons(vec![Chunk::Char(','), write(e)], rest_chunk)
            } else {
                let (es, we) = pp_element(e, col0, ll, m, t_ind, ind, ld1, 0);
                let rest_chunk = pp_tail(
                    rest,
                    tail,
                    col0,
                    col0 + we,
                    ll,
                    m,
                    t_ind,
                    ind,
                    ld,
                    sep,
                    we,
                );
                cons(vec![Chunk::Char(','), Chunk::Char('\n'), ind.clone(), es], rest_chunk)
            }
        }
    }
}
/// `io_lib_pretty:pp_tail/10`'s improper-tail clauses — the list ended in a
/// non-list term, which follows `sep` (or starts a new line when it does not fit).
fn pp_improper(
    t: &Node,
    col0: i64,
    col: i64,
    ll: i64,
    m: i64,
    t_ind: i64,
    ind: &Chunk,
    ld: i64,
    sep: char,
    w: i64,
) -> Chunk {
    if t.len + 1 < ll - col - (ld + 1) && t.len + 1 + w + (ld + 1) <= m && atomic(t) {
        // `[S | write(E)]` — the tail's own rendering is spliced.
        let mut items = vec![Chunk::Char(sep)];
        items.extend(write(t).into_items());
        Chunk::List(items)
    } else {
        // `[S, $\n, Ind | pp(E, …, LD + 1, 0)]` — also a splice.
        let mut items = vec![Chunk::Char(sep), Chunk::Char('\n'), ind.clone()];
        items.extend(pp(t, col0, ll, m, t_ind, ind, ld + 1, 0).into_items());
        Chunk::List(items)
    }
}
/// `io_lib_pretty:pp_element/8` — one element, flat when it fits and is atomic,
/// else broken onto its own line (the `WE = Ll` "force nl" convention).
fn pp_element(
    e: &Node,
    col: i64,
    ll: i64,
    m: i64,
    t_ind: i64,
    ind: &Chunk,
    ld: i64,
    w: i64,
) -> (Chunk, i64) {
    if e.len < ll - col - ld && e.len + w + ld <= m && atomic(e) {
        (write(e), e.len)
    } else {
        (pp(e, col, ll, m, t_ind, ind, ld, w), ll)
    }
}
/// `io_lib_pretty:pp_tag_tuple/10` — a tagged tuple's elements, indented after
/// the tag (by the tag's own width, or by the caller's `TInd` when the tag is
/// longer than it).
fn pp_tag_tuple(
    seq: &Seq,
    col: i64,
    ll: i64,
    m: i64,
    t_ind: i64,
    ind: &Chunk,
    ld: i64,
    w: i64,
) -> Chunk {
    let (tag, rest) = match seq.elems.split_first() {
        Some(split) => split,
        None => return Chunk::text("..."),
    };
    let tag_ind = tag.len + 2;
    let tcol = col + tag_ind;
    if t_ind > 0 && tag_ind > t_ind {
        let indent = indent_chunk(t_ind as usize, ind);
        let rest_chunk = pp_tail(
            rest,
            &seq.tail,
            col + t_ind,
            tcol,
            ll,
            m,
            t_ind,
            &indent,
            ld,
            ',',
            w + tag.len,
        );
        cons(vec![write(tag)], rest_chunk)
    } else {
        let indent = indent_chunk(tag_ind as usize, ind);
        let rest_chunk = pp_elems(
            rest,
            &seq.tail,
            tcol,
            ll,
            m,
            t_ind,
            &indent,
            ld,
            ',',
            w + tag.len + 1,
        );
        cons(vec![write(tag), Chunk::Char(',')], rest_chunk)
    }
}
/// `io_lib_pretty:pp_map/9`.
fn pp_map(seq: &Seq, col: i64, ll: i64, m: i64, t_ind: i64, ind: &Chunk, ld: i64, w: i64) -> Chunk {
    match seq.elems.split_first() {
        None => match &seq.tail {
            SeqTail::Dots => Chunk::text("..."),
            _ => Chunk::List(Vec::new()),
        },
        Some((p, ps)) => {
            let (ps_chunk, pw) = pp_pair(
                p,
                col,
                col,
                ll,
                m,
                t_ind,
                ind,
                last_depth(!ps.is_empty(), ld),
                w,
            );
            let rest_chunk = pp_pairs_tail(ps, &seq.tail, col, col + pw, ll, m, t_ind, ind, ld, pw);
            cons(vec![ps_chunk], rest_chunk)
        }
    }
}
/// `io_lib_pretty:pp_pairs_tail/9`.
#[allow(clippy::too_many_arguments)]
fn pp_pairs_tail(
    ps: &[Node],
    tail: &SeqTail,
    col0: i64,
    col: i64,
    ll: i64,
    m: i64,
    t_ind: i64,
    ind: &Chunk,
    ld: i64,
    w: i64,
) -> Chunk {
    match ps.split_first() {
        None => match tail {
            SeqTail::Dots => Chunk::text(",..."),
            _ => Chunk::List(Vec::new()),
        },
        Some((p, rest)) => {
            let ld1 = last_depth(!rest.is_empty(), ld);
            let e_len = 1 + p.len;
            let flat = if ld1 == 0 {
                e_len + 1 < ll - col && w + e_len + 1 <= m && atomic_pair(p)
            } else {
                e_len < ll - col - ld1 && w + e_len + ld1 <= m && atomic_pair(p)
            };
            if flat {
                let rest_chunk = pp_pairs_tail(
                    rest,
                    tail,
                    col0,
                    col + e_len,
                    ll,
                    m,
                    t_ind,
                    ind,
                    ld,
                    w + e_len,
                );
                cons(vec![Chunk::Char(','), write_pair(p)], rest_chunk)
            } else {
                let (ps_chunk, pw) = pp_pair(p, col0, col0, ll, m, t_ind, ind, ld1, 0);
                let rest_chunk = pp_pairs_tail(
                    rest,
                    tail,
                    col0,
                    col0 + pw,
                    ll,
                    m,
                    t_ind,
                    ind,
                    ld,
                    pw,
                );
                cons(vec![Chunk::Char(','), Chunk::Char('\n'), ind.clone(), ps_chunk], rest_chunk)
            }
        }
    }
}
/// `io_lib_pretty:pp_pair/9` — a `K => V` pair and the width it occupied.
fn pp_pair(
    p: &Node,
    col0: i64,
    col: i64,
    ll: i64,
    m: i64,
    t_ind: i64,
    ind0: &Chunk,
    ld: i64,
    w: i64,
) -> (Chunk, i64) {
    if p.len < ll - col - ld && p.len + w + ld <= m {
        let we = if atomic_pair(p) { p.len } else { ll };
        return (write_pair(p), we);
    }
    let (k, v) = match &p.tpl {
        Tpl::Pair(k, v) => (k.as_ref(), v.as_ref()),
        _ => return (write_pair(p), ll),
    };
    let indent = map_value_indent(t_ind);
    let ind = indent_chunk(indent as usize, ind0);
    let head = vec![
        pp(k, col0, ll, m, t_ind, ind0, ld, w),
        Chunk::text(" =>\n"),
        ind.clone(),
    ];
    (
        cons(head, pp(v, col0 + indent, ll, m, t_ind, &ind, ld, 0)),
        ll,
    )
}
/// `io_lib_pretty:pp_binary/7` — the `[LT, LT, S, GT, GT]` clause, with
/// `N = max(8, min(Ll - Col, M - 4 - W) - LD)` and the continuation indent, the
/// body repacked by [`pp_binary_body`].
fn pp_binary(body: &[Chunk], col: i64, ll: i64, m: i64, ind: &Chunk, ld: i64, w: i64) -> Chunk {
    let n = 8.max((ll - col).min(m - 4 - w) - ld);
    Chunk::List(vec![
        Chunk::Char('<'),
        Chunk::Char('<'),
        pp_binary_body(body, n, n, ind),
        Chunk::Char('>'),
        Chunk::Char('>'),
    ])
}
/// `io_lib_pretty:pp_binary/4` — emit the body's `[element, $,]` pairs (the comma
/// counts towards the pair's width and is itself an output element), breaking onto
/// a new line indented by `ind` when the pair does not fit and resetting the budget
/// to `N0 - Len`; a final comma-less element is the catch-all clause.
fn pp_binary_body(body: &[Chunk], n: i64, n0: i64, ind: &Chunk) -> Chunk {
    let mut out: Vec<Chunk> = Vec::new();
    let mut remaining = n;
    let mut i = 0;
    while i < body.len() {
        let comma = body.get(i + 1);
        let len = body[i].flat_len() as i64 + comma.map_or(0, |c| c.flat_len() as i64);
        if remaining - len < 0 {
            out.push(Chunk::Char('\n'));
            out.push(ind.clone());
            remaining = n0 - len;
        } else {
            remaining -= len;
        }
        out.push(body[i].clone());
        if let Some(c) = comma {
            out.push(c.clone());
        }
        i += 2;
    }
    Chunk::List(out)
}
