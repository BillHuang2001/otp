//! Bitstring **construction**: `bs_create_bin` (and the `bs_init_writable` gap).
//!
//! Ground truth (read-only): `emu/bs_instrs.tab` (`i_bs_create_bin` `:415`,
//! `i_bs_private_append`), the segment table `emu/generators.tab` and the flag bits
//! `erl_bits.h:461-465`; the encoders `erl_bits.c` (`erts_bs_put_utf8` `:1162`,
//! `erts_bs_put_utf16` `:1222`, `erts_bs_put_float` `:1341`, `f64_to_f16` `:431`).
//!
//! # Segment list model
//!
//! The loader keeps the segment descriptors **flat**: groups of six operands
//! `[TypeAtom, Index, Unit, Flags, Value, Size]` (the compiler's `bs_args/3` layout
//! as `beam_asm` writes it). The compiler emits groups such as
//! `{atom,integer},1,1,nil,{x,0},{integer,8}` and
//! `{atom,binary},2,8,nil,{x,0},{atom,all}`; a `string` constant is its own
//! `{string,Bs}` value operand.
//!
//! # Scope
//!
//! Implemented: `integer` (any width ≤ 64 bits, big- or little-endian, values masked
//! into the field), `binary` and `string` segments (byte-aligned, `all` or an
//! explicit bit count), `float` (16/32/64 bits, big- or little-endian), and
//! `utf8`/`utf16`/`utf32`, combined into one result bitstring.
//!
//! Not implemented (each a precise typed [`crate::error::unsupported`]): `append`,
//! `private_append` and `bs_init_writable` (they need a mutable off-heap
//! ref-counted binary the term layer does not provide), results larger than the
//! on-heap binary limit, and non-small-integer segment values.

use std::collections::BTreeMap;

use beam_common::Result;
use beam_loader::{Instr, Label, Module, Operand};
use beam_term::Eterm;

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::Machine;
use crate::operand::{read_operand, write_operand};
use crate::Step;

use super::{decode_flags, fail_step, is_all, raw, read_size, small_value, BitWriter, BSF_LITTLE};

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
        // `bs_init_writable` (`i_bs_init_writable`, `bs_instrs.tab:285`) allocates a
        // writable off-heap ref-counted binary and a match context over it, so a
        // following `bs_create_bin` with an `append`/`private_append` segment can grow
        // it in place. Neither the mutable-binary allocation nor the term layer's
        // ref-counted binary exists yet, so this is a documented gap.
        Instr::BsInitWritable => Err(error::unsupported(
            "bs_init_writable: needs a writable off-heap ref-counted binary",
        )),
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
        // Each segment is `[Type, Index, Unit, Flags, Value, Size]`; the index is
        // ignored (it is only a compiler-side identifier).
        let unit = read_size(machine, seg_at(segments, i + 2)?)?;
        let flags = decode_flags(module, seg_at(segments, i + 3)?)?;
        let value_op = seg_at(segments, i + 4)?;
        let size_op = seg_at(segments, i + 5)?;

        match kind {
            "integer" => {
                if is_all(module, size_op) {
                    return Err(error::unsupported(
                        "bs_create_bin: integer segment size `all`",
                    ));
                }
                let size = read_size(machine, size_op)?;
                let Some(nbits) = size.checked_mul(unit) else {
                    return fail_seg(machine, module, labels, fail, value_op, ctx);
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
                    match size.checked_mul(unit) {
                        Some(nbits) => nbits,
                        None => {
                            return fail_seg(machine, module, labels, fail, value_op, ctx);
                        }
                    }
                };
                if nbits > src_bits {
                    return fail_seg(machine, module, labels, fail, value_op, ctx);
                }
                writer.push_bits_from(bytes, 0, nbits);
            }

            // `float Unit Fs Value Size` — write a 16/32/64-bit float
            // (`erts_bs_put_float`, `erl_bits.c:1341`). `Size` is the bit count
            // directly (already `size * unit`), defaulting to 64.
            "float" => {
                let nbits = if is_all(module, size_op) {
                    64
                } else {
                    match read_size(machine, size_op)?.checked_mul(unit) {
                        Some(n) if n != 0 => n,
                        _ => 64,
                    }
                };
                if !matches!(nbits, 16 | 32 | 64) {
                    return Err(error::unsupported(
                        "bs_create_bin: float segment size other than 16/32/64",
                    ));
                }
                let value = read_value(machine, module, value_op)?;
                let Some(f) = float_source(value) else {
                    return Err(error::unsupported(
                        "bs_create_bin: float segment value is not a float/small integer",
                    ));
                };
                let bytes = put_float(f, nbits, flags & BSF_LITTLE != 0);
                writer.push_bits_from(&bytes, 0, nbits);
            }

            // `utf8`/`utf16`/`utf32` — the codepoint is variable-width; the
            // `Size`/`Unit` operands are not used (only the endianness flag is).
            "utf8" | "utf16" | "utf32" => {
                let value = read_value(machine, module, value_op)?;
                let encoded = match (kind, small_value(value)) {
                    ("utf8", Some(v)) => put_utf8(v),
                    ("utf16", Some(v)) => put_utf16(v, flags & BSF_LITTLE != 0),
                    ("utf32", Some(v)) => put_utf32(v),
                    // A non-small value (a bignum) can never be a scalar.
                    _ => None,
                };
                match encoded {
                    Some(bytes) => writer.push_bits_from(&bytes, 0, bytes.len() * 8),
                    None => return fail_seg(machine, module, labels, fail, value_op, ctx),
                }
            }

            // `append`/`private_append` (BSC_APPEND/BSC_PRIVATE_APPEND) need a
            // writable in-place binary (see `bs_init_writable`); they fall through to
            // the generic unsupported-segment-type error.
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

/// Build the segment failure `Step` for `value_op` (a completed `Err(step)`).
#[allow(clippy::type_complexity)]
fn fail_seg(
    machine: &mut Machine,
    module: &Module,
    labels: &BTreeMap<Label, (u32, u32)>,
    fail: Label,
    value_op: &Operand,
    ctx: &mut dyn ExecutionContext,
) -> Result<std::result::Result<(Vec<u8>, usize), Step>> {
    let value = read_value(machine, module, value_op)?;
    let step = fail_step(machine, module, labels, fail, value, ctx)?
        .expect("fail_step yields a step");
    Ok(Err(step))
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

/// The `f64` a float-segment value denotes: an Erlang float, or a small integer
/// (a bignum would need `big_to_double`, which is not available).
fn float_source(value: Eterm) -> Option<f64> {
    if let Some(f) = crate::terms::float_value(value).ok() {
        return Some(f);
    }
    small_value(value).map(|n| n as f64)
}

// ---------------------------------------------------------------------------
// Encoders (`erl_bits.c`)
// ---------------------------------------------------------------------------

/// `erts_bs_put_float` (`erl_bits.c:1341`) for a byte-aligned destination: the
/// `nbits`-bit big- or little-endian representation of `f`.
fn put_float(f: f64, nbits: usize, little: bool) -> Vec<u8> {
    let mut bytes = match nbits {
        16 => f64_to_f16(f).to_be_bytes().to_vec(),
        32 => (f as f32).to_bits().to_be_bytes().to_vec(),
        _ => f.to_bits().to_be_bytes().to_vec(),
    };
    if little {
        bytes.reverse();
    }
    bytes
}

/// `f64_to_f16` (`erl_bits.c:431`): round-to-nearest-even conversion of a finite
/// double to IEEE-754 binary16, ported line for line.
fn f64_to_f16(fp: f64) -> u16 {
    let u64bits = fp.to_bits();
    let sign = ((u64bits >> 48) & 0x8000) as u16;
    let exp = ((u64bits >> 52) & 0x7FF) as i32;
    let mut man = u64bits & ((1u64 << 52) - 1);
    if exp == 0 {
        // Subnormal doubles are far below the binary16 subnormal range: they round
        // to zero.
        return sign;
    }
    let mut e = exp - 1023;
    man |= 1u64 << 52; // Full 53-bit significand.
    if e >= -14 {
        if e > 15 {
            return sign | 0x7C00; // Overflow becomes infinity.
        }
        let mut k = man >> 42; // 11 bits: 1024..2047
        let guard = (man >> 41) & 1;
        let sticky = man & ((1u64 << 41) - 1) != 0;
        if guard != 0 && (sticky || (k & 1) != 0) {
            k += 1;
            if k == 2048 {
                k = 1024;
                e += 1;
                if e > 15 {
                    return sign | 0x7C00;
                }
            }
        }
        return sign | ((e + 15) as u16) << 10 | (k as u16 & 0x3FF);
    }
    // Subnormal value in binary16: RNE(|fp| * 2^24).
    let shift = 28 - e; // e <= -15, so shift >= 43.
    if shift > 63 {
        return sign; // Underflow to zero.
    }
    let shift = shift as u32;
    let mut keep = man >> shift;
    let guard = (man >> (shift - 1)) & 1;
    let low = man & ((1u64 << (shift - 1)) - 1);
    let sticky = low != 0;
    if guard != 0 && (sticky || (keep & 1) != 0) {
        keep += 1;
    }
    sign | keep as u16
}

/// `erts_bs_put_utf8` (`erl_bits.c:1162`): encode a non-negative scalar as UTF-8.
/// `None` for a negative value, a surrogate, or a value ≥ U+110000.
fn put_utf8(value: i64) -> Option<Vec<u8>> {
    if value < 0 || value >= 0x110000 {
        return None;
    }
    let v = value as u32;
    if (0x800..0x10000).contains(&v) && (0xD800..=0xDFFF).contains(&v) {
        return None;
    }
    let mut out = Vec::with_capacity(4);
    if v < 0x80 {
        out.push(v as u8);
    } else if v < 0x800 {
        out.push(0xC0 | (v >> 6) as u8);
        out.push(0x80 | (v & 0x3F) as u8);
    } else if v < 0x10000 {
        out.push(0xE0 | (v >> 12) as u8);
        out.push(0x80 | ((v >> 6) & 0x3F) as u8);
        out.push(0x80 | (v & 0x3F) as u8);
    } else {
        out.push(0xF0 | (v >> 18) as u8);
        out.push(0x80 | ((v >> 12) & 0x3F) as u8);
        out.push(0x80 | ((v >> 6) & 0x3F) as u8);
        out.push(0x80 | (v & 0x3F) as u8);
    }
    Some(out)
}

/// `erts_bs_put_utf16` (`erl_bits.c:1222`): encode a scalar as UTF-16, big- or
/// little-endian. `None` for a value > U+10FFFF or a surrogate.
fn put_utf16(value: i64, little: bool) -> Option<Vec<u8>> {
    if value < 0 || value > 0x10FFFF {
        return None;
    }
    let v = value as u32;
    if (0xD800..=0xDFFF).contains(&v) {
        return None;
    }
    let mut words: Vec<u16> = Vec::with_capacity(2);
    if v < 0x10000 {
        words.push(v as u16);
    } else {
        let t = v - 0x10000;
        words.push(0xD800 | (t >> 10) as u16);
        words.push(0xDC00 | (t & 0x3FF) as u16);
    }
    let mut out = Vec::with_capacity(words.len() * 2);
    for w in words {
        if little {
            out.extend_from_slice(&w.to_le_bytes());
        } else {
            out.extend_from_slice(&w.to_be_bytes());
        }
    }
    Some(out)
}

/// `erts_bs_put_utf32`: encode a scalar as 32-bit big-endian. `None` for a value
/// > U+10FFFF or a surrogate.
fn put_utf32(value: i64) -> Option<Vec<u8>> {
    if value < 0 || value > 0x10FFFF {
        return None;
    }
    let v = value as u32;
    if (0xD800..=0xDFFF).contains(&v) {
        return None;
    }
    Some(v.to_be_bytes().to_vec())
}
