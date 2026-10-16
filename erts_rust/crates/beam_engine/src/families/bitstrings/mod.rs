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

//! Bitstring / binary-syntax instruction family.
//!
//! Owns the whole `Bs*` instruction family (per `../beam_loader/CONTEXT.md`, "The
//! instruction vocabulary"):
//! - match entry: [`Instr::BsStartMatch3`], [`Instr::BsStartMatch4`]
//! - matching: [`Instr::BsMatch`], [`Instr::BsMatchString`], [`Instr::BsGetTail`],
//!   [`Instr::BsGetPosition`], [`Instr::BsSetPosition`], [`Instr::BsGetInteger2`],
//!   [`Instr::BsGetFloat2`], [`Instr::BsGetBinary2`], [`Instr::BsSkipBits2`],
//!   [`Instr::BsTestTail2`]
//! - UTF: [`Instr::BsGetUtf8`], [`Instr::BsGetUtf16`], [`Instr::BsGetUtf32`],
//!   [`Instr::BsSkipUtf8`], [`Instr::BsSkipUtf16`], [`Instr::BsSkipUtf32`]
//! - construction: [`Instr::BsInitWritable`], [`Instr::BsCreateBin`]
//!
//! The work is split by responsibility: this file holds [`exec`] (the dispatch) and
//! the helpers shared by both halves; [`matching`] holds the match side
//! (`bs_start_match*`, `bs_match`, the reads); [`create`] holds construction
//! (`bs_create_bin`); [`raw`] is the family's single `#[allow(unsafe_code)]` module
//! for the `ErlSubBits`/`ErlHeapBits` raw layouts.
//!
//! # Match-context model
//!
//! A match context is an on-heap boxed term in the exact C `ErlSubBits` shape
//! `[HEADER_SUB_BITS, 0, start_bit, end_bit, orig]` where `orig` is the subject
//! heap binary ([`raw::make_ctx`]). All state is re-derived from the term at every
//! opcode, so matching survives calls and branches without any side table — the
//! interpreter's answer to C's native `ErlSubBits` reuse (`bs_instrs.tab:1085-1118`).
//!
//! Bit extraction/insertion is plain **safe** `&[u8]` arithmetic in MSB-first
//! order; only the frozen-header dereferences live in [`raw`].
//!
//! # Error strategy
//!
//! Every failure is a typed [`beam_common::BeamError`] — never a panic and never a
//! wrong result:
//! * a *match* failure branches to the instruction's fail label, or raises
//!   `{badmatch, Value}` when the label is [`NO_FAIL`] ([`fail_step`]);
//! * a genuine M1 coverage edge is [`crate::error::unsupported`] with a precise
//!   reason (see the `Known Issues` in the crate `CONTEXT.md`).

mod create;
mod matching;
#[allow(unsafe_code)]
mod raw;
#[cfg(test)]
mod run_tests;

use crate::interpreter::LabelMap;

use beam_common::Result;
use beam_loader::{Instr, Label, Module, Operand, NO_FAIL};
use beam_term::{make_atom, Eterm};

use crate::context::{ExecutionContext, RootSet};
use crate::error;
use crate::machine::Machine;
use crate::operand::{read_operand, read_operand_slack};
use crate::Step;

/// `BSF_ALIGNED` (`erl_bits.h:461`): the field is guaranteed byte-aligned.
///
/// Kept for completeness of the frozen `erl_bits.h` flag table (`:461-465`). No
/// opcode consults it: the flags a segment operand decodes never include it
/// ([`decode_flags`]), and C's `erts_bs_get_integer_2`/`erts_bs_put_integer_*` never
/// branch on it either — the byte alignment a little-endian field needs is enforced
/// structurally by the byte-wise read/write helpers instead.
#[allow(dead_code)]
pub(super) const BSF_ALIGNED: u32 = 1;/// `BSF_LITTLE` (`erl_bits.h:462`): the field is little-endian.
pub(super) const BSF_LITTLE: u32 = 2;
/// `BSF_SIGNED` (`erl_bits.h:463`): the field is signed.
pub(super) const BSF_SIGNED: u32 = 4;
/// `BSF_NATIVE` (`erl_bits.h:465`): the field is in native endianness.
///
/// Kept for completeness of the frozen `erl_bits.h` flag table (`:461-465`). No
/// decoded segment operand ever carries it: [`flag_bit`] resolves `native` to the
/// machine endianness exactly as the C loader's `NativeEndian` does, so this bit
/// only ever appears in a flag *set*, never in a decoded one.
#[allow(dead_code)]
pub(super) const BSF_NATIVE: u32 = 16;

/// Execute a bitstring-family instruction.
///
/// Returns `Ok(Some(step))` when `instr` is a `Bs*` opcode this family executes,
/// `Ok(None)` when `instr` is not this family's variant, and `Err` on a raise.
pub(crate) fn exec(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    instr: &Instr,
    func: u32,
    pc: u32,
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    if let Some(step) = matching::exec(machine, module, labels, instr, func, pc, ctx)? {
        return Ok(Some(step));
    }
    if let Some(step) = create::exec(machine, module, labels, instr, func, pc, ctx)? {
        return Ok(Some(step));
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Shared failure / label helpers
// ---------------------------------------------------------------------------

/// Dispatch a `bs_*` match failure: branch to `fail`, or raise `{badmatch, V}`
/// when `fail` is [`NO_FAIL`].
///
/// Reproduces the C `$FAIL`/`$FAIL_HEAD_OR_BODY` split (`emu/macros.tab:266`): the
/// compiler emits a live fail label for an ordinary clause mismatch, while `f,0`
/// means "there is no handler — this is a hard match failure" and becomes a
/// `badmatch` raise with the subject as the value.
///
/// The subject is handed in as its **operand**, not as a read `Eterm`: the raise
/// reason's block is reserved first and the subject is then re-read from the
/// machine's post-collection state inside the reader closure
/// ([`crate::terms::badmatch_reason`]), so a moving collector can never leave a
/// stale subject pointer inside the raised `{badmatch, Subject}` — the frozen
/// **reserve, then read, then write** ordering. A subject operand is a
/// register/literal, never a non-small integer immediate, so the reason block's
/// zero-word tail is exactly right (a bignum operand cannot occur here).
pub(super) fn fail_step(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    fail: Label,
    subject: &Operand,
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    if fail == NO_FAIL {
        let reason = crate::terms::badmatch_reason_rooted(
            machine,
            ctx,
            RootSet::ALL_LIVE,
            module,
            |m, slack| read_operand_slack(m, subject, slack),
        )?;
        return machine.throw_error(reason).map(Some);
    }
    Ok(Some(goto_label(labels, fail)?))
}

/// Resolve a module-local label to its `(function, code)` jump target.
pub(super) fn goto_label(labels: &LabelMap, label: Label) -> Result<Step> {
    let (func, pc) = labels.resolve(label)?;
    Ok(Step::Goto { func, pc })
}

// ---------------------------------------------------------------------------
// Operand helpers
// ---------------------------------------------------------------------------

/// Read a segment *size* operand: a non-negative small integer.
///
/// The operand may be a literal in the segment list **or** a register holding a
/// runtime size (the compiler emits `Operand::X(..)` for `<<V:N>>` with a computed
/// `N`), which [`read_operand`] resolves — so a runtime-sized segment works
/// unchanged. A size whose *value* is negative, oversized or not an integer cannot
/// size a segment, and the C `BS_GET_FIELD_SIZE` macro fails the match there; this
/// engine reports that case as a typed [`crate::error::unsupported`] rather than
/// guessing.
pub(super) fn read_size(
    machine: &Machine,
    ctx: &mut dyn ExecutionContext,
    operand: &Operand,
) -> Result<usize> {
    let value = read_operand(machine, ctx, operand)?;
    if beam_term::is_small(value) {
        let n = beam_term::signed_value(value);
        if n >= 0 {
            return Ok(n as usize);
        }
    }
    Err(error::unsupported(
        "bitstring: size is not a non-negative small integer",
    ))
}

/// True when `operand` is the atom `all` (a whole-tail segment size).
pub(super) fn is_all(module: &Module, operand: &Operand) -> bool {
    match operand {
        Operand::Atom(atom) => module.atom_name(*atom) == Some("all"),
        _ => false,
    }
}

/// Decode a segment *flags* operand into the `BSF_*` bits.
///
/// The operand is `Nil` (no flags — `encode_arg` maps the empty list to `a,0`) or a
/// `Literal` holding a flag-atom list (`little`/`signed`/`native`; `big`/`unsigned`
/// are the defaults and contribute nothing). The comparison is by `make_atom` on
/// this module's atom table, so it needs no index arithmetic.
pub(super) fn decode_flags(module: &Module, operand: &Operand) -> Result<u32> {
    match operand {
        Operand::Nil => Ok(0),
        Operand::Literal(index) => {
            let term = module
                .literal(*index)
                .ok_or_else(|| error::unsupported("bitstring: flags literal index out of range"))?;
            let mut flags = 0;
            for elem in crate::terms::list_elements(term)? {
                flags |= flag_bit(module, elem)?;
            }
            Ok(flags)
        }
        _ => Err(error::unsupported("bitstring: unsupported flags operand")),
    }
}

/// The `BSF_*` bit a single flag atom contributes (`big`/`unsigned` are the
/// defaults, i.e. zero).
fn flag_bit(module: &Module, elem: Eterm) -> Result<u32> {
    let named = |name: &str| module.atom_table.lookup(name).map(make_atom) == Some(elem);
    if named("little") {
        return Ok(BSF_LITTLE);
    }
    if named("signed") {
        return Ok(BSF_SIGNED);
    }
    if named("native") {
        // The C loader collapses the `native` flag at load time (`NativeEndian`,
        // `erts/emulator/beam/generators.tab:43-55`): on a little-endian machine
        // `BSF_NATIVE` is cleared and `BSF_LITTLE` set, on a big-endian machine both
        // are cleared (`native` is the `big` default there). Every consumer in this
        // family tests `BSF_LITTLE` alone, so normalise here rather than leaking a
        // bit no consumer honours (which would encode/decode a `native` segment with
        // the wrong endianness on every target).
        return Ok(if cfg!(target_endian = "little") {
            BSF_LITTLE
        } else {
            0
        });
    }
    if named("big") || named("unsigned") {
        return Ok(0);
    }
    Err(error::unsupported("bitstring: unknown segment flag"))
}

// ---------------------------------------------------------------------------
// Safe bit arithmetic (MSB-first within the byte stream)
// ---------------------------------------------------------------------------

/// Read `nbits` (≤ 64) bits starting at bit `start`, MSB-first, as an integer.
///
/// The caller guarantees `bytes` covers `start + nbits` bits (the `bs_*` matching
/// ops only reach here after a range check); a violated precondition is reported as
/// a typed error rather than an out-of-bounds panic.
pub(super) fn read_uint_be(bytes: &[u8], start: usize, nbits: usize) -> Result<u64> {
    if nbits > 64 {
        return Err(error::unsupported(
            "bitstring: integer segment wider than 64 bits",
        ));
    }
    let end = start
        .checked_add(nbits)
        .ok_or_else(|| error::unsupported("bitstring: integer segment out of range"))?;
    if end > bytes.len() * 8 {
        return Err(error::unsupported(
            "bitstring: integer segment out of range",
        ));
    }
    let mut value = 0u64;
    for i in 0..nbits {
        let bit_index = start + i;
        let bit = (bytes[bit_index / 8] >> (7 - (bit_index % 8))) & 1;
        value = (value << 1) | u64::from(bit);
    }
    Ok(value)
}

/// Read a little-endian integer field of `nbits` bits (a whole number of bytes)
/// starting at byte-aligned bit `start`.
pub(super) fn read_uint_le(bytes: &[u8], start: usize, nbits: usize) -> Result<u64> {
    if nbits % 8 != 0 || start % 8 != 0 {
        return Err(error::unsupported(
            "bitstring: little-endian integer segment is not byte-aligned",
        ));
    }
    let nbytes = nbits / 8;
    if start / 8 + nbytes > bytes.len() {
        return Err(error::unsupported(
            "bitstring: integer segment out of range",
        ));
    }
    let base = start / 8;
    let mut value = 0u64;
    for i in 0..nbytes {
        // Little-endian: the least-significant byte is first in the stream.
        value |= u64::from(bytes[base + i]) << (8 * i);
    }
    Ok(value)
}

/// Sign-extend the low `nbits` bits of `raw` to 64 bits.
pub(super) fn sign_extend(raw: u64, nbits: usize) -> u64 {
    if nbits == 0 || nbits >= 64 {
        return raw;
    }
    let sign = 1u64 << (nbits - 1);
    if raw & sign != 0 {
        raw | (!0u64 << nbits)
    } else {
        raw
    }
}

/// Turn an `i64` into a small-integer term, or report the bignum gap.
pub(super) fn make_small_checked(value: i64) -> Result<Eterm> {
    beam_term::try_make_small(value)
        .map_err(|_| error::unsupported("bitstring: value needs a bignum"))
}

/// Encode a small integer as a term, or report the bignum gap (used by the
/// integer *builders*, where C masks the value into the segment width).
pub(super) fn small_value(t: Eterm) -> Option<i64> {
    if beam_term::is_small(t) {
        Some(beam_term::signed_value(t))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Wide (> 64 bit) integer segments
//
// Match-side `erts_bs_get_integer_2` (`erl_bits.c:170`) and build-side
// `fmt_big_be`/`fmt_big_le` (`erl_bits.c:724`/`:811`) both represent an integer as
// a **little-endian byte array** (C's `bigbuf`, "stored in little-endian order,
// since bignums are little-endian") that is then turned into, or read back from, a
// bignum. These helpers are that byte-level model, in plain safe Rust — the
// bignum build/read itself is owned by `beam_term` (through [`raw`]).
// ---------------------------------------------------------------------------

/// The bignum digits (`ErtsDigit`s, least significant first) of a little-endian
/// byte array, with every leading zero digit stripped.
///
/// An all-zero input yields an **empty** list (the caller turns that into
/// `SMALL_ZERO`, mirroring `erts_bs_get_integer_2`'s normalisation loop
/// `while (*MSB == 0 && MSB > LSB)`).
pub(super) fn le_bytes_to_digits(bytes: &[u8]) -> Vec<u64> {
    let mut digits: Vec<u64> = bytes
        .chunks(8)
        .map(|chunk| {
            let mut d = 0u64;
            for (i, &b) in chunk.iter().enumerate() {
                d |= u64::from(b) << (8 * i);
            }
            d
        })
        .collect();
    while digits.last() == Some(&0) {
        digits.pop();
    }
    digits
}

/// Replace a little-endian byte array holding a value `< 2^nbits` with its
/// two's complement **within `nbits` bits** — `(2^nbits - value) mod 2^nbits`.
///
/// This is C's sign handling (`erts_bs_get_integer_2`'s two's-complement loop and
/// `MAYBE_SIGN_EXTEND`/`SUBc`): a negative value is turned into its *magnitude*.
pub(super) fn twos_complement_bits(bytes: &mut [u8], nbits: usize) {
    if bytes.is_empty() {
        return;
    }
    let mut carry = 1u16;
    for b in bytes.iter_mut() {
        let v = u16::from(!*b) + carry;
        *b = (v & 0xFF) as u8;
        carry = v >> 8;
    }
    if nbits % 8 != 0 {
        let top = bytes.len() - 1;
        bytes[top] &= (1u8 << (nbits % 8)) - 1;
    }
}

/// Build the term for a magnitude `digits` (least significant digit first) with
/// the given sign.
///
/// The magnitude is taken to be normalised (no leading zero digit); an empty
/// `digits` is the value zero. `IS_USMALL` (`big.h:76`) decides between a small
/// (`w <= MAX_SMALL`, or `<= MAX_SMALL + 1` when negative — exactly the range
/// [`beam_term::try_make_small`] accepts) and a **bignum**, which is reserved
/// **rooted** ([`raw::make_big`]).
pub(super) fn make_int(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    negative: bool,
    digits: &[u64],
) -> Result<Eterm> {
    match digits {
        [] => make_small_checked(0),
        [d] if *d <= i64::MAX as u64 => {
            let magnitude = *d as i64;
            let signed = if negative { -magnitude } else { magnitude };
            match beam_term::try_make_small(signed) {
                Ok(t) => Ok(t),
                Err(_) => raw::make_big(machine, ctx, negative, digits),
            }
        }
        _ => raw::make_big(machine, ctx, negative, digits),
    }
}

/// Append the `nbits`-bit field representation of an integer term to `writer`,
/// mirroring `fmt_big_be`/`fmt_big_le` (`erl_bits.c:724`/`:811`) — i.e. of a small
/// *or* a bignum value.
///
/// The field holds `value mod 2^nbits` (C masks the value into the field width,
/// which for a negative value is its `nbits`-bit two's complement — `SUBc` on the
/// build side). A big-endian field is the natural MSB-first encoding; a
/// little-endian field emits the least-significant 8-bit group first, leaving the
/// final partial group (`nbits % 8` bits) left-aligned in its byte, exactly as
/// `fmt_big_le` does with its trailing `*buf <<= 8 - offs`. That shared rule is
/// [`BitWriter::push_le_groups`], the same one the machine-word writer
/// [`BitWriter::push_uint_le`] uses, so a non-byte-multiple width is bit-granular
/// however wide (or however big) the value is.
pub(super) fn push_wide_int(
    writer: &mut BitWriter,
    value: Eterm,
    nbits: usize,
    little: bool,
) -> Result<()> {
    let num_bytes = nbits.div_ceil(8);
    let (negative, mut bytes) = integer_magnitude_bytes(value, num_bytes)?;
    if negative {
        twos_complement_bits(&mut bytes, nbits);
    }
    if little {
        writer.push_le_groups(&bytes, nbits);
    } else {
        for k in (0..nbits).rev() {
            writer.push_bit((bytes[k / 8] >> (k % 8)) & 1);
        }
    }
    Ok(())
}

/// The sign and the little-endian magnitude bytes (truncated to `num_bytes`) of an
/// integer term.
///
/// A small integer contributes its `i64` magnitude; a bignum contributes its
/// `ErtsDigit`s byte by byte. Anything else (a float, atom, …) is not an integer and
/// is a typed [`crate::error::unsupported`] **defensive floor**: the `bs_create_bin`
/// arm classifies a non-integer as the oracle's bare `badarg` *before* it reaches
/// this point ([`super::create`]'s integer arm, C's `erts_bs_put_integer_be`/`_le`
/// not-an-integer arm `erl_bits.c:952`/`:1152`), so the reason here names the value
/// type rather than the segment type the caller already knows.
fn integer_magnitude_bytes(value: Eterm, num_bytes: usize) -> Result<(bool, Vec<u8>)> {
    if let Some(v) = small_value(value) {
        let magnitude = v.unsigned_abs();
        let mut bytes = vec![0u8; num_bytes];
        for (j, byte) in bytes.iter_mut().enumerate().take(8) {
            *byte = ((magnitude >> (8 * j)) & 0xFF) as u8;
        }
        return Ok((v < 0, bytes));
    }
    if let Some((negative, digits)) = raw::big_parts_safe(value) {
        let mut bytes = vec![0u8; num_bytes];
        for (di, &digit) in digits.iter().enumerate() {
            for k in 0..8 {
                let index = di * 8 + k;
                if index < num_bytes {
                    bytes[index] = ((digit >> (8 * k)) & 0xFF) as u8;
                }
            }
        }
        return Ok((negative, bytes));
    }
    Err(error::unsupported(
        "bitstring: integer segment value is not an integer",
    ))
}

/// Copy `nbits` bits starting at `start` (MSB-first) out of `bytes` into a fresh
/// buffer of `ceil(nbits / 8)` bytes, zeroing the unused trailing bits.
pub(super) fn extract_bits(bytes: &[u8], start: usize, nbits: usize) -> Result<Vec<u8>> {
    let end = start
        .checked_add(nbits)
        .ok_or_else(|| error::unsupported("bitstring: bit range out of range"))?;
    if end > bytes.len() * 8 {
        return Err(error::unsupported("bitstring: bit range out of range"));
    }
    let mut out = vec![0u8; nbits.div_ceil(8)];
    for i in 0..nbits {
        let bit_index = start + i;
        let bit = (bytes[bit_index / 8] >> (7 - (bit_index % 8))) & 1;
        if bit != 0 {
            out[i / 8] |= 1 << (7 - (i % 8));
        }
    }
    Ok(out)
}

/// A bit sink: append bits (MSB-first) to a growable byte buffer.
///
/// Used by the `bs_create_bin` builder, which assembles the result as a bit vector
/// and materialises it once at the end ([`raw::make_binary`]).
#[derive(Default)]
pub(super) struct BitWriter {
    bytes: Vec<u8>,
    /// Bits written so far (≤ `bytes.len() * 8`).
    nbits: usize,
}

impl BitWriter {
    /// Append a single bit (`0` or `1`).
    pub(super) fn push_bit(&mut self, bit: u8) {
        let byte = self.nbits / 8;
        if byte >= self.bytes.len() {
            self.bytes.push(0);
        }
        if bit & 1 != 0 {
            self.bytes[byte] |= 1 << (7 - (self.nbits % 8));
        }
        self.nbits += 1;
    }

    /// Append the low `width` bits of `value`, most-significant bit first.
    pub(super) fn push_uint(&mut self, value: u64, width: usize) {
        for k in (0..width).rev() {
            self.push_bit(((value >> k) & 1) as u8);
        }
    }

    /// Append the low `width` bits of `value` in little-endian **bit** order
    /// (`width` ≤ 64, the machine-word field width).
    ///
    /// The field is the value's little-endian bytes, each written most-significant
    /// bit first, with the final partial group (`width % 8` bits) carrying the
    /// value's remaining high bits left-aligned in the last byte — exactly
    /// `fmt_small_le` (`erl_bits.c:686`) and the `restore_and_shift` placement
    /// (`:891`) of an unaligned destination, so a **non-byte-multiple** width is
    /// written bit-granularly rather than truncated to whole bytes. The rule itself
    /// is shared with the bignum writer [`push_wide_int`] through
    /// [`BitWriter::push_le_groups`].
    pub(super) fn push_uint_le(&mut self, value: u64, width: usize) {
        let mut bytes = [0u8; 8];
        for (j, byte) in bytes.iter_mut().enumerate() {
            *byte = ((value >> (8 * j)) & 0xFF) as u8;
        }
        self.push_le_groups(&bytes, width);
    }

    /// Append `nbits` bits of the little-endian byte sequence `bytes` in
    /// little-endian order: `bytes[0]`'s bits most-significant first, then
    /// `bytes[1]`'s, and so on, with the final partial group (`nbits % 8` bits)
    /// taking `bytes`'s **low** remaining bits most-significant first, so the last
    /// byte ends up left-aligned in the field.
    ///
    /// This is the one place the little-endian rule lives: [`BitWriter::push_uint_le`]
    /// (the machine-word writer) and [`push_wide_int`] (the `fmt_big_le` byte-array
    /// writer, `erl_bits.c:812`) are the same rule over a `u64` and over a byte slice.
    /// The caller guarantees `bytes` covers `nbits` bits (for `push_uint_le`:
    /// `width <= 64`, i.e. within the eight bytes of a `u64`).
    fn push_le_groups(&mut self, bytes: &[u8], nbits: usize) {
        let rem = nbits % 8;
        let nbytes = nbits.div_ceil(8).min(bytes.len());
        for (j, &byte) in bytes.iter().enumerate().take(nbytes) {
            let bits = if j + 1 == nbytes && rem != 0 { rem } else { 8 };
            for k in (0..bits).rev() {
                self.push_bit((byte >> k) & 1);
            }
        }
    }

    /// Append `count` bits from `src` (MSB-first) starting at bit `start`.
    pub(super) fn push_bits_from(&mut self, src: &[u8], start: usize, count: usize) {
        for i in 0..count {
            let bit_index = start + i;
            let bit = (src[bit_index / 8] >> (7 - (bit_index % 8))) & 1;
            self.push_bit(bit);
        }
    }

    /// The assembled bytes and the exact bit count.
    pub(super) fn finish(self) -> (Vec<u8>, usize) {
        (self.bytes, self.nbits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_loader::Module;

    /// Decode a single flag atom of `module` through [`flag_bit`].
    fn bit_of(module: &Module, name: &str) -> u32 {
        let atom = module
            .atom_table
            .lookup(name)
            .unwrap_or_else(|| panic!("the test module defines the {name:?} atom"));
        flag_bit(module, make_atom(atom)).expect("a known flag atom decodes")
    }

    /// `native` decodes to the **machine's** endianness, not to a distinct bit.
    ///
    /// The C loader collapses the flag at load time (`NativeEndian`,
    /// `erts/emulator/beam/generators.tab:43-55`): on a little-endian machine
    /// `BSF_NATIVE` is cleared and `BSF_LITTLE` set; on a big-endian machine both are
    /// cleared (`native` is the `big` default there). Every consumer in this family
    /// tests `BSF_LITTLE` alone, so a `native` segment must never keep `BSF_NATIVE` —
    /// on this little-endian target such a segment would otherwise be written and read
    /// **big-endian**, exactly backwards (`<<16#0102:16/native>>` is `<<2,1>>` in the
    /// oracle, `<<1,2>>` for the big-endian default).
    #[test]
    fn the_native_flag_is_the_machine_endianness() {
        let mut module = Module::default();
        for name in ["native", "little", "signed"] {
            module.atom_table.intern(name);
        }

        let expected = if cfg!(target_endian = "little") {
            BSF_LITTLE
        } else {
            0
        };
        assert_eq!(
            bit_of(&module, "native"),
            expected,
            "`native` must resolve to the machine endianness (BSF_LITTLE on a LE target)"
        );
        assert_eq!(
            bit_of(&module, "native") & BSF_NATIVE,
            0,
            "`native` must never survive as BSF_NATIVE: every consumer tests BSF_LITTLE"
        );
        // The explicit flags keep their own bits, unaffected.
        assert_eq!(bit_of(&module, "little"), BSF_LITTLE);
        assert_eq!(bit_of(&module, "signed"), BSF_SIGNED);
    }

    /// A little-endian field is written **bit-granular**: the field is the value's
    /// little-endian bytes, each written most-significant bit first, with the final
    /// partial group carrying the value's remaining high bits left-aligned in the
    /// last byte (`fmt_small_le`, `erl_bits.c:686`) — never truncated to whole bytes.
    ///
    /// Every expected vector is the live oracle's (system OTP 29.1.1 / erts-17.1, a
    /// little-endian host): `<<16#0102:12/little>>` = `<<2,1:4>>`,
    /// `<<16#0102:20/little>>` = `<<2,1,0:4>>`, `<<16#0102:4/little>>` = `<<2:4>>`,
    /// `<<16#ABC:12/little>>` = `<<188,10:4>>`, `<<16#1ABC:12/little>>` =
    /// `<<188,10:4>>` (bits 12..15 are outside the field), `<<(-1):12/little>>` =
    /// `<<255,15:4>>`.
    #[test]
    fn push_uint_le_writes_a_bit_granular_little_endian_field() {
        let le = |value: u64, width: usize| {
            let mut writer = BitWriter::default();
            writer.push_uint_le(value, width);
            writer.finish()
        };
        assert_eq!(le(0x0102, 12), (vec![0x02, 0x10], 12));
        assert_eq!(le(0x0102, 20), (vec![0x02, 0x01, 0x00], 20));
        assert_eq!(le(0x0102, 4), (vec![0x20], 4));
        assert_eq!(le(0x0102, 1), (vec![0x00], 1));
        assert_eq!(le(0xABC, 12), (vec![0xBC, 0xA0], 12));
        assert_eq!(le(0x1ABC, 12), (vec![0xBC, 0xA0], 12));
        assert_eq!(le(0, 12), (vec![0x00, 0x00], 12));
        assert_eq!(le(u64::MAX, 12), (vec![0xFF, 0xF0], 12));
        assert_eq!(le(0x0102, 0), (vec![], 0));
        // Byte-aligned widths stay byte-identical (regression guard).
        assert_eq!(le(0x0102, 8), (vec![0x02], 8));
        assert_eq!(le(0x0102, 16), (vec![0x02, 0x01], 16));
        assert_eq!(le(0x0102, 24), (vec![0x02, 0x01, 0x00], 24));
        assert_eq!(le(0x0102, 32), (vec![0x02, 0x01, 0x00, 0x00], 32));
        assert_eq!(le(0x0102, 64), (vec![0x02, 0x01, 0, 0, 0, 0, 0, 0], 64));
    }

    /// The **byte-slice** form of the same rule, which the bignum/wide writer
    /// ([`push_wide_int`], `fmt_big_le`) uses: `<<X:72/little>>` with
    /// `X = (1 bsl 70) + 16#ABC` is `<<188,10,0,0,0,0,0,0,64>>` in the oracle, and a
    /// narrow partial group takes the byte's **low** remaining bits (`X:20/little` is
    /// `<<188,26,0:4>>`).
    #[test]
    fn push_le_groups_writes_a_wide_little_endian_field() {
        let groups = |bytes: &[u8], nbits: usize| {
            let mut writer = BitWriter::default();
            writer.push_le_groups(bytes, nbits);
            writer.finish()
        };
        // The little-endian magnitude bytes of `(1 bsl 70) + 16#ABC` and of
        // `(1 bsl 70) + 16#1ABC` (byte 1 is `0x0A` / `0x1A`).
        let y = [0xBC, 0x0A, 0, 0, 0, 0, 0, 0, 0x40];
        let x = [0xBC, 0x1A, 0, 0, 0, 0, 0, 0, 0x40];
        assert_eq!(groups(&y, 72), (y.to_vec(), 72));
        assert_eq!(groups(&x, 20), (vec![0xBC, 0x1A, 0x00], 20));
        assert_eq!(groups(&x, 12), (vec![0xBC, 0xA0], 12));
        assert_eq!(groups(&x, 4), (vec![0xC0], 4));
        assert_eq!(groups(&y, 0), (vec![], 0));
    }
}
