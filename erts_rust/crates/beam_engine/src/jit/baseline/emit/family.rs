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
//! The emitter arm for the **instruction families** the interpreter's dispatch loop does
//! not arm itself: [`emit_family`] is the emit-side half of the family-helper lowering
//! (see [`super::super::family`]).
//!
//! The shape set is decided by `emit_block`'s own dispatch (the one arm naming the
//! variants it claims), not here: this module only owns the *emitted sequence*, which is
//! the shared helper-call contract — spill, call, then the continuation check — so the
//! family-helper shape is exactly as cheap as the receive shape (`emit_receive` is the
//! same sequence) and every un-lowered instruction keeps its per-instruction deopt.

use beam_common::Result;

use crate::jit::emitter::{CodeBuffer, Label as CodeLabel};

use super::super::family::jit_family;
use super::super::runtime::StepHelper;
use super::{emit_continue_check, emit_helper_call, Allocation};

/// Emit the native arm for one family-owned instruction: the
/// [`jit_family`](super::super::family::jit_family) helper followed by the same
/// continuation check a deopt uses.
///
/// The helper has [`StepHelper`]'s exact signature (`rt`, the instruction's `(func, pc)`)
/// and runs the owning family's own body, so it reuses
/// [`emit_helper_call`] + [`emit_continue_check`] — the spill-then-call contract and the
/// partial refill after the call included. It writes any destination **through
/// `machine.x`** (like every other helper), which is why the variants it serves
/// contribute no [`cacheable register`](super::regalloc) touches or writes: the
/// continuation's refill is what keeps a cached register coherent, and every register a
/// later instruction reads natively is in that refill set by construction.
///
/// The outcomes the emitted code distinguishes are the block ABI's:
///
/// * `encode_target(func, pc + 1)` — the family reported `Step::Next`, so the block
///   continues in-block;
/// * anything else — a family `Goto` (a fail-label branch), the `Wait` word of a
///   suspension, an `ERROR` — leaves the block with the word in `rax`, which the driver
///   decodes exactly as it decodes a deopt's.
pub(super) fn emit_family(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
) -> Result<()> {
    let helper: StepHelper = jit_family;
    emit_helper_call(
        buffer,
        allocation,
        helper as *const (),
        &[i64::from(func), i64::from(pc)],
    )?;
    emit_continue_check(buffer, done, allocation, func, pc)
}
