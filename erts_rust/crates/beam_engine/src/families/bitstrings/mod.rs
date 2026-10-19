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
//! The work is split by responsibility: this file holds [`exec`] (the dispatch),
//! [`claimed`] (the family's one statement of its variant set) and the helpers shared
//! by both halves; [`matching`] holds the match side
//! (`bs_start_match*`, `bs_match`, the reads); [`create`] holds construction
//! (`bs_create_bin`); [`raw`] is the family's single `#[allow(unsafe_code)]` module
//! for the `ErlSubBits`/`ErlHeapBits` raw layouts.
//!
//! [`exec`] is the **only** entry into the family, and it is what every caller uses:
//! the router ([`crate::families::dispatch`], for a `Bs*` instruction the interpreter
//! reaches from its catch-all arm) and the native baseline tier's in-block helper
//! (`jit::baseline::bitstr::jit_bitstr`). The exception-class anchor and the
//! operand/term work therefore live here once, never in a caller.
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
/// The one narrow name a host's BIF floor adopts to serve `erlang:bit_size/1`: a BIF
/// cannot reach this module's `pub(super)` family readers, so the frozen bitstring /
/// `ErlSubBits` size is exposed as a single safe function that answers `None` (→ the
/// caller's `badarg`) for a non-bitstring rather than a substituted size.
///
/// The lint allow is deliberate and temporary: the adopters are the runtime-side BIF
/// floors (`src/bench/counting.rs`'s harness floor and `tests/support/bifs.rs`'s test
/// host), which live outside this node.
#[allow(unused_imports)]
pub(crate) use raw::bit_size;

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
pub(super) const BSF_ALIGNED: u32 = 1;
/// `BSF_LITTLE` (`erl_bits.h:462`): the field is little-endian.
pub(super) const BSF_LITTLE: u32 = 2;
/// `BSF_SIGNED` (`erl_bits.h:463`): the field is signed.
pub(super) const BSF_SIGNED: u32 = 4;
/// `BSF_NATIVE` (`erl_bits.h:465`): the field is in native endianness.
///
/// This is a **live** flag, not a flag-table placeholder: the standalone reads whose
/// flags operand is a raw `u32` (`bs_get_integer2`/`bs_get_float2`/`bs_get_utf16`/
/// `bs_get_utf32`, which the loader decodes with `as_u32`) really do carry `16#10`
/// here — `beam_asm:flag_to_bit/1` encodes `native` as `16#10` — and it is consumed by
/// [`matching::reads::field_is_little`], which reads such a field little-endian on a
/// little-endian target and big-endian otherwise, mirroring the C loader's
/// `$NativeEndian` (`erts/emulator/beam/emu/generators.tab:43-55`). [`flag_bit`]
/// additionally normalises the `native` **flag atom** to that same target order for
/// the literal-atom-list route ([`decode_flags`]).
pub(super) const BSF_NATIVE: u32 = 16;

/// `true` for exactly the variants this family owns.
///
/// The family's **single source of truth** for its variant set. [`exec`] answers
/// `Ok(None)` for every other instruction, and every caller that has to decide
/// "does the bitstring family own this opcode?" *before* executing it asks this
/// predicate instead of re-listing the variants: the router
/// ([`crate::families::dispatch`]) and the native baseline tier (its emitted arm in
/// `jit::baseline::emit::block` and its in-block helper in `jit::baseline::bitstr`)
/// both do, so the three can never disagree about who owns an opcode.
///
/// The predicate is **purely syntactic** (the instruction's own variant), which is
/// what lets the native tier take the decision at compile time and skip it at
/// execution time.
pub(crate) fn claimed(instr: &Instr) -> bool {
    matches!(
        instr,
        Instr::BsStartMatch3 { .. }
            | Instr::BsStartMatch4 { .. }
            | Instr::BsGetPosition { .. }
            | Instr::BsSetPosition { .. }
            | Instr::BsGetTail { .. }
            | Instr::BsTestTail2 { .. }
            | Instr::BsGetInteger2(_)
            | Instr::BsGetBinary2(_)
            | Instr::BsSkipBits2 { .. }
            | Instr::BsMatch { .. }
            | Instr::BsMatchString { .. }
            | Instr::BsGetFloat2(_)
            | Instr::BsGetUtf8 { .. }
            | Instr::BsGetUtf16 { .. }
            | Instr::BsGetUtf32 { .. }
            | Instr::BsSkipUtf8 { .. }
            | Instr::BsSkipUtf16 { .. }
            | Instr::BsSkipUtf32 { .. }
            | Instr::BsInitWritable
            | Instr::BsCreateBin { .. }
    )
}

/// Execute a bitstring-family instruction.
///
/// Returns `Ok(Some(step))` when `instr` is a `Bs*` opcode this family executes,
/// `Ok(None)` when `instr` is not this family's variant, and `Err` on a raise.
///
/// This is the family's **one** entry, so it also owns the exception-class anchor
/// (`Machine::ensure_class_atoms`): a raise that lands in a live `try`/`catch` frame
/// names its class from the engine's live atom namespace (with the module snapshot
/// as the fallback), and without the resolution a family raise whose class atom the
/// *module's* table cannot name would select its catch frame and then abandon it
/// with a typed `unsupported` instead of landing. The router runs the same latch
/// before it routes to any family, so on the routed path this is one `bool` load and
/// a not-taken branch — but the entry cannot assume it has a router in front of it:
/// the native baseline tier's in-block helper
/// (`jit::baseline::bitstr::jit_bitstr`) calls it directly.
pub(crate) fn exec(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    instr: &Instr,
    func: u32,
    pc: u32,
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    if !machine.class_atoms_live {
        machine.ensure_class_atoms(ctx, module);
    }
    #[cfg(test)]
    EXEC_CALLS.with(|calls| calls.set(calls.get() + 1));
    if let Some(step) = matching::exec(machine, module, labels, instr, func, pc, ctx)? {
        return Ok(Some(step));
    }
    if let Some(step) = create::exec(machine, module, labels, instr, func, pc, ctx)? {
        return Ok(Some(step));
    }
    Ok(None)
}

/// How many times [`exec`] has been entered on this thread.
///
/// Test-only structural instrument (`#[cfg(test)]`-gated, so a production build
/// carries no counter at all): it is how a test observes **where the router sent** an
/// instruction — [`crate::families::dispatch`] reaching this family is the router's
/// own statement that the bitstring family owns the variant — without re-deriving the
/// router's routing table in the test. Per-thread and monotonic, so a caller takes a
/// `before`/`after` pair around the dispatch it is describing.
#[cfg(test)]
pub(crate) fn exec_calls() -> u64 {
    EXEC_CALLS.with(std::cell::Cell::get)
}

// The `exec_calls` counter's per-thread storage (test builds only).
#[cfg(test)]
thread_local! {
    static EXEC_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
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
#[inline(always)]
pub(super) fn read_size(
    machine: &Machine,
    ctx: &mut dyn ExecutionContext,
    operand: &Operand,
) -> Result<usize> {
    // The compiler emits a segment size as a **literal integer operand** (`<<V:8>>`
    // is `Operand::Integer(8)`), which is by far the hottest shape here: it denotes
    // `n` itself, so the term the general read would materialise is only read back
    // for that same value. Short-circuit it (and `Character`, its `h`-tagged twin)
    // rather than paying an out-of-line `make_integer_unrooted` call for every
    // segment; every other operand kind — a register holding a runtime `size(Expr)`,
    // a literal — goes through [`read_size_slow`].
    //
    // Keeping the two literal arms in this always-inlined shell and the
    // `read_operand` route out of line is what lets a caller fold the size decode
    // into two discriminant tests instead of a call plus a `Result` returned through
    // memory.
    match operand {
        Operand::Integer(n) => size_of_int(*n),
        Operand::Character(c) => size_of_int(i64::from(*c)),
        other => read_size_slow(machine, ctx, other),
    }
}

/// The non-literal size operands: a register/Y slot holding a runtime `size(Expr)`, or
/// a literal-area term, resolved through [`read_operand`]; also the accept rule's
/// failure (a negative, oversized or non-integer value).
#[inline(never)]
fn read_size_slow(
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
    Err(size_error())
}

/// The size an integer operand value denotes.
///
/// The accept rule is exactly [`read_size`]'s: a value that is a non-negative small
/// integer ([`beam_term::try_make_small`]'s range) is the size; anything else — a
/// negative value, or a magnitude that only a bignum represents — cannot size a
/// segment and is the same typed error.
#[inline]
fn size_of_int(n: i64) -> Result<usize> {
    if n >= 0 && beam_term::try_make_small(n).is_ok() {
        return Ok(n as usize);
    }
    Err(size_error())
}

/// The typed error a size operand that is not a non-negative small integer raises.
#[cold]
fn size_error() -> beam_common::BeamError {
    error::unsupported("bitstring: size is not a non-negative small integer")
}

/// True when `operand` is the atom `all` (a whole-tail segment size).
#[inline]
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
///
/// The hot arm (`Nil`, every flagless segment the compiler emits) is kept in an
/// `#[inline(always)]` shell whose only other work is a call to the out-of-line
/// [`decode_flags_slow`], so the flags decode costs the caller one discriminant test
/// instead of a call, a `Result` return through memory and a prologue.
#[inline(always)]
pub(super) fn decode_flags(module: &Module, operand: &Operand) -> Result<u32> {
    // The overwhelmingly common segment carries no flags (`encode_arg` maps the
    // empty list to `a,0`), so the hot arm is a plain zero.
    match operand {
        Operand::Nil => Ok(0),
        other => decode_flags_slow(module, other),
    }
}

/// The non-`Nil` flags operands: a flag-atom list literal, or the typed error.
#[inline(never)]
fn decode_flags_slow(module: &Module, operand: &Operand) -> Result<u32> {
    match operand {
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
        // `Nil` is the fast shell's own arm, so it cannot reach here.
        _ => Err(error::unsupported("bitstring: unsupported flags operand")),
    }
}

/// The `BSF_*` bit a single flag atom contributes (`big`/`unsigned` are the
/// defaults, i.e. zero).
#[inline]
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
        // are cleared (`native` is the `big` default there). The build side's
        // little-endian writers select their path from `BSF_LITTLE` alone (the raw
        // `BSF_NATIVE` a standalone read's `u32` flags operand can carry is resolved
        // by `matching::reads::field_is_little` instead), so normalise the `native`
        // flag *atom* here rather than leaking a bit those writers do not honour —
        // which would encode a `native` segment with the wrong endianness on every
        // target.
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
    // A byte-aligned field of a whole number of bytes — every `<<V:8>>`/`<<V:16>>`/…
    // segment the compiler emits for real data — is the field's bytes read
    // big-endian, so accumulate whole bytes instead of shifting in one bit at a time.
    if start % 8 == 0 && nbits % 8 == 0 {
        let base = start / 8;
        let nbytes = nbits / 8;
        let mut value = 0u64;
        for &byte in &bytes[base..base + nbytes] {
            value = (value << 8) | u64::from(byte);
        }
        return Ok(value);
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
#[inline]
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
#[inline]
pub(super) fn make_small_checked(value: i64) -> Result<Eterm> {
    beam_term::try_make_small(value)
        .map_err(|_| error::unsupported("bitstring: value needs a bignum"))
}

/// Encode a small integer as a term, or report the bignum gap (used by the
/// integer *builders*, where C masks the value into the segment width).
#[inline]
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
#[inline]
pub(super) fn push_wide_int(
    writer: &mut BitWriter,
    value: Eterm,
    nbits: usize,
    little: bool,
    machine: &mut Machine,
) -> Result<()> {
    writer.ensure_room(machine, nbits)?;
    let num_bytes = nbits.div_ceil(8);
    let (negative, mut bytes) = integer_magnitude_bytes(value, num_bytes)?;
    if negative {
        twos_complement_bits(&mut bytes, nbits);
    }
    if little {
        writer.push_le_groups(&bytes, nbits);
    } else {
        // Big-endian: the magnitude's most significant byte first, MSB-first within
        // each byte. A byte-aligned whole-byte field writes the bytes directly (in
        // reverse, since `bytes` is the little-endian magnitude) instead of paying a
        // `push_bit` per bit.
        if nbits % 8 == 0 && writer.bit_len() % 8 == 0 {
            for &byte in bytes[..num_bytes].iter().rev() {
                writer.push_whole_bytes(&[byte]);
            }
        } else {
            for k in (0..nbits).rev() {
                writer.push_bit((bytes[k / 8] >> (k % 8)) & 1);
            }
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
    // A byte-aligned field of a whole number of bytes is exactly its source bytes —
    // copy them whole instead of transposing bit by bit.
    if start % 8 == 0 && nbits % 8 == 0 {
        let base = start / 8;
        let nbytes = nbits / 8;
        let mut out = vec![0u8; nbytes];
        out.copy_from_slice(&bytes[base..base + nbytes]);
        return Ok(out);
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

/// Where a [`BitWriter`] deposits its bits.
///
/// * [`Dest::Vec`] — the general **copy-rebuild** destination: a growable `Vec<u8>` the
///   build fills and `raw::make_binary` materialises once at the end (the on-heap
///   `ErlHeapBits`/off-heap refc binary choice of [`raw::make_binary`]).
/// * [`Dest::InPlace`] — the compiler's **linear accumulator**: the bits already live in
///   a writable `bs_init_writable` payload ([`raw::InPlace`]), so the new segment's bits
///   are written straight after them and the build publishes the *same* term with a
///   wider `ErlSubBits.end` (`erts_bs_private_append_checked`, `erl_bits.c:1943-2020`).
pub(super) enum Dest {
    /// A fresh byte buffer and the bit count written into it.
    Vec {
        bytes: Vec<u8>,
        /// Bits written so far (`bytes.len() == nbits.div_ceil(8)`).
        nbits: usize,
    },
    /// The writable accumulator's payload, written in place.
    InPlace(raw::InPlace),
}

impl Default for Dest {
    fn default() -> Dest {
        Dest::Vec {
            bytes: Vec::new(),
            nbits: 0,
        }
    }
}

/// What a completed [`BitWriter`] build produced.
///
/// The in-place form is a **term**: the accumulated bits never left the payload, so the
/// result is the accumulator itself with a bumped window end and no allocation at all.
/// The byte form is the copy-rebuild result, which the caller materialises with
/// [`raw::make_binary`].
pub(super) enum Built {
    /// Assembled bytes and their exact bit count.
    Bytes(Vec<u8>, usize),
    /// The accumulator term, already extended ([`raw::InPlace::finish`]).
    Term(Eterm),
}

/// A bit sink: append bits (MSB-first) to a growable byte buffer **or** in place into a
/// writable accumulator payload ([`Dest`]).
///
/// Used by the `bs_create_bin` builder, which assembles the result as a bit vector and
/// materialises it once at the end ([`raw::make_binary`]) — unless it started in
/// [`Dest::InPlace`], where the materialisation is the term itself.
///
/// # Promotion
///
/// The in-place destination is bounded by the payload capacity, which
/// [`raw::InPlace::grow`] widens when it can, and can be invalidated by a segment source
/// that aliases the payload. When the payload cannot grow (a shared handle, or the
/// `max_capacity` cap), when a second on-heap pair in this machine names it
/// (`raw::payload_aliased_elsewhere`), or when the source overlaps it
/// ([`raw::InPlace::overlaps`]), the writer **promotes**: the written prefix is copied out
/// ([`raw::InPlace::into_bytes`]) and the build continues in [`Dest::Vec`]. Promotion is
/// exact — the bits already written are the same bits — so the final result is
/// byte-identical to the copy-rebuild path, and the accumulator's window is left unbumped.
pub(super) struct BitWriter {
    dest: Dest,
}

impl Default for BitWriter {
    fn default() -> BitWriter {
        BitWriter {
            dest: Dest::default(),
        }
    }
}

impl BitWriter {
    /// A writer that deposits into `sink` — the writable accumulator's payload.
    pub(super) fn in_place(sink: raw::InPlace) -> BitWriter {
        BitWriter {
            dest: Dest::InPlace(sink),
        }
    }

    /// Whether this build has written nothing yet (a writer that has not been switched
    /// to a payload, and has deposited no bits) — the `bs_create_bin` loop's gate for
    /// entering the in-place mode on its **first** segment.
    pub(super) fn is_pristine(&self) -> bool {
        matches!(self.dest, Dest::Vec { nbits: 0, .. })
    }

    /// Switch to the copy-rebuild destination, carrying the bits already written.
    fn promote(&mut self) {
        let Dest::InPlace(sink) = core::mem::replace(&mut self.dest, Dest::default()) else {
            // Only called when the destination is in place.
            unreachable!("promotion is only reachable from the in-place destination");
        };
        let (bytes, nbits) = sink.into_bytes();
        self.dest = Dest::Vec { bytes, nbits };
    }

    /// Make room for `bits` more bits, growing the writable accumulator's payload when it
    /// no longer fits (C's `GROW_PROC_BIN_SIZE` arm, `erl_bits.c:1965-1976`) and falling back
    /// to the copy-rebuild form when the payload cannot grow.
    ///
    /// The growth door is consulted ONLY on a shortfall: it scans the machine's payload
    /// roster, so a per-append call would be an O(#payloads) regression on exactly the
    /// workload this exists to speed up.
    #[inline]
    fn ensure_room(&mut self, machine: &mut Machine, bits: usize) -> Result<()> {
        let Dest::InPlace(sink) = &mut self.dest else {
            return Ok(());
        };
        if sink.remaining_bits() >= bits {
            return Ok(());
        }
        if !sink.grow(machine, bits)? {
            self.promote();
        }
        Ok(())
    }

    /// The bits written so far — the **whole** result's bit count (for an in-place build,
    /// the accumulator's window end plus everything this build appended).
    #[inline]
    pub(super) fn bit_len(&self) -> usize {
        match &self.dest {
            Dest::Vec { nbits, .. } => *nbits,
            Dest::InPlace(sink) => sink.bit_len(),
        }
    }

    /// Whether the segment source `src` overlaps the in-place payload this writer appends
    /// into; such a source is copied out first (promotion) so no read ever aliases a write.
    #[inline]
    fn source_overlaps(&self, src: &[u8]) -> bool {
        match &self.dest {
            Dest::Vec { .. } => false,
            Dest::InPlace(sink) => sink.overlaps(src.as_ptr(), src.len()),
        }
    }

    /// Append a single bit (`0` or `1`).
    #[inline]
    pub(super) fn push_bit(&mut self, bit: u8) {
        if let Dest::InPlace(sink) = &self.dest {
            if sink.remaining_bits() == 0 {
                self.promote();
            }
        }
        match &mut self.dest {
            Dest::Vec { bytes, nbits } => {
                let byte = *nbits / 8;
                if byte >= bytes.len() {
                    bytes.push(0);
                }
                if bit & 1 != 0 {
                    bytes[byte] |= 1 << (7 - (*nbits % 8));
                }
                *nbits += 1;
            }
            Dest::InPlace(sink) => sink.push_bit(bit),
        }
    }
    /// Append the low `width` bits of `value`, most-significant bit first.
    ///
    /// A **whole number of bytes** written to a **byte-aligned** destination (every
    /// `<<V:8>>`/`<<V:16>>`/… segment the compiler emits for real data) is the
    /// value's big-endian bytes, so it takes the whole-byte path instead of a
    /// `push_bit` per bit. Every other width/alignment keeps the bit loop, which is
    /// what makes a bit-granular field exact.
    pub(super) fn push_uint(
        &mut self,
        value: u64,
        width: usize,
        machine: &mut Machine,
    ) -> Result<()> {
        self.ensure_room(machine, width)?;
        if width % 8 == 0 && width <= 64 && self.bit_len() % 8 == 0 {
            let nbytes = width / 8;
            if nbytes != 0 {
                let bytes = value.to_be_bytes();
                self.push_whole_bytes(&bytes[bytes.len() - nbytes..]);
            }
            return Ok(());
        }
        for k in (0..width).rev() {
            self.push_bit(((value >> k) & 1) as u8);
        }
        Ok(())
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
    pub(super) fn push_uint_le(
        &mut self,
        value: u64,
        width: usize,
        machine: &mut Machine,
    ) -> Result<()> {
        self.ensure_room(machine, width)?;
        // A whole number of bytes to a byte-aligned destination is the value's low
        // `width / 8` little-endian bytes — byte-identical to what
        // [`BitWriter::push_le_groups`] writes for the same field (`<<16#0102:16>>`
        // is `<<2,1>>` on both routes), just without a `push_bit` per bit.
        if width % 8 == 0 && width <= 64 && self.bit_len() % 8 == 0 {
            let nbytes = width / 8;
            if nbytes != 0 {
                let bytes = value.to_le_bytes();
                self.push_whole_bytes(&bytes[..nbytes]);
            }
            return Ok(());
        }
        let mut bytes = [0u8; 8];
        for (j, byte) in bytes.iter_mut().enumerate() {
            *byte = ((value >> (8 * j)) & 0xFF) as u8;
        }
        self.push_le_groups(&bytes, width);
        Ok(())
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
        // No trailing partial group to place and a byte-aligned destination: the
        // groups ARE the source bytes in order, so write them whole.
        if rem == 0 && self.bit_len() % 8 == 0 {
            if nbytes != 0 {
                self.push_whole_bytes(&bytes[..nbytes]);
            }
            return;
        }
        for (j, &byte) in bytes.iter().enumerate().take(nbytes) {
            let bits = if j + 1 == nbytes && rem != 0 { rem } else { 8 };
            for k in (0..bits).rev() {
                self.push_bit((byte >> k) & 1);
            }
        }
    }

    /// Append `count` bits from `src` (MSB-first) starting at bit `start`.
    ///
    /// A segment source that **overlaps** the in-place payload is copied out first
    /// (promotion), so a read is never interleaved with the writes of the same build.
    /// The **byte-aligned** case (`start` and the current position both on a byte
    /// boundary) copies whole bytes — the fast path a `binary`/`append`/`string`
    /// segment of real data always takes — and only the trailing partial byte goes
    /// through the per-bit writer, which keeps every unaligned case bit-exact.
    pub(super) fn push_bits_from(
        &mut self,
        src: &[u8],
        start: usize,
        count: usize,
        machine: &mut Machine,
    ) -> Result<()> {
        // ORDER IS LOAD-BEARING: the aliasing check promotes (copying the payload out)
        // before any growth can move it, so no live `&[u8]` ever names a relocated payload.
        if self.source_overlaps(src) {
            self.promote();
        }
        self.ensure_room(machine, count)?;
        let mut done = 0usize;
        if start % 8 == 0 && self.bit_len() % 8 == 0 {
            let whole = count / 8;
            if whole != 0 {
                self.push_whole_bytes(&src[start / 8..start / 8 + whole]);
                done = whole * 8;
            }
        }
        for i in done..count {
            let bit_index = start + i;
            let bit = (src[bit_index / 8] >> (7 - (bit_index % 8))) & 1;
            self.push_bit(bit);
        }
        Ok(())
    }

    /// Append `bytes` verbatim to a byte-aligned destination.
    ///
    /// The caller guarantees `bytes` does **not** overlap the in-place payload (the
    /// promotion in [`BitWriter::push_bits_from`] establishes that), so the in-place arm
    /// may use a non-overlapping copy.
    #[inline]
    fn push_whole_bytes(&mut self, bytes: &[u8]) {
        if let Dest::InPlace(sink) = &self.dest {
            if sink.remaining_bits() < bytes.len() * 8 {
                self.promote();
            }
        }
        match &mut self.dest {
            Dest::Vec {
                bytes: buf,
                nbits,
            } => {
                buf.extend_from_slice(bytes);
                *nbits += bytes.len() * 8;
            }
            Dest::InPlace(sink) => sink.copy_bytes(bytes),
        }
    }

    /// The assembled bytes and the exact bit count.
    ///
    /// For an in-place build this **copies out** the accumulated bits (the promotion
    /// half of [`Dest`]) without publishing them; the builder itself uses
    /// [`BitWriter::finish_built`]. Only the writer's own unit tests — which always
    /// drive the copy-rebuild form — need the byte view.
    #[cfg(test)]
    pub(super) fn finish(self) -> (Vec<u8>, usize) {
        match self.dest {
            Dest::Vec { bytes, nbits } => (bytes, nbits),
            Dest::InPlace(sink) => sink.into_bytes(),
        }
    }

    /// The finished build: the accumulator term itself for an in-place build, the
    /// assembled bytes otherwise.
    pub(super) fn finish_built(self) -> Built {
        match self.dest {
            Dest::Vec { bytes, nbits } => Built::Bytes(bytes, nbits),
            Dest::InPlace(sink) => Built::Term(sink.finish()),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use beam_loader::Module;

    /// **The family entry installs the exception-class anchor itself**, so a caller that
    /// bypasses the router (the native baseline tier's in-block helper) still gets a raise
    /// that lands in a live `try`/`catch` frame.
    ///
    /// The three class atoms are resolved from the engine's live namespace on [`exec`]'s own
    /// entry — not by a caller and not only by [`crate::families::dispatch`]'s pre-routing
    /// latch — which is what makes the router an optimisation rather than a precondition.
    #[test]
    fn exec_installs_the_class_atom_anchor_for_a_direct_caller() {
        // A module that names none of the class atoms; the host's live namespace names them.
        let module = Module::default();
        let mut live = module.atom_table.clone();
        for name in ["error", "exit", "throw"] {
            live.intern(name);
        }
        let error = live.lookup("error").expect("the namespace names error");
        let mut ctx = crate::context::MinimalContext::with_shared_atoms(live);
        let labels = LabelMap::new();
        let mut machine = Machine::new();
        machine.push_frame(0).expect("a frame");
        machine.push_catch_frame(crate::machine::CatchFrame::new(2, 5, 0));
        assert!(!machine.class_atoms_live, "a fresh machine has no anchor");

        // An instruction this family **declines** is enough: the anchor is the entry's own
        // responsibility and is installed before the variant check.
        let declined = Instr::Move {
            src: Operand::Nil,
            dst: Operand::X(0),
        };
        assert!(!claimed(&declined), "a `move` is not the family's");
        let step = exec(&mut machine, &module, &labels, &declined, 0, 0, &mut ctx)
            .expect("a declined variant is `Ok(None)`, not an error");
        assert!(step.is_none(), "the family declines a `move`");
        assert!(
            machine.class_atoms_live,
            "the family entry installed the anchor itself"
        );

        // A class-`error` raise therefore lands in the live frame instead of being abandoned
        // with a typed `unsupported`.
        let step = machine
            .throw_error(Eterm::from_raw(0x77))
            .expect("the class atom came from the live namespace");
        assert_eq!(step, Step::Goto { func: 2, pc: 5 });
        assert_eq!(machine.x(3), Some(make_atom(error)), "x(3) = the class atom");
    }

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
    /// cleared (`native` is the `big` default there). The build side's little-endian
    /// writers select their path from `BSF_LITTLE` alone (a raw `u32` flags operand
    /// keeps `BSF_NATIVE` and is resolved by `matching::reads::field_is_little`
    /// instead), so a `native` *flag atom* must never keep `BSF_NATIVE` — on this
    /// little-endian target such a segment would otherwise be written **big-endian**,
    /// exactly backwards (`<<16#0102:16/native>>` is `<<2,1>>` in the oracle, `<<1,2>>`
    /// for the big-endian default).
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
            "`native` must never survive as BSF_NATIVE on the flag-atom route"
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
            let mut machine = Machine::new();
            writer
                .push_uint_le(value, width, &mut machine)
                .expect("the copy-rebuild destination never refuses room");
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

    /// The bit vector (MSB-first) of the low `width` bits of `value` — the field a
    /// big-endian segment holds.
    fn be_bits(value: u64, width: usize) -> Vec<u8> {
        (0..width).rev().map(|k| ((value >> k) & 1) as u8).collect()
    }

    /// The little-endian group layout [`BitWriter::push_le_groups`] documents, as a
    /// bit vector (MSB-first): `bytes[0]`'s bits, then `bytes[1]`'s, …, with the
    /// final partial group carrying `bytes`'s low remaining bits left-aligned.
    fn le_bits(value: u64, width: usize) -> Vec<u8> {
        let bytes = value.to_le_bytes();
        let rem = width % 8;
        let nbytes = width.div_ceil(8);
        let mut bits = Vec::new();
        for (j, &byte) in bytes.iter().enumerate().take(nbytes) {
            let n = if j + 1 == nbytes && rem != 0 { rem } else { 8 };
            for k in (0..n).rev() {
                bits.push((byte >> k) & 1);
            }
        }
        bits
    }

    /// Pack an MSB-first bit vector into `(bytes, bit count)`.
    fn pack(bits: &[u8]) -> (Vec<u8>, usize) {
        let mut bytes = vec![0u8; bits.len().div_ceil(8)];
        for (i, &bit) in bits.iter().enumerate() {
            if bit != 0 {
                bytes[i / 8] |= 1 << (7 - (i % 8));
            }
        }
        (bytes, bits.len())
    }

    /// The whole-byte fast paths of [`BitWriter::push_uint`]/[`push_uint_le`] are
    /// **byte-identical** to the per-bit rule at every width — the fast path is taken
    /// exactly when the destination is byte-aligned and the field is a whole number of
    /// bytes, and the bit loop keeps every other shape.
    #[test]
    fn the_whole_byte_writers_match_the_bit_rule_at_every_width() {
        let mut machine = Machine::new();
        for width in 0..=64usize {
            for value in [0u64, 1, 0x02, 0x0102, 0xABC, 0x1ABC, 0x80, u64::MAX] {
                let mut writer = BitWriter::default();
                writer
                    .push_uint(value, width, &mut machine)
                    .expect("the copy-rebuild destination never refuses room");
                assert_eq!(
                    writer.finish(),
                    pack(&be_bits(value, width)),
                    "big-endian {value:#x}:{width}"
                );

                let mut writer = BitWriter::default();
                writer
                    .push_uint_le(value, width, &mut machine)
                    .expect("the copy-rebuild destination never refuses room");
                assert_eq!(
                    writer.finish(),
                    pack(&le_bits(value, width)),
                    "little-endian {value:#x}:{width}"
                );
            }
        }
    }

    /// A whole-byte width written to a destination that is **not** byte-aligned keeps
    /// the bit loop, so the field lands bit-granularly instead of being shifted a
    /// whole byte.
    #[test]
    fn a_whole_byte_width_after_a_partial_bit_stays_bit_granular() {
        let mut machine = Machine::new();
        for (width, value) in [(8usize, 0x02u64), (16, 0x0102), (32, 0x0000_0102)] {
            let mut writer = BitWriter::default();
            for _ in 0..4 {
                writer.push_bit(0);
            }
            writer
                .push_uint(value, width, &mut machine)
                .expect("the copy-rebuild destination never refuses room");
            let mut expect = vec![0u8; 4];
            expect.extend(be_bits(value, width));
            assert_eq!(writer.finish(), pack(&expect), "big-endian {value:#x}:{width}");

            let mut writer = BitWriter::default();
            for _ in 0..4 {
                writer.push_bit(0);
            }
            writer
                .push_uint_le(value, width, &mut machine)
                .expect("the copy-rebuild destination never refuses room");
            let mut expect = vec![0u8; 4];
            expect.extend(le_bits(value, width));
            assert_eq!(
                writer.finish(),
                pack(&expect),
                "little-endian {value:#x}:{width}"
            );
        }
    }

    /// The wide (`fmt_big_be`) writer's byte-aligned branch writes the same field as
    /// its per-bit loop (`<<16#0102:32>>` is `<<0,0,1,2>>`), and a non-multiple-of-8
    /// width stays bit-granular through the loop.
    #[test]
    fn the_wide_big_endian_writer_matches_the_bit_rule() {
        let mut machine = Machine::new();
        for nbits in [8usize, 12, 16, 24, 32] {
            let mut writer = BitWriter::default();
            push_wide_int(
                &mut writer,
                beam_term::make_small(0x0102),
                nbits,
                false,
                &mut machine,
            )
            .expect("the copy-rebuild destination never refuses room");
            assert_eq!(
                writer.finish(),
                pack(&be_bits(0x0102, nbits)),
                "big-endian wide {nbits}"
            );
        }
        // The oracle shape: `<<16#0102:32>>` is `<<0,0,1,2>>`.
        let mut writer = BitWriter::default();
        push_wide_int(
            &mut writer,
            beam_term::make_small(0x0102),
            32,
            false,
            &mut machine,
        )
        .expect("the copy-rebuild destination never refuses room");
        assert_eq!(writer.finish(), (vec![0x00, 0x00, 0x01, 0x02], 32));
    }

    /// The byte-aligned reader/the byte-aligned extractor agree with the per-bit rule
    /// they replace on every start/width pair (both paths are exercised: the aligned
    /// one is taken only when `start` **and** `nbits` are whole bytes).
    #[test]
    fn the_byte_aligned_readers_agree_with_the_bit_rule() {
        let bytes: Vec<u8> = (0..16u8).map(|i| i.wrapping_mul(37).wrapping_add(11)).collect();
        let bits_of = |start: usize, nbits: usize| -> Vec<u8> {
            (0..nbits)
                .map(|i| {
                    let index = start + i;
                    (bytes[index / 8] >> (7 - (index % 8))) & 1
                })
                .collect()
        };
        for start in 0..8usize {
            for nbits in 0..=64usize {
                let bits = bits_of(start, nbits);
                let mut value = 0u64;
                for &bit in &bits {
                    value = (value << 1) | u64::from(bit);
                }
                assert_eq!(
                    read_uint_be(&bytes, start, nbits).expect("in range"),
                    value,
                    "read_uint_be {start}+{nbits}"
                );
                assert_eq!(
                    extract_bits(&bytes, start, nbits).expect("in range"),
                    pack(&bits).0,
                    "extract_bits {start}+{nbits}"
                );
            }
        }
        // Out of range stays a typed error on both readers.
        assert!(read_uint_be(&bytes, 120, 16).is_err());
        assert!(extract_bits(&bytes, 120, 16).is_err());
    }

    /// A size operand's accept rule is unchanged by the literal-integer fast path: a
    /// non-negative in-range integer is the size, and a negative/oversized one is the
    /// same typed error the general read produced.
    #[test]
    fn a_literal_integer_size_operand_is_classified_exactly_as_before() {
        let machine = Machine::new();
        let mut ctx = crate::context::MinimalContext::new();
        for n in [0i64, 1, 8, 64, beam_term::MAX_SMALL] {
            assert_eq!(
                read_size(&machine, &mut ctx, &Operand::Integer(n)).expect("in range"),
                n as usize
            );
        }
        for n in [-1i64, -8, beam_term::MIN_SMALL, beam_term::MAX_SMALL + 1, i64::MAX] {
            assert!(
                read_size(&machine, &mut ctx, &Operand::Integer(n)).is_err(),
                "{n} cannot size a segment"
            );
        }
        assert_eq!(
            read_size(&machine, &mut ctx, &Operand::Character(65)).expect("in range"),
            65
        );
        // `Character` is a `u32`, so it is always a non-negative small.
        assert_eq!(
            read_size(&machine, &mut ctx, &Operand::Character(u32::MAX)).expect("in range"),
            u32::MAX as usize
        );
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
