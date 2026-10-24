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

//! The **prepared** `bs_match` command program.
//!
//! [`super::commands::run_commands`] runs the flat command list the compiler emits
//! **without decoding it**: per command it checks that the operand is an atom, resolves
//! that atom's name through [`Module::atom_name`], compares the name as a `&str`, reads
//! the size/unit operands, decodes the flags operand and validates
//! `size * unit` — and a compiled binary-comprehension loop re-enters that very
//! instruction once per element (16 384 times on `tools/bench`'s `binary_append`, whose
//! command list is two commands). The decode is what that loop pays for, not the read.
//!
//! [`prepare`] resolves all of it **once per `(module image, function, code offset,
//! descriptor list)`** into a [`MatchProgram`]: one [`MatchCmd`] per command, with
//!
//! * the command atom folded into a [`MatchCmd`] variant (no name comparison at all),
//! * every size/unit operand resolved to a plain `usize` and `size * unit` already
//!   computed (the C `$BS_SAFE_MUL` applied at prepare time),
//! * the flags decoded to the `BSF_*` bits, and
//! * the destination operand kept as it was (it is what varies per execution).
//!
//! # The straight-line fast form
//!
//! The shape *every* binary comprehension (and this benchmark) runs is a run of
//! `ensure_at_least Bits Unit` + `integer Live Flags Size Unit Dst` over a constant
//! bit count, with the implicit commit after each command. For a program made **only**
//! of such runs [`prepare`] additionally derives a [`FastForm`]: the whole command walk
//! collapses into one straight line per element — one bounds check on the subject
//! window, one whole-byte big-endian load, one `x(Dst)` store and one context-position
//! store (plus, when the previous element's field did not fit a small integer, the one
//! re-read that field's own allocation forces).
//!
//! **The shape decision is made at prepare time**, so no execution carries a shape
//! predicate: a run-time shape test inside the family was measured a *regression* on a
//! neighbouring workload (`to_lower`, 9/9 paired rounds), and the fields are all
//! statically known here anyway.
//!
//! A field is only admitted to the fast form when it is a whole number of bytes at most
//! eight bytes wide, **unsigned** and **big-endian** — the general case, and exactly
//! what [`super::reads::read_integer_field_tracked`] answers with its whole-byte
//! `read_uint_be` arm and `make_int`. Every other field (signed, little/native — resolved
//! through [`super::reads::field_is_little`], the single place that decides byte order —
//! a non-byte width, or wider than 8 bytes) keeps the prepared command walk, which is
//! the generic walk's own body per command.
//!
//! # Nothing is fabricated
//!
//! [`prepare`] is a **pure function of the loaded module's IR plus its atom table** —
//! never of a run, a machine or a host hook — and it never substitutes a value. A shape
//! it cannot fully pre-resolve (a non-atom command word, an unknown command name, a
//! truncated list, a *dynamic* size/unit operand, a `size * unit` whose `$BS_SAFE_MUL`
//! overflows) answers `None`. The caller then files the instruction as
//! [`Program::Generic`](super::super::programs::Program) and runs
//! [`super::commands::run_commands`], the unchanged generic walk, which reproduces the
//! **exact** typed error (or dynamic behaviour) it always produced.
//!
//! # Sharing the command bodies
//!
//! The prepared walk does not re-implement a command: it calls the very bodies the
//! generic walk calls ([`super::commands::skip_cmd`], `binary_cmd`, `get_tail_cmd`,
//! `eq_cmd`, [`super::commands::commit`]) with the operands *already resolved*, so the
//! two routes cannot drift apart in semantics — only in where the size/unit/destination
//! operands were read. The one command whose read is inlined on both routes is
//! `integer`, because it is the hot one: both routes call
//! [`super::reads::read_integer_field_tracked`], and the fast form calls the very same
//! whole-byte `read_uint_be` + `make_int` pair that reader uses for an unsigned
//! big-endian field.
//!
//! Equivalence between the routes is pinned by `prepared_tests.rs`.

use crate::interpreter::LabelMap;

use beam_common::Result;
use beam_loader::{Label, Module, Operand};

use crate::context::ExecutionContext;
use crate::machine::Machine;
use crate::operand::write_operand;
use crate::Step;

use super::super::programs::{program_key, Program};
use super::super::{decode_flags, fail_step, is_all, make_int, raw, read_uint_be, BSF_SIGNED};
use super::commands::{
    binary_cmd, commit, ensure_at_least, eq_cmd, get_tail_cmd, skip_cmd, BinaryWidth, CmdFlow,
};
use super::match_ctx::read_ctx;
use super::reads::{field_is_little, may_have_allocated, read_integer_field_tracked};

/// One fully resolved `bs_match` command.
///
/// The size/unit operands are already folded into `nbits`/`stride`/`size`/`unit`
/// (plain `usize`s), and the command atom is the variant itself — so executing a
/// command costs no atom lookup, no `&str` comparison and no operand read for its
/// static fields.
pub(crate) enum MatchCmd {
    /// `ensure_at_least Size Unit` (`i_bs_ensure_bits`/`i_bs_test_unit`).
    EnsureAtLeast { size: usize, unit: usize },
    /// `ensure_exactly Stride` (`i_bs_test_tail2`).
    EnsureExactly { stride: usize },
    /// `skip Stride` (`i_bs_skip`).
    Skip { stride: usize },
    /// `integer Live Flags Size Unit Dst` (`i_bs_get_fixed_integer`), its width
    /// `size * unit` already validated.
    Integer {
        flags: u32,
        nbits: usize,
        dst: Operand,
    },
    /// `binary Live Flags Size Unit Dst` (`i_bs_get_fixed_binary`); `nbits` is `None`
    /// for the `all` form (the whole remaining window).
    Binary { nbits: Option<usize>, dst: Operand },
    /// `get_tail Live Unit Dst` (`i_bs_get_tail`).
    GetTail { dst: Operand },
    /// `'=:=' Nil Bits Value` (`i_bs_eq`). `Value` stays an operand: it is where a
    /// non-integer value is reported, at execution time, exactly as C does.
    Eq { bits: usize, value: Operand },
}

/// One straight-line field of a [`FastForm`].
pub(crate) struct FastField {
    /// The `ensure_at_least Size Unit` that guards this field, if the command list had
    /// one directly before it.
    ensure: Option<(usize, usize)>,
    /// The field's width in bits: a whole number of bytes, at most eight.
    nbits: usize,
    /// Where the value is bound.
    dst: Operand,
}

/// The **straight-line fast form** of a whole program: a sequence of unsigned
/// big-endian whole-byte fields, each optionally guarded by one `ensure_at_least`.
pub(crate) struct FastForm {
    fields: Vec<FastField>,
}

/// A resolved `bs_match` command program.
pub(crate) struct MatchProgram {
    /// The command list, one resolved [`MatchCmd`] per command.
    pub(super) cmds: Vec<MatchCmd>,
    /// The straight-line form of the whole program, when every command is part of a
    /// `(ensure_at_least, integer)` run.
    fast: Option<FastForm>,
}

/// How many operands each command's flattened group holds (the command word included).
fn group_len(name: &str) -> Option<usize> {
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

/// The `i`-th command operand, or `None` when the list is truncated (the generic walk
/// then reports its own typed error).
fn at(commands: &[Operand], index: usize) -> Option<&Operand> {
    commands.get(index)
}

/// The accept rule of a *static* size/unit operand: the value
/// [`read_size`](crate::families::bitstrings::read_size) would produce for a literal
/// (`Operand::Integer`/`Operand::Character`), or `None` for anything that needs a
/// runtime read.
#[inline]
fn static_size(operand: &Operand) -> Option<usize> {
    match operand {
        Operand::Integer(n) if *n >= 0 && beam_term::try_make_small(*n).is_ok() => Some(*n as usize),
        Operand::Character(c) => Some(*c as usize),
        _ => None,
    }
}

/// Resolve `commands` into a program, or `None` when the shape must stay on the generic
/// walk (see the module docs).
///
/// Pure: it reads the module's atom table and the instruction's own operands. No run
/// state is consulted, so caching the result cannot go stale while the image lives.
pub(super) fn prepare(module: &Module, commands: &[Operand]) -> Option<MatchProgram> {
    let mut cmds = Vec::with_capacity(commands.len() / 2 + 1);
    let mut i = 0usize;
    while i < commands.len() {
        let Operand::Atom(command) = &commands[i] else {
            // The generic walk's own typed error.
            return None;
        };
        let name = module.atom_name(*command)?;
        group_len(name)?;
        cmds.push(prepare_command(module, name, commands, i)?);
        i += group_len(name)?;
    }
    let fast = fast_form(&cmds);
    Some(MatchProgram { cmds, fast })
}

/// Resolve the command whose word is `commands[i]` and whose name is `name`.
fn prepare_command(
    module: &Module,
    name: &str,
    commands: &[Operand],
    i: usize,
) -> Option<MatchCmd> {
    match name {
        "ensure_at_least" => Some(MatchCmd::EnsureAtLeast {
            size: static_size(at(commands, i + 1)?)?,
            unit: static_size(at(commands, i + 2)?)?,
        }),
        "ensure_exactly" => Some(MatchCmd::EnsureExactly {
            stride: static_size(at(commands, i + 1)?)?,
        }),
        "skip" => Some(MatchCmd::Skip {
            stride: static_size(at(commands, i + 1)?)?,
        }),
        "integer" => {
            // The generic walk reads the flags, the size, the unit and only then the
            // destination, and validates `size * unit` afterwards; a malformed operand
            // of any of them declines here so the generic walk reports it.
            let flags = decode_flags(module, at(commands, i + 2)?).ok()?;
            let size = static_size(at(commands, i + 3)?)?;
            let unit = static_size(at(commands, i + 4)?)?;
            let dst = at(commands, i + 5)?;
            Some(MatchCmd::Integer {
                flags,
                nbits: size.checked_mul(unit)?,
                dst: dst.clone(),
            })
        }
        "binary" => {
            // The unit is read before the size is examined (so a malformed unit is
            // refused even for `all`), and the size is either `all` or a static width.
            let unit = static_size(at(commands, i + 4)?)?;
            let size_op = at(commands, i + 3)?;
            let dst = at(commands, i + 5)?;
            let nbits = if is_all(module, size_op) {
                None
            } else {
                Some(static_size(size_op)?.checked_mul(unit)?)
            };
            Some(MatchCmd::Binary {
                nbits,
                dst: dst.clone(),
            })
        }
        "get_tail" => Some(MatchCmd::GetTail {
            dst: at(commands, i + 3)?.clone(),
        }),
        "=:=" => Some(MatchCmd::Eq {
            bits: static_size(at(commands, i + 2)?)?,
            value: at(commands, i + 3)?.clone(),
        }),
        _ => None,
    }
}

/// Derive the straight-line form of `cmds`, or `None` when the program is anything but
/// a run of `(ensure_at_least, integer)` pairs with unsigned big-endian whole-byte
/// fields (see the module docs).
fn fast_form(cmds: &[MatchCmd]) -> Option<FastForm> {
    let mut fields = Vec::with_capacity(cmds.len());
    let mut pending: Option<(usize, usize)> = None;
    for cmd in cmds {
        match cmd {
            MatchCmd::EnsureAtLeast { size, unit } => {
                // Two ensures in a row is not the shape this form models.
                if pending.is_some() {
                    return None;
                }
                pending = Some((*size, *unit));
            }
            MatchCmd::Integer { flags, nbits, dst } if fast_eligible(*flags, *nbits) => {
                fields.push(FastField {
                    ensure: pending.take(),
                    nbits: *nbits,
                    dst: dst.clone(),
                });
            }
            // Any other command (or an ineligible field) keeps the whole program on the
            // prepared command walk.
            _ => return None,
        }
    }
    // A dangling `ensure_at_least` would be a test with no reader: not this shape.
    if pending.is_some() || fields.is_empty() {
        return None;
    }
    Some(FastForm { fields })
}

/// Whether a field may take the straight-line form: a whole number of bytes, at most
/// eight, **unsigned** and **big-endian**.
///
/// The byte-order question is asked through [`field_is_little`] — the same single
/// resolution point every other reader uses — so a `native`-flagged field is admitted
/// exactly when the target's own order *is* big-endian.
#[inline]
fn fast_eligible(flags: u32, nbits: usize) -> bool {
    flags & BSF_SIGNED == 0 && !field_is_little(flags) && nbits % 8 == 0 && nbits <= 64
}

/// Execute a `bs_match` command list through the prepared-program cache.
///
/// The program is fetched from the current module context's
/// [`BitstrPrograms`](super::super::programs::BitstrPrograms) table — handed over **by
/// value** for the duration of this instruction, so the body keeps the `&mut Machine`
/// the term builders need — and prepared on a miss.
///
/// A **synthetic** image (all-zero [`Module::checksum`], the documented marker of an
/// image with no content identity — `machine/label_cache.rs`) takes the generic walk
/// and is not cached at all: `(function, code offset)` is not an identity there, so a
/// hit could answer for another instruction's descriptor list. A loaded `.beam` is
/// cached by that key, which *is* its identity.
pub(super) fn run_commands_prepared(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    fail: Label,
    ctx_op: &Operand,
    commands: &[Operand],
    func: u32,
    pc: u32,
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    if module.checksum() == [0u8; 16] {
        return super::commands::run_commands(machine, module, labels, fail, ctx_op, commands, ctx);
    }
    let mut programs = machine.take_bitstr_programs();
    let program = programs.get_or_prepare(program_key(func, pc), module, || {
        match prepare(module, commands) {
            Some(program) => Program::Match(Box::new(program)),
            None => Program::Generic,
        }
    });
    let outcome = match program {
        Program::Match(program) => match &program.fast {
            Some(fast) => {
                #[cfg(test)]
                note_fast_walk();
                run_fast(machine, module, labels, fail, ctx_op, fast, ctx)
            }
            None => {
                #[cfg(test)]
                note_prepared_walk();
                run_prepared(machine, module, labels, fail, ctx_op, &program.cmds, ctx)
            }
        },
        // A shape the decode declined (or another family's program in this slot): the
        // unchanged generic walk, which reproduces the exact typed error it always did.
        Program::Generic | Program::Create(_) => {
            super::commands::run_commands(machine, module, labels, fail, ctx_op, commands, ctx)
        }
    };
    machine.restore_bitstr_programs(programs);
    outcome
}

/// Execute the **straight-line** fast form.
///
/// One bounds check on the subject window, one whole-byte big-endian load, one `x(Dst)`
/// store and one context-position store per field, in the C's own order: the position is
/// committed **before** the destination is bound (it may be the context's own operand),
/// and the capture taken at entry is re-derived only after a field that actually
/// reserved through the rooted seam (`may_have_allocated`, the same witness the generic
/// walk elides on).
fn run_fast(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    fail: Label,
    ctx_op: &Operand,
    fast: &FastForm,
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    let (mut term, orig, start, mut end) = read_ctx(machine, ctx, ctx_op, "bs_match")?;
    let (mut bytes, mut off) = raw::subject_bytes(orig)?;
    let mut pos = start;
    for field in &fast.fields {
        if let Some((size, unit)) = field.ensure {
            if !ensure_at_least(end.saturating_sub(pos), size, unit) {
                return fail_step(machine, module, labels, fail, ctx_op, ctx);
            }
        }
        if field.nbits > end.saturating_sub(pos) {
            return fail_step(machine, module, labels, fail, ctx_op, ctx);
        }
        #[cfg(test)]
        note_fast_field();
        // The field is unsigned big-endian and a whole number of bytes, so this is
        // exactly [`read_integer_field_tracked`]'s whole-byte arm
        // (`read_uint_be` then `make_int` with a false sign), without re-deciding the
        // order and the sign per execution.
        let raw_value = read_uint_be(bytes, pos + off, field.nbits)?;
        let value = make_int(machine, ctx, false, &[raw_value])?;
        pos += field.nbits;
        // Only a field that does not fit a small integer reserved through the rooted
        // seam, and only such a reservation can have relocated the capture above.
        if may_have_allocated(value) {
            let (fresh, fresh_orig, _, fresh_end) = read_ctx(machine, ctx, ctx_op, "bs_match")?;
            let (fresh_bytes, fresh_off) = raw::subject_bytes(fresh_orig)?;
            term = fresh;
            end = fresh_end;
            bytes = fresh_bytes;
            off = fresh_off;
        }
        commit(machine, ctx, ctx_op, term, false, pos)?;
        write_operand(machine, &field.dst, value)?;
    }
    Ok(Some(Step::Next))
}

/// Execute a resolved command list — the generic walk's semantics, one typed command at
/// a time.
///
/// The allocation-aware elision is the generic walk's own: the context term, its inner
/// subject and the subject's byte view are captured once and re-derived only after a
/// command that may have reserved through the rooted seam ([`CmdFlow::Relocated`], or an
/// `integer` field that did not fit a small integer).
fn run_prepared(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    fail: Label,
    ctx_op: &Operand,
    cmds: &[MatchCmd],
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    let (mut term, orig, start, mut end) = read_ctx(machine, ctx, ctx_op, "bs_match")?;
    let (mut bytes, mut off) = raw::subject_bytes(orig)?;
    let mut pos = start;
    // `true` when the previous command may have relocated the context block.
    let mut stale = false;
    for cmd in cmds {
        if stale {
            let (fresh, fresh_orig, _, fresh_end) = read_ctx(machine, ctx, ctx_op, "bs_match")?;
            let (fresh_bytes, fresh_off) = raw::subject_bytes(fresh_orig)?;
            term = fresh;
            end = fresh_end;
            bytes = fresh_bytes;
            off = fresh_off;
            stale = false;
        }
        match cmd {
            MatchCmd::EnsureAtLeast { size, unit } => {
                if !ensure_at_least(end.saturating_sub(pos), *size, *unit) {
                    return fail_step(machine, module, labels, fail, ctx_op, ctx);
                }
            }
            MatchCmd::EnsureExactly { stride } => {
                if end.saturating_sub(pos) != *stride {
                    return fail_step(machine, module, labels, fail, ctx_op, ctx);
                }
            }
            MatchCmd::Skip { stride } => {
                match skip_cmd(machine, ctx, ctx_op, *stride, term, &mut pos, end)? {
                    CmdFlow::Fail => return fail_step(machine, module, labels, fail, ctx_op, ctx),
                    CmdFlow::Done | CmdFlow::Relocated => {}
                }
            }
            MatchCmd::Integer { flags, nbits, dst } => {
                if *nbits > end.saturating_sub(pos) {
                    return fail_step(machine, module, labels, fail, ctx_op, ctx);
                }
                let (value, allocated) =
                    read_integer_field_tracked(machine, ctx, bytes, pos + off, *nbits, *flags)?;
                pos += *nbits;
                // The C advances `sb->start` *before* it binds `$Dst`, so the advance is
                // mirrored first: `Dst` may be the context's own operand.
                commit(machine, ctx, ctx_op, term, allocated, pos)?;
                write_operand(machine, dst, value)?;
                stale = allocated;
            }
            MatchCmd::Binary { nbits, dst } => {
                let width = match nbits {
                    Some(nbits) => BinaryWidth::Static(*nbits),
                    None => BinaryWidth::All,
                };
                match binary_cmd(machine, ctx, width, dst, &mut pos, end, bytes, off, term, ctx_op)?
                {
                    CmdFlow::Fail => return fail_step(machine, module, labels, fail, ctx_op, ctx),
                    CmdFlow::Done => {}
                    CmdFlow::Relocated => stale = true,
                }
            }
            MatchCmd::GetTail { dst } => {
                match get_tail_cmd(machine, ctx, dst, pos, end, bytes, off)? {
                    CmdFlow::Fail => return fail_step(machine, module, labels, fail, ctx_op, ctx),
                    CmdFlow::Done => {}
                    CmdFlow::Relocated => stale = true,
                }
            }
            MatchCmd::Eq { bits, value } => {
                match eq_cmd(machine, ctx, *bits, value, &mut pos, end, bytes, off, term, ctx_op)? {
                    CmdFlow::Fail => return fail_step(machine, module, labels, fail, ctx_op, ctx),
                    CmdFlow::Done | CmdFlow::Relocated => {}
                }
            }
        }
    }
    Ok(Some(Step::Next))
}

// ---------------------------------------------------------------------------
// Test-only structural instruments
//
// Which route a `bs_match` actually took is not observable from its result (the whole
// point is that the routes agree), so `prepared_tests.rs` pins it with per-thread
// counters — the same instrument shape `decode_flags_slow_calls` and `bitstrings::exec_calls`
// use. Compiled out entirely outside `cfg(test)`, so a production build carries none.
// ---------------------------------------------------------------------------

#[cfg(test)]
thread_local! {
    static FAST_WALKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PREPARED_WALKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static FAST_FIELDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn note_fast_walk() {
    FAST_WALKS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
fn note_prepared_walk() {
    PREPARED_WALKS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
fn note_fast_field() {
    FAST_FIELDS.with(|count| count.set(count.get() + 1));
}

/// `(fast-form walks, prepared-command walks, fast fields read)` **on this thread**.
#[cfg(test)]
pub(super) fn route_counts() -> (usize, usize, usize) {
    (
        FAST_WALKS.with(std::cell::Cell::get),
        PREPARED_WALKS.with(std::cell::Cell::get),
        FAST_FIELDS.with(std::cell::Cell::get),
    )
}

#[cfg(test)]
#[path = "prepared_tests.rs"]
mod prepared_tests;
