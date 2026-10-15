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

//! `Test`/`is_*` evaluation (`impl Machine`).

use beam_common::Result;
use beam_loader::{Module, Operand, TestKind};
use beam_term::{
    cmp, cmp_arith, is_atom, is_internal_pid, is_internal_port, is_nil, is_not_nil_list,
    make_atom, Eterm, MAX_ARITYVAL,
};

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::Machine;
use crate::operand::read_operand;
use crate::predicates;

impl Machine {
    /// Evaluate one `is_*`/`test_arity` test; `true` means the test *succeeds*.
    ///
    /// `#[inline]`-marked: this is one of the interpreter's hottest out-of-line
    /// helpers and returns a `Result<bool, BeamError>` whose `Err` variant is a
    /// 72-byte `BeamError`; without the hint the `Ok` result round-trips through
    /// memory on every `test` instruction. The hint lets the optimizer keep the
    /// common arms in registers and see through the `?` in the dispatch arm.
    #[inline]
    pub(super) fn eval_test(
        &self,
        module: &Module,
        kind: TestKind,
        args: &[Operand],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<bool> {
        match kind {
            TestKind::IsLt => {
                let (a, b) = self.test_pair(args, ctx)?;
                Ok(cmp_arith(a, b, ctx.atoms(module)).is_lt())
            }
            TestKind::IsGe => {
                let (a, b) = self.test_pair(args, ctx)?;
                Ok(cmp_arith(a, b, ctx.atoms(module)).is_ge())
            }
            TestKind::IsEq => {
                let (a, b) = self.test_pair(args, ctx)?;
                Ok(cmp_arith(a, b, ctx.atoms(module)).is_eq())
            }
            TestKind::IsNe => {
                let (a, b) = self.test_pair(args, ctx)?;
                Ok(!cmp_arith(a, b, ctx.atoms(module)).is_eq())
            }
            TestKind::IsEqExact => {
                let (a, b) = self.test_pair(args, ctx)?;
                Ok(cmp(a, b, ctx.atoms(module)).is_eq())
            }
            TestKind::IsNeExact => {
                let (a, b) = self.test_pair(args, ctx)?;
                Ok(!cmp(a, b, ctx.atoms(module)).is_eq())
            }
            TestKind::IsInteger => Ok(predicates::is_integer(self.test_arg(args, 0, ctx)?)),
            TestKind::IsFloat => Ok(predicates::is_float(self.test_arg(args, 0, ctx)?)),
            TestKind::IsNumber => Ok(predicates::is_number(self.test_arg(args, 0, ctx)?)),
            TestKind::IsAtom => Ok(is_atom(self.test_arg(args, 0, ctx)?)),
            TestKind::IsPid => {
                let v = self.test_arg(args, 0, ctx)?;
                Ok(is_internal_pid(v) || predicates::is_external_pid(v))
            }
            TestKind::IsReference => {
                let v = self.test_arg(args, 0, ctx)?;
                Ok(predicates::is_internal_ref(v) || predicates::is_external_ref(v))
            }
            TestKind::IsPort => {
                let v = self.test_arg(args, 0, ctx)?;
                Ok(is_internal_port(v) || predicates::is_external_port(v))
            }
            TestKind::IsNil => Ok(is_nil(self.test_arg(args, 0, ctx)?)),
            TestKind::IsBinary => Ok(predicates::is_binary(self.test_arg(args, 0, ctx)?)),
            TestKind::IsBitstr => Ok(predicates::is_bitstring(self.test_arg(args, 0, ctx)?)),
            TestKind::IsList => {
                // `is_list` is true for `[]` *or* a cons cell.
                let v = self.test_arg(args, 0, ctx)?;
                Ok(is_nil(v) || is_not_nil_list(v))
            }
            TestKind::IsNonemptyList => Ok(is_not_nil_list(self.test_arg(args, 0, ctx)?)),
            TestKind::IsTuple => Ok(predicates::is_tuple(self.test_arg(args, 0, ctx)?)),
            TestKind::IsMap => Ok(predicates::is_map(self.test_arg(args, 0, ctx)?)),
            TestKind::IsBoolean => {
                // `is_boolean Fail Src` (`emu/instrs.tab:933`): `Src` is the atom
                // `true` or `false`. Both are ordinary atoms, so compare against the
                // host's atom namespace; a name absent from it can never be the
                // value, so a missing name is `false` (not an error).
                let v = self.test_arg(args, 0, ctx)?;
                if !is_atom(v) {
                    return Ok(false);
                }
                let atoms = ctx.atoms(module);
                let is_true = atoms
                    .lookup("true")
                    .map(|a| v == make_atom(a))
                    .unwrap_or(false);
                let is_false = atoms
                    .lookup("false")
                    .map(|a| v == make_atom(a))
                    .unwrap_or(false);
                Ok(is_true || is_false)
            }
            // `is_function Fail Fun` and `is_function2 Fail Fun Arity` — reuse the
            // `terms` fun readers (`is_any_fun` / the fun-header arity field).
            TestKind::IsFunction => Ok(crate::terms::is_fun(self.test_arg(args, 0, ctx)?)),
            TestKind::IsFunction2 => self.eval_is_function2(args, ctx),
            TestKind::IsTaggedTuple => self.eval_is_tagged_tuple(args, ctx),
            TestKind::TestArity => self.eval_test_arity(args, ctx),
        }
    }

    /// `is_function2 Fail Fun Arity` — `Fun` is a fun of the given arity.
    ///
    /// Operand order is the file's (`emu/ops.tab:831`, verified against the oracle:
    /// `{test,is_function2,{f,N},[Fun,{integer,Arity}]}`), i.e. `args[0]` = `Fun`,
    /// `args[1]` = `Arity` (an integer operand).
    fn eval_is_function2(&self, args: &[Operand], ctx: &mut dyn ExecutionContext) -> Result<bool> {
        let value = self.test_arg(args, 0, ctx)?;
        if !crate::terms::is_fun(value) {
            return Ok(false);
        }
        match args.get(1) {
            Some(Operand::Integer(arity)) if *arity >= 0 && *arity <= u32::MAX as i64 => {
                Ok(crate::terms::fun_arity(value)? == *arity as u32)
            }
            _ => Err(error::unsupported("is_function2: arity operand")),
        }
    }

    /// `is_tagged_tuple Fail Src Arity Tag` — `Src` is a tuple of `Arity` whose first
    /// element is `Tag` (the compiler's record test).
    ///
    /// Operand order is the file's (`emu/ops.tab:664`; verified against the oracle:
    /// `{test,is_tagged_tuple,{f,N},[{x,0},3,{atom,r}]}`), i.e. `args[0]` = `Src`,
    /// `args[1]` = `Arity` (an integer), `args[2]` = `Tag` (an atom) — the same layout
    /// `beam_loader::TestKind::IsTaggedTuple` documents.
    fn eval_is_tagged_tuple(
        &self,
        args: &[Operand],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<bool> {
        let value = self.test_arg(args, 0, ctx)?;
        let arity = match args.get(1) {
            Some(Operand::Integer(arity)) => *arity,
            _ => return Err(error::unsupported("is_tagged_tuple: arity operand")),
        };
        // A tagged tuple has at least the tag element and its arity always fits
        // `MAX_ARITYVAL`; any other operand can never match, so the test fails.
        if !(1..=MAX_ARITYVAL as i64).contains(&arity) {
            return Ok(false);
        }
        if !predicates::is_tuple_arity(value, arity as usize) {
            return Ok(false);
        }
        let tag = self.test_arg(args, 2, ctx)?;
        // The first element of a tagged tuple is the (immediate) tag atom.
        Ok(crate::terms::tuple_element(value, 0)? == tag)
    }

    /// `test_arity Value Integer` — the value is a tuple of that arity.
    ///
    /// This reproduces C's `test_arity(Fail, Pointer, Arity)`
    /// (`emu/instrs.tab:1055`): `if (*tuple_val($Pointer) != $Arity) FAIL;` — a bare
    /// header-word compare with **no** `is_boxed` guard. [`predicates::is_tuple_arity`]
    /// is `is_boxed(x) && *boxed_val(x) == make_arityval(A)`, and `tuple_val(p) ==
    /// boxed_val(p)`, so for a **boxed** operand the two are the exact same test: a
    /// tuple of the right arity succeeds and every other boxed term (float / bignum /
    /// map / record / binary — none of whose headers is an `ARITYVAL`) fails. Those
    /// are precisely the semantics of C's production `BEAM_IS_TUPLE`
    /// (`emu/beam_emu.c:222-227`) and of the fused `is_tuple_of_arity`
    /// (`emu/instrs.tab:1048`) the compiler emits.
    ///
    /// The one divergence is an **immediate** operand: C dereferences
    /// `p - TAG_PRIMARY_BOXED` (an out-of-term word — undefined behaviour) while the
    /// engine safely returns `false`. A well-formed module never reaches `test_arity`
    /// with an immediate, because the loader rewrites that operand to a jump
    /// (`emu/ops.tab:686`), so the engine is behaviourally equivalent — only strictly
    /// safer.
    fn eval_test_arity(&self, args: &[Operand], ctx: &mut dyn ExecutionContext) -> Result<bool> {
        let value = self.test_arg(args, 0, ctx)?;
        match args.get(1) {
            Some(Operand::Integer(arity)) => {
                if *arity < 0 || *arity > MAX_ARITYVAL as i64 {
                    // No real tuple has a negative or out-of-range arity.
                    return Ok(false);
                }
                Ok(predicates::is_tuple_arity(value, *arity as usize))
            }
            _ => Err(error::unsupported("test_arity: arity operand")),
        }
    }

    /// Read test argument `i`, or a typed error if the instruction has too few.
    ///
    /// `#[inline(always)]`-marked: every `test` instruction reads one argument, and
    /// with only `#[inline]` this stayed out of line (measured: a ~5% self-time
    /// symbol on the BIF-free list walk), paying a call and a 72-byte
    /// `Result<Eterm, BeamError>` round trip per `test`.
    #[inline(always)]
    fn test_arg(
        &self,
        args: &[Operand],
        i: usize,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        match args.get(i) {
            Some(operand) => read_operand(self, ctx, operand),
            None => Err(error::unsupported("test: missing argument")),
        }
    }

    /// Read the two arguments of a comparison test.
    fn test_pair(
        &self,
        args: &[Operand],
        ctx: &mut dyn ExecutionContext,
    ) -> Result<(Eterm, Eterm)> {
        Ok((self.test_arg(args, 0, ctx)?, self.test_arg(args, 1, ctx)?))
    }
}

#[cfg(test)]
mod tests {
    //! Unit coverage for the `test_arity` test kind (Handoff #6).
    //!
    //! Drives `Machine::eval_test` with `TestKind::TestArity` and a real operand
    //! list (`[x(0), Arity]`) — the interpreter path, not just the predicate. It pins
    //! the equivalence with C's `test_arity` (`emu/instrs.tab:1055`): a tuple of the
    //! matching arity succeeds, a tuple of a different arity fails, and a **boxed
    //! non-tuple** (float / bignum / map — the case the crate note missed) fails too,
    //! because none of those headers is an `ARITYVAL`.
    use super::*;
    use crate::context::MinimalContext;
    use crate::terms;
    use beam_term::{make_small, NIL};

    /// Evaluate `test_arity x(0) Arity` over `machine` with `x(0)` set to `value`.
    ///
    /// `ctx` is threaded in (rather than created here) so the arena holding the
    /// boxed `value` outlives the call — `MinimalContext` frees its arena on drop.
    fn eval_test_arity(
        ctx: &mut MinimalContext,
        machine: &mut Machine,
        module: &Module,
        value: Eterm,
        arity: i64,
    ) -> bool {
        machine.x_mut()[0] = value;
        let args = [Operand::X(0), Operand::Integer(arity)];
        machine
            .eval_test(module, TestKind::TestArity, &args, ctx)
            .expect("test_arity evaluates")
    }

    #[test]
    fn test_arity_matches_a_tuple_of_the_right_arity() {
        let module = Module::default();
        let mut ctx = MinimalContext::new();
        let mut machine = Machine::new();
        let t2 = terms::make_tuple(&mut ctx, &[make_small(1), make_small(2)]).unwrap();
        let elems3 = [make_small(1), make_small(2), make_small(3)];
        let t3 = terms::make_tuple(&mut ctx, &elems3).unwrap();

        assert!(eval_test_arity(&mut ctx, &mut machine, &module, t2, 2));
        assert!(!eval_test_arity(&mut ctx, &mut machine, &module, t2, 3));
        assert!(eval_test_arity(&mut ctx, &mut machine, &module, t3, 3));
        assert!(!eval_test_arity(&mut ctx, &mut machine, &module, t3, 2));
    }

    #[test]
    fn test_arity_rejects_boxed_non_tuples() {
        // For every *boxed* non-tuple C's `test_arity` (`emu/instrs.tab:1055`) reads
        // the very same header word the engine reads, and it is never an `ARITYVAL`,
        // so both fail — the equivalence C's production `BEAM_IS_TUPLE`
        // (`emu/beam_emu.c:222-227`) implies.
        let module = Module::default();
        let mut ctx = MinimalContext::new();
        let mut machine = Machine::new();
        let float = terms::make_float(&mut ctx, 1.5).unwrap();
        let big = terms::make_integer(&mut ctx, i64::MAX).unwrap(); // an exact bignum
        let map = terms::make_map(
            &mut ctx,
            &[(make_small(1), make_small(2))],
            &module.atom_table,
        )
        .unwrap();

        for value in [float, big, map] {
            for arity in [0, 1, 2, 3] {
                assert!(
                    !eval_test_arity(&mut ctx, &mut machine, &module, value, arity),
                    "a boxed non-tuple must fail test_arity for any arity"
                );
            }
        }
    }

    #[test]
    fn test_arity_is_safe_for_immediates() {
        // The one place the engine differs from C: `test_arity` on an immediate
        // dereferences an out-of-term word in C (undefined behaviour); here it is a
        // safe `false`. A well-formed module never reaches this (the loader rewrites
        // an immediate operand to a jump — `emu/ops.tab:686`), but the engine must not
        // crash on it either.
        let mut module = Module::default();
        let atom = module.atom_table.intern("an_atom");
        let mut ctx = MinimalContext::new();
        let mut machine = Machine::new();

        for arity in [0, 1, 2] {
            assert!(!eval_test_arity(
                &mut ctx,
                &mut machine,
                &module,
                make_small(7),
                arity
            ));
            assert!(!eval_test_arity(
                &mut ctx,
                &mut machine,
                &module,
                NIL,
                arity
            ));
            assert!(!eval_test_arity(
                &mut ctx,
                &mut machine,
                &module,
                make_atom(atom),
                arity
            ));
        }
    }

    #[test]
    fn test_arity_out_of_range_or_negative_fails() {
        let module = Module::default();
        let mut ctx = MinimalContext::new();
        let mut machine = Machine::new();
        let t2 = terms::make_tuple(&mut ctx, &[make_small(1), make_small(2)]).unwrap();

        assert!(!eval_test_arity(&mut ctx, &mut machine, &module, t2, -1));
        assert!(!eval_test_arity(
            &mut ctx,
            &mut machine,
            &module,
            t2,
            MAX_ARITYVAL as i64 + 1
        ));
    }

    /// The oracle's answer for `{module_info,0} < {g,1}` (atom **name** order), or
    /// `None` when there is no runnable `erl` on `PATH` (the caller skips loudly).
    ///
    /// The two atoms sort oppositely by name (`"g" < "module_info"`) and by the id
    /// order the module's snapshot would imply (`module_info` is interned first) —
    /// exactly the shape that made `sofs:restrict_n/5` take the wrong clause.
    fn oracle_tuple_lt() -> Option<bool> {
        let probe = std::process::Command::new("erl")
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok();
        if !probe {
            return None;
        }
        let eval = "io:format(\"~w\", [{module_info,0} < {g,1}]), halt().";
        let out = std::process::Command::new("erl")
            .args(["-noshell", "-eval", eval])
            .output()
            .ok()?;
        assert!(
            out.status.success(),
            "the system `erl` exited {} for `{eval}`\n  stderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
        match String::from_utf8_lossy(&out.stdout).trim() {
            "true" => Some(true),
            "false" => Some(false),
            other => panic!("unexpected oracle output {other:?} for `{eval}`"),
        }
    }

    /// A comparison `test` must order atoms by **name**, through the host's shared
    /// atom namespace — exactly as the `<` BIF does — never by raw atom id through
    /// the running module's load-time snapshot.
    ///
    /// Reproduces the `sofs:restrict_n/5` divergence: a module loaded *before* an
    /// atom is interned cannot name it, so `{module_info,0} < {G,1}` (with `G`
    /// interned afterwards and therefore beyond the snapshot) was ordered by atom id
    /// and took the wrong clause. The engine must consult the namespace
    /// [`ExecutionContext::atoms`] supplies.
    #[test]
    fn comparisons_order_atoms_by_name_through_the_shared_namespace() {
        let Some(expected) = oracle_tuple_lt() else {
            eprintln!(
                "SKIP interpreter::test::comparisons_order_atoms_by_name_through_the_shared_namespace: \
                 no runnable `erl` on PATH"
            );
            return;
        };

        // The module's **load-time snapshot**: it interns `module_info` but not `g`.
        let mut module = Module::default();
        let module_info = module.atom_table.intern("module_info");

        // The **live** shared namespace: the snapshot plus `g`, interned afterwards
        // so `g`'s global id is HIGHER than `module_info`'s. Raw-id order would then
        // say `module_info < g`, the opposite of the name order (`"g" < "module_info"`).
        let mut live = module.atom_table.clone();
        let g = live.intern("g");
        assert!(
            module.atom_name(g).is_none(),
            "`g` must be beyond the module's load-time snapshot"
        );
        assert_eq!(module.atom_name(module_info), Some("module_info"));

        let mut ctx = MinimalContext::with_shared_atoms(live);
        let mut machine = Machine::new();
        let lhs = terms::make_tuple(&mut ctx, &[make_atom(module_info), make_small(0)]).unwrap();
        let rhs = terms::make_tuple(&mut ctx, &[make_atom(g), make_small(1)]).unwrap();
        machine.x_mut()[0] = lhs;
        machine.x_mut()[1] = rhs;

        let args = [Operand::X(0), Operand::X(1)];
        let lt = machine
            .eval_test(&module, TestKind::IsLt, &args, &mut ctx)
            .expect("a `{A,B} < {C,D}` guard evaluates");

        assert_eq!(
            lt, expected,
            "`{{module_info,0}} < {{g,1}}` must order atoms by NAME (the `<` BIF \
             answers {expected}); the module's snapshot cannot name `g`, so an \
             id-ordered fallback flips the clause"
        );
    }
}
