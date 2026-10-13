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
//! **Chars limit (`{chars_limit, N}`).** The template model carries the oracle's
//! fourth field as well: a [`More`] closure recording how to *re-run* the generator
//! at a deeper depth, plus the [`intermediate`]/[`find_upper`]/[`search_depth`]
//! depth search and [`expand`] that call it. [`pretty_write_limited`] is
//! `io_lib:write/5` (`~w`/`~W` with a limit) and [`pretty_chunk_limited`] is
//! `io_lib_pretty:print/10` (`~p`/`~P` with a limit). With a **negative** limit the
//! new machinery is inert and the two unlimited entry points behave byte-identically
//! to before.
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

use std::rc::Rc;

use beam_common::Result;
use beam_term::{signed_value, Eterm};

use crate::access::{
    car, cdr, is_atom, is_list, is_map, is_nil, is_small, is_tuple, tuple_element, tuple_size,
};
use crate::bits;
use crate::context::BifContext;
use crate::error::unsupported_bif;

use super::{
    char_printable, dup_chars, latin1_printable_char, printable_string, proper_list, quoted_string,
    resolve_map_pairs, term_chunk_order, Chunk, MapOrder, UNLIMITED_DEPTH,
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
/// As [`crate::print::term_chunk`].
pub(crate) fn pretty_chunk(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    printable: bool,
    depth: i64,
    line_length: usize,
    column: usize,
) -> Result<Chunk> {
    pretty_chunk_order(
        ctx,
        term,
        printable,
        depth,
        line_length,
        column,
        MapOrder::Storage,
    )
}

/// As [`pretty_chunk`], but with the map order a `~k`/`~K` modifier selected
/// ([`MapOrder`]) applied to every map at any depth.
///
/// # Errors
///
/// As [`pretty_chunk`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn pretty_chunk_order(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    printable: bool,
    depth: i64,
    line_length: usize,
    column: usize,
    order: MapOrder,
) -> Result<Chunk> {
    let ll = line_length as i64;
    let col = column.max(1) as i64;
    if ll == 0 || depth == 0 {
        // `print/10`'s `Ll =:= 0` and `D =:= 0` clauses.
        return term_chunk_order(ctx, term, printable, depth, order);
    }
    if depth < 0 && !is_reflowable(term) {
        // A printable string/binary leaf has no line break of its own — but only
        // while the depth is unlimited: a depth-limited `~P` truncates the leaf
        // (`~P <<"abcdef">> 2` is `<<"abcd"...>>`), which only [`build`] knows.
        return term_chunk_order(ctx, term, printable, depth, order);
    }
    // The single-line rendering is produced **only** by the two clauses above, so it
    // is rendered there rather than eagerly for every call: the flat writer walks a
    // list/tuple once per element (`crate::print`'s `render_tail_chunk`), which is
    // pure waste for a term this printer is about to re-flow — and it used to be the
    // first thing to exhaust the stack on a long container, before this port's own
    // walk even started.
    let node = build(ctx, term, printable, depth, -1, order)?;
    // `M = max_cs(-1, Len) = Len`, so the oracle's `Len =< M` always holds.
    if node.len < ll - col {
        return Ok(write(&node));
    }
    let m = node.len;
    let t_ind = while_fail_indent(&node, col, ll, m);
    Ok(pp(&node, col, ll, m, t_ind, &indent_base(column), 0, 0))
}

/// `io_lib:write/5` with a **non-negative** chars limit and `Str = false` — the
/// oracle's `~w`/`~W` limited path.
///
/// * `chars_limit < 0` — the unlimited `write1/4` render, i.e.
///   [`term_chunk_order`] (the `printable` flag is the caller's `Str`);
/// * `depth == 0 || chars_limit == 0` — the literal `"..."`;
/// * else — `write(intermediate(Term, Depth, CharsLimit, …, Str = false, …))`.
///
/// # Errors
///
/// As [`crate::print::term_chunk`], plus the typed gaps [`intermediate`] may report.
pub(crate) fn pretty_write_limited(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    printable: bool,
    depth: i64,
    chars_limit: i64,
    order: MapOrder,
) -> Result<Chunk> {
    if depth == 0 || chars_limit == 0 {
        return Ok(Chunk::text("..."));
    }
    if chars_limit < 0 {
        return term_chunk_order(ctx, term, printable, depth, order);
    }
    // `io_lib:write/5` passes `Str = false` unconditionally on this path.
    let node = intermediate(ctx, term, false, depth, chars_limit, order)?;
    Ok(write(&node))
}

/// `io_lib_pretty:print/10` with a **non-negative** chars limit — the oracle's
/// `~p`/`~P` limited path (`T = chars_limit`, `M0 = CHAR_MAX` collapsed to `-1`,
/// i.e. `M = Len`).
///
/// Unlike [`pretty_chunk_order`] this does **not** take the
/// `depth < 0 && !is_reflowable` shortcut: with a limit, a printable string *is*
/// truncated.
///
/// # Errors
///
/// As [`crate::print::term_chunk`], plus the typed gaps [`intermediate`] may report.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pretty_chunk_limited(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    printable: bool,
    depth: i64,
    line_length: usize,
    column: usize,
    chars_limit: i64,
    order: MapOrder,
) -> Result<Chunk> {
    let ll = line_length as i64;
    let col = column.max(1) as i64;
    if depth == 0 || chars_limit == 0 {
        return Ok(Chunk::text("..."));
    }
    if chars_limit < 0 {
        // `max_cs(CharsLimit, Len)` with a negative limit is `Len`, so the limited
        // search below is what a non-negative limit is for; a negative one is the
        // plain unlimited re-flow.
        return pretty_chunk_order(ctx, term, printable, depth, line_length, column, order);
    }
    let node = intermediate(ctx, term, printable, depth, chars_limit, order)?;
    if ll == 0 || node.len < ll - col {
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
/// A template's "S": what the value is, without the surrounding `Len`/`Dots`/
/// `More` (`io_lib_pretty`'s `{S, Len, Dots, More}`).
#[derive(Clone)]
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
#[derive(Clone)]
struct Seq {
    elems: Vec<Node>,
    tail: SeqTail,
}
/// How a [`Seq`] ends.
#[derive(Clone)]
enum SeqTail {
    /// A proper end.
    None,
    /// `{dots, 3, 3, More}` — the remaining elements were elided by the depth
    /// (or chars) budget, together with the closure that re-runs their generator.
    Dots(More),
    /// An improper list tail: the oracle's `print_length_list1/7` last clause,
    /// whose value is the tail term's own template.
    Improper(Box<Node>),
}
impl Seq {
    /// The `{dots,3,3}` sequence (an elided container's contents).
    fn dots(more: More) -> Seq {
        Seq {
            elems: Vec::new(),
            tail: SeqTail::Dots(more),
        }
    }
}
/// The oracle's `More` closure — how to re-run the template's generator at a
/// **deeper** depth (`More(T1, Dd)` regenerates at `D + Dd` with the new budget
/// `T1`).
///
/// Only two shapes are needed: the container-level re-run (the list-level
/// `print_length/7` closure and the `D =:= 1` clauses of
/// `print_length_tuple`/`print_length_map`/`print_length`'s bitstring arm, plus
/// `print_length_binary`'s own dots) all re-enter [`build`] at `d + dd`, and the
/// three sequence-level dots (`print_length_list1`/`_tuple1`/`_map_pairs`)
/// re-enter their generator directly.
#[derive(Clone)]
enum More {
    /// `no_more` — the template is already fully expanded ([`expand`] returns it
    /// unchanged).
    None,
    /// `fun(T1, Dd) -> print_length(Term, D + Dd, T1, …) end` — re-run [`build`]
    /// on `term` at `d + dd`.
    Build { term: Eterm, d: i64 },
    /// `print_length_list1(Term, D + Dd, T1, …)` — the remaining list at that point.
    List1 { term: Eterm, d: i64 },
    /// `print_length_tuple1(Tuple, I, D + Dd, T1, …)`.
    Tuple1 { term: Eterm, i: usize, d: i64 },
    /// `print_length_map_pairs(Term, D + Dd, D0, T1, …)`. Owns the pair list (an
    /// `Rc`) so the closure outlives the builder's borrow of it.
    Map1 {
        pairs: Rc<[(Eterm, Eterm)]>,
        i: usize,
        d: i64,
        d0: i64,
    },
}
/// A built template — `io_lib_pretty`'s `{S, Len, Dots, More}`.
#[derive(Clone)]
struct Node {
    tpl: Tpl,
    /// `Len` — the flat character count of the template's rendering.
    len: i64,
    /// `Dots` — non-zero once the rendering was truncated.
    dots: i64,
    /// `More` — the re-run closure (see [`More`]).
    more: More,
}
impl Node {
    /// A flat chars leaf.
    fn text(chunk: Chunk) -> Node {
        Node {
            len: chunk.flat_len() as i64,
            dots: 0,
            tpl: Tpl::Text(chunk),
            more: More::None,
        }
    }
    /// A flat chars leaf of literal text.
    fn leaf(text: &str) -> Node {
        Node::text(Chunk::text(text))
    }
    /// A depth-elided container leaf (`[...]`, `{...}`, `#{...}`, `<<...>>`) —
    /// the oracle's templates with `Dots = 3`.
    fn elided(text: &str, more: More) -> Node {
        Node {
            tpl: Tpl::Text(Chunk::text(text)),
            len: text.chars().count() as i64,
            dots: 3,
            more,
        }
    }
}
/// `io_lib_pretty:tsub/2` — `t` minus `e`, never crossing zero; a negative `t`
/// (the unlimited chars limit) is preserved.
pub(crate) fn tsub(t: i64, e: i64) -> i64 {
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
        SeqTail::Dots(_) => {
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
/// `io_lib_pretty:printable_latin1_list/2`'s trichotomy.
enum PrintablePrefix {
    /// `all` — the list was exhausted within the budget, so every element is
    /// printable.
    All,
    /// `0` — the budget ran out with elements left: the returned value is the
    /// first `N` characters.
    Prefix(usize),
    /// `_NC` (a positive remaining budget) — a non-printable element stopped the
    /// walk, so the list is not printable.
    NotPrintable,
}
/// `io_lib_pretty:printable_latin1_list/2` — the printable prefix of the first `n`
/// characters of the list `term`.
///
/// `N = 0` is the interesting case: `([_|_], 0) -> 0` (the caller truncates to
/// zero characters, and `intermediate/7` then re-enters at a deeper depth so the
/// list is rendered as `[...]` rather than a bogus empty string).
fn printable_latin1_list(term: Eterm, n: i64) -> PrintablePrefix {
    let mut cur = term;
    let mut budget = n;
    loop {
        if budget == 0 {
            // `([], _) -> all` precedes `([_|_], 0) -> 0`, so the exhausted list
            // wins; anything else (a cons, or an improper tail) takes the `0` arm.
            return if is_nil(cur) {
                PrintablePrefix::All
            } else {
                PrintablePrefix::Prefix(n as usize)
            };
        }
        if !is_list(cur) {
            return if is_nil(cur) {
                PrintablePrefix::All
            } else {
                PrintablePrefix::NotPrintable
            };
        }
        let head = car(cur);
        if !is_small(head) || !char_printable(signed_value(head)) {
            return PrintablePrefix::NotPrintable;
        }
        budget -= 1;
        cur = cdr(cur);
    }
}
/// The byte values of the first `limit` elements of the list `term` (all of which
/// the caller has already established are printable).
fn list_prefix_bytes(term: Eterm, limit: usize) -> Vec<u8> {
    // `limit` is `usize::MAX` on the `PrintablePrefix::All` arm, where the caller
    // wants *every* element: reserving `limit` words up front would ask the allocator
    // for an impossible block and abort the whole process (`capacity overflow`), so
    // only a small bounded amount is reserved and the `Vec` grows to the list's real
    // length. The result is identical for every `limit` (the loop is the same).
    let mut bytes = Vec::with_capacity(limit.min(PREFIX_RESERVE));
    let mut cur = term;
    while bytes.len() < limit && is_list(cur) {
        bytes.push(signed_value(car(cur)) as u8);
        cur = cdr(cur);
    }
    bytes
}
/// How much [`list_prefix_bytes`] reserves up front, whatever the (possibly
/// unbounded) `limit` — enough to avoid a reallocation for a typical short prefix,
/// small enough to be allocated unconditionally.
const PREFIX_RESERVE: usize = 32;
/// `io_lib_pretty:print_length/7` (`RecDefFun = no_fun`, `Str = true`): the
/// template of `term` at depth `d` and chars budget `t` (`-1` = unlimited) — the
/// *shape* of the render, with its flat length and dots count, and no rendering at
/// all.
///
/// # Errors
///
/// As [`crate::print::term_chunk`]: a term outside the Milestone-1 printable set.
fn build(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    printable: bool,
    d: i64,
    t: i64,
    order: MapOrder,
) -> Result<Node> {
    if is_nil(term) {
        return Ok(Node::leaf("[]"));
    }
    if is_tuple(term) {
        if tuple_size(term) == 0 {
            return Ok(Node::leaf("{}"));
        }
        if d == 1 {
            // `print_length_tuple(Tuple, 1, …)`.
            return Ok(Node::elided(
                "{...}",
                More::Build { term, d: 1 },
            ));
        }
        return build_tuple(ctx, term, printable, d, t, order);
    }
    if is_map(term) {
        if beam_term::map::map_size(term, ctx.atoms())? == 0 {
            return Ok(Node::leaf("#{}"));
        }
        if d == 1 {
            // `print_length_map(Map, 1, …)`.
            return Ok(Node::elided(
                "#{...}",
                More::Build { term, d: 1 },
            ));
        }
        return build_map(ctx, term, printable, d, t, order);
    }
    if is_list(term) {
        // `printable_list/4` is `false` at `D =:= 1`, so only a deeper printable
        // list becomes the escaped-string leaf.
        if printable && d != 1 {
            match printable_list(term, t) {
                Some(PrintableString::Whole(bytes)) => {
                    return Ok(Node::text(Chunk::text(&quoted_string(&bytes))));
                }
                Some(PrintableString::Truncated(bytes)) => {
                    // `{[S | "..."], 3 + Len, 0, no_more}` — `[S | "…"]` is a
                    // *proper* list whose first element is the escaped prefix.
                    let s = quoted_string(&bytes);
                    let len = 3 + s.chars().count() as i64;
                    let mut items = vec![Chunk::text(&s)];
                    items.extend([Chunk::Char('.'), Chunk::Char('.'), Chunk::Char('.')]);
                    return Ok(Node {
                        tpl: Tpl::Text(Chunk::List(items)),
                        len,
                        dots: 0,
                        more: More::None,
                    });
                }
                None => {}
            }
        }
        if d == 1 {
            // `print_length_list(List, 1, …)`: the sequence's dots carry
            // `print_length_list1`'s closure, the node the list-level one.
            return Ok(Node {
                tpl: Tpl::Container(Kind::List, Seq::dots(More::List1 { term, d: 1 })),
                len: 5,
                dots: 3,
                more: More::Build { term, d: 1 },
            });
        }
        return build_list(ctx, term, printable, d, t, order);
    }
    if bits::is_bitstring(term) {
        if bits::bit_size(term)? == 0 {
            return Ok(Node::leaf("<<>>"));
        }
        if d == 1 {
            // `print_length(<<_/bitstring>> = Bin, 1, …)`.
            return Ok(Node::elided(
                "<<...>>",
                More::Build { term, d: 1 },
            ));
        }
        return build_binary(ctx, term, printable, d, t, order);
    }
    // `print_length/7`'s catch-all: `io_lib:write/1`, never truncated.
    Ok(Node::text(term_chunk_order(
        ctx,
        term,
        false,
        UNLIMITED_DEPTH,
        order,
    )?))
}
/// `io_lib_pretty:printable_list/4` for the latin1 encoding: the whole printable
/// string, its truncation, or not a string at all.
enum PrintableString {
    /// `true` — every element is printable and the list fits the budget.
    Whole(Vec<u8>),
    /// `{true, Prefix}` — the first `tsub(T, 2)` characters, with the rest elided.
    Truncated(Vec<u8>),
}
/// `io_lib_pretty:printable_list/4` for latin1 (the `D =:= 1 -> false` clause is
/// the caller's).
fn printable_list(term: Eterm, t: i64) -> Option<PrintableString> {
    if t < 0 {
        // `printable_list(L, _D, T, latin1) when T < 0 -> io_lib:printable_latin1_list(L)`
        // — the whole list must be printable (and non-empty, like
        // [`printable_string`]).
        return printable_string(term).map(PrintableString::Whole);
    }
    // `N = tsub(T, 2)`, then the `all` / `0` / `_NC` trichotomy.
    match printable_latin1_list(term, tsub(t, 2)) {
        PrintablePrefix::All => {
            let bytes = list_prefix_bytes(term, usize::MAX);
            if bytes.is_empty() {
                // `[]` is handled before `build` ever reaches here; an empty prefix
                // can only come from a non-list, which is not a string.
                return None;
            }
            Some(PrintableString::Whole(bytes))
        }
        PrintablePrefix::Prefix(n) => Some(PrintableString::Truncated(list_prefix_bytes(term, n))),
        PrintablePrefix::NotPrintable => None,
    }
}
/// `io_lib_pretty:print_length_list/7`.
fn build_list(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    printable: bool,
    d: i64,
    t: i64,
    order: MapOrder,
) -> Result<Node> {
    let seq = list1(ctx, term, printable, d, tsub(t, 2), order)?;
    let (len, dots) = list_length(&seq, 2, 0);
    // The oracle attaches the list-level `More` **only when `Dots > 0`** (an
    // untruncated list is final).
    let more = if dots > 0 {
        More::Build { term, d }
    } else {
        More::None
    };
    Ok(Node {
        tpl: Tpl::Container(Kind::List, seq),
        len,
        dots,
        more,
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
    order: MapOrder,
) -> Result<Seq> {
    let mut elems: Vec<Node> = Vec::new();
    while !is_nil(term) {
        if d == 1 || t == 0 {
            return Ok(Seq {
                elems,
                tail: SeqTail::Dots(More::List1 { term, d }),
            });
        }
        if !is_list(term) {
            // The oracle's last clause: the recursion's value *is* the tail's
            // template.
            return Ok(Seq {
                elems,
                tail: SeqTail::Improper(Box::new(build(ctx, term, printable, d - 1, t, order)?)),
            });
        }
        let rest = cdr(term);
        let t1 = if is_nil(rest) { t } else { tsub(t, 1) };
        let elem = build(ctx, car(term), printable, d - 1, t1, order)?;
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
    order: MapOrder,
) -> Result<Node> {
    let seq = tuple1(ctx, term, printable, 1, d, tsub(t, 2), order)?;
    let (len, dots) = list_length(&seq, 2, 0);
    let tagged = tuple_size(term) > 1 && is_atom(tuple_element(term, 0));
    Ok(Node {
        tpl: Tpl::Container(Kind::Tuple(tagged), seq),
        len,
        dots,
        more: More::None,
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
    order: MapOrder,
) -> Result<Seq> {
    let size = tuple_size(term);
    let mut elems: Vec<Node> = Vec::new();
    while i <= size {
        if d == 1 || t == 0 {
            return Ok(Seq {
                elems,
                tail: SeqTail::Dots(More::Tuple1 { term, i, d }),
            });
        }
        let t1 = if i == size { t } else { tsub(t, 1) };
        let elem = build(ctx, tuple_element(term, i - 1), printable, d - 1, t1, order)?;
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
    order: MapOrder,
) -> Result<Node> {
    let pairs: Rc<[(Eterm, Eterm)]> = resolve_map_pairs(term, order, ctx.atoms())?.into();
    let seq = map1(ctx, &pairs, printable, 0, d, d - 1, tsub(t, 3), order)?;
    let (len, dots) = list_length(&seq, 3, 0);
    Ok(Node {
        tpl: Tpl::Container(Kind::Map, seq),
        len,
        dots,
        more: More::None,
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
    order: MapOrder,
) -> Result<Seq> {
    let mut elems: Vec<Node> = Vec::new();
    while i < pairs.len() {
        if d == 1 || t == 0 {
            return Ok(Seq {
                elems,
                tail: SeqTail::Dots(More::Map1 {
                    pairs: pairs.to_vec().into(),
                    i,
                    d,
                    d0,
                }),
            });
        }
        let t1 = if i + 1 == pairs.len() { t } else { tsub(t, 1) };
        let (key, value) = pairs[i];
        let pair = map_pair(ctx, key, value, printable, d0, t1, order)?;
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
    order: MapOrder,
) -> Result<Node> {
    let k = build(ctx, key, printable, d0, t, order)?;
    let sep_len = k.len + 4;
    let v = build(ctx, value, printable, d0, tsub(t, sep_len), order)?;
    Ok(Node {
        len: sep_len + v.len,
        dots: k.dots + v.dots,
        tpl: Tpl::Pair(Box::new(k), Box::new(v)),
        more: More::None,
    })
}
/// `io_lib_pretty:print_length_binary/7` with a latin1, list output: a printable
/// byte-aligned binary keeps the oracle's quoted-string leaf (a chars template,
/// never `{bin, S}` — which is exactly why a long printable binary is *not*
/// re-flowed), anything else becomes [`Tpl::Bin`] for `pp_binary/7` to repack.
fn build_binary(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    printable: bool,
    d: i64,
    t: i64,
    order: MapOrder,
) -> Result<Node> {
    if bits::bit_size(term)? % 8 != 0 {
        // A non-byte-aligned bitstring is never printable, so the oracle goes
        // straight to `io_lib:write_binary/3`.
        return Ok(Node::text(term_chunk_order(ctx, term, false, d, order)?));
    }
    let bytes = bits::bytes(term)?;
    let n = bytes.len() as i64;
    let d1 = d - 1;
    // `printable_bin0/5`: four characters per depth level, capped by the binary
    // and (when non-negative) by the chars limit `tsub(T, 6)`.
    let t6 = tsub(t, 6);
    let mut len = if d1 >= 0 { (4 * d1).min(n) } else { n };
    if t6 >= 0 {
        len = len.min(t6);
    }
    if printable && len > 0 && n > 0 {
        let window = len.min(n) as usize;
        // `printable_latin1_bin/2`: `all` (the whole window is printable), or the
        // number of leading printable bytes before a non-printable one.
        let k = bytes[..window]
            .iter()
            .take_while(|&&b| latin1_printable_char(b))
            .count();
        // `printable_bin/5`'s two accepting arms.
        let prefix_len = if k == window {
            // `all`: the whole `Len`-byte window is printable.
            if len >= n {
                // `{true, List}` — the whole binary, a quoted-string leaf.
                let prefix = quoted_string(&bytes);
                return Ok(Node::text(Chunk::List(vec![
                    Chunk::Char('<'),
                    Chunk::Char('<'),
                    Chunk::text(&prefix),
                    Chunk::Char('>'),
                    Chunk::Char('>'),
                ])));
            }
            Some(k)
        } else if d1 > 0 && k as i64 >= d1 {
            // `NC` with `D > 0, Len - NC >= D`: the printable prefix.
            Some(k)
        } else {
            None
        };
        if let Some(k) = prefix_len {
            // `{true, true, Prefix}` — `{[$<,$<,S|"...>>"], 7 + length(S), 3, More}`.
            let s = quoted_string(&bytes[..k]);
            let chars = s.chars().count() as i64;
            return Ok(Node {
                tpl: Tpl::Text(Chunk::List(vec![
                    Chunk::Char('<'),
                    Chunk::Char('<'),
                    Chunk::text(&s),
                    Chunk::Char('.'),
                    Chunk::Char('.'),
                    Chunk::Char('.'),
                    Chunk::Char('>'),
                    Chunk::Char('>'),
                ])),
                len: 7 + chars,
                dots: 3,
                more: More::Build { term, d },
            });
        }
    }
    // `false`: `io_lib:write_binary(Bin, D, T)`'s `{bin, S}`, `Dots = 3` when the
    // budget truncated the body (the oracle's non-empty `Rest`).
    let (body, truncated) = write_binary_body(&bytes, d, t);
    let len = 4 + body.iter().map(|c| c.flat_len() as i64).sum::<i64>();
    Ok(Node {
        tpl: Tpl::Bin(body),
        len,
        dots: if truncated { 3 } else { 0 },
        more: if truncated {
            More::Build { term, d }
        } else {
            More::None
        },
    })
}
/// `io_lib:write_binary/3`'s body with the oracle's **element-count depth** budget
/// (`D - 1` elements at depth `D`, then `"..."`) *and* its chars budget
/// (`tsub(T, 4)` at entry, `tsub(T, length(S) + 1)` per element) — plus whether the
/// body was truncated.
fn write_binary_body(bytes: &[u8], d: i64, t: i64) -> (Vec<Chunk>, bool) {
    let mut budget = tsub(t, 4);
    let mut body: Vec<Chunk> = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if d - i as i64 == 1 || budget == 0 {
            if i > 0 {
                body.push(Chunk::Char(','));
            }
            body.push(Chunk::text("..."));
            return (body, true);
        }
        let text = bytes[i].to_string();
        budget = tsub(budget, text.chars().count() as i64 + 1);
        if i > 0 {
            body.push(Chunk::Char(','));
        }
        body.push(Chunk::text(&text));
        i += 1;
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
    if seq.elems.is_empty() && matches!(seq.tail, SeqTail::Dots(_)) {
        // `write_list({dots,…}, _S) -> "..."` — the whole-sequence clause, whose
        // value is the nested `[46,46,46]` leaf itself (not a list wrapping it).
        return Chunk::text("...");
    }
    // `2 * n + 2` is the exact item count, but computed saturating: a `Vec` that
    // large cannot exist, so the arithmetic must never be allowed to wrap into a
    // bogus (and unreachable) reservation.
    let mut items: Vec<Chunk> =
        Vec::with_capacity(seq.elems.len().saturating_mul(2).saturating_add(2));
    for (i, e) in seq.elems.iter().enumerate() {
        if i > 0 {
            items.push(Chunk::Char(','));
        }
        items.push(write(e));
    }
    match &seq.tail {
        SeqTail::None => {}
        SeqTail::Dots(_) => items.extend([sep, '.', '.', '.'].map(Chunk::Char)),
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
            SeqTail::Dots(_) => Chunk::text("..."),
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
                    SeqTail::Dots(_) => out.extend([sep, '.', '.', '.'].map(Chunk::Char)),
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
            SeqTail::Dots(_) => Chunk::text("..."),
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
                if matches!(tail, SeqTail::Dots(_)) {
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

// ---------------------------------------------------------------------------
// The chars-limit machinery: intermediate/7, find_upper/11, search_depth/9,
// expand/3 and expand_list/4
// ---------------------------------------------------------------------------
/// `io_lib_pretty:intermediate/7` — the `Term` template whose flat rendering is as
/// close to the requested `chars_limit` as a *deeper* template can get.
///
/// `D0 = 1`: the first template is the elided one, then the depth is raised until
/// the rendering's `Len` reaches the limit (or the template stops being truncated).
///
/// # Errors
///
/// As [`build`] (the first template), plus a typed gap from [`expand`] for a shape
/// the oracle itself cannot expand.
fn intermediate(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    printable: bool,
    depth: i64,
    chars_limit: i64,
    order: MapOrder,
) -> Result<Node> {
    let d0 = 1;
    let if_ = build(ctx, term, printable, d0, chars_limit, order)?;
    if if_.dots == 0 || if_.len > chars_limit || depth == 1 {
        return Ok(if_);
    }
    let last_len = if_.len;
    find_upper(
        ctx,
        if_,
        chars_limit,
        d0,
        2,
        depth,
        printable,
        order,
        last_len,
    )
}
/// `io_lib_pretty:find_upper/11` — raise the depth geometrically until a template
/// is found whose `Len` exceeds the limit (its predecessor becomes the lower
/// bound of the binary search).
#[allow(clippy::too_many_arguments)]
fn find_upper(
    ctx: &mut BifContext<'_>,
    lower: Node,
    t: i64,
    dl: i64,
    dd: i64,
    d: i64,
    printable: bool,
    order: MapOrder,
    last_len: i64,
) -> Result<Node> {
    let dd2 = dd * 2;
    let d1 = if d < 0 { dl + dd2 } else { (dl + dd2).min(d) };
    let if_ = expand(ctx, &lower, t, d1 - dl, printable, order)?;
    if if_.dots == 0 {
        // Even if `Len > T`.
        return Ok(if_);
    }
    if if_.len == last_len {
        // Cannot happen if `print_length/7` is free of bugs.
        return Ok(if_);
    }
    if if_.len <= t && (d1 < d || d < 0) {
        let len = if_.len;
        return find_upper(ctx, if_, t, d1, dd2, d, printable, order, len);
    }
    search_depth(ctx, lower, if_, t, dl, d1, printable, order)
}
/// `io_lib_pretty:search_depth/9` — binary-search the depth whose template's `Len`
/// reaches the limit. `Lower` has `Dots > 0` and `Len =< T`; `Upper` has
/// `Dots > 0` and `Len > T`.
fn search_depth(
    ctx: &mut BifContext<'_>,
    lower: Node,
    upper: Node,
    t: i64,
    dl: i64,
    du: i64,
    printable: bool,
    order: MapOrder,
) -> Result<Node> {
    if du - dl == 1 {
        // The returned intermediate format has `Len >= T`.
        return Ok(if lower.len == t { lower } else { upper });
    }
    let d1 = (dl + du) / 2;
    let if_ = expand(ctx, &lower, t, d1 - dl, printable, order)?;
    if if_.len > t {
        // `Len` can be greater than `Upper`'s length.
        search_depth(ctx, lower, if_, t, dl, d1, printable, order)
    } else {
        search_depth(ctx, if_, upper, t, d1, du, printable, order)
    }
}
/// `io_lib_pretty:expand/3` — re-run a template's generator `Dd` levels deeper.
///
/// The identity clause comes first (`Dots =:= 0` and `no_more`), then the
/// container clauses (which expand their element sequence in place), then the
/// catch-all `More(T, Dd)`.
///
/// # Errors
///
/// The typed [`unsupported_bif`] for a `Dots > 0` template whose `More` is
/// `no_more` — a shape the oracle's own `expand/3` would crash on.
fn expand(
    ctx: &mut BifContext<'_>,
    node: &Node,
    t: i64,
    dd: i64,
    printable: bool,
    order: MapOrder,
) -> Result<Node> {
    if node.dots == 0 && matches!(node.more, More::None) {
        return Ok(node.clone());
    }
    match (&node.tpl, &node.more) {
        (Tpl::Container(kind @ Kind::Tuple(_), seq), More::None) => {
            let seq = expand_seq(ctx, seq, t, dd, printable, order, 2)?;
            let (len, dots) = list_length(&seq, 2, 0);
            Ok(Node {
                tpl: Tpl::Container(*kind, seq),
                len,
                dots,
                more: More::None,
            })
        }
        (Tpl::Container(Kind::Map, seq), More::None) => {
            let seq = expand_seq(ctx, seq, t, dd, printable, order, 3)?;
            let (len, dots) = list_length(&seq, 3, 0);
            Ok(Node {
                tpl: Tpl::Container(Kind::Map, seq),
                len,
                dots,
                more: More::None,
            })
        }
        (Tpl::Pair(k, v), More::None) => {
            let k1 = expand(ctx, k, tsub(t, 1), dd, printable, order)?;
            let kl1 = k1.len + 4;
            let v1 = expand(ctx, v, tsub(t, kl1), dd, printable, order)?;
            Ok(Node {
                len: kl1 + v1.len,
                dots: k1.dots + v1.dots,
                tpl: Tpl::Pair(Box::new(k1), Box::new(v1)),
                more: More::None,
            })
        }
        (_, More::Build { term, d }) => build(ctx, *term, printable, d + dd, t, order),
        // A sequence-level dots closure is only reached through [`expand_seq`].
        (_, More::List1 { .. } | More::Tuple1 { .. } | More::Map1 { .. }) | (_, More::None) => {
            Err(unsupported_bif())
        }
    }
}
/// `io_lib_pretty:expand_list/4` — expand each element of a sequence `Dd` levels
/// deeper, threading the chars budget exactly as the oracle does (every element but
/// a properly-terminated last one also pays for its comma).
///
/// # Errors
///
/// As [`expand`].
fn expand_seq(
    ctx: &mut BifContext<'_>,
    seq: &Seq,
    t0: i64,
    dd: i64,
    printable: bool,
    order: MapOrder,
    acc: i64,
) -> Result<Seq> {
    let mut t = tsub(t0, acc);
    let n = seq.elems.len();
    let mut elems: Vec<Node> = Vec::with_capacity(n);
    for (idx, e) in seq.elems.iter().enumerate() {
        // `T1 = case Ifs =:= [] of false -> tsub(T,1); true -> T end`: only a
        // sequence that *ends* here (no dots, no improper tail) spares the comma.
        let last = idx + 1 == n && matches!(seq.tail, SeqTail::None);
        let t1 = if last { t } else { tsub(t, 1) };
        let ex = expand(ctx, e, t1, dd, printable, order)?;
        t = tsub(t1, ex.len);
        elems.push(ex);
    }
    match &seq.tail {
        SeqTail::None => Ok(Seq {
            elems,
            tail: SeqTail::None,
        }),
        SeqTail::Improper(node) => {
            let ex = expand(ctx, node, t, dd, printable, order)?;
            Ok(Seq {
                elems,
                tail: SeqTail::Improper(Box::new(ex)),
            })
        }
        SeqTail::Dots(more) => {
            let inner = expand_dots(ctx, more, t, dd, printable, order)?;
            elems.extend(inner.elems);
            Ok(Seq {
                elems,
                tail: inner.tail,
            })
        }
    }
}
/// `expand_list/4`'s tail clause `expand_list({_,_,_,More}, T, Dd) -> More(T, Dd)`:
/// the elided remainder regenerated at `d + dd`.
///
/// # Errors
///
/// The typed [`unsupported_bif`] if the closure is not a sequence generator (it
/// always is: only [`list1`]/[`tuple1`]/[`map1`] build a `SeqTail::Dots`).
fn expand_dots(
    ctx: &mut BifContext<'_>,
    more: &More,
    t: i64,
    dd: i64,
    printable: bool,
    order: MapOrder,
) -> Result<Seq> {
    match more {
        More::List1 { term, d } => list1(ctx, *term, printable, d + dd, t, order),
        More::Tuple1 { term, i, d } => tuple1(ctx, *term, printable, *i, d + dd, t, order),
        More::Map1 { pairs, i, d, d0 } => {
            map1(ctx, pairs, printable, *i, d + dd, *d0, t, order)
        }
        More::None | More::Build { .. } => Err(unsupported_bif()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::process::Command;

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

    /// The atom table every test's [`BifContext`] borrows.
    ///
    /// `BifContext::atom/1` is a **pure lookup** (`TermNotFound` for an unseeded
    /// name) — nothing here interns on demand — so every name the tests render must
    /// be seeded up front: the three booleans/`ok` the other suites use, plus the
    /// single-letter names the `[a,b,…]` / `{a,1,…}` limited goldens render and
    /// `abcdefghij` (the `never truncated` atom).
    fn seeded_atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "true",
            "false",
            "ok",
            "a",
            "b",
            "c",
            "d",
            "e",
            "f",
            "g",
            "h",
            "i",
            "j",
            "k",
            "l",
            "abcdefghij",
        ]);
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

    // -----------------------------------------------------------------------
    // The chars-limit machinery (`{chars_limit, N}`)
    // -----------------------------------------------------------------------

    /// A `BifContext` on a fresh heap, plus the atom table it borrows.
    fn with_ctx<R>(f: impl FnOnce(&mut BifContext<'_>, &crate::BifRegistry) -> R) -> R {
        let reg = crate::default_registry();
        let mut heap = Heap::new(1 << 22).expect("test heap");
        let atoms = seeded_atoms();
        let mut sink = BytesSink::new();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
        f(&mut ctx, &reg)
    }

    /// The flattened text of a rendered chunk.
    fn flat(chunk: &Chunk) -> String {
        crate::print::flatten(std::slice::from_ref(chunk))
    }

    /// `io_lib:write(Term, Depth, latin1, undefined, CL)` — flattened.
    fn write_limited(
        ctx: &mut BifContext<'_>,
        term: Eterm,
        depth: i64,
        cl: i64,
    ) -> Result<String> {
        pretty_write_limited(ctx, term, false, depth, cl, MapOrder::Storage).map(|c| flat(&c))
    }

    /// `io_lib:format("~p", [Term], [{chars_limit, CL}])` — flattened. With a single
    /// `~p` the per-directive budget *is* `CL`, and the column starts at 1.
    fn print_limited(
        ctx: &mut BifContext<'_>,
        term: Eterm,
        cl: i64,
    ) -> Result<String> {
        pretty_chunk_limited(ctx, term, true, -1, LL as usize, 1, cl, MapOrder::Storage)
            .map(|c| flat(&c))
    }

    /// The `[1,2,3]`-style integer list `1..=n`.
    fn int_list(ctx: &mut BifContext<'_>, n: i64) -> Result<Eterm> {
        let elems: Vec<Eterm> = (1..=n).map(|i| ctx.int(i)).collect::<Result<Vec<_>>>()?;
        ctx.list(&elems)
    }

    #[test]
    fn write_limited_matches_the_pinned_goldens() {
        with_ctx(|ctx, _| {
            let list = int_list(ctx, 3).unwrap();
            let expected = [
                (0, "..."),
                (1, "[...]"),
                (3, "[...]"),
                (4, "[...]"),
                (5, "[...]"),
                (6, "[1|...]"),
                (7, "[1,2,3]"),
                (8, "[1,2,3]"),
            ];
            for (cl, want) in expected {
                assert_eq!(write_limited(ctx, list, -1, cl).unwrap(), want, "CL={cl}");
            }
        });
    }

    #[test]
    fn write_limited_seq_1_to_12_matches_the_pinned_goldens() {
        with_ctx(|ctx, _| {
            let list = int_list(ctx, 12).unwrap();
            let expected = [
                (6, "[1|...]"),
                (7, "[1|...]"),
                (8, "[1,2|...]"),
                (9, "[1,2|...]"),
                (10, "[1,2,3|...]"),
                (11, "[1,2,3|...]"),
                (12, "[1,2,3,4|...]"),
                (13, "[1,2,3,4|...]"),
                (14, "[1,2,3,4,5|...]"),
            ];
            for (cl, want) in expected {
                assert_eq!(write_limited(ctx, list, -1, cl).unwrap(), want, "CL={cl}");
            }
        });
    }

    #[test]
    fn write_limited_tuple_and_map_match_the_pinned_goldens() {
        with_ctx(|ctx, _| {
            let a = ctx.atom("a").unwrap();
            let tuple = ctx
                .tuple(&[
                    a,
                    ctx.int(1).unwrap(),
                    ctx.int(2).unwrap(),
                    ctx.int(3).unwrap(),
                    ctx.int(4).unwrap(),
                    ctx.int(5).unwrap(),
                    ctx.int(6).unwrap(),
                ])
                .unwrap();
            for (cl, want) in [
                (1, "{...}"),
                (5, "{...}"),
                (6, "{a,...}"),
                (7, "{a,...}"),
                (8, "{a,1,...}"),
                (9, "{a,1,...}"),
                (10, "{a,1,2,...}"),
                (12, "{a,1,2,3,...}"),
                (14, "{a,1,2,3,4,...}"),
            ] {
                assert_eq!(write_limited(ctx, tuple, -1, cl).unwrap(), want, "CL={cl}");
            }
        });
    }

    #[test]
    fn write_limited_map_matches_the_pinned_goldens() {
        // Integer keys **on purpose.** A flat map stores its keys in
        // `erts_cmp_flatmap_keys` order, which compares two atoms by their immediate
        // value — i.e. by atom *index* — and the oracle's atom indices are seeded per
        // VM start: the same `erl -eval` renders `#{c => 3,a => 1,…}` and
        // `#{c => 3,b => 2,…}` on different runs (`maps:to_list/1` returns
        // `[{c,3},{a,1},{b,2},{d,4}]` or `[{c,3},{b,2},{a,1},{d,4}]`). An atom-keyed
        // map therefore has **no** single oracle rendering to pin. Integers order by
        // term order, which both sides agree on.
        with_ctx(|ctx, reg| {
            let pairs: Vec<Eterm> = (1i64..=4)
                .map(|i| {
                    let k = ctx.int(i)?;
                    let v = ctx.int(i)?;
                    ctx.tuple(&[k, v])
                })
                .collect::<Result<Vec<_>>>()
                .unwrap();
            let list = ctx.list(&pairs).unwrap();
            let map = reg.call("maps", "from_list", 1, ctx, &[list]).unwrap();
            // The oracle's `maps:to_list/1` for this map is `[{1,1},{2,2},{3,3},{4,4}]`,
            // and that storage order is the one `~w`/`~p` walk.
            let storage = beam_term::map::map_pairs_storage_order(map).unwrap();
            let keys: Vec<i64> = storage.iter().map(|(k, _)| signed_value(*k)).collect();
            assert_eq!(keys, [1, 2, 3, 4], "integers order by term order");
            for (cl, want) in [
                (1, "#{...}"),
                (6, "#{...}"),
                (7, "#{1 => 1,...}"),
                (13, "#{1 => 1,...}"),
                (14, "#{1 => 1,2 => 2,...}"),
            ] {
                assert_eq!(write_limited(ctx, map, -1, cl).unwrap(), want, "CL={cl}");
            }
        });
    }

    #[test]
    fn atomics_are_never_truncated() {
        with_ctx(|ctx, _| {
            let n = ctx.int(12345).unwrap();
            assert_eq!(write_limited(ctx, n, -1, 0).unwrap(), "...");
            for cl in 1..=8 {
                assert_eq!(write_limited(ctx, n, -1, cl).unwrap(), "12345", "CL={cl}");
            }
            let atom = ctx.atom("abcdefghij").unwrap();
            for cl in 1..=8 {
                assert_eq!(write_limited(ctx, atom, -1, cl).unwrap(), "abcdefghij", "CL={cl}");
            }
        });
    }

    #[test]
    fn write_limited_binary_matches_the_pinned_goldens() {
        with_ctx(|ctx, reg| {
            let bytes = ctx
                .list(
                    &(b'a'..=b'j')
                        .map(|b| ctx.int(b as i64).unwrap())
                        .collect::<Vec<_>>(),
                )
                .unwrap();
            let bin = reg.call("erlang", "list_to_binary", 1, ctx, &[bytes]).unwrap();
            for (cl, want) in [
                (1, "<<...>>"),
                (7, "<<...>>"),
                (8, "<<97,...>>"),
                (10, "<<97,...>>"),
                (11, "<<97,98,...>>"),
                (13, "<<97,98,...>>"),
                (14, "<<97,98,99,...>>"),
            ] {
                assert_eq!(write_limited(ctx, bin, -1, cl).unwrap(), want, "CL={cl}");
            }
        });
    }

    #[test]
    fn write_limited_atom_list_matches_the_pinned_goldens() {
        with_ctx(|ctx, _| {
            let elems: Vec<Eterm> = "abcdefghijkl"
                .chars()
                .map(|c| ctx.atom(&c.to_string()).unwrap())
                .collect();
            let list = ctx.list(&elems).unwrap();
            for (cl, want) in [
                (6, "[a|...]"),
                (7, "[a|...]"),
                (8, "[a,b|...]"),
                (9, "[a,b|...]"),
                (10, "[a,b,c|...]"),
                (12, "[a,b,c,d|...]"),
                (14, "[a,b,c,d,e|...]"),
            ] {
                assert_eq!(write_limited(ctx, list, -1, cl).unwrap(), want, "CL={cl}");
            }
        });
    }

    #[test]
    fn print_limited_truncates_a_printable_string_prefix() {
        with_ctx(|ctx, _| {
            let elems: Vec<Eterm> = "hello world"
                .bytes()
                .map(|b| ctx.int(b as i64).unwrap())
                .collect();
            let list = ctx.list(&elems).unwrap();
            for (cl, want) in [
                (1, "[...]"),
                (4, "[...]"),
                (5, "\"hel\"..."),
                (6, "\"hell\"..."),
                (7, "\"hello\"..."),
                (8, "\"hello \"..."),
                (12, "\"hello worl\"..."),
                (13, "\"hello world\""),
            ] {
                assert_eq!(print_limited(ctx, list, cl).unwrap(), want, "CL={cl}");
            }
        });
    }

    #[test]
    fn print_limited_binary_prints_its_printable_prefix() {
        with_ctx(|ctx, reg| {
            let bytes = ctx
                .list(
                    &(b'a'..=b'j')
                        .map(|b| ctx.int(b as i64).unwrap())
                        .collect::<Vec<_>>(),
                )
                .unwrap();
            let bin = reg.call("erlang", "list_to_binary", 1, ctx, &[bytes]).unwrap();
            for (cl, want) in [
                (8, "<<\"ab\"...>>"),
                (9, "<<\"abc\"...>>"),
                (10, "<<\"abcd\"...>>"),
                (11, "<<\"abcd\"...>>"),
                (14, "<<\"abcdefgh\"...>>"),
                (16, "<<\"abcdefghij\">>"),
            ] {
                assert_eq!(print_limited(ctx, bin, cl).unwrap(), want, "CL={cl}");
            }
        });
    }

    #[test]
    fn print_limited_matches_write_limited_for_non_strings() {
        with_ctx(|ctx, _| {
            let list = int_list(ctx, 12).unwrap();
            let three = int_list(ctx, 3).unwrap();
            for cl in 0..=20 {
                assert_eq!(
                    print_limited(ctx, list, cl).unwrap(),
                    write_limited(ctx, list, -1, cl).unwrap(),
                    "seq(1,12) CL={cl}"
                );
                assert_eq!(
                    print_limited(ctx, three, cl).unwrap(),
                    write_limited(ctx, three, -1, cl).unwrap(),
                    "[1,2,3] CL={cl}"
                );
            }
        });
    }

    #[test]
    fn a_negative_chars_limit_is_the_unlimited_render() {
        with_ctx(|ctx, _| {
            let long = int_list(ctx, 200).unwrap();
            let unlimited = pretty_chunk_order(ctx, long, true, -1, LL as usize, 1, MapOrder::Storage)
                .unwrap();
            let limited =
                pretty_chunk_limited(ctx, long, true, -1, LL as usize, 1, -1, MapOrder::Storage)
                    .unwrap();
            assert_eq!(flat(&limited), flat(&unlimited));
            assert_eq!(
                write_limited(ctx, long, -1, -1).unwrap(),
                flat(&term_chunk_order(ctx, long, false, -1, MapOrder::Storage).unwrap())
            );
        });
    }

    /// The oracle terms the live cross-checks sweep, as `(label, Erlang source)`.
    ///
    /// The label is what the `Term|CL|Rendering` lines carry and what
    /// [`build_oracle_term`] rebuilds; the source is the expression the oracle
    /// **evaluates literally**. It must be a rendering both sides agree on
    /// *deterministically* — hence the integer-keyed map (an atom-keyed one orders by
    /// the oracle's per-VM-start atom indices; see
    /// [`write_limited_map_matches_the_pinned_goldens`]).
    ///
    /// `lists:seq(1,60)` is the **re-flow** term: its flat render is 172 characters,
    /// so once `CL` passes the 80-character line length `~p` breaks it across lines
    /// and the limited `pp/8` walk runs with a limit in force. Every other term is
    /// short enough to stay on one line at any limit.
    const ORACLE_TERMS: [(&str, &str); 9] = [
        ("[1,2,3]", "[1,2,3]"),
        ("lists:seq(1,12)", "lists:seq(1,12)"),
        ("{a,1,2,3,4,5,6}", "{a,1,2,3,4,5,6}"),
        ("#{1=>1,2=>2,3=>3,4=>4}", "#{1=>1,2=>2,3=>3,4=>4}"),
        ("12345", "12345"),
        ("[a,b,c,d,e,f,g,h,i,j,k,l]", "[a,b,c,d,e,f,g,h,i,j,k,l]"),
        ("\"hello world\"", "\"hello world\""),
        ("<<a..j>>", "list_to_binary(lists:seq($a,$j))"),
        ("lists:seq(1,60)", "lists:seq(1,60)"),
    ];
    /// The chars limits every sweep uses — far past the 80-character line length, so
    /// the sweeps cross from "truncate on one line" into "re-flow with a limit".
    const ORACLE_LIMITS: i64 = 200;
    /// How many cases one sweep compares (`ORACLE_TERMS` × `ORACLE_LIMITS + 1`), the
    /// floor both live cross-checks assert — so a sweep that quietly stops comparing
    /// anything cannot pass.
    const ORACLE_CASES: usize = ORACLE_TERMS.len() * (ORACLE_LIMITS as usize + 1);

    /// Runs `erl -noshell -eval <expr>` and returns stdout.
    ///
    /// `None` means **no oracle exists at all** — `erl` could not be spawned — in
    /// which case the caller skips loudly. An oracle that *ran and failed* is a hard
    /// panic instead: the first cut of these sweeps shipped an expression
    /// (`{ok,Tk,_}=erl_scan:string(Str++"."), {value,Term,_}=erl_eval:exprs(Tk,[])`)
    /// that made `erl -eval` exit **1 with no output for every input** — `-eval`
    /// lints its argument as a *shell command* (`erl_lint:exprs/3` →
    /// `check_command/2`), which a list literal is not — and mapping that to "no
    /// oracle" silently skipped the comparison. The terms are therefore embedded
    /// literally in the expression instead of being re-scanned.
    fn run_oracle(expr: &str) -> Option<String> {
        let erl = std::env::var("ERTS_ORACLE").unwrap_or_else(|_| "erl".to_string());
        let out = match Command::new(&erl)
            .args(["-noshell", "-eval", expr])
            .output()
        {
            Ok(out) => out,
            Err(e) => {
                eprintln!("[beam_bifs print] cannot run `{erl}`: {e}");
                eprintln!(
                    "($ERTS_ORACLE unset or not executable, and no `erl` on $PATH).\n\
                     This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP)."
                );
                return None;
            }
        };
        assert!(
            out.status.success(),
            "`{erl} -noshell -eval` failed with {}: stderr {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// The `Term|CL|Rendering` lines the oracle prints for every term and limit.
    ///
    /// `render` is the Erlang expression rendering the bound `Tm` at the bound `CL`.
    /// A re-flowed rendering spans several lines, so every newline in it is escaped to
    /// the two characters `\n` before it is printed (and escaped the same way on this
    /// side) to keep one case on one line. Any other byte — including a rendering's own
    /// backslashes — passes through untouched on both sides.
    fn oracle_sweep(render: &str) -> String {
        let terms = ORACLE_TERMS
            .iter()
            .map(|(label, source)| {
                let label = label.replace('\\', "\\\\").replace('"', "\\\"");
                format!("{{\"{label}\",{source}}}")
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "T=[{terms}],\
             lists:foreach(fun({{S,Tm}})->\
               lists:foreach(fun(CL)->\
                 R={render},\
                 io:format(\"~s|~p|~s~n\",[S,CL,\
                   lists:flatten(string:replace(R,\"\\n\",\"\\\\n\",all))])\
               end,lists:seq(0,{ORACLE_LIMITS}))\
             end,T),\
             halt(0)."
        )
    }

    /// Parses `Term|CL|Rendering` lines into a `Term|CL` → rendering map.
    ///
    /// The rendering itself may contain `|` (`[1|...]`), so only the first two
    /// separators are separators — `splitn(3, '|')`, not `split_once`.
    fn parse_sweep(golden: &str) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for line in golden.lines().filter(|l| !l.is_empty()) {
            let mut parts = line.splitn(3, '|');
            let label = parts.next().expect("a `Term` field");
            let cl = parts.next().expect("a `CL` field");
            let text = parts
                .next()
                .unwrap_or_else(|| panic!("not Term|CL|Text: {line}"));
            out.insert(format!("{label}|{cl}"), text.to_string());
        }
        out
    }

    /// The same `Term|CL` → rendering map from this crate's printer, with the same
    /// newline escaping the oracle sweep uses.
    fn ours_sweep(
        ctx: &mut BifContext<'_>,
        reg: &crate::BifRegistry,
        render: impl Fn(&mut BifContext<'_>, Eterm, i64) -> Result<String>,
    ) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for (label, _) in ORACLE_TERMS {
            let term = build_oracle_term(ctx, reg, label);
            for cl in 0..=ORACLE_LIMITS {
                let text = render(ctx, term, cl).unwrap().replace('\n', "\\n");
                out.insert(format!("{label}|{cl}"), text);
            }
        }
        out
    }

    /// Compares a whole sweep case by case and returns how many matched.
    fn compare_sweeps(golden: &str, ours: &BTreeMap<String, String>) -> usize {
        let expected = parse_sweep(golden);
        assert_eq!(
            expected.len(),
            ORACLE_CASES,
            "the oracle printed every term and limit"
        );
        assert_eq!(ours.len(), ORACLE_CASES, "the printer rendered every case");
        for (key, want) in &expected {
            let got = ours.get(key).unwrap_or_else(|| panic!("no case {key}"));
            assert_eq!(got, want, "{key}");
        }
        expected.len()
    }

    /// Builds one of [`ORACLE_TERMS`] in the crate's own terms.
    fn build_oracle_term(ctx: &mut BifContext<'_>, reg: &crate::BifRegistry, src: &str) -> Eterm {
        match src {
            "[1,2,3]" => int_list(ctx, 3).unwrap(),
            "lists:seq(1,12)" => int_list(ctx, 12).unwrap(),
            "{a,1,2,3,4,5,6}" => {
                let a = ctx.atom("a").unwrap();
                let mut elems = vec![a];
                elems.extend((1..=6).map(|i| ctx.int(i).unwrap()));
                ctx.tuple(&elems).unwrap()
            }
            "#{1=>1,2=>2,3=>3,4=>4}" => {
                let pairs: Vec<Eterm> = (1i64..=4)
                    .map(|i| {
                        let k = ctx.int(i)?;
                        let v = ctx.int(i)?;
                        ctx.tuple(&[k, v])
                    })
                    .collect::<Result<Vec<_>>>()
                    .unwrap();
                let list = ctx.list(&pairs).unwrap();
                reg.call("maps", "from_list", 1, ctx, &[list]).unwrap()
            }
            "12345" => ctx.int(12345).unwrap(),
            "[a,b,c,d,e,f,g,h,i,j,k,l]" => {
                let elems: Vec<Eterm> = "abcdefghijkl"
                    .chars()
                    .map(|c| ctx.atom(&c.to_string()).unwrap())
                    .collect();
                ctx.list(&elems).unwrap()
            }
            "\"hello world\"" => {
                let elems: Vec<Eterm> = "hello world"
                    .bytes()
                    .map(|b| ctx.int(b as i64).unwrap())
                    .collect();
                ctx.list(&elems).unwrap()
            }
            "<<a..j>>" => {
                let bytes = ctx
                    .list(
                        &(b'a'..=b'j')
                            .map(|b| ctx.int(b as i64).unwrap())
                            .collect::<Vec<_>>(),
                    )
                    .unwrap();
                reg.call("erlang", "list_to_binary", 1, ctx, &[bytes])
                    .unwrap()
            }
            "lists:seq(1,60)" => int_list(ctx, 60).unwrap(),
            other => panic!("unknown oracle term {other}"),
        }
    }

    #[test]
    fn write_limited_agrees_with_the_live_oracle() {
        let expr = oracle_sweep("io_lib:write(Tm,-1,latin1,undefined,CL)");
        let Some(golden) = run_oracle(&expr) else {
            return; // No `erl` at all — `run_oracle` has said so loudly.
        };
        let ours = with_ctx(|ctx, reg| {
            ours_sweep(ctx, reg, |ctx, term, cl| write_limited(ctx, term, -1, cl))
        });
        let checked = compare_sweeps(&golden, &ours);
        assert!(checked >= 100, "the sweep compared {checked} cases");
    }

    #[test]
    fn print_limited_agrees_with_the_live_oracle() {
        // The **`~p`** sweep this time: `io_lib:format/3` with `{chars_limit, CL}`,
        // which is the oracle's `io_lib_pretty:print/10` pipeline that
        // [`print_limited`] ports (a single directive, so the per-directive budget is
        // `CL` itself and the term starts at column 1, `MaxChars = CL`, indentation 0).
        let expr = oracle_sweep("lists:flatten(io_lib:format(\"~p\",[Tm],[{chars_limit,CL}]))");
        let Some(golden) = run_oracle(&expr) else {
            return; // No `erl` at all — `run_oracle` has said so loudly.
        };
        let ours =
            with_ctx(|ctx, reg| ours_sweep(ctx, reg, |ctx, term, cl| print_limited(ctx, term, cl)));
        let checked = compare_sweeps(&golden, &ours);
        assert!(checked >= 100, "the sweep compared {checked} cases");
    }
}
