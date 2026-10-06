//! Integration tests for the `~p`/`~P` **re-flow** (`io_lib_pretty` line break).
//!
//! The oracle prints a container under `~p`/`~P` with `io_lib_pretty`, which breaks
//! it across lines when its single-line rendering is **at least**
//! `line_length - column` characters long (80 and 1 by default, i.e. 79). This
//! crate now reproduces that break for a container all of whose elements are
//! themselves single-line ("atomic") — proper/improper lists, tagged and untagged
//! tuples, and maps of atomic pairs — and keeps the typed `error:badarg` for the
//! forms it does not model (a container holding another container, a non-printable
//! bitstring, a depth-limited `~P`). See the `crate::io`/`crate::print` module docs.
//!
//! Every expectation is the literal output of
//!
//! ```text
//! erl -noshell -eval 'io:format("~p", [Term]), halt().'
//! ```
//!
//! on the OTP 29 / erts-17.1 oracle. `io:format` is asserted on the exact capture
//! bytes; `io_lib:format` on the deep list's flattening AND — for the narrowest
//! repro — its exact nesting.

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
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut sink = BytesSink::new();
    let result;
    {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
        let format = byte_list(&mut ctx, fmt);
        let arg = build(&mut ctx, reg);
        let args = ctx.list(&[arg]).expect("a short arg list fits");
        result = reg
            .call("io", "format", 2, &mut ctx, &[format, args])
            .map(|_| ());
    }
    (sink.contents().to_vec(), result)
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
fn unsupported_reflow_forms_stay_a_typed_badarg() {
    // A non-printable bitstring uses `io_lib_pretty:pp_binary`'s own packing, which
    // this model does not reproduce.
    let reg = default_registry();
    let (_out, result) = run_io(&reg, b"~p", |ctx, reg| {
        let bytes = vec![0u8; 100];
        binary(ctx, reg, &bytes)
    });
    assert!(matches!(result, Err(BeamError::BadArg)), "{result:?}");

    // A container holding another container is not modelled.
    let (_out, result) = run_io(&reg, b"~p", |ctx, _| {
        let inner = int_list(ctx, 1..=28);
        let outer = ctx.list(&[inner, ctx.int(1).expect("fits")]).expect("fits");
        outer
    });
    assert!(matches!(result, Err(BeamError::BadArg)), "{result:?}");

    // A depth-limited `~P` keeps the typed error.
    let (_out, result) = run_io(&reg, b"~100P", |ctx, _| int_list(ctx, 1..=40));
    assert!(matches!(result, Err(BeamError::BadArg)), "{result:?}");
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