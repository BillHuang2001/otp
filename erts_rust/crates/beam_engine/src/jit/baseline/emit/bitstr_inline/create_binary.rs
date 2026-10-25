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

//! The **`binary all`** route of a natively lowered `bs_create_bin`: one positioned helper
//! call, no field phase and no emitted window-end commit.
//!
//! The shape is the compiler's accumulator with a single whole-binary tail
//! (`<<Acc/binary, B/binary>>` — `seam::AppendPlan::binary_all`). The source's width is a
//! run-time figure, so there is no static width to write at a static offset: the whole
//! instruction is a run-time-length copy, and the machinery around it (the writable-payload
//! registry proof, the payload write, the term commit) lives in the machine. The tier's
//! contribution is therefore small and entirely static:
//!
//! 1. the plan is proved at **compile** time
//!    ([`CreateBinShape::of`](super::CreateBinShape::of): the accumulator and the source both
//!    resolve to an in-bound x-register or an addressable `Y` slot — a literal-area or
//!    out-of-bound operand refuses the shape, emitting nothing);
//! 2. the up-front guards the other routes share still run first (the destination's
//!    frame-window guard, the accumulator's `Y` guard, and this route's own `Y` guard for the
//!    source), so a rejected slot writes nothing;
//! 3. **one** positioned call to
//!    [`jit_append_binary_all`](crate::jit::baseline::runtime::jit_append_binary_all), whose
//!    `rax` answer is the accumulator term on success and `0` for every refusal:
//!    `test rax, rax ; je fallback`;
//! 4. the accumulator word into the destination register (the shared phase (6) of
//!    [`try_inline_bs_create_bin`](super::try_inline_bs_create_bin)).
//!
//! **No window-end commit is emitted here**: the helper's single `ErlSubBits.end` write *is*
//! the commit, and it happens only after every one of the helper's guards passed. A refusal
//! therefore leaves the accumulator's payload and term word byte-identical, which is what
//! makes the `fallback` — one whole-instruction interpreter step re-executing this very
//! instruction with the same operands — idempotent.
//!
//! The refusals the helper answers are the shapes whose copy the tier does not express: a
//! non-writable or non-refc accumulator, a window end that is not byte-aligned, a
//! non-refc/non-byte-aligned/oversized source, and a payload without room. Each is one
//! interpreter step, so the family's own value, fail transfer or raise (a source size that is
//! not a whole number of the field's `unit` is the family's `badarg`) is what the process
//! observes.

use beam_common::Result;

use crate::jit::baseline::emit::{emit_positioned_helper_call, Allocation};
use crate::jit::baseline::runtime::{
    jit_append_binary_all, AppendBinaryAllHelper, WRITE_ACC_IN_Y, WRITE_VALUE_IN_Y,
};
use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};

use super::Loc;
/// Emit the `binary all` route: the one positioned helper call and its refusal branch.
///
/// Callers emit this **last** among the guards (the destination's and both `Y` operands'
/// frame-window guards have already run, so nothing here can write to an out-of-frame slot),
/// and then the shared accumulator-into-destination phase. The helper performs the append's
/// only mutation, so this function adds none.
///
/// Both terms cross as **slot indices** plus one flags word
/// ([`WRITE_ACC_IN_Y`]/[`WRITE_VALUE_IN_Y`]) — all three are compile-time constants the
/// emitter writes as `movabs` immediates, because no run-time word can cross the block ABI.
pub(super) fn emit_binary_all(
    buffer: &mut CodeBuffer,
    shape: &super::CreateBinShape,
    src: Loc,
    allocation: &Allocation,
    fallback: CodeLabel,
    func: u32,
    pc: u32,
) -> Result<()> {
    let src_slot = match src {
        Loc::X(x) => i64::from(x),
        Loc::Y(k) => i64::from(k),
    };
    let flags = if shape.source.is_y() {
        WRITE_ACC_IN_Y
    } else {
        0
    } | if src.is_y() { WRITE_VALUE_IN_Y } else { 0 };
    let helper: AppendBinaryAllHelper = jit_append_binary_all;
    emit_positioned_helper_call(
        buffer,
        allocation,
        helper as *const (),
        &[i64::from(shape.source_slot()), src_slot, flags],
        func,
        pc,
    )?;
    buffer.test_rr(Reg::Rax, Reg::Rax)?;
    buffer.jcc(Cond::E, fallback)
}
