//! Bitstring **matching**: match entry, the fixed reads, the position ops and the
//! flattened `bs_match` command interpreter.
//!
//! Ground truth (read-only): `emu/bs_instrs.tab` (`i_bs_start_match3` `:1199`,
//! `i_bs_get_tail` `:1481`, `bs_get_position`/`bs_set_position` `:1085-1153`,
//! `i_bs_get_fixed_integer` `:1420`, `i_bs_get_fixed_binary` `:1447`, `i_bs_skip`
//! `:1512`, `bs_test_tail_imm2` `:753`) and the `bs_match` command lowering
//! `emu/generators.tab:1102-1409` with the flag table `erl_bits.h:461-465`.
//!
//! # Half-open restore pattern
//!
//! The compiler brackets a match sequence with `bs_get_position` **before** and a
//! failure handler that runs `bs_set_position` then `bs_get_tail` then raises
//! (`first8/1` in the fixture). A read opcode therefore must **not** advance the
//! context when it *fails* — it only commits the new position on success — so the
//! handler's `bs_set_position` always sees the pre-match position.
//!
//! # Command model
//!
//! `bs_match`'s `commands` operand is the raw flattened list the loader preserved
//! (`instr.rs:730`); decoding the records is this module's job. A `pos` cursor
//! advances locally through the commands and is written back into the context once
//! the whole list has succeeded. `ensure_at_least`/`ensure_exactly` only test; the
//! reads snapshot a value into a register.

use std::collections::BTreeMap;

use beam_common::Result;
use beam_loader::{Instr, Label, Module, Operand};
use beam_term::Eterm;

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::Machine;
use crate::operand::{read_operand, write_operand};
use crate::Step;

use super::{
    decode_flags, extract_bits, fail_step, is_all, make_small_checked, raw, read_size,
    read_uint_be, read_uint_le, sign_extend, small_value, BSF_ALIGNED, BSF_LITTLE, BSF_SIGNED,
};

/// Execute a bitstring **matching** instruction.
pub(super) fn exec(
    machine: &mut Machine,
    module: &Module,
    labels: &BTreeMap<Label, (u32, u32)>,
    instr: &Instr,
    func: u32,
    pc: u32,
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    let _ = (func, pc);
    match instr {
        Instr::BsStartMatch3 {
            fail,
            bin,
            live: _,
            dst,
        } => {
            let subject = read_operand(machine, bin)?;
            match enter_match(ctx, subject)? {
                Some(context) => {
                    write_operand(machine, dst, context)?;
                    Ok(Some(Step::Next))
                }
                None => fail_step(machine, module, labels, *fail, subject, ctx),
            }
        }

        Instr::BsStartMatch4 {
            fail,
            live: _,
            src,
            dst,
        } => {
            let (fail_label, resume) = resolve_match4_fail(module, fail)?;
            let subject = read_operand(machine, src)?;
            if resume {
                // `bs_start_match4 resume Ctx Dst` reduces to `move Ctx Dst`
                // (`emu/ops.tab:1251-1252`): the source is already a match context,
                // so rebinding must keep the *same* mutable context, not a copy.
                if raw::ctx_parts(subject).is_none() {
                    return fail_step(machine, module, labels, fail_label, subject, ctx);
                }
                write_operand(machine, dst, subject)?;
                return Ok(Some(Step::Next));
            }
            match enter_match(ctx, subject)? {
                Some(context) => {
                    write_operand(machine, dst, context)?;
                    Ok(Some(Step::Next))
                }
                None => fail_step(machine, module, labels, fail_label, subject, ctx),
            }
        }

        Instr::BsGetPosition {
            ctx: ctx_op,
            dst,
            live: _,
        } => {
            let (_, orig, start, end) = read_ctx(machine, ctx_op, "bs_get_position")?;
            let snapshot = raw::make_ctx(ctx, orig, start, end)?;
            write_operand(machine, dst, snapshot)?;
            Ok(Some(Step::Next))
        }

        Instr::BsSetPosition { ctx: ctx_op, pos } => {
            let (term, _, _, _) = read_ctx(machine, ctx_op, "bs_set_position")?;
            let value = read_operand(machine, pos)?;
            let new_start = position_of(value)?;
            raw::set_ctx_start(term, new_start);
            Ok(Some(Step::Next))
        }

        Instr::BsGetTail {
            ctx: ctx_op,
            dst,
            live: _,
        } => {
            // `i_bs_get_tail` builds the remaining bits from `start` to `end` and
            // deliberately leaves the position unchanged.
            let (_, orig, start, end) = read_ctx(machine, ctx_op, "bs_get_tail")?;
            let nbits = end.saturating_sub(start);
            let bytes = raw::subject_bytes(orig)?;
            let out = extract_bits(bytes, start, nbits)?;
            let value = raw::make_bits(ctx, &out, nbits)?;
            write_operand(machine, dst, value)?;
            Ok(Some(Step::Next))
        }

        Instr::BsTestTail2 {
            fail,
            ctx: ctx_op,
            bits,
        } => {
            let (term, _, start, end) = read_ctx(machine, ctx_op, "bs_test_tail2")?;
            if end.saturating_sub(start) == *bits as usize {
                Ok(Some(Step::Next))
            } else {
                fail_step(machine, module, labels, *fail, term, ctx)
            }
        }

        Instr::BsGetInteger2 {
            fail,
            ctx: ctx_op,
            live: _,
            size,
            unit,
            flags,
            dst,
        } => {
            let (term, orig, start, end) = read_ctx(machine, ctx_op, "bs_get_integer2")?;
            let size = read_size(machine, size)?;
            let Some(nbits) = size.checked_mul(*unit as usize) else {
                return fail_step(machine, module, labels, *fail, term, ctx);
            };
            if nbits > end.saturating_sub(start) {
                return fail_step(machine, module, labels, *fail, term, ctx);
            }
            if flags & BSF_ALIGNED != 0 && start % 8 != 0 {
                return fail_step(machine, module, labels, *fail, term, ctx);
            }
            if nbits > 64 {
                return Err(error::unsupported(
                    "bs_get_integer2: integer field wider than 64 bits",
                ));
            }
            let bytes = raw::subject_bytes(orig)?;
            let value = read_integer_field(bytes, start, nbits, *flags)?;
            write_operand(machine, dst, value)?;
            raw::set_ctx_start(term, start + nbits);
            Ok(Some(Step::Next))
        }

        Instr::BsGetBinary2 {
            fail,
            ctx: ctx_op,
            live: _,
            size,
            unit,
            flags: _,
            dst,
        } => {
            let (term, orig, start, end) = read_ctx(machine, ctx_op, "bs_get_binary2")?;
            let remaining = end.saturating_sub(start);
            let nbits = if is_all(module, size) {
                remaining
            } else {
                let size = read_size(machine, size)?;
                match size.checked_mul(*unit as usize) {
                    Some(nbits) => nbits,
                    None => return fail_step(machine, module, labels, *fail, term, ctx),
                }
            };
            if nbits > remaining {
                return fail_step(machine, module, labels, *fail, term, ctx);
            }
            let bytes = raw::subject_bytes(orig)?;
            let out = extract_bits(bytes, start, nbits)?;
            let value = raw::make_bits(ctx, &out, nbits)?;
            write_operand(machine, dst, value)?;
            raw::set_ctx_start(term, start + nbits);
            Ok(Some(Step::Next))
        }

        Instr::BsSkipBits2 {
            fail,
            ctx: ctx_op,
            size,
            unit,
            flags: _,
        } => {
            let (term, _, start, end) = read_ctx(machine, ctx_op, "bs_skip_bits2")?;
            let size = read_size(machine, size)?;
            let Some(nbits) = size.checked_mul(*unit as usize) else {
                return fail_step(machine, module, labels, *fail, term, ctx);
            };
            let Some(next) = start.checked_add(nbits) else {
                return fail_step(machine, module, labels, *fail, term, ctx);
            };
            if next > end {
                return fail_step(machine, module, labels, *fail, term, ctx);
            }
            raw::set_ctx_start(term, next);
            Ok(Some(Step::Next))
        }

        Instr::BsMatch {
            fail,
            ctx: ctx_op,
            commands,
        } => run_commands(machine, module, labels, *fail, ctx_op, commands, ctx),

        Instr::BsMatchString { .. } => Err(error::unsupported("bs_match_string")),

        Instr::BsGetFloat2 { .. } => Err(error::unsupported("bs_get_float2")),

        Instr::BsGetUtf8 { .. } => Err(error::unsupported("bs_get_utf8")),
        Instr::BsGetUtf16 { .. } => Err(error::unsupported("bs_get_utf16")),
        Instr::BsGetUtf32 { .. } => Err(error::unsupported("bs_get_utf32")),
        Instr::BsSkipUtf8 { .. } => Err(error::unsupported("bs_skip_utf8")),
        Instr::BsSkipUtf16 { .. } => Err(error::unsupported("bs_skip_utf16")),
        Instr::BsSkipUtf32 { .. } => Err(error::unsupported("bs_skip_utf32")),

        _ => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Match entry
// ---------------------------------------------------------------------------

/// Start (or resume) a match over `subject`, yielding the match context.
///
/// A heap binary becomes a fresh context at position 0 (`i_bs_start_match3`,
/// `bs_instrs.tab:1199`); a term that is already an `ErlSubBits` is re-wrapped with
/// its current position (the C `reuse` path). Anything else is a *match* failure
/// (`None`), not an error.
fn enter_match(ctx: &mut dyn ExecutionContext, subject: Eterm) -> Result<Option<Eterm>> {
    if let Some((_, bits)) = raw::heap_bits(subject) {
        return Ok(Some(raw::make_ctx(ctx, subject, 0, bits)?));
    }
    if let Some((orig, start, end)) = raw::ctx_parts(subject) {
        return Ok(Some(raw::make_ctx(ctx, orig, start, end)?));
    }
    Ok(None)
}

/// Decode `bs_start_match4`'s `fail` operand: a label, or the atoms
/// `no_fail`/`resume` (`emu/ops.tab:1243-1262`).
fn resolve_match4_fail(module: &Module, fail: &Operand) -> Result<(Label, bool)> {
    match fail {
        Operand::Label(label) => Ok((*label, false)),
        Operand::Atom(atom) => match module.atom_name(*atom) {
            Some("no_fail") => Ok((beam_loader::NO_FAIL, false)),
            Some("resume") => Ok((beam_loader::NO_FAIL, true)),
            Some(other) => Err(error::unsupported(&format!(
                "bs_start_match4: fail atom {other}"
            ))),
            None => Err(error::unsupported("bs_start_match4: unknown fail atom")),
        },
        _ => Err(error::unsupported("bs_start_match4: unsupported fail operand")),
    }
}

/// Read a match-context operand, returning `(term, orig, start, end)`.
fn read_ctx(
    machine: &Machine,
    operand: &Operand,
    what: &str,
) -> Result<(Eterm, Eterm, usize, usize)> {
    let term = read_operand(machine, operand)?;
    let (orig, start, end) = raw::ctx_parts(term).ok_or_else(|| {
        error::unsupported(&format!("{what}: operand is not a match context"))
    })?;
    Ok((term, orig, start, end))
}

/// The bit position a `bs_set_position` operand denotes.
///
/// The engine's `bs_get_position` snapshots the whole context, so the ordinary
/// input is another `ErlSubBits`; the C 64-bit lowering instead stores the raw
/// position as a small integer (`bs_instrs.tab:1125-1129`), so that form is
/// accepted too.
fn position_of(value: Eterm) -> Result<usize> {
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

/// Extract an integer field of `nbits` bits at bit `start`, honouring the
/// `BSF_LITTLE`/`BSF_SIGNED` flags, and build the resulting term.
fn read_integer_field(bytes: &[u8], start: usize, nbits: usize, flags: u32) -> Result<Eterm> {
    let raw_value = if flags & BSF_LITTLE != 0 {
        read_uint_le(bytes, start, nbits)?
    } else {
        read_uint_be(bytes, start, nbits)?
    };
    let value = if flags & BSF_SIGNED != 0 {
        sign_extend(raw_value, nbits)
    } else {
        raw_value
    };
    make_small_checked(value as i64)
}

// ---------------------------------------------------------------------------
// The `bs_match` command interpreter
// ---------------------------------------------------------------------------

/// Execute a `bs_match` command list against the context in `ctx_op`.
fn run_commands(
    machine: &mut Machine,
    module: &Module,
    labels: &BTreeMap<Label, (u32, u32)>,
    fail: Label,
    ctx_op: &Operand,
    commands: &[Operand],
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    let (term, orig, start, end) = read_ctx(machine, ctx_op, "bs_match")?;
    let bytes = raw::subject_bytes(orig)?;
    let mut pos = start;
    let mut i = 0usize;
    while i < commands.len() {
        let Operand::Atom(command) = &commands[i] else {
            return Err(error::unsupported(
                "bs_match: command list does not start with an atom",
            ));
        };
        let name = module
            .atom_name(*command)
            .ok_or_else(|| error::unsupported("bs_match: unknown command atom"))?;
        match name {
            // `ensure_at_least Size Unit` (`generators.tab:1164`).
            "ensure_at_least" => {
                let size = read_size(machine, cmd_at(commands, i + 1)?)?;
                let unit = read_size(machine, cmd_at(commands, i + 2)?)?;
                if !ensure_at_least(end.saturating_sub(pos), size, unit) {
                    return fail_step(machine, module, labels, fail, term, ctx);
                }
                i += 3;
            }

            // `ensure_exactly Stride` — `bs_test_tail_imm2` (`generators.tab:1210`).
            "ensure_exactly" => {
                let stride = read_size(machine, cmd_at(commands, i + 1)?)?;
                if end.saturating_sub(pos) != stride {
                    return fail_step(machine, module, labels, fail, term, ctx);
                }
                i += 2;
            }

            // `skip Stride` — advance without binding (`i_bs_skip`).
            "skip" => {
                let stride = read_size(machine, cmd_at(commands, i + 1)?)?;
                let Some(next) = pos.checked_add(stride) else {
                    return fail_step(machine, module, labels, fail, term, ctx);
                };
                if next > end {
                    return fail_step(machine, module, labels, fail, term, ctx);
                }
                pos = next;
                i += 2;
            }

            // `integer Live Flags Size Unit Dst` (`i_bs_get_fixed_integer`).
            "integer" => {
                let flags = decode_flags(module, cmd_at(commands, i + 2)?)?;
                let size = read_size(machine, cmd_at(commands, i + 3)?)?;
                let unit = read_size(machine, cmd_at(commands, i + 4)?)?;
                let dst = cmd_at(commands, i + 5)?;
                let Some(nbits) = size.checked_mul(unit) else {
                    return fail_step(machine, module, labels, fail, term, ctx);
                };
                if nbits > end.saturating_sub(pos) {
                    return fail_step(machine, module, labels, fail, term, ctx);
                }
                if nbits > 64 {
                    return Err(error::unsupported(
                        "bs_match integer: field wider than 64 bits",
                    ));
                }
                let value = read_integer_field(bytes, pos, nbits, flags)?;
                write_operand(machine, dst, value)?;
                pos += nbits;
                i += 6;
            }

            // `binary Live Flags Size Unit Dst` (`i_bs_get_fixed_binary`).
            "binary" => {
                let size_op = cmd_at(commands, i + 3)?;
                let unit = read_size(machine, cmd_at(commands, i + 4)?)?;
                let dst = cmd_at(commands, i + 5)?;
                let nbits = if is_all(module, size_op) {
                    end.saturating_sub(pos)
                } else {
                    let size = read_size(machine, size_op)?;
                    match size.checked_mul(unit) {
                        Some(nbits) => nbits,
                        None => return fail_step(machine, module, labels, fail, term, ctx),
                    }
                };
                if nbits > end.saturating_sub(pos) {
                    return fail_step(machine, module, labels, fail, term, ctx);
                }
                let out = extract_bits(bytes, pos, nbits)?;
                let value = raw::make_bits(ctx, &out, nbits)?;
                write_operand(machine, dst, value)?;
                pos += nbits;
                i += 6;
            }

            // `get_tail Live Unit Dst` — bind the whole remaining bits
            // (`i_bs_get_tail`); the position is left unchanged (it is the last
            // command, `bs_translate_fixup`).
            "get_tail" => {
                let dst = cmd_at(commands, i + 3)?;
                let nbits = end.saturating_sub(pos);
                let out = extract_bits(bytes, pos, nbits)?;
                let value = raw::make_bits(ctx, &out, nbits)?;
                write_operand(machine, dst, value)?;
                i += 4;
            }

            // `'=:=' Nil Bits Value` — compare the next bits with a constant.
            "'=:='" => {
                return Err(error::unsupported("bs_match: '=:=' command"));
            }

            other => {
                return Err(error::unsupported(&format!(
                    "bs_match: command {other}"
                )));
            }
        }
    }
    // Commit the advanced position only once the whole command list succeeded.
    raw::set_ctx_start(term, pos);
    Ok(Some(Step::Next))
}

/// The `i`-th command operand, or a typed error when the list is truncated.
fn cmd_at(commands: &[Operand], index: usize) -> Result<&Operand> {
    commands
        .get(index)
        .ok_or_else(|| error::unsupported("bs_match: truncated command list"))
}

/// The `ensure_at_least` test (`generators.tab:1164-1206`):
/// * `Size != 0`, `Unit == 1` ⇒ `i_bs_ensure_bits` (`remaining >= Size`);
/// * `Size != 0`, `Unit != 1` ⇒ `i_bs_ensure_bits_unit` (`remaining >= Size` **and**
///   the surplus is a whole number of units);
/// * `Size == 0`, `Unit == 8` ⇒ `i_bs_test_unit8` (byte alignment only);
/// * `Size == 0`, `Unit != 1` ⇒ `i_bs_test_unit` (alignment to `Unit`);
/// * `Size == 0`, `Unit == 1` ⇒ a redundant no-op.
fn ensure_at_least(remaining: usize, size: usize, unit: usize) -> bool {
    if size == 0 {
        if unit == 8 {
            remaining % 8 == 0
        } else if unit == 1 {
            true
        } else {
            remaining % unit == 0
        }
    } else if unit == 1 {
        remaining >= size
    } else {
        remaining >= size && (remaining - size) % unit == 0
    }
}
