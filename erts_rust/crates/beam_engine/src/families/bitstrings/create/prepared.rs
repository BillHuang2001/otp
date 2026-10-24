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

//! The **prepared** `bs_create_bin` segment program.
//!
//! [`super::assemble`] walks the flat `[Type, Index, Unit, Flags, Value, Size]` operand
//! groups the compiler emits and re-decodes every descriptor on every execution: a
//! `module.atom_name` plus a `&str` match per segment, four bounds-checked operand
//! reads, the flags decode, the size/unit read and the `$BS_SAFE_MUL` validation. A
//! compiled binary-comprehension loop re-enters that same instruction once per element,
//! so on `tools/bench`'s `binary_append` (five segments × 16 384 iterations) the decode
//! — not the writing — is the dominant cost.
//!
//! [`prepare`] resolves all of it **once per `(module image, function, code offset)`**
//! into a [`CreateProgram`]: one [`PreparedSeg`] per segment, with
//!
//! * the type atom folded into a [`PreparedSeg`] variant (no name comparison at all),
//! * the flags decoded to the `BSF_*` bits,
//! * the segment width (`size * unit`, the C `$BS_SAFE_MUL` already applied) reduced to
//!   a plain `usize`,
//! * a `string` segment's `StrT` window resolved and range-checked, and
//! * the value/destination operands kept as they were (they are what varies per
//!   execution).
//!
//! Execution then collapses to reading the source values, the arithmetic and the writes.
//!
//! # Nothing is fabricated
//!
//! [`prepare`] is a **pure function of the loaded module's IR plus its atom/string
//! tables** — never of a run, a machine or a host hook — and it never substitutes a
//! value. A shape it cannot fully pre-resolve (a non-atom type word, an unknown segment
//! type, a truncated group, a *dynamic* unit operand, a `string` value that is not a
//! `StrT` offset, a `string`/`integer` size that is `all`, a width whose `$BS_SAFE_MUL`
//! overflows, a `float` width that is not 16/32/64) answers `None`. The caller then
//! files the instruction as [`Program::Generic`](super::super::programs::Program) and
//! runs [`super::assemble`], the unchanged generic walk, which reproduces the **exact**
//! typed error (or dynamic behaviour) it always produced. A *dynamically sized* segment
//! (`<<V:N>>` with a computed `N`) is not a refusal: [`Width::Dynamic`] keeps the size
//! operand and the exec arm derives the width per execution, exactly as before.
//!
//! Equivalence between the two routes is pinned by `create/prepared_tests.rs`.

use crate::interpreter::LabelMap;

use beam_common::Result;
use beam_loader::{Label, Module, Operand};

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::Machine;
use crate::predicates;
use crate::Step;

use super::segments::{float_source, put_float, put_utf16, put_utf32, put_utf8};
use super::size::{fail_seg, float_nbits, seg_nbits, segment_size, SegReason, SegSize};
use super::read_value;
use crate::families::bitstrings::{
    decode_flags, is_all, push_wide_int, raw, small_value, BitWriter, Built, BSF_LITTLE,
};

/// The six operands one segment group is made of (`[Type, Index, Unit, Flags, Value,
/// Size]`).
const SEGMENT_OPERANDS: usize = 6;

/// One resolved segment of a `bs_create_bin`.
pub(crate) enum PreparedSeg {
    /// `integer Flags Value Size` — a numeric field. The width is `size * unit` with
    /// the C `$BS_SAFE_MUL` + signed-fit check already applied.
    Integer {
        flags: u32,
        value: Operand,
        width: Width,
    },
    /// `binary Unit Flags Value Size` — a bitstring operand's bits.
    Binary {
        unit: usize,
        value: Operand,
        width: Width,
    },
    /// `string Unit Flags Offset Size` — a constant `StrT` window, resolved and
    /// range-checked, so it can no longer fail.
    String { start: usize, end: usize },
    /// `float Unit Flags Value Size` — the width is a validated 16/32/64 (or dynamic).
    Float {
        little: bool,
        value: Operand,
        width: Width,
    },
    /// `utf8`/`utf16`/`utf32` — the `Size`/`Unit` operands are unused; only `utf16`
    /// consults the endianness flag.
    Utf {
        kind: UtfKind,
        little: bool,
        value: Operand,
    },
    /// `append`/`private_append` — concatenate the source's bits. `private` is the
    /// writable-accumulator form, which is written **in place** when it is the first
    /// segment of a pristine writer.
    Append {
        unit: usize,
        value: Operand,
        private: bool,
    },
}

/// Which Unicode encoding a `utf*` segment writes.
pub(crate) enum UtfKind {
    Utf8,
    Utf16,
    Utf32,
}

/// A segment's field width in bits.
pub(crate) enum Width {
    /// `size * unit`, already validated (`$BS_SAFE_MUL` and the signed-fit check) — the
    /// shape every compiler-emitted fixed-width segment has.
    Imm(usize),
    /// The whole remaining source (`all`) — `binary` only.
    All,
    /// A runtime `size` operand: the width is derived per execution through
    /// [`segment_size`]/[`seg_nbits`], exactly as the generic walk does.
    Dynamic { size: Operand, unit: usize },
}

/// A resolved `bs_create_bin` segment program.
pub(crate) struct CreateProgram {
    pub(super) segments: Vec<PreparedSeg>,
}

/// Resolve `segments` into a program, or `None` when the shape must stay on the generic
/// walk (see the module docs).
///
/// Pure: it reads the module's atom table, literal area and `StrT` table, and the
/// instruction's own operands. No run state is consulted.
pub(super) fn prepare(module: &Module, segments: &[Operand]) -> Option<CreateProgram> {
    if segments.len() % SEGMENT_OPERANDS != 0 {
        // The generic walk reports the truncation (or the stray operand) with its own
        // typed error.
        return None;
    }
    let mut prepared = Vec::with_capacity(segments.len() / SEGMENT_OPERANDS);
    for group in segments.chunks_exact(SEGMENT_OPERANDS) {
        prepared.push(prepare_segment(module, group)?);
    }
    Some(CreateProgram { segments: prepared })
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

/// Resolve one `[Type, Index, Unit, Flags, Value, Size]` group.
fn prepare_segment(module: &Module, group: &[Operand]) -> Option<PreparedSeg> {
    let Operand::Atom(type_atom) = &group[0] else {
        return None;
    };
    let kind = module.atom_name(*type_atom)?;
    // The generic walk reads the unit and the flags for **every** segment before it
    // dispatches on the type, so a malformed one is refused here the same way (by
    // declining, so the generic walk reports it).
    let unit = static_size(&group[2])?;
    let flags = decode_flags(module, &group[3]).ok()?;
    let value_op = &group[4];
    let size_op = &group[5];
    match kind {
        "integer" => {
            let width = match static_size(size_op) {
                Some(size) => Width::Imm(seg_nbits(size, unit).ok()?),
                // `<<V:all>>` is the generic arm's typed error; decline so it reports it.
                None if is_all(module, size_op) => return None,
                None => Width::Dynamic {
                    size: size_op.clone(),
                    unit,
                },
            };
            Some(PreparedSeg::Integer {
                flags,
                value: value_op.clone(),
                width,
            })
        }
        "binary" => {
            let width = if is_all(module, size_op) {
                Width::All
            } else if let Some(size) = static_size(size_op) {
                Width::Imm(seg_nbits(size, unit).ok()?)
            } else {
                Width::Dynamic {
                    size: size_op.clone(),
                    unit,
                }
            };
            Some(PreparedSeg::Binary {
                unit,
                value: value_op.clone(),
                width,
            })
        }
        "string" => {
            // The value operand is the string's byte offset into the module's `StrT`
            // table (`beam_asm.erl:771-773` writes it as an untagged integer) and the
            // size operand its byte count; everything about it is static, so resolve
            // the window now — including the range check the exec arm would do.
            let Operand::Integer(offset) = value_op else {
                return None;
            };
            if is_all(module, size_op) {
                return None;
            }
            let len = static_size(size_op)?;
            let start = usize::try_from(*offset).ok()?;
            let end = start.checked_add(len)?;
            module.strings.get(start..end)?;
            Some(PreparedSeg::String { start, end })
        }
        "float" => {
            let width = if is_all(module, size_op) {
                // C's default width for the (hand-built only) `all` form.
                Width::Imm(64)
            } else if let Some(size) = static_size(size_op) {
                Width::Imm(float_nbits(size, unit).ok()?)
            } else {
                Width::Dynamic {
                    size: size_op.clone(),
                    unit,
                }
            };
            Some(PreparedSeg::Float {
                little: flags & BSF_LITTLE != 0,
                value: value_op.clone(),
                width,
            })
        }
        "utf8" | "utf16" | "utf32" => {
            let kind = match kind {
                "utf8" => UtfKind::Utf8,
                "utf16" => UtfKind::Utf16,
                _ => UtfKind::Utf32,
            };
            Some(PreparedSeg::Utf {
                kind,
                little: flags & BSF_LITTLE != 0,
                value: value_op.clone(),
            })
        }
        "append" | "private_append" => Some(PreparedSeg::Append {
            unit,
            value: value_op.clone(),
            private: kind == "private_append",
        }),
        // An unknown segment type is the generic walk's typed error.
        _ => None,
    }
}

/// Execute a prepared segment program — [`
/// super::assemble`]'s hot counterpart.
///
/// The failure surface is the generic walk's, arm for arm: the same `$BS_FAIL_INFO`
/// reason per condition, the same order (a `binary` segment still checks the source's
/// shape *before* its size), and the same in-place `private_append` arm at position `0`.
pub(super) fn assemble_prepared(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    fail: Label,
    program: &CreateProgram,
    ctx: &mut dyn ExecutionContext,
) -> Result<std::result::Result<Built, Step>> {
    let mut writer = BitWriter::default();
    for (position, segment) in program.segments.iter().enumerate() {
        match segment {
            PreparedSeg::Append {
                unit,
                value,
                private,
            } => {
                let source = read_value(machine, module, ctx, value)?;
                let Some((_, src_bits, _)) = raw::heap_bits(source) else {
                    return fail_seg(machine, module, labels, fail, SegReason::BadArg);
                };
                // `erts_bs_append_checked` (`erl_bits.c:1877`): the source's bit size
                // must be a multiple of the segment's unit.
                if *unit != 1 && src_bits % *unit != 0 {
                    return fail_seg(machine, module, labels, fail, SegReason::BadArg);
                }
                // The compiler's writable accumulator: `private_append` in the **first**
                // position over a payload this machine allocates for appending. See
                // `create/mod.rs` — the window only ever grows past bits no alias can
                // read, so the mutation is unobservable.
                if *private && position == 0 && writer.is_pristine() {
                    if let Some((base, capacity_bits, end_bits)) =
                        raw::writable_append_target(machine, source)
                    {
                        writer = BitWriter::in_place(raw::InPlace::new(
                            base,
                            capacity_bits,
                            end_bits,
                            source,
                        ));
                        continue;
                    }
                }
                let (bytes, src_off) = raw::subject_bytes(source)?;
                writer.push_bits_from(bytes, src_off, src_bits, machine)?;
            }
            PreparedSeg::Integer {
                flags,
                value,
                width,
            } => {
                let nbits = match width {
                    Width::Imm(nbits) => *nbits,
                    Width::Dynamic { size, unit } => {
                        let size = match segment_size(machine, ctx, size)? {
                            SegSize::Small(n) => n,
                            SegSize::Fail(reason) => {
                                return fail_seg(machine, module, labels, fail, reason)
                            }
                        };
                        match seg_nbits(size, *unit) {
                            Ok(nbits) => nbits,
                            Err(reason) => {
                                return fail_seg(machine, module, labels, fail, reason)
                            }
                        }
                    }
                    // `prepare` declines an `integer` segment sized `all`, so this is
                    // the generic arm's typed error kept as the same answer rather than
                    // a fabricated width.
                    Width::All => {
                        return Err(error::unsupported(
                            "bs_create_bin: integer segment size `all`",
                        ))
                    }
                };
                let value = read_value(machine, module, ctx, value)?;
                // A value that is neither a small nor a big is the bare `badarg` at
                // every field width (`erts_bs_put_integer_be`/`_le` return 0 for a
                // non-integer, `erl_bits.c:952-955`/`:1152-1155`).
                if !predicates::is_integer(value) {
                    return fail_seg(machine, module, labels, fail, SegReason::BadArg);
                }
                let little = flags & BSF_LITTLE != 0;
                if nbits > 64 || raw::big_parts_safe(value).is_some() {
                    push_wide_int(&mut writer, value, nbits, little, machine)?;
                } else if let Some(raw_value) = small_value(value) {
                    if little {
                        writer.push_uint_le(raw_value as u64, nbits, machine)?;
                    } else {
                        // `erts_bs_put_integer_*` masks the value into the field width.
                        writer.push_uint(raw_value as u64, nbits, machine)?;
                    }
                } else {
                    return fail_seg(machine, module, labels, fail, SegReason::BadArg);
                }
            }
            PreparedSeg::Binary {
                unit,
                value,
                width,
            } => {
                // The size loop's `is_not_bitstring(Src)` check fails **before** the
                // size is even read.
                let Some((bytes, src_off, src_bits)) = segment_source(machine, module, ctx, value)?
                else {
                    return fail_seg(machine, module, labels, fail, SegReason::BadArg);
                };
                let nbits = match width {
                    // `erts_bs_put_binary_all` (`erl_bits.c:1311`): a whole-tail
                    // segment must be a multiple of the segment's unit.
                    Width::All => {
                        if *unit != 1 && src_bits % *unit != 0 {
                            return fail_seg(machine, module, labels, fail, SegReason::BadArg);
                        }
                        src_bits
                    }
                    Width::Imm(nbits) => *nbits,
                    Width::Dynamic { size, unit } => {
                        let size = match segment_size(machine, ctx, size)? {
                            SegSize::Small(n) => n,
                            SegSize::Fail(reason) => {
                                return fail_seg(machine, module, labels, fail, reason)
                            }
                        };
                        match seg_nbits(size, *unit) {
                            Ok(nbits) => nbits,
                            Err(reason) => {
                                return fail_seg(machine, module, labels, fail, reason)
                            }
                        }
                    }
                };
                if nbits > src_bits {
                    return fail_seg(machine, module, labels, fail, SegReason::BadArg);
                }
                writer.push_bits_from(bytes, src_off, nbits, machine)?;
            }
            PreparedSeg::String { start, end } => {
                // Resolved and range-checked by `prepare`, and the module's string table
                // is immutable, so the only way to miss here is a corrupted program —
                // reported as the generic walk's own typed error, never a wrong byte
                // sequence.
                let bytes = module.strings.get(*start..*end).ok_or_else(|| {
                    error::unsupported("bs_create_bin: string segment outside the StrT table")
                })?;
                writer.push_bits_from(bytes, 0, bytes.len() * 8, machine)?;
            }
            PreparedSeg::Float {
                little,
                value,
                width,
            } => {
                let nbits = match width {
                    Width::Imm(nbits) => *nbits,
                    Width::All => 64,
                    Width::Dynamic { size, unit } => {
                        let size = match segment_size(machine, ctx, size)? {
                            SegSize::Small(n) => n,
                            SegSize::Fail(reason) => {
                                return fail_seg(machine, module, labels, fail, reason)
                            }
                        };
                        match float_nbits(size, *unit) {
                            Ok(nbits) => nbits,
                            Err(reason) => {
                                return fail_seg(machine, module, labels, fail, reason)
                            }
                        }
                    }
                };
                let value = read_value(machine, module, ctx, value)?;
                let Some(f) = float_source(value) else {
                    return fail_seg(machine, module, labels, fail, SegReason::BadArg);
                };
                let bytes = put_float(f, nbits, *little);
                writer.push_bits_from(&bytes, 0, nbits, machine)?;
            }
            PreparedSeg::Utf {
                kind,
                little,
                value,
            } => {
                let value = read_value(machine, module, ctx, value)?;
                let encoded = match (kind, small_value(value)) {
                    (UtfKind::Utf8, Some(v)) => put_utf8(v),
                    (UtfKind::Utf16, Some(v)) => put_utf16(v, *little),
                    (UtfKind::Utf32, Some(v)) => put_utf32(v),
                    // A non-small value (a bignum) can never be a scalar.
                    _ => None,
                };
                match encoded {
                    Some(bytes) => writer.push_bits_from(&bytes, 0, bytes.len() * 8, machine)?,
                    None => return fail_seg(machine, module, labels, fail, SegReason::BadArg),
                }
            }
        }
    }
    Ok(Ok(writer.finish_built()))
}

/// The `binary` segment's source: the byte slice, window bit offset and bit count the
/// value operand draws from, or `None` when it is not a bitstring (the C
/// `is_not_bitstring(Src)` arm).
///
/// The prepared twin of `segments::segment_source`, kept here so the prepared path owns
/// its own source reader and the generic bodies stay untouched.
#[inline]
fn segment_source<'a>(
    machine: &Machine,
    module: &Module,
    ctx: &mut dyn ExecutionContext,
    operand: &Operand,
) -> Result<Option<(&'a [u8], usize, usize)>> {
    let value = read_value(machine, module, ctx, operand)?;
    let Some((_, bits, _)) = raw::heap_bits(value) else {
        return Ok(None);
    };
    let (bytes, off) = raw::subject_bytes(value)?;
    Ok(Some((bytes, off, bits)))
}
