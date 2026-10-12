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
//!
//! **Totality.** The oracle recurses once per *element* of a list/tuple/map
//! (`print_length_list1/7`, `print_length_tuple1/7`, `print_length_map_pairs/8`)
//! and once per element again in the `pp` walk (`pp_tail/10`,
//! `pp_pairs_tail/9`). Rust does **not** flatten those: each recursive call is
//! followed by a `prepend`/`cons` that splices the step's own items in front of the
//! result, so the walk is not a tail call and a long-but-valid container used to
//! abort the process with a native stack overflow (`fatal runtime error: stack
//! overflow`, SIGABRT). Those five helpers are therefore written **iteratively** —
//! one stack frame per *container* — while producing the very same [`Seq`]/[`Chunk`]
//! (element order, tail, `Len`/`Dots` and chunk nesting are unchanged).
//! `cind_tail/7`/`cind_pairs_tail/7` (the [`cind`] submodule) recurse in *tail*
//! position and are left as they are. Deep **nesting** still recurses through
//! [`build`]/[`pp`]; that is a separate (unbounded-depth) concern.

use beam_common::Result;
use beam_term::{signed_value, Eterm};

use crate::access::{
    car, cdr, is_atom, is_list, is_map, is_nil, is_small, is_tuple, tuple_element, tuple_size,
};
use crate::bits;
use crate::context::BifContext;

use super::{
    char_printable, dup_chars, latin1_printable_char, printable_string, proper_list, quoted_string,
    term_chunk, Chunk, UNLIMITED_DEPTH,
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
    let ll = line_length as i64;
    let col = column.max(1) as i64;
    if ll == 0 || depth == 0 {
        // `print/10`'s `Ll =:= 0` and `D =:= 0` clauses.
        return term_chunk(ctx, term, printable, depth);
    }
    if depth < 0 && !is_reflowable(term) {
        // A printable string/binary leaf has no line break of its own — but only
        // while the depth is unlimited: a depth-limited `~P` truncates the leaf
        // (`~P <<"abcdef">> 2` is `<<"abcd"...>>`), which only [`build`] knows.
        return term_chunk(ctx, term, printable, depth);
    }
    // The single-line rendering is produced **only** by the two clauses above, so it
    // is rendered there rather than eagerly for every call: the flat writer walks a
    // list/tuple once per element (`crate::print`'s `render_tail_chunk`), which is
    // pure waste for a term this printer is about to re-flow — and it used to be the
    // first thing to exhaust the stack on a long container, before this port's own
    // walk even started.
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
/// As [`term_chunk`]: a term outside the Milestone-1 printable set.
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
        if beam_term::map::map_size(term, ctx.atoms())? == 0 {
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
///
/// **Iterative** (the recursion is per *element*, so a long list is walked on one
/// stack frame; see the module docs on totality). The oracle's `[Elem | Rest]` is
/// reproduced by pushing each element's template and carrying the last step's tail —
/// exactly what `prepend(elem, inner)` did, since every recursive step prepends one
/// element and only the final base case supplies the tail.
fn list1(
    ctx: &mut BifContext<'_>,
    mut term: Eterm,
    printable: bool,
    mut d: i64,
    mut t: i64,
) -> Result<Seq> {
    let mut elems: Vec<Node> = Vec::new();
    while !is_nil(term) {
        if d == 1 || t == 0 {
            return Ok(Seq {
                elems,
                tail: SeqTail::Dots,
            });
        }
        if !is_list(term) {
            // The oracle's last clause: the recursion's value *is* the tail's
            // template.
            return Ok(Seq {
                elems,
                tail: SeqTail::Improper(Box::new(build(ctx, term, printable, d - 1, t)?)),
            });
        }
        let rest = cdr(term);
        let t1 = if is_nil(rest) { t } else { tsub(t, 1) };
        let elem = build(ctx, car(term), printable, d - 1, t1)?;
        t = tsub(t1, elem.len);
        d -= 1;
        elems.push(elem);
        term = rest;
    }
    Ok(Seq {
        elems,
        tail: SeqTail::None,
    })
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
/// (1-based, like `element/2`). **Iterative**, like [`list1`].
fn tuple1(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    printable: bool,
    mut i: usize,
    mut d: i64,
    mut t: i64,
) -> Result<Seq> {
    let size = tuple_size(term);
    let mut elems: Vec<Node> = Vec::new();
    while i <= size {
        if d == 1 || t == 0 {
            return Ok(Seq {
                elems,
                tail: SeqTail::Dots,
            });
        }
        let t1 = if i == size { t } else { tsub(t, 1) };
        let elem = build(ctx, tuple_element(term, i - 1), printable, d - 1, t1)?;
        t = tsub(t1, elem.len);
        d -= 1;
        elems.push(elem);
        i += 1;
    }
    Ok(Seq {
        elems,
        tail: SeqTail::None,
    })
}
/// `io_lib_pretty:print_length_map/7` (the `D =:= 1` elision is [`build`]'s):
/// the pairs in the map's storage order (see the module docs).
fn build_map(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    printable: bool,
    d: i64,
    t: i64,
) -> Result<Node> {
    let pairs: Vec<(Eterm, Eterm)> = beam_term::map::map_pairs_storage_order(term)?;
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
/// and `d0` (the depth the keys/values themselves are rendered at). **Iterative**,
/// like [`list1`]; `d0` is carried unchanged.
fn map1(
    ctx: &mut BifContext<'_>,
    pairs: &[(Eterm, Eterm)],
    printable: bool,
    mut i: usize,
    mut d: i64,
    d0: i64,
    mut t: i64,
) -> Result<Seq> {
    let mut elems: Vec<Node> = Vec::new();
    while i < pairs.len() {
        if d == 1 || t == 0 {
            return Ok(Seq {
                elems,
                tail: SeqTail::Dots,
            });
        }
        let t1 = if i + 1 == pairs.len() { t } else { tsub(t, 1) };
        let (key, value) = pairs[i];
        let pair = map_pair(ctx, key, value, printable, d0, t1)?;
        t = tsub(t1, pair.len);
        d -= 1;
        elems.push(pair);
        i += 1;
    }
    Ok(Seq {
        elems,
        tail: SeqTail::None,
    })
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
///
/// **Iterative** (the recursion is per *element*, so a long container is rendered on
/// one stack frame; see the module docs on totality). The oracle conses each step's
/// own items onto the recursive result (`[$\,, write(E) | pp_tail(…)]`), i.e. it
/// splices them in front of it, so appending them in walk order to one item vector
/// is the very same chunk sequence.
#[allow(clippy::too_many_arguments)]
fn pp_tail(
    elems: &[Node],
    tail: &SeqTail,
    col0: i64,
    mut col: i64,
    ll: i64,
    m: i64,
    t_ind: i64,
    ind: &Chunk,
    ld: i64,
    sep: char,
    mut w: i64,
) -> Chunk {
    let mut out: Vec<Chunk> = Vec::new();
    let mut rest = elems;
    loop {
        match rest.split_first() {
            None => {
                match tail {
                    SeqTail::None => {}
                    // `pp_tail({dots,…}, …, S, _W) -> [S | "..."]` — a splice.
                    SeqTail::Dots => out.extend([sep, '.', '.', '.'].map(Chunk::Char)),
                    SeqTail::Improper(t) => out.extend(
                        pp_improper(t, col0, col, ll, m, t_ind, ind, ld, sep, w).into_items(),
                    ),
                }
                return Chunk::List(out);
            }
            Some((e, more)) => {
                let ld1 = last_depth(!more.is_empty(), ld);
                let e_len = 1 + e.len;
                let flat = if ld1 == 0 {
                    e_len + 1 < ll - col && w + e_len + 1 <= m && atomic(e)
                } else {
                    e_len < ll - col - ld1 && w + e_len + ld1 <= m && atomic(e)
                };
                if flat {
                    out.push(Chunk::Char(','));
                    out.push(write(e));
                    col += e_len;
                    w += e_len;
                } else {
                    let (es, we) = pp_element(e, col0, ll, m, t_ind, ind, ld1, 0);
                    out.push(Chunk::Char(','));
                    out.push(Chunk::Char('\n'));
                    out.push(ind.clone());
                    out.push(es);
                    col = col0 + we;
                    w = we;
                }
                rest = more;
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
/// `io_lib_pretty:pp_pairs_tail/9`. **Iterative**, like [`pp_tail`].
#[allow(clippy::too_many_arguments)]
fn pp_pairs_tail(
    ps: &[Node],
    tail: &SeqTail,
    col0: i64,
    mut col: i64,
    ll: i64,
    m: i64,
    t_ind: i64,
    ind: &Chunk,
    ld: i64,
    mut w: i64,
) -> Chunk {
    let mut out: Vec<Chunk> = Vec::new();
    let mut rest = ps;
    loop {
        match rest.split_first() {
            None => {
                if matches!(tail, SeqTail::Dots) {
                    out.push(Chunk::text(",..."));
                }
                return Chunk::List(out);
            }
            Some((p, more)) => {
                let ld1 = last_depth(!more.is_empty(), ld);
                let e_len = 1 + p.len;
                let flat = if ld1 == 0 {
                    e_len + 1 < ll - col && w + e_len + 1 <= m && atomic_pair(p)
                } else {
                    e_len < ll - col - ld1 && w + e_len + ld1 <= m && atomic_pair(p)
                };
                if flat {
                    out.push(Chunk::Char(','));
                    out.push(write_pair(p));
                    col += e_len;
                    w += e_len;
                } else {
                    let (ps_chunk, pw) = pp_pair(p, col0, col0, ll, m, t_ind, ind, ld1, 0);
                    out.push(Chunk::Char(','));
                    out.push(Chunk::Char('\n'));
                    out.push(ind.clone());
                    out.push(ps_chunk);
                    col = col0 + pw;
                    w = pw;
                }
                rest = more;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BytesSink;
    use beam_alloc::Heap;
    use beam_process::ProcessId;
    use beam_term::AtomTable;

    /// The `~p` line length `io:format/2` defaults to (column 1, `Ld = 0`).
    const LL: i64 = 80;
    /// How many elements the long-container tests render. The pre-fix per-element
    /// recursion needs **megabytes** of stack at this count (measured: the built
    /// runtime aborts `io:format("~p", [lists:seq(1,25_000)])` on the default 8 MiB
    /// main stack at `opt-level=1`), so it overflows the 256 KiB thread below many
    /// times over.
    const LONG: i64 = 20_000;

    fn seeded_atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok"]);
        atoms
    }

    /// The oracle's `~p` line-fill of a container whose elements are all atomic,
    /// written out **independently** of the printer (so a long container's exact
    /// bytes can be expected without a ~100 KB golden literal).
    ///
    /// This is `io_lib_pretty:pp_tail/10`/`pp_pairs_tail/9`'s greedy rule: the first
    /// element starts at `col0 = len(open) + 1`; every later one is appended after a
    /// `,` while it still fits (`Off + 1 < LL - Col` for a non-final element and
    /// `Off < LL - Col - 1` for the final one, `Ld = 0`/`1`), else after `,\n` plus
    /// the container's continuation indent, which resets the column to
    /// `col0 + len(elem)`. The `W + Off + Ld =< M` half of the oracle's condition is
    /// dropped: `M` is the whole template's flat length and `W` a position inside it,
    /// and `pp` is only reached when `Len >= LL - Col`.
    ///
    /// Anchored to the OTP oracle by `the_fill_generator_matches_the_pinned_goldens`.
    fn expected_fill(open: &str, close: &str, indent: &str, elems: &[String]) -> String {
        let col0 = open.chars().count() as i64 + 1;
        let mut col = col0;
        let mut out = String::from(open);
        for (i, e) in elems.iter().enumerate() {
            let elen = e.chars().count() as i64;
            if i > 0 {
                let off = 1 + elen;
                let flat = if i + 1 == elems.len() {
                    off < LL - col - 1
                } else {
                    off + 1 < LL - col
                };
                out.push(',');
                if flat {
                    col += off;
                } else {
                    out.push('\n');
                    out.push_str(indent);
                    col = col0 + elen;
                }
            } else {
                col += elen;
            }
            out.push_str(e);
        }
        out.push_str(close);
        out
    }

    /// The integer texts `1..=n`.
    fn int_texts(n: i64) -> Vec<String> {
        (1..=n).map(|i| i.to_string()).collect()
    }

    /// Renders the term `build` produces under `~p` and returns the captured bytes.
    ///
    /// The render runs on a **deliberately tiny 256 KiB stack** (the ambient 8 MiB
    /// one hides a per-element recursion), so a passing call pins the iterative walk:
    /// before the fix this aborts the whole test process with
    /// `fatal runtime error: stack overflow` (SIGABRT).
    fn render_p_on_a_tiny_stack(
        build: impl FnOnce(&mut BifContext<'_>, &crate::BifRegistry) -> Result<Eterm> + Send + 'static,
    ) -> String {
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                let reg = crate::default_registry();
                let mut heap = Heap::new(1 << 22).expect("test heap");
                let atoms = seeded_atoms();
                let mut sink = BytesSink::new();
                {
                    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
                        .with_output(&mut sink);
                    let format = ctx.list(&[ctx.int(126).unwrap(), ctx.int(112).unwrap()]).unwrap();
                    let term = build(&mut ctx, &reg).expect("the term builds");
                    let args = ctx.list(&[term]).expect("one argument fits");
                    let rendered = reg.call("io", "format", 2, &mut ctx, &[format, args]);
                    assert!(rendered.is_ok(), "~p renders: {rendered:?}");
                }
                String::from_utf8(sink.contents().to_vec()).expect("ASCII")
            })
            .expect("spawn")
            .join()
            .expect("the render must not overflow the 256 KiB stack")
    }

    #[test]
    fn the_fill_generator_matches_the_pinned_goldens() {
        // The generator is checked against two goldens `tests/pretty_print.rs` pins
        // from the OTP oracle, so the long renders below are expected against the
        // oracle's own line-fill rule rather than against this printer.
        assert_eq!(
            expected_fill("[", "]", " ", &int_texts(29)),
            "[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,\n 29]"
        );
        let pairs: Vec<String> = (1..=20).map(|i| format!("{i} => {i}")).collect();
        assert_eq!(
            expected_fill("#{", "}", "  ", &pairs),
            "#{1 => 1,2 => 2,3 => 3,4 => 4,5 => 5,6 => 6,7 => 7,8 => 8,9 => 9,10 => 10,\n  \
             11 => 11,12 => 12,13 => 13,14 => 14,15 => 15,16 => 16,17 => 17,18 => 18,\n  \
             19 => 19,20 => 20}"
        );
    }

    #[test]
    fn a_long_list_is_walked_iteratively() {
        let out = render_p_on_a_tiny_stack(|ctx, _| {
            let elems: Vec<Eterm> = (1..=LONG).map(|i| ctx.int(i)).collect::<Result<Vec<_>>>()?;
            ctx.list(&elems)
        });
        assert_eq!(out, expected_fill("[", "]", " ", &int_texts(LONG)));
    }

    #[test]
    fn a_long_tuple_is_walked_iteratively() {
        let out = render_p_on_a_tiny_stack(|ctx, _| {
            let elems: Vec<Eterm> = (1..=LONG).map(|i| ctx.int(i)).collect::<Result<Vec<_>>>()?;
            ctx.tuple(&elems)
        });
        assert_eq!(out, expected_fill("{", "}", " ", &int_texts(LONG)));
    }

    #[test]
    fn a_long_map_is_walked_iteratively() {
        let (out, elems) = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| {
                let reg = crate::default_registry();
                let mut heap = Heap::new(1 << 23).expect("test heap");
                let atoms = seeded_atoms();
                let mut sink = BytesSink::new();
                let elems: Vec<String>;
                {
                    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
                        .with_output(&mut sink);
                    let pairs: Vec<Eterm> = (1..=LONG)
                        .map(|i| {
                            let k = ctx.int(i)?;
                            let v = ctx.int(i)?;
                            ctx.tuple(&[k, v])
                        })
                        .collect::<Result<Vec<_>>>()
                        .expect("the pairs build");
                    let list = ctx.list(&pairs).expect("the pair list builds");
                    let map = reg
                        .call("maps", "from_list", 1, &mut ctx, &[list])
                        .expect("the map builds");
                    // The element texts in the map's **storage** order — the order
                    // `build_map`/`pp_map` walk and `io_lib_pretty` renders.
                    elems = beam_term::map::map_pairs_storage_order(map)
                        .expect("a HAMT's storage order")
                        .iter()
                        .map(|(k, v)| format!("{} => {}", signed_value(*k), signed_value(*v)))
                        .collect();
                    let format = ctx
                        .list(&[ctx.int(126).unwrap(), ctx.int(112).unwrap()])
                        .unwrap();
                    let args = ctx.list(&[map]).expect("one argument fits");
                    let rendered = reg.call("io", "format", 2, &mut ctx, &[format, args]);
                    assert!(rendered.is_ok(), "~p renders: {rendered:?}");
                }
                (String::from_utf8(sink.contents().to_vec()).expect("ASCII"), elems)
            })
            .expect("spawn")
            .join()
            .expect("the render must not overflow the 256 KiB stack");
        assert_eq!(elems.len(), LONG as usize);
        assert_eq!(out, expected_fill("#{", "}", "  ", &elems));
    }
}
