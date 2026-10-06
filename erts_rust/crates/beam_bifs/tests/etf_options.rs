//! Integration tests for the `erlang:term_to_binary/2` option surface
//! (`compressed` / `{compressed, Level}` / `{minor_version, N}`) and
//! `erlang:binary_to_term/2`'s `used` / `safe` options, driven through
//! [`BifRegistry::call`](beam_bifs::BifRegistry::call) — the crate's public seam.
//!
//! Every expectation is pinned to the OTP 29 / erts-17.1 oracle (`erl` on
//! `PATH`); the oracle command that produced each one is cited in a comment next
//! to the assertion. Terms are built and read with the sibling **safe**
//! `beam_codec::term` helpers (`make_binary`/`make_flatmap`/`bitstring_bytes`/…),
//! because the crate's own bitstring helpers are private.
//!
//! Coverage maps 1:1 onto the behaviour `src/binconv.rs` now implements:
//!
//! * the C revert rule (`external.c:2250-2301`) — a body too small to pay for
//!   zlib's 5-byte framing stays a *plain* image, a large repetitive one becomes
//!   `[131][80][len:4 BE][zlib]`;
//! * `parse_t2b_opts`'s rejections (a non-list, an unknown option, an
//!   out-of-range value);
//! * `{minor_version, N}`: `2` is the default tag set (`NEW_FLOAT_EXT` +
//!   UTF-8 atoms), `0`/`1` clear `DFLAG_UTF8_ATOMS` (and `0` also clears
//!   `DFLAG_NEW_FLOATS`) — which the codec reproduces byte-for-byte for an
//!   atom-/float-free term and *refuses* (typed `unsupported_bif`) otherwise;
//! * `binary_to_term/2`'s `used` count (which includes the `131` version byte)
//!   and `safe` (which refuses an image that would create an atom).

use std::cell::RefCell;
use std::rc::Rc;

use beam_alloc::Heap;
use beam_bifs::{default_registry, AtomInterner, BifContext};
use beam_codec::term as codec_term;
use beam_common::{BeamError, ExitReason, Result};
use beam_process::ProcessId;
use beam_term::{compare::eq, signed_value, Atom, AtomTable, Eterm, NIL};

/// `external.c`'s `VERSION_MAGIC` — the first byte of every image.
const VERSION_MAGIC: u8 = 131;
/// `external.c`'s `COMPRESSED` tag (`80`) — the second byte of a zlib image.
const COMPRESSED_EXT: u8 = 80;

/// `binary_to_term(<<131,119,8,"tjenixen">>)` — a `SMALL_ATOM_UTF8_EXT` image
/// of a name no seeded table knows, i.e. one `safe` must refuse.
const FRESH_ATOM_BLOB: &[u8] = &[131, 119, 8, 116, 106, 101, 110, 105, 120, 101, 110];

/// `term_to_binary(fun lists:reverse/1)` — the `EXPORT_EXT` (`113`) image the
/// oracle round-trips byte-exactly
/// (`erl -noshell -eval 'F = binary_to_term(<<131,113,119,5,108,105,115,116,115,
/// 119,7,114,101,118,101,114,115,101,97,1>>), io:format("~p~n",[term_to_binary(F)
/// =:= <<131,113,119,5,108,105,115,116,115,119,7,114,101,118,101,114,115,101,97,1>>]),halt().'
/// ⇒ `true`, and `F` prints as `fun lists:reverse/1`).
const EXPORT_EXT_BLOB: &[u8] = &[
    131, 113, 119, 5, 108, 105, 115, 116, 115, 119, 7, 114, 101, 118, 101, 114, 115, 101, 97, 1,
];

/// The names every test table is seeded with. `compressed`, `minor_version`,
/// `used`, `safe` and `bogus` are the option atoms the BIFs recognise (or
/// reject); `lists`/`reverse` are **deliberately absent** so the `EXPORT_EXT`
/// test actually exercises the [`AtomInterner`] seam.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true",
        "false",
        "ok",
        "a",
        "b",
        "hello",
        "compressed",
        "minor_version",
        "used",
        "safe",
        "bogus",
    ]);
    atoms
}

/// The bytes of a byte-aligned binary term.
///
/// Re-packs the `size_bits` bits starting at the term's bit offset into a fresh
/// MSB-first buffer, so the result is canonical whatever representation the term
/// uses (a freshly built binary has offset `0`, but the reader must not assume it).
fn bytes_of(term: Eterm) -> Vec<u8> {
    let (buf, offset, size_bits) =
        codec_term::bitstring_bytes(term).expect("the result is a bitstring");
    assert_eq!(size_bits % 8, 0, "the result is a byte-aligned binary");
    let mut out = vec![0u8; size_bits / 8];
    for i in 0..size_bits {
        let bit = offset + i;
        if (buf[bit / 8] >> (7 - (bit % 8))) & 1 == 1 {
            out[i / 8] |= 0x80 >> (i % 8);
        }
    }
    out
}

/// The option list `[Name]` — a bare-atom option (e.g. `compressed`).
fn option_atom(ctx: &mut BifContext<'_>, name: &str) -> Eterm {
    ctx.list(&[ctx.atom(name).expect("seeded")])
        .expect("fits")
}

/// The option list `[{Name, Value}]` — a single `{Name, Value}` option whose
/// value is the small integer `value`.
fn option_tuple(ctx: &mut BifContext<'_>, name: &str, value: i64) -> Eterm {
    let value = ctx.int(value).expect("a small int fits");
    let pair = ctx
        .tuple(&[ctx.atom(name).expect("seeded"), value])
        .expect("fits");
    ctx.list(&[pair]).expect("fits")
}

/// Asserts `result` is `badarg`.
fn assert_badarg(result: Result<Eterm>) {
    assert!(
        matches!(result, Err(BeamError::BadArg)),
        "expected badarg, got {result:?}"
    );
}

/// Asserts `result` is the `error:unsupported_bif` raise `{minor_version, 0|1}`
/// reports for a term whose Latin-1/legacy-float image the codec cannot write.
fn assert_unsupported_bif(result: Result<Eterm>) {
    match result {
        Err(BeamError::Raise(raise)) => assert_eq!(
            raise.reason,
            ExitReason::atom("unsupported_bif"),
            "the capability-gap reason atom"
        ),
        other => panic!("expected unsupported_bif, got {other:?}"),
    }
}

#[test]
fn term_to_binary_small_compressed_reverts_to_plain() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let list = ctx
        .list(&[
            ctx.int(1).expect("fits"),
            ctx.int(2).expect("fits"),
            ctx.int(3).expect("fits"),
        ])
        .expect("fits");

    // erl -noshell -eval 'io:format("~p~n",[term_to_binary([1,2,3],[compressed])]),halt().'
    //   % <<131,107,0,3,1,2,3>>
    // The body cannot pay for zlib's 5-byte framing, so the image stays plain.
    let expected = vec![VERSION_MAGIC, 107, 0, 3, 1, 2, 3];
    let compressed = option_atom(&mut ctx, "compressed");
    assert_eq!(
        bytes_of(
            reg.call("erlang", "term_to_binary", 2, &mut ctx, &[list, compressed])
                .expect("term_to_binary([1,2,3],[compressed])")
        ),
        expected
    );

    // Every explicit level reverts too —
    // erl -noshell -eval 'io:format("~p~n",[[term_to_binary([1,2,3],[{compressed,N}])
    //   =:= term_to_binary([1,2,3]) || N <- lists:seq(0,9)]]),halt().'
    //   % [true,true,true,true,true,true,true,true,true,true]
    for level in 0..=9i64 {
        let options = option_tuple(&mut ctx, "compressed", level);
        assert_eq!(
            bytes_of(
                reg.call("erlang", "term_to_binary", 2, &mut ctx, &[list, options])
                    .expect("a compressed image")
            ),
            expected,
            "{{compressed, {level}}} must revert for the small body"
        );
    }
}

#[test]
fn term_to_binary_large_compressed_emits_compressed_ext() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // A large, highly repetitive term — the oracle's `lists:duplicate(500, hello)`.
    let hello = ctx.atom("hello").expect("seeded");
    let mut big = NIL;
    for _ in 0..500 {
        big = ctx.cons(hello, big).expect("fits");
    }

    // erl -noshell -eval 'io:format("~p~n",[byte_size(term_to_binary(
    //   lists:duplicate(500,hello)))]),halt().'  % 3507
    let plain = bytes_of(
        reg.call("erlang", "term_to_binary", 1, &mut ctx, &[big])
            .expect("the plain image"),
    );
    assert_eq!(plain.len(), 3507, "the oracle's plain image for 500 `hello`s");

    // erl ... 'C = term_to_binary(Big,[compressed]),
    //          io:format("~p~n",[{binary:at(C,0),binary:at(C,1),binary:part(C,2,4)}]),halt().'
    //   % {131,80,<<0,0,13,178>>}   (0x0DB2 = 3506 = byte_size(Plain) - 1)
    let compressed = option_atom(&mut ctx, "compressed");
    let image = bytes_of(
        reg.call("erlang", "term_to_binary", 2, &mut ctx, &[big, compressed])
            .expect("the compressed image"),
    );
    assert_eq!(image[0], VERSION_MAGIC, "VERSION_MAGIC");
    assert_eq!(image[1], COMPRESSED_EXT, "COMPRESSED_EXT");
    let size = u32::from_be_bytes([image[2], image[3], image[4], image[5]]) as usize;
    assert_eq!(
        size,
        plain.len() - 1,
        "the 4-byte size field is the body length (without the version byte)"
    );
    assert!(
        image.len() < plain.len(),
        "compression helped: {} vs {}",
        image.len(),
        plain.len()
    );

    // `binary_to_term/1` unwraps a top-level COMPRESSED image back to the equal
    // term — erl ... 'binary_to_term(term_to_binary(Big,[compressed])) =:= Big' % true
    let blob = codec_term::make_binary(ctx.heap(), &image).expect("fits");
    let back = reg
        .call("erlang", "binary_to_term", 1, &mut ctx, &[blob])
        .expect("binary_to_term of a COMPRESSED image");
    assert!(
        eq(big, back, &atoms),
        "binary_to_term(term_to_binary(Big,[compressed])) must equal Big"
    );

    // `{compressed, 9}` wraps the large body as well —
    // erl ... 'C9 = term_to_binary(Big,[{compressed,9}]),
    //          {binary:at(C9,0),binary:at(C9,1)}'  % {131,80}
    let level9 = option_tuple(&mut ctx, "compressed", 9);
    let image9 = bytes_of(
        reg.call("erlang", "term_to_binary", 2, &mut ctx, &[big, level9])
            .expect("a {compressed, 9} image"),
    );
    assert_eq!(&image9[..2], &[VERSION_MAGIC, COMPRESSED_EXT]);

    // `{compressed, 0}` stores without compressing, so the wrapper does not pay
    // off (`deflated.len() > body.len() - 5`): the image reverts to plain —
    // erl ... 'term_to_binary(Big,[{compressed,0}]) =:= term_to_binary(Big)' % true
    let level0 = option_tuple(&mut ctx, "compressed", 0);
    let image0 = bytes_of(
        reg.call("erlang", "term_to_binary", 2, &mut ctx, &[big, level0])
            .expect("a {compressed, 0} image"),
    );
    assert_eq!(image0, plain);
    assert_ne!(image0[1], COMPRESSED_EXT, "the image must have reverted");
}

#[test]
fn term_to_binary_rejects_malformed_options() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let term = ctx
        .list(&[ctx.int(1).expect("fits"), ctx.int(2).expect("fits")])
        .expect("fits");
    let compressed = ctx.atom("compressed").expect("seeded");

    // A bare `{compressed, 99}` tuple (not a list) — `badarg`
    // (`term_to_binary([1,2], {compressed,99})` ⇒ `badarg`).
    let tuple = ctx
        .tuple(&[compressed, ctx.int(99).expect("fits")])
        .expect("fits");
    assert_badarg(reg.call("erlang", "term_to_binary", 2, &mut ctx, &[term, tuple]));

    // A bare `compressed` atom (not a list) — `badarg`
    // (`term_to_binary([1,2], compressed)` ⇒ `badarg`).
    assert_badarg(reg.call("erlang", "term_to_binary", 2, &mut ctx, &[term, compressed]));

    // `[{compressed, 10}]` (level > 9) and `[{compressed, -1}]` — `badarg`
    // (`term_to_binary([1,2], [{compressed,10}])` / `[{compressed,-1}]` ⇒ `badarg`).
    for level in [10i64, -1] {
        let options = option_tuple(&mut ctx, "compressed", level);
        assert_badarg(reg.call("erlang", "term_to_binary", 2, &mut ctx, &[term, options]));
    }

    // `[{minor_version, -1}]` and `[{minor_version, 3}]` — `badarg`.
    for version in [-1i64, 3] {
        let options = option_tuple(&mut ctx, "minor_version", version);
        assert_badarg(reg.call("erlang", "term_to_binary", 2, &mut ctx, &[term, options]));
    }

    // An unknown option atom — `badarg` (`term_to_binary([1,2], [bogus])` ⇒ `badarg`).
    let unknown = option_atom(&mut ctx, "bogus");
    assert_badarg(reg.call("erlang", "term_to_binary", 2, &mut ctx, &[term, unknown]));

    // An improper option list `[{compressed, 6} | 1]` — `badarg`
    // (`term_to_binary([1,2], [{compressed,6}|x])` ⇒ `badarg`).
    let pair = ctx
        .tuple(&[compressed, ctx.int(6).expect("fits")])
        .expect("fits");
    let improper = ctx
        .cons(pair, ctx.int(1).expect("fits"))
        .expect("fits");
    assert_badarg(reg.call("erlang", "term_to_binary", 2, &mut ctx, &[term, improper]));
}

#[test]
fn term_to_binary_minor_version_2_is_the_default_image() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let a = ctx.atom("a").expect("seeded");
    let one = ctx.int(1).expect("fits");
    let map = codec_term::make_flatmap(ctx.heap(), &[(a, one)]).expect("fits");

    // erl -noshell -eval 'io:format("~p~n",[term_to_binary(#{a=>1},[{minor_version,2}])]),halt().'
    //   % <<131,116,0,0,0,1,119,1,97,97,1>>
    let expected = vec![VERSION_MAGIC, 116, 0, 0, 0, 1, 119, 1, 97, 97, 1];
    let mv2 = option_tuple(&mut ctx, "minor_version", 2);
    assert_eq!(
        bytes_of(
            reg.call("erlang", "term_to_binary", 2, &mut ctx, &[map, mv2])
                .expect("term_to_binary(#{a=>1},[{minor_version,2}])")
        ),
        expected
    );

    // ...and `{minor_version, 2}` is byte-identical to the default `/1` tag set —
    // erl ... 'term_to_binary(#{a=>1})' % <<131,116,0,0,0,1,119,1,97,97,1>>
    assert_eq!(
        bytes_of(
            reg.call("erlang", "term_to_binary", 1, &mut ctx, &[map])
                .expect("term_to_binary(#{a=>1})")
        ),
        expected
    );
}

#[test]
fn term_to_binary_minor_version_0_and_1_clear_the_utf8_atom_flag() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // An atom-free, float-free term is byte-identical for every minor version —
    // erl ... 'term_to_binary([1,2,3],[{minor_version,N}])'  % <<131,107,0,3,1,2,3>>
    let list = ctx
        .list(&[
            ctx.int(1).expect("fits"),
            ctx.int(2).expect("fits"),
            ctx.int(3).expect("fits"),
        ])
        .expect("fits");
    let expected = vec![VERSION_MAGIC, 107, 0, 3, 1, 2, 3];
    for version in [0i64, 1] {
        let options = option_tuple(&mut ctx, "minor_version", version);
        assert_eq!(
            bytes_of(
                reg.call("erlang", "term_to_binary", 2, &mut ctx, &[list, options])
                    .expect("an atom-free term encodes for mv 0/1")
            ),
            expected,
            "minor_version {version} on [1,2,3]"
        );
    }

    // A term whose image would carry an atom is refused with a typed
    // `unsupported_bif`: minor versions 0/1 clear `DFLAG_UTF8_ATOMS`, so the oracle
    // writes the Latin-1 `ATOM_EXT` (100) which `beam_codec` cannot yet emit —
    // erl ... 'term_to_binary(hello,[{minor_version,0}])'  % <<131,100,0,5,"hello">>
    //          'term_to_binary([hello],[{minor_version,1}])'
    //            % <<131,108,0,0,0,1,100,0,5,"hello",106>>
    let hello = ctx.atom("hello").expect("seeded");
    for version in [0i64, 1] {
        let options = option_tuple(&mut ctx, "minor_version", version);
        assert_unsupported_bif(reg.call("erlang", "term_to_binary", 2, &mut ctx, &[hello, options]));
    }

    // ...including an atom nested inside a container.
    let nested = ctx.list(&[hello]).expect("fits");
    for version in [0i64, 1] {
        let options = option_tuple(&mut ctx, "minor_version", version);
        assert_unsupported_bif(reg.call("erlang", "term_to_binary", 2, &mut ctx, &[nested, options]));
    }
}

/// An [`AtomInterner`](beam_bifs::AtomInterner) over a **clone** of the table the
/// context borrows.
///
/// This reproduces the index coupling `binary_to_term` relies on: the clone has
/// the context's indices for every already-interned name, and a name the hook
/// creates gets exactly the index the context's own table would have assigned —
/// which is the index the decoder wrote into the decoded term.
struct Interner {
    table: Rc<RefCell<AtomTable>>,
}

impl AtomInterner for Interner {
    fn intern(&mut self, name: &str) -> Atom {
        self.table.borrow_mut().intern(name)
    }
}

#[test]
fn export_ext_fun_round_trips_through_decode_and_encode() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();

    // The blob carries the fresh names `lists`/`reverse`, so decoding must intern
    // them through the hook (over a clone of the context table).
    let shared = Rc::new(RefCell::new(atoms.clone()));
    let fun = {
        let mut interner = Interner {
            table: Rc::clone(&shared),
        };
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_atom_interner(&mut interner);
        let blob = codec_term::make_binary(ctx.heap(), EXPORT_EXT_BLOB).expect("the blob fits");
        reg.call("erlang", "binary_to_term", 1, &mut ctx, &[blob])
            .expect("an EXPORT_EXT image decodes")
    };

    // The decoded term is the external fun `fun lists:reverse/1`
    // (erl ... 'binary_to_term(<<131,113,...>>)' ⇒ `fun lists:reverse/1`).
    assert_eq!(codec_term::fun_arity_term(fun).expect("a fun"), 1);
    let table = shared.borrow();
    assert_eq!(
        table.name(codec_term::fun_module(fun).expect("an external fun")),
        Some("lists")
    );
    assert_eq!(
        table.name(codec_term::fun_function(fun).expect("an external fun")),
        Some("reverse")
    );

    // Re-encode over the table the interner extended (the decoded fun's atom
    // indices are only valid there): the result is the very same `EXPORT_EXT`
    // image — erl ... 'term_to_binary(binary_to_term(<<131,113,...>>))'
    //   % <<131,113,119,5,108,105,115,116,115,119,7,114,101,118,101,114,115,101,97,1>>
    let mut heap2 = Heap::new(4096).expect("test heap");
    let mut ctx2 = BifContext::new(&mut heap2, &table, ProcessId::new(1, 0));
    let image = reg
        .call("erlang", "term_to_binary", 1, &mut ctx2, &[fun])
        .expect("the fun re-encodes");
    assert_eq!(bytes_of(image), EXPORT_EXT_BLOB.to_vec());
}

#[test]
fn binary_to_term_used_option_counts_the_version_byte() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // erl -noshell -eval 'io:format("~p~n",[binary_to_term(<<131,97,17>>,[used])]),halt().'
    //   % {17,3}   (the count includes the `131` version byte)
    let blob = codec_term::make_binary(ctx.heap(), &[131, 97, 17]).expect("fits");
    let used = option_atom(&mut ctx, "used");
    let result = reg
        .call("erlang", "binary_to_term", 2, &mut ctx, &[blob, used])
        .expect("binary_to_term(<<131,97,17>>, [used])");
    assert_eq!(codec_term::tuple_arity(result).expect("a 2-tuple"), 2);
    assert_eq!(
        signed_value(codec_term::tuple_element(result, 0).expect("element 0")),
        17
    );
    assert_eq!(
        signed_value(codec_term::tuple_element(result, 1).expect("element 1")),
        3
    );

    // Trailing bytes are ignored by `/1`, and the `used` count still stops at the
    // term's end — erl ... 'binary_to_term(<<131,97,17,99>>,[used])'  % {17,3}
    let trailing = codec_term::make_binary(ctx.heap(), &[131, 97, 17, 99]).expect("fits");
    let used = option_atom(&mut ctx, "used");
    let result = reg
        .call("erlang", "binary_to_term", 2, &mut ctx, &[trailing, used])
        .expect("binary_to_term(<<131,97,17,99>>, [used])");
    assert_eq!(
        signed_value(codec_term::tuple_element(result, 0).expect("element 0")),
        17
    );
    assert_eq!(
        signed_value(codec_term::tuple_element(result, 1).expect("element 1")),
        3
    );
}

#[test]
fn binary_to_term_safe_option_refuses_a_fresh_atom() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // erl -noshell -eval 'io:format("~p~n",[catch binary_to_term(
    //   <<131,119,8,"tjenixen">>,[safe])]),halt().'  % badarg
    let fresh = codec_term::make_binary(ctx.heap(), FRESH_ATOM_BLOB).expect("fits");
    let safe = option_atom(&mut ctx, "safe");
    assert_badarg(reg.call("erlang", "binary_to_term", 2, &mut ctx, &[fresh, safe]));

    // The premise: without `safe`, the very same image *does* decode (its atom is
    // created through the hook elsewhere) — here, with the plain context and no
    // interner, it reports the documented `TermNotFound` gap rather than `badarg`.
    let fresh = codec_term::make_binary(ctx.heap(), FRESH_ATOM_BLOB).expect("fits");
    assert!(matches!(
        reg.call("erlang", "binary_to_term", 1, &mut ctx, &[fresh]),
        Err(BeamError::TermNotFound)
    ));
}
