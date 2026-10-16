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

//! Bitstring **construction**: `bs_create_bin` and `bs_init_writable`.
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
//! `{atom,binary},2,8,nil,{x,0},{atom,all}`; a `string` constant's value operand is
//! written as an untagged integer byte offset into the module's `StrT` string table
//! (`beam_asm.erl:771-773`), which the loader decodes to `Operand::Integer`.
//!
//! # Scope
//!
//! Implemented: `integer` (any width, big- or little-endian, the value masked into
//! the field — a **small** value up to 64 bits through the machine-word path, any
//! value at a wider field *or* a **bignum** at any width through the bignum path
//! [`super::push_wide_int`]), `binary` (byte-aligned or not, `all` or an
//! explicit bit count), `string` constants from the `StrT` table, `float` (16/32/64
//! bits, big- or little-endian), `utf8`/`utf16`/`utf32`, an
//! `append`/`private_append` segment in **any** position (its source's bits are
//! concatenated at the point it appears), and `bs_init_writable` — combined into one
//! result bitstring of any size (an on-heap `ErlHeapBits` up to the 64-byte limit, an
//! off-heap refc binary above it; see [`raw::make_binary`]).
//!
//! # `append` / `private_append`: copy-rebuild, not in-place growth
//!
//! C's `private_append` (and `append` on a writable source) *reallocates and extends*
//! the source's off-heap payload in place and returns a new `ErlSubBits` aliasing it,
//! so a loop that appends `n` times is amortised O(n). The engine's payloads are
//! immutable ([`beam_alloc::RefcBinary`] has no grow operation and `BinRef.val` has no
//! setter), so an append instead **seeds a bit vector with the source's bits, appends
//! the remaining segments, and materialises a fresh term** — the same bits C's
//! `not_writable` arm copies (`erl_bits.c:1855-1924`). The result bytes are identical
//! for every successful build (so this is observationally correct), but a long append
//! loop is O(n²) rather than amortised O(n). Three further C observables are
//! deliberately unmodelled in M1: the `BIN_FLAG_WRITABLE`/`BIN_FLAG_ACTIVE_WRITER`
//! flags on the underlying binary, `binary:referenced_byte_size/1`'s "capacity vs
//! size" answer, and the fact that a `private_append` result lets a *later*
//! `private_append` reuse the same buffer (the engine's result is an ordinary,
//! non-writable refc binary — which the next append simply seeds from).
//!
//! # `BS_FAIL_INFO`: why a segment failure raises a bare reason atom
//!
//! C's segment-failure macro `BS_FAIL_INFO(Fail, Reason, ErrorType, Value)`
//! (`emu/bs_instrs.tab:347`) runs `erts_prepare_bs_construct_fail_info` and then
//! `$FAIL_HEAD_OR_BODY($Fail)` (`emu/macros.tab:266`) — **not** the bare `$FAIL`
//! the matching opcodes use. A live fail label is `$FAIL($Fail)`, a branch to it
//! (the guard case); `{f,0}` ([`NO_FAIL`]) takes the `goto find_func_info` arm, a
//! real raise whose reason is exactly the atom `BS_FAIL_INFO` left in
//! `c_p->freason` — the bare `badarg` for every `i_bs_create_bin` call site, or
//! `system_limit` from the size validation's `BS_FAIL_INFO_SYSTEM_LIMIT`
//! (`:355`, `:424`/`:427`/`:438`). Both are modelled, as [`SegReason`]; the extended
//! error info `erts_prepare_bs_construct_fail_info` records (`emu/beam_emu.c:725`)
//! only decorates the *stacktrace*, so it is not modelled at M1: the reason is the
//! bare atom. See [`fail_seg`] — this is why `bs_create_bin` must **not** reuse the
//! matching side's [`super::fail_step`], whose `NO_FAIL` arm raises
//! `{badmatch, Subject}`.

mod segments;
mod size;

use crate::interpreter::LabelMap;

use beam_common::Result;
use beam_loader::{Instr, Label, Module, Operand};
use beam_term::Eterm;

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::Machine;
use crate::operand::{read_operand, write_operand};
use crate::Step;

use super::{
    decode_flags, is_all, push_wide_int, raw, read_size, small_value, BitWriter, BSF_LITTLE,
};

use self::segments::{
    float_source, put_float, put_utf16, put_utf32, put_utf8, segment_source, string_source,
};
use self::size::{fail_seg, float_nbits, seg_nbits, segment_size, SegReason, SegSize};

/// `erts_bs_init_writable`'s default payload capacity (`erl_bits.c:2023`):
/// `bin_size = 1024` unless `x(0)` holds a non-negative small integer.
const DEFAULT_WRITABLE_SIZE: usize = 1024;
/// Execute a bitstring **construction** instruction.
pub(super) fn exec(
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
        // `bs_init_writable` (`i_bs_init_writable`, `emu/bs_instrs.tab:285`) is the
        // compiler's accumulator opener: C does `x(0) = erts_bs_init_writable(c_p,
        // x(0))` — the size hint is in `x(0)`, and the result replaces it. ERTS
        // allocates the ref-counted payload with that capacity but reports a
        // **zero-length** window (`build_writable_bitstring` passes
        // `current_size = 0`), so the term is a 0-bit refc binary that the first
        // `bs_create_bin` `append`/`private_append` segment extends.
        Instr::BsInitWritable => {
            let capacity = match machine.x(0).and_then(small_value) {
                Some(size) if size >= 0 => size as usize,
                _ => DEFAULT_WRITABLE_SIZE,
            };
            let value = raw::make_writable(machine, ctx, capacity)?;
            machine.set_x(0, value)?;
            Ok(Some(Step::Next))
        }
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
            let value = raw::make_binary(machine, ctx, &bytes, nbits)?;
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
    labels: &LabelMap,
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
        let unit = read_size(machine, ctx, seg_at(segments, i + 2)?)?;
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
                let size = match segment_size(machine, ctx, size_op)? {
                    SegSize::Small(n) => n,
                    SegSize::Fail(reason) => return fail_seg(machine, module, labels, fail, reason),
                };
                // C's `$BS_SAFE_MUL(..., $BS_FAIL_INFO_SYSTEM_LIMIT, ...)`
                // (`emu/bs_instrs.tab:424`) plus the signed-size-fits check (`:425`):
                // a `size*unit` overflow, or a product whose signed value does not fit
                // in a word, is `system_limit` (`:427`).
                let nbits = match seg_nbits(size, unit) {
                    Ok(nbits) => nbits,
                    Err(reason) => return fail_seg(machine, module, labels, fail, reason),
                };
                let value = read_value(machine, module, ctx, value_op)?;
                // `fmt_big_be`/`fmt_big_le` (`erl_bits.c:724`/`:811`) take the value
                // **modulo `2^nbits`** for a wide field of a small *or* a bignum value
                // (a negative value becomes its nbits-bit two's complement, `SUBc` on
                // the build side). `erts_bs_put_integer_*` masks a **narrow** field the
                // same way, so a bignum into a `<= 64`-bit field routes here too.
                if nbits > 64 || raw::big_parts_safe(value).is_some() {
                    push_wide_int(&mut writer, value, nbits, flags & BSF_LITTLE != 0)?;
                } else if let Some(raw_value) = small_value(value) {
                    if flags & BSF_LITTLE != 0 {
                        writer.push_uint_le(raw_value as u64, nbits);
                    } else {
                        // `erts_bs_put_integer_*` masks the value into the field width.
                        writer.push_uint(raw_value as u64, nbits);
                    }
                } else {
                    // `erts_bs_put_integer_*` rejects a genuine non-integer value
                    // (a float, atom, …) (`erl_bits.c:926`) -> `BADARG`.
                    return fail_seg(machine, module, labels, fail, SegReason::BadArg);
                }
            }
            // `binary Unit Fs Value Size` — append a bitstring operand's bytes.
            "binary" => {
                // The size loop's `is_not_bitstring(Src)` check
                // (`emu/bs_instrs.tab:390`) fails **before** the size is even read.
                let Some((bytes, src_off, src_bits)) =
                    segment_source(machine, module, value_op, ctx)?
                else {
                    return fail_seg(machine, module, labels, fail, SegReason::BadArg);
                };
                let nbits = if is_all(module, size_op) {
                    // `erts_bs_put_binary_all` (`erl_bits.c:1311`): a whole-tail
                    // segment must be a multiple of the segment's unit.
                    if unit != 1 && src_bits % unit != 0 {
                        return fail_seg(machine, module, labels, fail, SegReason::BadArg);
                    }
                    src_bits
                } else {
                    let size = match segment_size(machine, ctx, size_op)? {
                        SegSize::Small(n) => n,
                        SegSize::Fail(reason) => {
                            return fail_seg(machine, module, labels, fail, reason);
                        }
                    };
                    // `$BS_SAFE_MUL` + the signed-size-fits check → `system_limit`.
                    match seg_nbits(size, unit) {
                        Ok(nbits) => nbits,
                        Err(reason) => return fail_seg(machine, module, labels, fail, reason),
                    }
                };
                if nbits > src_bits {
                    return fail_seg(machine, module, labels, fail, SegReason::BadArg);
                }
                writer.push_bits_from(bytes, src_off, nbits);
            }
            // `string Unit Fs Offset Size` — append a constant byte string from the
            // module's `StrT` table (`BS_LOAD_STRING_SRC`/`erts_bs_put_string`,
            // `bs_instrs.tab:627-633`, `erl_bits.c:1641`). The compiler emits the
            // *value* operand as the string's byte offset into `StrT`
            // (`beam_asm.erl:771-773` writes it with `tag_u`) and the *size* operand
            // as its byte count, always with unit 8
            // (`beam_ssa_codegen.erl:1976`); C computes `size * 8` bits and no
            // failure is possible here, so this segment never branches to `fail`.
            "string" => {
                let bytes = string_source(module, value_op, size_op, machine, ctx)?;
                writer.push_bits_from(bytes, 0, bytes.len() * 8);
            }

            // `float Unit Fs Value Size` — write a 16/32/64-bit float
            // (`erts_bs_put_float`, `erl_bits.c:1341`). The field width is
            // `Size * Unit`, the **same** size arm `BSC_INTEGER`/`BSC_BINARY` share
            // in C (`emu/bs_instrs.tab:415-417`): a `size*unit` overflow — or a
            // product whose signed value does not fit a word — is `system_limit`,
            // and a width other than 16/32/64 (including `Size == 0`) is `badarg`;
            // see [`float_nbits`]. `Size` is the bit *count* the compiler emits
            // with unit 1; the bare `<<F/float>>` form is emitted as the constant
            // 64, never `all`, so the `all` arm is only reachable from a
            // hand-built module (the compiler rejects `<<F/all/float>>` with a
            // syntax error) and keeps the C default of 64 bits.
            "float" => {
                let nbits = if is_all(module, size_op) {
                    64
                } else {
                    let size = match segment_size(machine, ctx, size_op)? {
                        SegSize::Small(n) => n,
                        SegSize::Fail(reason) => {
                            return fail_seg(machine, module, labels, fail, reason);
                        }
                    };
                    match float_nbits(size, unit) {
                        Ok(nbits) => nbits,
                        Err(reason) => {
                            return fail_seg(machine, module, labels, fail, reason);
                        }
                    }
                };
                let value = read_value(machine, module, ctx, value_op)?;
                let Some(f) = float_source(value) else {
                    return fail_seg(machine, module, labels, fail, SegReason::BadArg);
                };
                let bytes = put_float(f, nbits, flags & BSF_LITTLE != 0);
                writer.push_bits_from(&bytes, 0, nbits);
            }

            // `utf8`/`utf16`/`utf32` — the codepoint is variable-width; the
            // `Size`/`Unit` operands are not used (only the endianness flag is).
            "utf8" | "utf16" | "utf32" => {
                let value = read_value(machine, module, ctx, value_op)?;
                let encoded = match (kind, small_value(value)) {
                    ("utf8", Some(v)) => put_utf8(v),
                    ("utf16", Some(v)) => put_utf16(v, flags & BSF_LITTLE != 0),
                    ("utf32", Some(v)) => put_utf32(v),
                    // A non-small value (a bignum) can never be a scalar.
                    _ => None,
                };
                match encoded {
                    Some(bytes) => writer.push_bits_from(&bytes, 0, bytes.len() * 8),
                    None => return fail_seg(machine, module, labels, fail, SegReason::BadArg),
                }
            }
            // `append`/`private_append` (`BSC_APPEND`/`BSC_PRIVATE_APPEND`,
            // `bs_instrs.tab:541-569`). The compiler emits it as the **first**
            // segment, with the accumulator in the value operand and the size operand
            // `all`; an appending segment in any other position concatenates the
            // source's bits onto the segments already accumulated. C seeds the build
            // with the whole of the source bitstring (`erts_bs_append_checked` /
            // `erts_bs_private_append_checked` take `sb->end` as the starting position)
            // and then appends the remaining segments. The source may be the
            // `bs_init_writable` result (a 0-bit refc binary) or a previously built
            // refc binary, both of which [`raw::heap_bits`] reads — at whatever bit
            // offset their window starts.
            //
            // The engine seeds its bit vector from the source and rebuilds a fresh
            // term rather than growing the source's payload in place (the engine's
            // payloads are immutable; C's `not_writable` arm does the same copy). That
            // is observationally identical for a successful build; see the module docs
            // for the honest divergences.
            "append" | "private_append" => {
                let value = read_value(machine, module, ctx, value_op)?;
                let Some((_, src_bits, _)) = raw::heap_bits(value) else {
                    // `erts_bs_append_checked` (`erl_bits.c:1744`) /
                    // `erts_bs_private_append_checked` reject a non-bitstring
                    // accumulator with `BADARG`.
                    return fail_seg(machine, module, labels, fail, SegReason::BadArg);
                };
                // `erts_bs_append_checked` (`erl_bits.c:1877`): the source's bit size
                // must be a multiple of the segment's unit.
                if unit != 1 && src_bits % unit != 0 {
                    return fail_seg(machine, module, labels, fail, SegReason::BadArg);
                }
                let (bytes, src_off) = raw::subject_bytes(value)?;
                writer.push_bits_from(bytes, src_off, src_bits);
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
fn read_value(
    machine: &Machine,
    module: &Module,
    ctx: &mut dyn ExecutionContext,
    operand: &Operand,
) -> Result<Eterm> {
    match operand {
        Operand::Literal(index) => module
            .literal(*index)
            .ok_or_else(|| error::unsupported("bs_create_bin: literal index out of range")),
        _ => read_operand(machine, ctx, operand),
    }
}

// ---------------------------------------------------------------------------
// Unit tests for the segment encoders
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
#[cfg(test)]
mod wide_tests;
#[cfg(test)]
mod float_seg_tests;
