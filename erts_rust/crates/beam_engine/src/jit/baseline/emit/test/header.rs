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

//! The baseline JIT's boxed-term **header predicate emitters**: the shared header load
//! (`load_boxed_header`) and header-equality test (`header_matches`) primitives, the
//! word/immediate tag helpers (`jump_if_small`/`jump_if_immediate`) they build on, and
//! the per-kind `emit_is_*` lowerings the parent `test` module branches to for its
//! boxed-term `test` kinds.
//!
//! The mask/tag constants these emitters test with are shared with the shape test, so
//! they stay in the parent `test` module and are read here through `use super::{…}` —
//! no constant is duplicated. A child module of `test`, so `super` is that module.

use beam_common::Result;

use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};

use super::super::arith::{FAST_C, SMALL_MASK};
use super::{
    ARITYVAL_SHIFT, BIG_MASK, BITSTRING_MASK, BOXED_TAG, BYTE_MASK, EXTERNAL_PID_SUBTAG,
    EXTERNAL_PORT_SUBTAG, EXTERNAL_REF_SUBTAG, FUN_ARITY_MASK, FUN_ARITY_SHIFT, FUN_SUBTAG,
    HEADER_FLONUM, HEADER_KIND_MASK, HEADER_MASK, HEADER_SUB_BITS, HEAP_BITS_SUBTAG, IMMED1_MASK,
    INTERNAL_PID_TAG, INTERNAL_PORT_MASK, INTERNAL_PORT_TAG, POS_BIG_SUBTAG, PRIMARY_MASK,
    PTR_MASK, REF_SUBTAG, TEST_A, TEST_B, TEST_C, TEST_H, TEST_T,
};

// ---------------------------------------------------------------------------
// Header-test helpers
// ---------------------------------------------------------------------------

/// Load the **header word** of the boxed term in `value` into `out`, leaving its
/// masked pointer in `ptr`, or branch to `failed` when `value` is not a boxed term.
///
/// `(value & 3) == TAG_PRIMARY_BOXED` is exactly `beam_term::is_boxed_term`
/// (`is_boxed(value) && is_term_word(value)`), so passing it makes the single
/// `[ptr]` read below — the one word `beam_term`'s own predicate reads — safe. A
/// non-boxed word is a definite `false` for **every** header predicate, so it takes
/// the fail path without any dereference.
pub(super) fn load_boxed_header(
    buffer: &mut CodeBuffer,
    value: Reg,
    ptr: Reg,
    out: Reg,
    failed: CodeLabel,
) -> Result<()> {
    buffer.mov_ri(FAST_C, PRIMARY_MASK)?;
    buffer.mov_rr(ptr, value)?;
    buffer.and_rr(ptr, FAST_C)?;
    buffer.mov_ri(out, BOXED_TAG)?;
    buffer.cmp_rr(ptr, out)?;
    buffer.jcc(Cond::Ne, failed)?;
    // `ptr` = boxed_val(value): mask off TAG_PTR_MASK and read the header word.
    //
    // `ptr` currently holds the *primary tag* of `value` (the `2` the guard just
    // matched), **not** `value`, so reload the term word first: masking the tag
    // value with `PTR_MASK` would leave `2 & !0x7 == 0` and the header read below
    // would dereference a null pointer.
    buffer.mov_rr(ptr, value)?;
    buffer.mov_ri(FAST_C, PTR_MASK)?;
    buffer.and_rr(ptr, FAST_C)?;
    buffer.mov_rm(out, ptr, 0)
}

/// `jcc E, success` when `(reg & mask) == tag` — the header predicate itself.
///
/// `reg` is **preserved**: the masked value is staged in [`TEST_H`], not in `reg`, so a
/// caller that still needs the header word (the `HEADER_FLONUM` branch of
/// [`emit_is_number`], the `HEADER_SUB_BITS` branch of [`emit_is_binary`], the arity
/// field of [`emit_is_fun_arity`]) can keep reading it.
pub(super) fn header_matches(
    buffer: &mut CodeBuffer,
    reg: Reg,
    mask: i64,
    tag: i64,
    success: CodeLabel,
) -> Result<()> {
    buffer.mov_ri(FAST_C, mask)?;
    buffer.mov_rr(TEST_H, reg)?;
    buffer.and_rr(TEST_H, FAST_C)?;
    buffer.mov_ri(FAST_C, tag)?;
    buffer.cmp_rr(TEST_H, FAST_C)?;
    buffer.jcc(Cond::E, success)
}

/// `jcc E, success` when `(word & 0xF) == 0xF` — the small-int immediate test
/// (`beam_term::is_small`).
fn jump_if_small(buffer: &mut CodeBuffer, reg: Reg, success: CodeLabel) -> Result<()> {
    buffer.mov_ri(FAST_C, SMALL_MASK)?;
    buffer.mov_rr(TEST_T, reg)?;
    buffer.and_rr(TEST_T, FAST_C)?;
    buffer.cmp_rr(TEST_T, FAST_C)?;
    buffer.jcc(Cond::E, success)
}

/// `jcc E, success` when the immediate tag test `(word & mask) == tag` holds (an
/// internal pid/port).
fn jump_if_immediate(
    buffer: &mut CodeBuffer,
    reg: Reg,
    mask: i64,
    tag: i64,
    success: CodeLabel,
) -> Result<()> {
    buffer.mov_ri(FAST_C, mask)?;
    buffer.mov_rr(TEST_T, reg)?;
    buffer.and_rr(TEST_T, FAST_C)?;
    buffer.mov_ri(TEST_C, tag)?;
    buffer.cmp_rr(TEST_T, TEST_C)?;
    buffer.jcc(Cond::E, success)
}

/// `is_integer`: a small word, else a boxed bignum header.
pub(super) fn emit_is_integer(
    buffer: &mut CodeBuffer,
    success: CodeLabel,
    failed: CodeLabel,
) -> Result<()> {
    jump_if_small(buffer, TEST_A, success)?;
    load_boxed_header(buffer, TEST_A, TEST_C, TEST_T, failed)?;
    header_matches(buffer, TEST_T, BIG_MASK, POS_BIG_SUBTAG, success)
}

/// `is_number`: a small word, else a boxed bignum or `HEADER_FLONUM` header.
pub(super) fn emit_is_number(
    buffer: &mut CodeBuffer,
    success: CodeLabel,
    failed: CodeLabel,
) -> Result<()> {
    jump_if_small(buffer, TEST_A, success)?;
    load_boxed_header(buffer, TEST_A, TEST_C, TEST_T, failed)?;
    header_matches(buffer, TEST_T, BIG_MASK, POS_BIG_SUBTAG, success)?;
    buffer.mov_ri(FAST_C, HEADER_FLONUM)?;
    buffer.cmp_rr(TEST_T, FAST_C)?;
    buffer.jcc(Cond::E, success)
}

/// `is_float`: a boxed `HEADER_FLONUM`.
pub(super) fn emit_is_float(
    buffer: &mut CodeBuffer,
    success: CodeLabel,
    failed: CodeLabel,
) -> Result<()> {
    load_boxed_header(buffer, TEST_A, TEST_C, TEST_T, failed)?;
    buffer.mov_ri(FAST_C, HEADER_FLONUM)?;
    buffer.cmp_rr(TEST_T, FAST_C)?;
    buffer.jcc(Cond::E, success)
}

/// `is_binary`: a bitstring header whose `bitstring_size` is byte-aligned. The size is
/// read exactly as `beam_term::boxed::bitstring_size` reads it — `p[3] - p[2]` for an
/// `ErlSubBits` (`HEADER_SUB_BITS`), otherwise the heap-bits size word `p[1]`.
pub(super) fn emit_is_binary(
    buffer: &mut CodeBuffer,
    success: CodeLabel,
    failed: CodeLabel,
) -> Result<()> {
    load_boxed_header(buffer, TEST_A, TEST_C, TEST_T, failed)?;
    let bits = buffer.label();
    let sub_bits = buffer.label();
    let size_ready = buffer.label();
    header_matches(buffer, TEST_T, BITSTRING_MASK, HEAP_BITS_SUBTAG, bits)?;
    buffer.jmp(failed)?;
    buffer.bind(bits)?;
    buffer.mov_ri(FAST_C, HEADER_SUB_BITS)?;
    buffer.cmp_rr(TEST_T, FAST_C)?;
    buffer.jcc(Cond::E, sub_bits)?;
    // ErlHeapBits: [thing_word, size, data...]
    buffer.mov_rm(TEST_T, TEST_C, 8)?;
    buffer.jmp(size_ready)?;
    buffer.bind(sub_bits)?;
    // ErlSubBits: [thing_word, base_flags, start, end, orig]
    buffer.mov_rm(TEST_T, TEST_C, 16)?;
    buffer.mov_rm(TEST_C, TEST_C, 24)?;
    buffer.sub_rr(TEST_C, TEST_T)?;
    buffer.mov_rr(TEST_T, TEST_C)?;
    buffer.bind(size_ready)?;
    buffer.mov_ri(FAST_C, BYTE_MASK)?;
    buffer.and_rr(TEST_T, FAST_C)?;
    buffer.jcc(Cond::E, success)
}

/// `is_tuple_arity`: the full arity-value header equality (`hdr == arity << 6`).
pub(super) fn emit_is_arity(
    buffer: &mut CodeBuffer,
    arity: i64,
    success: CodeLabel,
    failed: CodeLabel,
) -> Result<()> {
    load_boxed_header(buffer, TEST_A, TEST_C, TEST_T, failed)?;
    buffer.mov_ri(FAST_C, arity << ARITYVAL_SHIFT)?;
    buffer.cmp_rr(TEST_T, FAST_C)?;
    buffer.jcc(Cond::E, success)
}

/// `is_function2`: a fun header whose arity field equals `arity`.
pub(super) fn emit_is_fun_arity(
    buffer: &mut CodeBuffer,
    arity: i64,
    success: CodeLabel,
    failed: CodeLabel,
) -> Result<()> {
    load_boxed_header(buffer, TEST_A, TEST_C, TEST_T, failed)?;
    let is_fun = buffer.label();
    header_matches(buffer, TEST_T, HEADER_KIND_MASK, FUN_SUBTAG, is_fun)?;
    buffer.jmp(failed)?;
    buffer.bind(is_fun)?;
    // `beam_term::fun::fun_arity` is `(hdr >> FUN_HEADER_ARITY_OFFS) & FUN_MAX_FIELD`
    // (an arithmetic shift is identical under the `& 0xFF` mask).
    buffer.sar_ri(TEST_T, FUN_ARITY_SHIFT)?;
    buffer.mov_ri(FAST_C, FUN_ARITY_MASK)?;
    buffer.and_rr(TEST_T, FAST_C)?;
    buffer.mov_ri(TEST_C, arity)?;
    buffer.cmp_rr(TEST_T, TEST_C)?;
    buffer.jcc(Cond::E, success)
}

/// `is_tagged_tuple`: an arity-value header of `arity` whose element 0 equals the tag
/// already loaded into `TEST_B` (`beam_term::tuple_element(v, 0)`).
pub(super) fn emit_is_tagged_tuple(
    buffer: &mut CodeBuffer,
    arity: i64,
    success: CodeLabel,
    failed: CodeLabel,
) -> Result<()> {
    load_boxed_header(buffer, TEST_A, TEST_C, TEST_T, failed)?;
    buffer.mov_ri(FAST_C, arity << ARITYVAL_SHIFT)?;
    buffer.cmp_rr(TEST_T, FAST_C)?;
    buffer.jcc(Cond::Ne, failed)?;
    buffer.mov_rm(TEST_T, TEST_C, 8)?;
    buffer.cmp_rr(TEST_T, TEST_B)?;
    buffer.jcc(Cond::E, success)
}

/// `is_pid`: an internal-pid immediate or a boxed external-pid header.
pub(super) fn emit_is_pid(
    buffer: &mut CodeBuffer,
    success: CodeLabel,
    failed: CodeLabel,
) -> Result<()> {
    jump_if_immediate(buffer, TEST_A, IMMED1_MASK, INTERNAL_PID_TAG, success)?;
    load_boxed_header(buffer, TEST_A, TEST_C, TEST_T, failed)?;
    header_matches(buffer, TEST_T, HEADER_MASK, EXTERNAL_PID_SUBTAG, success)
}

/// `is_port`: an internal-port immediate or a boxed external-port header.
pub(super) fn emit_is_port(
    buffer: &mut CodeBuffer,
    success: CodeLabel,
    failed: CodeLabel,
) -> Result<()> {
    jump_if_immediate(
        buffer,
        TEST_A,
        INTERNAL_PORT_MASK,
        INTERNAL_PORT_TAG,
        success,
    )?;
    load_boxed_header(buffer, TEST_A, TEST_C, TEST_T, failed)?;
    header_matches(buffer, TEST_T, HEADER_MASK, EXTERNAL_PORT_SUBTAG, success)
}

/// `is_reference`: a boxed internal or external reference header.
pub(super) fn emit_is_reference(
    buffer: &mut CodeBuffer,
    success: CodeLabel,
    failed: CodeLabel,
) -> Result<()> {
    load_boxed_header(buffer, TEST_A, TEST_C, TEST_T, failed)?;
    header_matches(buffer, TEST_T, HEADER_MASK, REF_SUBTAG, success)?;
    header_matches(buffer, TEST_T, HEADER_MASK, EXTERNAL_REF_SUBTAG, success)
}
