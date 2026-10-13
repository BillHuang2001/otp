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

//! The capture renderer: the matcher's capture spans turn into the oracle's
//! `{match, …}` / bare `match` result terms.
//!
//! This is the only place in the engine where a `{capture, …}` specification
//! becomes terms. [`resolve_ids`] turns the parsed [`CapValue`] into the ordered
//! group ids the BIF must render — the same id list the matcher's `step_ids`
//! walks — and [`render`] turns the matcher's [`Captures`] into the oracle's
//! `{match, [Capture, …]}` (non-global), `{match, [[…], …]}` (global) or the bare
//! `match` atom for an empty capture set (an unknown id or name renders the
//! non-participating form, never an error).

use beam_common::Result;
use beam_term::{Eterm, NIL};

use crate::bits;
use crate::context::BifContext;
use crate::error::badarg;

use super::exec::{Captures, Subject};
use super::options::{CapId, CapType, CapValue};
use super::prog::Program;

/// The `match` atom an empty capture specification (and every successful
/// non-`nomatch` run) reports.
const MATCH: &str = "match";

/// Resolves a capture specification against the compiled program: the ordered
/// group ids the BIF must render (also the matcher's `step_ids`).
///
/// [`u32::MAX`] marks an id that can never participate — a requested subpattern
/// name no group carries. Every other shape is reproduced exactly:
///
/// * `all` → `0, 1, … ncap` (the whole match, then every group);
/// * `all_but_first` → `1, … ncap`;
/// * `first` → `0`;
/// * `none` (including the empty list `[]`) → the empty id list;
/// * `all_names` → the ids of the *named* groups in name order (group 0 is never
///   named, so it is not included; [`Program::names`] is already sorted by name
///   then group, the oracle's name-table order);
/// * a `[ids…]` list → one id per entry, in the requested order, a name looked up
///   in the program's name table.
pub(crate) fn resolve_ids(value: &CapValue, program: &Program) -> Vec<u32> {
    match value {
        CapValue::All => (0..=program.ncap).collect(),
        CapValue::AllButFirst => (1..=program.ncap).collect(),
        CapValue::First => vec![0],
        CapValue::None => Vec::new(),
        CapValue::AllNames => program.names.iter().map(|(_, group)| *group).collect(),
        CapValue::Names(ids) => ids.iter().map(|id| resolve_one(id, program)).collect(),
    }
}

/// One entry of a `{capture, [ids…]}` list: an index is used as-is (an
/// out-of-range one renders non-participating later, not an error), a name is
/// looked up in the program's name table — [`u32::MAX`] when no group has it.
fn resolve_one(id: &CapId, program: &Program) -> u32 {
    match id {
        CapId::Index(index) => *index,
        CapId::Name(name) => program
            .names
            .iter()
            .find(|(candidate, _)| candidate == name)
            .map_or(u32::MAX, |(_, group)| *group),
    }
}

/// Renders the complete result term for a whole `re:run` call: the bare atom
/// `match`, or `{match, …}` (non-global) / `{match, [[…], …]}` (global).
///
/// An empty `ids` (a `none`/`[]` specification, or an `all_names` that resolved
/// to no names) is the bare `match` atom in **both** modes, regardless of the
/// matches — the oracle's `re:run("abab","a",[global,{capture,none}])` is
/// `match`. Otherwise every match contributes one list holding exactly the
/// resolved ids' values, in order: the non-participating form (`{-1,0}`, `[]`,
/// `<<>>`) for a group that did not participate, an unknown name or an
/// out-of-range index — never an error.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `ids` is non-empty
/// but `matches` is empty: a `re:run` with no match reports the bare `nomatch`
/// and never reaches the renderer, so rendering the empty set would fabricate a
/// result the oracle never produces. Heap exhaustion propagates from the
/// term constructors.
pub(crate) fn render(
    ctx: &mut BifContext<'_>,
    subject: &Subject,
    matches: &[Captures],
    ids: &[u32],
    ty: CapType,
    global: bool,
) -> Result<Eterm> {
    if ids.is_empty() {
        return ctx.atom(MATCH);
    }
    if matches.is_empty() {
        return Err(badarg());
    }
    if global {
        let mut per_match: Vec<Eterm> = Vec::with_capacity(matches.len());
        for caps in matches {
            per_match.push(render_match(ctx, subject, caps, ids, ty)?);
        }
        let inner = ctx.list(&per_match)?;
        let tag = ctx.atom(MATCH)?;
        ctx.tuple(&[tag, inner])
    } else {
        let caps = matches.first().ok_or_else(badarg)?;
        let list = render_match(ctx, subject, caps, ids, ty)?;
        let tag = ctx.atom(MATCH)?;
        ctx.tuple(&[tag, list])
    }
}

/// The ordered list of rendered values for one match — one element per id.
fn render_match(
    ctx: &mut BifContext<'_>,
    subject: &Subject,
    caps: &Captures,
    ids: &[u32],
    ty: CapType,
) -> Result<Eterm> {
    let mut elems: Vec<Eterm> = Vec::with_capacity(ids.len());
    for &id in ids {
        elems.push(render_capture(ctx, subject, caps, id, ty)?);
    }
    ctx.list(&elems)
}

/// One capture value, in the requested type. A non-participating group (no span
/// for `id`, an `id == u32::MAX` or an out-of-range index) renders the type's
/// empty form rather than an error — [`Captures::group`] bounds-checks the id.
fn render_capture(
    ctx: &mut BifContext<'_>,
    subject: &Subject,
    caps: &Captures,
    id: u32,
    ty: CapType,
) -> Result<Eterm> {
    match caps.group(id) {
        Some(span) => match ty {
            CapType::Index => render_index(ctx, subject, span),
            CapType::List => render_list(ctx, subject, span),
            CapType::Binary => render_binary(ctx, subject, span),
        },
        None => match ty {
            CapType::Index => {
                let start = ctx.int(-1)?;
                let len = ctx.int(0)?;
                ctx.tuple(&[start, len])
            }
            CapType::List => Ok(NIL),
            CapType::Binary => bits::make_binary(ctx.heap(), &[]),
        },
    }
}

/// `{Start, Len}` — the **byte** offsets of a codepoint span (`index`).
fn render_index(ctx: &mut BifContext<'_>, subject: &Subject, span: (usize, usize)) -> Result<Eterm> {
    let (start, end) = subject.byte_span(span);
    let len = end.saturating_sub(start);
    let start = ctx.int(offset(start))?;
    let len = ctx.int(offset(len))?;
    ctx.tuple(&[start, len])
}

/// A charlist of the span's **decoded codepoints** (`list`).
fn render_list(ctx: &mut BifContext<'_>, subject: &Subject, span: (usize, usize)) -> Result<Eterm> {
    let codes = subject.codepoints(span);
    let mut elems: Vec<Eterm> = Vec::with_capacity(codes.len());
    for &code in codes {
        elems.push(ctx.int(i64::from(code))?);
    }
    ctx.list(&elems)
}

/// The span's bytes as a binary (`binary`). The byte span is clamped by
/// [`Subject::byte_span`], so the slice is always in range; a `get` that missed
/// would be a bug rather than a runtime condition, and is refused rather than
/// panicked on.
fn render_binary(ctx: &mut BifContext<'_>, subject: &Subject, span: (usize, usize)) -> Result<Eterm> {
    let (start, end) = subject.byte_span(span);
    let bytes = subject.bytes.get(start..end).ok_or_else(badarg)?;
    bits::make_binary(ctx.heap(), bytes)
}

/// A `usize` byte offset as a small-integer `i64`. A subject larger than
/// `i64::MAX` cannot exist on a real heap; saturating keeps the conversion total
/// (and `BifContext::int` still reports `SystemLimit` for a value past 60 bits).
fn offset(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    use beam_alloc::Heap;
    use beam_process::ProcessId;
    use beam_term::AtomTable;

    use crate::re::options::Newline;
    use crate::re::prog::ProgFlags;

    /// Runs `f` with a `BifContext` over a real test heap and a seeded atom table.
    fn with_ctx<R>(f: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
        let mut heap = Heap::new(1 << 16).expect("test heap");
        let mut atoms = AtomTable::new();
        atoms.seed(["match"]);
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&mut ctx)
    }

    /// The `xabcy` subject of this node's oracle goldens.
    fn subject() -> Subject {
        Subject::latin1(b"xabcy".to_vec())
    }

    /// The `(b)(c)` program: two groups, `foo` naming group 1.
    fn program() -> Program {
        Program {
            insts: Vec::new(),
            classes: Vec::new(),
            ncap: 2,
            names: vec![("foo".to_string(), 1)],
            flags: ProgFlags {
                unicode: false,
                anchored: false,
                firstline: false,
                has_cr_or_lf: false,
                newline: Newline::default(),
            },
        }
    }

    /// A `Captures` whose group `g` has span `spans[g]`, laid out as the matcher
    /// writes it (the odd slot carries the completed span; group 0 is written into
    /// both by `Match`).
    fn captures(spans: &[Option<(usize, usize)>]) -> Captures {
        let mut slots = Vec::with_capacity(spans.len() * 2);
        for (group, span) in spans.iter().enumerate() {
            match span {
                Some((start, end)) => {
                    slots.push(Some(if group == 0 { (*start, *end) } else { (*start, *start) }));
                    slots.push(Some((*start, *end)));
                }
                None => {
                    slots.push(None);
                    slots.push(None);
                }
            }
        }
        Captures { slots }
    }

    /// The `(b)(c)` match of `xabcy`: group 0 `(2,4)`, group 1 `(2,3)`, group 2
    /// `(3,4)`.
    fn matched() -> Captures {
        captures(&[Some((2, 4)), Some((2, 3)), Some((3, 4))])
    }

    /// Renders `term` with the `~p` printer, for readable assertions.
    fn show(ctx: &mut BifContext<'_>, term: Eterm) -> String {
        crate::print::format_term(ctx, term, true).expect("the term is printable")
    }

    /// Renders the single non-global match of `xabcy`.
    fn rendered(value: &CapValue, ty: CapType, caps: &Captures) -> String {
        let ids = resolve_ids(value, &program());
        with_ctx(|ctx| {
            let out = render(ctx, &subject(), std::slice::from_ref(caps), &ids, ty, false)
                .expect("the render fits the heap");
            show(ctx, out)
        })
    }

    #[test]
    fn resolve_ids_covers_every_value_spec() {
        let program = program();
        assert_eq!(resolve_ids(&CapValue::All, &program), vec![0, 1, 2]);
        assert_eq!(resolve_ids(&CapValue::AllButFirst, &program), vec![1, 2]);
        assert_eq!(resolve_ids(&CapValue::First, &program), vec![0]);
        assert!(resolve_ids(&CapValue::None, &program).is_empty());
        // `all_names` yields the named group's id, in name order.
        assert_eq!(resolve_ids(&CapValue::AllNames, &program), vec![1]);
        // An explicit id list keeps its requested order.
        assert_eq!(
            resolve_ids(&CapValue::Names(vec![CapId::Index(2), CapId::Index(1)]), &program),
            vec![2, 1]
        );
        // A mixed index/name list: the name is looked up, an unknown one is the
        // `u32::MAX` sentinel.
        assert_eq!(
            resolve_ids(
                &CapValue::Names(vec![
                    CapId::Name("foo".to_string()),
                    CapId::Index(2),
                    CapId::Name("zz".to_string()),
                ]),
                &program
            ),
            vec![1, 2, u32::MAX]
        );
    }

    #[test]
    fn resolve_ids_of_a_groupless_program() {
        let bare = Program { ncap: 0, names: Vec::new(), ..program() };
        assert_eq!(resolve_ids(&CapValue::All, &bare), vec![0], "just the whole match");
        assert!(resolve_ids(&CapValue::AllButFirst, &bare).is_empty());
        assert!(resolve_ids(&CapValue::AllNames, &bare).is_empty(), "no names to yield");
        assert_eq!(resolve_ids(&CapValue::First, &bare), vec![0]);
        assert!(resolve_ids(&CapValue::None, &bare).is_empty());
    }

    #[test]
    fn all_with_index_is_the_oracle_golden() {
        assert_eq!(
            rendered(&CapValue::All, CapType::Index, &matched()),
            "{match,[{2,2},{2,1},{3,1}]}"
        );
    }

    #[test]
    fn first_with_index_keeps_only_the_whole_match() {
        assert_eq!(rendered(&CapValue::First, CapType::Index, &matched()), "{match,[{2,2}]}");
    }

    #[test]
    fn all_but_first_with_list_is_a_charlist() {
        assert_eq!(
            rendered(&CapValue::AllButFirst, CapType::List, &matched()),
            r#"{match,["b","c"]}"#
        );
    }

    #[test]
    fn an_explicit_id_list_keeps_the_requested_order_and_type() {
        // `re:run("xabcy","(b)(c)",[{capture,[2,1],index}])` = {match,[{3,1},{2,1}]}:
        // the ids are rendered exactly, with no implicit index-0 prepend.
        let ids = CapValue::Names(vec![CapId::Index(2), CapId::Index(1)]);
        assert_eq!(rendered(&ids, CapType::Index, &matched()), "{match,[{3,1},{2,1}]}");
        assert_eq!(rendered(&ids, CapType::List, &matched()), r#"{match,["c","b"]}"#);
        assert_eq!(
            rendered(&ids, CapType::Binary, &matched()),
            r#"{match,[<<"c">>,<<"b">>]}"#
        );
    }

    #[test]
    fn all_names_renders_the_named_groups_values_in_name_order() {
        // `re:run("xabcy","(?<foo>b)(c)",[{capture,all_names,index}])` = {match,[{2,1}]}.
        assert_eq!(rendered(&CapValue::AllNames, CapType::Index, &matched()), "{match,[{2,1}]}");
        assert_eq!(rendered(&CapValue::AllNames, CapType::List, &matched()), r#"{match,["b"]}"#);
    }

    #[test]
    fn a_none_capture_is_the_bare_match_atom_in_both_modes() {
        let ids = resolve_ids(&CapValue::None, &program());
        assert!(ids.is_empty());
        assert_eq!(
            with_ctx(|ctx| {
                let out = render(ctx, &subject(), &[matched()], &ids, CapType::Index, false)
                    .expect("fits");
                show(ctx, out)
            }),
            "match"
        );
        assert_eq!(
            with_ctx(|ctx| {
                let out = render(ctx, &subject(), &[matched()], &ids, CapType::Index, true)
                    .expect("fits");
                show(ctx, out)
            }),
            "match",
            "global with no captures is still the bare atom"
        );
    }

    #[test]
    fn an_out_of_range_id_and_an_unknown_name_render_non_participating() {
        // An index above `ncap`: `{match,[{-1,0}]}`.
        assert_eq!(
            rendered(&CapValue::Names(vec![CapId::Index(5)]), CapType::Index, &matched()),
            "{match,[{-1,0}]}"
        );
        // An unknown subpattern name: `{match,[[]]}` (`list`) / `{match,[{-1,0}]}` (`index`).
        let unknown = CapValue::Names(vec![CapId::Name("zz".to_string())]);
        assert_eq!(rendered(&unknown, CapType::List, &matched()), "{match,[[]]}");
        assert_eq!(rendered(&unknown, CapType::Index, &matched()), "{match,[{-1,0}]}");
    }

    #[test]
    fn a_non_participating_group_renders_its_empty_form_per_type() {
        // `re:run("xabcy","(z)?(b)",[{capture,all,index}])` = {match,[{2,1},{-1,0},{2,1}]}
        // shape: group 2 did not participate.
        let caps = captures(&[Some((2, 4)), Some((2, 3)), None]);
        assert_eq!(
            rendered(&CapValue::All, CapType::Index, &caps),
            "{match,[{2,2},{2,1},{-1,0}]}"
        );
        let caps = captures(&[Some((2, 4)), Some((2, 3)), None]);
        assert_eq!(rendered(&CapValue::All, CapType::List, &caps), r#"{match,["bc","b",[]]}"#);
        let caps = captures(&[Some((2, 4)), Some((2, 3)), None]);
        assert_eq!(
            rendered(&CapValue::All, CapType::Binary, &caps),
            r#"{match,[<<"bc">>,<<"b">>,<<>>]}"#
        );
    }

    #[test]
    fn a_global_call_nests_one_list_per_match() {
        let ids = resolve_ids(&CapValue::All, &program());
        let second = captures(&[Some((0, 1)), Some((0, 1)), None]);
        let matches = [matched(), second];
        assert_eq!(
            with_ctx(|ctx| {
                let out = render(ctx, &subject(), &matches, &ids, CapType::Index, true)
                    .expect("fits");
                show(ctx, out)
            }),
            "{match,[[{2,2},{2,1},{3,1}],[{0,1},{0,1},{-1,0}]]}"
        );
    }

    #[test]
    fn an_empty_match_set_with_ids_is_refused() {
        // `re:run` returns `nomatch` for an empty match set, so the renderer is
        // never handed one; it refuses rather than fabricating `{match,[]}`.
        let ids = resolve_ids(&CapValue::All, &program());
        with_ctx(|ctx| {
            assert!(matches!(
                render(ctx, &subject(), &[], &ids, CapType::Index, false),
                Err(beam_common::BeamError::BadArg)
            ));
            assert!(matches!(
                render(ctx, &subject(), &[], &ids, CapType::Index, true),
                Err(beam_common::BeamError::BadArg)
            ));
        });
    }

    #[test]
    fn a_unicode_subject_renders_codepoints_and_byte_offsets() {
        // `é` is two UTF-8 bytes but one codepoint: `list` reports the codepoint,
        // `index` the byte offset, and `binary` the two bytes.
        let subject = Subject::utf8("aé".as_bytes().to_vec()).expect("valid UTF-8");
        // Group 1 is the single `é` codepoint `(1, 2)`.
        let caps = captures(&[Some((1, 2)), Some((1, 2)), None]);
        let ids = resolve_ids(&CapValue::Names(vec![CapId::Index(1)]), &program());

        let index = with_ctx(|ctx| {
            let out =
                render(ctx, &subject, std::slice::from_ref(&caps), &ids, CapType::Index, false)
                    .expect("fits");
            show(ctx, out)
        });
        assert_eq!(index, "{match,[{1,2}]}", "one codepoint is two bytes");

        let list = with_ctx(|ctx| {
            let out = render(ctx, &subject, std::slice::from_ref(&caps), &ids, CapType::List, false)
                .expect("fits");
            show(ctx, out)
        });
        assert_eq!(list, r#"{match,["é"]}"#);

        // The binary renders each byte as a latin-1 character, so it is compared
        // by its bytes rather than by text.
        let bytes = with_ctx(|ctx| {
            let out =
                render(ctx, &subject, std::slice::from_ref(&caps), &ids, CapType::Binary, false)
                    .expect("fits");
            let binary = crate::access::car(crate::access::tuple_element(out, 1));
            bits::bytes(binary).expect("a byte-aligned binary")
        });
        assert_eq!(bytes, vec![0xC3, 0xA9], "the UTF-8 encoding of `é`");
    }
}
