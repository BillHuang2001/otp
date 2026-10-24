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

//! List instruction family.
//!
//! Owns the cons-cell instructions (per `../beam_loader/CONTEXT.md`, "The
//! instruction vocabulary"):
//! - [`Instr::PutList`] — build a cons cell
//! - [`Instr::GetList`] — split a cons cell into head and tail
//! - [`Instr::GetHd`] — fetch the head (car) of a cons cell
//! - [`Instr::GetTl`] — fetch the tail (cdr) of a cons cell
//!
//! Ground truth (read-only): `emu/instrs.tab` `put_list` (`:732`), `get_list`
//! (`:389`), `get_hd` (`:398`) and `get_tl` (`:403`).
//!
//! ## Semantics
//! - `PutList { head, tail, dst }` — build `[Head | Tail]` and store it in `dst`.
//!   `Tail` need not be a list: an improper list is a legal term.
//! - `GetList { source, head, tail }` / `GetHd { source, head }` /
//!   `GetTl { source, tail }` — `source` must be a cons cell (a non-empty list;
//!   `[]` is *not* one). The C opcodes trust a preceding `is_nonempty_list` test
//!   and read the cell unchecked, so this is the **safety net**: a non-cons source
//!   raises `{badmatch, Source}` (class `error`).
//!
//! Every failure is a typed [`beam_common::BeamError`]: `write_operand`/
//! `read_operand` propagate, an unsupported operand is `error::unsupported`, and
//! the `{badmatch, V}` raise is `crate::terms::badmatch_reason` +
//! [`Machine::throw_error`] (a `Step::Goto` into a live catch frame, or a typed
//! `Err` when there is none). Never a panic.

use crate::interpreter::LabelMap;

use beam_common::Result;
use beam_loader::{Instr, Module, Operand};
use beam_term::Eterm;

use crate::context::{ExecutionContext, RootSet};
use crate::machine::Machine;
use crate::operand::{operand_slack_words, read_operand, read_operand_slack, write_operand};
use crate::terms;
use crate::Step;

/// Execute a list-family instruction.
///
/// Returns `Ok(Some(step))` when `instr` is a list opcode this family executes,
/// `Ok(None)` when `instr` is not this family's variant, and `Err` on a raise.
///
/// This is the family's complete **ownership statement** (all four cons-cell
/// variants are claimed here and nowhere else). The router
/// ([`super::dispatch`]) calls [`put_list`] and [`get_list`] **directly** with the
/// operands its own pattern has already destructured — those two are the hottest
/// dispatched instructions in real code (every list build, every list traversal) and
/// re-matching the variant a second time cost a second jump table plus a re-derived
/// operand slice per instruction. The remaining two variants come through here, and
/// the duplicated arms below are the same bodies the router's fast path calls, so the
/// claim and the behaviour cannot drift.
///
/// `#[inline]`-marked: the two remaining cons-cell variants are still traversals, and
/// out of line this entry cost ~39 extra instructions per dispatched instruction
/// (measured; see [`super::dispatch`]'s doc).
#[inline]
pub(crate) fn exec(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    instr: &Instr,
    func: u32,
    pc: u32,
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    let _ = (labels, func, pc);
    match instr {
        Instr::PutList { head, tail, dst } => put_list(machine, head, tail, dst, ctx),

        Instr::GetList { source, head, tail } => get_list(machine, module, source, head, tail, ctx),

        Instr::GetHd { source, head } => {
            let value = read_operand(machine, ctx, source)?;
            match terms::cons_head(value) {
                Ok(h) => {
                    write_operand(machine, head, h)?;
                    Ok(Some(Step::Next))
                }
                Err(_) => {
                    let reason = badmatch(machine, ctx, module, source)?;
                    machine.throw_error(reason).map(Some)
                }
            }
        }

        Instr::GetTl { source, tail } => {
            let value = read_operand(machine, ctx, source)?;
            match terms::cons_tail(value) {
                Ok(t) => {
                    write_operand(machine, tail, t)?;
                    Ok(Some(Step::Next))
                }
                Err(_) => {
                    let reason = badmatch(machine, ctx, module, source)?;
                    machine.throw_error(reason).map(Some)
                }
            }
        }

        _ => Ok(None),
    }
}

/// `put_list Head Tail Dst` — build `[Head | Tail]` and store it in `Dst`.
///
/// One of the two bodies the router calls directly (see [`exec`]): the operands are
/// bound by the caller's `Instr` pattern, so no second variant match and no operand
/// re-derivation. `Tail` need not be a list — an improper list is a legal term.
#[inline]
pub(crate) fn put_list(
    machine: &mut Machine,
    head: &Operand,
    tail: &Operand,
    dst: &Operand,
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    let extra = operand_slack_words(head) + operand_slack_words(tail);
    let cons = terms::make_cons_rooted(machine, ctx, RootSet::ALL_LIVE, extra, |m, s| {
        Ok((
            read_operand_slack(m, head, s)?,
            read_operand_slack(m, tail, s)?,
        ))
    })?;
    write_operand(machine, dst, cons)?;
    Ok(Some(Step::Next))
}

/// `get_list Source Head Tail` — split the cons cell in `Source`.
///
/// The other directly-routed body (see [`exec`]). `Source` must be a cons cell (a
/// non-empty list; `[]` is *not* one): the C opcode trusts a preceding
/// `is_nonempty_list` test and reads the cell unchecked, so this is the **safety
/// net** — a non-cons source raises `{badmatch, Source}` (class `error`).
#[inline]
pub(crate) fn get_list(
    machine: &mut Machine,
    module: &Module,
    source: &Operand,
    head: &Operand,
    tail: &Operand,
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    let value = read_operand(machine, ctx, source)?;
    match (terms::cons_head(value), terms::cons_tail(value)) {
        (Ok(h), Ok(t)) => {
            write_operand(machine, head, h)?;
            write_operand(machine, tail, t)?;
            Ok(Some(Step::Next))
        }
        // `[]` or a non-list is not a cons cell.
        _ => {
            let reason = badmatch(machine, ctx, module, source)?;
            machine.throw_error(reason).map(Some)
        }
    }
}

/// The `{badmatch, Source}` reason for a non-cons `source`, re-reading the operand
/// **after** the reason block's reserve (see [`crate::terms::badmatch_reason`]).
fn badmatch(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    module: &Module,
    source: &beam_loader::Operand,
) -> Result<Eterm> {
    terms::badmatch_reason_rooted(machine, ctx, RootSet::ALL_LIVE, module, |m, s| {
        read_operand_slack(m, source, s)
    })
}
