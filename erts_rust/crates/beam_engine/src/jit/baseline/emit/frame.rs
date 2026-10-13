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

//! The baseline JIT's **frame-discipline** lowerings: `deallocate`, `return` and the
//! fused `deallocate_return`.
//!
//! These are the three instructions that end a function body. What a compiled block
//! cannot do itself is address the **call-frame stack** and the **Erlang stack**: both
//! are Rust-side `Machine` state (`Vec`s of `Frame` and `Eterm`) with no layout a block
//! could compute an offset into, and popping a frame is a checked operation that must
//! raise the interpreter's own error on a `deallocate` operand that no longer matches
//! the live frame. So the pop and the continuation read go through the tier's helpers
//! ([`jit_deallocate`]/[`jit_return`]/[`jit_deallocate_return`], in `super::runtime`),
//! which are the interpreter's own calls in the interpreter's own order — but the
//! *instruction itself* no longer needs an interpreter step: the helper performs
//! exactly this instruction's `Machine` work and encodes the outcome as the block-ABI
//! word, and the driver decodes that word (`(func, pc)` target, [`HALT`], [`ENTER`],
//! [`ERROR`]) exactly as it decodes a deopt's.
//!
//! # The two shapes
//!
//! * **`deallocate N`** ([`emit_deallocate`]) is the *non-terminating* member: the
//!   interpreter's arm is `Machine::pop_frame_checked(N)` then `Step::Next`, so the
//!   emitted code compares the helper's word with `encode_target(func, pc + 1)` and
//!   **continues in-block** on a match (branching to the block epilogue with the word
//!   otherwise, which for this helper is always the [`ERROR`] word). It emits **no**
//!   register refill: the pop truncates `machine.stack` and drops a call frame, and
//!   touches no x-register, so the block's cached registers still hold exactly what
//!   `machine.x` does. It does re-publish the Y window on the continue arm when a later
//!   instruction of the block still accesses a Y slot ([`Allocation::refresh_y_after`]),
//!   because the pop *is* a frame change.
//! * **`return`** ([`emit_return`]) and the fused **`deallocate_return`**
//!   ([`emit_deallocate_return`]) **always transfer control** — the interpreter's
//!   `return_step` yields `Step::Done`/`Goto`/`Enter`, never `Next` — so the block
//!   leaves with the helper's word in `rax` and the driver takes it. The fused form is
//!   one helper round-trip for the pair `deallocate N; return`: the loader fuses that
//!   adjacent pair into `Instr::DeallocateReturn` (`super::super::super` docs,
//!   `beam_loader::lower::fuse`), and [`super::emit_block`] fuses the separate pair a
//!   hand-built module (the crate's own test modules, the bench fixtures) still holds.
//!
//! # Guards
//!
//! There are none to emit. Every operand shape is lowered: a `Deallocate` frame size is
//! a `u32` the helper takes as an argument, and a `return` has no operand at all. The
//! only way these lowerings fail is a buffer that cannot hold the emitted bytes, which
//! is the shared typed `SystemLimit` compile error — never a wrong result.
//!
//! [`HALT`]: super::super::runtime::HALT
//! [`ENTER`]: super::super::runtime::ENTER
//! [`ERROR`]: super::super::runtime::ERROR
//! [`Allocation::refresh_y_after`]: super::Allocation::refresh_y_after

use beam_common::Result;

use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};

use super::super::runtime::{
    encode_target, jit_deallocate, jit_deallocate_return, jit_return, FrameExitHelper,
    ReturnHelper,
};
use super::{emit_helper_call, emit_refresh_y, Allocation};

/// Lower `deallocate N` — the frame pop, then continue in-block at `pc + 1`.
///
/// The helper ([`jit_deallocate`]) pops with [`Machine::pop_frame_checked`] and answers
/// either `encode_target(func, pc + 1)` (the interpreter's `Step::Next`) or `ERROR`
/// (with the typed diagnostic in `rt.error`). The emitted code continues on the first
/// and branches to the block epilogue with `rax` untouched on anything else, so the
/// driver propagates the pop's own error verbatim.
///
/// [`Machine::pop_frame_checked`]: crate::Machine
pub(super) fn emit_deallocate(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    deallocate: u32,
) -> Result<()> {
    let helper: FrameExitHelper = jit_deallocate;
    emit_helper_call(
        buffer,
        allocation,
        helper as *const (),
        &[i64::from(func), i64::from(pc), i64::from(deallocate)],
    )?;
    // `rax` holds the helper's word; only "the next instruction" continues here.
    buffer.mov_ri(Reg::R11, encode_target(func, pc + 1))?;
    buffer.cmp_rr(Reg::Rax, Reg::R11)?;
    buffer.jcc(Cond::Ne, done)?;
    // Deliberately **no** register refill (unlike `emit_continue_check`): the pop
    // touches no x-register, so the cached registers are still current. The Y window
    // is not: a frame was dropped, and `machine.stack` may have been truncated under
    // the window the block published, so re-publish it before the next instruction
    // when a later instruction of this block still accesses a Y slot.
    if allocation.refresh_y_after(pc) {
        emit_refresh_y(buffer)?;
    }
    Ok(())
}

/// Lower `return` — follow the current frame's continuation.
///
/// The helper ([`jit_return`]) reads the frame's continuation and encodes it as the
/// block-ABI word; `return` always transfers control, so the block leaves with that
/// word in `rax` for the driver (a `(func, pc)` target, [`HALT`] on the bottom frame,
/// or [`ENTER`] for a caller in another module context).
///
/// [`HALT`]: super::super::runtime::HALT
/// [`ENTER`]: super::super::runtime::ENTER
pub(super) fn emit_return(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
) -> Result<()> {
    let helper: ReturnHelper = jit_return;
    emit_helper_call(
        buffer,
        allocation,
        helper as *const (),
        &[i64::from(func), i64::from(pc)],
    )?;
    emit_word_exit(buffer, done)
}

/// Lower `deallocate N; return` — the loader's fused superinstruction, or the separate
/// pair [`super::emit_block`] fuses.
///
/// One helper round-trip ([`jit_deallocate_return`]) performs the interpreter's two
/// constituents in order (the checked pop, then the continuation read), and the block
/// then leaves with the helper's word in `rax` — [`HALT`], a `(func, pc)` target,
/// [`ENTER`] or [`ERROR`]. `func`/`pc` name the **return**: the fused instruction's own
/// address, or the `return`'s address when the emitter fused a separate pair (exactly
/// the position the interpreter would have recorded last).
///
/// [`HALT`]: super::super::runtime::HALT
/// [`ENTER`]: super::super::runtime::ENTER
/// [`ERROR`]: super::super::runtime::ERROR
pub(super) fn emit_deallocate_return(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    deallocate: u32,
) -> Result<()> {
    let helper: FrameExitHelper = jit_deallocate_return;
    emit_helper_call(
        buffer,
        allocation,
        helper as *const (),
        &[i64::from(func), i64::from(pc), i64::from(deallocate)],
    )?;
    emit_word_exit(buffer, done)
}

/// Leave the block with the helper's word already in `rax`.
///
/// The word *is* the exit value — the driver decodes it as a `(func, pc)` target or as
/// [`HALT`]/[`ENTER`]/[`ERROR`] — so nothing is materialised and no comparison is made:
/// the block jumps to its shared `done` epilogue, which tears the **native** frame down
/// (`emit_helper_call` has already spilled the cached x-registers into `machine.x`, the
/// tier's root-set contract) and returns. Fabricating or re-encoding a word here could
/// only disagree with the helper.
///
/// [`HALT`]: super::super::runtime::HALT
/// [`ENTER`]: super::super::runtime::ENTER
/// [`ERROR`]: super::super::runtime::ERROR
fn emit_word_exit(buffer: &mut CodeBuffer, done: CodeLabel) -> Result<()> {
    buffer.jmp(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jit::emitter::CodeBuffer;
    use crate::jit::emitter::Reg;

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
        // A filler so the exit's displacement is non-trivial, then the epilogue label.
        buffer.ret().unwrap();
        buffer.bind(done).unwrap();
        buffer.seal().unwrap();
        emitted(&buffer)[..len].to_vec()
    }

    /// `spill` of `allocation`, as emitted.
    fn spill_bytes(allocation: &Allocation) -> Vec<u8> {
        encoded(|buffer| super::super::spill(buffer, allocation).unwrap())
    }

    /// The `movabs r11, <helper>` immediate every indirect helper call emits.
    fn helper_immediate(helper: *const ()) -> Vec<u8> {
        encoded(|buffer| buffer.mov_ri(Reg::R11, helper as i64).unwrap())
    }

    /// Whether `code` materialises a full word in `rax` (`movabs rax, imm64`). A word
    /// exit must not: the helper's word already *is* the exit value, and re-encoding it
    /// could only disagree.
    fn fabricates_a_word_in_rax(code: &[u8]) -> bool {
        code.windows(2).any(|w| w == [0x48, 0xB8])
    }

    /// Whether `code` ends with the emitted `jmp rel32` (`E9 <disp32>`) — the word
    /// exit's "leave with the helper's word in `rax`" branch. The displacement's four
    /// bytes follow the opcode, so the opcode is the fifth byte from the end.
    fn ends_with_a_jmp(code: &[u8]) -> bool {
        code.len() >= 5 && code[code.len() - 5] == 0xE9
    }

    #[test]
    fn the_fused_exit_hands_the_helpers_word_to_the_driver() {
        let allocation = Allocation::default();
        let exit = lowered(|buffer, done| {
            emit_deallocate_return(buffer, &allocation, done, 0, 7, 3)
        });

        // The root-set contract: the spill comes first, before the helper reads
        // `machine` (and before the driver reads `machine.x`).
        let spill = spill_bytes(&allocation);
        assert_eq!(&exit[..spill.len()], &spill[..], "the spill comes first");

        // The helper is called with `rt` from the block's own stack slot plus the
        // instruction's `(func, pc)` and the frame size — and it is the *fused* helper.
        assert!(
            exit.windows(helper_immediate(jit_deallocate_return as *const ()).len())
                .any(|window| window == helper_immediate(jit_deallocate_return as *const ()).as_slice()),
            "the fused exit calls `jit_deallocate_return`"
        );
        let word = encoded(|buffer| buffer.mov_ri(Reg::Rsi, 0).unwrap());
        assert!(
            exit.windows(word.len()).any(|window| window == word.as_slice()),
            "the instruction's function index is passed to the helper"
        );
        let deallocate = encoded(|buffer| buffer.mov_ri(Reg::Rcx, 3).unwrap());
        assert!(
            exit.windows(deallocate.len())
                .any(|window| window == deallocate.as_slice()),
            "the frame size the pop must match is passed to the helper"
        );

        // The exit leaves with the helper's word in `rax`: an unconditional jump to the
        // shared epilogue, and nothing materialised in `rax`.
        assert!(ends_with_a_jmp(&exit), "the exit ends in `jmp rel32`");
        assert!(
            !fabricates_a_word_in_rax(&exit),
            "the exit must not re-encode the helper's word"
        );
    }

    #[test]
    fn a_bare_return_ends_the_block_like_the_fused_exit() {
        let allocation = Allocation::default();
        let bare = lowered(|buffer, done| emit_return(buffer, &allocation, done, 0, 7));
        let fused = lowered(|buffer, done| {
            emit_deallocate_return(buffer, &allocation, done, 0, 7, 0)
        });
        assert!(
            bare.windows(helper_immediate(jit_return as *const ()).len())
                .any(|window| window == helper_immediate(jit_return as *const ()).as_slice()),
            "a bare `return` calls `jit_return`"
        );
        assert!(!fabricates_a_word_in_rax(&bare));
        // Both are "leave with the helper's word": the same single trailing jump, and
        // the bare form's is the shorter sequence (no frame-size argument).
        assert!(ends_with_a_jmp(&bare));
        assert!(ends_with_a_jmp(&fused));
        assert!(
            bare.len() < fused.len(),
            "a bare `return` passes no frame size: {} vs {}",
            bare.len(),
            fused.len()
        );
    }

    #[test]
    fn a_lone_deallocate_continues_only_on_the_next_instruction_word() {
        let allocation = Allocation::default();
        let exit = lowered(|buffer, done| emit_deallocate(buffer, &allocation, done, 4, 7, 2));

        assert!(
            exit.windows(helper_immediate(jit_deallocate as *const ()).len())
                .any(|window| window == helper_immediate(jit_deallocate as *const ()).as_slice()),
            "a lone `deallocate` calls `jit_deallocate`"
        );
        // The continuation it compares `rax` against is the *next instruction* — the
        // interpreter's `Step::Next` for `deallocate` (`pc + 1`, not `pc`).
        let next = encoded(|buffer| buffer.mov_ri(Reg::R11, encode_target(4, 8)).unwrap());
        assert!(
            exit.windows(next.len()).any(|window| window == next.as_slice()),
            "the continue arm compares against `encode_target(func, pc + 1)`"
        );
        // …and the block can continue: the lowering ends in a *conditional* branch to
        // the epilogue, never an unconditional jump.
        let jcc = encoded(|buffer| {
            let target = buffer.label();
            buffer.jcc(Cond::Ne, target).unwrap();
            buffer.bind(target).unwrap();
        });
        let tail = &exit[exit.len() - jcc.len()..];
        assert_eq!(
            &tail[..2],
            &jcc[..2],
            "the last branch of a lone `deallocate` is the `jne` to the epilogue"
        );
        assert!(
            !fabricates_a_word_in_rax(&exit),
            "the continue arm comes from the helper, not from a materialised word"
        );
    }

    #[test]
    fn a_y_accessing_deallocate_refreshes_the_window_but_a_y_free_one_does_not() {
        // A block that accesses a Y slot *after* the pop must re-publish the window
        // (the pop truncated `machine.stack` under it); a block that does not, must not.
        let mut accessing = Allocation::default();
        accessing.y_last = Some(9);
        let refreshed = lowered(|buffer, done| emit_deallocate(buffer, &accessing, done, 0, 7, 2));
        let plain = lowered(|buffer, done| emit_deallocate(buffer, &Allocation::default(), done, 0, 7, 2));
        let refresh = helper_immediate(super::super::jit_refresh_y as *const ());
        assert!(
            refreshed
                .windows(refresh.len())
                .any(|window| window == refresh.as_slice()),
            "a later Y access in the same block must re-publish the window"
        );
        assert!(
            !plain.windows(refresh.len()).any(|window| window == refresh.as_slice()),
            "a Y-free block pays nothing"
        );
        // The refresh is the *only* difference: both lowerings call the same helper,
        // and the refresh sits **after** the continue check (the pop it follows is what
        // invalidated the window the next instruction would read).
        let helper = helper_immediate(jit_deallocate as *const ());
        let call = refreshed
            .windows(helper.len())
            .position(|window| window == helper.as_slice())
            .expect("both lowerings call `jit_deallocate`");
        assert!(
            plain.len() > call && plain.len() < refreshed.len(),
            "the refresh is emitted past the helper call: {} vs {} bytes",
            plain.len(),
            refreshed.len()
        );
    }
}
