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

//! Integration tests for the **deep-list** contract of `io_lib:format/2,3`.
//!
//! `io_lib:format` does not return a flat character list: a literal format
//! character and every "small" control (`~s` of an **atom**, `~e`/`~f`/`~g`,
//! `~b`/`~B`, `~x`/`~X`, `~c`, `~~`, `~n`, `~i`) contribute their own element,
//! while every other control (`~w`, `~p`, `~W`, `~P`, `~s` of a list/binary)
//! contributes a **single sub-list** element. That is exactly
//! `io_lib_format:build_small/2` + `build_limited/5` (whose `S` values are the
//! nested renderings of `io_lib:write/…`/`io_lib_pretty:print/…`).
//!
//! Every expectation below is the literal output of
//!
//! ```text
//! erl -noshell -eval 'io:format("~w~n",[io_lib:format(Fmt, Args)]),halt().'
//! ```
//!
//! on the oracle (OTP 29 / erts-17.1), captured with `~w` so the structure — not
//! the printable-string rendering — is visible. The result is compared
//! structurally with `beam_term::compare::eq`, so a differing nesting is a
//! failure even when the flattened text agrees.
//!
//! This file complements `tests/io_ets.rs`, which pins `io:format`'s exact output
//! bytes (the flattened form of the same chunk tree).

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, BytesSink};
use beam_codec::term as codec_term;
use beam_common::{BeamError, ExceptionClass};
use beam_process::ProcessId;
use beam_term::{AtomTable, Eterm};

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true", "false", "ok", "a", "b", "c", "foo", "xy", "hello",
        // The format-argument matrix below needs an atom format, an empty atom and
        // the `error`/`badarg` pair the grapheme walk interns on a bad UTF-8 binary.
        "aaa", "atom_nl", "", "a~nb", "bb", "cc", "error", "badarg",
    ]);
    atoms
}

/// The expected nested-list shape of an `io_lib:format` result.
///
/// `I(c)` is one element (a character code), `S(s)` a flat character list and
/// `L(items)` a sub-list — the same shapes `io_lib_format` builds.
enum E {
    /// One character element.
    I(i64),
    /// A flat character list (a run of individual characters).
    S(&'static str),
    /// A sub-list element.
    L(Vec<E>),
}

/// Materializes an expected shape as a term on `ctx`'s heap.
fn build(ctx: &mut BifContext<'_>, e: &E) -> Eterm {
    match e {
        E::I(value) => ctx.int(*value).expect("a character code fits"),
        E::S(text) => {
            let mut elems = Vec::with_capacity(text.len());
            for byte in text.bytes() {
                elems.push(ctx.int(i64::from(byte)).expect("a character code fits"));
            }
            ctx.list(&elems).expect("a short list fits")
        }
        E::L(items) => {
            let mut elems = Vec::with_capacity(items.len());
            for item in items {
                elems.push(build(ctx, item));
            }
            ctx.list(&elems).expect("a short list fits")
        }
    }
}

fn int(ctx: &mut BifContext<'_>, value: i64) -> Eterm {
    ctx.int(value).expect("a small int fits")
}

fn atom(ctx: &mut BifContext<'_>, name: &str) -> Eterm {
    ctx.atom(name).expect("the atom is seeded")
}

fn float(ctx: &mut BifContext<'_>, value: f64) -> Eterm {
    ctx.make_float(value).expect("a float fits")
}

fn tuple(ctx: &mut BifContext<'_>, elems: &[Eterm]) -> Eterm {
    ctx.tuple(elems).expect("a short tuple fits")
}

/// A proper list of the byte-sized integers of `text` (an Erlang string).
fn char_list(ctx: &mut BifContext<'_>, text: &str) -> Eterm {
    let mut elems = Vec::with_capacity(text.len());
    for byte in text.bytes() {
        elems.push(int(ctx, i64::from(byte)));
    }
    ctx.list(&elems).expect("a short list fits")
}

/// A binary built through the registered `erlang:list_to_binary/1` (the public API
/// exposes no binary constructor).
fn binary(ctx: &mut BifContext<'_>, reg: &BifRegistry, bytes: &[u8]) -> Eterm {
    let mut elems = Vec::with_capacity(bytes.len());
    for &byte in bytes {
        elems.push(int(ctx, i64::from(byte)));
    }
    let list = ctx.list(&elems).expect("a short list fits");
    reg.call("erlang", "list_to_binary", 1, ctx, &[list])
        .expect("list_to_binary")
}

/// The format term: a flat proper list of the byte-sized integers of `bytes`.
fn byte_list(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
    let mut elems = Vec::with_capacity(bytes.len());
    for &byte in bytes {
        elems.push(int(ctx, i64::from(byte)));
    }
    ctx.list(&elems).expect("a short format list fits")
}

/// Asserts that `io_lib:format(Format, Args...)` is the oracle's deep list
/// `expected`.
fn check(
    ctx: &mut BifContext<'_>,
    reg: &BifRegistry,
    format: &str,
    args: impl FnOnce(&mut BifContext<'_>) -> Vec<Eterm>,
    expected: &E,
) {
    let arg_terms = args(ctx);
    let format_term = byte_list(ctx, format.as_bytes());
    let args_term = ctx.list(&arg_terms).expect("a short argument list fits");
    let got = reg
        .call("io_lib", "format", 2, ctx, &[format_term, args_term])
        .unwrap_or_else(|err| panic!("io_lib:format({format:?}) failed: {err:?}"));
    let want = build(ctx, expected);
    assert!(
        beam_term::compare::eq(got, want, ctx.atoms()),
        "io_lib:format({format:?}) does not match the oracle's deep list"
    );
}

/// Literal characters and the "small" controls are spliced flat; `~w`/`~p` insert
/// a single sub-list element.
#[test]
fn literals_and_small_controls_are_flat() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // `hello` (no control at all) is one character per element.
    check(&mut ctx, &reg, "hello", |_| Vec::new(), &E::S("hello"));
    // `a~nb` — `~n` is a small control, its newline is one element.
    check(
        &mut ctx,
        &reg,
        "a~nb",
        |_| Vec::new(),
        &E::L(vec![E::I(97), E::I(10), E::I(98)]),
    );
    // `~w` — the rendering is ONE element: `[[52,50]]`.
    check(
        &mut ctx,
        &reg,
        "~w",
        |c| vec![int(c, 42)],
        &E::L(vec![E::S("42")]),
    );
    // Mixed: `a~wb~n` -> `[97,[52,50],98,10]`.
    check(
        &mut ctx,
        &reg,
        "a~wb~n",
        |c| vec![int(c, 42)],
        &E::L(vec![E::I(97), E::S("42"), E::I(98), E::I(10)]),
    );
    // Two substitutions: `~w ~w` -> `[[49],32,[50]]`.
    check(
        &mut ctx,
        &reg,
        "~w ~w",
        |c| vec![int(c, 1), int(c, 2)],
        &E::L(vec![E::S("1"), E::I(32), E::S("2")]),
    );
    // `~p` behaves like `~w` structurally.
    check(
        &mut ctx,
        &reg,
        "~p",
        |c| vec![int(c, 42)],
        &E::L(vec![E::S("42")]),
    );
    // `~p` of a printable string: the quoted form is one element.
    check(
        &mut ctx,
        &reg,
        "~p",
        |c| vec![atom(c, "xy")],
        &E::L(vec![E::S("xy")]),
    );
    // `~i` consumes its argument and contributes nothing.
    check(
        &mut ctx,
        &reg,
        "~i~w",
        |c| vec![int(c, 5), int(c, 7)],
        &E::L(vec![E::S("7")]),
    );
}

/// `~s` of an atom is small (flat) but `~s` of a list or binary is not (one
/// element) — `io_lib_format:control_small/7` has a clause for atoms only.
#[test]
fn string_control_splits_on_the_argument_class() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // `~s` of the ATOM `xy`: spliced flat.
    check(
        &mut ctx,
        &reg,
        "~s",
        |c| vec![atom(c, "xy")],
        &E::S("xy"),
    );
    // `~s` of the STRING "xy" (`[120,121]`): one sub-list element.
    check(
        &mut ctx,
        &reg,
        "~s",
        |c| vec![char_list(c, "xy")],
        &E::L(vec![E::S("xy")]),
    );
    // `~s` of a string with a precision: `~.2s` of "hello" -> `["he"]`.
    check(
        &mut ctx,
        &reg,
        "~.2s",
        |c| vec![char_list(c, "hello")],
        &E::L(vec![E::S("he")]),
    );
    // `~s` of a BINARY is likewise one element.
    check(
        &mut ctx,
        &reg,
        "~s",
        |c| vec![binary(c, &reg, b"xy")],
        &E::L(vec![E::S("xy")]),
    );
    // ... and is still an element when embedded in literal text.
    check(
        &mut ctx,
        &reg,
        "a~s~nb",
        |c| vec![binary(c, &reg, b"xy")],
        &E::L(vec![E::I(97), E::S("xy"), E::I(10), E::I(98)]),
    );
}

/// Radix, character and float controls are all "small": their characters are
/// spliced flat.
#[test]
fn numeric_and_character_controls_are_flat() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    check(
        &mut ctx,
        &reg,
        "~b",
        |c| vec![int(c, 255)],
        &E::S("255"),
    );
    // `~2..0b` — field 2 with pad `0`, base 10 (no precision) -> "05".
    check(&mut ctx, &reg, "~2..0b", |c| vec![int(c, 5)], &E::S("05"));
    check(
        &mut ctx,
        &reg,
        "~5..0b",
        |c| vec![int(c, 5)],
        &E::S("00005"),
    );
    check(
        &mut ctx,
        &reg,
        "~.16b",
        |c| vec![int(c, 255)],
        &E::S("ff"),
    );
    check(
        &mut ctx,
        &reg,
        "~8.16b",
        |c| vec![int(c, 255)],
        &E::S("      ff"),
    );
    check(
        &mut ctx,
        &reg,
        "~-8.16b",
        |c| vec![int(c, 255)],
        &E::S("ff      "),
    );
    check(
        &mut ctx,
        &reg,
        "~w",
        |c| vec![int(c, -5)],
        &E::L(vec![E::S("-5")]),
    );
    check(&mut ctx, &reg, "~c", |c| vec![int(c, 65)], &E::L(vec![E::I(65)]));
    check(
        &mut ctx,
        &reg,
        "~3c",
        |c| vec![int(c, 65)],
        &E::L(vec![E::I(65), E::I(65), E::I(65)]),
    );
    check(
        &mut ctx,
        &reg,
        "~e",
        |c| vec![float(c, 1.5)],
        &E::S("1.50000e+0"),
    );
    check(
        &mut ctx,
        &reg,
        "~f",
        |c| vec![float(c, 1.5)],
        &E::S("1.500000"),
    );
    check(
        &mut ctx,
        &reg,
        "~g",
        |c| vec![float(c, 1.5)],
        &E::S("1.50000"),
    );
    check(
        &mut ctx,
        &reg,
        "~12.4f",
        |c| vec![float(c, 1.567)],
        &E::S("      1.5670"),
    );
    // `~w` of a float is NOT small: the rendering is one element.
    check(
        &mut ctx,
        &reg,
        "~w",
        |c| vec![float(c, 1.0)],
        &E::L(vec![E::S("1.0")]),
    );
    check(
        &mut ctx,
        &reg,
        "~w",
        |c| vec![float(c, 42.5)],
        &E::L(vec![E::S("42.5")]),
    );
    // An atom under `~w` is one element too.
    check(
        &mut ctx,
        &reg,
        "~w",
        |c| vec![atom(c, "foo")],
        &E::L(vec![E::S("foo")]),
    );
}

/// The container renderings are themselves deeply chunked, mirroring
/// `io_lib:write1/4`: delimiters and separators are individual integers, every
/// element its own sub-list.
#[test]
fn container_renderings_are_deeply_chunked() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // `~w` of `{1,2}` -> `[[123,[[49],44,[50]],125]]`.
    check(
        &mut ctx,
        &reg,
        "~w",
        |c| {
            let one = int(c, 1);
            let two = int(c, 2);
            vec![tuple(c, &[one, two])]
        },
        &E::L(vec![E::L(vec![
            E::I(123),
            E::L(vec![E::S("1"), E::I(44), E::S("2")]),
            E::I(125),
        ])]),
    );
    // `~p` of `{1,2}` is identical.
    check(
        &mut ctx,
        &reg,
        "~p",
        |c| {
            let one = int(c, 1);
            let two = int(c, 2);
            vec![tuple(c, &[one, two])]
        },
        &E::L(vec![E::L(vec![
            E::I(123),
            E::L(vec![E::S("1"), E::I(44), E::S("2")]),
            E::I(125),
        ])]),
    );
    // `~w` of `[1,2,3]` -> `[[91,[[49],44,[50],44,[51]],93]]`.
    check(
        &mut ctx,
        &reg,
        "~w",
        |c| {
            let elems = vec![int(c, 1), int(c, 2), int(c, 3)];
            vec![c.list(&elems).expect("fits")]
        },
        &E::L(vec![E::L(vec![
            E::I(91),
            E::L(vec![
                E::S("1"),
                E::I(44),
                E::S("2"),
                E::I(44),
                E::S("3"),
            ]),
            E::I(93),
        ])]),
    );
    // `~w` of `{}` / `[]` is a single element (a flat run).
    check(
        &mut ctx,
        &reg,
        "~w",
        |c| vec![tuple(c, &[])],
        &E::L(vec![E::S("{}")]),
    );
    check(
        &mut ctx,
        &reg,
        "~w",
        |c| vec![c.list(&[]).expect("fits")],
        &E::L(vec![E::S("[]")]),
    );
    check(
        &mut ctx,
        &reg,
        "~p",
        |c| vec![c.list(&[]).expect("fits")],
        &E::L(vec![E::S("[]")]),
    );
    // `~10p` of `{a,1}` — with a field width the container is still one element.
    check(
        &mut ctx,
        &reg,
        "~10p",
        |c| {
            let a = atom(c, "a");
            let one = int(c, 1);
            vec![tuple(c, &[a, one])]
        },
        &E::L(vec![E::L(vec![
            E::I(123),
            E::L(vec![E::S("a"), E::I(44), E::S("1")]),
            E::I(125),
        ])]),
    );
}

/// Binaries: `~w` renders the bytes (each its own sub-list, `,` separators), `~p`
/// keeps a printable binary as one flat `<<"…">>` element.
#[test]
fn binary_renderings_match_the_oracle() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // `~w` of `<<1,2,3>>` -> `[[60,60,[[49],44,[50],44,[51]],62,62]]`.
    check(
        &mut ctx,
        &reg,
        "~w",
        |c| vec![binary(c, &reg, &[1, 2, 3])],
        &E::L(vec![E::L(vec![
            E::I(60),
            E::I(60),
            E::L(vec![
                E::S("1"),
                E::I(44),
                E::S("2"),
                E::I(44),
                E::S("3"),
            ]),
            E::I(62),
            E::I(62),
        ])]),
    );
    // An empty binary has an EMPTY body element: `[[60,60,[],62,62]]`.
    check(
        &mut ctx,
        &reg,
        "~w",
        |c| vec![binary(c, &reg, &[])],
        &E::L(vec![E::L(vec![
            E::I(60),
            E::I(60),
            E::L(Vec::new()),
            E::I(62),
            E::I(62),
        ])]),
    );
    // `~p` of a printable binary: one element, `<<"…">>` with the string flat.
    check(
        &mut ctx,
        &reg,
        "~p",
        |c| vec![binary(c, &reg, b"abc")],
        &E::L(vec![E::L(vec![
            E::I(60),
            E::I(60),
            E::S("\"abc\""),
            E::I(62),
            E::I(62),
        ])]),
    );
    // `~p` of an EMPTY binary is the flat `<<>>` (one element).
    check(
        &mut ctx,
        &reg,
        "~p",
        |c| vec![binary(c, &reg, &[])],
        &E::L(vec![E::S("<<>>")]),
    );
}

/// The `~W`/`~P` depth argument: an elided container is still one element, and the
/// `,...`/`|...` marker characters are individual integers.
#[test]
fn depth_limited_renderings_match_the_oracle() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // `{a,{b,{c,1}}}`.
    let nested = |c: &mut BifContext<'_>| {
        let a = atom(c, "a");
        let b = atom(c, "b");
        let cc = atom(c, "c");
        let one = int(c, 1);
        let inner = tuple(c, &[cc, one]);
        let mid = tuple(c, &[b, inner]);
        tuple(c, &[a, mid])
    };
    // `~W` at depth 3 -> `{a,{...}}` -> `[[123,[[97],44,[123,46,46,46,125]],125]]`.
    check(
        &mut ctx,
        &reg,
        "~W",
        |c| {
            let term = nested(c);
            let depth = int(c, 3);
            vec![term, depth]
        },
        &E::L(vec![E::L(vec![
            E::I(123),
            E::L(vec![E::S("a"), E::I(44), E::S("{...}")]),
            E::I(125),
        ])]),
    );
    // `~W` at depth 2 -> `{a,...}` -> `[[123,[[97],44,46,46,46],125]]`.
    check(
        &mut ctx,
        &reg,
        "~W",
        |c| {
            let term = nested(c);
            let depth = int(c, 2);
            vec![term, depth]
        },
        &E::L(vec![E::L(vec![
            E::I(123),
            E::L(vec![E::S("a"), E::I(44), E::I(46), E::I(46), E::I(46)]),
            E::I(125),
        ])]),
    );
    // A list at depth 3 is `[1,2|...]` -> `[[91,[[49],44,[50],124,46,46,46],93]]`.
    let long_list = |c: &mut BifContext<'_>| {
        let elems: Vec<Eterm> = (1..=5).map(|i| int(c, i)).collect();
        c.list(&elems).expect("fits")
    };
    check(
        &mut ctx,
        &reg,
        "~W",
        |c| {
            let term = long_list(c);
            let depth = int(c, 3);
            vec![term, depth]
        },
        &E::L(vec![E::L(vec![
            E::I(91),
            E::L(vec![
                E::S("1"),
                E::I(44),
                E::S("2"),
                E::I(124),
                E::I(46),
                E::I(46),
                E::I(46),
            ]),
            E::I(93),
        ])]),
    );
    // `~P` produces the same structure.
    check(
        &mut ctx,
        &reg,
        "~P",
        |c| {
            let term = long_list(c);
            let depth = int(c, 3);
            vec![term, depth]
        },
        &E::L(vec![E::L(vec![
            E::I(91),
            E::L(vec![
                E::S("1"),
                E::I(44),
                E::S("2"),
                E::I(124),
                E::I(46),
                E::I(46),
                E::I(46),
            ]),
            E::I(93),
        ])]),
    );
}

/// Field padding is a *tree* of sub-lists (`io_lib_format:chars/2`), not a flat
/// run — visible for widths of four and more.
#[test]
fn field_padding_is_a_linked_tree() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // `~12s` of "ab" -> `[[[[32,[32,32],32,32],32,[32,32],32,32],97,98]]`.
    check(
        &mut ctx,
        &reg,
        "~12s",
        |c| {
            let a = int(c, 97);
            let b = int(c, 98);
            vec![c.list(&[a, b]).expect("fits")]
        },
        &E::L(vec![E::L(vec![
            E::L(vec![
                E::L(vec![E::I(32), E::L(vec![E::I(32), E::I(32)]), E::I(32), E::I(32)]),
                E::I(32),
                E::L(vec![E::I(32), E::I(32)]),
                E::I(32),
                E::I(32),
            ]),
            E::I(97),
            E::I(98),
        ])]),
    );
    // `~10.3s` of "hello" -> `[[[32,[32,32,32],32,32,32],104,101,108]]`.
    check(
        &mut ctx,
        &reg,
        "~10.3s",
        |c| vec![char_list(c, "hello")],
        &E::L(vec![E::L(vec![
            E::L(vec![
                E::I(32),
                E::L(vec![E::I(32), E::I(32), E::I(32)]),
                E::I(32),
                E::I(32),
                E::I(32),
            ]),
            E::I(104),
            E::I(101),
            E::I(108),
        ])]),
    );
}

/// `io_lib:format/3`'s `Options` list: the default/unlimited options reproduce
/// `format/2`, an unknown option is ignored, and a *limiting* `chars_limit` is
/// honoured (`build_small`/`count_small`/`build_limited`, eliding a big control
/// with `"..."` when it does not fit the per-directive budget) — never a wrong
/// (always-unlimited) rendering. The list shape is the same as `format/2` (the
/// oracle's `["42"]`).
#[test]
fn format_3_options_reproduce_or_reject_the_chars_limit() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let mut atoms = seeded_atoms();
    atoms.seed(["chars_limit"]);
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let format_term = byte_list(&mut ctx, b"~w");
    let arg = int(&mut ctx, 42);
    let args = ctx.list(&[arg]).expect("a short argument list fits");
    let empty = ctx.list(&[]).expect("fits");
    let reference = reg
        .call("io_lib", "format", 2, &mut ctx, &[format_term, args])
        .expect("io_lib:format/2 fits");
    // `io_lib:format("~w", [42])` => `[[52,50]]`.
    let want = build(&mut ctx, &E::L(vec![E::S("42")]));
    assert!(beam_term::compare::eq(reference, want, ctx.atoms()));

    // Empty options: `format/3` is `format/2`.
    let got = reg
        .call("io_lib", "format", 3, &mut ctx, &[format_term, args, empty])
        .expect("io_lib:format/3 fits");
    assert!(beam_term::compare::eq(got, reference, ctx.atoms()));

    // An unknown option is ignored (the oracle only reads `chars_limit`).
    let foo = atom(&mut ctx, "foo");
    let unknown = ctx.list(&[foo]).expect("fits");
    let got = reg
        .call("io_lib", "format", 3, &mut ctx, &[format_term, args, unknown])
        .expect("an unknown option is ignored");
    assert!(beam_term::compare::eq(got, reference, ctx.atoms()));

    // A limiting `chars_limit` is honoured: `~w` of `42` fits a 3-character
    // budget, so the oracle renders it unchanged (`[[52,50]]`).
    let tag = atom(&mut ctx, "chars_limit");
    let three = int(&mut ctx, 3);
    let option = tuple(&mut ctx, &[tag, three]);
    let limit = ctx.list(&[option]).expect("fits");
    let got = reg
        .call("io_lib", "format", 3, &mut ctx, &[format_term, args, limit])
        .expect("a limiting chars_limit is honoured");
    assert!(beam_term::compare::eq(got, reference, ctx.atoms()));

    // A zero budget elides the control, as the oracle does:
    // `io_lib:format("~w", [42], [{chars_limit,0}])` => `["..."]`.
    let zero = int(&mut ctx, 0);
    let option = tuple(&mut ctx, &[tag, zero]);
    let limit = ctx.list(&[option]).expect("fits");
    let got = reg
        .call("io_lib", "format", 3, &mut ctx, &[format_term, args, limit])
        .expect("a zero chars_limit is honoured");
    let elided = build(&mut ctx, &E::L(vec![E::S("...")]));
    assert!(beam_term::compare::eq(got, elided, ctx.atoms()));

    // A negative `chars_limit` is the oracle's default (no limit).
    let neg = int(&mut ctx, -1);
    let option = tuple(&mut ctx, &[tag, neg]);
    let unlimited = ctx.list(&[option]).expect("fits");
    let got = reg
        .call("io_lib", "format", 3, &mut ctx, &[format_term, args, unlimited])
        .expect("a negative chars_limit is the default");
    assert!(beam_term::compare::eq(got, reference, ctx.atoms()));

    // A non-list `Options` is `badarg`.
    let err = reg
        .call("io_lib", "format", 3, &mut ctx, &[format_term, args, foo])
        .expect_err("a non-list Options is badarg");
    assert!(matches!(err, BeamError::BadArg));
}

// ---------------------------------------------------------------------------
// The format argument: an atom, a binary, or a (possibly nested) chardata list
// ---------------------------------------------------------------------------

/// Asserts that `io_lib:format(Format, Args)` is the oracle's deep list `want`.
///
/// Unlike [`check`], the format term is built by the caller, so it may be an atom,
/// a binary or a chardata list rather than just a flat byte list.
fn check_format(
    ctx: &mut BifContext<'_>,
    reg: &BifRegistry,
    format: Eterm,
    args: &[Eterm],
    want: Eterm,
) {
    let args_term = ctx.list(args).expect("a short argument list fits");
    let got = reg
        .call("io_lib", "format", 2, ctx, &[format, args_term])
        .unwrap_or_else(|err| panic!("io_lib:format failed: {err:?}"));
    assert!(
        beam_term::compare::eq(got, want, ctx.atoms()),
        "io_lib:format does not match the oracle's deep list"
    );
}

/// Asserts that `io_lib:format(Format, Args)` raises the oracle's `error:badarg`.
///
/// The oracle raises the **bare** atom `badarg`: `io_lib:format/2` is
/// `try io_lib_format:fwrite/2 catch _:_ -> erlang:error(badarg, [Format, Args])`
/// (`lib/stdlib/src/io_lib.erl:354-360`), so *every* inner failure — `scan/2`'s
/// missing clause and `string:length/1`'s structured `{badarg, Bin}` alike — leaves
/// as the bare atom. This crate raises the bare [`BeamError::BadArg`] for every
/// `scan/2`/`collect/2` reject and for a top-level binary element `string:length/1`
/// cannot decode — that function's own **structured** `{badarg, Bin}` raise is
/// mapped back to the bare atom at the format boundary, exactly as the oracle's
/// wrapper does.
fn check_format_badarg(
    ctx: &mut BifContext<'_>,
    reg: &BifRegistry,
    format: Eterm,
    args: &[Eterm],
) {
    let args_term = ctx.list(args).expect("a short argument list fits");
    let err = reg
        .call("io_lib", "format", 2, ctx, &[format, args_term])
        .expect_err("the format must be rejected");
    match err {
        BeamError::BadArg => {}
        BeamError::Raise(raise) => assert_eq!(
            raise.class,
            ExceptionClass::Error,
            "expected an error-class badarg raise, got {raise:?}"
        ),
        other => panic!("expected a badarg raise, got {other:?}"),
    }
}

/// The format argument is scanned exactly as `io_lib_format:scan/2` does: an
/// **atom**'s name and a **binary**'s raw bytes are ordinary format characters (a
/// `~` inside them *is* a directive), while a **list** contributes one element per
/// cons cell. The former `badarg` for a non-list format is gone.
///
/// ```text
/// erl -noshell -eval 'io:format("~w~n",[io_lib:format(F,A)]),halt().'
/// ```
///
/// | `io_lib:format(F, A)` | result |
/// |---|---|
/// | `(<<"x~n">>, [])` | `[120,10]` |
/// | `(<<"n=~p">>, [42])` | `[110,61,[52,50]]` |
/// | `(<<"~s!~n">>, ["hi"])` | `[[104,105],33,10]` |
/// | `(aaa, [])` | `[97,97,97]` |
/// | `('a~nb', [])` | `[97,10,98]` |
/// | `(atom_nl, [])` | `[97,116,111,109,95,110,108]` |
/// | `(<<>>, [])` | `[]` |
/// | `('', [])` | `[]` |
/// | `([], [])` | `[]` |
/// | `(<<97,1>>, [])` | `[97,1]` |
#[test]
fn the_format_argument_may_be_an_atom_a_binary_or_a_list() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let mut atoms = seeded_atoms();
    atoms.seed(["aaa", "atom_nl", ""]);
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // A binary format: its bytes are format characters, so `~n` *is* a directive.
    let f = binary(&mut ctx, &reg, b"x~n");
    let want = build(&mut ctx, &E::L(vec![E::I(120), E::I(10)]));
    check_format(&mut ctx, &reg, f, &[], want);

    let f = binary(&mut ctx, &reg, b"n=~p");
    let forty_two = int(&mut ctx, 42);
    let want = build(&mut ctx, &E::L(vec![E::I(110), E::I(61), E::S("42")]));
    check_format(&mut ctx, &reg, f, &[forty_two], want);

    let f = binary(&mut ctx, &reg, b"~s!~n");
    let hi = char_list(&mut ctx, "hi");
    // `<<"~s!~n">>` with `["hi"]` ⇒ `[[104,105],33,10]`: `~s` of a list is ONE
    // sub-list element, the `!` and the `~n` newline are flat characters.
    let want = build(
        &mut ctx,
        &E::L(vec![E::S("hi"), E::I(33), E::I(10)]),
    );
    check_format(&mut ctx, &reg, f, &[hi], want);

    // An atom format: its name is scanned the same way, `~` included.
    let f = atom(&mut ctx, "aaa");
    let want = build(&mut ctx, &E::S("aaa"));
    check_format(&mut ctx, &reg, f, &[], want);

    let a_tilde_nb = atom(&mut ctx, "a~nb");
    let want = build(
        &mut ctx,
        &E::L(vec![E::I(97), E::I(10), E::I(98)]),
    );
    check_format(&mut ctx, &reg, a_tilde_nb, &[], want);

    // `atom_nl` carries no `~`, so it is eight literal characters.
    let f = atom(&mut ctx, "atom_nl");
    let want = build(&mut ctx, &E::S("atom_nl"));
    check_format(&mut ctx, &reg, f, &[], want);

    // The empty binary, the empty atom and `[]` are the empty format.
    let f = binary(&mut ctx, &reg, b"");
    let want = build(&mut ctx, &E::L(Vec::new()));
    check_format(&mut ctx, &reg, f, &[], want);

    let f = atom(&mut ctx, "");
    let want = build(&mut ctx, &E::L(Vec::new()));
    check_format(&mut ctx, &reg, f, &[], want);

    let f = ctx.list(&[]).expect("fits");
    let want = build(&mut ctx, &E::L(Vec::new()));
    check_format(&mut ctx, &reg, f, &[], want);

    // A binary with a control character that is not `~`.
    let f = binary(&mut ctx, &reg, &[97, 1]);
    let want = build(&mut ctx, &E::L(vec![E::I(97), E::I(1)]));
    check_format(&mut ctx, &reg, f, &[], want);
}

/// `io:format` with an atom/binary/chardata format writes the flattened text.
///
/// | `io:format(F, A)` | stdout |
/// |---|---|
/// | `(<<"bin ~p~n">>, [1])` | `bin 1\n` |
/// | `(aaa, [])` | `aaa` |
/// | `(atom_nl, [])` | `atom_nl` |
/// | `(<<"x~n">>, [])` | `x\n` |
#[test]
fn io_format_writes_an_atom_binary_or_list_format() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let mut atoms = seeded_atoms();
    atoms.seed(["aaa", "atom_nl"]);
    let mut sink = BytesSink::new();
    {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);

        // `io:format(<<"bin ~p~n">>, [1])` prints `bin 1` + a newline.
        let f = binary(&mut ctx, &reg, b"bin ~p~n");
        let one = int(&mut ctx, 1);
        let args = ctx.list(&[one]).expect("fits");
        reg.call("io", "format", 2, &mut ctx, &[f, args])
            .expect("io:format/2");

        // `io:format(aaa, [])` prints `aaa` (its name is scanned, no directive).
        let f = atom(&mut ctx, "aaa");
        let args = ctx.list(&[]).expect("fits");
        reg.call("io", "format", 2, &mut ctx, &[f, args])
            .expect("io:format/2");

        // `io:format(atom_nl, [])` prints `atom_nl` (a literal `n`, not `~n`).
        let f = atom(&mut ctx, "atom_nl");
        let args = ctx.list(&[]).expect("fits");
        reg.call("io", "format", 2, &mut ctx, &[f, args])
            .expect("io:format/2");

        // `io:format(<<"x~n">>, [])` prints `x` + a newline.
        let f = binary(&mut ctx, &reg, b"x~n");
        let args = ctx.list(&[]).expect("fits");
        reg.call("io", "format", 2, &mut ctx, &[f, args])
            .expect("io:format/2");
    }
    assert_eq!(sink.as_str(), "bin 1\naaaatom_nlx\n");
}

/// A format that is neither an atom, a binary nor a **proper** list — or that
/// `count_small/1` has no clause for — is the oracle's `error:badarg`.
///
/// ```text
/// io_lib:format(1, [])       ⇒ badarg         io_lib:format(1.0, [])    ⇒ badarg
/// io_lib:format({}, [])      ⇒ badarg         io_lib:format({bb,cc},[]) ⇒ badarg
/// io_lib:format(<<1:3>>, []) ⇒ badarg         io_lib:format([1|2], []) ⇒ badarg
/// io_lib:format(foo, [1])    ⇒ badarg   (foo consumes no argument)
/// ```
#[test]
fn an_unscannable_format_is_badarg() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let mut atoms = seeded_atoms();
    atoms.seed(["bb", "cc"]);
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // An integer format.
    let one = int(&mut ctx, 1);
    check_format_badarg(&mut ctx, &reg, one, &[]);

    // A float format.
    let f = float(&mut ctx, 1.0);
    check_format_badarg(&mut ctx, &reg, f, &[]);

    // The empty tuple.
    let f = tuple(&mut ctx, &[]);
    check_format_badarg(&mut ctx, &reg, f, &[]);

    // A two-element tuple.
    let bb = atom(&mut ctx, "bb");
    let cc = atom(&mut ctx, "cc");
    let f = tuple(&mut ctx, &[bb, cc]);
    check_format_badarg(&mut ctx, &reg, f, &[]);

    // An improper format list.
    let one = int(&mut ctx, 1);
    let two = int(&mut ctx, 2);
    let improper = ctx.cons(one, two).expect("fits");
    check_format_badarg(&mut ctx, &reg, improper, &[]);

    // An atom format that leaves an argument unconsumed.
    let foo = atom(&mut ctx, "foo");
    let one = int(&mut ctx, 1);
    check_format_badarg(&mut ctx, &reg, foo, &[one]);

    // A non-byte-aligned bitstring format (`<<1:3>>`) — built through the sibling
    // safe `beam_codec::term` helper, since the crate's own is private.
    drop(ctx);
    let bits = codec_term::make_bitstring(&mut heap, &[0b0010_0000], 3).expect("fits");
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    check_format_badarg(&mut ctx, &reg, bits, &[]);
}

/// A **list** format keeps a nested list/binary element verbatim: it is emitted as
/// one element and is *never* scanned for a directive, so an embedded `~n` stays
/// the two characters `[126,110]` and an integer above 255 stays itself.
///
/// ```text
/// io_lib:format([<<120>>,[126,110]], []) ⇒ [<<120>>,[126,110]]
/// io_lib:format([[[120]],[126,110]], []) ⇒ [[[120]],[126,110]]
/// io_lib:format([<<120>>], [])           ⇒ [<<120>>]
/// io_lib:format([[126,110]], [])         ⇒ [[126,110]]
/// io_lib:format([20013,120], [])       ⇒ [20013,120]  (KNOWN DIVERGENCE: see below)
/// io_lib:format([[[<<"a">>]]], [])       ⇒ [[[<<97>>]]]
/// ```
#[test]
fn a_nested_element_of_a_list_format_is_kept_verbatim() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // `[<<120>>, [126,110]]` — the binary and the nested `"~n"` list are both
    // verbatim, so the result is the format itself.
    let bin120 = binary(&mut ctx, &reg, &[120]);
    let tilde_n = char_list(&mut ctx, "~n");
    let f = ctx.list(&[bin120, tilde_n]).expect("fits");
    let want = ctx.list(&[bin120, tilde_n]).expect("fits");
    check_format(&mut ctx, &reg, f, &[], want);

    // `[[[120]], [126,110]]` — one level deeper, still verbatim.
    let n120 = int(&mut ctx, 120);
    let inner = ctx.list(&[n120]).expect("fits");
    let first = ctx.list(&[inner]).expect("fits");
    let second = char_list(&mut ctx, "~n");
    let f = ctx.list(&[first, second]).expect("fits");
    let want = ctx.list(&[first, second]).expect("fits");
    check_format(&mut ctx, &reg, f, &[], want);

    // A single verbatim binary element.
    let bin120 = binary(&mut ctx, &reg, &[120]);
    let f = ctx.list(&[bin120]).expect("fits");
    let want = ctx.list(&[bin120]).expect("fits");
    check_format(&mut ctx, &reg, f, &[], want);

    // A single verbatim nested-list element.
    let tilde_n = char_list(&mut ctx, "~n");
    let f = ctx.list(&[tilde_n]).expect("fits");
    let want = ctx.list(&[tilde_n]).expect("fits");
    check_format(&mut ctx, &reg, f, &[], want);

    // `[20013, 120]` — 20013 is outside 0..=255, so it is a *verbatim integer*
    // element: `count_small/1`'s `is_integer(C) -> Other + 1` clause counts it as
    // one character while it is emitted verbatim, and 120 stays the literal `x`.
    //     erl -noshell -eval 'io:format("~p~n",[io_lib:format([20013,120],[])]),halt().'
    //     % [20013,120]
    let big = int(&mut ctx, 20013);
    let x = int(&mut ctx, 120);
    let f = ctx.list(&[big, x]).expect("fits");
    let want = ctx.list(&[big, x]).expect("fits");
    check_format(&mut ctx, &reg, f, &[], want);
    // `[[[<<"a">>]]]` — a triple-nested binary element.
    let bin_a = binary(&mut ctx, &reg, b"a");
    let l1 = ctx.list(&[bin_a]).expect("fits");
    let l2 = ctx.list(&[l1]).expect("fits");
    let f = ctx.list(&[l2]).expect("fits");
    let want = ctx.list(&[l2]).expect("fits");
    check_format(&mut ctx, &reg, f, &[], want);
}

/// `io_lib_format:count_small/1` measures a **top-level binary** element with
/// `string:length/1` — grapheme clusters — and raises `{badarg, Bin}` for a binary
/// that is not valid UTF-8. A binary nested inside a list element is measured with
/// `io_lib:chars_length/1` instead and is safe.
///
/// ```text
/// io_lib:format([<<255>>], [])             ⇒ badarg
/// io_lib:format([<<255>>,120], [])         ⇒ badarg
/// io_lib:format([<<128>>], [])             ⇒ badarg
/// io_lib:format([<<1,2,3>>], [])           ⇒ [<<1,2,3>>]
/// io_lib:format([<<195,169>>,120], [])     ⇒ [<<195,169>>,120]
/// io_lib:format([<<195,169>>,126,110], []) ⇒ [<<195,169>>,10]
/// io_lib:format([[<<255>>],120], [])       ⇒ [[<<255>>],120]
/// io_lib:format([[97,<<255>>]], [])        ⇒ [[97,<<255>>]]
/// ```
#[test]
fn a_top_level_binary_format_element_is_measured_by_string_length() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // Invalid UTF-8 at the top level raises on both rendering paths.
    for bad in [&[255u8][..], &[128][..]] {
        let bin = binary(&mut ctx, &reg, bad);
        let f = ctx.list(&[bin]).expect("fits");
        check_format_badarg(&mut ctx, &reg, f, &[]);
    }
    let bin = binary(&mut ctx, &reg, &[255]);
    let x = int(&mut ctx, 120);
    let f = ctx.list(&[bin, x]).expect("fits");
    check_format_badarg(&mut ctx, &reg, f, &[]);

    // A valid UTF-8 binary element is kept verbatim (3 characters).
    let bin = binary(&mut ctx, &reg, &[1, 2, 3]);
    let f = ctx.list(&[bin]).expect("fits");
    let want = ctx.list(&[bin]).expect("fits");
    check_format(&mut ctx, &reg, f, &[], want);

    // `[<<195,169>>,120]` — `x` is a literal character, so the result is the same.
    let bin = binary(&mut ctx, &reg, &[195, 169]);
    let x = int(&mut ctx, 120);
    let f = ctx.list(&[bin, x]).expect("fits");
    let want = ctx.list(&[bin, x]).expect("fits");
    check_format(&mut ctx, &reg, f, &[], want);

    // `[<<195,169>>,126,110]` — here `~n` IS scanned (a top-level format element),
    // so the result carries a newline instead of the two characters.
    let bin = binary(&mut ctx, &reg, &[195, 169]);
    let n126 = int(&mut ctx, 126);
    let n110 = int(&mut ctx, 110);
    let f = ctx.list(&[bin, n126, n110]).expect("fits");
    let nl = int(&mut ctx, 10);
    let want = ctx.list(&[bin, nl]).expect("fits");
    check_format(&mut ctx, &reg, f, &[], want);

    // A **nested** invalid-UTF-8 binary is safe: the outer list is measured by
    // `io_lib:chars_length/1`, which never decodes the inner binary.
    let bin = binary(&mut ctx, &reg, &[255]);
    let inner = ctx.list(&[bin]).expect("fits");
    let x = int(&mut ctx, 120);
    let f = ctx.list(&[inner, x]).expect("fits");
    let want = ctx.list(&[inner, x]).expect("fits");
    check_format(&mut ctx, &reg, f, &[], want);

    let bin = binary(&mut ctx, &reg, &[255]);
    let a = int(&mut ctx, 97);
    let inner = ctx.list(&[a, bin]).expect("fits");
    let f = ctx.list(&[inner]).expect("fits");
    let want = ctx.list(&[inner]).expect("fits");
    check_format(&mut ctx, &reg, f, &[], want);
}
