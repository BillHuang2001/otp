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
//!   `return_step` yields `Step::Done`/`Goto`/`Enter`, never `Next` — but the transfer is
//!   not always the driver's: a continuation naming a block entry of **this artifact** is
//!   tail-jumped to **in-block** by [`emit_resume_or_exit`] — the common same-function shape
//!   (every non-tail call returns to its own `pc + 1`, a continuation sub-block of the same
//!   function) *and* the cross-function one (a call into another function of the module
//!   returns into the caller's function, in another executable mapping of the same
//!   artifact) — which is why a call-heavy function no longer pays a driver iteration per
//!   return. Every other outcome — a bottom frame's [`HALT`], another module context's
//!   [`ENTER`], a function this artifact holds no compiled code for, an address no block
//!   covers, a register-file bound the machine does not satisfy, a refused pre-emption
//!   charge ([`ERROR`]) — leaves the helper's word in `rax` for the driver, exactly as
//!   before. The fused form is one helper round-trip for the pair
//!   `deallocate N; return`: the loader fuses that adjacent pair into
//!   `Instr::DeallocateReturn` (`super::super::super` docs, `beam_loader::lower::fuse`),
//!   and [`super::emit_block`] fuses the separate pair a hand-built module (the crate's own
//!   test modules, the bench fixtures) still holds.
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
    ReturnHelper, RUNTIME_RESUME_DISP,
};
use super::link::emit_frame_teardown;
use super::{emit_helper_call, Allocation, RUNTIME_SLOT_DISP};

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
    pc)?;
    // `rax` holds the helper's word; only "the next instruction" continues here.
    buffer.mov_ri(Reg::R11, encode_target(func, pc + 1))?;
    buffer.cmp_rr(Reg::Rax, Reg::R11)?;
    buffer.jcc(Cond::Ne, done)?;
    // Deliberately **no** register refill (unlike `emit_continue_check`): the pop
    // touches no x-register, so the cached registers are still current. The Y window is
    // re-published *inside* `jit_deallocate` (a frame was dropped and `machine.stack` was
    // truncated under the window the block published), so this arm emits no refresh call.
    Ok(())
}

/// Lower `return` — follow the current frame's continuation.
///
/// The helper ([`jit_return`]) reads the frame's continuation and encodes it as the
/// block-ABI word; `return` always transfers control, so the block either tail-jumps to
/// the block entry the helper resolved inside **this module's artifact**
/// ([`emit_resume_or_exit`]) or leaves with that word in `rax` for the driver (an address
/// the artifact compiled no block for, a register-file bound the machine does not satisfy,
/// [`HALT`] on the bottom frame, or [`ENTER`] for a caller in another module context).
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
    pc)?;
    emit_resume_or_exit(buffer, allocation, done)
}

/// Lower `deallocate N; return` — the loader's fused superinstruction, or the separate
/// pair [`super::emit_block`] fuses.
///
/// One helper round-trip ([`jit_deallocate_return`]) performs the interpreter's two
/// constituents in order (the checked pop, then the continuation read), and the block then
/// either tail-jumps to the same-function block entry the helper resolved or leaves with the
/// helper's word in `rax` — [`HALT`], a `(func, pc)` target in another function/context,
/// [`ENTER`] or [`ERROR`] (see [`emit_resume_or_exit`]). `func`/`position_pc` name the
/// **return**: the fused instruction's own address, or the `return`'s address when the
/// emitter fused a separate pair (exactly the position the interpreter would have recorded
/// last).
///
/// # The two pcs — `position_pc` is not the store pc
///
/// The pair is lowered as **one** helper call, so there is **one** pre-call [`spill`] for
/// two instruction positions, and the store set must be the dirty set of the moment that
/// store happens: the dirty set **entering the first instruction of the pair**. Hence
/// `store_pc` is a parameter of its own, and is *not* `position_pc` when the emitter fused
/// a separate pair — there the single store covers the `deallocate`, whose `pc` is
/// `position_pc - 1`, and the `return`'s own entry in the dirty table is already empty
/// (the dirty walk clears it at the `deallocate`, whose store this call emits). Passing
/// `position_pc` would therefore store **nothing** for a register the inline prefix of the
/// pair's block made dirty, leaving `machine.x` stale for the helper (and for the driver
/// that reads `x(0)` on the bottom-frame [`HALT`]).
///
/// [`HALT`]: super::super::runtime::HALT
/// [`ENTER`]: super::super::runtime::ENTER
/// [`ERROR`]: super::super::runtime::ERROR
/// [`spill`]: super::spill
pub(super) fn emit_deallocate_return(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    position_pc: u32,
    store_pc: u32,
    deallocate: u32,
) -> Result<()> {
    let helper: FrameExitHelper = jit_deallocate_return;
    emit_helper_call(
        buffer,
        allocation,
        helper as *const (),
        &[
            i64::from(func),
            i64::from(position_pc),
            i64::from(deallocate),
        ],
        store_pc,
    )?;
    emit_resume_or_exit(buffer, allocation, done)
}

/// Leave the block with the helper's word in `rax` — **or** tail-jump straight to the block
/// entry the helper resolved for a continuation that names a block of this artifact (the
/// function that is already running, or another function of the same module).
///
/// The word *is* the exit value — the driver decodes it as a `(func, pc)` target or as
/// [`HALT`]/[`ENTER`]/[`ERROR`] — so nothing is materialised and no comparison is made
/// against it: every outcome the helper did not turn into an in-block transfer jumps to the
/// shared `done` epilogue with `rax` untouched. Fabricating or re-encoding a word here could
/// only disagree with the helper.
///
/// # The in-block continuation
///
/// A **non-tail call's** continuation is its own `pc + 1`, which the compiler lowered as a
/// continuation sub-block — of the *caller's* function, which for a call into another
/// function of the module is another executable mapping of the same artifact. The helper
/// resolves the entry itself ([`super::super::runtime::EntryTables`], else the artifact) and
/// stores it in [`JitRuntime::resume_entry`](super::super::runtime::JitRuntime) — having
/// already done the driver iteration's work (publish the position, bump the slice counter,
/// offer the pre-emption hook) in the driver's own order. When it did, `rax` is not consulted
/// at all and the block transfers **in-block**:
///
/// ```text
///   call <helper>                     ; the frame-discipline instruction; spills first
///   mov  rdi, [rsp + RUNTIME_SLOT_DISP] ; rt — reloaded, the helper may clobber rdi
///   mov  r11, [rdi + RUNTIME_RESUME_DISP] ; the resolved block entry (same artifact)
///   test r11, r11
///   je   <done>                       ; no entry: today's word exit, `rax` intact
///   lea  rsp, [rsp + slot] ; pop …    ; leave this block's native frame
///   jmp  r11                          ; the target prologue rebuilds its own frame
/// ```
///
/// This is a **tail** transfer: the block's frame is torn down before the jump, so the
/// native stack neither grows (deep Erlang recursion stays safe) nor is left holding a stale
/// frame, and it is exactly the shape a linked `call_only`/`jump` exit uses
/// ([`super::link::Linker::emit_exit`]) — reload `rt` into `rdi` (the one thing a target
/// prologue needs, and a register the teardown does not pop), tear down, jump. `rdi` and
/// `r11` are both caller-saved and the teardown only pops the block's saved callee-saved set,
/// so both survive it.
///
/// # Why the skipped checks cannot be skipped unsoundly
///
/// The helper's own docs carry the argument ([`super::super::runtime::return_word`]): the
/// entry is used only when it belongs to **the module's compiled artifact the block is
/// running** (a different module context is [`ENTER`] and keeps the driver path), the
/// artifact's code stays mapped for the whole slice, and a transfer into a function other
/// than the one the tables describe has had **that function's** register-file bound checked
/// inside the helper before the entry is handed over. The same-function shape stays within
/// the bound the driver already checked on entry.
///
/// [`HALT`]: super::super::runtime::HALT
/// [`ENTER`]: super::super::runtime::ENTER
/// [`ERROR`]: super::super::runtime::ERROR
fn emit_resume_or_exit(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    done: CodeLabel,
) -> Result<()> {
    // `rt`, reloaded from the block's own slot: the helper is an ordinary `extern "C"` call
    // and `rdi` is caller-saved, so it may hold anything now. It is also what the resumed
    // block's prologue stores as *its* `rt` slot.
    buffer.mov_rm(Reg::Rdi, Reg::Rsp, RUNTIME_SLOT_DISP)?;
    // The entry the helper resolved, or null when the continuation must go back to the
    // driver (a bottom frame's `HALT`, another context's `ENTER`, another function, an
    // address no block covers, a refused pre-emption charge).
    buffer.mov_rm(Reg::R11, Reg::Rdi, RUNTIME_RESUME_DISP)?;
    buffer.test_rr(Reg::R11, Reg::R11)?;
    buffer.jcc(Cond::E, done)?;
    emit_frame_teardown(buffer, allocation)?;
    buffer.jmp_r(Reg::R11)
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

    /// `spill` of `allocation` (its **written** cache), as emitted.
    fn spill_bytes(allocation: &Allocation) -> Vec<u8> {
        encoded(|buffer| {
            super::super::spill(buffer, allocation, super::super::SpillSite::Exit, 0).unwrap()
        })
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

    /// Whether `code` ends with the in-block resume transfer `jmp r11` (`41 FF E3`) — the
    /// tail transfer [`emit_resume_or_exit`] takes for a continuation of the function that
    /// is already running.
    fn ends_with_a_resume_jump(code: &[u8]) -> bool {
        code.ends_with(&encoded(|buffer| buffer.jmp_r(Reg::R11).unwrap()))
    }

    /// The `je <done>` guard of [`emit_resume_or_exit`], as `(site offset, target offset)`.
    ///
    /// `done` is bound by [`lowered`] one byte (a `ret` filler) past the end of the
    /// lowering, so the branch must land on `code.len() + 1`.
    fn resume_guard(code: &[u8]) -> (usize, usize) {
        // The last `je rel32` (`0F 84 <disp32>`) of the lowering.
        let site = (0..code.len().saturating_sub(5))
            .rev()
            .find(|&i| code[i] == 0x0F && code[i + 1] == 0x84)
            .expect("the exit guards the in-block transfer");
        let rel = i32::from_le_bytes(code[site + 2..site + 6].try_into().unwrap());
        (site, (site as i64 + 6 + i64::from(rel)) as usize)
    }

    #[test]
    fn the_fused_exit_hands_the_helpers_word_to_the_driver() {
        let allocation = Allocation::default();
        let exit = lowered(|buffer, done| {
            emit_deallocate_return(buffer, &allocation, done, 0, 7, 7, 3)
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

        // The exit transfers **in-block** when the helper resolved a same-function entry
        // (the tail `jmp r11`), guarded by a `je` to the shared epilogue for every other
        // outcome — where the helper's word leaves in `rax` untouched, nothing materialised.
        assert!(ends_with_a_resume_jump(&exit), "the exit ends in the resume jump");
        let (_, target) = resume_guard(&exit);
        assert_eq!(
            target,
            exit.len() + 1,
            "the guard falls through to the clean-exit arm"
        );
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
            emit_deallocate_return(buffer, &allocation, done, 0, 7, 7, 0)
        });
        assert!(
            bare.windows(helper_immediate(jit_return as *const ()).len())
                .any(|window| window == helper_immediate(jit_return as *const ()).as_slice()),
            "a bare `return` calls `jit_return`"
        );
        assert!(!fabricates_a_word_in_rax(&bare));
        // Both end in the same in-block resume transfer (guarded by a `je` to the shared
        // epilogue, where the helper's word leaves in `rax`), and the bare form's sequence
        // is the shorter one (no frame-size argument).
        assert!(ends_with_a_resume_jump(&bare));
        assert!(ends_with_a_resume_jump(&fused));
        let (_, bare_target) = resume_guard(&bare);
        let (_, fused_target) = resume_guard(&fused);
        assert_eq!(bare_target, bare.len() + 1);
        assert_eq!(fused_target, fused.len() + 1);
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
    fn a_deallocate_emits_no_separate_refresh_call() {
        // A block that accesses a Y slot *after* the pop needs the window re-published (the
        // pop truncated `machine.stack` under it) — but the helper that popped is the party
        // that re-publishes it, on its own success path, so the arm emits no second call.
        let mut accessing = Allocation::default();
        accessing.y_last = Some(9);
        let refreshed = lowered(|buffer, done| emit_deallocate(buffer, &accessing, done, 0, 7, 2));
        let plain = lowered(|buffer, done| emit_deallocate(buffer, &Allocation::default(), done, 0, 7, 2));
        let refresh = helper_immediate(super::super::jit_refresh_y as *const ());
        for (label, code) in [("y_accessing", &refreshed), ("y_free", &plain)] {
            assert!(
                !code
                    .windows(refresh.len())
                    .any(|window| window == refresh.as_slice()),
                "{label}: the pop's window is re-published inside `jit_deallocate`"
            );
        }
        // Both lowerings call the same helper and nothing else differs, so the arm is
        // allocation-independent: there is no gated tail left to tell them apart.
        assert_eq!(
            refreshed.len(),
            plain.len(),
            "a `deallocate` arm no longer depends on the block's Y access: {} vs {} bytes",
            refreshed.len(),
            plain.len()
        );
        let helper = helper_immediate(jit_deallocate as *const ());
        assert!(
            refreshed
                .windows(helper.len())
                .any(|window| window == helper.as_slice()),
            "the arm calls `jit_deallocate`"
        );
    }
}
