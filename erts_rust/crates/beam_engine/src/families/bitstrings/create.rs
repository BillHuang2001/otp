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
//! `{atom,binary},2,8,nil,{x,0},{atom,all}`; a `string` constant's value operand is
//! written as an untagged integer byte offset into the module's `StrT` string table
//! (`beam_asm.erl:771-773`), which the loader decodes to `Operand::Integer`.
//!
//! # Scope
//!
//! Implemented: `integer` (any width, big- or little-endian, the value masked into
//! the field — up to 64 bits through the machine-word path, wider fields through the
//! bignum path [`super::push_wide_int`]), `binary` (byte-aligned, `all` or an
//! explicit bit count), `string` constants from the `StrT` table, `float` (16/32/64
//! bits, big- or little-endian), `utf8`/`utf16`/`utf32`, an `append`/`private_append`
//! **first** segment, and `bs_init_writable` — combined into one result bitstring of
//! any size (an on-heap `ErlHeapBits` up to the 64-byte limit, an off-heap refc
//! binary above it; see [`raw::make_binary`]).
//!
//! # `append` / `private_append`: copy-rebuild, not in-place growth
//!
//! C's `private_append` (and `append` on a writable source) *reallocates and extends*
//! the source's off-heap payload in place and returns a new `ErlSubBits` aliasing it,
//! so a loop that appends `n` times is amortised O(n). The engine's payloads are
//! immutable ([`beam_alloc::RefcBinary`] has no grow operation and `BinRef.val` has no
//! setter), so an append instead **seeds a bit vector with the source's bits, appends
//! the remaining segments, and materialises a fresh term**. The result bytes are
//! identical for every successful build (so this is observationally correct), but a
//! long append loop is O(n²) rather than amortised O(n). Three further C observables
//! are deliberately unmodelled in M1: the `BIN_FLAG_WRITABLE`/`BIN_FLAG_ACTIVE_WRITER`
//! flags on the underlying binary, `binary:referenced_byte_size/1`'s "capacity vs
//! size" answer, and the fact that a `private_append` result lets a *later*
//! `private_append` reuse the same buffer (the engine's result is an ordinary,
//! non-writable refc binary — which the next append simply seeds from).
//!
//! Not implemented (a precise typed [`crate::error::unsupported`]): an
//! `append`/`private_append` segment that is not the first.
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
use std::collections::BTreeMap;

use beam_common::{BeamError, ExceptionClass, Result};
use beam_loader::{Instr, Label, Module, NO_FAIL, Operand};
use beam_term::{make_atom, Eterm};

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::Machine;
use crate::operand::{read_operand, write_operand};
use crate::predicates;
use crate::Step;

use super::{
    decode_flags, goto_label, is_all, push_wide_int, raw, read_size, small_value, BitWriter,
    BSF_LITTLE,
};

/// `erts_bs_init_writable`'s default payload capacity (`erl_bits.c:2023`):
/// `bin_size = 1024` unless `x(0)` holds a non-negative small integer.
const DEFAULT_WRITABLE_SIZE: usize = 1024;
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
                if nbits > 64 {
                    // `fmt_big_be`/`fmt_big_le` (`erl_bits.c:724`/`:811`): a wide
                    // field of a small *or* a bignum value, both taken modulo
                    // `2^nbits` (a negative value becomes its nbits-bit two's
                    // complement, `SUBc` on the build side).
                    push_wide_int(&mut writer, value, nbits, flags & BSF_LITTLE != 0)?;
                } else {
                    let Some(raw_value) = small_value(value) else {
                        // `erts_bs_put_integer_*` rejects a non-integer value
                        // (`erl_bits.c:926`) -> `BADARG`.
                        return fail_seg(machine, module, labels, fail, SegReason::BadArg);
                    };
                    if flags & BSF_LITTLE != 0 {
                        writer.push_uint_le(raw_value as u64, nbits);
                    } else {
                        // `erts_bs_put_integer_*` masks the value into the field width.
                        writer.push_uint(raw_value as u64, nbits);
                    }
                }
            }
            // `binary Unit Fs Value Size` — append a bitstring operand's bytes.
            "binary" => {
                // The size loop's `is_not_bitstring(Src)` check
                // (`emu/bs_instrs.tab:390`) fails **before** the size is even read.
                let Some((bytes, src_bits)) = segment_source(machine, module, value_op, ctx)? else {
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
                writer.push_bits_from(bytes, 0, nbits);
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
            // (`erts_bs_put_float`, `erl_bits.c:1341`). `Size` is the bit count
            // directly (already `size * unit`), defaulting to 64.
            "float" => {
                let nbits = if is_all(module, size_op) {
                    64
                } else {
                    match segment_size(machine, ctx, size_op)? {
                        SegSize::Small(n) => match n.checked_mul(unit) {
                            Some(m) if m != 0 => m,
                            _ => 64,
                        },
                        SegSize::Fail(reason) => {
                            return fail_seg(machine, module, labels, fail, reason);
                        }
                    }
                };
                // `erts_bs_put_float` (`erl_bits.c:1449`) returns a failure value for
                // any width other than 16/32/64 (`c_p->fvalue = am_invalid`), and the
                // caller still raises the bare atom `badarg`.
                if !matches!(nbits, 16 | 32 | 64) {
                    return fail_seg(machine, module, labels, fail, SegReason::BadArg);
                }
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
            // segment only, with the accumulator in the value operand and the size
            // operand `all`; C seeds the build with the whole of the source
            // bitstring (`erts_bs_append_checked` / `erts_bs_private_append_checked`
            // take `sb->end` as the starting position) and then appends the
            // remaining segments. The source may be the `bs_init_writable` result
            // (a 0-bit refc binary) or a previously built refc binary, both of which
            // [`raw::heap_bits`] reads.
            //
            // The engine seeds its bit vector from the source and rebuilds a fresh
            // term rather than growing the source's payload in place (the engine's
            // payloads are immutable). That is observationally identical for a
            // successful build; see the module docs for the honest divergences.
            "append" | "private_append" => {
                if i != 0 {
                    return Err(error::unsupported(
                        "bs_create_bin: append/private_append is not the first segment",
                    ));
                }
                let value = read_value(machine, module, ctx, value_op)?;
                let Some((_, src_bits)) = raw::heap_bits(value) else {
                    // `erts_bs_append_checked` (`erl_bits.c:1744`) rejects a
                    // non-bitstring accumulator with `BADARG`.
                    return fail_seg(machine, module, labels, fail, SegReason::BadArg);
                };
                // `erts_bs_append_checked` (`erl_bits.c:1778`): the accumulator's
                // bit size must be a multiple of the segment's unit.
                if unit != 1 && src_bits % unit != 0 {
                    return fail_seg(machine, module, labels, fail, SegReason::BadArg);
                }
                let bytes = raw::subject_bytes(value)?;
                writer.push_bits_from(bytes, 0, src_bits);
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

/// The reason a `bs_create_bin` segment failure (`$BS_FAIL_INFO`) raises — exactly
/// the atom C leaves in `c_p->freason`.
///
/// Every `i_bs_create_bin` call site reports the bare atom `badarg`
/// (`emu/bs_instrs.tab:347`, `BADARG`), **except** the size validation, which
/// reports `system_limit` via `BS_FAIL_INFO_SYSTEM_LIMIT` (`:355`): a `size*unit`
/// multiply overflow (`:424`), a product whose signed value does not fit a word
/// (`:427`), and a **positive** bignum size (`:438`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SegReason {
    /// The bare atom `badarg`.
    BadArg,
    /// The bare atom `system_limit`.
    SystemLimit,
}

impl SegReason {
    /// The BEAM atom name C leaves in `c_p->freason`.
    fn atom_name(self) -> &'static str {
        match self {
            SegReason::BadArg => "badarg",
            SegReason::SystemLimit => "system_limit",
        }
    }

    /// The typed [`BeamError`] a raise falls back to when it cannot land — no live
    /// catch frame, or a reason atom the running module cannot name.
    fn typed(self) -> BeamError {
        match self {
            SegReason::BadArg => BeamError::BadArg,
            SegReason::SystemLimit => BeamError::SystemLimit,
        }
    }
}

/// One bitstring segment's `size*unit` bit count, or the reason C reports.
///
/// The C size loop (`emu/bs_instrs.tab:424-428`) multiplies the (already validated,
/// non-negative) size by the unit under `$BS_SAFE_MUL` and then rejects a product
/// whose *signed* value does not fit in a word (`size >> (sizeof(Uint) * 8 - 1)`).
/// Both are `$BS_FAIL_INFO_SYSTEM_LIMIT`.
fn seg_nbits(size: usize, unit: usize) -> std::result::Result<usize, SegReason> {
    match size.checked_mul(unit) {
        // `(nbits as u64) >> 63 != 0` is C's `size >> (sizeof(Uint) * 8 - 1) != 0`
        // on a 64-bit word: the signed size does not fit.
        Some(nbits) if (nbits as u64) >> 63 == 0 => Ok(nbits),
        _ => Err(SegReason::SystemLimit),
    }
}

/// A `bs_create_bin` **segment failure** (a completed `Err(step)`).
///
/// C's `$BS_FAIL_INFO($Fail, Reason, ...)` (`emu/bs_instrs.tab:347`) ends in
/// `$FAIL_HEAD_OR_BODY($Fail)` (`emu/macros.tab:266`): a live fail label is a branch
/// (`$FAIL`), and `{f,0}` ([`NO_FAIL`]) takes the `goto find_func_info` arm — a real
/// raise whose reason is the atom `BS_FAIL_INFO` left in `c_p->freason`, the bare
/// `badarg` for every call site in `i_bs_create_bin` except the size validation
/// (which reports `system_limit`; see [`SegReason`]).
///
/// This is why `bs_create_bin` must **not** reuse the matching side's
/// [`super::fail_step`], whose `NO_FAIL` arm raises `{badmatch, Subject}`: the
/// matching opcodes branch on `$FAIL` only and never go through `BS_FAIL_INFO`.
#[allow(clippy::type_complexity)]
fn fail_seg(
    machine: &mut Machine,
    module: &Module,
    labels: &BTreeMap<Label, (u32, u32)>,
    fail: Label,
    reason: SegReason,
) -> Result<std::result::Result<(Vec<u8>, usize), Step>> {
    Ok(Err(seg_fail_step(machine, module, labels, fail, reason)?))
}

/// [`fail_seg`]'s `Step` half: branch, or raise the segment failure's reason atom.
fn seg_fail_step(
    machine: &mut Machine,
    module: &Module,
    labels: &BTreeMap<Label, (u32, u32)>,
    fail: Label,
    reason: SegReason,
) -> Result<Step> {
    if fail == NO_FAIL {
        raise_seg_fail(machine, module, reason)
    } else {
        goto_label(labels, fail)
    }
}

/// Raise class `error` with the bare reason atom (C's `BADARG(Fail)` /
/// `SYSTEM_LIMIT(Fail)` at `emu/macros.tab:280` followed by `find_func_info`),
/// mirroring `families/funs.rs::raise_badarg`: the reason atom comes from the
/// *running module's* atom table (the engine never invents an atom it cannot name),
/// and the `error` class atom is installed when the module has it. With no live catch
/// frame — or a reason atom the module cannot name — the raise cannot land and
/// propagates as the typed [`SegReason::typed`] error (`BadArg` / `SystemLimit`,
/// whose `as_atom()` is exactly the atom the oracle raises).
///
/// C also records the `{Segment, Op, ErrorType, Value}` extended error info
/// (`erts_prepare_bs_construct_fail_info`, `emu/beam_emu.c:725`); that only
/// decorates the *stacktrace* and is not modelled at M1.
fn raise_seg_fail(machine: &mut Machine, module: &Module, reason: SegReason) -> Result<Step> {
    if machine.catch_frame_depth() == 0 {
        return Err(reason.typed());
    }
    let atom = module
        .atom_table
        .lookup(reason.atom_name())
        .ok_or_else(|| reason.typed())?;
    if let Some(class_atom) = module.atom_table.lookup(ExceptionClass::Error.as_str()) {
        machine.set_class_atom(ExceptionClass::Error, make_atom(class_atom));
    }
    machine.throw_error(make_atom(atom))
}

/// The outcome of reading a *dynamic* segment size operand, mirroring the C size
/// loop's validation (`emu/bs_instrs.tab:420-445`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SegSize {
    /// A non-negative small integer (the C `signed_size >= 0` arm, `:423`).
    Small(usize),
    /// A segment failure carrying the reason atom C leaves in `c_p->freason`.
    Fail(SegReason),
}

/// Whether an integer term is negative (`big_sign` for a bignum).
///
/// [`beam_term::number_as_f64`] preserves a bignum's sign for every magnitude (the
/// nearest double when representable, else `±f64::INFINITY`), so the sign test is
/// exact; a small integer never reaches here (it takes the `is_small` arm).
fn integer_is_negative(value: Eterm) -> bool {
    beam_term::number_as_f64(value) < 0.0
}

/// Read a *dynamic* segment size operand, mirroring the C size loop's validation
/// (`emu/bs_instrs.tab:420-445`).
///
/// `SegSize::Small(n)` is a non-negative small integer; `SegSize::Fail(reason)` is
/// the `$BS_FAIL_INFO` reason the C reports — `badarg` for a negative size (`:431`),
/// a **negative** bignum (`:440`) or a non-integer (`:444`), and `system_limit` for
/// a **positive** bignum (`:438`).
fn segment_size(
    machine: &Machine,
    ctx: &mut dyn ExecutionContext,
    operand: &Operand,
) -> Result<SegSize> {
    let value = read_operand(machine, ctx, operand)?;
    if beam_term::is_small(value) {
        let n = beam_term::signed_value(value);
        return Ok(if n >= 0 {
            SegSize::Small(n as usize)
        } else {
            SegSize::Fail(SegReason::BadArg)
        });
    }
    if predicates::is_integer(value) {
        // An integer too large to be a small: a positive bignum is `system_limit`
        // (`emu/bs_instrs.tab:438`), a negative one `badarg` (`:440`).
        return Ok(SegSize::Fail(if integer_is_negative(value) {
            SegReason::BadArg
        } else {
            SegReason::SystemLimit
        }));
    }
    // Not an integer (`:444`).
    Ok(SegSize::Fail(SegReason::BadArg))
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

/// The byte slice and bit count a `binary` segment draws from, or `None` when the
/// operand is not a bitstring — the C `is_not_bitstring(Src)` -> `$BS_FAIL_INFO(...,
/// BADARG, am_type, Src)` arm (`emu/bs_instrs.tab:390`, `:650`).
fn segment_source<'a>(
    machine: &Machine,
    module: &Module,
    operand: &Operand,
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<(&'a [u8], usize)>> {
    let value = read_value(machine, module, ctx, operand)?;
    let Some((_, bits)) = raw::heap_bits(value) else {
        return Ok(None);
    };
    let bytes = raw::subject_bytes(value)?;
    Ok(Some((bytes, bits)))
}

/// The `StrT` bytes a `string` constant segment appends.
///
/// The value operand is the string's byte offset into [`Module::strings`]
/// (`beam_asm.erl:771-773` writes it as an untagged integer) and the size operand
/// is its byte count; C computes `size * 8` bits and the unit operand is asserted
/// to be 8 (`generators.tab:994-1001`) and otherwise unused.
///
/// A value operand that is not an integer, a size that is `all`/negative, or a
/// range outside the `StrT` table is a typed [`crate::error::unsupported`] — a
/// malformed segment is never turned into a wrong byte sequence.
fn string_source<'a>(
    module: &'a Module,
    value_op: &Operand,
    size_op: &Operand,
    machine: &Machine,
    ctx: &mut dyn ExecutionContext,
) -> Result<&'a [u8]> {
    let Operand::Integer(offset) = value_op else {
        return Err(error::unsupported(
            "bs_create_bin: string segment value is not a StrT offset",
        ));
    };
    if is_all(module, size_op) {
        return Err(error::unsupported("bs_create_bin: string segment size `all`"));
    }
    let size = read_size(machine, ctx, size_op)?;
    let start = usize::try_from(*offset)
        .map_err(|_| error::unsupported("bs_create_bin: negative StrT offset"))?;
    let end = start
        .checked_add(size)
        .ok_or_else(|| error::unsupported("bs_create_bin: string segment size overflow"))?;
    module
        .strings
        .get(start..end)
        .ok_or_else(|| error::unsupported("bs_create_bin: string segment outside the StrT table"))
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

// ---------------------------------------------------------------------------
// Unit tests for the segment encoders
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use beam_common::BeamError;
    use beam_term::{make_small, Atom};

    use crate::context::MinimalContext;

    // --- `string_source` (the `StrT` constant segment) ---------------------

    /// The fixture's `StrT` table (`bsstring.beam`, from
    /// `beam_lib:chunks(F, ["StrT"])`), used so the offsets are the oracle's.
    const STRT: &[u8] = b"abhicdabczxyQ";

    fn strt_module() -> Module {
        let mut module = Module::default();
        module.strings = STRT.to_vec();
        module
    }

    #[test]
    fn string_source_slices_the_strt_table() {
        let module = strt_module();
        let machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let mut slice = |offset: i64, size: i64| {
            string_source(
                &module,
                &Operand::Integer(offset),
                &Operand::Integer(size),
                &machine,
                &mut ctx,
            )
        };
        assert_eq!(slice(0, 2).unwrap(), b"ab");
        assert_eq!(slice(4, 2).unwrap(), b"cd");
        assert_eq!(slice(9, 1).unwrap(), b"z");
        assert_eq!(slice(12, 1).unwrap(), b"Q");
        // A zero-length slice resolves to the empty slice, never an error.
        assert_eq!(slice(3, 0).unwrap(), b"");
    }

    #[test]
    fn string_source_rejects_a_value_that_is_not_a_strt_offset() {
        let module = strt_module();
        let machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let size = Operand::Integer(2);
        for value in [Operand::Nil, Operand::Atom(module.name), Operand::X(0)] {
            let err = string_source(&module, &value, &size, &machine, &mut ctx).unwrap_err();
            assert!(matches!(err, BeamError::Raise(_)), "{value:?}: got {err:?}");
        }
    }

    #[test]
    fn string_source_rejects_an_all_size() {
        let mut module = strt_module();
        let all = Operand::Atom(module.atom_table.intern("all"));
        let machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let err =
            string_source(&module, &Operand::Integer(0), &all, &machine, &mut ctx).unwrap_err();
        assert!(matches!(err, BeamError::Raise(_)), "got {err:?}");
    }

    #[test]
    fn string_source_rejects_a_negative_offset_and_a_bad_size() {
        let module = strt_module();
        let machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let err = string_source(
            &module,
            &Operand::Integer(-1),
            &Operand::Integer(1),
            &machine,
            &mut ctx,
        )
        .unwrap_err();
        assert!(matches!(err, BeamError::Raise(_)), "got {err:?}");
        // A negative size is not a byte count.
        let err = string_source(
            &module,
            &Operand::Integer(0),
            &Operand::Integer(-1),
            &machine,
            &mut ctx,
        )
        .unwrap_err();
        assert!(matches!(err, BeamError::Raise(_)), "got {err:?}");
    }

    #[test]
    fn string_source_rejects_a_slice_outside_the_table() {
        let module = strt_module();
        let machine = Machine::new();
        let mut ctx = MinimalContext::new();
        // Offset 2 + size 2 runs past the end of a 3-byte table.
        let mut short = Module::default();
        short.strings = b"abc".to_vec();
        let err = string_source(
            &short,
            &Operand::Integer(2),
            &Operand::Integer(2),
            &machine,
            &mut ctx,
        )
        .unwrap_err();
        assert!(matches!(err, BeamError::Raise(_)), "got {err:?}");
        // A start past the end of the whole table is rejected too.
        let err = string_source(
            &module,
            &Operand::Integer(999),
            &Operand::Integer(1),
            &machine,
            &mut ctx,
        )
        .unwrap_err();
        assert!(matches!(err, BeamError::Raise(_)), "got {err:?}");
    }

    // --- `seg_nbits` (the `$BS_SAFE_MUL` + signed-fit check) ---------------

    #[test]
    fn seg_nbits_multiplies_and_flags_a_word_overflow() {
        assert_eq!(seg_nbits(8, 1), Ok(8));
        assert_eq!(seg_nbits(0, 4096), Ok(0));
        assert_eq!(seg_nbits(1 << 40, 8), Ok(1 << 43));
        assert_eq!(seg_nbits((1 << 63) - 1, 1), Ok((1 << 63) - 1));
        // A `size*unit` multiply that overflows a word (`:424`) ...
        assert_eq!(seg_nbits(usize::MAX, 2), Err(SegReason::SystemLimit));
        assert_eq!(seg_nbits(1 << 62, 4), Err(SegReason::SystemLimit));
        // ... and a product that fits a word but not a *signed* one, i.e. with bit
        // 63 set (`:425`-`:427`), are both `system_limit`.
        assert_eq!(seg_nbits(1 << 63, 1), Err(SegReason::SystemLimit));
        assert_eq!(seg_nbits(1 << 62, 2), Err(SegReason::SystemLimit));
    }

    // --- `segment_size` (the size-validation reasons) ----------------------

    /// Read a dynamic size from `x(0)` holding `value`.
    fn size_of(ctx: &mut MinimalContext, value: Eterm) -> SegSize {
        let mut machine = Machine::new();
        machine.set_x(0, value).expect("x0 in range");
        segment_size(&machine, ctx, &Operand::X(0)).expect("segment_size")
    }

    #[test]
    fn segment_size_accepts_a_non_negative_small() {
        let mut ctx = MinimalContext::new();
        assert_eq!(size_of(&mut ctx, make_small(8)), SegSize::Small(8));
        assert_eq!(size_of(&mut ctx, make_small(0)), SegSize::Small(0));
    }

    #[test]
    fn segment_size_rejects_a_negative_small_and_a_non_integer() {
        let mut ctx = MinimalContext::new();
        assert_eq!(
            size_of(&mut ctx, make_small(-1)),
            SegSize::Fail(SegReason::BadArg)
        );
        // `erl -noshell -eval 'io:format("~p~n",[try <<1:x/integer>> catch C:E -> {C,E} end]),halt().'`
        // ⇒ `{error,badarg}` (`emu/bs_instrs.tab:444`).
        assert_eq!(
            size_of(&mut ctx, make_atom(Atom::from_raw(1))),
            SegSize::Fail(SegReason::BadArg)
        );
    }

    #[test]
    fn segment_size_distinguishes_positive_and_negative_bignums() {
        // Oracle, from a clean directory (no `-pa tests/fixtures/`):
        //   X = <<1:4>>, <<X:(1 bsl 59)/binary>>      ⇒ {error,system_limit}
        //   X = <<1:4>>, <<X:(-(1 bsl 59))/binary>>   ⇒ {error,badarg}
        // 2^59 = MAX_SMALL + 1, so both sizes are bignums.
        let mut ctx = MinimalContext::new();
        let positive = crate::terms::make_integer(&mut ctx, 1i64 << 59).expect("bignum");
        assert!(
            !beam_term::is_small(positive),
            "the fixture must exercise the bignum arm"
        );
        assert_eq!(
            size_of(&mut ctx, positive),
            SegSize::Fail(SegReason::SystemLimit)
        );
        let negative = crate::terms::make_integer(&mut ctx, -(1i64 << 59)).expect("bignum");
        assert_eq!(
            size_of(&mut ctx, negative),
            SegSize::Fail(SegReason::BadArg)
        );
    }

    // --- `raise_seg_fail` (the reason atom, branch vs raise) ---------------

    /// A module whose atom table pins the reasons and the `error` class atom.
    fn naming_module() -> Module {
        let mut module = Module::default();
        module.atom_table.intern("badarg");
        module.atom_table.intern("system_limit");
        module.atom_table.intern("error");
        module
    }

    #[test]
    fn seg_reason_names_and_typed_errors_match_the_oracle_atoms() {
        assert_eq!(SegReason::BadArg.atom_name(), "badarg");
        assert_eq!(SegReason::SystemLimit.atom_name(), "system_limit");
        assert!(matches!(SegReason::BadArg.typed(), BeamError::BadArg));
        assert!(matches!(
            SegReason::SystemLimit.typed(),
            BeamError::SystemLimit
        ));
        assert_eq!(BeamError::SystemLimit.as_atom(), Some("system_limit"));
    }

    #[test]
    fn raise_seg_fail_without_a_catch_frame_is_the_typed_error() {
        let mut machine = Machine::new();
        let module = naming_module();
        assert!(matches!(
            raise_seg_fail(&mut machine, &module, SegReason::BadArg).unwrap_err(),
            BeamError::BadArg
        ));
        assert!(matches!(
            raise_seg_fail(&mut machine, &module, SegReason::SystemLimit).unwrap_err(),
            BeamError::SystemLimit
        ));
    }

    #[test]
    fn raise_seg_fail_lands_the_reason_atom_in_a_catch_frame() {
        let mut machine = Machine::new();
        machine.push_frame(0).expect("bottom frame");
        machine
            .push_catch_frame(crate::machine::CatchFrame::new(3, 7, 0));
        let module = naming_module();
        assert_eq!(
            raise_seg_fail(&mut machine, &module, SegReason::SystemLimit).expect("lands"),
            Step::Goto { func: 3, pc: 7 }
        );
        let reason = make_atom(module.atom_table.lookup("system_limit").expect("interned"));
        assert_eq!(machine.x(1), Some(reason));
    }

    #[test]
    fn raise_seg_fail_never_substitutes_an_atom_the_module_cannot_name() {
        // `Module::default()` has an empty atom table: neither the reason atom nor
        // `error` can be named, so the raise must fall back to the typed error
        // rather than inventing a *different* atom.
        let mut machine = Machine::new();
        machine.push_frame(0).expect("bottom frame");
        machine
            .push_catch_frame(crate::machine::CatchFrame::new(3, 7, 0));
        let module = Module::default();
        assert!(matches!(
            raise_seg_fail(&mut machine, &module, SegReason::SystemLimit).unwrap_err(),
            BeamError::SystemLimit
        ));
    }

    // --- `put_float` -------------------------------------------------------
    #[test]
    fn put_float_is_big_endian_for_every_width() {
        assert_eq!(put_float(0.0, 16, false), vec![0x00, 0x00]);
        assert_eq!(put_float(-0.0, 16, false), vec![0x80, 0x00]);
        assert_eq!(put_float(1.0, 16, false), vec![0x3C, 0x00]);
        assert_eq!(put_float(1.5, 16, false), vec![0x3E, 0x00]);
        assert_eq!(put_float(-1.5, 16, false), vec![0xBE, 0x00]);
        assert_eq!(put_float(1.0, 32, false), vec![0x3F, 0x80, 0x00, 0x00]);
        assert_eq!(put_float(1.5, 32, false), vec![0x3F, 0xC0, 0x00, 0x00]);
        assert_eq!(
            put_float(1.5, 64, false),
            vec![0x3F, 0xF8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
        assert_eq!(
            put_float(-1.0, 64, false),
            vec![0xBF, 0xF0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn put_float_little_endian_reverses_the_bytes() {
        assert_eq!(put_float(-0.0, 16, true), vec![0x00, 0x80]);
        assert_eq!(put_float(1.5, 16, true), vec![0x00, 0x3E]);
        assert_eq!(put_float(1.5, 32, true), vec![0x00, 0x00, 0xC0, 0x3F]);
        assert_eq!(
            put_float(1.5, 64, true),
            vec![0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xF8, 0x3F]
        );
    }

    // --- `f64_to_f16` (`erl_bits.c:431`) -----------------------------------

    #[test]
    fn f64_to_f16_exact_values() {
        assert_eq!(f64_to_f16(0.0), 0x0000);
        assert_eq!(f64_to_f16(-0.0), 0x8000);
        assert_eq!(f64_to_f16(1.0), 0x3C00);
        assert_eq!(f64_to_f16(-1.0), 0xBC00);
        assert_eq!(f64_to_f16(1.5), 0x3E00);
        assert_eq!(f64_to_f16(-1.5), 0xBE00);
        assert_eq!(f64_to_f16(0.1), 0x2E66);
        assert_eq!(f64_to_f16(6.103515625e-5), 0x0400); // 2^-14, smallest normal
        assert_eq!(f64_to_f16(5.960464477539063e-8), 0x0001); // 2^-24, smallest subnormal
        assert_eq!(f64_to_f16(65504.0), 0x7BFF); // largest finite binary16
    }

    #[test]
    fn f64_to_f16_overflow_becomes_infinity() {
        // Beyond the binary16 range the exponent field saturates.
        assert_eq!(f64_to_f16(65520.0), 0x7C00);
        assert_eq!(f64_to_f16(100000.0), 0x7C00);
        assert_eq!(f64_to_f16(-100000.0), 0xFC00);
        // The C body's overflow branch also catches a non-finite input; Erlang
        // floats are always finite, so this only documents port fidelity.
        assert_eq!(f64_to_f16(f64::INFINITY), 0x7C00);
        assert_eq!(f64_to_f16(f64::NEG_INFINITY), 0xFC00);
    }

    #[test]
    fn f64_to_f16_rounds_to_nearest_even() {
        // 2^-25 is exactly between 0 and 2^-24; ties go to the even significand.
        assert_eq!(f64_to_f16(2.9802322387695312e-8), 0x0000);
        assert_eq!(f64_to_f16(-2.9802322387695312e-8), 0x8000);
        // 3*2^-25 is exactly between 2^-24 and 2^-23; ties go to 2^-23 (even).
        assert_eq!(f64_to_f16(8.940696716308594e-8), 0x0002);
        // Ties in the normal range: 1 + 2^-11 rounds down to 1.0 (even), while
        // 1 + 3*2^-11 rounds up to 1.0009765625 = 0x3C02 (even).
        assert_eq!(f64_to_f16(1.00048828125), 0x3C00);
        assert_eq!(f64_to_f16(1.00146484375), 0x3C02);
        // A subnormal double is far below the binary16 subnormal range.
        assert_eq!(f64_to_f16(5.0e-324), 0x0000);
        assert_eq!(f64_to_f16(-5.0e-324), 0x8000);
    }

    // --- `put_utf8` --------------------------------------------------------

    #[test]
    fn put_utf8_encodes_each_length() {
        assert_eq!(put_utf8(0), Some(vec![0x00]));
        assert_eq!(put_utf8(0x7F), Some(vec![0x7F]));
        assert_eq!(put_utf8(0x80), Some(vec![0xC2, 0x80]));
        assert_eq!(put_utf8(0x7FF), Some(vec![0xDF, 0xBF]));
        assert_eq!(put_utf8(0x800), Some(vec![0xE0, 0xA0, 0x80]));
        assert_eq!(put_utf8(0xFFFF), Some(vec![0xEF, 0xBF, 0xBF]));
        assert_eq!(put_utf8(0x10000), Some(vec![0xF0, 0x90, 0x80, 0x80]));
        assert_eq!(put_utf8(0x10FFFF), Some(vec![0xF4, 0x8F, 0xBF, 0xBF]));
    }

    #[test]
    fn put_utf8_rejects_non_scalars() {
        assert_eq!(put_utf8(-1), None);
        assert_eq!(put_utf8(0xD7FF + 1), None); // first surrogate
        assert_eq!(put_utf8(0xDFFF), None); // last surrogate
        assert_eq!(put_utf8(0x110000), None);
    }

    // --- `put_utf16` -------------------------------------------------------

    #[test]
    fn put_utf16_encodes_bmp_and_surrogate_pairs() {
        assert_eq!(put_utf16(0, false), Some(vec![0x00, 0x00]));
        assert_eq!(put_utf16(0x7F, false), Some(vec![0x00, 0x7F]));
        assert_eq!(put_utf16(0xFFFF, false), Some(vec![0xFF, 0xFF]));
        assert_eq!(put_utf16(0x10000, false), Some(vec![0xD8, 0x00, 0xDC, 0x00]));
        assert_eq!(put_utf16(0x10FFFF, false), Some(vec![0xDB, 0xFF, 0xDF, 0xFF]));
    }

    #[test]
    fn put_utf16_little_endian_swaps_each_code_unit() {
        assert_eq!(put_utf16(0, true), Some(vec![0x00, 0x00]));
        assert_eq!(put_utf16(0x7F, true), Some(vec![0x7F, 0x00]));
        assert_eq!(put_utf16(0x1234, true), Some(vec![0x34, 0x12]));
        assert_eq!(put_utf16(0x10000, true), Some(vec![0x00, 0xD8, 0x00, 0xDC]));
        assert_eq!(put_utf16(0x10FFFF, true), Some(vec![0xFF, 0xDB, 0xFF, 0xDF]));
    }

    #[test]
    fn put_utf16_rejects_non_scalars() {
        assert_eq!(put_utf16(-1, false), None);
        assert_eq!(put_utf16(0xD800, false), None);
        assert_eq!(put_utf16(0xDFFF, false), None);
        assert_eq!(put_utf16(0x110000, false), None);
    }

    // --- `put_utf32` -------------------------------------------------------

    #[test]
    fn put_utf32_encodes_big_endian_fixed_width() {
        assert_eq!(put_utf32(0), Some(vec![0x00, 0x00, 0x00, 0x00]));
        assert_eq!(put_utf32(1), Some(vec![0x00, 0x00, 0x00, 0x01]));
        assert_eq!(put_utf32(0x10000), Some(vec![0x00, 0x01, 0x00, 0x00]));
        assert_eq!(put_utf32(0x10FFFF), Some(vec![0x00, 0x10, 0xFF, 0xFF]));
    }

    #[test]
    fn put_utf32_rejects_non_scalars() {
        assert_eq!(put_utf32(-1), None);
        assert_eq!(put_utf32(0xD800), None);
        assert_eq!(put_utf32(0xDFFF), None);
        assert_eq!(put_utf32(0x110000), None);
    }
}
