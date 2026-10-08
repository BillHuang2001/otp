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

//! The `erlang` **strict boolean operator** BIFs: `'not'/1`, `'and'/2`,
//! `'or'/2`, `'xor'/2`.
//!
//! These are the operators `bif.tab` lists as `ubif erlang:'and'/2` …
//! (`../../erts/emulator/beam/bif.tab:290-293`). The short-circuiting
//! `andalso`/`orelse` are deliberately **absent**: they are not BIFs — the
//! compiler lowers them to jumps — which is exactly why these four are "strict":
//! the caller has already evaluated both operands, and the BIF decides the result
//! from the two terms alone.
//!
//! Ground truth (read-only): `../../erts/emulator/beam/erl_bif_op.c`
//! (`not_1` line 84, `and_2` line 45, `or_2` line 58, `xor_2` line 71). Each
//! operator matches the `am_true`/`am_false` combinations in turn and otherwise
//! raises `badarg` — so a non-boolean operand (another atom, an integer, a list, a
//! tuple, a float, …) is `error:badarg`, never `false`.
//!
//! Oracle-verified with `escript` (OTP 29 / erts-17.1):
//!
//! | expression | result |
//! |---|---|
//! | `erlang:'not'(true)` | `false` |
//! | `erlang:'not'(false)` | `true` |
//! | `erlang:'not'(1)` / `erlang:'not'([])` | `error:badarg` |
//! | `erlang:'and'(true,false)` / `erlang:'and'(false,false)` | `false` |
//! | `erlang:'or'(false,true)` / `erlang:'or'(true,false)` | `true` |
//! | `erlang:'or'(false,false)` | `false` |
//! | `erlang:'xor'(true,true)` / `erlang:'xor'(false,false)` | `false` |
//! | `erlang:'xor'(true,false)` / `erlang:'xor'(false,true)` | `true` |
//! | `erlang:'and'(true,1)` / `erlang:'or'(1,true)` / `erlang:'xor'(true,atom)` | `error:badarg` |

use beam_common::Result;
use beam_term::{atom_val, Eterm};

use crate::access;
use crate::context::BifContext;
use crate::error;
use crate::registry::BifRegistry;

/// The boolean value of `t` when it is the atom `true` or `false`, else `None`.
///
/// Exactly the C `BIF_ARG_1 == am_true` / `am_false` test: any other atom (or a
/// name missing from the atom table) is not a boolean. `ctx.atoms()` is the
/// runtime's global table, so the well-known atoms resolve by name.
fn boolean_arg(ctx: &BifContext<'_>, t: Eterm) -> Option<bool> {
    if !access::is_atom(t) {
        return None;
    }
    match ctx.atoms().name(atom_val(t)) {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    }
}

/// `erlang:'not'/1` — `not true = false`, `not false = true`, else `badarg`.
fn not(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let value = boolean_arg(ctx, args[0]).ok_or_else(error::badarg)?;
    ctx.boolean(!value)
}

/// `erlang:'and'/2` — both operands must be booleans, else `badarg`.
fn and(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let left = boolean_arg(ctx, args[0]).ok_or_else(error::badarg)?;
    let right = boolean_arg(ctx, args[1]).ok_or_else(error::badarg)?;
    ctx.boolean(left && right)
}

/// `erlang:'or'/2` — both operands must be booleans, else `badarg`.
fn or(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let left = boolean_arg(ctx, args[0]).ok_or_else(error::badarg)?;
    let right = boolean_arg(ctx, args[1]).ok_or_else(error::badarg)?;
    ctx.boolean(left || right)
}

/// `erlang:'xor'/2` — both operands must be booleans, else `badarg`.
fn xor(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let left = boolean_arg(ctx, args[0]).ok_or_else(error::badarg)?;
    let right = boolean_arg(ctx, args[1]).ok_or_else(error::badarg)?;
    ctx.boolean(left != right)
}

/// Registers the `erlang` strict boolean operators (quoted operator names, exactly
/// as `bif.tab` spells them).
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "not", 1, not);
    reg.register("erlang", "and", 2, and);
    reg.register("erlang", "or", 2, or);
    reg.register("erlang", "xor", 2, xor);
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{make_atom, make_small, AtomTable, NIL};

    /// A seeded atom table plus the `true`/`false` terms built from it.
    fn seeded() -> (AtomTable, Eterm, Eterm) {
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok", "a"]);
        let t = make_atom(atoms.lookup("true").expect("seeded"));
        let f = make_atom(atoms.lookup("false").expect("seeded"));
        (atoms, t, f)
    }

    /// Runs `f` over a fresh heap/atom table.
    fn run(atoms: &AtomTable, f: crate::registry::BifFn, args: &[Eterm]) -> Result<Eterm> {
        let mut heap = Heap::new(64).expect("test heap");
        let mut ctx = BifContext::new(&mut heap, atoms, ProcessId::new(1, 0));
        f(&mut ctx, args)
    }

    /// The boolean value of a `true`/`false` atom term, resolved via `atoms`.
    fn truth(atoms: &AtomTable, t: Eterm) -> bool {
        match atoms.name(atom_val(t)) {
            Some("true") => true,
            Some("false") => false,
            other => panic!("expected a boolean atom, got {other:?}"),
        }
    }

    #[test]
    fn not_matches_the_oracle() {
        let (atoms, t, f) = seeded();
        assert!(!truth(&atoms, run(&atoms, not, &[t]).expect("not true")));
        assert!(truth(&atoms, run(&atoms, not, &[f]).expect("not false")));
    }

    #[test]
    fn and_or_xor_truth_tables_match_the_oracle() {
        let (atoms, t, f) = seeded();
        // (op, left, right, expected, name)
        let cases: [(crate::registry::BifFn, Eterm, Eterm, bool, &str); 12] = [
            (and, t, t, true, "true and true"),
            (and, t, f, false, "true and false"),
            (and, f, t, false, "false and true"),
            (and, f, f, false, "false and false"),
            (or, t, t, true, "true or true"),
            (or, t, f, true, "true or false"),
            (or, f, t, true, "false or true"),
            (or, f, f, false, "false or false"),
            (xor, t, t, false, "true xor true"),
            (xor, t, f, true, "true xor false"),
            (xor, f, t, true, "false xor true"),
            (xor, f, f, false, "false xor false"),
        ];
        for (op, left, right, expected, name) in cases {
            let got = run(&atoms, op, &[left, right]).expect(name);
            assert_eq!(truth(&atoms, got), expected, "{name}");
        }
    }

    #[test]
    fn a_non_boolean_operand_is_badarg() {
        let (atoms, t, f) = seeded();
        let ok = make_atom(atoms.lookup("ok").expect("seeded"));
        let a = make_atom(atoms.lookup("a").expect("seeded"));
        let int = make_small(1);

        // `tuple`/`float` point into this heap, so it must outlive the calls below.
        let mut heap = Heap::new(64).expect("test heap");
        let tuple = heap.tuple(&[ok]).expect("the tuple fits");
        let float = crate::access::make_float(&mut heap, 1.0).expect("the float fits");

        for bad in [ok, a, int, NIL, tuple, float] {
            assert!(
                matches!(run(&atoms, not, &[bad]), Err(BeamError::BadArg)),
                "not({bad:?}) must be badarg"
            );
            for good in [t, f] {
                assert!(matches!(
                    run(&atoms, and, &[good, bad]),
                    Err(BeamError::BadArg)
                ));
                assert!(matches!(
                    run(&atoms, and, &[bad, good]),
                    Err(BeamError::BadArg)
                ));
                assert!(matches!(
                    run(&atoms, or, &[good, bad]),
                    Err(BeamError::BadArg)
                ));
                assert!(matches!(
                    run(&atoms, xor, &[bad, good]),
                    Err(BeamError::BadArg)
                ));
            }
        }
    }

    #[test]
    fn register_installs_the_four_operators() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        for (name, arity) in [("not", 1), ("and", 2), ("or", 2), ("xor", 2)] {
            assert!(
                reg.contains("erlang", name, arity),
                "missing erlang:{name}/{arity}"
            );
        }
        assert_eq!(reg.len(), 4, "the strict operators are exactly four BIFs");
        // `andalso`/`orelse` are compiler-lowered, not BIFs.
        assert!(!reg.contains("erlang", "andalso", 2));
        assert!(!reg.contains("erlang", "orelse", 2));
    }
}
