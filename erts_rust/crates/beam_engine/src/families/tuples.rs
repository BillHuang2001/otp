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

//! Tuple instruction family.
//!
//! Owns the tuple-construction/-inspection instructions (per
//! `../beam_loader/CONTEXT.md`, "The instruction vocabulary"):
//! - [`Instr::PutTuple2`] — build a tuple from its element operands
//! - [`Instr::GetTupleElement`] — fetch one element of a tuple
//! - [`Instr::SelectTupleArity`] — switch on a tuple's arity
//!
//! Ground truth (read-only): `emu/instrs.tab` `put_tuple2` (`:746`) and
//! `i_get_tuple_element` (`:416`), and `emu/select_instrs.tab`
//! `i_select_tuple_arity` (`:111`, the `select_val_lin`/`get_arity` path).
//!
//! ## Semantics
//! - `PutTuple2 { dst, elements }` — read every element operand, build the tuple
//!   (an `arityval` header + the elements) and store it in `dst`.
//! - `GetTupleElement { source, element, dst }` — `source` must be a tuple with
//!   `arity > element`; the C opcode trusts a preceding type test and reads the
//!   word unchecked, so this is the **safety net**: anything else raises
//!   `{badmatch, Source}` (class `error`).
//! - `SelectTupleArity { tuple, fail, destinations }` — switch on the source
//!   tuple's arity. A non-tuple source has no arity at all (C's `get_arity` sets the
//!   switch value to `NIL`, which no integer alternative can equal), so it matches
//!   nothing and takes exactly the same `$FAIL(Fail)` path as an unmatched arity;
//!   `select_tuple_arity` is only emitted after an `is_tuple` test of the same fail
//!   label, so that path is unreachable for compiler output. The matching
//!   `(Integer(n), Label)` alternative wins; with no match it falls to `fail`, or —
//!   when `fail` is `NO_FAIL` (an exhaustive switch) — raises `{badmatch, Tuple}`.
//!
//! Every failure is a typed [`beam_common::BeamError`]: `write_operand`/
//! `read_operand` propagate, an unsupported operand is `error::unsupported`, and
//! the `{badmatch, V}` raise is `crate::terms::badmatch_reason` +
//! [`Machine::throw_error`] (a `Step::Goto` into a live catch frame, or a typed
//! `Err` when there is none). Never a panic.

use crate::interpreter::LabelMap;

use beam_common::Result;
use beam_loader::{Instr, Module, Operand, NO_FAIL};

use crate::context::ExecutionContext;
use crate::machine::Machine;
use crate::operand::{read_operand, write_operand};
use crate::terms;
use crate::Step;

/// Execute a tuple-family instruction.
///
/// Returns `Ok(Some(step))` when `instr` is a tuple opcode this family executes,
/// `Ok(None)` when `instr` is not this family's variant, and `Err` on a raise.
pub(crate) fn exec(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    instr: &Instr,
    func: u32,
    pc: u32,
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    let _ = (func, pc);
    match instr {
        Instr::PutTuple2 { dst, elements } => {
            let mut elems = Vec::with_capacity(elements.len());
            for operand in elements {
                elems.push(read_operand(machine, ctx, operand)?);
            }
            let tuple = terms::make_tuple(ctx, &elems)?;
            write_operand(machine, dst, tuple)?;
            Ok(Some(Step::Next))
        }

        Instr::GetTupleElement {
            source,
            element,
            dst,
        } => {
            let value = read_operand(machine, ctx, source)?;
            match terms::tuple_arity(value) {
                Ok(arity) if (*element as usize) < arity => {
                    let elem = terms::tuple_element(value, *element as usize)?;
                    write_operand(machine, dst, elem)?;
                    Ok(Some(Step::Next))
                }
                // Not a tuple, or the element index is out of range.
                _ => machine
                    .throw_error(terms::badmatch_reason(ctx, module, value)?)
                    .map(Some),
            }
        }

        Instr::SelectTupleArity {
            tuple,
            fail,
            destinations,
        } => {
            let value = read_operand(machine, ctx, tuple)?;
            // C's `select_val_lin.get_arity` sets the switch value of a non-tuple
            // source to `NIL` (`select_instrs.tab:122-128`), and `execute` then finds
            // no alternative equal to it and takes the ordinary `$FAIL($Fail)` jump
            // (`:130-153`) — so a non-tuple behaves exactly like an arity that matches
            // no destination.
            let arity = terms::tuple_arity(value).ok();
            let mut target = None;
            for (candidate, label) in destinations {
                // Arities are decoded as `{integer, N}` operands; a non-integer
                // alternative can never equal an arity and is skipped. A non-tuple
                // source has no arity and so matches nothing.
                if let (Some(arity), Operand::Integer(n)) = (arity, candidate) {
                    if *n >= 0 && *n as usize == arity {
                        target = Some(*label);
                        break;
                    }
                }
            }
            let label = match target {
                Some(label) => label,
                None if *fail == NO_FAIL => {
                    return machine
                        .throw_error(terms::badmatch_reason(ctx, module, value)?)
                        .map(Some);
                }
                None => *fail,
            };
            let (next_func, next_pc) = labels.resolve(label)?;
            Ok(Some(Step::Goto {
                func: next_func,
                pc: next_pc,
            }))
        }

        _ => Ok(None),
    }
}
