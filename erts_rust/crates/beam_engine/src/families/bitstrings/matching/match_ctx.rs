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

//! Bitstring **match entry** and match-context handling.
//!
//! Owns `bs_start_match3`/`bs_start_match4` (`enter_match` plus its
//! `no_fail`/`resume` fail-operand decode), the match-context operand readers
//! (`read_ctx`, and its post-allocation twin `reread_ctx`) and the
//! `bs_set_position` position decode (`position_of`).
use beam_common::Result;
use beam_loader::{Label, Module, Operand, NO_FAIL};
use beam_term::Eterm;

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::Machine;
use crate::operand::{operand_slack_words, read_operand, read_operand_slack};

use super::super::{raw, small_value};
// ---------------------------------------------------------------------------
// Match entry
// ---------------------------------------------------------------------------
/// Start (or resume) a match over the subject **operand**, yielding the match
/// context.
///
/// A heap binary becomes a fresh context at position 0 (`i_bs_start_match3`,
/// `bs_instrs.tab:1199`); a term that is already an `ErlSubBits` is re-wrapped with
/// its current position (the C `reuse` path). Anything else is a *match* failure
/// (`None`), not an error.
///
/// The subject is handed in as its operand rather than as a read `Eterm` so the new
/// context's `orig` word can be re-read from the machine's **post-collection** state
/// after the context block's reserve ([`raw::make_ctx`]): a moving collector may
/// relocate the subject, and storing the pre-reserve copy would leave a stale
/// pointer inside a live term.
pub(crate) fn enter_match(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    subject: &Operand,
) -> Result<Option<Eterm>> {
    let value = read_operand(machine, ctx, subject)?;
    let extra = operand_slack_words(subject);
    if let Some((_, bits)) = raw::heap_bits(value) {
        // A heap binary: the context starts at bit 0 and spans its whole size.
        let context = raw::make_ctx(machine, ctx, 0, bits, extra, |m, slack| {
            read_operand_slack(m, subject, slack)
        })?;
        return Ok(Some(context));
    }
    if let Some((_, start, end)) = raw::ctx_parts(value) {
        // The `reuse` path: re-wrap the *inner* subject at the current position, so
        // the re-derived `orig` keeps naming the underlying binary, never the outer
        // context.
        let context = raw::make_ctx(machine, ctx, start, end, extra, |m, slack| {
            let value = read_operand_slack(m, subject, slack)?;
            raw::inner_orig(value)
                .ok_or_else(|| error::unsupported("bs_start_match: subject is not a match context"))
        })?;
        return Ok(Some(context));
    }
    Ok(None)
}
/// Decode `bs_start_match4`'s `fail` operand: a label, or the atoms
/// `no_fail`/`resume` (`emu/ops.tab:1243-1262`).
pub(crate) fn resolve_match4_fail(module: &Module, fail: &Operand) -> Result<(Label, bool)> {
    match fail {
        Operand::Label(label) => Ok((*label, false)),
        Operand::Atom(atom) => match module.atom_name(*atom) {
            Some("no_fail") => Ok((NO_FAIL, false)),
            Some("resume") => Ok((NO_FAIL, true)),
            Some(other) => Err(error::unsupported(&format!(
                "bs_start_match4: fail atom {other}"
            ))),
            None => Err(error::unsupported("bs_start_match4: unknown fail atom")),
        },
        _ => Err(error::unsupported(
            "bs_start_match4: unsupported fail operand",
        )),
    }
}
/// Read a match-context operand, returning `(term, orig, start, end)`.
pub(crate) fn read_ctx(
    machine: &Machine,
    ctx: &mut dyn ExecutionContext,
    operand: &Operand,
    what: &str,
) -> Result<(Eterm, Eterm, usize, usize)> {
    let term = read_operand(machine, ctx, operand)?;
    let (orig, start, end) = raw::ctx_parts(term)
        .ok_or_else(|| error::unsupported(&format!("{what}: operand is not a match context")))?;
    Ok((term, orig, start, end))
}
/// Re-read a match-context operand **after** an allocating read; the fresh
/// (post-collection) context term is returned.
///
/// A rooted allocation ([`raw::make_bits`]/[`raw::make_big`]/
/// [`crate::terms::make_float`]) may run a **moving** collector that relocates the
/// context block, so an `Eterm` read *before* the allocation is a stale pointer
/// afterwards. Any match opcode that (1) reads its context, (2) performs an
/// allocating field read and (3) then writes the new position back into the
/// context must therefore re-read the context operand **here** before the write —
/// the frozen **reserve, then read, then write** ordering ([`crate::terms`]).
/// The operand (a register/Y slot, part of the machine's root set, which the
/// collector rewrites in place) is re-evaluated rather than the stale term reused.
///
/// This is the term-only twin of [`read_ctx`]; it still validates that the operand
/// is a live match context.
pub(crate) fn reread_ctx(
    machine: &Machine,
    ctx: &mut dyn ExecutionContext,
    operand: &Operand,
    what: &str,
) -> Result<Eterm> {
    let (term, _, _, _) = read_ctx(machine, ctx, operand, what)?;
    Ok(term)
}
/// The bit position a `bs_set_position` operand denotes.
///
/// The engine's `bs_get_position` snapshots the whole context, so the ordinary
/// input is another `ErlSubBits`; the C 64-bit lowering instead stores the raw
/// position as a small integer (`bs_instrs.tab:1125-1129`), so that form is
/// accepted too.
pub(crate) fn position_of(value: Eterm) -> Result<usize> {
    if let Some((_, start, _)) = raw::ctx_parts(value) {
        return Ok(start);
    }
    if let Some(n) = small_value(value) {
        if n >= 0 {
            return Ok(n as usize);
        }
        return Err(error::unsupported("bs_set_position: negative bit position"));
    }
    Err(error::unsupported(
        "bs_set_position: position is neither a context nor a small integer",
    ))
}
