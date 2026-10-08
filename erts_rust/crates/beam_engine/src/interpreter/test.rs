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
        let atoms = &module.atom_table;
        match kind {
            TestKind::IsLt => {
                let (a, b) = self.test_pair(args, ctx)?;
                Ok(cmp_arith(a, b, atoms).is_lt())
            }
            TestKind::IsGe => {
                let (a, b) = self.test_pair(args, ctx)?;
                Ok(cmp_arith(a, b, atoms).is_ge())
            }
            TestKind::IsEq => {
                let (a, b) = self.test_pair(args, ctx)?;
                Ok(cmp_arith(a, b, atoms).is_eq())
            }
            TestKind::IsNe => {
                let (a, b) = self.test_pair(args, ctx)?;
                Ok(!cmp_arith(a, b, atoms).is_eq())
            }
            TestKind::IsEqExact => {
                let (a, b) = self.test_pair(args, ctx)?;
                Ok(cmp(a, b, atoms).is_eq())
            }
            TestKind::IsNeExact => {
                let (a, b) = self.test_pair(args, ctx)?;
                Ok(!cmp(a, b, atoms).is_eq())
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
                // module's atom table; a name absent from the table can never be the
                // value, so a missing name is `false` (not an error).
                let v = self.test_arg(args, 0, ctx)?;
                if !is_atom(v) {
                    return Ok(false);
                }
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
