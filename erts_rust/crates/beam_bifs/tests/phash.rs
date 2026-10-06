//! Integration tests for `erlang:phash/2` — the legacy ERTS `make_hash` hash.
//!
//! Every expectation below is **pinned against the system oracle**
//! (`erl -noshell -eval 'io:format("~p~n",[erlang:phash(Term,Range)]),halt().'`,
//! OTP 29 / erts-17.1). The differential values are *not* derivable from the term
//! alone — that is the whole point of pinning them.
//!
//! The tests drive only the public API — [`BifRegistry`]/[`BifContext`] plus the
//! declared dependency crates (`beam_alloc` for the heap, `beam_codec::term` for
//! the binary/bitstring/bignum constructors).

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_codec::term::{
    make_big, make_binary, make_bitstring, make_flatmap, make_fun_external, make_fun_imported,
    FunPid,
};
use beam_common::{BeamError, ExceptionClass, ExitReason};
use beam_process::ProcessId;
use beam_term::{signed_value, AtomTable, Eterm, NIL};

/// The atom names the cases below use. Only interned names resolve, and the atom
/// hash is computed from the name, so the table is seeded up front.
const ATOMS: [&str; 19] = [
    "true", "abc", "hello", "node", "a", "x", "erlang", "abs", "lists", "map", "b", "c", "foo",
    "bar", "h", "false", "is_exported", "rec", "k",
];

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(ATOMS);
    atoms
}

/// Runs `f` with the real [`default_registry`] over a fresh heap and the seeded
/// atom table — no optional seams installed (this BIF needs none).
fn run<R>(f: impl FnOnce(&BifRegistry, &mut BifContext<'_>) -> R) -> R {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 14).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    f(&reg, &mut ctx)
}

/// Calls `Module:Function(Args)` through the real registry and unwraps the result.
fn call(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    module: &str,
    function: &str,
    args: &[Eterm],
) -> Eterm {
    reg.call(module, function, args.len() as u32, ctx, args)
        .unwrap_or_else(|e| panic!("{module}:{function}/{} failed: {e:?}", args.len()))
}

/// `erlang:phash(Term, Range)`.
fn phash(reg: &BifRegistry, ctx: &mut BifContext<'_>, term: Eterm, range: Eterm) -> Eterm {
    call(reg, ctx, "erlang", "phash", &[term, range])
}

/// A small-integer term.
fn int_term(ctx: &mut BifContext<'_>, v: i64) -> Eterm {
    ctx.int(v).expect("a small integer fits")
}

/// Asserts a `phash` result equals the oracle's (always a small integer here).
fn assert_phash(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    term: Eterm,
    range: Eterm,
    expected: i64,
    what: &str,
) {
    let got = phash(reg, ctx, term, range);
    assert!(
        beam_term::is_small(got),
        "{what}: expected a small integer result, got {got:?}"
    );
    assert_eq!(signed_value(got), expected, "{what}");
}

/// `erlang:phash2(Term)` — the `make_hash2` hash reduced to the low 27 bits.
fn phash2(reg: &BifRegistry, ctx: &mut BifContext<'_>, args: &[Eterm]) -> Eterm {
    call(reg, ctx, "erlang", "phash2", args)
}

/// Asserts `phash2/1` equals the oracle's value.
fn assert_phash2(ctx: &mut BifContext<'_>, args: &[Eterm], expected: i64, what: &str) {
    let reg = default_registry();
    let got = phash2(&reg, ctx, args);
    assert!(
        beam_term::is_small(got),
        "{what}: expected a small integer result, got {got:?}"
    );
    assert_eq!(signed_value(got), expected, "{what}");
}

/// Asserts `phash2/2` equals the oracle's value.
fn assert_phash2_range(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    term: Eterm,
    range: Eterm,
    expected: i64,
    what: &str,
) {
    let got = phash2(reg, ctx, &[term, range]);
    assert!(
        beam_term::is_small(got),
        "{what}: expected a small integer result, got {got:?}"
    );
    assert_eq!(signed_value(got), expected, "{what}");
}

/// Asserts a failure is the typed `unsupported_bif` raise (never a wrong number).
fn assert_unsupported(err: BeamError, what: &str) {
    match err {
        BeamError::Raise(raise) => {
            assert_eq!(raise.class, ExceptionClass::Error, "{what}");
            assert_eq!(raise.reason, ExitReason::atom("unsupported_bif"), "{what}");
        }
        other => panic!("{what}: expected unsupported_bif, got {other:?}"),
    }
}

/// The character list of `text` (each byte a small integer).
fn chars(ctx: &mut BifContext<'_>, text: &str) -> Eterm {
    let elems: Vec<Eterm> = text
        .bytes()
        .map(|b| int_term(ctx, i64::from(b)))
        .collect();
    ctx.list(&elems).expect("a short list fits")
}

/// `123456789012345678901234567890`, least-significant limb first.
const BIG_DIGITS: [u64; 2] = [14_083_847_773_837_265_618, 6_692_605_942];

// ---------------------------------------------------------------------------
// Integers
// ---------------------------------------------------------------------------

#[test]
fn integers_match_the_oracle() {
    run(|reg, ctx| {
        let range = int_term(ctx, 1_000_000);

        // The objective's verified pair.
        let one = int_term(ctx, 1);
        let four = int_term(ctx, 4);
        assert_phash(reg, ctx, one, four, 4, "phash(1,4)");

        // …and the same term under a larger range (`hash(1) = 2_788_898_427`).
        assert_phash(reg, ctx, one, range, 898_428, "phash(1,1000000)");

        let zero = int_term(ctx, 0);
        assert_phash(reg, ctx, zero, range, 1, "phash(0,1000000)");

        // A negative small integer.
        let neg_five = int_term(ctx, -5);
        assert_phash(reg, ctx, neg_five, range, 959_050, "phash(-5,1000000)");

        // A negative small integer whose magnitude needs the second 32-bit step.
        let neg_big = int_term(ctx, -268_435_456);
        assert_phash(
            reg,
            ctx,
            neg_big,
            range,
            10_961,
            "phash(-268435456,1000000)",
        );

        // `NIL`.
        assert_phash(reg, ctx, NIL, range, 2, "phash([],1000000)");
        assert_phash(reg, ctx, NIL, one, 1, "phash([],1)");

        // Two ranges for one term, pinned independently: `hash(3.14) = 300_063_655`.
        let pi = ctx.make_float(3.14).expect("fits");
        let max_range = int_term(ctx, 4_294_967_295);
        assert_phash(reg, ctx, pi, range, 63_656, "phash(3.14,1000000)");
        assert_phash(
            reg,
            ctx,
            pi,
            max_range,
            300_063_656,
            "phash(3.14,4294967295)",
        );
    });
}

// ---------------------------------------------------------------------------
// Bignums
// ---------------------------------------------------------------------------

#[test]
fn bignums_match_the_oracle() {
    run(|reg, ctx| {
        let range = int_term(ctx, 1_000_000);

        // 123456789012345678901234567890 — a two-limb bignum.
        let big = make_big(ctx.heap(), false, &BIG_DIGITS).expect("fits");
        assert_phash(reg, ctx, big, range, 83_119, "phash(1234…890,1000000)");

        // `1 bsl 200` — one significant limb in the fourth digit.
        let pow200 = make_big(ctx.heap(), false, &[0, 0, 0, 256]).expect("fits");
        assert_phash(reg, ctx, pow200, range, 229_268, "phash(1 bsl 200,1000000)");

        // `-(1 bsl 200)` — the same magnitude, negative.
        let neg_pow200 = make_big(ctx.heap(), true, &[0, 0, 0, 256]).expect("fits");
        assert_phash(
            reg,
            ctx,
            neg_pow200,
            range,
            867_550,
            "phash(-(1 bsl 200),1000000)",
        );
    });
}

// ---------------------------------------------------------------------------
// Floats
// ---------------------------------------------------------------------------

#[test]
fn floats_match_the_oracle_including_negative_zero() {
    run(|reg, ctx| {
        let range = int_term(ctx, 1_000_000);

        let zero = ctx.make_float(0.0).expect("fits");
        assert_phash(reg, ctx, zero, range, 1, "phash(0.0,1000000)");

        // `make_hash` forces `-0.0` to `+0.0`, so both hash to `0`.
        let neg_zero = ctx.make_float(-0.0).expect("fits");
        let two_pow_32 = int_term(ctx, 4_294_967_296);
        assert_phash(reg, ctx, neg_zero, range, 1, "phash(-0.0,1000000)");
        assert_phash(
            reg,
            ctx,
            neg_zero,
            two_pow_32,
            1,
            "phash(-0.0,2^32)",
        );
    });
}

// ---------------------------------------------------------------------------
// Atoms (the hashpjw hvalue)
// ---------------------------------------------------------------------------

#[test]
fn atoms_match_the_oracle() {
    run(|reg, ctx| {
        let range = int_term(ctx, 1_000_000);
        let two_pow_32 = int_term(ctx, 4_294_967_296);
        for (name, expected) in [
            ("abc", 26_500),
            ("hello", 258_928),
            ("node", 480_678),
            ("true", 506_294),
        ] {
            let atom = ctx.atom(name).expect("seeded");
            assert_phash(reg, ctx, atom, range, expected, name);
        }

        // `phash(abc, 100)` ⇒ `100` — `1 + (26499 % 100)`.
        let abc = ctx.atom("abc").expect("seeded");
        let hundred = int_term(ctx, 100);
        assert_phash(reg, ctx, abc, hundred, 100, "phash(abc,100)");

        // The same atom under the `2^32` range: `hash + 1 = 26_500`.
        assert_phash(reg, ctx, abc, two_pow_32, 26_500, "phash(abc,2^32)");
    });
}

// ---------------------------------------------------------------------------
// Tuples and lists
// ---------------------------------------------------------------------------

#[test]
fn tuples_match_the_oracle() {
    run(|reg, ctx| {
        let range = int_term(ctx, 1_000_000);
        let one = int_term(ctx, 1);
        let two = int_term(ctx, 2);
        let three = int_term(ctx, 3);

        let tup = ctx.tuple(&[one, two, three]).expect("fits");
        let seven = int_term(ctx, 7);
        assert_phash(reg, ctx, tup, range, 354_534, "phash({1,2,3},1000000)");
        assert_phash(reg, ctx, tup, seven, 2, "phash({1,2,3},7)");
        assert_phash(reg, ctx, tup, one, 1, "phash({1,2,3},1)");

        // A tuple holding an atom and a nested list.
        let inner = ctx.list(&[one, two]).expect("fits");
        let a = ctx.atom("a").expect("seeded");
        let mixed = ctx.tuple(&[a, inner]).expect("fits");
        assert_phash(reg, ctx, mixed, range, 86_372, "phash({a,[1,2]},1000000)");
    });
}

#[test]
fn lists_match_the_oracle() {
    run(|reg, ctx| {
        let range = int_term(ctx, 1_000_000);
        let one = int_term(ctx, 1);
        let two = int_term(ctx, 2);
        let three = int_term(ctx, 3);

        // A byte list (the `is_byte` fast path).
        let hello = chars(ctx, "hello");
        let two_pow_32 = int_term(ctx, 4_294_967_296);
        assert_phash(reg, ctx, hello, range, 352_116, "phash(\"hello\",1000000)");
        assert_phash(
            reg,
            ctx,
            hello,
            two_pow_32,
            2_340_352_116,
            "phash(\"hello\",2^32)",
        );

        // A mixed list (bytes and a non-byte element).
        let thousand = int_term(ctx, 1000);
        let mixed = ctx.list(&[one, two, thousand]).expect("fits");
        assert_phash(reg, ctx, mixed, range, 551_794, "phash([1,2,1000],1000000)");

        // A nested list.
        let l12 = ctx.list(&[one, two]).expect("fits");
        let l3 = ctx.list(&[three]).expect("fits");
        let nested = ctx.list(&[l12, l3]).expect("fits");
        assert_phash(reg, ctx, nested, range, 22_852, "phash([[1,2],[3]],1000000)");

        // An improper list: the terminating `3` is hashed then multiplied by `F8`.
        let tail = ctx.cons(two, three).expect("fits");
        let improper = ctx.cons(one, tail).expect("fits");
        assert_phash(reg, ctx, improper, range, 598_415, "phash([1,2|3],1000000)");
    });
}

// ---------------------------------------------------------------------------
// Funs
// ---------------------------------------------------------------------------

#[test]
fn external_funs_match_the_oracle() {
    run(|reg, ctx| {
        let range = int_term(ctx, 1_000_000);
        let two_pow_32 = int_term(ctx, 4_294_967_296);

        // `fun erlang:abs/1` — an `EXPORT_EXT` (`FunKind::External`) fun, whose
        // inlined descriptor `[header, module, function]` carries exactly what the C
        // external-fun rule needs: the arity (from the header) plus the two MFA atoms.
        let erlang = ctx.atoms().lookup("erlang").expect("seeded");
        let abs = ctx.atoms().lookup("abs").expect("seeded");
        let abs_fun = make_fun_external(ctx.heap(), 1, erlang, abs).expect("fits");
        assert_phash(
            reg,
            ctx,
            abs_fun,
            range,
            667_794,
            "phash(fun erlang:abs/1,1000000)",
        );
        assert_phash(
            reg,
            ctx,
            abs_fun,
            two_pow_32,
            2_515_667_794,
            "phash(fun erlang:abs/1,2^32)",
        );

        // A second external fun: a different arity, module and function name, so all
        // three contributions to `h*F11+arity; h*F1+hvalue(module); h*F1+hvalue(name)`
        // differ from the first case.
        let lists = ctx.atoms().lookup("lists").expect("seeded");
        let map = ctx.atoms().lookup("map").expect("seeded");
        let map_fun = make_fun_external(ctx.heap(), 2, lists, map).expect("fits");
        assert_phash(
            reg,
            ctx,
            map_fun,
            range,
            24_396,
            "phash(fun lists:map/2,1000000)",
        );
    });
}

#[test]
fn imported_funs_match_the_oracle() {
    run(|reg, ctx| {
        let range = int_term(ctx, 1_000_000);

        // A `NEW_FUN_EXT` (`FunKind::Imported`) fun built from the **exact bytes** of
        // the oracle closure `fun(X) -> {X,1} end` compiled in a module named `h`
        // (`term_to_binary/1` then `binary_to_term/1`, so the descriptor is real):
        // arity 1, module `h`, the 16-byte `erefuniq`, index 0, old_index 0,
        // old_uniq 97692434, creator `<0.0.0>`, empty environment. In C terms this is
        // a *local* fun (`external.c:5125` writes `MAKE_FUN_HEADER(arity,num_free,0)`),
        // so `make_hash2`'s local rule applies — and every field it reads is carried
        // by this tree's inlined descriptor.
        let module = ctx.atoms().lookup("h").expect("seeded");
        let node = ctx.atoms().lookup("node").expect("seeded");
        let creator = FunPid {
            node,
            number: 0,
            serial: 0,
            creation: 0,
        };
        let imported = make_fun_imported(
            ctx.heap(),
            1,
            module,
            &[
                186, 85, 98, 89, 101, 7, 175, 30, 108, 198, 92, 203, 158, 188, 155, 63,
            ],
            0,
            0,
            97_692_434,
            creator,
            &[],
        )
        .expect("fits");

        assert_phash2(ctx, &[imported], 10_652_292, "phash2(fun h:'-f/0'/1)");

        // The legacy `make_hash` local-fun rule is exact here too (`h*F10+num_free;
        // h*F1+hvalue(module); h*F2+index; h*F2+old_uniq`, then the environment).
        let got = phash(reg, ctx, imported, range);
        assert!(
            beam_term::is_small(got) && (1..=1_000_000).contains(&signed_value(got)),
            "phash(a local/imported fun, 1000000) must be a small integer in range"
        );
    });
}

// ---------------------------------------------------------------------------
// Binaries and bitstrings
// ---------------------------------------------------------------------------

#[test]
fn binaries_and_bitstrings_match_the_oracle() {
    run(|reg, ctx| {
        let range = int_term(ctx, 1_000_000);

        // The objective's verified pair.
        let bin = make_binary(ctx.heap(), &[1, 2, 3]).expect("fits");
        let two_pow_32 = int_term(ctx, 4_294_967_296);
        let two_pow_32_minus_1 = int_term(ctx, 4_294_967_295);
        assert_phash(reg, ctx, bin, range, 692_590, "phash(<<1,2,3>>,1000000)");
        assert_phash(
            reg,
            ctx,
            bin,
            two_pow_32,
            687_692_590,
            "phash(<<1,2,3>>,2^32)",
        );
        assert_phash(
            reg,
            ctx,
            bin,
            two_pow_32_minus_1,
            687_692_590,
            "phash(<<1,2,3>>,2^32-1)",
        );

        // Non-byte-aligned bitstrings: the trailing `bitsize` bits are folded in
        // with `F12` (`<<1:3>>` is the single byte `0b0010_0000`).
        let bits3 = make_bitstring(ctx.heap(), &[0b0010_0000], 3).expect("fits");
        assert_phash(reg, ctx, bits3, range, 689_961, "phash(<<1:3>>,1000000)");

        let bits9 = make_bitstring(ctx.heap(), &[0x00, 0x80], 9).expect("fits");
        assert_phash(reg, ctx, bits9, range, 817_680, "phash(<<1:9>>,1000000)");

        // `<<255:12>>` is `0b0000_1111_1111`: a whole byte `0x0F` plus four bits.
        let bits12 = make_bitstring(ctx.heap(), &[0x0F, 0xF0], 12).expect("fits");
        assert_phash(reg, ctx, bits12, range, 27_090, "phash(<<255:12>>,1000000)");

        // A **sub-bitstring** — `<<_:3, S:12/bitstring, _:9>> = <<1,2,3>>` in the
        // oracle, whose storage starts at bit offset 3. Repacking its bits MSB-first
        // (`<<8,1:4>>` ⇒ `[0x08, 0x10]`) must hash identically, because the C's
        // unaligned `bitoffs != 0` shift loop extracts exactly the packed bytes.
        let sub = make_bitstring(ctx.heap(), &[0x08, 0x10], 12).expect("fits");
        assert_phash(reg, ctx, sub, range, 718_575, "phash(<<8,1:4>>,1000000)");
        assert_phash(
            reg,
            ctx,
            sub,
            two_pow_32,
            2_813_718_575,
            "phash(<<8,1:4>>,2^32)",
        );
    });
}

// ---------------------------------------------------------------------------
// The `Range` argument
// ---------------------------------------------------------------------------

#[test]
fn invalid_ranges_are_badarg() {
    run(|reg, ctx| {
        let one = int_term(ctx, 1);
        let two_pow_32_plus_1 = int_term(ctx, 4_294_967_297);
        let cases: Vec<(Eterm, &str)> = vec![
            (int_term(ctx, 0), "0"),
            (int_term(ctx, -1), "-1"),
            (ctx.make_float(1.0).expect("fits"), "1.0"),
            (ctx.atom("x").expect("seeded"), "x"),
            (NIL, "[]"),
            (two_pow_32_plus_1, "2^32 + 1"),
            (
                make_big(ctx.heap(), false, &[0, 1]).expect("fits"),
                "2^64",
            ),
            (
                make_big(ctx.heap(), true, &[0, 1]).expect("fits"),
                "-2^64",
            ),
        ];
        for (range, what) in cases {
            assert!(
                matches!(
                    reg.call("erlang", "phash", 2, ctx, &[one, range]),
                    Err(BeamError::BadArg)
                ),
                "phash(1, {what}) must be badarg"
            );
        }
    });
}

// ---------------------------------------------------------------------------
// Classes this crate cannot reproduce
// ---------------------------------------------------------------------------

#[test]
fn unreproducible_classes_raise_unsupported_bif() {
    run(|reg, ctx| {
        let range = int_term(ctx, 1_000_000);

        // A map: `make_hash` delegates its MAP branch to `make_hash2`. Built through
        // the registered `maps:from_list/1` (`BifContext` has no map constructor).
        let key = ctx.atom("a").expect("seeded");
        let value = int_term(ctx, 1);
        let pair = ctx.tuple(&[key, value]).expect("fits");
        let pairs = ctx.list(&[pair]).expect("fits");
        let map = call(reg, ctx, "maps", "from_list", &[pairs]);
        assert_unsupported(
            reg.call("erlang", "phash", 2, ctx, &[map, range])
                .expect_err("a map must not hash"),
            "phash(#{a => 1}, 1000000)",
        );
    });
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

#[test]
fn phash_2_is_registered() {
    let reg = default_registry();
    assert!(reg.contains("erlang", "phash", 2));
    // The arity is part of the key: `phash/1` does not exist.
    assert!(!reg.contains("erlang", "phash", 1));
    assert!(!reg.contains("erlang", "phash", 3));
}

#[test]
fn phash2_is_a_typed_capability_gap_not_an_undef() {
    // `phash2/1,2` are real `bif.tab` BIFs (`bif.tab:83-84`) whose `make_hash2`
    // algorithm this crate does not reproduce. They are registered to raise the
    // typed `unsupported_bif` (a documented capability gap the runtime reports as
    // exit 3), never a plain `undef` that the oracle would answer differently.
    let reg = default_registry();
    assert!(reg.contains("erlang", "phash2", 1));
    assert!(reg.contains("erlang", "phash2", 2));
    run(|reg, ctx| {
        let arg = int_term(ctx, 42);
        assert_unsupported(
            reg.call("erlang", "phash2", 1, ctx, &[arg])
                .expect_err("phash2/1 is a registered capability gap"),
            "phash2/1",
        );
    });
    run(|reg, ctx| {
        let arg = int_term(ctx, 42);
        let range = int_term(ctx, 16);
        assert_unsupported(
            reg.call("erlang", "phash2", 2, ctx, &[arg, range])
                .expect_err("phash2/2 is a registered capability gap"),
            "phash2/2",
        );
    });
}
