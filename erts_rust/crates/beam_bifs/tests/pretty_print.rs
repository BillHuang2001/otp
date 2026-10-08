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

//! Integration tests for the `~p`/`~P` **re-flow** (`io_lib_pretty` line break).
//!
//! The oracle prints a container under `~p`/`~P` with `io_lib_pretty`, which breaks
//! it across lines when its single-line rendering is **at least**
//! `line_length - column` characters long (80 and 1 by default, i.e. 79). This
//! crate reproduces that break through a faithful port of `io_lib_pretty`'s
//! template/`pp`/`cind` machinery, so **nested containers** (`[[1,2,3],[4,5,6]]`)
//! and a **depth-limited `~P`** (`~P [[1,2,3],[4,5,6]] 2` ⇒ `[[...]|...]`) re-flow
//! exactly like the oracle. The only re-flow form still refused with a typed
//! `error:badarg` is the left-adjusted `~-<n>p`, which `io_lib_format:print/12` has
//! no clause for (the oracle raises `badarg` for it too).
//!
//! Every expectation is the literal output of
//!
//! ```text
//! erl -noshell -eval 'io:format("~p", [Term]), halt().'
//! ```
//!
//! on the OTP 29 / erts-17.1 oracle. `io:format` is asserted on the exact capture
//! bytes; `io_lib:format` on the deep list's flattening AND — for the narrowest
//! repro and the nested containers — its exact nesting. Map goldens use integer keys,
//! whose order both the oracle and this crate agree on (OTP leaves map order
//! unspecified).
use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, BytesSink};
use beam_common::BeamError;
use beam_process::ProcessId;
use beam_term::{AtomTable, Eterm};

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "a", "b", "c", "x", "xy"]);
    atoms
}

/// A flat proper list of the byte-sized integers of `bytes` (an Erlang string).
fn byte_list(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
    let mut elems = Vec::with_capacity(bytes.len());
    for &b in bytes {
        elems.push(ctx.int(i64::from(b)).expect("a byte fits a small int"));
    }
    ctx.list(&elems).expect("a short list fits")
}

/// A binary built through the registered `erlang:list_to_binary/1`.
fn binary(ctx: &mut BifContext<'_>, reg: &BifRegistry, bytes: &[u8]) -> Eterm {
    let list = byte_list(ctx, bytes);
    reg.call("erlang", "list_to_binary", 1, ctx, &[list])
        .expect("list_to_binary fits")
}

/// Runs `io:format(Fmt, [Arg])` through a `BytesSink`; returns `(bytes, result)`.
fn run_io(
    reg: &BifRegistry,
    fmt: &[u8],
    build: impl FnOnce(&mut BifContext<'_>, &BifRegistry) -> Eterm,
) -> (Vec<u8>, Result<(), BeamError>) {
    run_io_args(reg, fmt, |ctx, reg| vec![build(ctx, reg)])
}

/// Runs `io:format(Fmt, Args)` through a `BytesSink`; returns `(bytes, result)`.
fn run_io_args(
    reg: &BifRegistry,
    fmt: &[u8],
    build: impl FnOnce(&mut BifContext<'_>, &BifRegistry) -> Vec<Eterm>,
) -> (Vec<u8>, Result<(), BeamError>) {
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut sink = BytesSink::new();
    let result;
    {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
        let format = byte_list(&mut ctx, fmt);
        let args = build(&mut ctx, reg);
        let args = ctx.list(&args).expect("a short arg list fits");
        result = reg
            .call("io", "format", 2, &mut ctx, &[format, args])
            .map(|_| ());
    }
    (sink.contents().to_vec(), result)
}

/// Renders `io_lib:format(Fmt, Args)`'s **deep list** as text with `~w`, so the
/// exact nesting is asserted (not just the flattened characters). This is the same
/// dump form `tests/io_lib.rs` uses.
fn deep_text(
    reg: &BifRegistry,
    fmt: &[u8],
    build: impl FnOnce(&mut BifContext<'_>, &BifRegistry) -> Vec<Eterm>,
) -> Result<String, BeamError> {
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut sink = BytesSink::new();
    {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
        let format = byte_list(&mut ctx, fmt);
        let args = build(&mut ctx, reg);
        let args = ctx.list(&args).expect("a short arg list fits");
        let deep = reg.call("io_lib", "format", 2, &mut ctx, &[format, args])?;
        let dump = byte_list(&mut ctx, b"~w");
        let dump_args = ctx.list(&[deep]).expect("a short arg list fits");
        reg.call("io", "format", 2, &mut ctx, &[dump, dump_args])?;
    }
    Ok(String::from_utf8(sink.contents().to_vec()).expect("ASCII"))
}

/// Runs `io_lib:format("~p", [Arg])` in the **caller's** heap (so the returned
/// deep list outlives this call) and returns it.
fn run_io_lib_p(
    reg: &BifRegistry,
    heap: &mut Heap,
    atoms: &AtomTable,
    build: impl FnOnce(&mut BifContext<'_>) -> Eterm,
) -> Result<Eterm, BeamError> {
    let mut ctx = BifContext::new(heap, atoms, ProcessId::new(1, 0));
    let format = byte_list(&mut ctx, b"~p");
    let arg = build(&mut ctx);
    let args = ctx.list(&[arg]).expect("a short arg list fits");
    reg.call("io_lib", "format", 2, &mut ctx, &[format, args])
}

/// A proper list of the small integers `range`.
fn int_list(ctx: &mut BifContext<'_>, range: std::ops::RangeInclusive<i64>) -> Eterm {
    let elems: Vec<Eterm> = range.map(|i| ctx.int(i).expect("fits")).collect();
    ctx.list(&elems).expect("a short list fits")
}

/// `lists:seq(1,28) ++ [Tail]`.
fn seq_28_plus(ctx: &mut BifContext<'_>, tail: Eterm) -> Eterm {
    let mut elems: Vec<Eterm> = (1..=28).map(|i| ctx.int(i).expect("fits")).collect();
    elems.push(tail);
    ctx.list(&elems).expect("fits")
}

#[test]
fn narrowest_repro_breaks_after_the_28th_element() {
    // 79 characters, just at the oracle's `line_length - column` boundary:
    //   [1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,
    //    29]
    let reg = default_registry();
    let (out, result) = run_io(&reg, b"~p", |ctx, _| int_list(ctx, 1..=29));
    assert!(result.is_ok(), "a 79-char list re-flows: {result:?}");
    assert_eq!(
        String::from_utf8(out).expect("ASCII"),
        "[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,\n 29]"
    );
}

#[test]
fn the_boundary_is_the_oracles_flat_condition() {
    // The oracle's flat condition is `Len < 80 - 1 = 79`. A 78-character rendering
    // stays on one line; a 79-character one breaks.
    let reg = default_registry();

    let (out, result) = run_io(&reg, b"~p", |ctx, _| {
        let x = ctx.atom("x").expect("seeded");
        seq_28_plus(ctx, x)
    });
    assert!(result.is_ok(), "78 chars stay flat: {result:?}");
    assert_eq!(
        String::from_utf8(out).expect("ASCII"),
        "[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,x]"
    );

    let (out, result) = run_io(&reg, b"~p", |ctx, _| {
        let xy = ctx.atom("xy").expect("seeded");
        seq_28_plus(ctx, xy)
    });
    assert!(result.is_ok(), "79 chars re-flow: {result:?}");
    assert_eq!(
        String::from_utf8(out).expect("ASCII"),
        "[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,\n xy]"
    );
}

#[test]
fn printable_strings_and_binaries_are_never_reflowed() {
    // `io:format("~p", [lists:duplicate(100, $a)])` is a single 102-character line.
    let reg = default_registry();
    let (out, result) = run_io(&reg, b"~p", |ctx, _| {
        let elems: Vec<Eterm> = (0..100).map(|_| ctx.int(97).expect("fits")).collect();
        ctx.list(&elems).expect("fits")
    });
    assert!(result.is_ok(), "a long printable string is emitted: {result:?}");
    assert_eq!(out.len(), 102);
    assert_eq!(out[0], b'"');
    assert_eq!(out[101], b'"');

    // The same rule applies to a printable binary (`<<"aaa…">>`, 106 chars).
    let (out, result) = run_io(&reg, b"~p", |ctx, reg| {
        let bytes = vec![b'a'; 100];
        binary(ctx, reg, &bytes)
    });
    assert!(result.is_ok(), "a long printable binary is emitted: {result:?}");
    assert_eq!(out.len(), 106);
    assert_eq!(&out[..3], b"<<\"");
}

#[test]
fn tuples_lists_and_maps_break_like_the_oracle() {
    let reg = default_registry();

    // `list_to_tuple(lists:seq(1,30))` — an untagged tuple breaks after `28,`:
    //   {1,…,28,
    //    29,30}
    let (out, result) = run_io(&reg, b"~p", |ctx, _| {
        let elems: Vec<Eterm> = (1..=30).map(|i| ctx.int(i).expect("fits")).collect();
        ctx.tuple(&elems).expect("fits")
    });
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        String::from_utf8(out).expect("ASCII"),
        "{1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,\n 29,30}"
    );

    // `maps:from_list([{I,I} || I <- lists:seq(1,20)])` — a two-space indent:
    //   #{1 => 1,…,10 => 10,
    //     11 => 11,…,18 => 18,
    //     19 => 19,20 => 20}
    let (out, result) = run_io(&reg, b"~p", |ctx, reg| {
        let pairs: Vec<Eterm> = (1..=20)
            .map(|i| {
                ctx.tuple(&[ctx.int(i).expect("fits"), ctx.int(i).expect("fits")])
                    .expect("fits")
            })
            .collect();
        let list = ctx.list(&pairs).expect("fits");
        reg.call("maps", "from_list", 1, ctx, &[list])
            .expect("maps:from_list fits")
    });
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        String::from_utf8(out).expect("ASCII"),
        "#{1 => 1,2 => 2,3 => 3,4 => 4,5 => 5,6 => 6,7 => 7,8 => 8,9 => 9,10 => 10,\n  \
         11 => 11,12 => 12,13 => 13,14 => 14,15 => 15,16 => 16,17 => 17,18 => 18,\n  \
         19 => 19,20 => 20}"
    );

    // `[1,…,40|x]` — the improper tail is appended to the last line.
    let (out, result) = run_io(&reg, b"~p", |ctx, _| {
        let elems: Vec<Eterm> = (1..=40).map(|i| ctx.int(i).expect("fits")).collect();
        // Build the improper list `[1,…,40|x]` by consing onto the tail atom.
        let mut tail = ctx.atom("x").expect("seeded");
        for value in elems.iter().rev() {
            tail = ctx.cons(*value, tail).expect("fits");
        }
        tail
    });
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        String::from_utf8(out).expect("ASCII"),
        "[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,\n \
         29,30,31,32,33,34,35,36,37,38,39,40|x]"
    );
}

#[test]
fn a_tagged_tuple_indents_by_the_tag_width() {
    // `list_to_tuple([ok|lists:seq(100,130)])` — the tag `ok` is 2 wide, so the
    // wrapped elements are indented 4 columns (the oracle's `TagInd = Tlen + 2`):
    //   {ok,100,…,117,
    //       118,…,130}
    let reg = default_registry();
    let (out, result) = run_io(&reg, b"~p", |ctx, _| {
        let ok = ctx.atom("ok").expect("seeded");
        let mut elems = vec![ok];
        elems.extend((100..=130).map(|i| ctx.int(i).expect("fits")));
        ctx.tuple(&elems).expect("fits")
    });
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        String::from_utf8(out).expect("ASCII"),
        "{ok,100,101,102,103,104,105,106,107,108,109,110,111,112,113,114,115,116,117,\n    \
         118,119,120,121,122,123,124,125,126,127,128,129,130}"
    );
}

#[test]
fn nonprintable_binary_reflow_matches_the_oracle() {
    // `io_lib:format("~p", [list_to_binary(lists:duplicate(60, 0))])` (erts-17.1):
    //     [[60,60,[[48],44,…,[48],44,10,[[32,32]],[48],…,[48]],62,62]]
    // i.e. 38 byte elements on the first line, then `,` `\n` + the two-space
    // continuation indent + the remaining 22.
    let reg = default_registry();
    let (out, result) = run_io(&reg, b"~p", |ctx, reg| binary(ctx, reg, &[0u8; 60]));
    assert!(result.is_ok(), "{result:?}");
    let want = format!("<<{},\n  {}>>", ["0"; 38].join(","), ["0"; 22].join(","));
    assert_eq!(String::from_utf8(out).expect("ASCII"), want);

    // `io_lib:format("~p", [list_to_binary(lists:seq(1, 60))])`:
    //     <<1,…,28,\n  29,…,53,\n  54,…,60>>
    // (multi-digit elements are wider, so the lines hold 28/25/7 elements).
    let reg = default_registry();
    let (out, result) = run_io(&reg, b"~p", |ctx, reg| {
        let bytes: Vec<u8> = (1..=60).collect();
        binary(ctx, reg, &bytes)
    });
    assert!(result.is_ok(), "{result:?}");
    let seg = |a: i32, b: i32| (a..=b).map(|n| n.to_string()).collect::<Vec<_>>().join(",");
    let want = format!("<<{},\n  {},\n  {}>>", seg(1, 28), seg(29, 53), seg(54, 60));
    assert_eq!(String::from_utf8(out).expect("ASCII"), want);
}

#[test]
fn latin1_printable_binaries_are_the_oracles_string_leaf() {
    // A `16#A0` byte IS a printable latin1 character (`16#A0..=16#FF`), so the
    // oracle takes `io_lib:printable_bin0/5`'s `{true, …}` arm and prints the
    // quoted-string leaf on ONE line however long — never re-flowed. The bytes are
    // UTF-8, so each `16#A0` is `c2 a0`:
    //     io:format("~p", [list_to_binary(lists:duplicate(100, 16#A0))])
    //     => <<"…………………">>   (204 bytes)
    let reg = default_registry();
    let (out, result) = run_io(&reg, b"~p", |ctx, reg| binary(ctx, reg, &[0xA0u8; 100]));
    assert!(result.is_ok(), "{result:?}");
    let want = format!("<<\"{}\">>", "\u{a0}".repeat(100));
    assert_eq!(String::from_utf8(out).expect("UTF-8"), want);
}

/// Nested containers re-flow like the oracle (`io_lib_pretty:pp` recurses through
/// `pp_element`/`pp_list`), and a **depth-limited `~P`** is rendered rather than
/// refused. Both were a typed `badarg` before the full `io_lib_pretty` port; every
/// golden below is the OTP 29.1.1 oracle's `io:format` text.
#[test]
fn nested_containers_reflow_like_the_oracle() {
    let reg = default_registry();

    // `[[1,2,3],[4,5,6]]` is 25 characters — one line, no break.
    let (out, result) = run_io(&reg, b"~p", |ctx, _| {
        let a = int_list(ctx, 1..=3);
        let b = int_list(ctx, 4..=6);
        ctx.list(&[a, b]).expect("fits")
    });
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(String::from_utf8(out).expect("ASCII"), "[[1,2,3],[4,5,6]]");

    // `{a,{1,2,3}}` — a nested tagged tuple.
    let (out, result) = run_io(&reg, b"~p", |ctx, _| {
        let a = ctx.atom("a").expect("seeded");
        let one = ctx.int(1).expect("fits");
        let two = ctx.int(2).expect("fits");
        let three = ctx.int(3).expect("fits");
        let inner = ctx.tuple(&[one, two, three]).expect("fits");
        ctx.tuple(&[a, inner]).expect("fits")
    });
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(String::from_utf8(out).expect("ASCII"), "{a,{1,2,3}}");

    // `[lists:seq(1,28), 1]` is 79 characters: the inner list is written flat
    // (`pp_element` fits it), then the element that follows starts a new line.
    let (out, result) = run_io(&reg, b"~p", |ctx, _| {
        let inner = int_list(ctx, 1..=28);
        let one = ctx.int(1).expect("fits");
        ctx.list(&[inner, one]).expect("fits")
    });
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        String::from_utf8(out).expect("ASCII"),
        "[[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28],\n 1]"
    );

    // `[[[1,2],[3,4]]]` at depth 5 (`~P`) — the inner tuple/list re-flows too.
    let (out, result) = run_io_args(&reg, b"~P", |ctx, _| {
        let a = int_list(ctx, 1..=2);
        let b = int_list(ctx, 3..=4);
        let inner = ctx.list(&[a, b]).expect("fits");
        let term = ctx.list(&[inner]).expect("fits");
        vec![term, ctx.int(5).expect("fits")]
    });
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(String::from_utf8(out).expect("ASCII"), "[[[1,2],[3|...]]]");
}

/// The depth-limited `~P` goldens, all live-probed on the oracle.
#[test]
fn depth_limited_renderings_reflow_like_the_oracle() {
    let reg = default_registry();

    // `~P [[1,2,3],[4,5,6]] D`.
    let nested6 = |ctx: &mut BifContext<'_>| {
        let a = int_list(ctx, 1..=3);
        let b = int_list(ctx, 4..=6);
        ctx.list(&[a, b]).expect("fits")
    };
    for (depth, want) in [
        (3i64, "[[1|...],[...]]"),
        (2, "[[...]|...]"),
        (1, "[...]"),
        (0, "..."),
    ] {
        let (out, result) = run_io_args(&reg, b"~P", |ctx, _| {
            let term = nested6(ctx);
            vec![term, ctx.int(depth).expect("fits")]
        });
        assert!(result.is_ok(), "~P depth {depth}: {result:?}");
        assert_eq!(
            String::from_utf8(out).expect("ASCII"),
            want,
            "~P depth {depth}"
        );
    }

    // `~P {a,[1,2,3]} D` — the tag indents the continuation.
    let tag_list = |ctx: &mut BifContext<'_>| {
        let a = ctx.atom("a").expect("seeded");
        let l = int_list(ctx, 1..=3);
        ctx.tuple(&[a, l]).expect("fits")
    };
    for (depth, want) in [(3i64, "{a,[...]}"), (2, "{a,...}"), (4, "{a,[1|...]}")] {
        let (out, result) = run_io_args(&reg, b"~P", |ctx, _| {
            let term = tag_list(ctx);
            vec![term, ctx.int(depth).expect("fits")]
        });
        assert!(result.is_ok(), "~P {{a,[1,2,3]}} {depth}: {result:?}");
        assert_eq!(
            String::from_utf8(out).expect("ASCII"),
            want,
            "~P {{a,[1,2,3]}} {depth}"
        );
    }

    // `~P {[1,2],3} 2` — an untagged tuple whose first element is itself elided,
    // so the tuple takes `pp_list`'s `,` separator (not `pp_tag_tuple`).
    let (out, result) = run_io_args(&reg, b"~P", |ctx, _| {
        let l = int_list(ctx, 1..=2);
        let three = ctx.int(3).expect("fits");
        let term = ctx.tuple(&[l, three]).expect("fits");
        vec![term, ctx.int(2).expect("fits")]
    });
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(String::from_utf8(out).expect("ASCII"), "{[...],...}");

    // `~P lists:seq(1,10) 2` and `~P [[1,2]|3] 3` (an improper nested list).
    let (out, result) = run_io_args(&reg, b"~P", |ctx, _| {
        let term = int_list(ctx, 1..=10);
        vec![term, ctx.int(2).expect("fits")]
    });
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(String::from_utf8(out).expect("ASCII"), "[1|...]");

    let (out, result) = run_io_args(&reg, b"~P", |ctx, _| {
        let inner = int_list(ctx, 1..=2);
        let three = ctx.int(3).expect("fits");
        let term = ctx.cons(inner, three).expect("fits");
        vec![term, ctx.int(3).expect("fits")]
    });
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(String::from_utf8(out).expect("ASCII"), "[[1|...]|3]");

    // `~P <<1,2,3>> D`, `~P <<1,2,3,4,5>> 3`, `~W <<1,2,3,4,5>> 2`.
    let bin3 = |ctx: &mut BifContext<'_>, reg: &BifRegistry| binary(ctx, reg, &[1, 2, 3]);
    for (depth, want) in [(3i64, "<<1,2,...>>"), (4, "<<1,2,3>>")] {
        let (out, result) = run_io_args(&reg, b"~P", |ctx, reg| {
            let term = bin3(ctx, reg);
            vec![term, ctx.int(depth).expect("fits")]
        });
        assert!(result.is_ok(), "~P <<1,2,3>> {depth}: {result:?}");
        assert_eq!(
            String::from_utf8(out).expect("ASCII"),
            want,
            "~P <<1,2,3>> {depth}"
        );
    }
    let bin5 = |ctx: &mut BifContext<'_>, reg: &BifRegistry| binary(ctx, reg, &[1, 2, 3, 4, 5]);
    let (out, result) = run_io_args(&reg, b"~P", |ctx, reg| {
        let term = bin5(ctx, reg);
        vec![term, ctx.int(3).expect("fits")]
    });
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(String::from_utf8(out).expect("ASCII"), "<<1,2,...>>");

    // NOTE: `~W` (the depth-limited `~w`) does **not** go through
    // `crate::print::pretty_chunk` — `io.rs` renders it with the flat
    // `term_chunk` — so its depth-limited binary truncation is pinned by
    // `tests/io_lib.rs`, not here.
    // `~P <<"abcdef">> D` — the printable binary leaf truncates by element count.
    let binstr = |ctx: &mut BifContext<'_>, reg: &BifRegistry| binary(ctx, reg, b"abcdef");
    for (depth, want) in [
        (2i64, "<<\"abcd\"...>>"),
        (3, "<<\"abcdef\">>"),
        (1, "<<...>>"),
    ] {
        let (out, result) = run_io_args(&reg, b"~P", |ctx, reg| {
            let term = binstr(ctx, reg);
            vec![term, ctx.int(depth).expect("fits")]
        });
        assert!(result.is_ok(), "~P <<\"abcdef\">> {depth}: {result:?}");
        assert_eq!(
            String::from_utf8(out).expect("ASCII"),
            want,
            "~P <<\"abcdef\">> {depth}"
        );
    }

    // `~P {1,2,3,4} D` and `~P [1,2,3,4,5,6] D`.
    for (depth, want) in [(2i64, "{1,...}"), (3, "{1,2,...}")] {
        let (out, result) = run_io_args(&reg, b"~P", |ctx, _| {
            let elems: Vec<Eterm> = (1..=4).map(|i| ctx.int(i).expect("fits")).collect();
            let term = ctx.tuple(&elems).expect("fits");
            vec![term, ctx.int(depth).expect("fits")]
        });
        assert!(result.is_ok(), "~P {{1,2,3,4}} {depth}: {result:?}");
        assert_eq!(
            String::from_utf8(out).expect("ASCII"),
            want,
            "~P {{1,2,3,4}} {depth}"
        );
    }
    for (depth, want) in [(3i64, "[1,2|...]"), (5, "[1,2,3,4|...]")] {
        let (out, result) = run_io_args(&reg, b"~P", |ctx, _| {
            let term = int_list(ctx, 1..=6);
            vec![term, ctx.int(depth).expect("fits")]
        });
        assert!(result.is_ok(), "~P [1..6] {depth}: {result:?}");
        assert_eq!(
            String::from_utf8(out).expect("ASCII"),
            want,
            "~P [1..6] {depth}"
        );
    }

    // `~P #{1 => 1,2 => 2,3 => 3,4 => 4} D` (integer keys — an order both the
    // oracle and this crate agree on; OTP leaves map order unspecified).
    let map4 = |ctx: &mut BifContext<'_>, reg: &BifRegistry| {
        let pairs: Vec<Eterm> = (1..=4)
            .map(|i| {
                ctx.tuple(&[ctx.int(i).expect("fits"), ctx.int(i).expect("fits")])
                    .expect("fits")
            })
            .collect();
        let list = ctx.list(&pairs).expect("fits");
        reg.call("maps", "from_list", 1, ctx, &[list])
            .expect("maps:from_list fits")
    };
    for (depth, want) in [
        (2i64, "#{1 => 1,...}"),
        (4, "#{1 => 1,2 => 2,3 => 3,...}"),
    ] {
        let (out, result) = run_io_args(&reg, b"~P", |ctx, reg| {
            let term = map4(ctx, reg);
            vec![term, ctx.int(depth).expect("fits")]
        });
        assert!(result.is_ok(), "~P map4 {depth}: {result:?}");
        assert_eq!(
            String::from_utf8(out).expect("ASCII"),
            want,
            "~P map4 {depth}"
        );
    }

    // `~P #{1 => [1,2,3], 2 => {4,5}} D` — nested map values.
    let nested_map = |ctx: &mut BifContext<'_>, reg: &BifRegistry| {
        let l = int_list(ctx, 1..=3);
        let four = ctx.int(4).expect("fits");
        let five = ctx.int(5).expect("fits");
        let t = ctx.tuple(&[four, five]).expect("fits");
        let one = ctx.int(1).expect("fits");
        let two = ctx.int(2).expect("fits");
        let p1 = ctx.tuple(&[one, l]).expect("fits");
        let p2 = ctx.tuple(&[two, t]).expect("fits");
        let pairs = ctx.list(&[p1, p2]).expect("fits");
        reg.call("maps", "from_list", 1, ctx, &[pairs])
            .expect("maps:from_list fits")
    };
    for (depth, want) in [
        (3i64, "#{1 => [1|...],2 => {4,...}}"),
        (2, "#{1 => [...],...}"),
    ] {
        let (out, result) = run_io_args(&reg, b"~P", |ctx, reg| {
            let term = nested_map(ctx, reg);
            vec![term, ctx.int(depth).expect("fits")]
        });
        assert!(result.is_ok(), "~P nested map {depth}: {result:?}");
        assert_eq!(
            String::from_utf8(out).expect("ASCII"),
            want,
            "~P nested map {depth}"
        );
    }

    // `~p list_to_tuple([ok|lists:seq(1,30)])` — a tagged tuple whose tag `ok` sets
    // `TagInd = Tlen + 2 = 4`, so the wrapped elements are indented 4 columns
    // (`pp_tag_tuple/10`) and the 28th element starts the second line
    // (`erl -noshell -eval 'io:format("~p~n",[lists:flatten(io_lib:format("~p",[list_to_tuple([ok|lists:seq(1,30)])]))]).'`):
    //   {ok,1,…,27,
    //      28,29,30}
    let (out, result) = run_io(&reg, b"~p", |ctx, _| {
        let ok = ctx.atom("ok").expect("seeded");
        let mut elems = vec![ok];
        elems.extend((1..=30).map(|i| ctx.int(i).expect("fits")));
        ctx.tuple(&elems).expect("fits")
    });
    assert!(result.is_ok(), "{result:?}");
    let seq: Vec<String> = (1..=30).map(|i| i.to_string()).collect();
    assert_eq!(
        String::from_utf8(out).expect("ASCII"),
        format!("{{ok,{},\n    {}}}", seq[..27].join(","), seq[27..].join(","))
    );
}

/// The re-flow's **exact nesting** (not only its flattened text), dumped with `~w`
/// — the oracle's `io_lib:format` result as a char-list tree. A differing nesting is
/// a failure even when the text agrees (the spliced `...`/`#{`/` => ` sub-lists are
/// what makes it agree).
#[test]
fn nested_reflow_nesting_matches_the_oracles_deep_list() {
    let reg = default_registry();

    let (nested, dump) = (
        |ctx: &mut BifContext<'_>| {
            let a = int_list(ctx, 1..=3);
            let b = int_list(ctx, 4..=6);
            ctx.list(&[a, b]).expect("fits")
        },
        "[[91,[[91,[[49],44,[50],44,[51]],93],44,[91,[[52],44,[53],44,[54]],93]],93]]",
    );
    let got = deep_text(&reg, b"~p", |ctx, _| vec![nested(ctx)]).expect("fits");
    assert_eq!(got, dump);

    let got = deep_text(&reg, b"~P", |ctx, _| {
        let t = nested(ctx);
        vec![t, ctx.int(3).expect("fits")]
    })
    .expect("fits");
    assert_eq!(
        got,
        "[[91,[[91,[[49],124,46,46,46],93],44,[91,[46,46,46],93]],93]]"
    );

    // `~P {a,[1,2,3]} 2` — the elided list is a nested `[46,46,46]` *inside* the
    // tuple's element list, and the `,...` of an elided map pair is spliced.
    let got = deep_text(&reg, b"~P", |ctx, _| {
        let a = ctx.atom("a").expect("seeded");
        let l = int_list(ctx, 1..=3);
        let t = ctx.tuple(&[a, l]).expect("fits");
        vec![t, ctx.int(2).expect("fits")]
    })
    .expect("fits");
    assert_eq!(got, "[[123,[[97],44,46,46,46],125]]");

    let got = deep_text(&reg, b"~P", |ctx, reg| {
        let pairs: Vec<Eterm> = (1..=4)
            .map(|i| {
                ctx.tuple(&[ctx.int(i).expect("fits"), ctx.int(i).expect("fits")])
                    .expect("fits")
            })
            .collect();
        let list = ctx.list(&pairs).expect("fits");
        let m = reg
            .call("maps", "from_list", 1, ctx, &[list])
            .expect("maps:from_list fits");
        vec![m, ctx.int(4).expect("fits")]
    })
    .expect("fits");
    assert_eq!(
        got,
        "[[35,123,[[[49],[32,61,62,32],[49]],44,[[50],[32,61,62,32],[50]],44,\
         [[51],[32,61,62,32],[51]],44,46,46,46],125]]"
    );
}

/// The one remaining re-flow gap: a left-adjusted `~p`/`~P` has **no clause** in
/// `io_lib_format:print/12`, so the oracle raises `error:badarg` — as does this
/// crate. A depth-limited `~P` with a *missing* depth argument is `badarg` in the
/// oracle too (`io_lib_format` takes the depth from the argument list).
#[test]
fn left_adjusted_and_argument_short_p_renderings_stay_a_typed_badarg() {
    let reg = default_registry();

    let (_out, result) = run_io(&reg, b"~-p", |ctx, _| int_list(ctx, 1..=4));
    assert!(matches!(result, Err(BeamError::BadArg)), "{result:?}");
    let (_out, result) = run_io_args(&reg, b"~-10P", |ctx, _| {
        let t = int_list(ctx, 1..=4);
        vec![t, ctx.int(2).expect("fits")]
    });
    assert!(matches!(result, Err(BeamError::BadArg)), "{result:?}");

    // `io_lib:format("~100P", [lists:seq(1,40)])` ⇒ `{error,badarg}` on the oracle.
    let (_out, result) = run_io(&reg, b"~100P", |ctx, _| int_list(ctx, 1..=40));
    assert!(matches!(result, Err(BeamError::BadArg)), "{result:?}");
    // `io_lib:format("~W", [[1,2,3]])` ⇒ `{error,badarg}`.
    let (_out, result) = run_io(&reg, b"~W", |ctx, _| int_list(ctx, 1..=3));
    assert!(matches!(result, Err(BeamError::BadArg)), "{result:?}");
}

#[test]
fn the_latin1_printable_rule_and_its_escapes_match_the_oracle() {
    // `io:format("~p", [L])` on the OTP 29.1.1 oracle (printable range = latin1):
    // a byte is printable iff `16#20..=16#7E`, `16#A0..=16#FF`, or one of the
    // control characters `\t \n \r \v \b \f \e` (`16#09 16#0A 16#0D 16#0B 16#08
    // 16#0C 16#1B`). `16#00` and `16#7F` are NOT printable.
    let reg = default_registry();
    let cases: &[(&[u8], &str)] = &[
        // `~p [10,97]` => "\na" (the newline is a string character, escaped).
        (&[10, 97], "\"\\na\""),
        // `~p [0,160,255]` => [0,160,255] (0 is not printable, so not a string).
        (&[0, 160, 255], "[0,160,255]"),
        // `~p [159,160]` => [159,160] (159 is not printable).
        (&[159, 160], "[159,160]"),
        // `~p [200,201]` => "ÈÉ" (200/201 are printable latin1).
        (&[200, 201], "\"\u{c8}\u{c9}\""),
        // `~p [160,160]` => two UTF-8 `c2 a0` between quotes.
        (&[160, 160], "\"\u{a0}\u{a0}\""),
    ];
    for (bytes, want) in cases {
        let (out, result) = run_io(&reg, b"~p", |ctx, _| byte_list(ctx, bytes));
        assert!(result.is_ok(), "~p {bytes:?}: {result:?}");
        assert_eq!(
            String::from_utf8(out.clone()).expect("UTF-8"),
            *want,
            "~p {bytes:?}"
        );
    }

    // `~p [9,11,8,12,13,27,92,34]` => "\t\v\b\f\r\e\\\""
    // (the seven latin1 control escapes, then the escaped `\` and `"`).
    let (out, result) = run_io(&reg, b"~p", |ctx, _| {
        byte_list(ctx, &[9, 11, 8, 12, 13, 27, 92, 34])
    });
    assert!(result.is_ok(), "{result:?}");
    let want: String = [
        '"', '\\', 't', '\\', 'v', '\\', 'b', '\\', 'f', '\\', 'r', '\\', 'e', '\\', '\\', '\\',
        '"', '"',
    ]
    .iter()
    .collect();
    assert_eq!(String::from_utf8(out).expect("UTF-8"), want);
}

#[test]
fn io_lib_format_reflows_with_the_oracles_exact_nesting() {
    // `io_lib:format("~p", [lists:seq(1,29)])` (the oracle, erts-17.1):
    //   [[91,[[49],44,[50],44,…,[50,56],44,10,[32],[50,57]],93]]
    // i.e. `'['`, the element list (each written element its own sub-list, the
    // separators bare at 44, the break `44,10` then the one-space indent `[32]`),
    // and `']'`.
    enum E {
        I(i64),
        L(Vec<E>),
    }
    fn build(ctx: &mut BifContext<'_>, e: &E) -> Eterm {
        match e {
            E::I(v) => ctx.int(*v).expect("fits"),
            E::L(items) => {
                let mut elems = Vec::with_capacity(items.len());
                for item in items {
                    elems.push(build(ctx, item));
                }
                ctx.list(&elems).expect("fits")
            }
        }
    }
    fn digits(n: i64) -> E {
        E::L(n.to_string().bytes().map(|b| E::I(i64::from(b))).collect())
    }

    let mut body: Vec<E> = Vec::new();
    for i in 1..=29 {
        if i > 1 {
            body.push(E::I(44));
            if i == 29 {
                body.push(E::I(10));
                body.push(E::L(vec![E::I(32)]));
            }
        }
        body.push(digits(i));
    }
    let want = E::L(vec![E::L(vec![E::I(91), E::L(body), E::I(93)])]);

    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let got = run_io_lib_p(&reg, &mut heap, &atoms, |c| int_list(c, 1..=29))
        .expect("io_lib:format fits");
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    let expected = build(&mut ctx, &want);
    assert!(beam_term::compare::eq(got, expected, ctx.atoms()));

    // `iolist_to_binary/1` of the deep list is the same text `io:format` writes —
    // i.e. the deep list flattens to `"[1,…,28,\n 29]"`.
    let flattened = reg
        .call("erlang", "iolist_to_binary", 1, &mut ctx, &[got])
        .expect("the deep list is a valid iolist");
    let want_text = binary(
        &mut ctx,
        &reg,
        b"[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,\n 29]",
    );
    assert!(beam_term::compare::eq(flattened, want_text, ctx.atoms()));
}