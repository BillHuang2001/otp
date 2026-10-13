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

//! Integration tests for `io_lib:format/3`'s *limiting* `{chars_limit, N}` option
//! over the `~s` directive.
//!
//! `~s` of a **list or binary** is a "big" control: under an active (non-negative)
//! `chars_limit` its text is limited to the per-directive budget
//! (`io_lib_format:control_limited/11` → `iolist_to_chars/3` →
//! `limit_iolist_to_chars/4` → `string/6` → `limit_field/2`), eliding the overflow
//! with a `"..."` segment. `~s` of an **atom** stays a *small* control (its
//! characters splice flat and no control is budgeted). A **negative** limit is the
//! oracle's default (the unlimited path) and must be byte-identical to
//! `io_lib:format/2`.
//!
//! Every golden below is the literal deep list the system oracle (OTP 29.1.1 /
//! erts-17.1) produced for
//!
//! ```text
//! erl -noshell -eval 'io:format("~w~n",[io_lib:format(Fmt, Args, [{chars_limit,L}])]),halt().'
//! ```
//!
//! captured with `~w` so the *structure* — not the printable-string rendering — is
//! visible; the result is compared structurally with `beam_term::compare::eq`, so a
//! differing nested sub-list is a failure even when the flattened text agrees.
//!
//! `string_control_chars_limit_cross_checks_the_live_oracle` re-derives a corpus
//! **live** and compares the `erlang:term_to_binary/1` image of both sides
//! byte-for-byte (printer-free); it **skips loudly** when no oracle is available, so
//! the committed goldens above remain the no-oracle evidence.
//!
//! The tests drive only the public API — [`BifRegistry`]/[`BifContext`] plus the
//! declared dependency crates (`beam_alloc` for the heap, `beam_codec::term` for the
//! binary/bitstring constructors/readers).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_codec::term::{bitstring_bytes, make_binary, make_bitstring};
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{AtomTable, Eterm};

/// The atoms every fixture seeds: the option tag, the atom the oracle side renders
/// for a raising case (`try … catch error:badarg -> badarg end`) and the atom the
/// `~s`-of-an-atom case uses.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["chars_limit", "badarg", "abc", "true", "false", "ok"]);
    atoms
}

/// The expected nested-list shape of an `io_lib:format` result.
///
/// `I(c)` is one element (a character code), `S(text)` a flat character list (a run
/// of individual characters) and `L(items)` a sub-list element — the same shapes
/// `io_lib_format` builds.
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
            let elems: Vec<Eterm> = text
                .bytes()
                .map(|b| ctx.int(i64::from(b)).expect("a character code fits"))
                .collect();
            ctx.list(&elems).expect("a short list fits")
        }
        E::L(items) => {
            let elems: Vec<Eterm> = items.iter().map(|item| build(ctx, item)).collect();
            ctx.list(&elems).expect("a short list fits")
        }
    }
}

/// A small integer on `ctx`'s heap.
fn int(ctx: &mut BifContext<'_>, value: i64) -> Eterm {
    ctx.int(value).expect("a small int fits")
}

/// A seeded atom on `ctx`'s heap.
fn atom(ctx: &mut BifContext<'_>, name: &str) -> Eterm {
    ctx.atom(name).expect("the atom is seeded")
}

/// A proper list of `items` on `ctx`'s heap.
fn list(ctx: &mut BifContext<'_>, items: &[Eterm]) -> Eterm {
    ctx.list(items).expect("a short list fits")
}

/// The format term: a flat proper list of the byte-sized integers of `bytes`.
fn byte_list(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
    let elems: Vec<Eterm> = bytes
        .iter()
        .map(|&b| ctx.int(i64::from(b)).expect("a character code fits"))
        .collect();
    ctx.list(&elems).expect("a short format list fits")
}

/// A proper list of the byte-sized integers of `text` (an Erlang string).
fn char_list(ctx: &mut BifContext<'_>, text: &str) -> Eterm {
    byte_list(ctx, text.as_bytes())
}

/// A byte-aligned binary `<<bytes>>` on `ctx`'s heap.
fn binary(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
    make_binary(ctx.heap(), bytes).expect("the binary fits")
}

/// A float term on `ctx`'s heap.
fn float(ctx: &mut BifContext<'_>, value: f64) -> Eterm {
    ctx.make_float(value).expect("the float fits")
}

/// Runs `io_lib:format(Format, Args, [{chars_limit, Limit}])` through the real
/// registry and returns its result term.
fn format_limited(
    ctx: &mut BifContext<'_>,
    reg: &BifRegistry,
    format: &str,
    limit: i64,
    args: &[Eterm],
) -> Result<Eterm> {
    let format_term = byte_list(ctx, format.as_bytes());
    let args_term = ctx.list(args).expect("a short argument list fits");
    let tag = atom(ctx, "chars_limit");
    let limit_term = int(ctx, limit);
    let option = ctx.tuple(&[tag, limit_term]).expect("a short tuple fits");
    let options = ctx.list(&[option]).expect("a short options list fits");
    reg.call("io_lib", "format", 3, ctx, &[format_term, args_term, options])
}

/// Runs the unlimited `io_lib:format(Format, Args)` reference.
fn format_2(
    ctx: &mut BifContext<'_>,
    reg: &BifRegistry,
    format: &str,
    args: &[Eterm],
) -> Result<Eterm> {
    let format_term = byte_list(ctx, format.as_bytes());
    let args_term = ctx.list(args).expect("a short argument list fits");
    reg.call("io_lib", "format", 2, ctx, &[format_term, args_term])
}

/// Asserts that `io_lib:format(Format, Args, [{chars_limit, Limit}])` is the
/// oracle's deep list `expected`.
fn check_limited(
    ctx: &mut BifContext<'_>,
    reg: &BifRegistry,
    format: &str,
    limit: i64,
    args: impl FnOnce(&mut BifContext<'_>) -> Vec<Eterm>,
    expected: &E,
) {
    let arg_terms = args(ctx);
    let got = format_limited(ctx, reg, format, limit, &arg_terms)
        .unwrap_or_else(|e| panic!("io_lib:format({format:?}, limit {limit}) failed: {e:?}"));
    let want = build(ctx, expected);
    assert!(
        beam_term::compare::eq(got, want, ctx.atoms()),
        "io_lib:format({format:?}, limit {limit}) does not match the oracle's deep list"
    );
}

/// Asserts a call failed with `badarg` (the oracle's `function_clause`).
fn assert_badarg(result: Result<Eterm>, what: &str) {
    match result {
        Err(BeamError::BadArg) => {}
        other => panic!("{what}: expected badarg, got {other:?}"),
    }
}

/// A list of `[97,98,99]` (`"abc"`) — the argument for a one-`~s` format.
fn args_abc(ctx: &mut BifContext<'_>) -> Vec<Eterm> {
    vec![char_list(ctx, "abc")]
}

/// The argument for a one-`~s` format: the improper list `[97,98|99]`.
fn args_improper(ctx: &mut BifContext<'_>) -> Vec<Eterm> {
    let head = int(ctx, 97);
    let n98 = int(ctx, 98);
    let n99 = int(ctx, 99);
    let tail = ctx.cons(n98, n99).expect("a cons cell fits");
    vec![ctx.cons(head, tail).expect("a cons cell fits")]
}

/// The argument for a one-`~s` format: `[1.5]` (a float element).
fn args_float(ctx: &mut BifContext<'_>) -> Vec<Eterm> {
    let f = float(ctx, 1.5);
    vec![list(ctx, &[f])]
}

/// The argument for a one-`~s` format: `[400]` (an out-of-range integer).
fn args_out_of_range(ctx: &mut BifContext<'_>) -> Vec<Eterm> {
    let n = int(ctx, 400);
    vec![list(ctx, &[n])]
}

/// The argument for a one-`~s` format: `<<1:3>>` (a non-byte-aligned bitstring).
fn args_bitstring(ctx: &mut BifContext<'_>) -> Vec<Eterm> {
    vec![make_bitstring(ctx.heap(), &[0b0010_0000], 3).expect("the bitstring fits")]
}

/// The argument for a one-`~s` format: `[<<"abcdefghij">>]` (a long binary).
fn args_long_binary(ctx: &mut BifContext<'_>) -> Vec<Eterm> {
    let bin = binary(ctx, b"abcdefghij");
    vec![list(ctx, &[bin])]
}

/// The argument for a one-`~s` format: the nested iolist `[<<"ab">>,[$c,$d]]`.
fn args_nested_iolist(ctx: &mut BifContext<'_>) -> Vec<Eterm> {
    let bin = binary(ctx, b"ab");
    let c_ = int(ctx, 99);
    let d = int(ctx, 100);
    let cd = list(ctx, &[c_, d]);
    vec![list(ctx, &[bin, cd])]
}

/// The arguments for `~s ~w`: `"abc"` and `[1,2,3]`.
fn args_abc_and_list(ctx: &mut BifContext<'_>) -> Vec<Eterm> {
    let s = char_list(ctx, "abc");
    let one = int(ctx, 1);
    let two = int(ctx, 2);
    let three = int(ctx, 3);
    let l = list(ctx, &[one, two, three]);
    vec![s, l]
}

// ---------------------------------------------------------------------------
// Committed goldens (no oracle needed to run)
// ---------------------------------------------------------------------------

/// `~s` of a short list survives every budget (a `0` budget probes a further four
/// characters, so a 3-character string fits), while a longer list elides its
/// overflow with `"..."`.
#[test]
fn a_list_under_a_limit_is_kept_whole_or_elided() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // `io_lib:format("~s", ["abc"], [{chars_limit,L}])` => `[[97,98,99]]` for
    // L = 0, 3 and 100 on the oracle.
    for limit in [0, 3, 100] {
        check_limited(
            &mut ctx,
            &reg,
            "~s",
            limit,
            args_abc,
            &E::L(vec![E::S("abc")]),
        );
    }

    // `io_lib:format("~s", ["hello world!!"], [{chars_limit,10}])` => `["hello w..."]`,
    // i.e. the deep list `[[104,101,108,108,111,32,119,46,46,46]]` (the text is cut
    // at 7 characters and the elision marker splices into the same run).
    check_limited(
        &mut ctx,
        &reg,
        "~s",
        10,
        |c| vec![char_list(c, "hello world!!")],
        &E::L(vec![E::S("hello w...")]),
    );

    // A budget the text fits into keeps it whole.
    check_limited(
        &mut ctx,
        &reg,
        "~s",
        100,
        |c| vec![char_list(c, "hello world!!")],
        &E::L(vec![E::S("hello world!!")]),
    );
}

/// `~s` of a **binary** is cut at the budget: the prefix becomes a sub-list and the
/// overflow is re-walked with a `0` budget, so the `"..."` marker is a *separate*
/// element run (unlike the fully-flat list case).
#[test]
fn a_binary_under_a_limit_is_cut_and_elided() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // `io_lib:format("~s", [<<"hello world!!">>], [{chars_limit,10}])` =>
    // `[[[104,101,108,108,111,32,119],46,46,46]]`.
    check_limited(
        &mut ctx,
        &reg,
        "~s",
        10,
        |c| vec![binary(c, b"hello world!!")],
        &E::L(vec![E::L(vec![
            E::S("hello w"),
            E::I(46),
            E::I(46),
            E::I(46),
        ])]),
    );

    // `io_lib:format("~s", [<<"abc">>], [{chars_limit,5}])` => `[[[97,98],[99]]]`: the
    // 5-character budget fits `ab` (`CharsLimit - 3`), the tail `c` is re-walked with
    // a `0` budget and survives (the four-character probe).
    check_limited(
        &mut ctx,
        &reg,
        "~s",
        5,
        |c| vec![binary(c, b"abc")],
        &E::L(vec![E::L(vec![E::S("ab"), E::S("c")])]),
    );

    // `io_lib:format("~s", [<<"abc">>], [{chars_limit,3}])` => `[[[97,98,99]]]`.
    check_limited(
        &mut ctx,
        &reg,
        "~s",
        3,
        |c| vec![binary(c, b"abc")],
        &E::L(vec![E::L(vec![E::S("abc")])]),
    );
}

/// A nested iolist keeps its own nesting: a binary prefix stays a sub-list while a
/// nested list splices flat — the oracle's `[[[97,98],99,100]]`.
#[test]
fn a_nested_iolist_under_a_limit_keeps_its_nesting() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // `io_lib:format("~s", [[<<"ab">>,[$c,$d]]], [{chars_limit,100}])` =>
    // `[[[97,98],99,100]]` (the binary prefix is its own sub-list, the nested list
    // splices).
    check_limited(
        &mut ctx,
        &reg,
        "~s",
        100,
        args_nested_iolist,
        &E::L(vec![E::L(vec![E::S("ab"), E::I(99), E::I(100)])]),
    );

    // `{chars_limit,3}` leaves no room, so the whole iolist elides to `"..."`:
    // `io_lib:format("~s", [[<<"ab">>,[$c,$d]]], [{chars_limit,3}])` => `[[46,46,46]]`.
    check_limited(
        &mut ctx,
        &reg,
        "~s",
        3,
        args_nested_iolist,
        &E::L(vec![E::S("...")]),
    );

    // A single long binary `[<<"abcdefghij">>]` with `{chars_limit,6}` =>
    // `[[[97,98,99],46,46,46]]`.
    check_limited(
        &mut ctx,
        &reg,
        "~s",
        6,
        args_long_binary,
        &E::L(vec![E::L(vec![
            E::S("abc"),
            E::I(46),
            E::I(46),
            E::I(46),
        ])]),
    );
}

/// The budget elision produces a `"..."` segment: the literal text is charged first
/// and is never elided, and `~s`/`~w` share one per-directive budget.
#[test]
fn budget_elision_emits_a_three_dot_segment() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // `io_lib:format("hello ~s world", ["abcdefghij"], [{chars_limit,10}])` =>
    // `[104,101,108,108,111,32,[46,46,46],32,119,111,114,108,100]` — only the `~s`
    // elides (as one sub-list element); the literal characters are individual
    // elements and are charged against the budget first.
    check_limited(
        &mut ctx,
        &reg,
        "hello ~s world",
        10,
        |c| vec![char_list(c, "abcdefghij")],
        &E::L(vec![
            E::I(104),
            E::I(101),
            E::I(108),
            E::I(108),
            E::I(111),
            E::I(32),
            E::S("..."),
            E::I(32),
            E::I(119),
            E::I(111),
            E::I(114),
            E::I(108),
            E::I(100),
        ]),
    );

    // `io_lib:format("~s ~w", ["abc",[1,2,3]], [{chars_limit,6}])` =>
    // `[[97,98,99],32,[91,[46,46,46],93]]`: `~s` and `~w` share the `w` bucket, so
    // `~s` gets 3 (`"abc"`) and `~w` the rest, eliding its list.
    check_limited(
        &mut ctx,
        &reg,
        "~s ~w",
        6,
        args_abc_and_list,
        &E::L(vec![
            E::S("abc"),
            E::I(32),
            E::L(vec![E::I(91), E::S("..."), E::I(93)]),
        ]),
    );
}

/// The directive's field width is clamped to the budget (`limit_field/2`), so a
/// wider field is truncated and a too-small width is raised to 3; a precision cuts
/// the limited text the same way.
#[test]
fn field_width_and_precision_cut_the_limited_text() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // `io_lib:format("~8s", ["abc"], [{chars_limit,6}])` => `[["   ",97,98,99]]`: the
    // width 8 is clamped to the 6-budget, padding to 3 spaces + `abc`.
    check_limited(
        &mut ctx,
        &reg,
        "~8s",
        6,
        args_abc,
        &E::L(vec![E::L(vec![
            E::S("   "),
            E::I(97),
            E::I(98),
            E::I(99),
        ])]),
    );

    // `io_lib:format("~2s", ["hello"], [{chars_limit,10}])` => `["hel"]`: the width 2
    // is raised to 3 (`limit_field`'s floor) and truncates.
    check_limited(
        &mut ctx,
        &reg,
        "~2s",
        10,
        |c| vec![char_list(c, "hello")],
        &E::L(vec![E::S("hel")]),
    );

    // `io_lib:format("~.2s", ["hello"], [{chars_limit,10}])` => `["he"]` (precision,
    // no field).
    check_limited(
        &mut ctx,
        &reg,
        "~.2s",
        10,
        |c| vec![char_list(c, "hello")],
        &E::L(vec![E::S("he")]),
    );

    // `io_lib:format("~4s", ["hello world"], [{chars_limit,10}])` => `["hell"]`.
    check_limited(
        &mut ctx,
        &reg,
        "~4s",
        10,
        |c| vec![char_list(c, "hello world")],
        &E::L(vec![E::S("hell")]),
    );
}

/// An invalid or unprintable `~s` iolist is the oracle's `function_clause`, i.e. the
/// observable `badarg`, never a wrong rendering.
#[test]
fn an_invalid_iolist_is_badarg() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // `[97,98|99]` — an improper list.
    let args = args_improper(&mut ctx);
    assert_badarg(
        format_limited(&mut ctx, &reg, "~s", 5, &args),
        "~s of [97,98|99]",
    );

    // `[1.5]` — a float element.
    let args = args_float(&mut ctx);
    assert_badarg(
        format_limited(&mut ctx, &reg, "~s", 5, &args),
        "~s of [1.5]",
    );

    // `[400]` — an integer outside the byte range.
    let args = args_out_of_range(&mut ctx);
    assert_badarg(
        format_limited(&mut ctx, &reg, "~s", 5, &args),
        "~s of [400]",
    );

    // `<<1:3>>` — a non-byte-aligned bitstring.
    let args = args_bitstring(&mut ctx);
    assert_badarg(
        format_limited(&mut ctx, &reg, "~s", 5, &args),
        "~s of <<1:3>>",
    );
}

/// `~s` of an **atom** is a *small* control: its characters splice flat and no
/// control is budgeted, so an active limit is a no-op.
#[test]
fn string_control_of_an_atom_is_a_small_control() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // `io_lib:format("~s", [abc], [{chars_limit,100}])` => `[97,98,99]` (flat).
    check_limited(
        &mut ctx,
        &reg,
        "~s",
        100,
        |c| vec![atom(c, "abc")],
        &E::S("abc"),
    );
}

/// A very large limit keeps the text whole; a **negative** limit is the oracle's
/// default — the unlimited path, byte-identical to `io_lib:format/2`.
#[test]
fn a_large_or_negative_limit_does_not_elide() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // `io_lib:format("~s", ["hello world!!"], [{chars_limit,1000000}])` =>
    // `[[104,…,33,33]]`, the plain unlimited deep list.
    check_limited(
        &mut ctx,
        &reg,
        "~s",
        1_000_000,
        |c| vec![char_list(c, "hello world!!")],
        &E::L(vec![E::S("hello world!!")]),
    );

    // A negative limit selects the unlimited path: `format/3` with `{chars_limit,-1}`
    // is structurally identical to `format/2`.
    let args: Vec<Eterm> = vec![char_list(&mut ctx, "hello world!!")];
    let limited = format_limited(&mut ctx, &reg, "~s", -1, &args).expect("renders");
    let plain = format_2(&mut ctx, &reg, "~s", &args).expect("renders");
    assert!(
        beam_term::compare::eq(limited, plain, ctx.atoms()),
        "a negative chars_limit must be byte-identical to io_lib:format/2"
    );
}

// ---------------------------------------------------------------------------
// The live-oracle cross-check (byte-exact, printer-free; skips loudly)
// ---------------------------------------------------------------------------

/// One case of the live-oracle corpus: an `io_lib:format(Fmt, Args, [{chars_limit,
/// L}])` call described once in Erlang (`format` + `erl_args` + `limit`) and once
/// in Rust (`build`), so the two halves cannot drift apart silently.
struct OracleCase {
    /// The case's name, used in failure messages.
    name: &'static str,
    /// The Erlang source of the format string, without its quotes (e.g. `~8s`).
    format: &'static str,
    /// The Erlang source of the argument list (e.g. `["abc"]`) — must describe the
    /// same terms `build` produces.
    erl_args: &'static str,
    /// The `{chars_limit, L}` value.
    limit: i64,
    /// Builds the argument list on the heap (the Rust half).
    build: Box<dyn Fn(&mut BifContext<'_>, &BifRegistry) -> Vec<Eterm>>,
}

/// The live-oracle corpus: every golden the committed tests pin, plus the raising
/// inputs, described so both sides compute the same call.
fn oracle_corpus() -> Vec<OracleCase> {
    vec![
        OracleCase {
            name: "short-list/limit-0",
            format: "~s",
            erl_args: "[[97,98,99]]",
            limit: 0,
            build: Box::new(|c, _r| vec![char_list(c, "abc")]),
        },
        OracleCase {
            name: "short-list/limit-3",
            format: "~s",
            erl_args: "[[97,98,99]]",
            limit: 3,
            build: Box::new(|c, _r| vec![char_list(c, "abc")]),
        },
        OracleCase {
            name: "short-list/limit-100",
            format: "~s",
            erl_args: "[[97,98,99]]",
            limit: 100,
            build: Box::new(|c, _r| vec![char_list(c, "abc")]),
        },
        OracleCase {
            name: "long-list/limit-10",
            format: "~s",
            erl_args: "[\"hello world!!\"]",
            limit: 10,
            build: Box::new(|c, _r| vec![char_list(c, "hello world!!")]),
        },
        OracleCase {
            name: "long-list/limit-100",
            format: "~s",
            erl_args: "[\"hello world!!\"]",
            limit: 100,
            build: Box::new(|c, _r| vec![char_list(c, "hello world!!")]),
        },
        OracleCase {
            name: "binary/limit-10",
            format: "~s",
            erl_args: "[<<\"hello world!!\">>]",
            limit: 10,
            build: Box::new(|c, _r| vec![binary(c, b"hello world!!")]),
        },
        OracleCase {
            name: "binary/limit-5",
            format: "~s",
            erl_args: "[<<\"abc\">>]",
            limit: 5,
            build: Box::new(|c, _r| vec![binary(c, b"abc")]),
        },
        OracleCase {
            name: "binary/limit-3",
            format: "~s",
            erl_args: "[<<\"abc\">>]",
            limit: 3,
            build: Box::new(|c, _r| vec![binary(c, b"abc")]),
        },
        OracleCase {
            name: "nested-iolist/limit-100",
            format: "~s",
            erl_args: "[[<<\"ab\">>,[$c,$d]]]",
            limit: 100,
            build: Box::new(|c, _r| args_nested_iolist(c)),
        },
        OracleCase {
            name: "nested-iolist/limit-3",
            format: "~s",
            erl_args: "[[<<\"ab\">>,[$c,$d]]]",
            limit: 3,
            build: Box::new(|c, _r| args_nested_iolist(c)),
        },
        OracleCase {
            name: "long-binary/limit-6",
            format: "~s",
            erl_args: "[[<<\"abcdefghij\">>]]",
            limit: 6,
            build: Box::new(|c, _r| args_long_binary(c)),
        },
        OracleCase {
            name: "field-width/limit-6",
            format: "~8s",
            erl_args: "[\"abc\"]",
            limit: 6,
            build: Box::new(|c, _r| vec![char_list(c, "abc")]),
        },
        OracleCase {
            name: "width-2/limit-10",
            format: "~2s",
            erl_args: "[\"hello\"]",
            limit: 10,
            build: Box::new(|c, _r| vec![char_list(c, "hello")]),
        },
        OracleCase {
            name: "precision/limit-10",
            format: "~.2s",
            erl_args: "[\"hello\"]",
            limit: 10,
            build: Box::new(|c, _r| vec![char_list(c, "hello")]),
        },
        OracleCase {
            name: "width-4/limit-10",
            format: "~4s",
            erl_args: "[\"hello world\"]",
            limit: 10,
            build: Box::new(|c, _r| vec![char_list(c, "hello world")]),
        },
        OracleCase {
            name: "literal-text/limit-10",
            format: "hello ~s world",
            erl_args: "[\"abcdefghij\"]",
            limit: 10,
            build: Box::new(|c, _r| vec![char_list(c, "abcdefghij")]),
        },
        OracleCase {
            name: "shared-budget/limit-6",
            format: "~s ~w",
            erl_args: "[\"abc\",[1,2,3]]",
            limit: 6,
            build: Box::new(|c, _r| args_abc_and_list(c)),
        },
        OracleCase {
            name: "atom/limit-100",
            format: "~s",
            erl_args: "[abc]",
            limit: 100,
            build: Box::new(|c, _r| vec![atom(c, "abc")]),
        },
        OracleCase {
            name: "negative-limit",
            format: "~s",
            erl_args: "[\"hello world!!\"]",
            limit: -1,
            build: Box::new(|c, _r| vec![char_list(c, "hello world!!")]),
        },
        OracleCase {
            name: "huge-limit",
            format: "~s",
            erl_args: "[\"hello world!!\"]",
            limit: 1_000_000,
            build: Box::new(|c, _r| vec![char_list(c, "hello world!!")]),
        },
        OracleCase {
            name: "error/improper-list",
            format: "~s",
            erl_args: "[[97,98|99]]",
            limit: 5,
            build: Box::new(|c, _r| args_improper(c)),
        },
        OracleCase {
            name: "error/float-element",
            format: "~s",
            erl_args: "[[1.5]]",
            limit: 5,
            build: Box::new(|c, _r| args_float(c)),
        },
        OracleCase {
            name: "error/out-of-range-integer",
            format: "~s",
            erl_args: "[[400]]",
            limit: 5,
            build: Box::new(|c, _r| args_out_of_range(c)),
        },
        OracleCase {
            name: "error/non-byte-aligned-bitstring",
            format: "~s",
            erl_args: "[<<1:3>>]",
            limit: 5,
            build: Box::new(|c, _r| args_bitstring(c)),
        },
    ]
}

/// The `erl -eval` body for the corpus: one lowercase-hex line per case, the hex of
/// `erlang:term_to_binary/1` of the call's result (or of the atom `badarg` when the
/// call raises — `try … catch error:badarg -> badarg end`, so a raising case is
/// byte-comparable without a stack trace).
fn oracle_eval(cases: &[OracleCase]) -> String {
    let mut body = String::from(
        "Hex = fun(B) -> lists:flatten([io_lib:format(\"~2.16.0b\", [X]) \
         || X <- binary_to_list(B)]) end, ",
    );
    for case in cases {
        body.push_str(&format!(
            "io:format(\"~s~n\", [Hex(erlang:term_to_binary((try io_lib:format(\"{}\", {}, \
             [{{chars_limit,{}}}]) catch error:badarg -> badarg end)))]), ",
            case.format, case.erl_args, case.limit,
        ));
    }
    body.push_str("halt().");
    body
}

/// The lowercase hex of `term`'s `erlang:term_to_binary/1` image.
fn term_to_binary_hex(ctx: &mut BifContext<'_>, reg: &BifRegistry, term: Eterm) -> String {
    let image = reg
        .call("erlang", "term_to_binary", 1, ctx, &[term])
        .expect("erlang:term_to_binary/1");
    let (bytes, offset, size_bits) = bitstring_bytes(image).expect("a binary result");
    assert_eq!(offset, 0, "the image is a whole-byte binary");
    assert_eq!(size_bits % 8, 0, "the image is whole-byte aligned");
    let data = &bytes[..size_bits / 8];
    let mut out = String::with_capacity(data.len() * 2);
    for &byte in data {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// `$ERTS_ORACLE` if it names an existing executable, else `erl` on `$PATH` — the
/// same resolution `tools/differential` uses.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_bifs io_lib chars_limit] $ERTS_ORACLE='{}' is not an existing \
             executable — treating it as no oracle.",
            program.display()
        );
        return None;
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join("erl");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Runs one `erl -noshell -eval <eval>` (the eval must `halt()`) and returns stdout.
fn run_erl(oracle: &Path, eval: &str) -> String {
    let output = Command::new(oracle)
        .arg("-noshell")
        .arg("-eval")
        .arg(eval)
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", oracle.display()));
    assert!(
        output.status.success(),
        "the oracle '{}' exited with {:?}\n--- stderr ---\n{}\n--- eval ---\n{eval}",
        oracle.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout)
        .unwrap_or_else(|e| panic!("the oracle printed non-UTF-8 to stdout: {e}"))
}

/// Re-derives the whole corpus against the **live** oracle and requires the
/// `erlang:term_to_binary/1` image of every result to agree byte-for-byte. Skips
/// loudly when no oracle is available.
#[test]
fn string_control_chars_limit_cross_checks_the_live_oracle() {
    let Some(oracle) = resolve_oracle() else {
        eprintln!();
        eprintln!("===== beam_bifs io_lib chars_limit: live oracle SKIPPED =====");
        eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or not");
        eprintln!("executable, and no `erl` on $PATH).");
        eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP).");
        eprintln!("=============================================================");
        eprintln!();
        return;
    };

    let cases = oracle_corpus();
    let eval = oracle_eval(&cases);
    let stdout = run_erl(&oracle, &eval);
    let mut lines = stdout.lines().filter(|line| !line.trim().is_empty());
    let reg = default_registry();

    for case in &cases {
        let line = lines.next().unwrap_or_else(|| {
            panic!(
                "the oracle printed fewer lines than the corpus at case `{}`:\n{stdout}",
                case.name
            )
        });
        // A fresh heap + atom table per case keeps the term images independent.
        let mut heap = Heap::new(1 << 20).expect("test heap");
        let atoms = seeded_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        let arg_terms = (case.build)(&mut ctx, &reg);
        let term = match format_limited(&mut ctx, &reg, case.format, case.limit, &arg_terms) {
            Ok(term) => term,
            Err(BeamError::BadArg) => atom(&mut ctx, "badarg"),
            Err(other) => panic!("case `{}`: unexpected error {other:?}", case.name),
        };
        let got = term_to_binary_hex(&mut ctx, &reg, term);
        assert_eq!(
            got,
            line.trim(),
            "case `{}` disagrees with the live oracle (erlang:term_to_binary image)",
            case.name
        );
    }

    assert!(
        lines.next().is_none(),
        "the oracle printed more lines than the corpus:\n{stdout}"
    );
}
