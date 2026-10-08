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
//! and `erts_internal:cmp_term/2`.
//!
//! Ground truth (read-only): the `ubif erlang:'>'/2 … '=/'/2` roster in
//! `../../erts/emulator/beam/bif.tab` (lines 295–302), the guard implementations
//! in `../../erts/emulator/beam/erl_bif_guard.c`, and
//! `erts_internal_cmp_term_2` in `../../erts/emulator/beam/bif.c` (~line 5468).
//!
//! # Ordering is never re-implemented here
//!
//! Both orders come from `beam_term::compare`: [`cmp`] is the **exact** order
//! (`erts_cmp(a, b, exact=1, eq_only=0)` — the `=:=`/`=/=` order, where `1` and
//! `1.0` are distinct) and [`cmp_arith`] the **arithmetic** order (`exact=0` — the
//! `==`/`/=` and `<` `>` `=<` `>=` order, where `1 == 1.0`). This crate never
//! re-derives term order (crate `CONTEXT.md`, Constraints).
//!
//! # Pinned against the OTP 29 oracle
//!
//! * `1 == 1.0` ⇒ `true`, `1 =:= 1.0` ⇒ `false`.
//! * `1 < 1.0` ⇒ `false`, `1 =< 1.0` ⇒ `true`.
//! * `a < {a}` ⇒ `true` (atom sorts before tuple in the term order).
//! * `erts_internal:cmp_term(1, 1.0)` ⇒ `-1`, `(1.0, 1)` ⇒ `1`, `(a, {a})` ⇒ `-1`.

use core::cmp::Ordering;

use beam_common::Result;
use beam_term::compare::{cmp, cmp_arith};
use beam_term::Eterm;

use crate::context::BifContext;
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

/// Registers the `erlang` comparison operators and `erts_internal:cmp_term/2`.
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable, NIL};

    fn atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok", "a", "b"]);
        atoms
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
        assert_eq!(reg.len(), 9);
    }
}
