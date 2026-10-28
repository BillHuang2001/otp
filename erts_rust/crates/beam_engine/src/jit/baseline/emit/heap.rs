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
use beam_term::NIL;

use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};

use super::super::runtime::{
    encode_target, jit_allocate, jit_allocate_heap, jit_test_heap, jit_trim, AllocateHeapHelper,
    FrameSizeHelper, HeapNeedHelper, FRAME_SHIFT, FRAME_WORDS, FRAME_Y_BASE_OFF, FRAME_Y_COUNT_OFF,
    RUNTIME_FRAME_TEMPLATE_DISP, RUNTIME_FRAMES_STORE_DISP, RUNTIME_STACK_STORE_DISP,
    RUNTIME_YBASE_DISP, RUNTIME_YEND_DISP, STORE_CAP_OFF, STORE_LEN_OFF, STORE_PTR_OFF,
};
use super::prologue::emit_position_store;
use super::{
    emit_continue_check, emit_continue_check_helper_refresh, emit_helper_call, Allocation,
    RUNTIME_SLOT_DISP,
};

/// The largest `StackNeed` whose frame push is emitted inline.
///
/// The push's Y fill is an unrolled run of `mov [base + k*8], NIL`, so a wide frame would
/// cost more emitted bytes than the helper call it replaces (and more than the block cache
/// wants to hold). Above this bound the arm emits the helper call alone — the same two
/// instructions it emitted before the inline push existed.
const INLINE_PUSH_MAX_Y: u32 = 16;

/// Lower `allocate StackNeed Live` — push a stack frame, then continue in-block at
/// `pc + 1`.
///
/// The push itself is emitted **inline** behind a capacity check on the two stores
/// ([`emit_inline_frame_push`]); the helper ([`jit_allocate`], which makes the
/// interpreter's own `Machine::push_frame` call — no heap admission: `allocate`
/// deliberately skips C's `$AH` check) is the cold/taken-on-miss fallback, taken when
/// either store would have to grow. A growing store reallocates, which is exactly what the
/// published header cannot survive, so the capacity check must precede every write.
///
/// The helper answers either `encode_target(func, pc + 1)` (the interpreter's
/// `Step::Next`) or `ERROR`; both continue arms are the same compare-and-branch. The pop's
/// analogue [`super::emit_deallocate`]: no register refill (the push touches no
/// x-register), but the frame changed, so the Y window is re-published by the push itself
/// when a later instruction of this block still accesses a Y slot.
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
    let limit = std::env::var("DBG_INLINE_MAX")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(INLINE_PUSH_MAX_Y);
    match FRAME_SHIFT {
        Some(shift) if stack_need <= limit => {
            emit_inline_frame_push(buffer, allocation, done, func, pc, stack_need, shift)
        }
        _ => emit_allocate_helper(buffer, allocation, done, func, pc, stack_need),
    }
}

/// The cold half of [`emit_allocate`] — one [`jit_allocate`] round-trip, then the
/// frame-change continue arm.
fn emit_allocate_helper(
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
    pc)?;
    emit_frame_change_continue(buffer, done, func, pc)
}

/// Emit the frame push inline: a capacity check on both stores, then the position store
/// and every store `Machine::push_frame_window` makes.
///
/// # Emitted shape (the whole arm, in order)
///
/// ```text
///   mov   rdi, [rsp + rt]                ; rt
///   mov   r10, [rdi + stack_store]       ; &mut machine.stack
///   mov   r11, [rdi + frames_store]      ; &mut machine.frames
///   mov   rax, [r10 + len]               ; old stack len  (= the new frame's y_base)
///   mov   rcx, [r10 + cap]
///   lea   r8,  [rax + stack_need]        ; new stack len
///   cmp   r8,  rcx
///   ja    cold                           ; would grow -> helper
///   mov   rdx, [r11 + len]               ; old frame count
///   mov   rcx, [r11 + cap]
///   lea   r9,  [rdx + 1]                 ; new frame count
///   cmp   r9,  rcx
///   ja    cold                           ; would grow -> helper
///   <position store>                     ; inline, three instructions
///   ... NIL fill of the Y slots, frame-template copy, two field patches, both len bumps
///   jmp   cont
/// cold:
///   call  jit_allocate(rt, func, pc, stack_need)
///   <frame-change continue>
/// cont:
/// ```
///
/// # Deopt idempotency
///
/// A `fallback` is a deopt: `jit_step_one` re-executes the whole instruction with the same
/// operands. Both guard branches fire **before any write at all**, so a deopt from here
/// leaves the machine untouched and the re-execution (which takes the helper) applies the
/// push exactly once. The position store is the one write before the push, and it is the
/// same word the helper would store.
///
/// # Register budget
///
/// Everything caller-saved is dead at a frame helper (cached x-registers and `rbx` live in
/// callee-saved registers): `rax`/`rcx`/`rdx`/`rsi`/`rdi`/`r8`/`r9`/`r10`/`r11` are used
/// freely here and no value survives into the next instruction.

fn emit_inline_frame_push(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    stack_need: u32,
    frame_shift: u32,
) -> Result<()> {
    let cold = buffer.label();
    let cont = buffer.label();
    // rt (the push reads both store handles and the frame template through it).
    buffer.mov_rm(Reg::Rdi, Reg::Rsp, RUNTIME_SLOT_DISP)?;
    buffer.mov_rm(Reg::R10, Reg::Rdi, RUNTIME_STACK_STORE_DISP)?;
    buffer.mov_rm(Reg::R11, Reg::Rdi, RUNTIME_FRAMES_STORE_DISP)?;
    // Stack capacity: `old_len + stack_need <= cap`. Both figures are **words**
    // (`PubVec<Eterm>`'s `len`/`cap` are element counts), so no scaling here; `r8` is
    // also the length the push commits further down.
    buffer.mov_rm(Reg::Rax, Reg::R10, STORE_LEN_OFF)?;
    buffer.mov_rm(Reg::Rcx, Reg::R10, STORE_CAP_OFF)?;
    buffer.lea(Reg::R8, Reg::Rax, stack_need as i32)?;
    buffer.cmp_rr(Reg::R8, Reg::Rcx)?;
    buffer.jcc(Cond::A, cold)?;
    // Frame capacity: `frames.len() + 1 <= frames.cap()`.
    buffer.mov_rm(Reg::Rdx, Reg::R11, STORE_LEN_OFF)?;
    buffer.mov_rm(Reg::Rcx, Reg::R11, STORE_CAP_OFF)?;
    buffer.lea(Reg::R9, Reg::Rdx, 1)?;
    buffer.cmp_rr(Reg::R9, Reg::Rcx)?;
    buffer.jcc(Cond::A, cold)?;
    // Position: the instruction being executed (the helper stores the same word).
    // This clobbers `r10`/`r11`, so both store handles are re-read below.
    emit_position_store(buffer, func, pc)?;
    buffer.mov_rm(Reg::R10, Reg::Rdi, RUNTIME_STACK_STORE_DISP)?;
    buffer.mov_rm(Reg::R11, Reg::Rdi, RUNTIME_FRAMES_STORE_DISP)?;
    // The new frame's Y window: `stack.ptr + old_len` .. `+ stack_need` words.
    buffer.mov_rm(Reg::Rcx, Reg::R10, STORE_PTR_OFF)?;
    buffer.mov_rr(Reg::Rsi, Reg::Rax)?;
    buffer.shl_ri(Reg::Rsi, 3)?;
    buffer.add_rr(Reg::Rsi, Reg::Rcx)?;
    buffer.mov_mr(Reg::Rdi, RUNTIME_YBASE_DISP, Reg::Rsi)?;
    buffer.lea(Reg::Rcx, Reg::Rsi, (stack_need * 8) as i32)?;
    buffer.mov_mr(Reg::Rdi, RUNTIME_YEND_DISP, Reg::Rcx)?;
    // Y slots, filled exactly as `resize(.., Eterm::default())` fills them.
    buffer.mov_ri(Reg::Rcx, NIL.raw() as i64)?;
    for k in 0..stack_need {
        buffer.mov_mr(Reg::Rsi, (k * 8) as i32, Reg::Rcx)?;
    }
    buffer.mov_mr(Reg::R10, STORE_LEN_OFF, Reg::R8)?;
    // The frame slot: template words, then the two fields the push computes.
    buffer.mov_rm(Reg::Rcx, Reg::R11, STORE_PTR_OFF)?;
    buffer.mov_rr(Reg::Rsi, Reg::Rdx)?;
    buffer.shl_ri(Reg::Rsi, frame_shift)?;
    buffer.add_rr(Reg::Rsi, Reg::Rcx)?;
    for k in 0..FRAME_WORDS {
        buffer.mov_rm(Reg::Rcx, Reg::Rdi, RUNTIME_FRAME_TEMPLATE_DISP + (k * 8) as i32)?;
        buffer.mov_mr(Reg::Rsi, (k * 8) as i32, Reg::Rcx)?;
    }
    buffer.mov_mr(Reg::Rsi, FRAME_Y_BASE_OFF, Reg::Rax)?;
    buffer.mov_ri(Reg::Rcx, i64::from(stack_need))?;
    buffer.mov_mr32(Reg::Rsi, FRAME_Y_COUNT_OFF, Reg::Rcx)?;
    buffer.mov_mr(Reg::R11, STORE_LEN_OFF, Reg::R9)?;
    buffer.jmp(cont)?;
    // Cold: the store must grow (or a Frame is not a whole number of words), so the
    // helper reallocates and re-publishes; its continue arm folds `ERROR` into `done`.
    buffer.bind(cold)?;
    emit_allocate_helper(buffer, allocation, done, func, pc, stack_need)?;
    buffer.bind(cont)?;
    Ok(())
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
    pc)?;
    // The push inside the helper is a frame change, so the helper re-publishes the Y
    // window itself (see `jit_allocate_heap`): the continue arm keeps its register refill
    // but emits no separate refresh call.
    emit_continue_check_helper_refresh(buffer, done, allocation, func, pc)
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
    pc)?;
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
    pc)?;
    emit_frame_change_continue(buffer, done, func, pc)
}

/// The continue arm of a **frame-changing** helper that touches no x-register: continue
/// in-block only on "the next instruction", leaving the block with the helper's word in
/// `rax` otherwise.
///
/// Same compare-and-branch as [`super::emit_deallocate`] — deliberately **without** the
/// register refill [`super::emit_continue_check`] emits, because the caller's helper wrote
/// no x-register. The Y window the frame change moved is re-published **inside** the helper
/// (`jit_allocate`'s `push_frame`, `jit_trim`'s frame shift), on its success path, so the
/// continue arm emits no refresh call: the window is already the live frame's when the next
/// instruction of the block addresses a `Y` slot.
fn emit_frame_change_continue(
    buffer: &mut CodeBuffer,
    done: CodeLabel,
    func: u32,
    pc: u32,
) -> Result<()> {
    // `rax` holds the helper's word; only "the next instruction" continues here.
    buffer.mov_ri(Reg::R11, encode_target(func, pc + 1))?;
    buffer.cmp_rr(Reg::Rax, Reg::R11)?;
    buffer.jcc(Cond::Ne, done)?;
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
            super::super::spill(buffer, allocation, super::super::SpillSite::Exit, 0).unwrap()
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
    fn a_frame_changing_helper_emits_no_separate_refresh_call() {
        let mut accessing = Allocation::default();
        accessing.y_last = Some(9);
        // A frame-changing helper re-publishes the Y window **inside** itself: its
        // `push_frame`/`trim` moves `machine.frames.last()`, and `JitRuntime::refresh_y_base`
        // is a pure re-derivation from machine state, so the helper that just invalidated the
        // window is exactly the party that can restore it for free. The emitted continue arm
        // therefore never pays a second helper call for it — whether or not a later
        // instruction of the block still accesses a Y slot. The window is never *stale* at a
        // Y access (the helper leaves it live), and the emitted guard stays reject-only.
        for (label, code) in [
            (
                "allocate",
                lowered(|buffer, done| emit_allocate(buffer, &accessing, done, 0, 7, 2)),
            ),
            (
                "trim",
                lowered(|buffer, done| emit_trim(buffer, &accessing, done, 0, 7, 1)),
            ),
            (
                "allocate_heap",
                lowered(|buffer, done| emit_allocate_heap(buffer, &accessing, done, 0, 7, 2, 9, 3)),
            ),
        ] {
            assert!(
                !contains(&code, &refresh_immediate()),
                "{label}: the helper re-published the window, so the arm emits no refresh call"
            );
        }
        // The frame change is still followed by the continue check — that is what lets the
        // block carry on in-block after it (and, for `allocate_heap`, still reload the
        // x-registers a collecting host rewrote).
        let allocate = lowered(|buffer, done| emit_allocate(buffer, &accessing, done, 0, 7, 2));
        let next = encoded(|buffer| buffer.mov_ri(Reg::R11, encode_target(0, 8)).unwrap());
        assert!(
            allocate
                .windows(next.len())
                .any(|window| window == next.as_slice()),
            "the continue arm still compares against the next instruction"
        );
        // `test_heap` changes no frame, so its window is still the one the block published
        // and it keeps the gated emitted refresh.
        let heap_refreshed = lowered(|buffer, done| {
            emit_test_heap(buffer, &accessing, done, 0, 7, 9, 3)
        });
        let heap_plain = lowered(|buffer, done| {
            emit_test_heap(buffer, &Allocation::default(), done, 0, 7, 9, 3)
        });
        assert!(contains(&heap_refreshed, &refresh_immediate()));
        assert!(!contains(&heap_plain, &refresh_immediate()));
    }
    /// `allocate` is the one family member that is *not* a bare helper call: its push is
    /// emitted inline behind the two capacity guards, and the helper is the cold half.
    #[test]
    fn an_allocate_pushes_inline_behind_both_capacity_guards_and_keeps_the_helper_cold() {
        let allocation = cached_allocation();
        let spill = spill_bytes(&allocation);
        let rt_load = encoded(|buffer| {
            buffer.mov_rm(Reg::Rdi, Reg::Rsp, super::super::RUNTIME_SLOT_DISP).unwrap()
        });
        let guarded = lowered(|buffer, done| emit_allocate(buffer, &allocation, done, 4, 7, 2));
        assert!(
            guarded.starts_with(&rt_load),
            "the inline push loads `rt` out of the block's own slot first"
        );
        assert!(
            !guarded.starts_with(&spill),
            "the inline push is not a call, so it spills nothing on its success path"
        );
        let needle = helper_immediate(jit_allocate as *const ());
        let call = guarded
            .windows(needle.len())
            .position(|window| window == needle)
            .expect("the cold half still calls `jit_allocate`");
        assert!(
            call > rt_load.len(),
            "the helper call sits behind the capacity guards, not in front of them"
        );
        // The cold half is the pre-existing shape: spill, then the call. Anchoring the
        // spill *before* the helper's immediate is what proves the guards are the only
        // thing standing between the block and the old code path.
        let spill_at = guarded
            .windows(spill.len())
            .position(|window| window == spill)
            .expect("the cold half spills the root set before the call");
        assert!(spill_at < call, "the cold half spills before it calls");
        // A frame wider than the inline bound keeps the old single-call shape.
        let wide = lowered(|buffer, done| {
            emit_allocate(buffer, &allocation, done, 4, 7, super::INLINE_PUSH_MAX_Y + 1)
        });
        assert!(
            wide.starts_with(&spill),
            "a wide frame takes the helper first, like before the inline push existed"
        );
    }
}
