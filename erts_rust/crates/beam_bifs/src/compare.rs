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

//! Comparison BIFs for `erlang` (`==`, `/=`, `=:=`, `=/=`, `<`, `>`, `=<`, `>=`)
//! and `erts_internal:cmp_term/2`, plus the two externally-observable legacy
//! **term hashes** `erlang:phash/2` and `erlang:phash2/1,2`.
//!
//! Ground truth (read-only): the `ubif erlang:'>'/2 … '=/'/2` roster in
//! `../../erts/emulator/beam/bif.tab` (lines 295–302), the guard implementations
//! in `../../erts/emulator/beam/erl_bif_guard.c`, and
//! `erts_internal_cmp_term_2` in `../../erts/emulator/beam/bif.c` (~line 5468); for
//! the hashes, `bif.tab:82-84` and the BIF wrappers `phash_2`/`phash2_1`/`phash2_2`
//! (`../../erts/emulator/beam/bif.c:5370-5447`).
//!
//! # Ordering is never re-implemented here
//!
//! Both orders come from `beam_term::compare`: [`cmp`] is the **exact** order
//! (`erts_cmp(a, b, exact=1, eq_only=0)` — the `=:=`/`=/=` order, where `1` and
//! `1.0` are distinct) and [`cmp_arith`] the **arithmetic** order (`exact=0` — the
//! `==`/`/=` and `<` `>` `=<` `>=` order, where `1 == 1.0`). This crate never
//! re-derives term order (crate `CONTEXT.md`, Constraints).
//!
//! # Hashing is never re-implemented here either
//!
//! The two hashes are *term identity* siblings of the ordering above (all three are
//! "is this term the other term" predicates over the frozen layout), so the thin BIF
//! adapters live here too. The algorithms themselves are the lowest-common-ancestor
//! compatibility-pinned ports [`beam_term::hashing::make_hash`] (the legacy ERTS
//! `make_hash`, `erl_term_hashing.c:165-425`) and
//! [`beam_term::hashing::make_hash2`] (`make_hash2_helper`,
//! `erl_term_hashing.c:858-1585`) — both bit-exact against the oracle. This module
//! contributes only the `Range` argument parsing and the reduction the C BIF
//! wrappers apply.
//!
//! # The honest typed gap
//!
//! [`beam_term::hashing`] is **authoritative**: it answers `TermError` for a term whose
//! hash this runtime cannot reproduce **faithfully** — a native record whose definition
//! cannot be resolved, an internal reference (whose C hash mixes an id of the node
//! container this tree's M1 `Ref` thing does not carry), a `make_fun3` local fun (whose
//! inlined descriptor carries no module/`old_uniq`) and a malformed block. The BIF
//! contract here is the crate-wide one — a **typed** `unsupported_bif` capability gap,
//! never a wrong number and never a plain `badarg` — so every `TermError` is mapped to
//! `unsupported_bif` ([`hashing_gap`]).
//!
//! # Pinned against the OTP 29 oracle
//!
//! * `1 == 1.0` ⇒ `true`, `1 =:= 1.0` ⇒ `false`.
//! * `1 < 1.0` ⇒ `false`, `1 =< 1.0` ⇒ `true`.
//! * `a < {a}` ⇒ `true` (atom sorts before tuple in the term order).
//! * `erts_internal:cmp_term(1, 1.0)` ⇒ `-1`, `(1.0, 1)` ⇒ `1`, `(a, {a})` ⇒ `-1`.
//! * `erlang:phash(1, 4)` ⇒ `4`, `erlang:phash2(<<1,2,3>>)` ⇒ `6479071`.

use core::cmp::Ordering;

use beam_common::{BeamError, Result};
use beam_term::compare::{cmp, cmp_arith};
use beam_term::hashing::{make_hash, make_hash2};
use beam_term::{AtomTable, Eterm, TermError};

use crate::arith;
use crate::context::BifContext;
use crate::error::{badarg, unsupported_bif};
use crate::registry::BifRegistry;

/// `A == B` — arithmetic order.
fn eq_arith(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    ctx.boolean(cmp_arith(args[0], args[1], ctx.atoms()) == Ordering::Equal)
}

/// `A /= B` — arithmetic order.
fn ne_arith(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    ctx.boolean(cmp_arith(args[0], args[1], ctx.atoms()) != Ordering::Equal)
}

/// `A =:= B` — exact order.
fn eq_exact(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    ctx.boolean(cmp(args[0], args[1], ctx.atoms()) == Ordering::Equal)
}

/// `A =/= B` — exact order.
fn ne_exact(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    ctx.boolean(cmp(args[0], args[1], ctx.atoms()) != Ordering::Equal)
}

/// `A < B` — arithmetic order.
fn lt(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    ctx.boolean(cmp_arith(args[0], args[1], ctx.atoms()) == Ordering::Less)
}

/// `A > B` — arithmetic order.
fn gt(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    ctx.boolean(cmp_arith(args[0], args[1], ctx.atoms()) == Ordering::Greater)
}

/// `A =< B` — arithmetic order.
fn le(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    ctx.boolean(cmp_arith(args[0], args[1], ctx.atoms()) != Ordering::Greater)
}

/// `A >= B` — arithmetic order.
fn ge(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    ctx.boolean(cmp_arith(args[0], args[1], ctx.atoms()) != Ordering::Less)
}

/// `erts_internal:cmp_term(A, B)` ⇒ `-1 | 0 | 1` in the *exact* order.
fn cmp_term(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let value = match cmp(args[0], args[1], ctx.atoms()) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    };
    ctx.int(value)
}

/// Registers the `erlang` comparison operators, `erts_internal:cmp_term/2` and the
/// three legacy term hashes.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "==", 2, eq_arith);
    reg.register("erlang", "/=", 2, ne_arith);
    reg.register("erlang", "=:=", 2, eq_exact);
    reg.register("erlang", "=/=", 2, ne_exact);
    reg.register("erlang", "<", 2, lt);
    reg.register("erlang", ">", 2, gt);
    reg.register("erlang", "=<", 2, le);
    reg.register("erlang", ">=", 2, ge);
    reg.register("erts_internal", "cmp_term", 2, cmp_term);
    reg.register("erlang", "phash", 2, phash_2);
    reg.register("erlang", "phash2", 1, phash2_1);
    reg.register("erlang", "phash2", 2, phash2_2);
}

// ---------------------------------------------------------------------------
// `erlang:phash/2` and `erlang:phash2/1,2` (the two legacy term hashes)
// ---------------------------------------------------------------------------

/// Maps a [`beam_term::hashing`] failure to the typed `unsupported_bif` capability
/// gap.
///
/// The two hashes are *frozen observable behaviour*, so a term whose hash this
/// runtime cannot reproduce **faithfully** must never yield a number: the registry
/// contract ([`crate::error::unsupported_bif`]) is a typed raise instead. Mapping
/// the whole `TermError` set — not just the classes we can name today — keeps that
/// honest as `beam_term::hashing` grows: a new refusal there becomes a gap here
/// rather than being collapsed into a plain `badarg` by `From<TermError>`.
fn hashing_gap(_: TermError) -> BeamError {
    unsupported_bif()
}

/// `phash_2`'s `Range` argument (`bif.c:5376-5385`) — the `Uint32` modulus, or `0`
/// for the special `2^32` range.
///
/// # Errors
///
/// `badarg` for anything that is not an integer `u` with `1 <= u <= 0xFFFF_FFFF`
/// (`term_to_Uint` followed by the C's `((u >> 16) >> 16) != 0 || !u` check) — a
/// negative or too-large integer, a float, an atom, a list, `Range = 0`. The value
/// `2^32` is *accepted* and mapped to `0` (`term_equals_2pow32`).
fn range_of(term: Eterm) -> Result<u32> {
    let value: u64 = if beam_term::is_small(term) {
        let v = beam_term::signed_value(term);
        if v < 0 {
            return Err(badarg());
        }
        v as u64
    } else if crate::access::is_big(term) {
        let (negative, digits) = beam_codec::term::big_parts(term)?;
        if negative {
            return Err(badarg());
        }
        // The magnitude with any (non-canonical) leading zero limbs dropped.
        let significant = match digits.iter().rposition(|&d| d != 0) {
            Some(top) => &digits[..=top],
            None => &digits[..0],
        };
        match significant {
            [] => 0,
            [single] => *single,
            // More than one significant limb is far beyond 2^32 (and u64).
            _ => return Err(badarg()),
        }
    } else {
        return Err(badarg());
    };

    if value == (1u64 << 32) {
        return Ok(0);
    }
    if (1..=0xFFFF_FFFF).contains(&value) {
        return Ok(value as u32);
    }
    Err(badarg())
}

/// `erlang:phash(Term, Range)` (`bif.tab:82`, `phash_2`).
///
/// # Errors
///
/// `badarg` for an invalid `Range` and the typed `unsupported_bif` gap for a term
/// [`make_hash`] cannot reproduce.
fn phash_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let range = range_of(args[1])?;
    let hash = phash_of(args[0], ctx.atoms())?;
    // `range != 0` ⇒ `1 + (hash % range)`, i.e. `[1..range]`; the `2^32` range
    // (`range == 0`) ⇒ `hash + 1` (one past the top of the `u32` space, promoted
    // exactly as the C `erts_mixed_plus` does).
    let final_hash: u64 = if range != 0 {
        u64::from(hash % range) + 1
    } else {
        u64::from(hash) + 1
    };
    arith::int_from_i64(ctx, final_hash as i64)
}

/// `erlang:phash2(Term)` (`bif.tab:83`, `phash2_1`): `make_hash2(Term)` reduced to
/// the low 27 bits (`bif.c:5407`).
///
/// # Errors
///
/// The typed `unsupported_bif` gap for a term [`make_hash2`] cannot reproduce.
fn phash2_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let hash = phash2_of(args[0], ctx.atoms())?;
    // `make_small(hash & ((1L << 27) - 1))` — always a small integer.
    ctx.int(i64::from(hash & 0x07FF_FFFF))
}

/// `erlang:phash2(Term, Range)` (`bif.tab:84`, `phash2_2`): `make_hash2(Term)`
/// modulo `Range`, in `[0 .. Range-1]` (no `+1`, unlike `phash/2`); the special
/// `2^32` range returns the whole hash (`bif.c:5434-5438`).
///
/// # Errors
///
/// `badarg` for an invalid `Range` and the typed `unsupported_bif` gap for a term
/// [`make_hash2`] cannot reproduce.
fn phash2_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let range = range_of(args[1])?;
    let hash = phash2_of(args[0], ctx.atoms())?;
    let final_hash = if range != 0 { hash % range } else { hash };
    ctx.int(i64::from(final_hash))
}

/// [`make_hash`] behind this crate's typed-gap contract: a term the lowest-common-ancestor
/// port cannot reproduce faithfully ([`hashing_gap`]) is the typed `unsupported_bif`
/// capability gap, never a wrong number.
///
/// `beam_term::hashing` is **authoritative** here — it resolves a native record's
/// definition hash itself and refuses the identity it cannot derive (an internal
/// reference, a `make_fun3` local fun, a malformed block), so no consumer-side
/// pre-check is needed.
fn phash_of(t: Eterm, atoms: &AtomTable) -> Result<u32> {
    make_hash(t, atoms).map_err(hashing_gap)
}

/// [`make_hash2`] behind this crate's typed-gap contract — see [`phash_of`].
fn phash2_of(t: Eterm, atoms: &AtomTable) -> Result<u32> {
    make_hash2(t, atoms).map_err(hashing_gap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access;
    use beam_alloc::Heap;
    use beam_common::{ExceptionClass, ExitReason};
    use beam_process::ProcessId;
    use beam_term::{make_atom, make_small, AtomTable, NIL};

    fn atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        // The ERTS index convention: `false` = 0, `true` = 1 (see `atom.names` and
        // `beam_term::AtomTable::from_atom_names`), then the rest in any order. `x`
        // precedes `y` exactly as in `atom.names`, so a record's value walk order
        // matches the oracle's.
        atoms.seed(["false", "true", "ok", "a", "b", "rec", "r", "x", "y"]);
        atoms
    }

    /// Asserts a result is the typed `unsupported_bif` raise.
    fn assert_gap(result: Result<Eterm>, what: &str) {
        match result {
            Err(BeamError::Raise(raise)) => {
                assert_eq!(raise.class, ExceptionClass::Error, "{what}");
                assert_eq!(raise.reason, ExitReason::atom("unsupported_bif"), "{what}");
            }
            other => panic!("{what}: expected the typed unsupported_bif gap, got {other:?}"),
        }
    }

    /// The boolean value of a `true`/`false` atom term (resolved via `atoms`).
    fn truth(atoms: &AtomTable, t: Eterm) -> bool {
        match atoms.name(beam_term::atom_val(t)) {
            Some("true") => true,
            Some("false") => false,
            other => panic!("expected a boolean atom, got {other:?}"),
        }
    }

    #[test]
    fn arithmetic_equality_distinguishes_int_and_float_the_erlang_way() {
        let mut heap = Heap::new(64).unwrap();
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let one = make_small(1);
        let one_f = ctx.make_float(1.0).unwrap();

        let eq = eq_arith(&mut ctx, &[one, one_f]).unwrap();
        let ex = eq_exact(&mut ctx, &[one, one_f]).unwrap();
        let ne = ne_arith(&mut ctx, &[one, one_f]).unwrap();
        let nx = ne_exact(&mut ctx, &[one, one_f]).unwrap();

        assert!(truth(&atoms, eq), "1 == 1.0 is true");
        assert!(!truth(&atoms, ex), "1 =:= 1.0 is false");
        assert!(!truth(&atoms, ne), "1 /= 1.0 is false");
        assert!(truth(&atoms, nx), "1 =/= 1.0 is true");
    }

    #[test]
    fn ordering_operators_use_the_arithmetic_order() {
        let mut heap = Heap::new(64).unwrap();
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let one = make_small(1);
        let one_f = ctx.make_float(1.0).unwrap();
        let two = make_small(2);

        // `1 < 1.0` is false, `1 =< 1.0` is true (arithmetic order).
        assert!(!truth(&atoms, lt(&mut ctx, &[one, one_f]).unwrap()));
        assert!(truth(&atoms, le(&mut ctx, &[one, one_f]).unwrap()));
        assert!(truth(&atoms, ge(&mut ctx, &[one, one_f]).unwrap()));
        assert!(!truth(&atoms, gt(&mut ctx, &[one, one_f]).unwrap()));

        assert!(truth(&atoms, lt(&mut ctx, &[one, two]).unwrap()));
        assert!(truth(&atoms, gt(&mut ctx, &[two, one]).unwrap()));
        assert!(!truth(&atoms, lt(&mut ctx, &[two, one]).unwrap()));
    }

    #[test]
    fn cross_type_order_is_the_term_order() {
        let mut heap = Heap::new(64).unwrap();
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let a = ctx.atom("a").unwrap();
        let tup = ctx.tuple(&[a]).unwrap();

        // atom < tuple in the term order.
        assert!(truth(&atoms, lt(&mut ctx, &[a, tup]).unwrap()));
        assert!(truth(&atoms, gt(&mut ctx, &[tup, a]).unwrap()));
        // `[]` sorts after a tuple.
        assert!(truth(&atoms, lt(&mut ctx, &[tup, NIL]).unwrap()));
    }

    #[test]
    fn cmp_term_returns_minus_one_zero_one() {
        let mut heap = Heap::new(64).unwrap();
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let one = make_small(1);
        let one_f = ctx.make_float(1.0).unwrap();
        let a = ctx.atom("a").unwrap();
        let tup = ctx.tuple(&[a]).unwrap();

        // Exact order: `1` and `1.0` are distinct; small sorts before float.
        assert_eq!(cmp_term(&mut ctx, &[one, one_f]).unwrap(), make_small(-1));
        assert_eq!(cmp_term(&mut ctx, &[one_f, one]).unwrap(), make_small(1));
        assert_eq!(cmp_term(&mut ctx, &[one, one]).unwrap(), make_small(0));
        assert_eq!(cmp_term(&mut ctx, &[a, tup]).unwrap(), make_small(-1));
        assert_eq!(cmp_term(&mut ctx, &[tup, tup]).unwrap(), make_small(0));
    }

    #[test]
    fn register_installs_the_quoted_operator_names() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        for name in ["==", "/=", "=:=", "=/=", "<", ">", "=<", ">="] {
            assert!(reg.contains("erlang", name, 2), "missing erlang:{name}/2");
        }
        assert!(reg.contains("erts_internal", "cmp_term", 2));
        // The three legacy term hashes this module also hosts.
        assert!(reg.contains("erlang", "phash", 2));
        assert!(reg.contains("erlang", "phash2", 1));
        assert!(reg.contains("erlang", "phash2", 2));
        assert_eq!(reg.len(), 12);
    }

    // -----------------------------------------------------------------------
    // The legacy term hashes (adapters over `beam_term::hashing`)
    // -----------------------------------------------------------------------

    /// The `Range` reduction the C BIF wrappers apply, over the shared
    /// `beam_term::hashing::make_hash` / `make_hash2` ports.
    ///
    /// Every value below is the system oracle's (`erl -noshell -eval '…'`) for the
    /// equivalent Erlang term — `tests/phash.rs` holds the broad pinned corpus, this
    /// pins the *reduction* (`% Range` and the `2^32` special case) on one term of each
    /// hash:
    ///
    /// * `make_hash({1,2,3})` = `4267354533` — `phash(T, R)` = `1 + hash % R`, and the
    ///   `R = 2^32` form (mapped to `0`) = `hash + 1`; both give `4267354534`.
    /// * `make_hash2({1,2,3})` = `2847307263` — `phash2(T, R)` = `hash % R` (no `+1`),
    ///   and the `R = 2^32` form returns the whole hash; both give `2847307263`.
    #[test]
    fn the_legacy_hashes_reduce_exactly_as_the_c_wrappers_do() {
        let mut heap = Heap::new(64).unwrap();
        let table = atoms();
        let mut ctx = BifContext::new(&mut heap, &table, ProcessId::new(1, 0));
        let tup = ctx
            .tuple(&[make_small(1), make_small(2), make_small(3)])
            .unwrap();
        let million = make_small(1_000_000);
        let two_32 = make_small(1i64 << 32);
        let two_32_minus_1 = make_small((1i64 << 32) - 1);

        // `erlang:phash(1, 4)` = `1 + (make_hash(1) % 4)` = 4.
        assert_eq!(
            phash_2(&mut ctx, &[make_small(1), make_small(4)]).unwrap(),
            make_small(4)
        );
        // The `Range` modulus and the `2^32` special case (`hash + 1`).
        assert_eq!(
            phash_2(&mut ctx, &[tup, million]).unwrap(),
            make_small(426_735_4533 % 1_000_000 + 1)
        );
        assert_eq!(
            phash_2(&mut ctx, &[tup, two_32_minus_1]).unwrap(),
            make_small(4_267_354_534)
        );
        assert_eq!(
            phash_2(&mut ctx, &[tup, two_32]).unwrap(),
            make_small(4_267_354_534)
        );

        // `erlang:phash2([])` = `make_hash2([]) & ((1 << 27) - 1)` = 113427502.
        assert_eq!(phash2_1(&mut ctx, &[NIL]).unwrap(), make_small(113_427_502));
        // `erlang:phash2({1,2,3})` = 28734975 (the low 27 bits of the full hash).
        assert_eq!(phash2_1(&mut ctx, &[tup]).unwrap(), make_small(28_734_975));
        // The `Range` modulus (no `+1`, unlike `phash/2`) and the `2^32` special case
        // (the whole hash).
        assert_eq!(
            phash2_2(&mut ctx, &[tup, million]).unwrap(),
            make_small(307_263)
        );
        assert_eq!(
            phash2_2(&mut ctx, &[tup, two_32_minus_1]).unwrap(),
            make_small(2_847_307_263)
        );
        assert_eq!(
            phash2_2(&mut ctx, &[tup, two_32]).unwrap(),
            make_small(2_847_307_263)
        );
    }

    // -----------------------------------------------------------------------
    // The native-record and internal-reference classes
    // -----------------------------------------------------------------------

    /// `records:create(rec, r, [{a,1}], #{is_exported => true})` — built the way
    /// `crate::records::create/4` does, i.e. through
    /// [`access::native_record_write`], whose cached definition hash is the
    /// placeholder `make_small(0)`.
    ///
    /// A **one-field** record is the shape whose oracle value is stable run to run
    /// (see the test doc comment below); a record with several keys is not.
    fn sample_record(heap: &mut Heap, table: &AtomTable) -> Eterm {
        let module = table.lookup("rec").expect("seeded");
        let name = table.lookup("r").expect("seeded");
        let a = make_atom(table.lookup("a").expect("seeded"));
        access::native_record_write(heap, module, name, true, &[(a, make_small(1))])
            .expect("the record block fits")
    }

    /// A native record now hashes to the **oracle's** values instead of being a typed
    /// gap, at the top level and nested inside a tuple / a list / a map.
    ///
    /// The expectations are re-derived from the system oracle (OTP 29.1.1 / erts-17.1):
    ///
    /// ```text
    /// $ erl -noshell -eval '
    ///     R2 = records:create(rec, r, [{a,1}], #{is_exported => true}),
    ///     io:format("~p ~p ~p | ~p ~p ~p | ~p ~p ~p | ~p ~p ~p~n",
    ///      [erlang:phash2({1,R2}), erlang:phash2({1,R2},1000000), erlang:phash({1,R2},1000000),
    ///       erlang:phash2([R2,1]), erlang:phash2([R2,1],1000000), erlang:phash([R2,1],1000000),
    ///       erlang:phash2(#{a => R2}), erlang:phash2(#{a => R2},1000000), erlang:phash(#{a => R2},1000000),
    ///       erlang:phash2(R2), erlang:phash2(R2,1000000), erlang:phash(R2,1000000)]), halt().'
    /// 105608320 874240 964745 | 46225311 967135 812886 | 7457474 553858 994470 | 33491412 709140 567678
    /// ```
    ///
    /// (Identical over repeated runs.)
    ///
    /// **Why the one-field record.** The oracle's record-definition hash mixes the
    /// *atom indices* of the field keys, and `a`/`b` are not in `erts/emulator/beam/atom.names`,
    /// so the 2-field `[{a,1},{b,2}]` form is **not reproducible run to run** — the
    /// oracle itself answers `15179545` or `118035477` for `phash2/1` depending on the
    /// run (the Rust side reproduces the `118035477` family exactly). A one-field
    /// record with `is_exported => true` is stable, which is what is pinned here.
    ///
    /// The record is built with the **placeholder** definition hash, so these values
    /// prove the hash is taken from the definition block that
    /// `beam_term::hashing` resolves itself (the same value the C caches at
    /// creation time) — not from the stored word.
    #[test]
    fn native_records_hash_to_the_oracle_values() {
        let mut heap = Heap::new(512).unwrap();
        let table = atoms();
        let a_term = make_atom(table.lookup("a").expect("seeded"));
        let rec = sample_record(&mut heap, &table);

        let mut ctx = BifContext::new(&mut heap, &table, ProcessId::new(1, 0));
        let one = make_small(1);
        let million = make_small(1_000_000);

        // Top level.
        assert_eq!(phash2_1(&mut ctx, &[rec]).unwrap(), make_small(33_491_412));
        assert_eq!(
            phash2_2(&mut ctx, &[rec, million]).unwrap(),
            make_small(709_140)
        );
        assert_eq!(
            phash_2(&mut ctx, &[rec, million]).unwrap(),
            make_small(567_678)
        );

        // ... and nested: both hashes visit the record wherever it sits.
        let in_tuple = ctx.tuple(&[one, rec]).unwrap();
        let in_list = ctx.list(&[rec, one]).unwrap();
        let in_map = access::make_flatmap(ctx.heap(), &[(a_term, rec)]).unwrap();

        assert_eq!(
            phash2_1(&mut ctx, &[in_tuple]).unwrap(),
            make_small(105_608_320)
        );
        assert_eq!(
            phash2_2(&mut ctx, &[in_tuple, million]).unwrap(),
            make_small(874_240)
        );
        assert_eq!(
            phash_2(&mut ctx, &[in_tuple, million]).unwrap(),
            make_small(964_745)
        );

        assert_eq!(
            phash2_1(&mut ctx, &[in_list]).unwrap(),
            make_small(46_225_311)
        );
        assert_eq!(
            phash2_2(&mut ctx, &[in_list, million]).unwrap(),
            make_small(967_135)
        );
        assert_eq!(
            phash_2(&mut ctx, &[in_list, million]).unwrap(),
            make_small(812_886)
        );

        assert_eq!(
            phash2_1(&mut ctx, &[in_map]).unwrap(),
            make_small(7_457_474)
        );
        assert_eq!(
            phash2_2(&mut ctx, &[in_map, million]).unwrap(),
            make_small(553_858)
        );
        assert_eq!(
            phash_2(&mut ctx, &[in_map, million]).unwrap(),
            make_small(994_470)
        );
    }

    /// An **internal reference** is a class `beam_term::hashing` refuses with
    /// `TermError::RefIdentityUnavailable` (its C hash mixes an id of the node
    /// container that this tree's M1 `Ref` thing does not carry), so `erlang:phash/2`
    /// and `erlang:phash2/1,2` must answer the typed `unsupported_bif` capability gap
    /// at **every** nesting depth the two hashes reach — never a number.
    ///
    /// The oracle's `erlang:phash2(make_ref())` is environment-dependent (the C mixes
    /// the running node's container id), so no value is pinned; the contract asserted
    /// here is the typed refusal.
    #[test]
    fn internal_references_stay_the_typed_unsupported_bif_gap() {
        let mut heap = Heap::new(512).unwrap();
        let table = atoms();
        let a_term = make_atom(table.lookup("a").expect("seeded"));
        let r = access::make_ref_thing(&mut heap, 7).unwrap();

        let mut ctx = BifContext::new(&mut heap, &table, ProcessId::new(1, 0));
        let one = make_small(1);
        let range = make_small(1_000_000);

        let in_tuple = ctx.tuple(&[one, r]).unwrap();
        let in_list = ctx.list(&[r, one]).unwrap();
        let in_map = access::make_flatmap(ctx.heap(), &[(a_term, r)]).unwrap();
        // A record whose *field* holds the reference: the record's value walk reaches
        // it, so the record too is a gap.
        let rec_with_ref = access::native_record_write(
            ctx.heap(),
            table.lookup("rec").expect("seeded"),
            table.lookup("r").expect("seeded"),
            false,
            &[(a_term, r)],
        )
        .unwrap();

        for (t, what) in [
            (r, "reference"),
            (in_tuple, "{1, reference}"),
            (in_list, "[reference, 1]"),
            (in_map, "#{a => reference}"),
            (rec_with_ref, "a record with a reference field"),
        ] {
            assert_gap(phash2_1(&mut ctx, &[t]), &format!("phash2({what})"));
            assert_gap(
                phash2_2(&mut ctx, &[t, range]),
                &format!("phash2({what}, range)"),
            );
            assert_gap(
                phash_2(&mut ctx, &[t, range]),
                &format!("phash({what}, range)"),
            );
        }

        // A control: the same containers *without* a reference still hash.
        let clean = ctx.tuple(&[one, a_term]).unwrap();
        assert!(phash2_1(&mut ctx, &[clean]).is_ok());
    }
}
