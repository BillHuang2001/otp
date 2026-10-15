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

//! The baseline tier's **`send` / receive-control** helper.
//!
//! [`jit_receive`] is emitted for the seven opcodes that drive the copy-bound
//! message-passing path — `send`, `loop_rec`, `loop_rec_end`, `wait`,
//! `wait_timeout`, `timeout` and `remove_message` (see `../emit/block.rs`). Left to the
//! generic per-instruction deopt they each paid a whole interpreter step
//! ([`jit_step_one`] → [`Machine::step_instruction`](crate::Machine)) on every mailbox
//! operation, which is the dominant instruction class of a `send`/`receive` loop.
//!
//! The helper runs the instruction **without** the interpreter's generic dispatch: it
//! resolves the instruction at `(func, pc)` in the active module and hands it to the
//! receive family's own body ([`crate::families::dispatch`], the same entry the
//! interpreter's `other` arm reaches), so the semantics are the family's **by
//! construction** — `send`'s `x(0) = Message` and its `raise_bif_error` landing, a
//! `loop_rec` candidate's `write_operand`/fail-label branch, the mailbox seam calls,
//! and the suspend a `wait`/`wait_timeout` reports.
//!
//! The body is the instruction-agnostic [`super::family::jit_family`] — the one
//! implementation every family-owned shape the emitter claims shares — so this module
//! carries only the receive family's shape names (the emitter's `emit_receive` is
//! likewise the same emission sequence as `emit_family`). What that shared helper does
//! beyond the family body is exactly [`jit_step_one`]'s:
//!
//! * the position store ([`Machine::set_current_code`]) the interpreter's step makes
//!   at entry, so `Machine::current_code` keeps the instruction granularity a raise
//!   and a parked `receive` rely on;
//! * the context-`0` half of the uniform raise funnel
//!   ([`Machine::with_uncaught_frames`]) — in the run's entry context the active module
//!   *is* the entry module, so the frames are attached here, while a raise inside a
//!   cross-module callee is left for the driver, which knows the true entry module;
//! * the [`fold_step`] encoding of the resulting [`Step`](crate::Step) into the block
//!   ABI's word, so a `Step::Wait` reaches the driver as [`encode_wait`]'s word and a
//!   `Step::Next` as "the next instruction" — which the emitted continuation check
//!   turns back into an in-block continue.
//!
//! [`encode_wait`]: super::encode_wait

use super::family::jit_family;
use super::runtime::JitRuntime;

/// Run the `send`/receive-control instruction at `(func, pc)` natively and encode its
/// outcome as a block-ABI word.
///
/// A thin wrapper over the shared [`jit_family`] — the receive family's shape names,
/// nothing more — so the emitted sequence's contract (`emit_receive` in
/// [`super::emit`]) is the one implementation's.
#[inline]
pub(crate) extern "C" fn jit_receive(rt: *mut JitRuntime<'_>, func: u32, pc: u32) -> i64 {
    jit_family(rt, func, pc)
}
