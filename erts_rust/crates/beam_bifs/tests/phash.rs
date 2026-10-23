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

//! Integration tests for `erlang:phash/2` (the legacy ERTS `make_hash` term hash)
//! and `erlang:phash2/1,2` (the `make_hash2` block hash).
//!
//! Every expectation below is **pinned against the system oracle**
//! (`erl -noshell -eval 'io:format("~p~n",[erlang:phash(Term,Range)]),halt().'`,
//! OTP 29 / erts-17.1; the same values for a compiled literal-area copy). The
//! differential values are *not* derivable from the term alone — that is the whole
//! point of pinning them.
//!
//! The `make_fun3` (`FunKind::Local`) section additionally drives the
//! [`LocalFunIdentity`] seam: a closure carries only `uniq`/`index` inline, so its
//! hash needs the defining module from the host, and the section pins both the
//! attributed case (equal to the equivalent `NEW_FUN_EXT` image) and the three
//! unattributable ones (no hook, `Ok(None)`, a failing hook) — the last of which must
//! surface the seam's own error rather than the generic capability gap.
//!
//! The tests drive only the public API — [`BifRegistry`]/[`BifContext`] plus the
//! declared dependency crates (`beam_alloc` for the heap, `beam_codec::term` for
//! the binary/bitstring/bignum constructors).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, LocalFunIdentity};
use beam_codec::term::{
    bitstring_bytes, make_big, make_binary, make_bitstring, make_flatmap, make_fun_external,
    make_fun_imported, FunPid,
};
use beam_common::BeamError;
use beam_process::ProcessId;
use beam_term::fun::{fun_words, write_fun_local, FunKind};
use beam_term::{signed_value, Atom, AtomTable, Eterm, NIL};

/// The atom names the cases below use. Only interned names resolve, and the atom
/// hash is computed from the name, so the table is seeded up front.
///
/// Index 0/1 must be `false`/`true`: a native-record definition stores its
/// `is_exported` flag as the frozen `AM_FALSE`/`AM_TRUE` index, which the
/// definition hash resolves back to a *name* through this table, so the name at
/// index 1 must be `true` for the record hash to match the oracle.
const ATOMS: [&str; 21] = [
    "false", "true", "abc", "hello", "node", "a", "x", "erlang", "abs", "lists", "map", "b", "c",
    "foo", "bar", "h", "is_exported", "rec", "r", "k", "reverse",
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
// The hashed term is the float `3.14` (a three-digit float, not `f64::consts::PI`):
// the literal is the subject under test, so it must not become the constant.
#[allow(clippy::approx_constant)]
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
// `make_fun3` (`FunKind::Local`) funs — the fun-identity seam
// ---------------------------------------------------------------------------
//
// A `make_fun3` closure inlines only `[uniq:u64][index:u32]`: C's local-fun hash
// arms additionally mix `fe->module`'s atom-table `hvalue` (`erl_term_hashing.c:
// 242-261` for `make_hash`, `:1224-1246` for `make_hash2`), which the term does not
// carry. `erlang:phash/2`/`phash2/1,2` therefore ask the [`LocalFunIdentity`] seam
// for the closure's defining module (`src/compare.rs`'s `SeamFunIdentity`) — the same
// host answer that makes `term_to_binary/1` byte-faithful — and hash exactly like the
// equivalent `NEW_FUN_EXT` image when the seam can attribute the closure.
//
// The legacy identity below is the one `imported_funs_match_the_oracle` pins: the
// oracle closure `fun(X) -> {X,1} end` compiled in a module named `h`.

/// The 16-byte MD5 (`ErlFunEntry.uniq`) of that module — the `erefuniq` bytes of
/// the real `NEW_FUN_EXT` image.
const H_UNIQ: [u8; 16] = [
    186, 85, 98, 89, 101, 7, 175, 30, 108, 198, 92, 203, 158, 188, 155, 63,
];

/// `ErlFunEntry.old_uniq` of the same lambda — the word a `make_fun3` closure
/// stores as its `uniq` descriptor (`beam_term::fun::fun_uniq`).
const H_OLD_UNIQ: u32 = 97_692_434;

/// The pinned `phash2/1` value of that closure (`imported_funs_match_the_oracle`).
const H_PHASH2: i64 = 10_652_292;

/// Writes a `FunKind::Local` (`make_fun3`) closure block on `heap`.
///
/// The block is built with [`beam_term::fun`]'s own writer — the crate that owns the
/// frozen fun layout — rather than through the codec's convenience constructor, so
/// these cases hash a term laid out by the lowest common ancestor itself.
fn make_local_fun(heap: &mut Heap, arity: usize, uniq: u64, index: u32, env: &[Eterm]) -> Eterm {
    // SAFETY: `alloc_words` returns `fun_words(FunKind::Local, env.len())` fresh,
    // exclusively-owned and writable words in this test's own heap — exactly the
    // block size `write_fun_local` documents.
    let hp = unsafe { heap.alloc_words(fun_words(FunKind::Local, env.len())) }
        .expect("the test heap has room for one local fun");
    // SAFETY: `hp` owns exactly those words, and `write_fun_local` initialises every
    // one of them (header, `uniq`, `index`, then the `env` words).
    unsafe { write_fun_local(hp, arity, uniq, index, env) }.expect("a valid local-fun header")
}

/// The creator pid both sides of the identity use — `<0.0.0>`'s four descriptor
/// words, the same ones `imported_funs_match_the_oracle` builds its image with.
fn h_creator(atoms: &AtomTable) -> FunPid {
    FunPid {
        node: atoms.lookup("node").expect("seeded"),
        number: 0,
        serial: 0,
        creation: 0,
    }
}

/// A [`LocalFunIdentity`] hook that attributes every local fun to the oracle module
/// `h` — what `beam_rt` answers for a closure compiled in a loaded module.
struct HFunIdentity {
    module: Atom,
    creator: FunPid,
}

impl LocalFunIdentity for HFunIdentity {
    fn local_fun_identity(
        &mut self,
        _term: Eterm,
    ) -> beam_common::Result<Option<beam_codec::FunIdentity>> {
        Ok(Some(beam_codec::FunIdentity::new(
            self.module,
            H_UNIQ,
            0,
            H_OLD_UNIQ,
            self.creator,
        )))
    }
}

/// The "I cannot attribute this closure" answer — what the `-eval` `erl_eval`
/// template closure gets (it was never compiled by a module).
struct UnknownFunIdentity;

impl LocalFunIdentity for UnknownFunIdentity {
    fn local_fun_identity(
        &mut self,
        _term: Eterm,
    ) -> beam_common::Result<Option<beam_codec::FunIdentity>> {
        Ok(None)
    }
}

/// A hook whose seam fails outright. The failure is arbitrary — the property under
/// test is that it surfaces **as itself**, never as the generic capability gap.
struct FailingFunIdentity;

impl LocalFunIdentity for FailingFunIdentity {
    fn local_fun_identity(
        &mut self,
        _term: Eterm,
    ) -> beam_common::Result<Option<beam_codec::FunIdentity>> {
        Err(BeamError::BadArg)
    }
}

/// A context over `heap`/`atoms` with `hook` installed as the fun-identity seam.
///
/// [`BifContext::with_local_fun_identity`] consumes the context, so a hook-bearing
/// context cannot be produced by [`run`].
fn hook_context<'a>(
    heap: &'a mut Heap,
    atoms: &'a AtomTable,
    hook: &'a mut dyn LocalFunIdentity,
) -> BifContext<'a> {
    BifContext::new(heap, atoms, ProcessId::new(1, 0)).with_local_fun_identity(hook)
}

/// Asserts the call raised the crate's typed `unsupported_bif` capability gap.
fn assert_unsupported_bif(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    function: &str,
    args: &[Eterm],
    what: &str,
) {
    match reg.call("erlang", function, args.len() as u32, ctx, args) {
        Err(BeamError::Raise(raise)) => assert_eq!(
            raise.reason.as_atom(),
            Some("unsupported_bif"),
            "{what}: expected the typed unsupported_bif gap, got {:?}",
            raise.reason
        ),
        other => panic!("{what}: expected the typed unsupported_bif gap, got {other:?}"),
    }
}

#[test]
fn local_fun_hashes_match_its_imported_image_when_the_seam_attributes_it() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(1 << 14).expect("test heap");
    let module = atoms.lookup("h").expect("seeded");
    let creator = h_creator(&atoms);

    // The terms first: a hook-less context builds them and is then dropped, so the
    // heap can be re-borrowed by each leg below.
    let (local, imported) = {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let local = make_local_fun(ctx.heap(), 1, u64::from(H_OLD_UNIQ), 0, &[]);
        let imported = make_fun_imported(
            ctx.heap(),
            1,
            module,
            &H_UNIQ,
            0,
            0,
            H_OLD_UNIQ,
            creator,
            &[],
        )
        .expect("fits");
        (local, imported)
    };

    let mut hook = HFunIdentity { module, creator };
    let mut ctx = hook_context(&mut heap, &atoms, &mut hook);

    // `phash2/1` of the closure is the pinned `NEW_FUN_EXT` image's value: the C mixes
    // the same four fields (`num_free`, the module's `hvalue`, `index`, `old_uniq`)
    // and then the environment — for both representations of the same lambda.
    assert_phash2(
        &mut ctx,
        &[local],
        H_PHASH2,
        "phash2(a make_fun3 local fun attributed to module h)",
    );
    assert_phash2(&mut ctx, &[imported], H_PHASH2, "phash2(the same Image)");

    // The two remaining ports agree with the image's answers too. `phash2/2` folds the
    // *full* 32-bit hash to `hash rem Range` (so it is not the 27-bit `phash2/1` value
    // reduced); `phash/2` folds it to `1 + (hash rem Range)`.
    let range = int_term(&mut ctx, 1_000_000);
    let local_p2 = signed_value(phash2(&reg, &mut ctx, &[local, range]));
    let imported_p2 = signed_value(phash2(&reg, &mut ctx, &[imported, range]));
    assert_eq!(
        local_p2, imported_p2,
        "phash2(a local fun, 1000000) must equal phash2(its NEW_FUN_EXT image, 1000000)"
    );
    // Oracle-verified (OTP 29.1.1) for this closure: `make_hash2` of it is
    // `96_640_4…`, whose low 27 bits are `H_PHASH2` and whose `rem 1000000` is this.
    assert_eq!(local_p2, 966_404, "phash2(F, 1000000) of the h closure");

    let local_p1 = signed_value(phash(&reg, &mut ctx, local, range));
    let imported_p1 = signed_value(phash(&reg, &mut ctx, imported, range));
    assert_eq!(
        local_p1, imported_p1,
        "phash(a local fun, 1000000) must equal phash(its NEW_FUN_EXT image, 1000000)"
    );
    assert!(
        (1..=1_000_000).contains(&local_p1),
        "phash/2 answers `1 + hash rem Range`, so it lies in [1, Range]; got {local_p1}"
    );
}

#[test]
fn a_local_fun_no_seam_attributes_is_the_typed_gap() {
    // (a) No `LocalFunIdentity` hook at all: the defining module is unknown, so the
    // hash is the typed capability gap — never a number mixed from a substitute.
    run(|reg, ctx| {
        let local = make_local_fun(ctx.heap(), 1, u64::from(H_OLD_UNIQ), 0, &[]);
        let range = int_term(ctx, 1_000_000);
        assert_unsupported_bif(reg, ctx, "phash2", &[local], "phash2/1 without a seam");
        assert_unsupported_bif(
            reg,
            ctx,
            "phash2",
            &[local, range],
            "phash2/2 without a seam",
        );
        assert_unsupported_bif(
            reg,
            ctx,
            "phash",
            &[local, range],
            "phash/2 without a seam",
        );
    });

    // (b) A hook that answers `Ok(None)` — the `-eval`/`erl_eval` template closure,
    // which no module compiled. Same gap, same three ports.
    let mut hook = UnknownFunIdentity;
    let mut heap = Heap::new(1 << 14).expect("test heap");
    let atoms = seeded_atoms();
    let local = {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        make_local_fun(ctx.heap(), 1, u64::from(H_OLD_UNIQ), 0, &[])
    };
    let reg = default_registry();
    let mut ctx = hook_context(&mut heap, &atoms, &mut hook);
    let range = int_term(&mut ctx, 1_000_000);
    assert_unsupported_bif(&reg, &mut ctx, "phash2", &[local], "phash2/1, unknown identity");
    assert_unsupported_bif(
        &reg,
        &mut ctx,
        "phash2",
        &[local, range],
        "phash2/2, unknown identity",
    );
    assert_unsupported_bif(
        &reg,
        &mut ctx,
        "phash",
        &[local, range],
        "phash/2, unknown identity",
    );
}

#[test]
fn a_failing_identity_seam_surfaces_as_itself_not_as_the_gap() {
    // A seam *failure* is the host's error, not "the identity is unknown": it must be
    // reported unchanged (`BeamError::BadArg` here), never collapsed into the generic
    // `unsupported_bif` gap — otherwise a host bug would look like a capability gap.
    let mut hook = FailingFunIdentity;
    let mut heap = Heap::new(1 << 14).expect("test heap");
    let atoms = seeded_atoms();
    let local = {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        make_local_fun(ctx.heap(), 1, u64::from(H_OLD_UNIQ), 0, &[])
    };
    let reg = default_registry();
    let mut ctx = hook_context(&mut heap, &atoms, &mut hook);
    let range = int_term(&mut ctx, 1_000_000);

    for (function, args, what) in [
        ("phash2", vec![local], "phash2/1"),
        ("phash2", vec![local, range], "phash2/2"),
        ("phash", vec![local, range], "phash/2"),
    ] {
        match reg.call("erlang", function, args.len() as u32, &mut ctx, &args) {
            Err(BeamError::BadArg) => {}
            other => panic!(
                "{what} with a failing identity seam must report the seam's own \
                 error (badarg), got {other:?}"
            ),
        }
    }
}

#[test]
fn distinct_local_funs_hash_differently_and_stably() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(1 << 14).expect("test heap");
    let module = atoms.lookup("h").expect("seeded");
    let creator = h_creator(&atoms);

    // (label, uniq, index, env) — every field the local-fun hash arms mix except the
    // module, which the seam supplies.
    let (base, other_index, with_env) = {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let free = ctx.int(7).expect("a small integer fits");
        let base = make_local_fun(ctx.heap(), 1, u64::from(H_OLD_UNIQ), 0, &[]);
        let other_index = make_local_fun(ctx.heap(), 1, u64::from(H_OLD_UNIQ + 1), 1, &[]);
        let with_env = make_local_fun(ctx.heap(), 1, u64::from(H_OLD_UNIQ), 0, &[free]);
        (base, other_index, with_env)
    };

    let mut hook = HFunIdentity { module, creator };
    let mut ctx = hook_context(&mut heap, &atoms, &mut hook);
    // The `2^32` range keeps the whole 32-bit hash (`hash rem 2^32 == hash`), so these
    // comparisons are not narrowed to `phash2/1`'s low 27 bits.
    let full = int_term(&mut ctx, 4_294_967_296);
    let hash_of = |ctx: &mut BifContext<'_>, term: Eterm| {
        signed_value(phash2(&reg, ctx, &[term, full]))
    };

    let base_hash = hash_of(&mut ctx, base);
    let other_index_hash = hash_of(&mut ctx, other_index);
    let with_env_hash = hash_of(&mut ctx, with_env);
    assert_ne!(
        base_hash, other_index_hash,
        "a different `index`/`uniq` descriptor must hash differently"
    );
    assert_ne!(
        base_hash, with_env_hash,
        "a captured free variable must change the hash (`num_free` plus the env)"
    );
    assert_ne!(
        other_index_hash, with_env_hash,
        "the three local funs are pairwise distinct"
    );

    // The legacy port varies with the same fields (`h*F10+num_free; h*F1+hvalue(module);
    // h*F2+index; h*F2+old_uniq`, then the environment).
    assert_ne!(
        signed_value(phash(&reg, &mut ctx, base, full)),
        signed_value(phash(&reg, &mut ctx, other_index, full)),
        "`phash/2` must separate two descriptors that differ only in index/uniq"
    );
    assert_ne!(
        signed_value(phash(&reg, &mut ctx, base, full)),
        signed_value(phash(&reg, &mut ctx, with_env, full)),
        "`phash/2` must separate a closure from one with a captured variable"
    );

    // The same term hashed twice is the same number — the hash is a pure function of
    // the term (and of the seam's identity), never of call order.
    assert_eq!(
        hash_of(&mut ctx, base),
        base_hash,
        "hashing the same local fun twice must be stable"
    );
    assert_eq!(
        hash_of(&mut ctx, with_env),
        with_env_hash,
        "hashing the same local fun twice must be stable"
    );
}

#[test]
fn a_local_fun_round_trips_through_term_to_binary_with_its_hash() {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(1 << 14).expect("test heap");
    let module = atoms.lookup("h").expect("seeded");
    let creator = h_creator(&atoms);

    let local = {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        make_local_fun(ctx.heap(), 1, u64::from(H_OLD_UNIQ), 0, &[])
    };

    // Encoding a `make_fun3` closure needs the same seam (the codec's `NEW_FUN_EXT`
    // writer reads `context::local_fun_identity`).
    let image = {
        let mut hook = HFunIdentity { module, creator };
        let mut ctx = hook_context(&mut heap, &atoms, &mut hook);
        let encoded = call(&reg, &mut ctx, "erlang", "term_to_binary", &[local]);
        let (bytes, offset, size_bits) = bitstring_bytes(encoded).expect("a binary");
        assert_eq!(offset, 0, "`term_to_binary` builds a fresh, bit-0 binary");
        assert_eq!(size_bits % 8, 0, "`term_to_binary` is byte-aligned");
        bytes[..size_bits / 8].to_vec()
    };

    // The decode side needs no seam: a `NEW_FUN_EXT` image carries the whole identity.
    let decoded = {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let blob = make_binary(ctx.heap(), &image).expect("fits");
        call(&reg, &mut ctx, "erlang", "binary_to_term", &[blob])
    };

    let mut hook = HFunIdentity { module, creator };
    let mut ctx = hook_context(&mut heap, &atoms, &mut hook);
    let before = signed_value(phash2(&reg, &mut ctx, &[local]));
    assert_eq!(before, H_PHASH2, "phash2/1 of the make_fun3 closure");
    let after = signed_value(phash2(&reg, &mut ctx, &[decoded]));
    assert_eq!(
        after, before,
        "binary_to_term(term_to_binary(F)) must hash like F (the image carries the \
         whole identity)"
    );
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
// `phash2/1,2` — the `make_hash2` hash
// ---------------------------------------------------------------------------
//
// A *different* algorithm from `phash/2`: Bob Jenkins' `MIX` over a block hash
// (`erl_term_hashing.c:427-521, 858-1601`), so its goldens are pinned on their own.
// `phash2/1` is `hash & ((1 << 27) - 1)`; `phash2/2` is `hash rem Range` — no `+1`,
// unlike `phash/2`.

#[test]
// The hashed float is the three-digit `3.14` (whose word pair is what
// `phash2/1` folds), not `f64::consts::PI` — a literal golden by design.
#[allow(clippy::approx_constant)]
fn phash2_integers_match_the_oracle() {
    run(|_reg, ctx| {
        // Small integers inside `[-2^27, 2^27)` take the `SINT32_HASH` path; the four
        // values around that boundary pin the `IS_SSMALL28` split that selects it.
        for (value, expected) in [
            (0i64, 88_723_725),
            (1, 2_614_250),
            (-1, 44_071_773),
            (-5, 115_015_680),
            (1_000_000, 109_607_396),
            (134_217_727, 112_602_999),
            (134_217_728, 12_354_923),
            (-134_217_728, 69_672_967),
            (-134_217_729, 76_739_502),
            (268_435_456, 45_246_278),
            (-268_435_457, 86_968_359),
        ] {
            let term = int_term(ctx, value);
            assert_phash2(ctx, &[term], expected, &format!("phash2({value})"));
        }

        // Floats fold their normalised `(high, low)` word pair; `-0.0` is normalised
        // to `+0.0`, so both zeros agree with the oracle.
        for (value, expected) in [
            (1.0f64, 77_147_068),
            (0.0, 20_875_736),
            (-0.0, 20_875_736),
            (3.14, 107_830_559),
        ] {
            let term = ctx.make_float(value).expect("fits");
            assert_phash2(ctx, &[term], expected, &format!("phash2({value})"));
        }
    });
}

#[test]
fn phash2_containers_match_the_oracle() {
    run(|_reg, ctx| {
        let one = int_term(ctx, 1);
        let two = int_term(ctx, 2);
        let three = int_term(ctx, 3);
        let a = ctx.atom("a").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let c = ctx.atom("c").expect("seeded");
        let foo = ctx.atom("foo").expect("seeded");
        let bar = ctx.atom("bar").expect("seeded");

        // The arity is mixed in *before* the elements.
        let empty_tuple = ctx.tuple(&[]).expect("fits");
        assert_phash2(ctx, &[empty_tuple], 87_486_268, "phash2({})");

        let pair = ctx.tuple(&[a, one]).expect("fits");
        assert_phash2(ctx, &[pair], 72_425_156, "phash2({a,1})");

        let six = ctx.tuple(&[a, b, c, one, two, three]).expect("fits");
        assert_phash2(ctx, &[six], 23_473_964, "phash2({a,b,c,1,2,3})");

        let nums = ctx.list(&[one, two, three]).expect("fits");
        assert_phash2(ctx, &[nums], 25_788_620, "phash2([1,2,3])");
        assert_phash2(ctx, &[NIL], 113_427_502, "phash2([])");

        let atoms = ctx.list(&[a, b, c]).expect("fits");
        assert_phash2(ctx, &[atoms], 29_271_916, "phash2([a,b,c])");

        // An improper list: `cons(foo, bar)` is `[foo | bar]`.
        let improper = ctx.cons(foo, bar).expect("fits");
        assert_phash2(ctx, &[improper], 5_376_464, "phash2([foo|bar])");

        // A byte list — the four-byte `HCONST_4` run fast path.
        let hello = chars(ctx, "hello");
        assert_phash2(ctx, &[hello], 81_920_127, r#"phash2("hello")"#);

        let l12 = ctx.list(&[one, two]).expect("fits");
        let l3 = ctx.list(&[three]).expect("fits");
        let nested = ctx.list(&[l12, l3]).expect("fits");
        assert_phash2(ctx, &[nested], 118_504_057, "phash2([[1,2],[3]])");

        // A composite: a float, a list, a binary and an empty tuple.
        let half = ctx.make_float(1.5).expect("fits");
        let ab = ctx.list(&[a, b]).expect("fits");
        let bin = make_binary(ctx.heap(), &[1, 2]).expect("fits");
        let composite = ctx.tuple(&[half, ab, bin, empty_tuple]).expect("fits");
        assert_phash2(
            ctx,
            &[composite],
            56_771_977,
            "phash2({1.5,[a,b],<<1,2>>,{}})",
        );

        assert_phash2(ctx, &[foo], 27_999, "phash2(foo)");

        // The empty map: only the size mix applies.
        let empty_map = make_flatmap(ctx.heap(), &[]).expect("fits");
        assert_phash2(ctx, &[empty_map], 39_679_005, "phash2(#{})");
    });
}

#[test]
fn phash2_binaries_and_bitstrings_match_the_oracle() {
    run(|_reg, ctx| {
        let empty = make_binary(ctx.heap(), &[]).expect("fits");
        assert_phash2(ctx, &[empty], 13_708_901, r#"phash2(<<>>)"#);

        let ab = make_binary(ctx.heap(), b"ab").expect("fits");
        assert_phash2(ctx, &[ab], 127_597_802, r#"phash2(<<"ab">>)"#);

        let bin = make_binary(ctx.heap(), &[1, 2, 3]).expect("fits");
        assert_phash2(ctx, &[bin], 6_479_071, r#"phash2(<<1,2,3>>)"#);

        // Non-byte-aligned bitstrings: the trailing partial byte folds in with
        // `HCONST_15` after the whole bytes went through `block_hash`.
        let bits3 = make_bitstring(ctx.heap(), &[0b0010_0000], 3).expect("fits");
        assert_phash2(ctx, &[bits3], 73_037_028, r#"phash2(<<1:3>>)"#);

        // `<<5:3,3:2>>` is the five bits `10111` — `0b1011_1000` padded.
        let bits5 = make_bitstring(ctx.heap(), &[0b1011_1000], 5).expect("fits");
        assert_phash2(ctx, &[bits5], 126_763_390, r#"phash2(<<5:3,3:2>>)"#);

        // `<<255:12>>` is `0b0000_1111_1111`: the byte `0x0F` plus four bits.
        let bits12 = make_bitstring(ctx.heap(), &[0x0F, 0xF0], 12).expect("fits");
        assert_phash2(ctx, &[bits12], 26_733_754, r#"phash2(<<255:12>>)"#);
    });
}

#[test]
fn phash2_maps_match_the_oracle() {
    run(|_reg, ctx| {
        let a = ctx.atom("a").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let c = ctx.atom("c").expect("seeded");
        let one = int_term(ctx, 1);
        let two = int_term(ctx, 2);
        let three = int_term(ctx, 3);

        let one_pair = make_flatmap(ctx.heap(), &[(a, one)]).expect("fits");
        assert_phash2(ctx, &[one_pair], 59_617_982, "phash2(#{a => 1})");

        let three_pairs =
            make_flatmap(ctx.heap(), &[(a, one), (b, two), (c, three)]).expect("fits");
        assert_phash2(
            ctx,
            &[three_pairs],
            98_643_667,
            "phash2(#{a => 1, b => 2, c => 3})",
        );

        // A nested map and list: each pair is hashed from an independent `0`
        // accumulator and the pair hashes are xor-ed, so nesting recurses exactly as
        // the C does — and the result is deliberately independent of pair order.
        let inner = make_flatmap(ctx.heap(), &[(b, two)]).expect("fits");
        let list = ctx.list(&[one, two, three]).expect("fits");
        let nested = make_flatmap(ctx.heap(), &[(a, inner), (c, list)]).expect("fits");
        assert_phash2(
            ctx,
            &[nested],
            40_732_915,
            "phash2(#{a => #{b => 2}, c => [1,2,3]})",
        );

        // A two-limb bignum and its negative — the `HCONST_10`/`HCONST_11` limb fold.
        let big = make_big(ctx.heap(), false, &BIG_DIGITS).expect("fits");
        assert_phash2(ctx, &[big], 132_143_600, "phash2(1234…890)");
        let neg_big = make_big(ctx.heap(), true, &BIG_DIGITS).expect("fits");
        assert_phash2(ctx, &[neg_big], 33_199_935, "phash2(-(1234…890))");
    });
}

#[test]
fn phash2_external_funs_match_the_oracle() {
    run(|_reg, ctx| {
        // `FUN_SUBTAG` external fun: `(arity, hvalue(module))` with `HCONST`, then
        // `hvalue(function)` with `HCONST_14`.
        let erlang = ctx.atoms().lookup("erlang").expect("seeded");
        let abs = ctx.atoms().lookup("abs").expect("seeded");
        let abs_fun = make_fun_external(ctx.heap(), 1, erlang, abs).expect("fits");
        assert_phash2(ctx, &[abs_fun], 27_471_588, "phash2(fun erlang:abs/1)");

        // A second external fun — a different module and function name, so both
        // `hvalue` contributions and the arity differ from the first case.
        let lists = ctx.atoms().lookup("lists").expect("seeded");
        let reverse = ctx.atoms().lookup("reverse").expect("seeded");
        let rev_fun = make_fun_external(ctx.heap(), 1, lists, reverse).expect("fits");
        assert_phash2(ctx, &[rev_fun], 134_027_972, "phash2(fun lists:reverse/1)");
    });
}

#[test]
fn phash2_range_matches_the_oracle() {
    run(|reg, ctx| {
        let thousand = int_term(ctx, 1000);
        let foo = ctx.atom("foo").expect("seeded");
        let a = ctx.atom("a").expect("seeded");
        let one = int_term(ctx, 1);
        let two = int_term(ctx, 2);
        let three = int_term(ctx, 3);

        let pair = ctx.tuple(&[a, one]).expect("fits");
        let list = ctx.list(&[one, two, three]).expect("fits");
        let bin = make_binary(ctx.heap(), b"ab").expect("fits");
        let map = make_flatmap(ctx.heap(), &[(a, one)]).expect("fits");
        let empty_tuple = ctx.tuple(&[]).expect("fits");
        let empty_map = make_flatmap(ctx.heap(), &[]).expect("fits");
        let empty_bin = make_binary(ctx.heap(), &[]).expect("fits");
        let million = int_term(ctx, 1_000_000);
        let neg_five = int_term(ctx, -5);

        assert_phash2_range(reg, ctx, foo, thousand, 999, "phash2(foo,1000)");
        assert_phash2_range(reg, ctx, pair, thousand, 996, "phash2({a,1},1000)");
        assert_phash2_range(reg, ctx, list, thousand, 188, "phash2([1,2,3],1000)");
        assert_phash2_range(reg, ctx, bin, thousand, 530, r#"phash2(<<"ab">>,1000)"#);
        assert_phash2_range(reg, ctx, map, thousand, 262, "phash2(#{a => 1},1000)");
        assert_phash2_range(reg, ctx, million, thousand, 324, "phash2(1000000,1000)");
        assert_phash2_range(reg, ctx, neg_five, thousand, 864, "phash2(-5,1000)");
        assert_phash2_range(reg, ctx, NIL, thousand, 702, "phash2([],1000)");
        assert_phash2_range(reg, ctx, empty_tuple, thousand, 996, "phash2({},1000)");
        assert_phash2_range(reg, ctx, empty_map, thousand, 373, "phash2(#{},1000)");

        // The `2^32 - 1` upper bound: `hash < Range`, so the result is the whole
        // hash (`27999` — the same value `phash2/1` reduces from).
        let seven = int_term(ctx, 7);
        assert_phash2_range(reg, ctx, list, seven, 0, "phash2([1,2,3],7)");
        let sixteen = int_term(ctx, 16);
        assert_phash2_range(reg, ctx, foo, sixteen, 15, "phash2(foo,16)");
        assert_phash2_range(reg, ctx, foo, one, 0, "phash2(foo,1)");
        let max_range = int_term(ctx, 4_294_967_295);
        assert_phash2_range(reg, ctx, foo, max_range, 27_999, "phash2(foo,2^32-1)");
        let range_997 = int_term(ctx, 997);
        assert_phash2_range(reg, ctx, empty_bin, range_997, 742, r#"phash2(<<>>,997)"#);
    });
}

#[test]
fn phash2_invalid_ranges_are_badarg() {
    run(|reg, ctx| {
        let one = int_term(ctx, 1);
        let cases: Vec<(Eterm, &str)> = vec![
            (int_term(ctx, 0), "0"),
            (int_term(ctx, -1), "-1"),
            (ctx.make_float(1.0).expect("fits"), "1.0"),
            (ctx.atom("x").expect("seeded"), "x"),
            (NIL, "[]"),
            (int_term(ctx, 4_294_967_297), "2^32 + 1"),
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
                    reg.call("erlang", "phash2", 2, ctx, &[one, range]),
                    Err(BeamError::BadArg)
                ),
                "phash2(1, {what}) must be badarg"
            );
        }
    });
}

// ---------------------------------------------------------------------------
// The legacy `make_hash` branches that were closed with the `phash2` work
// ---------------------------------------------------------------------------

#[test]
fn phash_maps_and_funs_match_the_oracle() {
    run(|reg, ctx| {
        let range = int_term(ctx, 1_000_000_000);
        let a = ctx.atom("a").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let c = ctx.atom("c").expect("seeded");
        let one = int_term(ctx, 1);
        let two = int_term(ctx, 2);
        let three = int_term(ctx, 3);

        // The `MAP_DEF` rule delegates the map's whole contribution to the *other*
        // hash: `h*F13 + F14 + make_hash2(map)`.
        let one_pair = make_flatmap(ctx.heap(), &[(a, one)]).expect("fits");
        assert_phash(
            reg,
            ctx,
            one_pair,
            range,
            670_235_874,
            "phash(#{a => 1},1000000000)",
        );
        let three_pairs =
            make_flatmap(ctx.heap(), &[(a, one), (b, two), (c, three)]).expect("fits");
        assert_phash(
            reg,
            ctx,
            three_pairs,
            range,
            185_656_567,
            "phash(#{a => 1, b => 2, c => 3},1000000000)",
        );

        // The external-fun rule: `h*F11 + arity; h*F1 + hvalue(module);
        // h*F1 + hvalue(function)`.
        let erlang = ctx.atoms().lookup("erlang").expect("seeded");
        let abs = ctx.atoms().lookup("abs").expect("seeded");
        let abs_fun = make_fun_external(ctx.heap(), 1, erlang, abs).expect("fits");
        assert_phash(
            reg,
            ctx,
            abs_fun,
            range,
            515_667_794,
            "phash(fun erlang:abs/1,1000000000)",
        );
    });
}

// ---------------------------------------------------------------------------
// Native records hash to the oracle's definition hash
// ---------------------------------------------------------------------------

/// A native record's `make_hash`/`make_hash2` identity is its definition's
/// computed hash (`beam_term::hashing::compute_record_definition_hash`), so both
/// hashes agree with the oracle both at the top level and nested inside a tuple,
/// a list and a flat map.
///
/// The record is built **through the real registry**
/// (`records:create(rec, r, [{a,1}], #{is_exported => true})`); the exported
/// one-field shape is the one whose oracle definition hash is stable run-to-run.
///
/// A *reference* cannot be built from this integration test (there is no public
/// internal-reference constructor), so the reference-refusal contract lives in the
/// `src/compare.rs` in-module test
/// `internal_references_stay_the_typed_unsupported_bif_gap`.
#[test]
fn records_hash_to_the_oracle_definition_hash() {
    run(|reg, ctx| {
        let key = ctx.atom("a").expect("seeded");
        let value = int_term(ctx, 1);
        let pair = ctx.tuple(&[key, value]).expect("fits");
        let fields = ctx.list(&[pair]).expect("fits");
        let module = ctx.atom("rec").expect("seeded");
        let name = ctx.atom("r").expect("seeded");
        let opt_key = ctx.atom("is_exported").expect("seeded");
        let opt_value = ctx.atom("true").expect("seeded");
        let opts = make_flatmap(ctx.heap(), &[(opt_key, opt_value)]).expect("fits");
        let rec = call(reg, ctx, "records", "create", &[module, name, fields, opts]);

        let range = int_term(ctx, 1_000_000);

        // Top level.
        assert_phash2(ctx, &[rec], 33_491_412, "phash2(a record)");
        assert_phash2_range(reg, ctx, rec, range, 709_140, "phash2(a record, 1000000)");
        assert_phash(reg, ctx, rec, range, 567_678, "phash(a record, 1000000)");

        // `{1, Record}`.
        let in_tuple = ctx.tuple(&[value, rec]).expect("fits");
        assert_phash2(ctx, &[in_tuple], 105_608_320, "phash2({1, record})");
        assert_phash2_range(
            reg,
            ctx,
            in_tuple,
            range,
            874_240,
            "phash2({1, record}, 1000000)",
        );
        assert_phash(reg, ctx, in_tuple, range, 964_745, "phash({1, record}, 1000000)");

        // `[Record, 1]`.
        let in_list = ctx.list(&[rec, value]).expect("fits");
        assert_phash2(ctx, &[in_list], 46_225_311, "phash2([record, 1])");
        assert_phash2_range(reg, ctx, in_list, range, 967_135, "phash2([record, 1], 1000000)");
        assert_phash(reg, ctx, in_list, range, 812_886, "phash([record, 1], 1000000)");

        // `#{a => Record}`.
        let in_map = make_flatmap(ctx.heap(), &[(key, rec)]).expect("fits");
        assert_phash2(ctx, &[in_map], 7_457_474, "phash2(#{a => record})");
        assert_phash2_range(
            reg,
            ctx,
            in_map,
            range,
            553_858,
            "phash2(#{a => record}, 1000000)",
        );
        assert_phash(reg, ctx, in_map, range, 994_470, "phash(#{a => record}, 1000000)");
    });
}

// ---------------------------------------------------------------------------
// Binaries and bitstrings
// ---------------------------------------------------------------------------

/// `phash2/1` of a binary payload of **12 bytes or more**, against the oracle.
///
/// A length of ≥ 12 exercises `block_hash`'s 12-byte unrolled loop, whose `MIX`
/// folds the mutated `a`/`b`/`c` forward into the tail-bytes fold — so these
/// values pin both the per-iteration carry and the final fold (lengths 0..11 only
/// take the tail path).
#[test]
fn phash2_binaries_of_twelve_bytes_or_more_match_the_oracle() {
    run(|_reg, ctx| {
        for (len, expected) in BINARY_SWEEP {
            let bytes = sweep_payload(len);
            let bin = make_binary(ctx.heap(), &bytes).expect("fits");
            assert_phash2(
                ctx,
                &[bin],
                expected,
                &format!("phash2(a {len}-byte binary)"),
            );
        }
    });
}

/// `payload[i] = (i * 7 + 3) mod 256`, so every length is distinguishable.
fn sweep_payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 7 + 3) % 256) as u8).collect()
}

/// `(length, oracle erlang:phash2/1)` for `sweep_payload(length)`, covering 0..=64
/// (all tail lengths and several loop iterations) plus a few 100+/1000+ payloads.
const BINARY_SWEEP: [(usize, i64); 70] = [
    (0, 13_708_901),
    (1, 110_786_568),
    (2, 66_323_310),
    (3, 54_114_709),
    (4, 29_944_881),
    (5, 46_655_611),
    (6, 60_461_966),
    (7, 87_067_119),
    (8, 29_465_626),
    (9, 115_750_623),
    (10, 13_298_210),
    (11, 6_277_990),
    (12, 43_353_299),
    (13, 101_390_983),
    (14, 16_032_732),
    (15, 129_358_774),
    (16, 127_549_786),
    (17, 63_262_311),
    (18, 98_915_469),
    (19, 109_044_614),
    (20, 95_852_011),
    (21, 30_756_980),
    (22, 82_493_865),
    (23, 101_444_995),
    (24, 11_879_809),
    (25, 19_225_834),
    (26, 37_272_023),
    (27, 33_331_196),
    (28, 113_893_337),
    (29, 32_449_564),
    (30, 58_858_503),
    (31, 130_388_158),
    (32, 27_873_932),
    (33, 107_969_872),
    (34, 34_568_304),
    (35, 37_708_219),
    (36, 52_616_682),
    (37, 83_758_007),
    (38, 106_657_277),
    (39, 82_778_620),
    (40, 4_148_690),
    (41, 46_351_670),
    (42, 65_061_131),
    (43, 57_748_611),
    (44, 58_602_716),
    (45, 79_122_520),
    (46, 19_092_667),
    (47, 24_720_672),
    (48, 87_896_660),
    (49, 19_009_191),
    (50, 106_899_171),
    (51, 22_914_714),
    (52, 41_431_194),
    (53, 12_085_088),
    (54, 51_274_675),
    (55, 66_913_128),
    (56, 57_933_425),
    (57, 47_943_959),
    (58, 105_731_260),
    (59, 66_071_240),
    (60, 59_671_866),
    (61, 46_414_219),
    (62, 93_247_227),
    (63, 50_941_664),
    (64, 39_181_186),
    (100, 60_468_140),
    (200, 970_580),
    (1000, 98_493_776),
    (1001, 74_767_649),
    (1200, 128_287_640),
];

/// `phash2/1` of a **non-byte-aligned** bitstring `<<Payload/binary, R:R>>`.
///
/// This pins the trailing-bits fold (`HCONST_15`) after a `block_hash` that itself
/// ran one full 12-byte iteration on the byte-aligned prefix.
#[test]
fn phash2_non_byte_aligned_bitstrings_match_the_oracle() {
    run(|_reg, ctx| {
        // `(payload length in bytes, trailing bits, oracle value)`.
        let cases: [(usize, u8, i64); 16] = [
            (11, 1, 128_602_037),
            (11, 3, 18_899_048),
            (11, 5, 19_126_151),
            (11, 7, 50_145_949),
            (12, 1, 15_612_406),
            (12, 3, 108_349_436),
            (12, 5, 64_923_878),
            (12, 7, 26_910_108),
            (13, 1, 45_084_759),
            (13, 3, 57_191_926),
            (13, 5, 113_897_411),
            (13, 7, 93_853_623),
            (14, 1, 93_395_084),
            (14, 3, 54_513_294),
            (14, 5, 54_518_462),
            (14, 7, 54_265_566),
        ];
        for (k, r, expected) in cases {
            let mut bytes = sweep_payload(k);
            // `R` in the top `r` bits (the MSB-first packing `make_bitstring` wants).
            bytes.push(r << (8 - r));
            let bits = make_bitstring(ctx.heap(), &bytes, k * 8 + usize::from(r)).expect("fits");
            assert_phash2(
                ctx,
                &[bits],
                expected,
                &format!("phash2(<<{k}-byte payload, {r}:{r}>>)"),
            );
        }
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
fn phash2_is_registered() {
    // `bif.tab:83-84` — both arities are real BIFs, and both are implemented (no
    // longer a registered capability gap).
    let reg = default_registry();
    assert!(reg.contains("erlang", "phash2", 1));
    assert!(reg.contains("erlang", "phash2", 2));
    assert!(!reg.contains("erlang", "phash2", 3));
}

// ---------------------------------------------------------------------------
// The live oracle
// ---------------------------------------------------------------------------
//
// The local-fun goldens above are pinned numbers; this leg re-derives them from a
// real VM. The oracle cannot be handed a `make_fun3` closure through `-eval` without
// compiling a module, so it hashes the **`NEW_FUN_EXT` image** the closure encodes to
// instead: for C both are the same `ErlFunEntry`-backed local fun, which is exactly
// what these cases assert. Skips loudly (a banner, never a silent pass) when no
// oracle can be run.

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && path
            .metadata()
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// `$ERTS_ORACLE` when it names an executable, else `erl` on `$PATH` — the same
/// resolution `tools/differential` and this crate's sibling oracle files use.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        if is_executable(&program) {
            return Some(program);
        }
        eprintln!(
            "[beam_bifs phash] $ERTS_ORACLE='{}' is not an existing executable — \
             treating it as no oracle.",
            program.display()
        );
        return None;
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join("erl"))
        .find(|candidate| is_executable(candidate))
}

#[test]
fn local_fun_hashes_match_the_live_oracle_through_its_imported_image() {
    let Some(oracle) = resolve_oracle() else {
        eprintln!();
        eprintln!("=========== beam_bifs phash: live oracle SKIPPED ===========");
        eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or not");
        eprintln!("executable, and no `erl` on $PATH).");
        eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP).");
        eprintln!("===========================================================");
        eprintln!();
        return;
    };

    let reg = default_registry();
    let atoms = seeded_atoms();
    let mut heap = Heap::new(1 << 14).expect("test heap");
    let module = atoms.lookup("h").expect("seeded");
    let creator = h_creator(&atoms);

    let local = {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        make_local_fun(ctx.heap(), 1, u64::from(H_OLD_UNIQ), 0, &[])
    };

    // The closure's wire image, produced by this crate's own encoder through the same
    // fun-identity seam the hash uses.
    let image = {
        let mut hook = HFunIdentity { module, creator };
        let mut ctx = hook_context(&mut heap, &atoms, &mut hook);
        let encoded = call(&reg, &mut ctx, "erlang", "term_to_binary", &[local]);
        let (bytes, offset, size_bits) = bitstring_bytes(encoded).expect("a binary");
        assert_eq!(offset, 0, "`term_to_binary` builds a fresh, bit-0 binary");
        assert_eq!(size_bits % 8, 0, "`term_to_binary` is byte-aligned");
        bytes[..size_bits / 8].to_vec()
    };

    // `F = binary_to_term(<<…>>)` is the oracle's own local fun for that lambda.
    let literal = image
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let eval = format!(
        "F = binary_to_term(<<{literal}>>), \
         io:format(\"~w ~w ~w~n\", [erlang:phash2(F), erlang:phash2(F, 1000000), \
         erlang:phash(F, 1000000)]), halt()."
    );

    // A scratch cwd of our own, so an `erl_crash.dump` can never land in the repo.
    let scratch = std::env::temp_dir().join(format!("beam_bifs_phash_{}", std::process::id()));
    std::fs::create_dir_all(&scratch)
        .unwrap_or_else(|e| panic!("cannot create {}: {e}", scratch.display()));
    let output = Command::new(&oracle)
        .args(["-noshell", "-eval", &eval, "-s", "init", "stop"])
        .current_dir(&scratch)
        .stdin(Stdio::null())
        .output();
    // Removed before any assertion below, so a failed comparison leaves nothing behind.
    match std::fs::remove_dir_all(&scratch) {
        Ok(()) => {}
        Err(error) => println!("note: could not remove {}: {error}", scratch.display()),
    }
    let output = output
        .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", oracle.display()));
    assert!(
        output.status.success(),
        "the oracle '{}' exited with {:?}\n--- stderr ---\n{}\n--- eval ---\n{eval}",
        oracle.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout)
        .unwrap_or_else(|e| panic!("the oracle printed non-UTF-8 to stdout: {e}"));
    let theirs: Vec<i64> = stdout
        .split_whitespace()
        .map(|field| {
            field
                .parse()
                .unwrap_or_else(|e| panic!("the oracle printed {field:?}, not an integer: {e}"))
        })
        .collect();
    assert_eq!(
        theirs.len(),
        3,
        "the oracle printed an unexpected transcript: {stdout:?}"
    );

    // Ours: the `make_fun3` closure through the very same three ports.
    let ours = {
        let mut hook = HFunIdentity { module, creator };
        let mut ctx = hook_context(&mut heap, &atoms, &mut hook);
        let range = int_term(&mut ctx, 1_000_000);
        [
            signed_value(phash2(&reg, &mut ctx, &[local])),
            signed_value(phash2(&reg, &mut ctx, &[local, range])),
            signed_value(phash(&reg, &mut ctx, local, range)),
        ]
    };
    assert_eq!(
        theirs, ours,
        "{{phash2(F), phash2(F,1000000), phash(F,1000000)}}: the oracle's local fun vs \
         this crate's make_fun3 closure (oracle image {image:?})"
    );

    // Spot-pin the first field, so a regression reads even though the triplet was
    // compared above.
    assert_eq!(ours[0], H_PHASH2, "phash2/1 of the h closure (OTP 29.1.1)");
}
