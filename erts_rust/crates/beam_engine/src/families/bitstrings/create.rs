//! Bitstring **construction**: `bs_create_bin` (and the `bs_init_writable` gap).
//!
//! Ground truth (read-only): `emu/bs_instrs.tab` (`i_bs_create_bin` `:415`,
//! `i_bs_private_append`), the segment table `emu/generators.tab` and the flag bits
//! `erl_bits.h:461-465`.
//!
//! # Segment list model
//!
//! The loader keeps the segment descriptors **flat**: groups of six operands
//! `[TypeAtom, Segment, Unit, Flags, Value, Size]` (`lib/compiler/src/beam_ssa_codegen.erl`
//! `bs_args/3`). The compiler emits:
//! * `{integer, Value, Size}` ⇒ `integer` (an `N`-bit field, `N = Size * Unit`);
//! * `{binary, Value, Size}` ⇒ `binary` (`all` or `Size * Unit` bits);
//! * a string literal ⇒ `string` (the value operand resolves to a **literal**
//!   heap binary, or the atom `""` when the literal is empty).
//!
//! # Scope
//!
//! Implemented: `integer` (any width ≤ 64 bits, big- or little-endian, signed
//! values masked into the field), `binary` and `string` segments (byte-aligned, `all`
//! or an explicit bit count), combined into one result bitstring.
//!
//! Not implemented (each a precise typed [`crate::error::unsupported`]): `float`,
//! `utf8`/`utf16`/`utf32`, `append`/`private_append`, results larger than the on-heap
//! binary limit, and non-small-integer segment values.

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
    decode_flags, fail_step, is_all, raw, read_size, small_value, BitWriter, BSF_LITTLE,
};

/// Execute a bitstring **construction** instruction.
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
        Instr::BsInitWritable => Err(error::unsupported("bs_init_writable")),
        Instr::BsCreateBin {
            fail,
            alloc: _,
            live: _,
            unit: _,
            dst,
            segments,
        } => {
            let (bytes, nbits) = match assemble(machine, module, labels, *fail, segments, ctx)? {
                Ok(parts) => parts,
                // `assemble` already produced the failure `Step`.
                Err(step) => return Ok(Some(step)),
            };
            let value = raw::make_bits(ctx, &bytes, nbits)?;
            write_operand(machine, dst, value)?;
            if *fail != beam_loader::NO_FAIL {
                // The compiler emits `{f,0}`, but a live fail label is harmless: a
                // successful build simply falls through.
            }
            Ok(Some(Step::Next))
        }
        _ => Ok(None),
    }
}

/// Walk the flat segment list, appending to one bit vector.
///
/// `Err(step)` is a completed failure `Step` (a branch to `fail`, or a raise) — the
/// caller returns it verbatim.
#[allow(clippy::type_complexity)]
fn assemble(
    machine: &mut Machine,
    module: &Module,
    labels: &BTreeMap<Label, (u32, u32)>,
    fail: Label,
    segments: &[Operand],
    ctx: &mut dyn ExecutionContext,
) -> Result<std::result::Result<(Vec<u8>, usize), Step>> {
    let mut writer = BitWriter::default();
    let mut i = 0usize;
    while i < segments.len() {
        let Operand::Atom(type_atom) = &segments[i] else {
            return Err(error::unsupported(
                "bs_create_bin: segment list does not start with a type atom",
            ));
        };
        let kind = module
            .atom_name(*type_atom)
            .ok_or_else(|| error::unsupported("bs_create_bin: unknown segment type atom"))?;
        let unit = read_size(machine, seg_at(segments, i + 2)?)?;
        let flags = decode_flags(module, seg_at(segments, i + 3)?)?;
        let value_op = seg_at(segments, i + 4)?;
        let size_op = seg_at(segments, i + 5)?;

        match kind {
            "integer" => {
                if is_all(module, size_op) {
                    return Err(error::unsupported("bs_create_bin: integer segment size `all`"));
                }
                let size = read_size(machine, size_op)?;
                let Some(nbits) = size.checked_mul(unit) else {
                    return Ok(Err(fail_step(
                        machine,
                        module,
                        labels,
                        fail,
                        read_value(machine, module, value_op)?,
                        ctx,
                    )?
                    .expect("fail_step yields a step")));
                };
                if nbits > 64 {
                    return Err(error::unsupported(
                        "bs_create_bin: integer segment wider than 64 bits",
                    ));
                }
                let value = read_value(machine, module, value_op)?;
                let Some(raw_value) = small_value(value) else {
                    return Err(error::unsupported(
                        "bs_create_bin: integer segment value is not a small integer",
                    ));
                };
                if flags & BSF_LITTLE != 0 {
                    writer.push_uint_le(raw_value as u64, nbits);
                } else {
                    // `erts_bs_put_integer_*` masks the value into the field width.
                    writer.push_uint(raw_value as u64, nbits);
                }
            }

            "binary" | "string" => {
                let (bytes, src_bits) = segment_source(machine, module, value_op, kind)?;
                let nbits = if is_all(module, size_op) {
                    src_bits
                } else {
                    let size = read_size(machine, size_op)?;
                    let Some(nbits) = size.checked_mul(unit) else {
                        return Ok(Err(fail_step(
                            machine,
                            module,
                            labels,
                            fail,
                            read_value(machine, module, value_op)?,
                            ctx,
                        )?
                        .expect("fail_step yields a step")));
                    };
                    nbits
                };
                if nbits > src_bits {
                    return Ok(Err(fail_step(
                        machine,
                        module,
                        labels,
                        fail,
                        read_value(machine, module, value_op)?,
                        ctx,
                    )?
                    .expect("fail_step yields a step")));
                }
                writer.push_bits_from(bytes, 0, nbits);
            }

            other => {
                return Err(error::unsupported(&format!(
                    "bs_create_bin: segment type {other}"
                )))
            }
        }
        i += 6;
    }
    Ok(Ok(writer.finish()))
}

/// The `i`-th segment operand, or a typed error when the list is truncated.
fn seg_at(segments: &[Operand], index: usize) -> Result<&Operand> {
    segments
        .get(index)
        .ok_or_else(|| error::unsupported("bs_create_bin: truncated segment list"))
}

/// Read a segment value operand: a `Literal` (a string constant) resolves through
/// the module's literal area, everything else through the ordinary operand reader.
fn read_value(machine: &Machine, module: &Module, operand: &Operand) -> Result<Eterm> {
    match operand {
        Operand::Literal(index) => module
            .literal(*index)
            .ok_or_else(|| error::unsupported("bs_create_bin: literal index out of range")),
        _ => read_operand(machine, operand),
    }
}

/// The byte slice and bit count a `binary`/`string` segment draws from.
///
/// A `string` segment whose value is the empty-string atom (the compiler's
/// "no string table" form) contributes nothing.
fn segment_source<'a>(
    machine: &Machine,
    module: &Module,
    operand: &Operand,
    kind: &str,
) -> Result<(&'a [u8], usize)> {
    if kind == "string" {
        if let Operand::Atom(_) = operand {
            return Ok((&[], 0));
        }
    }
    let value = read_value(machine, module, operand)?;
    let (_, bits) = raw::heap_bits(value)
        .ok_or_else(|| error::unsupported("bs_create_bin: segment value is not a binary"))?;
    let bytes = raw::subject_bytes(value)?;
    Ok((bytes, bits))
}
