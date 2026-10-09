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
//! [`beam_term::hashing`] answers `TermError` for a term it cannot hash faithfully
//! (a native record whose cached definition hash is not a `Uint`, a `make_fun3` local
//! fun without a module/`old_uniq`, a malformed block) and for a class whose
//! *transcription* this runtime does not carry (an internal reference). The BIF
//! contract here is the crate-wide one — a **typed** `unsupported_bif` capability gap,
//! never a wrong number and never a plain `badarg` — so every `TermError` is mapped to
//! `unsupported_bif` ([`hashing_gap`]).
//!
//! Two classes *succeed* in the LCA hashes yet must still be a gap here, because the
//! value they would produce is not the oracle's: a **native record** and an **internal
//! reference**. Both are found *before* delegating by [`hash_gap`], which walks the
//! nested positions the hashes visit (the transcription this module replaced refused
//! them at the same positions).
//!
//! # Pinned against the OTP 29 oracle
//!
//! * `1 == 1.0` ⇒ `true`, `1 =:= 1.0` ⇒ `false`.
//! * `1 < 1.0` ⇒ `false`, `1 =< 1.0` ⇒ `true`.
//! * `a < {a}` ⇒ `true` (atom sorts before tuple in the term order).
//! * `erts_internal:cmp_term(1, 1.0)` ⇒ `-1`, `(1.0, 1)` ⇒ `1`, `(a, {a})` ⇒ `-1`.
//! * `erlang:phash(1, 4)` ⇒ `4`, `erlang:phash2(<<1,2,3>>)` ⇒ `6479071`.

use core::cmp::Ordering;

use beam_codec::term::{fun_env, fun_kind_term, FunKind};
use beam_common::{BeamError, Result};
use beam_term::compare::{cmp, cmp_arith};
use beam_term::hashing::{make_hash, make_hash2};
use beam_term::{AtomTable, Eterm, TermError};

use crate::access;
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

/// `true` when `t`, or any term reachable from it, is a class whose `make_hash` /
/// `make_hash2` value this runtime cannot reproduce faithfully — so the BIF must
/// answer the typed `unsupported_bif` gap rather than a number.
///
/// Two classes qualify, and neither is reachable through [`hashing_gap`] (both
/// *succeed* in the LCA hashes), so they are checked up front:
///
/// * a **native record**. Both hashes mix in the record definition's **cached** hash
///   word (`erl_term_hashing.c:369` and `:1093`, which the C *requires* to be the
///   definition's `make_hash2`) — and `records:create/4` stores the placeholder
///   `make_small(0)` there, because [`beam_term::record::write_record`] takes that
///   word as an explicit parameter (the value is a word of the very block being
///   written, so it cannot be known before the block exists; see the
///   [`beam_term::record`] module docs). The placeholder is a *small integer*, so
///   `make_hash2` accepts it and would silently return a non-oracle value. A record
///   is therefore a gap whatever its cache holds — the same contract the transcription
///   this module replaced had.
/// * an **internal reference**: both hashes mix in `internal_ref_numbers(ref)[0]`
///   (`erl_term_hashing.c:289` and `:1456`) — the first id of the reference's *node
///   container*, which this runtime's M1 on-heap `Ref` thing does not carry (it holds
///   a single process-local id, see [`crate::access::make_ref_thing`]).
///
/// The walk visits exactly the nested positions the two hashes visit — a list's head
/// and tail, a tuple's elements, a map's keys and values, an imported fun's
/// environment — so a gap class is detected *wherever* it would have been mixed in,
/// never only at the top level. It never dereferences blind: every step asks
/// [`crate::access`] first, so a non-term word, a `NIL` or a `BinRef` is a leaf.
fn hash_gap(t: Eterm, atoms: &AtomTable) -> bool {
    let mut work = vec![t];
    while let Some(t) = work.pop() {
        if access::is_record(t) || access::is_internal_ref(t) {
            return true;
        }
        if access::is_list(t) {
            work.push(access::car(t));
            work.push(access::cdr(t));
        } else if access::is_tuple(t) {
            for i in 0..access::tuple_size(t) {
                work.push(access::tuple_element(t, i));
            }
        } else if access::is_map(t) {
            match beam_term::map::map_pairs(t, atoms) {
                Ok(pairs) => work.extend(pairs.into_iter().flat_map(|(k, v)| [k, v])),
                // A map the LCA reader rejects is one the hashes reject too.
                Err(_) => return true,
            }
        } else if access::is_function(t) {
            match fun_kind_term(t) {
                // A `make_fun3` fun carries no module/`old_uniq` to mix in; the hashes
                // refuse it themselves (it is a `TermError` gap as well).
                Ok(FunKind::Local) | Err(_) => return true,
                Ok(FunKind::Imported) => match fun_env(t) {
                    Ok(env) => work.extend(env.iter().copied()),
                    Err(_) => return true,
                },
                Ok(FunKind::External) => {}
            }
        }
    }
    false
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

/// [`make_hash`] behind this crate's typed-gap contract: an unfaithful class
/// ([`hash_gap`]) or an LCA refusal ([`hashing_gap`]) is `unsupported_bif`.
fn phash_of(t: Eterm, atoms: &AtomTable) -> Result<u32> {
    if hash_gap(t, atoms) {
        return Err(unsupported_bif());
    }
    make_hash(t, atoms).map_err(hashing_gap)
}

/// [`make_hash2`] behind this crate's typed-gap contract — see [`phash_of`].
fn phash2_of(t: Eterm, atoms: &AtomTable) -> Result<u32> {
    if hash_gap(t, atoms) {
        return Err(unsupported_bif());
    }
    make_hash2(t, atoms).map_err(hashing_gap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::{ExceptionClass, ExitReason};
    use beam_process::ProcessId;
    use beam_term::{make_atom, make_small, AtomTable, NIL};

    fn atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok", "a", "b"]);
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

    /// [`hash_gap`] finds an unfaithful class at **every** nesting depth the hashes
    /// reach, not only at the top level — the contract the deleted transcription had
    /// (its own walk refused these classes wherever it met them).
    ///
    /// Both classes here are ones the LCA hashes *accept*, so this is the guard that
    /// keeps them a typed gap: a native record (whose cached definition hash this
    /// runtime stores as the placeholder `make_small(0)`) and an internal reference
    /// (whose id words this runtime's M1 `Ref` thing does not carry).
    #[test]
    fn the_hash_gap_is_found_at_every_nesting_depth() {
        let mut heap = Heap::new(256).unwrap();
        let table = atoms();
        let a = table.lookup("a").unwrap();

        // The way `records:create/4` builds one: `native_record_write` stores the
        // placeholder `make_small(0)` as the definition's cached hash.
        let rec = access::native_record_write(
            &mut heap,
            a,
            a,
            false,
            &[(make_atom(a), make_small(2))],
        )
        .unwrap();
        let r = access::make_ref_thing(&mut heap, 7).unwrap();
        let a_term = make_atom(a);

        let mut ctx = BifContext::new(&mut heap, &table, ProcessId::new(1, 0));
        let range = make_small(1_000_000);
        let rec_in_tuple = ctx.tuple(&[make_small(1), rec]).unwrap();
        let rec_in_list = ctx.list(&[rec, make_small(1)]).unwrap();
        let rec_in_map = access::make_flatmap(ctx.heap(), &[(a_term, rec)]).unwrap();
        let ref_in_tuple = ctx.tuple(&[r]).unwrap();

        // A record is a gap at the top level and nested, for both hashes.
        assert_gap(phash2_1(&mut ctx, &[rec]), "phash2(record)");
        assert_gap(phash_2(&mut ctx, &[rec, range]), "phash(record, range)");
        assert_gap(
            phash2_1(&mut ctx, &[rec_in_tuple]),
            "phash2({1, record})",
        );
        assert_gap(phash2_1(&mut ctx, &[rec_in_list]), "phash2([record, 1])");
        assert_gap(
            phash2_1(&mut ctx, &[rec_in_map]),
            "phash2(#{a => record})",
        );
        // ... and so is an internal reference.
        assert_gap(phash2_1(&mut ctx, &[r]), "phash2(reference)");
        assert_gap(
            phash2_1(&mut ctx, &[ref_in_tuple]),
            "phash2({reference})",
        );
        assert_gap(phash_2(&mut ctx, &[r, range]), "phash(reference, range)");

        // A control: the same containers *without* a gap class still hash.
        let clean = ctx.tuple(&[make_small(1), a_term]).unwrap();
        assert!(phash2_1(&mut ctx, &[clean]).is_ok());
    }
}
