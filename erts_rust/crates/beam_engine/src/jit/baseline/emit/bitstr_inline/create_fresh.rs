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
//! The **bare** `bs_create_bin` build ([`super::Route::Fresh`]): a construction whose segments
//! carry no `private_append` group at all, so its fields *are* the whole result
//! (`<<A:8, B:8>>`, `<<Digit:6>>` — the emqx `emqx_base62` digit extraction).
//!
//! Such an instruction has no accumulator: nothing is appended to, no append door is opened,
//! and the payload is written from scratch. `seam::fresh_plan` is the one predicate that
//! decides which segment sets have that form (statically sized big-endian unsigned integer
//! fields over x/y registers, 1..=64 bits each, at most 64 bits in total), and
//! `seam::make_fresh_binary` is the one materialization rule — so this module owns only the
//! emitted bytes.
//!
//! # What this route emits today
//!
//! One `jit_bitstr` family step for the whole instruction — exactly the bytes
//! [`super::create::try_inline_bs_create_bin`]'s own `fallback` emits — followed by the shared
//! `fast_done` binding. The family therefore produces the term, and the route is
//! behaviourally identical to a shape the predicate refused.

use crate::jit::emitter::{CodeBuffer, Label as CodeLabel};

use super::super::Allocation;
use super::CreateBinShape;
use super::emit_fallback_and_done;

use beam_common::Result;

/// Emit the bare-build route for `shape` (see the module docs), or report that the block must
/// helper the instruction (`Ok(false)` with **zero bytes emitted**).
pub(super) fn emit_fresh_build(
    buffer: &mut CodeBuffer,
    _shape: &CreateBinShape,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    _highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let fallback: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();
    emit_fallback_and_done(buffer, allocation, done, func, pc, fallback, fast_done)
}
