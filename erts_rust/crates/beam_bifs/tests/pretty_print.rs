//! Integration tests for the documented `~p`/`~P` **re-flow gap**.
//!
//! The oracle prints a container under `~p`/`~P` with `io_lib_pretty`, which breaks
//! it across lines when its single-line rendering is **at least**
//! `line_length - column` characters long (80 and 1 by default, i.e. 79). This
//! crate implements only the single-line form, so for the re-flowable classes (a
//! tuple, a map, a non-printable list, a non-printable bitstring) whose rendering
//! reaches that length it raises `error:badarg` instead of emitting a line the
//! oracle would have broken (see the `crate::io` module docs). Terms the oracle
//! never re-flows — atoms, numbers, refs, pids, funs, printable strings and
//! printable binaries — are emitted on one line however long.
//!
//! Every expectation is pinned against the OTP oracle (erts-17.1); the oracle
//! output it *differs from* (a multi-line break) is quoted in the comments. This
//! file asserts the current **honest** behaviour: a typed `badarg`, never a wrong
//! (silently mis-formatted) string.

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

/// A binary built through the registered `erlang:list_to_binary/1` (the public API
/// exposes no binary constructor).
fn binary(ctx: &mut BifContext<'_>, reg: &BifRegistry, bytes: &[u8]) -> Eterm {
    let list = byte_list(ctx, bytes);
    reg.call("erlang", "list_to_binary", 1, ctx, &[list])
        .expect("list_to_binary fits")
}

/// Runs `io:format(Fmt, [Arg])` through a `BytesSink` and returns
/// `(captured bytes, result)`.
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

/// A proper list of the small integers `range`.
fn int_list(ctx: &mut BifContext<'_>, range: std::ops::RangeInclusive<i64>) -> Eterm {
    let elems: Vec<Eterm> = range.map(|i| ctx.int(i).expect("fits")).collect();
    ctx.list(&elems).expect("a short list fits")
}

#[test]
fn narrowest_reflow_repro_is_a_typed_badarg() {
    // `lists:seq(1,29)` renders as exactly 79 characters, so the oracle breaks it:
    //   [1,2,3,4,5,6,7,8,9,10,...,28,
    //    29]
    // This engine cannot break the line and raises `error:badarg` instead of
    // emitting a single 79-character line. (`Len >= Ll - Col` = `79 >= 79`.)
    let reg = default_registry();
    let (out, result) = run_io(&reg, b"~p", |ctx, _| int_list(ctx, 1..=29));
    assert!(
        matches!(result, Err(BeamError::BadArg)),
        "the re-flow gap is a typed badarg, got {result:?}"
    );
    assert!(out.is_empty(), "nothing is written on failure");
}

#[test]
fn the_boundary_is_the_oracles_flat_condition() {
    // The oracle's flat condition is `Len < 80 - 1 = 79`, i.e. `Len <= 78`.
    // A 78-character rendering stays on one line in both implementations.
    //   `[1,2,...,28,x]` => `[...]` with `x` last, 78 chars.
    let reg = default_registry();
    let (out, result) = run_io(&reg, b"~p~n", |ctx, _| {
        let mut elems: Vec<Eterm> = (1..=28).map(|i| ctx.int(i).expect("fits")).collect();
        elems.push(ctx.atom("x").expect("seeded"));
        ctx.list(&elems).expect("fits")
    });
    assert!(result.is_ok(), "a 78-char list is flat: {result:?}");
    assert_eq!(
        out,
        b"[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,x]\n"
    );

    // A 79-character rendering trips the guard (`79 >= 79`); the oracle breaks it
    //   [1,...,28,
    //    xy]
    let (_out, result) = run_io(&reg, b"~p", |ctx, _| {
        let mut elems: Vec<Eterm> = (1..=28).map(|i| ctx.int(i).expect("fits")).collect();
        elems.push(ctx.atom("xy").expect("seeded"));
        ctx.list(&elems).expect("fits")
    });
    assert!(matches!(result, Err(BeamError::BadArg)), "79 chars re-flows");
}

#[test]
fn printable_strings_and_binaries_are_never_reflowed() {
    // The oracle prints a printable list/binary on one line however long:
    //   `io:format("~p~n", [lists:duplicate(100, $a)])` =>
    //   a 102-character `"aaa…"` with no break.
    let reg = default_registry();
    let (out, result) = run_io(&reg, b"~p~n", |ctx, _| {
        let elems: Vec<Eterm> = (0..100).map(|_| ctx.int(97).expect("fits")).collect();
        ctx.list(&elems).expect("fits")
    });
    assert!(result.is_ok(), "a long printable string is emitted: {result:?}");
    assert_eq!(out.len(), 102 + 1, "quote + 100 chars + quote + newline");
    assert_eq!(out[0], b'"');
    assert_eq!(out[101], b'"');

    // The same rule applies to a printable binary (`<<"aaa…">>`).
    let (out, result) = run_io(&reg, b"~p~n", |ctx, reg| {
        let bytes = vec![b'a'; 100];
        binary(ctx, reg, &bytes)
    });
    assert!(result.is_ok(), "a long printable binary is emitted: {result:?}");
    // `<<"` + 100 + `">>` (= 106) + newline.
    assert_eq!(out.len(), 106 + 1);
    assert_eq!(&out[..3], b"<<\"");
}

#[test]
fn long_containers_of_every_reflowable_class_raise_badarg() {
    let reg = default_registry();

    // A non-printable list (`lists:seq(1,40)`), the objective's example.
    let (_out, result) = run_io(&reg, b"~p", |ctx, _| int_list(ctx, 1..=40));
    assert!(matches!(result, Err(BeamError::BadArg)));

    // A tuple (`list_to_tuple(lists:seq(1,30))`); the oracle breaks after `28,`
    // with a one-space indent.
    let (_out, result) = run_io(&reg, b"~p", |ctx, _| {
        let elems: Vec<Eterm> = (1..=30).map(|i| ctx.int(i).expect("fits")).collect();
        ctx.tuple(&elems).expect("fits")
    });
    assert!(matches!(result, Err(BeamError::BadArg)));

    // A map of 20 pairs (`maps:from_list([{I,I} || I <- lists:seq(1,20)])`); the
    // oracle breaks after `10 => 10,` with a two-space indent.
    let (_out, result) = run_io(&reg, b"~p", |ctx, reg| {
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
    assert!(matches!(result, Err(BeamError::BadArg)));

    // A non-printable bitstring (`binary:copy(<<0>>, 100)`); the oracle breaks
    // after every ~37 bytes with a two-space indent.
    let (_out, result) = run_io(&reg, b"~p", |ctx, reg| {
        let bytes = vec![0u8; 100];
        binary(ctx, reg, &bytes)
    });
    assert!(matches!(result, Err(BeamError::BadArg)));
}

#[test]
fn atoms_numbers_and_refs_are_never_reflowed() {
    // An atom is a single token the oracle never breaks however long its name —
    // `is_tuple`/`is_list`/`is_map`/`is_bitstring` are all false for it, so the
    // guard never fires. Here we can only seed short atoms (the public API has no
    // interner hook installed), so this pins the classification, not a long name.
    let reg = default_registry();
    let (out, result) = run_io(&reg, b"~p~n", |ctx, _| ctx.atom("a").expect("seeded"));
    assert!(result.is_ok(), "an atom is always emitted: {result:?}");
    assert_eq!(out, b"a\n");

    // A number is likewise a single token (`~p` of a large integer).
    let (out, result) = run_io(&reg, b"~p~n", |ctx, _| {
        ctx.int(123_456_789_012_345).expect("fits")
    });
    assert!(result.is_ok());
    assert_eq!(out, b"123456789012345\n");
}