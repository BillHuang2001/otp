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

//! Integration tests for printing **non-byte-aligned bitstrings** — `io:format`
//! and `io_lib:format` with `~w`/`~p`/`~W`/`~P` — driven through
//! [`BifRegistry::call`](beam_bifs::BifRegistry::call) (the crate's public seam).
//!
//! Every expectation is pinned to the OTP 29 / erts-17.1 oracle, e.g.
//!
//! ```text
//! erl -noshell -eval 'io:format("~w~n",[<<1:1>>]).'          % <<1:1>>
//! erl -noshell -eval 'io:format("~w~n",[<<5:3,3:2>>]).'       % <<23:5>>
//! erl -noshell -eval 'io:format("~w~n",[<<1:9>>]).'           % <<0,1:1>>
//! erl -noshell -eval 'io:format("~w~n",[<<255:12>>]).'        % <<15,15:4>>
//! erl -noshell -eval 'io:format("~w~n",[{<<1:1>>,a}]).'       % {<<1:1>>,a}
//! erl -noshell -eval 'io:format("~w~n",[[<<1:1>>,2]]).'       % [<<1:1>>,2]
//! erl -noshell -eval 'io:format("~p~n",[<<65,1:1>>]).'        % <<65,1:1>>
//! erl -noshell -eval 'io:format("~W~n",[<<1:1>>,1]).'         % <<...>>
//! erl -noshell -eval 'io:format("~W~n",[<<1:1>>,0]).'         % ...
//! erl -noshell -eval 'io:format("~w~n",[io_lib:format("~w",[<<1:1>>])]).'
//!   % [[60,60,[[49],58,[49]],62,62]]
//! ```
//!
//! The `io:format` cases are asserted on the *exact captured bytes* of a
//! [`BytesSink`]; the `io_lib:format` cases on the oracle's exact **deep list**
//! (compared structurally with `beam_term::compare::eq`, so a differing nesting is
//! a failure even when the flattened text agrees). Bitstring arguments are built
//! with the sibling safe `beam_codec::term::make_bitstring` helper (the crate's own
//! bitstring builders are private).

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, BytesSink};
use beam_codec::term as codec_term;
use beam_common::BeamError;
use beam_process::ProcessId;
use beam_term::{compare::eq, AtomTable, Eterm};

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "a", "b"]);
    atoms
}

/// The calling process id every test uses.
fn pid() -> ProcessId {
    ProcessId::new(1, 0)
}

/// Runs `f` over a bare context on `heap` (no output sink), so terms can be built
/// and then handed to a fresh sink context afterwards.
fn build<R>(heap: &mut Heap, atoms: &AtomTable, f: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
    let mut ctx = BifContext::new(heap, atoms, pid());
    f(&mut ctx)
}

/// The format term: a flat proper list of the byte-sized integers of `text`.
fn byte_list(ctx: &mut BifContext<'_>, text: &str) -> Eterm {
    let elems: Vec<Eterm> = text
        .bytes()
        .map(|b| ctx.int(i64::from(b)).expect("a byte fits a small int"))
        .collect();
    ctx.list(&elems).expect("a short format list fits")
}

/// Builds the non-byte-aligned bitstring `<<bytes:n_bits>>`.
fn bitstring(ctx: &mut BifContext<'_>, bytes: &[u8], n_bits: usize) -> Eterm {
    codec_term::make_bitstring(ctx.heap(), bytes, n_bits).expect("the bitstring fits")
}

/// Runs `io:format(Format, Args…)` over a `BytesSink` and returns the captured text.
fn io_text(
    heap: &mut Heap,
    atoms: &AtomTable,
    reg: &BifRegistry,
    format: &str,
    args: &[Eterm],
) -> Result<String, BeamError> {
    let mut sink = BytesSink::new();
    {
        let mut ctx = BifContext::new(heap, atoms, pid()).with_output(&mut sink);
        let format_term = byte_list(&mut ctx, format);
        let args_term = ctx.list(args).expect("a short argument list fits");
        reg.call("io", "format", 2, &mut ctx, &[format_term, args_term])?;
    }
    Ok(String::from_utf8(sink.contents().to_vec()).expect("the rendering is ASCII"))
}

/// A shape of the `io_lib:format` result: `C` one character, `L` a sub-list.
enum N {
    /// A single character element.
    C(i64),
    /// A sub-list element.
    L(Vec<N>),
}

/// The container shape `["<<", [["1"], ":", ["1"]], ">>"]` — i.e. the rendering
/// of `<<1:1>>` (`[[60,60,[[49],58,[49]],62,62]]` decoded).
fn one_bit_container() -> N {
    N::L(vec![
        N::C(60),
        N::C(60),
        N::L(vec![N::L(vec![N::C(49)]), N::C(58), N::L(vec![N::C(49)])]),
        N::C(62),
        N::C(62),
    ])
}

/// Materializes an expected shape as a term on `ctx`'s heap.
fn build_n(ctx: &mut BifContext<'_>, n: &N) -> Eterm {
    match n {
        N::C(value) => ctx.int(*value).expect("a character code fits"),
        N::L(items) => {
            let elems: Vec<Eterm> = items.iter().map(|item| build_n(ctx, item)).collect();
            ctx.list(&elems).expect("a short list fits")
        }
    }
}

/// Asserts that `io_lib:format(Format, Args…)` is the oracle's deep list `expected`.
fn check_io_lib(
    heap: &mut Heap,
    atoms: &AtomTable,
    reg: &BifRegistry,
    format: &str,
    args: &[Eterm],
    expected: &N,
) {
    let mut ctx = BifContext::new(heap, atoms, pid());
    let format_term = byte_list(&mut ctx, format);
    let args_term = ctx.list(args).expect("a short argument list fits");
    let got = reg
        .call("io_lib", "format", 2, &mut ctx, &[format_term, args_term])
        .unwrap_or_else(|err| panic!("io_lib:format({format:?}) failed: {err:?}"));
    let want = build_n(&mut ctx, expected);
    assert!(
        eq(got, want, ctx.atoms()),
        "io_lib:format({format:?}) does not match the oracle's deep list"
    );
}

/// The non-byte-aligned renderings the oracle produces for `~w`/`~p` (text form).
#[test]
fn writes_non_byte_aligned_bitstrings() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();

    // `(bytes, bit length, oracle text)`.
    let cases: &[(&[u8], usize, &str)] = &[
        // `<<1:1>>` — one bit, no whole leading byte.
        (&[0b1000_0000], 1, "<<1:1>>"),
        // `<<5:3,3:2>>` = `10111` ⇒ `<<23:5>>`.
        (&[0b1011_1000], 5, "<<23:5>>"),
        // `<<1:9>>` — one whole leading byte then one trailing bit.
        (&[0x00, 0b1000_0000], 9, "<<0,1:1>>"),
        // `<<255:12>>` = `00001111 1111` ⇒ `<<15,15:4>>`.
        (&[0x0F, 0b1111_0000], 12, "<<15,15:4>>"),
        // `<<0:3>>` — every bit zero.
        (&[0], 3, "<<0:3>>"),
        // `<<2#110:3>>` ⇒ `<<6:3>>`.
        (&[0b1100_0000], 3, "<<6:3>>"),
        // `<<255:9>>` ⇒ `<<127,1:1>>`.
        (&[0b0111_1111, 0b1000_0000], 9, "<<127,1:1>>"),
    ];
    for &(bytes, n_bits, want) in cases {
        let term = build(&mut heap, &atoms, |ctx| bitstring(ctx, bytes, n_bits));
        // `~w` never applies the printable-string rule; `~p` must not either.
        assert_eq!(
            io_text(&mut heap, &atoms, &reg, "~w", &[term]).expect("~w prints the bitstring"),
            want,
            "~w of a {n_bits}-bit bitstring"
        );
        assert_eq!(
            io_text(&mut heap, &atoms, &reg, "~p", &[term]).expect("~p prints the bitstring"),
            want,
            "~p of a {n_bits}-bit bitstring"
        );
    }
}

/// A non-byte-aligned bitstring renders inside a container.
#[test]
fn writes_non_byte_aligned_bitstrings_nested() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();

    let (tuple, list) = build(&mut heap, &atoms, |ctx| {
        let one_bit = bitstring(ctx, &[0b1000_0000], 1);
        let a = ctx.atom("a").expect("seeded");
        let two = ctx.int(2).expect("fits");
        (
            ctx.tuple(&[one_bit, a]).expect("fits"),
            ctx.list(&[one_bit, two]).expect("fits"),
        )
    });

    // `{<<1:1>>,a}` and `[<<1:1>>,2]` (oracle).
    assert_eq!(io_text(&mut heap, &atoms, &reg, "~w", &[tuple]).expect("~w"), "{<<1:1>>,a}");
    assert_eq!(io_text(&mut heap, &atoms, &reg, "~p", &[tuple]).expect("~p"), "{<<1:1>>,a}");
    assert_eq!(io_text(&mut heap, &atoms, &reg, "~w", &[list]).expect("~w"), "[<<1:1>>,2]");
    assert_eq!(io_text(&mut heap, &atoms, &reg, "~p", &[list]).expect("~p"), "[<<1:1>>,2]");
}

/// `~W`/`~P` apply the same depth semantics to a non-byte-aligned bitstring as to
/// any other container: depth `0` ⇒ `...`, depth `1` ⇒ `<<...>>`.
#[test]
fn writes_non_byte_aligned_bitstrings_at_a_depth() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();

    let (one_bit, zero, one, two) = build(&mut heap, &atoms, |ctx| {
        (
            bitstring(ctx, &[0b1000_0000], 1),
            ctx.int(0).expect("fits"),
            ctx.int(1).expect("fits"),
            ctx.int(2).expect("fits"),
        )
    });

    assert_eq!(io_text(&mut heap, &atoms, &reg, "~W", &[one_bit, zero]).expect("~W"), "...");
    assert_eq!(io_text(&mut heap, &atoms, &reg, "~W", &[one_bit, one]).expect("~W"), "<<...>>");
    assert_eq!(io_text(&mut heap, &atoms, &reg, "~P", &[one_bit, zero]).expect("~P"), "...");
    assert_eq!(io_text(&mut heap, &atoms, &reg, "~P", &[one_bit, one]).expect("~P"), "<<...>>");
    // A deeper limit renders the full form (unlimited for this term).
    assert_eq!(io_text(&mut heap, &atoms, &reg, "~W", &[one_bit, two]).expect("~W"), "<<1:1>>");
}

/// A **byte-aligned** bitstring / binary keeps its exact previous rendering — no
/// regression from the non-byte-aligned support.
#[test]
fn writes_byte_aligned_bitstrings_unchanged() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();

    let (binary, abc, empty) = build(&mut heap, &atoms, |ctx| {
        (
            bitstring(ctx, &[0b1000_0000], 8),
            codec_term::make_binary(ctx.heap(), b"abc").expect("fits"),
            codec_term::make_binary(ctx.heap(), b"").expect("fits"),
        )
    });

    // `<<1:1,0:7>>` is exactly the binary `<<128>>`.
    assert_eq!(io_text(&mut heap, &atoms, &reg, "~w", &[binary]).expect("~w"), "<<128>>");
    assert_eq!(io_text(&mut heap, &atoms, &reg, "~p", &[binary]).expect("~p"), "<<128>>");

    // `<<"abc">>` keeps the `~p` printable-string form.
    assert_eq!(io_text(&mut heap, &atoms, &reg, "~w", &[abc]).expect("~w"), "<<97,98,99>>");
    assert_eq!(io_text(&mut heap, &atoms, &reg, "~p", &[abc]).expect("~p"), "<<\"abc\">>");

    // An empty binary is `<<>>` in both forms.
    assert_eq!(io_text(&mut heap, &atoms, &reg, "~w", &[empty]).expect("~w"), "<<>>");
    assert_eq!(io_text(&mut heap, &atoms, &reg, "~p", &[empty]).expect("~p"), "<<>>");
}

/// `io_lib:format` returns the oracle's exact **deep list** for a non-byte-aligned
/// bitstring.
#[test]
fn io_lib_format_deep_list_for_bitstrings() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();

    // `[[60,60,[[49],58,[49]],62,62]]` — `[["<<", [["1"], ":", ["1"]], ">>"]]`.
    let one_bit = build(&mut heap, &atoms, |ctx| bitstring(ctx, &[0b1000_0000], 1));
    check_io_lib(&mut heap, &atoms, &reg, "~w", &[one_bit], &N::L(vec![one_bit_container()]));

    // `~p` produces the same structure (no printable-string rule applies).
    let one_bit = build(&mut heap, &atoms, |ctx| bitstring(ctx, &[0b1000_0000], 1));
    check_io_lib(&mut heap, &atoms, &reg, "~p", &[one_bit], &N::L(vec![one_bit_container()]));

    // `[[60,60,[[48],44,[49],58,[49]],62,62]]` for `<<1:9>>` (`<<0,1:1>>`).
    let nine_bits = build(&mut heap, &atoms, |ctx| bitstring(ctx, &[0x00, 0b1000_0000], 9));
    check_io_lib(
        &mut heap,
        &atoms,
        &reg,
        "~w",
        &[nine_bits],
        &N::L(vec![N::L(vec![
            N::C(60),
            N::C(60),
            N::L(vec![
                N::L(vec![N::C(48)]),
                N::C(44),
                N::L(vec![N::C(49)]),
                N::C(58),
                N::L(vec![N::C(49)]),
            ]),
            N::C(62),
            N::C(62),
        ])]),
    );

    // Nested in a tuple: `[[123,[[60,60,[[49],58,[49]],62,62],44,[97]],125]]`.
    let tuple = build(&mut heap, &atoms, |ctx| {
        let one_bit = bitstring(ctx, &[0b1000_0000], 1);
        let a = ctx.atom("a").expect("seeded");
        ctx.tuple(&[one_bit, a]).expect("fits")
    });
    check_io_lib(
        &mut heap,
        &atoms,
        &reg,
        "~w",
        &[tuple],
        &N::L(vec![N::L(vec![
            N::C(123),
            N::L(vec![
                one_bit_container(),
                N::C(44),
                N::L(vec![N::C(97)]),
            ]),
            N::C(125),
        ])]),
    );
}

/// `~s`/`~ts` of a **non-byte-aligned** bitstring is `badarg` in the oracle, and
/// stays `badarg` here (no invented rendering).
#[test]
fn string_directives_reject_non_byte_aligned_bitstrings() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();

    let one_bit = build(&mut heap, &atoms, |ctx| bitstring(ctx, &[0b1000_0000], 1));
    for format in ["~s", "~ts", "~ts~n"] {
        assert!(
            matches!(
                io_text(&mut heap, &atoms, &reg, format, &[one_bit]),
                Err(BeamError::BadArg)
            ),
            "{format} of a non-byte-aligned bitstring must be badarg"
        );
    }
    // A byte-aligned binary is still printable as a string.
    let abc = build(&mut heap, &atoms, |ctx| {
        codec_term::make_binary(ctx.heap(), b"abc").expect("fits")
    });
    assert_eq!(io_text(&mut heap, &atoms, &reg, "~s", &[abc]).expect("~s"), "abc");
}
