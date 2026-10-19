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
//! `bs_get_position`/`bs_set_position` position pair ([`position_term`] and
//! [`position_of`]).
//!
//! # The `pub(crate)` seam
//!
//! Every entry point in this module is `pub(crate)` **on purpose**: a native JIT round
//! that wants to inline a match-context opcode drives exactly these bodies (the semantics
//! below are the family's, not the interpreter's), so the visibility is the documented
//! seam rather than an accident. The enclosing `matching`/`raw` modules are still private
//! to `families::bitstrings`, so nothing outside the family resolves them today
//! (`raw::{heap_bits, ctx_parts, set_ctx_start}` are widened alongside for the same
//! reason).
use beam_common::Result;
use beam_loader::{Label, Module, Operand, NO_FAIL};
use beam_term::Eterm;

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::Machine;
use crate::operand::{operand_slack_words, read_operand, read_operand_slack};

use super::super::{make_small_checked, raw, small_value};
// ---------------------------------------------------------------------------
// Match entry
// ---------------------------------------------------------------------------
/// Start (or resume) a match over the subject **operand**, yielding the match
/// context.
///
/// A heap binary (or an off-heap refc window) becomes a fresh context at position 0
/// (`i_bs_start_match3`, `bs_instrs.tab:1199-1213`); a term that already **is** a match
/// context is handed back **unchanged** — the C `reuse` path, which does `$Dst =
/// context` and allocates nothing at all (`bs_instrs.tab:1204-1223`). Anything else is a
/// *match* failure (`None`), not an error.
///
/// The reuse test is C's `erl_sub_bits_is_match_context` (`erl_bits.h:107-108`), which
/// reads the `ErlSubBits` **flags**: a match context is the only non-volatile one. This
/// engine never materialises a non-match-context, non-refc `ErlSubBits` — [`raw::make_ctx`]
/// builds match contexts, [`raw::make_binary`]/[`raw::make_writable`] build an
/// `ErlHeapBits` or the `[BinRef][ErlSubBits]` refc pair — so "is an `ErlSubBits` term
/// that is not a refc binary" ([`raw::ctx_parts`] accepted, `raw::heap_bits` declined)
/// is exactly that predicate. The `heap_bits` test therefore runs **first**: a refc window
/// is a *subject*, not a context, and gets a fresh match context over it, just as the C's
/// `reuse == 0` arm does.
///
/// Identity is faithful, not merely cheap: the C re-uses the term **without** re-reading
/// `orig`/`start`/`end`, and the block the engine would have rebuilt carries those very
/// fields (a re-wrap could only copy the context's own `orig` into a second block). It also
/// keeps the accumulator loop of a binary comprehension at **zero** rooted allocations per
/// iteration — the comprehension's `bs_start_match3 {x,0} → {x,2}` re-enters the context
/// produced by the previous iteration, so every one of those iterations used to pay a
/// 5-word collecting reservation.
///
/// A `bs_start_match3` whose `Dst` is its own `Src` stays a no-op (the write publishes the
/// value the register already holds), matching `emu/ops.tab:1251-1252`'s
/// `equal(Ctx, Dst) => _` for the `resume` form.
///
/// The fresh-context arm takes the subject as its **operand** rather than as a read
/// `Eterm` so the new context's `orig` word can be re-read from the machine's
/// **post-collection** state after the context block's reserve ([`raw::make_ctx`]): a
/// moving collector may relocate the subject, and storing the pre-reserve copy would
/// leave a stale pointer inside a live term.
pub(crate) fn enter_match(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    subject: &Operand,
) -> Result<Option<Eterm>> {
    let value = read_operand(machine, ctx, subject)?;
    if let Some((_, bits, _)) = raw::heap_bits(value) {
        // A heap binary (or a refc window): the context starts at bit 0 and spans the
        // subject's whole size.
        let extra = operand_slack_words(subject);
        let context = raw::make_ctx(machine, ctx, 0, bits, extra, |m, slack| {
            read_operand_slack(m, subject, slack)
        })?;
        return Ok(Some(context));
    }
    if raw::ctx_parts(value).is_some() {
        // The `reuse` path: the operand already **is** the mutable match context, so the
        // C's `$Dst = context` re-binds that very term — no reservation, no re-wrap, and
        // the position the context carries is the position the match continues from.
        return Ok(Some(value));
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
/// A rooted allocation ([`raw::make_binary`]/[`raw::make_big`]/
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
/// Encode a bit position the way C's `i_bs_get_position` does: `make_small(position)`
/// (`emu/bs_instrs.tab:1145-1149`; the ARCH_64 fused `i_bs_start_match3_gp` publishes
/// the same immediate into its `$Pos`, `:1091`).
///
/// The result is an **immediate**, so a `bs_get_position`/`bs_set_position` save and
/// restore pair reserves nothing at all — the C instruction pair allocates nothing
/// either (the store is `sb->start`, `:1140-1143`).
///
/// `None` when the position does not fit a small integer (60-bit on the frozen layout):
/// C then builds a `BIG_UINT_HEAP_SIZE` bignum (`:1150-1163`) and its `bs_set_position`
/// reads `BIG_V(big_val(position))` back. [`position_of`] does not decode a bignum yet,
/// so the caller keeps its own fallback rather than storing a term it could not restore.
pub(crate) fn position_term(start: usize) -> Option<Eterm> {
    let position = i64::try_from(start).ok()?;
    make_small_checked(position).ok()
}

/// The bit position a `bs_set_position` operand denotes.
///
/// The ordinary input is the small integer [`position_term`] stored; C's own pair works
/// that way (`bs_instrs.tab:1140-1149`), and its `bs_set_position` additionally accepts a
/// bignum for a position that needs one. An `ErlSubBits` **match context** is accepted
/// too: it is what the engine's over-large-position `bs_get_position` fallback publishes
/// (a whole-context snapshot, see the `Instr::BsGetPosition` arm) and what the pre-`-gp`
/// C lowering's `i_bs_start_match3` consumed, so both spellings of "a saved position"
/// restore correctly.
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
