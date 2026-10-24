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

//! The `bs_match` shape: a fully static command list lowered in place.
//!
//! Only `ensure_at_least` / `ensure_exactly` / `skip` / `integer` commands are accepted —
//! everything else builds a fresh term on every execution (`raw::make_binary` reserves
//! unconditionally), which a moving collection could use to relocate the context block
//! mid-list. [`bs_match_group_len`], [`bs_match_static_size`] and
//! [`bs_match_integer_flags`] are the decoder, shared with the refusal census
//! (`emit/test/refusal_census.rs`) that attributes the commands this shape declines.

use beam_common::Result;
use beam_loader::{Label, Module, Operand, NO_FAIL};
use beam_term::Eterm;

use crate::interpreter::LabelMap;
use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};

use super::super::link::{ExitSpill, Linker};
use super::super::operands::{Loc, Y_ADDR_SCRATCH, Y_ADDR_SCRATCH_2};
use super::{
    emit_bit_extract, emit_ctx_guard, emit_step_and_continue, emit_subject_view,
    field_is_unsupported, field_width_supported, Allocation, BSF_ALIGNED, BSF_EXACT, BSF_LITTLE,
    BSF_NATIVE, BSF_SIGNED, MOVE_SCRATCH, SMALL_TAG, SUB_BITS_END_DISP, SUB_BITS_START_DISP,
};

#[cfg(test)]
use super::{record_inlined, SHAPE_BS_MATCH};

// ---------------------------------------------------------------------------
// Shape 8: `bs_match { fail, ctx, commands }` with a fully static command list
// ---------------------------------------------------------------------------

/// One statically-decoded `bs_match` command — the subset whose whole effect is a word
/// compare and (for `skip`/`integer`) one `start`-word commit.
#[derive(Clone, Debug, PartialEq, Eq)]
enum BsMatchCmd {
    /// `ensure_at_least Size Unit` — `commands::ensure_at_least`'s five branches.
    EnsureAtLeast { size: usize, unit: usize },
    /// `ensure_exactly Stride` — the remaining window must be exactly `stride` bits.
    EnsureExactly { stride: usize },
    /// `skip Stride` — advance the position by `stride` bits without binding anything.
    Skip { stride: usize },
    /// `integer Live Flags Size Unit Dst` — extract `nbits` bits and bind `dst`.
    Integer { nbits: usize, dst: Loc },
}

/// The operands of a `bs_match` the tier lowers natively.
#[derive(Clone, Debug)]
pub(crate) struct BsMatchShape {
    /// The match-context operand's location.
    ctx: Loc,
    /// The decoded command list, in program order.
    commands: Vec<BsMatchCmd>,
}

/// The operand count of each `bs_match` command group, the name word included — a local
/// re-derivation of the family's `prepared::group_len` (which is `pub(super)` inside the
/// private `families::bitstrings` subtree), pinned against it by
/// [`super::tests::the_command_group_lengths_match_the_familys_own`].
///
/// The steps below are the census' too: `emit/test/refusal_census.rs` walks a command list
/// with them to attribute the commands this shape declines.
pub(crate) fn bs_match_group_len(name: &str) -> Option<usize> {
    match name {
        "ensure_at_least" => Some(3),
        "ensure_exactly" => Some(2),
        "skip" => Some(2),
        "integer" => Some(6),
        "binary" => Some(6),
        "get_tail" => Some(4),
        "=:=" => Some(4),
        _ => None,
    }
}

/// The accept rule of a **static** size/unit/stride operand — a local re-derivation of the
/// family's `prepared::static_size`: a non-negative small integer or a character literal.
pub(crate) fn bs_match_static_size(operand: &Operand) -> Option<usize> {
    match operand {
        Operand::Integer(n) if *n >= 0 && beam_term::try_make_small(*n).is_ok() => Some(*n as usize),
        Operand::Character(c) => Some(*c as usize),
        _ => None,
    }
}

/// Decode an `integer` command's flags operand **statically**, or `None` when it holds a flag
/// this lowering does not model (an unknown atom, a non-list) — a re-derivation of the
/// family's `decode_flags`/`flag_bit` over the two operand forms the compiler emits (`Nil`,
/// or a literal holding the flag-atom list). Only the four bits that change the value are
/// named; `integer`/`unsigned`/`big` are the zero default, exactly as `flag_bit` reads them.
pub(crate) fn bs_match_integer_flags(module: &Module, operand: &Operand) -> Option<u32> {
    fn flags_of(module: &Module, elem: Eterm) -> Option<u32> {
        let named = |name: &str| module.atom_table.lookup(name).map(beam_term::make_atom);
        let eq = |name: &str| named(name) == Some(elem);
        if eq("aligned") {
            Some(BSF_ALIGNED)
        } else if eq("little") {
            Some(BSF_LITTLE)
        } else if eq("signed") {
            Some(BSF_SIGNED)
        } else if eq("exact") {
            Some(BSF_EXACT)
        } else if eq("native") {
            Some(BSF_NATIVE)
        } else if eq("integer") || eq("unsigned") || eq("big") {
            Some(0)
        } else {
            // An unmodelled flag atom: the family decides (the safe direction).
            None
        }
    }
    match operand {
        Operand::Nil => Some(0),
        Operand::Literal(index) => {
            let term = module.literal(*index)?;
            if beam_term::is_nil(term) {
                return Some(0);
            }
            let mut flags = 0;
            for elem in crate::terms::list_elements(term).ok()? {
                flags |= flags_of(module, elem)?;
            }
            Some(flags)
        }
        _ => None,
    }
}

impl BsMatchShape {
    /// The [`BsMatchShape`] the instruction's operands name, or `None` when the whole
    /// instruction must stay on the family.
    ///
    /// Accepted: a **fully static** list of `ensure_at_least` / `ensure_exactly` / `skip` /
    /// `integer`, where every size/unit/stride is a constant, every `ensure_at_least` unit is
    /// a power of two (so `%` is a mask), and every `integer` field is unsigned, big-endian
    /// and of a width [`emit_bit_extract`] covers. Everything else declines the **whole**
    /// instruction:
    ///
    /// * `binary` / `get_tail` / `=:=` build a fresh term on every execution
    ///   (`raw::make_binary` reserves unconditionally), so a moving collection could relocate
    ///   the context block in the middle of the list and the block base this lowering reads
    ///   once would go stale;
    /// * a runtime size/unit/stride is a compare the emitted sequence would not have;
    /// * a signed, little/native-endian or wider-than-`{<= 16, 32}` field is the family's
    ///   general reader;
    /// * more than one `Y` destination needs a second live Y address, which this sequence
    ///   does not keep.
    ///
    /// Purely syntactic and **label-free** (the fail classification is the caller's), so the
    /// block's Y gate ([`super::super::arith::uses_y`]) derives the same answer as the emitter.
    pub(crate) fn of(module: &Module, ctx: &Operand, commands: &[Operand]) -> Option<BsMatchShape> {
        let ctx = Loc::of(ctx)?;
        let mut parsed = Vec::with_capacity(commands.len() / 2 + 1);
        let mut y_destinations = 0usize;
        let mut i = 0usize;
        while i < commands.len() {
            let Operand::Atom(command) = &commands[i] else {
                return None;
            };
            let name = module.atom_name(*command)?;
            let width = bs_match_group_len(name)?;
            let end = i.checked_add(width)?;
            if end > commands.len() {
                // A group running past the end of the list: the family's own typed
                // "truncated command list".
                return None;
            }
            match name {
                "ensure_at_least" => {
                    let size = bs_match_static_size(commands.get(i + 1)?)?;
                    let unit = bs_match_static_size(commands.get(i + 2)?)?;
                    // `remaining % unit` must lower to a mask, and `unit == 0` is a division
                    // by zero — both are compile-time refusals.
                    if !unit.is_power_of_two() {
                        return None;
                    }
                    parsed.push(BsMatchCmd::EnsureAtLeast { size, unit });
                }
                "ensure_exactly" => parsed.push(BsMatchCmd::EnsureExactly {
                    stride: bs_match_static_size(commands.get(i + 1)?)?,
                }),
                "skip" => parsed.push(BsMatchCmd::Skip {
                    stride: bs_match_static_size(commands.get(i + 1)?)?,
                }),
                // `binary` / `get_tail` / `=:=` are decoded only to find their group's
                // length, so the walk stays aligned — never to be lowered. Each one builds a
                // fresh term on every execution (`raw::make_binary` reserves
                // unconditionally), so a moving collection could relocate the context block
                // in the middle of the list. `binary` matters most: its group is the *same
                // width* as `integer`'s, so a fall-through into the `integer` arm below
                // would read it as an integer field and bind an integer where a sub-binary
                // is due — a wrong value, not a refusal.
                "binary" | "get_tail" | "=:=" => return None,
                "integer" => {
                    let flags = bs_match_integer_flags(module, commands.get(i + 2)?)?;
                    let size = bs_match_static_size(commands.get(i + 3)?)?;
                    let unit = bs_match_static_size(commands.get(i + 4)?)?;
                    let nbits = size.checked_mul(unit)?;
                    if !field_width_supported(nbits) {
                        return None;
                    }
                    if field_is_unsupported(flags) {
                        return None;
                    }
                    let dst = Loc::of(commands.get(i + 5)?)?;
                    if dst.is_y() {
                        y_destinations += 1;
                        if y_destinations > 1 {
                            return None;
                        }
                    }
                    parsed.push(BsMatchCmd::Integer { nbits, dst });
                }
                // Unreachable: `bs_match_group_len` above already declined every name that
                // is not one of the six it knows.
                _ => return None,
            }
            i = end;
        }
        Some(BsMatchShape { ctx, commands: parsed })
    }

    /// The x-register indices the shape touches — the context and every `integer` destination.
    pub(crate) fn registers(&self) -> Vec<u32> {
        let mut registers = Vec::with_capacity(1 + self.commands.len());
        registers.extend(self.ctx.register());
        for command in &self.commands {
            if let BsMatchCmd::Integer { dst, .. } = command {
                registers.extend(dst.register());
            }
        }
        registers
    }

    /// The x-register indices the shape **writes** — every `integer` destination. Charging
    /// them is mandatory: a dropped write lets a stale cache register survive an exit.
    pub(crate) fn writes(&self) -> Vec<u32> {
        self.commands
            .iter()
            .filter_map(|command| match command {
                BsMatchCmd::Integer { dst, .. } => dst.register(),
                _ => None,
            })
            .collect()
    }

    /// The x-register indices the shape **reads** — the context only.
    pub(crate) fn reads(&self) -> Vec<u32> {
        self.ctx.register().into_iter().collect()
    }

    /// Whether the lowering reads the current frame's Y window.
    pub(crate) fn uses_y(&self) -> bool {
        self.ctx.is_y()
            || self.commands.iter().any(|command| match command {
                BsMatchCmd::Integer { dst, .. } => dst.is_y(),
                _ => false,
            })
    }
}

/// The inlinable shape of a `bs_match`, or `None` when the whole instruction must stay on the
/// family.
///
/// A `NO_FAIL` fail code **raises** on a short subject (the family's allocating reason), and
/// only a real label is inlined; it must resolve in the module's map because the fail transfer
/// needs its `(function, code)` target — an unresolved one keeps the family's exact
/// `unknown_label`.
pub(crate) fn bs_match_shape(
    labels: &LabelMap,
    module: &Module,
    fail: Label,
    ctx: &Operand,
    commands: &[Operand],
) -> Option<(BsMatchShape, (u32, u32))> {
    let shape = BsMatchShape::of(module, ctx, commands)?;
    if fail == NO_FAIL {
        return None;
    }
    let target = labels.resolve(fail).ok()?;
    Some((shape, target))
}

/// Emit `remaining = end.saturating_sub(pos)` into [`Reg::Rcx`], reading `end` from the
/// context block and `pos` from [`Reg::Rsi`] — `commands::run_commands`' own figure.
fn emit_bs_match_remaining(buffer: &mut CodeBuffer) -> Result<()> {
    let sat: CodeLabel = buffer.label();
    buffer.mov_rm(Reg::Rax, MOVE_SCRATCH, SUB_BITS_END_DISP)?;
    buffer.xor_rr(Reg::Rcx, Reg::Rcx)?;
    buffer.cmp_rr(Reg::Rax, Reg::Rsi)?;
    buffer.jcc(Cond::B, sat)?;
    buffer.mov_rr(Reg::Rcx, Reg::Rax)?;
    buffer.sub_rr(Reg::Rcx, Reg::Rsi)?;
    buffer.bind(sat)?;
    Ok(())
}

/// Emit `ensure_at_least Size Unit`'s test, branch for branch as
/// `commands::ensure_at_least` decides it, jumping to `fail_arm` on a rejection.
fn emit_bs_match_ensure_at_least(
    buffer: &mut CodeBuffer,
    size: usize,
    unit: usize,
    fail_arm: CodeLabel,
) -> Result<()> {
    if size == 0 && unit == 1 {
        // The family's redundant no-op: no test at all.
        return Ok(());
    }
    emit_bs_match_remaining(buffer)?;
    if size == 0 {
        // `remaining % unit == 0` — `unit` is a power of two, so the modulo is a mask.
        buffer.mov_ri(Reg::Rax, (unit - 1) as i64)?;
        buffer.test_rr(Reg::Rcx, Reg::Rax)?;
        return buffer.jcc(Cond::Ne, fail_arm);
    }
    buffer.mov_ri(Reg::Rax, size as i64)?;
    buffer.cmp_rr(Reg::Rcx, Reg::Rax)?;
    buffer.jcc(Cond::B, fail_arm)?;
    if unit != 1 {
        // `(remaining - size) % unit == 0`.
        buffer.sub_rr(Reg::Rcx, Reg::Rax)?;
        buffer.mov_ri(Reg::Rax, (unit - 1) as i64)?;
        buffer.test_rr(Reg::Rcx, Reg::Rax)?;
        buffer.jcc(Cond::Ne, fail_arm)?;
    }
    Ok(())
}

/// Emit one statically-decoded `bs_match` command.
fn emit_bs_match_command(
    buffer: &mut CodeBuffer,
    command: &BsMatchCmd,
    fail_arm: CodeLabel,
    fallback: CodeLabel,
    allocation: &Allocation,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    match command {
        BsMatchCmd::EnsureAtLeast { size, unit } => {
            emit_bs_match_ensure_at_least(buffer, *size, *unit, fail_arm)
        }
        BsMatchCmd::EnsureExactly { stride } => {
            // `end.saturating_sub(pos) != stride` — the remaining window, exact equality.
            emit_bs_match_remaining(buffer)?;
            buffer.mov_ri(Reg::Rax, *stride as i64)?;
            buffer.cmp_rr(Reg::Rcx, Reg::Rax)?;
            buffer.jcc(Cond::Ne, fail_arm)
        }
        BsMatchCmd::Skip { stride } => {
            // `commands::skip_cmd`: a checked advance, then the commit.
            buffer.mov_ri(Reg::Rcx, *stride as i64)?;
            buffer.add_rr(Reg::Rcx, Reg::Rsi)?;
            buffer.jcc(Cond::B, fail_arm)?;
            buffer.mov_rm(Reg::Rax, MOVE_SCRATCH, SUB_BITS_END_DISP)?;
            buffer.cmp_rr(Reg::Rcx, Reg::Rax)?;
            buffer.jcc(Cond::A, fail_arm)?;
            buffer.mov_rr(Reg::Rsi, Reg::Rcx)?;
            buffer.mov_mr(MOVE_SCRATCH, SUB_BITS_START_DISP, Reg::Rsi)
        }
        BsMatchCmd::Integer { nbits, dst } => {
            // `nbits > remaining` -> the FAIL label.
            emit_bs_match_remaining(buffer)?;
            buffer.mov_ri(Reg::Rax, *nbits as i64)?;
            buffer.cmp_rr(Reg::Rcx, Reg::Rax)?;
            buffer.jcc(Cond::B, fail_arm)?;
            // The subject's readable window, then the shared guarded extraction.
            let (base, bit_pos, num_bytes) =
                emit_subject_view(buffer, MOVE_SCRATCH, Reg::Rsi, fallback)?;
            emit_bit_extract(buffer, base, bit_pos, *nbits, num_bytes, Reg::R8, fallback)?;
            // The commit **before** the destination write (`ctx` may alias `dst`).
            buffer.mov_ri(Reg::Rcx, *nbits as i64)?;
            buffer.add_rr(Reg::Rsi, Reg::Rcx)?;
            buffer.mov_mr(MOVE_SCRATCH, SUB_BITS_START_DISP, Reg::Rsi)?;
            // `make_small(raw)` — `(raw << 4) | TAG_IMMED1_SMALL`, zero allocation.
            buffer.shl_ri(Reg::R8, 4)?;
            buffer.mov_ri(Reg::Rcx, SMALL_TAG)?;
            buffer.or_rr(Reg::R8, Reg::Rcx)?;
            dst.store_at(buffer, Y_ADDR_SCRATCH, Reg::R8, allocation, highest_touched)
        }
    }
}

/// Emit `bs_match Fail Ctx {Commands}` natively, or report that the block must helper it
/// (see [`try_inline_bs_get_position`] for the return convention).
///
/// ```text
///   <each Y destination's frame-window guard>   ; before any store
///   <the match-context guard>                   ; not a context -> fallback
///   mov  rsi, [ctx + 16]                        ; pos
///   <one sequence per command, in order>        ; each mutating one commits as it goes
///   jmp  fast_done
/// fail_arm:
///   <the linker's exit to the fail label>
/// fallback:
///   <one interpreter step>
/// ```
///
/// **Position stores go command by command**, never batched: a rejected guard therefore
/// leaves the context at the last successful command, exactly as the family's own
/// command-by-command loop does. Nothing on the accepted path allocates and nothing raises,
/// so no `emit_position_store` is needed and the context block base read by
/// [`emit_ctx_guard`] stays valid for the whole list.
#[allow(clippy::too_many_arguments)]
pub(in crate::jit::baseline::emit) fn try_inline_bs_match(
    buffer: &mut CodeBuffer,
    module: &Module,
    labels: &LabelMap,
    linker: &Linker<'_>,
    fail: Label,
    ctx: &Operand,
    commands: &[Operand],
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some((shape, target)) = bs_match_shape(labels, module, fail, ctx, commands) else {
        return Ok(false);
    };
    #[cfg(test)]
    record_inlined(SHAPE_BS_MATCH);

    let fallback: CodeLabel = buffer.label();
    let fail_arm: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();

    // Every `Y` destination's frame-window guard first, so a rejection writes nothing (at
    // most one is accepted, so the single address scratch is enough).
    for command in &shape.commands {
        if let BsMatchCmd::Integer { dst, .. } = command {
            dst.guard_store(buffer, Y_ADDR_SCRATCH, Y_ADDR_SCRATCH_2, fallback)?;
        }
    }
    emit_ctx_guard(buffer, shape.ctx, allocation, fallback, highest_touched)?;
    buffer.mov_rm(Reg::Rsi, MOVE_SCRATCH, SUB_BITS_START_DISP)?;
    for command in &shape.commands {
        emit_bs_match_command(
            buffer,
            command,
            fail_arm,
            fallback,
            allocation,
            highest_touched,
        )?;
    }
    buffer.jmp(fast_done)?;

    buffer.bind(fail_arm)?;
    linker.emit_exit(
        buffer,
        allocation,
        done,
        target.0,
        target.1,
        pc,
        ExitSpill::Required,
    )?;

    buffer.bind(fallback)?;
    emit_step_and_continue(buffer, done, allocation, func, pc)?;
    buffer.bind(fast_done)?;
    Ok(true)
}

