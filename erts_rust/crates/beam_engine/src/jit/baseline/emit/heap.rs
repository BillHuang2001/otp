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

//! The baseline JIT's **heap / frame-admission** lowerings: `allocate`,
//! `allocate_heap`, `test_heap` and `trim`.
//!
//! These four instructions touch the two pieces of `Machine` state a compiled block
//! cannot address itself: the **call-frame stack** (`Machine::frames`/`stack`, Rust-side
//! `Vec`s with no layout a block could compute an offset into) and the **process heap**
//! (the host's `ExecutionContext::ensure_heap_full` admission door, which may run a
//! Stage-4 collection). The frame push, the frame trim and the heap admission therefore
//! go through the tier's helpers ([`jit_allocate`], [`jit_allocate_heap`],
//! [`jit_test_heap`], [`jit_trim`], in `super::runtime`), which make the interpreter's own
//! `Machine` calls in the interpreter's own order — but the *instruction itself* no
//! longer needs an interpreter step: the helper performs exactly this instruction's
//! `Machine` work and encodes the outcome as the block-ABI word, and the driver decodes
//! that word exactly as it decodes a deopt's.
//!
//! # The two shapes
//!
//! All four instructions are **non-terminating**: the interpreter's arm returns
//! `Step::Next`, so the emitted code compares the helper's word with
//! `encode_target(func, pc + 1)` and continues in-block on a match, or branches to the
//! block epilogue with the word in `rax` (always the [`ERROR`] word of a failed
//! admission / a failed trim) on anything else. The four differ only in whether the
//! helper can touch `machine.x`:
//!
//! * **`allocate`** ([`emit_allocate`]) and **`trim`** ([`emit_trim`]) touch **no**
//!   x-register: `push_frame` grows `machine.stack` and pushes a `Frame`, and `trim`
//!   shifts the stack and renumbers the frame's Y slots. Like
//!   [`super::emit_deallocate`] they therefore emit **no** register refill — the block's
//!   cached registers still hold exactly what `machine.x` does — but they *are* frame
//!   changes, so they re-publish the Y window on the continue arm when a later
//!   instruction of the block still accesses a Y slot.
//! * **`allocate_heap`** ([`emit_allocate_heap`]) and **`test_heap`**
//!   ([`emit_test_heap`]) reach the host's heap-admission door, and a **collecting** host
//!   relocates the young heap and rewrites every root **in place** — including
//!   `machine.x`. The block's cached x-registers are stale the moment the helper
//!   returns, so both reuse [`super::emit_continue_check`], whose partial refill
//!   ([`super::fill`]) is exactly the `test_heap`/`allocate_heap` post-admission contract
//!   (pinned by `tests.rs::a_collecting_helper_does_not_leave_a_cached_heap_term_stale`).
//!   The Y-window refresh a frame change needs comes from the same
//!   [`super::emit_continue_check`] (gated on [`Allocation::refresh_y_after`]).
//!
//! # Guards
//!
//! There are none to emit. Every operand shape is lowered: a frame size / heap need /
//! live count is a `u32` the helper takes as an argument (`.words()` on the loader's
//! `HeapNeed`, computed at compile time), and `trim`'s `n` likewise. The only way these
//! lowerings fail is a buffer that cannot hold the emitted bytes, which is the shared
//! typed `SystemLimit` compile error — never a wrong result.
//!
//! [`ERROR`]: super::super::runtime::ERROR
//! [`Allocation::refresh_y_after`]: super::Allocation::refresh_y_after

use beam_common::Result;

use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};

use super::super::runtime::{
    encode_target, jit_allocate, jit_allocate_heap, jit_test_heap, jit_trim, AllocateHeapHelper,
    FrameSizeHelper, HeapNeedHelper,
};
use super::{emit_continue_check, emit_helper_call, emit_refresh_y, Allocation};

/// Lower `allocate StackNeed Live` — push a stack frame, then continue in-block at
/// `pc + 1`.
///
/// The helper ([`jit_allocate`]) makes the interpreter's own `Machine::push_frame` call
/// (no heap admission: `allocate` deliberately skips C's `$AH` check) and answers either
/// `encode_target(func, pc + 1)` (the interpreter's `Step::Next`) or `ERROR`. The pop's
/// analogue [`super::emit_deallocate`]: no register refill (the push touches no
/// x-register), but the frame changed, so the Y window is re-published when a later
/// instruction of this block still accesses a Y slot.
///
/// [`Machine::push_frame`]: crate::Machine
pub(super) fn emit_allocate(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    stack_need: u32,
) -> Result<()> {
    let helper: FrameSizeHelper = jit_allocate;
    emit_helper_call(
        buffer,
        allocation,
        helper as *const (),
        &[i64::from(func), i64::from(pc), i64::from(stack_need)],
    )?;
    emit_frame_change_continue(buffer, allocation, done, func, pc)
}

/// Lower `allocate_heap StackNeed HeapNeed Live` — admit the heap need, then push the
/// frame, then continue in-block at `pc + 1`.
///
/// One helper round-trip ([`jit_allocate_heap`]) performs the interpreter's two calls in
/// the interpreter's order (`ensure_heap` **then** `push_frame`). Because the admission
/// may collect, the continue arm goes through [`super::emit_continue_check`]: the cached
/// x-registers are reloaded before the block continues (a collecting host rewrote
/// `machine.x`), and the Y window is re-published (the frame changed).
pub(super) fn emit_allocate_heap(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    stack_need: u32,
    heap_need: u32,
    live: u32,
) -> Result<()> {
    let helper: AllocateHeapHelper = jit_allocate_heap;
    emit_helper_call(
        buffer,
        allocation,
        helper as *const (),
        &[
            i64::from(func),
            i64::from(pc),
            i64::from(stack_need),
            i64::from(heap_need),
            i64::from(live),
        ],
    )?;
    emit_continue_check(buffer, done, allocation, func, pc)
}

/// Lower `test_heap HeapNeed Live` — admit the heap need, then continue in-block at
/// `pc + 1`.
///
/// The helper ([`jit_test_heap`]) reaches the host's heap-admission door exactly once
/// with the instruction's own operands; as with [`emit_allocate_heap`] the continue arm
/// goes through [`super::emit_continue_check`] so a collecting host's in-place rewrite of
/// `machine.x` is picked back up.
pub(super) fn emit_test_heap(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    heap_need: u32,
    live: u32,
) -> Result<()> {
    let helper: HeapNeedHelper = jit_test_heap;
    emit_helper_call(
        buffer,
        allocation,
        helper as *const (),
        &[
            i64::from(func),
            i64::from(pc),
            i64::from(heap_need),
            i64::from(live),
        ],
    )?;
    emit_continue_check(buffer, done, allocation, func, pc)
}

/// Lower `trim N Remaining` — shrink the current frame, then continue in-block at
/// `pc + 1`.
///
/// The helper ([`jit_trim`]) makes the interpreter's own `Machine::trim` call, so an
/// over-trim raises the same typed error the interpreter's arm does (via `ERROR`). Like
/// [`emit_allocate`] it touches no x-register, so there is no register refill; the frame
/// changed, so the Y window is re-published when a later instruction of this block still
/// accesses a Y slot.
pub(super) fn emit_trim(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    n: u32,
) -> Result<()> {
    let helper: FrameSizeHelper = jit_trim;
    emit_helper_call(
        buffer,
        allocation,
        helper as *const (),
        &[i64::from(func), i64::from(pc), i64::from(n)],
    )?;
    emit_frame_change_continue(buffer, allocation, done, func, pc)
}

/// The continue arm of a **frame-changing** helper that touches no x-register: continue
/// in-block only on "the next instruction", leaving the block with the helper's word in
/// `rax` otherwise.
///
/// Same compare-and-branch as [`super::emit_deallocate`] — deliberately **without** the
/// register refill [`super::emit_continue_check`] emits, because the caller's helper wrote
/// no x-register — and, because the helper changed the frame (`machine.stack` may have
/// been grown or shifted under the window the block published), it re-publishes the Y
/// window when a later instruction of the block still accesses a Y slot.
fn emit_frame_change_continue(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
) -> Result<()> {
    // `rax` holds the helper's word; only "the next instruction" continues here.
    buffer.mov_ri(Reg::R11, encode_target(func, pc + 1))?;
    buffer.cmp_rr(Reg::Rax, Reg::R11)?;
    buffer.jcc(Cond::Ne, done)?;
    if allocation.refresh_y_after(pc) {
        emit_refresh_y(buffer)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jit::emitter::CodeBuffer;

    /// The emitted bytes of `buffer` (a raw view of the still-unsealed mapping).
    fn emitted(buffer: &CodeBuffer) -> Vec<u8> {
        // SAFETY: `code_ptr()` is the start of the buffer's live mapping and `len()`
        // bytes of it have been emitted, so `[ptr, ptr + len)` is a valid, initialized
        // range; the slice borrows nothing that is mutated while it lives.
        unsafe { std::slice::from_raw_parts(buffer.code_ptr(), buffer.len()).to_vec() }
    }

    /// The bytes `CodeBuffer` itself produces for `f` on a throwaway buffer, so an
    /// expected encoding is taken from the emitter rather than hard-coded.
    fn encoded(f: impl FnOnce(&mut CodeBuffer)) -> Vec<u8> {
        let mut buffer = CodeBuffer::new().unwrap();
        f(&mut buffer);
        emitted(&buffer)
    }

    /// The bytes of one lowering, up to (and including) its exit.
    fn lowered<F: FnOnce(&mut CodeBuffer, CodeLabel) -> Result<()>>(emit: F) -> Vec<u8> {
        let mut buffer = CodeBuffer::new().unwrap();
        let done = buffer.label();
        emit(&mut buffer, done).unwrap();
        let len = buffer.len();
        buffer.ret().unwrap();
        buffer.bind(done).unwrap();
        buffer.seal().unwrap();
        emitted(&buffer)[..len].to_vec()
    }

    /// The `movabs r11, <helper>` immediate every indirect helper call emits.
    fn helper_immediate(helper: *const ()) -> Vec<u8> {
        encoded(|buffer| buffer.mov_ri(Reg::R11, helper as i64).unwrap())
    }

    /// `spill` of `allocation` (its **written** cache), as emitted.
    fn spill_bytes(allocation: &Allocation) -> Vec<u8> {
        encoded(|buffer| {
            super::super::spill(buffer, allocation, super::super::SpillSite::Exit).unwrap()
        })
    }

    /// The `jit_refresh_y` immediate.
    fn refresh_immediate() -> Vec<u8> {
        helper_immediate(super::super::jit_refresh_y as *const ())
    }

    /// Whether `code` contains `needle`.
    fn contains(code: &[u8], needle: &[u8]) -> bool {
        code.windows(needle.len()).any(|window| window == needle)
    }

    /// A cache holding x0 in `r12`, marked written so the spill and the refill both
    /// emit real instructions.
    fn cached_allocation() -> Allocation {
        let mut allocation = Allocation::default();
        allocation.slots.push((0, Reg::R12));
        allocation.written.push(0);
        allocation
    }

    #[test]
    fn each_heap_family_helper_is_called_with_the_spill_first() {
        let allocation = cached_allocation();
        let cases: Vec<(&str, *const (), Vec<u8>)> = vec![
            (
                "allocate",
                jit_allocate as *const (),
                lowered(|buffer, done| {
                    emit_allocate(buffer, &allocation, done, 4, 7, 2)
                }),
            ),
            (
                "allocate_heap",
                jit_allocate_heap as *const (),
                lowered(|buffer, done| {
                    emit_allocate_heap(buffer, &allocation, done, 4, 7, 2, 9, 3)
                }),
            ),
            (
                "test_heap",
                jit_test_heap as *const (),
                lowered(|buffer, done| emit_test_heap(buffer, &allocation, done, 4, 7, 9, 3)),
            ),
            (
                "trim",
                jit_trim as *const (),
                lowered(|buffer, done| emit_trim(buffer, &allocation, done, 4, 7, 1)),
            ),
        ];
        let spill = spill_bytes(&allocation);
        assert!(!spill.is_empty(), "the probe allocation must spill a register");
        for (label, helper, code) in cases {
            assert!(
                code.starts_with(&spill),
                "{label}: the root-set spill must come first"
            );
            let needle = helper_immediate(helper);
            let call = code
                .windows(needle.len())
                .position(|window| window == needle)
                .unwrap_or_else(|| panic!("{label}: the helper must be called"));
            assert!(call >= spill.len(), "{label}: the call follows the spill");
            // …and the emitted code continues only on "the next instruction".
            let next = encoded(|buffer| buffer.mov_ri(Reg::R11, encode_target(4, 8)).unwrap());
            assert!(
                contains(&code, &next),
                "{label}: the continue arm compares against `encode_target(func, pc + 1)`"
            );
        }
    }

    #[test]
    fn the_heap_admitting_helpers_refill_the_cache_but_the_frame_only_ones_do_not() {
        let allocation = cached_allocation();
        // What `emit_continue_check`'s continue arm reloads at pc 7.
        let refill = encoded(|buffer| {
            super::super::fill(buffer, &allocation, super::super::FillSite::Helper, 7).unwrap()
        });
        assert!(!refill.is_empty(), "the probe allocation must refill a register");

        // A collecting admission rewrites `machine.x` in place, so the block reloads.
        for (label, code) in [
            (
                "allocate_heap",
                lowered(|buffer, done| {
                    emit_allocate_heap(buffer, &allocation, done, 4, 7, 2, 9, 3)
                }),
            ),
            (
                "test_heap",
                lowered(|buffer, done| emit_test_heap(buffer, &allocation, done, 4, 7, 9, 3)),
            ),
        ] {
            assert!(
                contains(&code, &refill),
                "{label}: the continue arm must go through `emit_continue_check`'s refill"
            );
        }

        // The frame-only helpers touch no x-register and emit no refill.
        for (label, code) in [
            (
                "allocate",
                lowered(|buffer, done| emit_allocate(buffer, &allocation, done, 4, 7, 2)),
            ),
            (
                "trim",
                lowered(|buffer, done| emit_trim(buffer, &allocation, done, 4, 7, 1)),
            ),
        ] {
            assert!(
                !contains(&code, &refill),
                "{label}: it touches no x-register and must emit no refill"
            );
        }
    }

    #[test]
    fn a_frame_changing_helper_refreshes_the_window_only_when_a_later_y_access_follows() {
        let mut accessing = Allocation::default();
        accessing.y_last = Some(9);
        let refreshed = lowered(|buffer, done| emit_allocate(buffer, &accessing, done, 0, 7, 2));
        let plain =
            lowered(|buffer, done| emit_allocate(buffer, &Allocation::default(), done, 0, 7, 2));
        assert!(
            contains(&refreshed, &refresh_immediate()),
            "a later Y access in the same block must re-publish the window"
        );
        assert!(
            !contains(&plain, &refresh_immediate()),
            "a Y-free block pays nothing"
        );
        // The refresh is emitted *past* the continue check (the frame change it follows
        // is what invalidated the window the next instruction would read).
        let next = encoded(|buffer| buffer.mov_ri(Reg::R11, encode_target(0, 8)).unwrap());
        let compare = refreshed
            .windows(next.len())
            .position(|window| window == next)
            .expect("the continue arm compares against the next instruction");
        let refresh = refreshed
            .windows(refresh_immediate().len())
            .position(|window| window == refresh_immediate())
            .expect("the window is re-published");
        assert!(
            refresh > compare,
            "the refresh follows the continue check: {refresh} vs {compare}"
        );
        // …and the same gating holds for the two heap-admitting lowerings.
        let heap_refreshed = lowered(|buffer, done| {
            emit_test_heap(buffer, &accessing, done, 0, 7, 9, 3)
        });
        let heap_plain = lowered(|buffer, done| {
            emit_test_heap(buffer, &Allocation::default(), done, 0, 7, 9, 3)
        });
        assert!(contains(&heap_refreshed, &refresh_immediate()));
        assert!(!contains(&heap_plain, &refresh_immediate()));
    }
}
