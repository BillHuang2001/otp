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

//! The baseline JIT's **block linker**: a taken branch to a successor basic block
//! that was compiled jumps **straight to that block's native entry** instead of
//! returning the successor's `(func, pc)` word to the driver.
//!
//! # Why a direct jump is exact
//!
//! Every compiled block is a *standalone* function with its own push-prologue /
//! pop-epilogue pair (see [`super`]), so a block boundary is always a complete
//! frame boundary: the cached x-registers are back in `machine.x` and `rsp` points
//! at the **original return address** the driver's `call` pushed. A direct jump
//! therefore reproduces exactly what the driver's round-trip does, minus the
//! round-trip:
//!
//! ```text
//!   spill             ; the block's dirty cached x-registers back into
//!                     ; `machine.x` — MANDATORY: this is the tier's whole
//!                     ; liveness / root-set model, so a linked exit must never
//!                     ; skip it (see [`super::spill`]) — except under
//!                     ; [`ExitSpill::Done`], where a helper call emitted
//!                     ; immediately before the exit already stored that very
//!                     ; same dirty set at that very same `pc`
//!   mov rdi, [rsp+RUNTIME_SLOT_DISP]  ; `rt` for the successor's prologue
//!   lea rsp, [rsp+slot] ; drop the `rt` slot (`slot` = the prologue's own reservation)
//!   pop … ; leave the frame, restoring exactly the registers the prologue pushed
//!         ; (`rbx` plus the block's cache slots — see [`native_frame_regs`])
//!   jmp <successor entry>   ; the successor's own prologue rebuilds its frame; the
//!                           ; original return address is untouched, so *its* `ret`
//!                           ; returns to the driver
//! ```
//!
//! The `rdi` reload is what makes a `jmp` equivalent to the driver's `call`: the
//! driver passes `rt` in `rdi` (System V's first argument), so a block reached by a
//! jump must find it there too — its prologue stores `rdi` into *its* `rt` slot and
//! loads `rbx` (the `machine.x` base) from it. `rdi` is caller-saved and is not one
//! of the registers [`emit_frame_teardown`] pops, so the reloaded value survives
//! both the teardown and the jump. The `ret`-terminated `done` epilogue does **not**
//! need it: it returns to the driver, which never reads `rdi` afterwards.
//!
//! The return word is deliberately **not** computed: the successor is responsible
//! for the word the driver eventually sees, exactly as if the driver had decoded
//! the branch target and called the successor itself.
//!
//! # The local tail call (`call_last`)
//!
//! [`Linker::emit_tail_exit`] links a **local tail call** the same way, with the one
//! extra piece `call_last` owns: the current frame must be popped *before* control is
//! transferred (`i_call_last` is `$deallocate(D); $DISPATCH_REL(Dest)`,
//! `emu/instrs.tab:163`). The call-frame stack is Rust-side [`Machine`] state a block
//! cannot address, so the pop goes through [`jit_tail_call`] — literally
//! `Machine::pop_frame_checked`, the call the interpreter's own `Instr::CallLast` arm
//! makes — while the target is a compile-time constant (a module-local label) and is
//! therefore resolved **before** anything is popped, exactly like the interpreter's
//! `resolve`-then-`pop` order:
//!
//! ```text
//!   spill                        ; the root-set contract (see above)
//!   mov rdi, [rsp+RUNTIME_SLOT_DISP]  ; `rt`, the helper's first argument
//!   mov rsi, <resolved successor word>
//!   mov rdx, <deallocate>
//!   call <jit_tail_call>         ; the interpreter's checked frame pop; returns the
//!                                ; successor word, or `ERROR` (rt.error set)
//!   cmp rax, <resolved successor word>
//!   jne <done>                   ; a failed pop: back to the driver with `ERROR`
//!   mov rdi, [rsp+RUNTIME_SLOT_DISP]  ; …and `rt` again for the successor's prologue
//!   lea rsp, [rsp+slot] ; pop the saved set   ; leave the frame
//!   jmp <successor entry>        ; the successor's prologue rebuilds its own frame
//! ```
//!
//! A failed pop (a `Deallocate` operand that does not match the live frame — a
//! `trim`ed frame, or a tail call reaching past the bottom frame) takes the `jne` to
//! the shared epilogue, whose `ERROR` word makes the driver propagate the exact typed
//! error the interpreter produces. A successor that is a *resolved* label this
//! compilation emitted no block for (another function of a lazy per-function artifact,
//! or an address inside a block) is not linked either, but it is not handed to the
//! interpreter: [`Linker::emit_tail_exit_to_driver`] does the same checked pop and then
//! leaves the resolved `(func, pc)` word in `rax` for the driver — the interpreter's own
//! `Step::Goto`. Only an **undefined** label keeps the documented deopt, so the
//! interpreter's own `call_last` — and therefore its exact `unknown_label` error —
//! surfaces.
//!
//! `call_only` (a local tail call with **no** frame to pop) needs no helper: it is
//! `$DISPATCH_REL(Dest)`, i.e. [`Linker::emit_exit`] exactly as `jump` uses it.
//! `call_ext_last` is *not* linked: it enters code outside this module's compiled
//! blocks, so it keeps its own deopt.
//!
//! # The frame-preserving local `call` (a **non**-tail call)
//!
//! Its continuation store goes through a helper (`super::super::call`), and its transfer
//! has two shapes, chosen at compile time:
//!
//! * the callee's block **is** in this compilation's map (the eager whole-module artifact,
//!   or a call inside the function being compiled) — [`Linker::emit_exit`]'s linked path,
//!   exactly as `call_only` uses it;
//! * the callee is **another function of the same module** the lazy per-function artifact
//!   has not compiled — the runtime path's shape — and the block asks for
//!   [`jit_call_store_linked`](super::super::call::jit_call_store_linked) instead, which
//!   stores the continuation **and** resolves the callee's native entry (lazily compiling
//!   that function, checking its register-file bound and performing the skipped driver
//!   iteration). [`Linker::emit_in_block_call`] then tail-jumps into it.
//!
//! [`Linker::emit_in_block_call`] is the frame-preserving call's counterpart of
//! [`emit_resume_or_exit`](super::frame)'s return-side transfer, and it is a **decline-only**
//! emitter: every outcome the helper could not resolve (no published artifact, an
//! uncovered address, a failed lazy compile, an unsatisfied bound, a refused pre-emption
//! charge) leaves the same `encode_target` word in `rax` that the unlinked exit produced
//! before the transfer existed.
//!
//! # What is *not* linked
//!
//! `(func, pc)` is linked directly only when it is the start of a block **this
//! compilation emitted** — including blocks of **other functions** of the same module
//! (`fib/1`'s tail call into `fib/3`) for the *eager* whole-module artifact, because
//! every block is a standalone frame. A `call_last` whose resolved target is not in the
//! map (the lazy per-function artifact's cross-function case) still runs natively: it
//! takes [`Linker::emit_tail_exit_to_driver`]'s checked pop + return-to-driver, exactly
//! the interpreter's `Step::Goto`. Every other continuation keeps today's documented
//! deopt: spill, materialise the `encode_target` word in `rax`, and jump to the block
//! epilogue so the driver decodes it and takes one interpreter step (an unexplained
//! label — the interpreter's `unknown_label` — plus, for `jump`/`call_only`, an
//! address inside a block, an empty block's start and the end of a function's code).
//!
//! A cross-**module** transfer (`call_ext*`) is never linked at all: the block ABI's only
//! vocabulary for it is [`ENTER`](super::super::runtime::ENTER), because the driver owns the
//! module-context registry (`Machine::switch_context`), the label-map and literal-area
//! rebinding and the per-callee artifact publication, and a block's `rt.module`/`rt.labels`
//! are baked for one module.
//!
//! # How the successor is resolved without post-`seal` patching
//!
//! [`compile_module`](super::compile_module) reserves one [`CodeLabel`] per compiled
//! block **before** emitting any code and binds it at the block's entry offset, so a
//! branch to a not-yet-emitted (forward) block is a normal forward `rel32` reference
//! the [`CodeBuffer`](crate::jit::emitter::CodeBuffer) resolves at [`bind`] time —
//! before [`seal`](crate::jit::emitter::CodeBuffer::seal) flips the mapping to
//! read+execute. There is no self-modifying code and no second pass, so the W^X
//! policy is unchanged.

use std::collections::BTreeMap;

use beam_common::Result;

use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};

use super::super::runtime::{encode_target, jit_tail_call, TailHelper, RUNTIME_RESUME_DISP};
use super::{native_frame_regs, frame_slot_len, spill, Allocation, SpillSite, RUNTIME_SLOT_DISP};

/// Whether a block exit still owes the cached-register **spill**, or whether the code
/// emitted immediately in front of it has already done that store.
///
/// A [`spill`] is site- and path-independent: [`spill_slots_at`] stores exactly the cached
/// x-registers **dirty at the exit's `pc`**, and no other code in the tier stores them. So
/// two spill sites reaching the *same* `pc` with no cached-register write in between are
/// two stores of the same words — the second one is pure redundancy, and that is what
/// [`ExitSpill::Done`] declares.
///
/// `Done` is sound only under all three of these, which the two probed local-call shapes
/// (and only they) satisfy:
///
/// 1. the caller emitted [`emit_helper_call`](super::emit_helper_call) **first**, whose
///    own [`SpillSite::Helper`] spill at the **same `pc`** is the tier's mandatory
///    root-set contract (it must run for the helper's sake, so it is never the one
///    removed);
/// 2. nothing emitted between that helper call and this exit writes a cached x-register
///    (the only code there is a helper `call`, a `mov` of a constant into the caller-saved
///    scratch `r11`, a `cmp` and a `jcc` — no cache slot, no `machine.x` write);
/// 3. the helper's **decline** path mutates nothing —
///    [`jit_local_call`](super::super::runtime::jit_local_call) mutates neither `machine.x`
///    nor any cached register on the decline it hands back to the exit, and
///    [`jit_call_store`](super::super::call::jit_call_store) only stores the continuation
///    on the path this exit is reached from — so the values already spilled are still the
///    ones `machine.x` must receive.
///
/// Every other exit — the `jump`, the `test`/`select_val` fail-label branches, the
/// block's fall-through — passes [`ExitSpill::Required`]: no helper ran in front of it, so
/// the store is the exit's own.
///
/// [`spill_slots_at`]: super::Allocation::spill_slots_at
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ExitSpill {
    /// The exit owns the store: nothing ahead of it spilled this `pc`.
    Required,
    /// A helper call ahead of this exit already spilled the same dirty set at the same
    /// `pc` (see the type docs); the exit emits no second store.
    Done,
}

impl ExitSpill {
    /// Emit the exit's cached-register store, or nothing when it is already done.
    fn emit(
        self,
        buffer: &mut CodeBuffer,
        allocation: &Allocation,
        pc: u32,
    ) -> Result<()> {
        match self {
            ExitSpill::Required => spill(buffer, allocation, SpillSite::Exit, pc),
            ExitSpill::Done => Ok(()),
        }
    }
}

/// The compiled blocks a branch may jump to directly, keyed by `(func, block
/// start)` — the same key the driver's entry table uses.
///
/// Holding the entries as labels (rather than as the absolute entry addresses the
/// driver's table stores) is what makes a **forward** reference work: the label is
/// bound when the successor is emitted, and the `rel32` displacement is patched
/// before the buffer is sealed.
pub(super) struct Linker<'a> {
    /// Every compiled block's entry label.
    entries: &'a BTreeMap<(u32, u32), CodeLabel>,
}

impl<'a> Linker<'a> {
    /// A linker over the compiled block table `entries`.
    pub(super) fn new(entries: &'a BTreeMap<(u32, u32), CodeLabel>) -> Linker<'a> {
        Linker { entries }
    }

    /// Is `(func, pc)` the entry of a block this compilation emitted?
    ///
    /// The one question a *tail* call has to ask before it may be linked: unlike a
    /// `jump`/`fail` target, a tail call can be emitted by a block that would
    /// otherwise continue, so the lowering has to choose between the linked exit and
    /// the documented deopt **before** it emits anything.
    pub(super) fn is_compiled(&self, func: u32, pc: u32) -> bool {
        self.entries.contains_key(&(func, pc))
    }
    /// Emit the block exit that transfers control to `(func, pc)`.
    ///
    /// A **linked** successor (`(func, pc)` is a compiled block's entry) is reached
    /// by leaving this block's frame and jumping straight to that block's entry;
    /// anything else is the documented return-to-driver deopt (spill, the
    /// `encode_target` word in `rax`, `jmp done`).
    ///
    /// The cached x-registers are spilled on **both** paths, before the frame is
    /// torn down — that spill *is* the tier's liveness/root-set model, so an exit
    /// may never skip it — **unless** the caller passes [`ExitSpill::Done`], which
    /// declares that a helper call emitted immediately before this exit already
    /// stored the very same dirty set at the very same `pc` (the probed local-call
    /// shapes; see [`ExitSpill`]). The linked path additionally reloads this block's
    /// `rt` into `rdi` (the block ABI's first argument), because a successor entered by a
    /// `jmp` gets no `call` to set it.
    pub(super) fn emit_exit(
        &self,
        buffer: &mut CodeBuffer,
        allocation: &Allocation,
        done: CodeLabel,
        func: u32,
        pc: u32,
        store_pc: u32,
        spill_mode: ExitSpill,
    ) -> Result<()> {
        match self.entries.get(&(func, pc)) {
            Some(&target) => {
                spill_mode.emit(buffer, allocation, store_pc)?;
                // `rt` for the successor's prologue: it stores `rdi` into *its* `rt`
                // slot and loads `rbx` from it. `rdi` is not one of the registers the
                // teardown pops, so the reloaded value survives the teardown and the
                // jump.
                buffer.mov_rm(Reg::Rdi, Reg::Rsp, RUNTIME_SLOT_DISP)?;
                emit_frame_teardown(buffer, allocation)?;
                buffer.jmp(target)
            }
            None => {
                spill_mode.emit(buffer, allocation, store_pc)?;
                buffer.mov_ri(Reg::Rax, encode_target(func, pc))?;
                buffer.jmp(done)
            }
        }
    }

}

/// Emit the **in-block transfer** of a frame-preserving local `call` whose callee
    /// block is not part of this compilation.
    ///
    /// This is the shape the runtime path actually takes: `CodeCache::compiled_handle`
    /// stages a **lazy** per-function artifact, so a `call` into another function of the
    /// same module is never in this compilation's block map and
    /// [`Linker::emit_exit`]'s `None` arm would hand the driver a word — one full driver
    /// round-trip per call (position store, reduction bump, `reduce(1)`, block-entry
    /// lookup, block prologue). The block instead asks for
    /// [`jit_call_store_linked`](super::super::call::jit_call_store_linked), which stores
    /// the continuation **and** resolves the callee's native entry, lazily compiling that
    /// function on first entry, checking its register-file bound and performing the
    /// skipped driver iteration's work.
    ///
    /// Entered with `rax == CALL_STORED`; the emitted sequence is
    /// [`emit_resume_or_exit`](super::frame)'s return-side twin:
    ///
    /// ```text
    ///   mov  rdi, [rsp + RUNTIME_SLOT_DISP] ; rt — reloaded, the helper may clobber rdi
    ///   mov  r11, [rdi + RUNTIME_RESUME_DISP] ; the callee's entry, or null on a decline
    ///   test r11, r11
    ///   je   <declined>                   ; no entry: today's word exit
    ///   lea  rsp, [rsp + slot] ; pop …    ; leave this block's native frame
    ///   jmp  r11                          ; the callee's prologue rebuilds its own frame
    /// <declined>:
    ///   mov  rax, encode_target(func, pc) ; the pre-existing unlinked word
    ///   jmp  <done>
    /// ```
    ///
    /// The cached x-registers were **already spilled** by
    /// [`emit_helper_call`](super::emit_helper_call) before the helper — that spill is the
    /// tier's liveness/root-set model and it is mandatory, which is why this emitter emits
    /// no second one.
    ///
    /// The transfer is a **tail** transfer: the frame is torn down before the jump, so the
    /// native stack does not grow with Erlang non-tail recursion (each nested `call` leaves
    /// its native frame behind before entering the callee, whose own prologue builds a
    /// fresh one) and no stale frame is left behind. `rdi`/`r11` are caller-saved and the
    /// teardown only pops the block's saved callee-saved set, so both survive it.
    ///
    /// Nothing here can produce a wrong result: the helper resolves an entry only for a
    /// block of the artifact the driver published, inside the register-file bound it
    /// checked, and every other outcome — no artifact (an interpreter-step iteration), an
    /// address no block covers, a missing function, a failed lazy compile, an unsatisfied
    /// bound, a **refused `ctx.reduce` charge** (`ERROR` in `rax`, the host's error in
    /// `rt.error`) — leaves `resume_entry` null and takes the declined arm, which hands the
    /// driver exactly the word it decoded before this transfer existed.
    ///
    /// [`jit_call_store_linked`]: super::super::call::jit_call_store_linked
    pub(super) fn emit_in_block_call(
        buffer: &mut CodeBuffer,
        allocation: &Allocation,
        done: CodeLabel,
        func: u32,
        pc: u32,
    ) -> Result<()> {
        let declined: CodeLabel = buffer.label();
        // `rt`, reloaded from the block's own slot: `jit_call_store_linked` is an ordinary
        // `extern "C"` call and `rdi` is caller-saved, so it may hold anything now. It is
        // also what the callee block's prologue stores as *its* `rt` slot.
        buffer.mov_rm(Reg::Rdi, Reg::Rsp, RUNTIME_SLOT_DISP)?;
        // The callee entry the helper resolved, or null on every decline.
        buffer.mov_rm(Reg::R11, Reg::Rdi, RUNTIME_RESUME_DISP)?;
        buffer.test_rr(Reg::R11, Reg::R11)?;
        buffer.jcc(Cond::E, declined)?;
        // A tail transfer: leave this block's native frame, then jump (the callee's own
        // prologue rebuilds a frame), exactly `emit_exit`'s linked path.
        emit_frame_teardown(buffer, allocation)?;
        buffer.jmp_r(Reg::R11)?;
        buffer.bind(declined)?;
        // The declined path is byte-for-byte today's `emit_exit` unlinked arm: the resolved
        // target's word in `rax` for the driver to decode.
        buffer.mov_ri(Reg::Rax, encode_target(func, pc))?;
        buffer.jmp(done)
    }

impl<'a> Linker<'a> {
    /// Emit the exit of a **linkable local tail call** (`call_last`): pop the current
    /// frame with the interpreter's exact semantics, then jump straight to the
    /// successor block's entry.
    ///
    /// `(func, pc)` must be a compiled block's entry — the caller checks with
    /// [`Linker::is_compiled`]; a typed error is returned otherwise rather than a
    /// panic, because a block that named a target it has no landing address for could
    /// only produce a wrong result.
    ///
    /// This is [`Linker::emit_exit`]'s linked path with the frame pop inserted in front
    /// of the transfer, in the interpreter's own order (`resolve` then
    /// `pop_frame_checked` then transfer — the resolution being a compile-time constant
    /// here): the pop happens **while this block's frame is still live**, so a
    /// frame-size mismatch raises exactly as it does on the interpreter path, and the
    /// spill (the tier's root-set contract, see [`super::spill`]) happens before the
    /// successor's prologue reloads `machine.x` — skipped only under [`ExitSpill::Done`],
    /// the probed local-call shape whose helper call already stored the same dirty set at
    /// the same `pc`.
    pub(super) fn emit_tail_exit(
        &self,
        buffer: &mut CodeBuffer,
        allocation: &Allocation,
        done: CodeLabel,
        func: u32,
        pc: u32,
        deallocate: u32,
        store_pc: u32,
        spill_mode: ExitSpill,
    ) -> Result<()> {
        let &target = self.entries.get(&(func, pc)).ok_or_else(|| {
            crate::error::unsupported("jit: tail-call target has no compiled block")
        })?;
        let word = encode_target(func, pc);

        // The root set first: the successor's prologue loads `machine.x`, and a failed
        // pop returns to a driver that owns the same root set. `jit_tail_call` reads no
        // x-register, so the spill order relative to the call is free — this one keeps
        // the exit shaped like every other exit in the tier.
        spill_mode.emit(buffer, allocation, store_pc)?;
        // `rt` (the helper's first argument), the already-resolved successor and the
        // frame size the pop must match.
        buffer.mov_rm(Reg::Rdi, Reg::Rsp, RUNTIME_SLOT_DISP)?;
        buffer.mov_ri(Reg::Rsi, word)?;
        buffer.mov_ri(Reg::Rdx, i64::from(deallocate))?;
        let helper: TailHelper = jit_tail_call;
        buffer.mov_ri(Reg::R11, helper as *const () as i64)?;
        buffer.call_r(Reg::R11)?;
        // `rax` is the successor word on success and `ERROR` (with `rt.error` set)
        // otherwise; the compiled-in word is the constant to compare against, so any
        // other outcome goes to the epilogue and the driver reports it.
        buffer.mov_ri(Reg::R11, word)?;
        buffer.cmp_rr(Reg::Rax, Reg::R11)?;
        buffer.jcc(Cond::Ne, done)?;
        // The frame is popped. Leave this block's native frame and jump into the
        // successor, which is entered by a `jmp` and so gets no `call` to set the `rdi`
        // its prologue stores as its `rt` (see the module docs).
        buffer.mov_rm(Reg::Rdi, Reg::Rsp, RUNTIME_SLOT_DISP)?;
        emit_frame_teardown(buffer, allocation)?;
        buffer.jmp(target)
    }

    /// Emit the exit of a **resolved but unlinked** local tail call (`call_last`):
    /// pop the current frame with the interpreter's exact semantics, then hand the
    /// successor's `(func, pc)` word back to the driver in `rax` — exactly what the
    /// interpreter's own `Step::Goto` folds to.
    ///
    /// This is [`Linker::emit_tail_exit`] **minus** the final frame teardown + direct
    /// jump: the frame pop is still the interpreter's own checked pop
    /// ([`jit_tail_call`] = `Machine::pop_frame_checked`, the call the interpreter's
    /// `Instr::CallLast` arm makes) and the block still spills the cached x-registers
    /// first (the tier's root-set contract, see [`super::spill`]), but the transfer is
    /// the shared `done` epilogue's — which `ret`s to the driver with the helper's word
    /// still in `rax`. The driver decodes that word into `(func, pc)` and takes one
    /// interpreter step there, i.e. the interpreter's own `call_last` arm
    /// (`resolve` → `pop_frame_checked` → `Step::Goto`) — so this exit is
    /// interpreter-identical **by construction** and needs no landing address.
    ///
    /// Unlike [`Linker::emit_tail_exit`] it therefore never returns a typed error: a
    /// resolved label this compilation emitted no block for (the common case for a tail
    /// call into **another** function of the lazy per-function artifact) is precisely
    /// what the driver serves.
    ///
    /// `spill_mode` is [`ExitSpill::Done`] for the probed local-call shape, whose helper
    /// call already stored the same dirty set at the same `pc`; every other caller passes
    /// [`ExitSpill::Required`].
    pub(super) fn emit_tail_exit_to_driver(
        &self,
        buffer: &mut CodeBuffer,
        allocation: &Allocation,
        done: CodeLabel,
        func: u32,
        pc: u32,
        deallocate: u32,
        store_pc: u32,
        spill_mode: ExitSpill,
    ) -> Result<()> {
        let word = encode_target(func, pc);

        // The root set first (the exit's contract, see [`super::spill`]), then `rt` and
        // the helper's two value arguments: the resolved successor word and the frame
        // size the pop must match.
        spill_mode.emit(buffer, allocation, store_pc)?;
        buffer.mov_rm(Reg::Rdi, Reg::Rsp, RUNTIME_SLOT_DISP)?;
        buffer.mov_ri(Reg::Rsi, word)?;
        buffer.mov_ri(Reg::Rdx, i64::from(deallocate))?;
        let helper: TailHelper = jit_tail_call;
        buffer.mov_ri(Reg::R11, helper as *const () as i64)?;
        buffer.call_r(Reg::R11)?;
        // `rax` is the successor word on success and `ERROR` (with `rt.error` set)
        // otherwise. Compare it against the compiled-in word: any other outcome leaves for
        // the epilogue, whose `ERROR` the driver reports as the interpreter's exact frame
        // error. A success falls through with the word **still in `rax`** — nothing after
        // the compare writes `rax` — so the driver decodes it and steps at that address,
        // exactly as it decodes the word `emit_exit`'s unlinked path leaves there.
        buffer.mov_ri(Reg::R11, word)?;
        buffer.cmp_rr(Reg::Rax, Reg::R11)?;
        buffer.jcc(Cond::Ne, done)?;
        buffer.jmp(done)
    }
}

/// Drop the block's stack frame: release the `rt` slot and restore every callee-saved
/// register the prologue pushed, leaving `rsp` on the **original return address** (so a
/// following `ret` — or a linked jump into a successor whose own `ret` does it — returns
/// to the driver).
///
/// The exact inverse of [`super::emit_block`]'s prologue, defined once so the two exits
/// that need it (the shared `done` epilogue and a linked jump) can never drift apart: the
/// saved set is exactly [`native_frame_regs`]`(allocation)` and the `rt` slot reservation
/// exactly [`frame_slot_len`]`(saved.len())`.
pub(super) fn emit_frame_teardown(buffer: &mut CodeBuffer, allocation: &Allocation) -> Result<()> {
    let frame = native_frame_regs(allocation);
    buffer.lea(Reg::Rsp, Reg::Rsp, frame_slot_len(frame.len()))?;
    for reg in frame.iter().rev() {
        buffer.pop_r(*reg)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The emitted bytes of `buffer` (a raw view of the still-unsealed mapping).
    fn emitted(buffer: &CodeBuffer) -> Vec<u8> {
        // SAFETY: `code_ptr()` is the start of the buffer's live mapping and
        // `len()` bytes of it have been emitted, so `[ptr, ptr + len)` is a valid,
        // initialized range; the slice borrows nothing that is mutated while it
        // lives.
        unsafe { std::slice::from_raw_parts(buffer.code_ptr(), buffer.len()).to_vec() }
    }

    /// The bytes the emitter itself produces for `f` on a throwaway buffer.
    ///
    /// The expected encodings of the pieces an exit is composed of are always taken
    /// from `CodeBuffer` rather than hard-coded, so a test pins the *composition* of
    /// the exit (`spill`, reload, teardown, jump) without pinning a single byte
    /// offset — an emitter encoding detail (a `REX.W` prefix, a displacement form)
    /// cannot make the test lie about the linker.
    fn encoded(f: impl FnOnce(&mut CodeBuffer)) -> Vec<u8> {
        let mut buffer = CodeBuffer::new().unwrap();
        f(&mut buffer);
        emitted(&buffer)
    }

    /// The trailing `jmp rel32` of `code`, as `(site offset, displacement)`.
    ///
    /// The exit *ends* in that jump on both paths, so the caller passes exactly the
    /// exit's bytes — not a buffer with anything (a filler, an epilogue) appended.
    fn trailing_jump(code: &[u8]) -> (usize, i32) {
        let site = code.len() - 5;
        assert_eq!(code[site], 0xE9, "the exit must end in a `jmp rel32`");
        let rel = i32::from_le_bytes(code[site + 1..site + 5].try_into().unwrap());
        (site, rel)
    }

    /// `spill` of `allocation` (its **written** cache), as emitted.
    fn spill_bytes(allocation: &Allocation) -> Vec<u8> {
        encoded(|buffer| spill(buffer, allocation, SpillSite::Exit, 0).unwrap())
    }

    /// The linked exit's `mov rdi, [rsp + RUNTIME_SLOT_DISP]` — the reload that
    /// hands the successor's prologue its `rt` — as emitted.
    fn reload_bytes() -> Vec<u8> {
        encoded(|buffer| {
            buffer
                .mov_rm(Reg::Rdi, Reg::Rsp, RUNTIME_SLOT_DISP)
                .unwrap()
        })
    }

    /// `emit_frame_teardown` of `allocation`, as emitted.
    fn teardown_bytes(allocation: &Allocation) -> Vec<u8> {
        encoded(|buffer| emit_frame_teardown(buffer, allocation).unwrap())
    }

    #[test]
    fn a_linkable_successor_is_jumped_to_directly() {
        let mut buffer = CodeBuffer::new().unwrap();
        let target = buffer.label();
        let done = buffer.label();
        let mut entries = BTreeMap::new();
        entries.insert((0u32, 7u32), target);
        let linker = Linker::new(&entries);
        let allocation = Allocation::default();

        linker
            .emit_exit(&mut buffer, &allocation, done, 0, 7, 0, ExitSpill::Required)
            .unwrap();
        let exit_len = buffer.len();
        // A filler byte between the exit and the successor's entry, so the
        // displacement is non-trivial (a `jmp` to the very next byte would be 0).
        buffer.ret().unwrap();
        let target_at = buffer.len();
        buffer.bind(target).unwrap();
        buffer.bind(done).unwrap();
        buffer.seal().unwrap();

        let code = emitted(&buffer);
        let exit = &code[..exit_len];
        let (site, rel) = trailing_jump(exit);
        assert_eq!(site, exit_len - 5, "the jump is the exit's last 5 bytes");
        // A `rel32` is measured from the end of the instruction (`jmp` is `E9` + the
        // four displacement bytes), so the landing address is `site + 5 + rel`.
        assert_eq!(
            (site + 5) as i64 + i64::from(rel),
            target_at as i64,
            "the direct jump must land on the successor's entry"
        );

        // The exact composition: the spill, the `rt` reload the successor's prologue
        // reads out of `rdi`, the frame teardown and the jump — nothing else.
        let mut expected = spill_bytes(&allocation);
        expected.extend(reload_bytes());
        assert!(
            !reload_bytes().is_empty(),
            "the reload is what the fix adds"
        );
        expected.extend(teardown_bytes(&allocation));
        assert_eq!(
            &exit[..expected.len()],
            &expected[..],
            "the exit is spill + `mov rdi, [rsp+rt]` + frame teardown + `jmp rel32`"
        );
        assert_eq!(
            exit.len(),
            expected.len() + 5,
            "plus the 5-byte `jmp rel32`"
        );

        // And the return word is *not* materialised: no `movabs rax, imm64`.
        assert!(
            !exit.windows(2).any(|w| w == [0x48, 0xB8]),
            "a linked exit never materialises a return word in rax"
        );
    }

    #[test]
    fn a_linked_exit_reloads_its_runtime_pointer_for_the_successor() {
        // The regression this pins: a successor reached by `jmp` is *not* entered
        // through the driver's `call`, so nothing sets the `rdi` its prologue stores
        // as its `rt`. The reload must read this block's own `rt` slot **before** the
        // teardown releases that slot.
        let mut buffer = CodeBuffer::new().unwrap();
        let target = buffer.label();
        let done = buffer.label();
        let mut entries = BTreeMap::new();
        entries.insert((0u32, 7u32), target);
        let linker = Linker::new(&entries);
        let allocation = Allocation::default();

        linker
            .emit_exit(&mut buffer, &allocation, done, 0, 7, 0, ExitSpill::Required)
            .unwrap();
        let exit_len = buffer.len();
        buffer.bind(target).unwrap();
        buffer.bind(done).unwrap();
        buffer.seal().unwrap();

        let exit = &emitted(&buffer)[..exit_len];
        // It reads the block's own slot: `[rsp + RUNTIME_SLOT_DISP]`.
        assert_eq!(
            RUNTIME_SLOT_DISP, 0,
            "the prologue reserves the slot at rsp"
        );
        let reload = reload_bytes();
        let reload_at = exit
            .windows(reload.len())
            .position(|window| window == reload.as_slice())
            .expect("the linked exit must reload `rdi` from the block's `rt` slot");
        let teardown = teardown_bytes(&allocation);
        let teardown_at = exit
            .windows(teardown.len())
            .position(|window| window == teardown.as_slice())
            .expect("the linked exit must tear its frame down");
        assert!(
            reload_at < teardown_at,
            "the reload must precede the teardown (which drops the slot)"
        );
        // … and it is reloaded into `rdi`, where the successor's prologue looks for it.
        assert_eq!(
            reload[2] & 0b0011_1000,
            0b0011_1000,
            "the load targets `rdi`"
        );
    }

    #[test]
    fn an_unlinkable_successor_returns_its_word_to_the_driver() {
        let mut buffer = CodeBuffer::new().unwrap();
        let done = buffer.label();
        // No compiled block at `(0, 7)`: the exit must be today's deopt.
        let entries = BTreeMap::new();
        let linker = Linker::new(&entries);
        let allocation = Allocation::default();

        linker
            .emit_exit(&mut buffer, &allocation, done, 0, 7, 0, ExitSpill::Required)
            .unwrap();
        let exit_len = buffer.len();
        buffer.bind(done).unwrap();
        buffer.seal().unwrap();

        let code = emitted(&buffer);
        let exit = &code[..exit_len];
        let (site, rel) = trailing_jump(exit);
        assert_eq!(site, exit_len - 5);
        assert_eq!(rel, 0, "the epilogue follows the exit immediately");

        // The word is materialised in `rax`, then control goes to the shared epilogue
        // (which `ret`s, so no `rdi` reload is needed here).
        let mut expected = spill_bytes(&allocation);
        let word_bytes = encoded(|buffer| buffer.mov_ri(Reg::Rax, encode_target(0, 7)).unwrap());
        expected.extend_from_slice(&word_bytes);
        assert_eq!(
            &exit[..expected.len()],
            &expected[..],
            "the deopt exit is spill + the return word in rax + `jmp rel32`"
        );
        assert_eq!(exit.len(), expected.len() + 5);
        // The materialised word really is `encode_target(0, 7)`.
        let word = i64::from_le_bytes(exit[expected.len() - 8..expected.len()].try_into().unwrap());
        assert_eq!(word, encode_target(0, 7));

        // An unlinkable exit never reloads `rdi`: no frame is entered by a jump.
        let reload = reload_bytes();
        assert!(
            !exit
                .windows(reload.len())
                .any(|window| window == reload.as_slice()),
            "only the linked path readies `rdi` for a successor"
        );
    }

    #[test]
    fn every_exit_spills_the_cached_registers_before_leaving_the_frame() {
        // The spill is the tier's liveness model: with one cached x-register (x0 in
        // r12) **written by an inline lowering**, the exit must store it to `[rbx + 0]`
        // *before* the frame teardown — on the linked path too, because the successor's
        // prologue reloads `machine.x`. The store set is the block's write set
        // (`Allocation::written`), which here is the single written register.
        let allocation = Allocation {
            slots: vec![(0u32, Reg::R12)],
            written: vec![0],
            y_last: None,
            ..Default::default()
        };
        let store = encoded(|buffer| buffer.mov_mr(Reg::Rbx, 0, Reg::R12).unwrap());
        let spill = spill_bytes(&allocation);
        assert_eq!(spill, store, "the spill stores x0 from r12 to `machine.x`");

        for (func, pc, linkable) in [(0u32, 0u32, true), (0u32, 7u32, false)] {
            let mut buffer = CodeBuffer::new().unwrap();
            let target = buffer.label();
            let done = buffer.label();
            let mut entries = BTreeMap::new();
            if linkable {
                entries.insert((func, pc), target);
            }
            let linker = Linker::new(&entries);

            linker
                .emit_exit(&mut buffer, &allocation, done, func, pc, pc, ExitSpill::Required)
                .unwrap();
            let exit_len = buffer.len();
            buffer.bind(target).unwrap();
            buffer.bind(done).unwrap();
            buffer.seal().unwrap();

            let exit = &emitted(&buffer)[..exit_len];
            let what = if linkable { "linked" } else { "unlinkable" };
            // The spill comes first, byte for byte.
            assert_eq!(
                &exit[..spill.len()],
                &spill[..],
                "{what} exit: the spill must come first"
            );
            let mut rest = spill.len();
            // Then the path's own middle (the reload, or the return word in rax) …
            let middle = if linkable {
                reload_bytes()
            } else {
                encoded(|buffer| buffer.mov_ri(Reg::Rax, encode_target(func, pc)).unwrap())
            };
            assert_eq!(&exit[rest..rest + middle.len()], &middle[..], "{what} exit");
            rest += middle.len();
            if linkable {
                // … and on the linked path the teardown follows the reload.
                let teardown = teardown_bytes(&allocation);
                assert_eq!(
                    &exit[rest..rest + teardown.len()],
                    &teardown[..],
                    "{what} exit: the frame teardown"
                );
                rest += teardown.len();
            }
            // And the exit is exactly the spill, the middle, the teardown and the jump.
            assert_eq!(exit.len(), rest + 5, "{what} exit length");
        }
    }

    #[test]
    fn a_tail_exit_spills_pops_the_frame_and_then_jumps_to_the_successor() {
        // The tail exit is the linked exit with the interpreter's *checked frame pop*
        // in front of the transfer: the spill (the root set) first, then the helper's
        // arguments — the already-resolved successor and the frame size — then the
        // compare against the constant, the frame teardown and the direct jump.
        let mut buffer = CodeBuffer::new().unwrap();
        let target = buffer.label();
        let done = buffer.label();
        let mut entries = BTreeMap::new();
        entries.insert((0u32, 7u32), target);
        let linker = Linker::new(&entries);
        assert!(linker.is_compiled(0, 7) && !linker.is_compiled(0, 8));
        let allocation = Allocation {
            slots: vec![(0u32, Reg::R12)],
            written: vec![0],
            y_last: None,
            ..Default::default()
        };

        linker
            .emit_tail_exit(&mut buffer, &allocation, done, 0, 7, 3, 0, ExitSpill::Required)
            .unwrap();
        let exit_len = buffer.len();
        // A filler byte between the exit and the successor's entry, so the jump's
        // displacement is non-trivial.
        buffer.ret().unwrap();
        let target_at = buffer.len();
        buffer.bind(target).unwrap();
        buffer.bind(done).unwrap();
        buffer.seal().unwrap();

        let exit = &emitted(&buffer)[..exit_len];
        // The spill comes first, byte for byte: the pop helper must not run before the
        // cached x-registers are back in `machine.x`.
        let spill = spill_bytes(&allocation);
        assert_eq!(&exit[..spill.len()], &spill[..], "the spill comes first");

        // The helper's two value arguments, in the order the block passes them: the
        // resolved successor word, then the frame size the pop must match.
        let word = encoded(|buffer| buffer.mov_ri(Reg::Rsi, encode_target(0, 7)).unwrap());
        let deallocate = encoded(|buffer| buffer.mov_ri(Reg::Rdx, 3).unwrap());
        let word_at = exit
            .windows(word.len())
            .position(|window| window == word.as_slice())
            .expect("the resolved successor is passed to the pop helper");
        let deallocate_at = exit
            .windows(deallocate.len())
            .position(|window| window == deallocate.as_slice())
            .expect("the frame size is passed to the pop helper");
        assert!(word_at < deallocate_at, "successor word, then frame size");

        // The transfer comes *after* the pop: the frame teardown (and the jump) follow
        // the helper's arguments.
        let teardown = teardown_bytes(&allocation);
        let teardown_at = exit
            .windows(teardown.len())
            .position(|window| window == teardown.as_slice())
            .expect("the tail exit must tear its frame down");
        assert!(
            deallocate_at < teardown_at,
            "the frame is popped before the frame is left"
        );

        // … and the jump lands exactly on the successor's compiled entry.
        let (site, rel) = trailing_jump(exit);
        assert_eq!(
            (site + 5) as i64 + i64::from(rel),
            target_at as i64,
            "the direct jump must land on the successor's entry"
        );

        // A tail exit never fabricates a return word in `rax`: `rax` carries the pop
        // helper's answer (the successor word, or `ERROR`), which is what the compare
        // against the compiled-in constant reads.
        assert!(
            !exit.windows(2).any(|w| w == [0x48, 0xB8]),
            "the tail exit must not materialise a word in rax"
        );
    }

    #[test]
    fn a_tail_exit_without_a_compiled_successor_is_a_typed_error() {
        // The caller resolves the target and checks `is_compiled` first; a link that
        // named a target with no landing address could only produce a wrong result, so
        // it must be a typed error rather than a panic.
        let mut buffer = CodeBuffer::new().unwrap();
        let done = buffer.label();
        let entries = BTreeMap::new();
        let linker = Linker::new(&entries);
        let allocation = Allocation::default();
        let err = linker
            .emit_tail_exit(&mut buffer, &allocation, done, 0, 7, 3, 0, ExitSpill::Required)
            .expect_err("an unlinkable tail target is the documented typed error");
        assert!(
            format!("{err:?}").contains("no compiled block"),
            "the error must name the missing block, got {err:?}"
        );
    }

    /// The exit the `call_last` arm uses for a **resolved but unlinked** target: the
    /// interpreter's own checked frame pop, then the resolved `(func, pc)` word left in
    /// `rax` for the **driver** (the interpreter's `Step::Goto`), *not* a direct jump
    /// into a successor's native entry.
    ///
    /// Four structural facts, each of which the exit would get wrong a different way:
    ///
    /// * **(a) the spill comes first** — the tier's root-set contract, which no exit may
    ///   skip ([`super::spill`]);
    /// * **(b) the helper gets the resolved successor word and then the frame size**, in
    ///   that order (the interpreter's `resolve`-then-`pop` order);
    /// * **(c) the exit does not leave the frame** — no [`emit_frame_teardown`], and the
    ///   trailing jump lands on the block's **shared epilogue** (the driver), never on a
    ///   successor entry (so a successor this compilation has no code for needs no
    ///   address at all);
    /// * **(d) `rax` still holds the helper's word after the compare** — the word is
    ///   never materialised into (nor clobbered out of) `rax`, so the epilogue's `ret`
    ///   hands the driver exactly the word to decode.
    #[test]
    fn an_unlinked_tail_exit_pops_the_frame_and_returns_the_word_to_the_driver() {
        let mut buffer = CodeBuffer::new().unwrap();
        let done = buffer.label();
        // The successor entry the exit must **not** jump to (it is a filler offset that
        // a `jmp` into the successor's compiled entry would land on instead).
        let successor = buffer.label();
        let entries = BTreeMap::new();
        let linker = Linker::new(&entries);
        let allocation = Allocation {
            slots: vec![(0u32, Reg::R12)],
            written: vec![0],
            y_last: None,
            ..Default::default()
        };

        linker
            .emit_tail_exit_to_driver(&mut buffer, &allocation, done, 0, 7, 3, 0, ExitSpill::Required)
            .unwrap();
        let exit_len = buffer.len();
        // A filler byte between the exit and the epilogue, so the trailing jump's
        // displacement is non-trivial.
        buffer.ret().unwrap();
        let successor_at = buffer.len();
        buffer.bind(successor).unwrap();
        // … and the epilogue one byte further on, so "lands on the epilogue" and "lands
        // on the successor entry" are distinguishable addresses.
        buffer.int3().unwrap();
        let done_at = buffer.len();
        buffer.bind(done).unwrap();
        buffer.seal().unwrap();

        let code = emitted(&buffer);
        let exit = &code[..exit_len];
        let word = encode_target(0, 7);

        // (a) the spill is byte-for-byte first.
        let spill = spill_bytes(&allocation);
        assert_eq!(&exit[..spill.len()], &spill[..], "the spill comes first");

        // (b) the helper's two value arguments, in the order the block passes them: the
        // resolved successor word, then the frame size the pop must match — and they are
        // handed to `jit_tail_call` (the interpreter's own checked pop).
        let mov_word = encoded(|buffer| buffer.mov_ri(Reg::Rsi, word).unwrap());
        let mov_deallocate = encoded(|buffer| buffer.mov_ri(Reg::Rdx, 3).unwrap());
        let word_at = exit
            .windows(mov_word.len())
            .position(|window| window == mov_word.as_slice())
            .expect("the resolved successor is passed to the pop helper");
        let deallocate_at = exit
            .windows(mov_deallocate.len())
            .position(|window| window == mov_deallocate.as_slice())
            .expect("the frame size is passed to the pop helper");
        assert!(word_at < deallocate_at, "successor word, then frame size");
        let helper_bytes = encoded(|buffer| {
            buffer
                .mov_ri(Reg::R11, jit_tail_call as *const () as i64)
                .unwrap();
            buffer.call_r(Reg::R11).unwrap();
        });
        let helper_at = exit
            .windows(helper_bytes.len())
            .position(|window| window == helper_bytes.as_slice())
            .expect("the frame is popped by `jit_tail_call`, the interpreter's own pop");
        assert!(
            deallocate_at < helper_at,
            "the frame size is in its argument register before the pop runs"
        );

        // (c) the exit does not leave this block's frame: a linked exit, by contrast,
        // tears the frame down and jumps straight into the successor's entry.
        let teardown = teardown_bytes(&allocation);
        assert!(
            !exit.windows(teardown.len()).any(|w| w == teardown.as_slice()),
            "an unlinked tail exit must leave the frame teardown to the shared epilogue"
        );
        let (site, rel) = trailing_jump(exit);
        let landing = (site + 5) as i64 + i64::from(rel);
        assert_eq!(
            landing, done_at as i64,
            "the exit must return the word to the driver's epilogue, not jump into a \
             successor's entry"
        );
        assert_ne!(
            landing, successor_at as i64,
            "the exit must not jump into the successor's compiled entry"
        );
        // The frame window is untouched: no release of its own `rt` slot either.
        let slot = frame_slot_len(native_frame_regs(&allocation).len());
        let lea_release = encoded(|buffer| buffer.lea(Reg::Rsp, Reg::Rsp, slot).unwrap());
        assert!(
            !exit.windows(lea_release.len()).any(|w| w == lea_release.as_slice()),
            "an unlinked tail exit must not release its own `rt` slot"
        );

        // (d) `rax` keeps the helper's answer: the exit materialises no word into `rax`
        // (no `movabs rax, imm64`) and, after the compare, only branches on it — so the
        // epilogue's `ret` hands the driver the word to decode.
        assert!(
            !exit.windows(2).any(|w| w == [0x48, 0xB8]),
            "the unlinked tail exit must not materialise a word in rax"
        );
        let compare_tail = encoded(|buffer| {
            buffer.mov_ri(Reg::R11, word).unwrap();
            buffer.cmp_rr(Reg::Rax, Reg::R11).unwrap();
        });
        let compare_at = exit
            .windows(compare_tail.len())
            .position(|window| window == compare_tail.as_slice())
            .expect("the exit compares the helper's answer against the compiled-in word");
        // Immediately after the compare come exactly two branches — the `jne <done>`
        // guard and the unconditional fall-through to that same epilogue — and nothing
        // else, so no instruction between the compare and the exit could clobber `rax`.
        let jcc_at = site - 6;
        assert_eq!(
            compare_at + compare_tail.len(),
            jcc_at,
            "after the compare (`cmp rax, r11`) the exit does exactly one `jcc` and one \
             `jmp` — nothing that could clobber `rax`"
        );
        let jcc = &exit[jcc_at..site];
        assert_eq!(jcc[0], 0x0F, "the compare is followed by a `jcc`");
        assert_eq!(jcc[1], 0x85, "… and it is `jne`");
        let guard_rel = i32::from_le_bytes(jcc[2..6].try_into().unwrap());
        assert_eq!(
            (site as i64) + i64::from(guard_rel),
            done_at as i64,
            "a failed pop leaves for the epilogue with the helper's `ERROR`"
        );
        assert_eq!(exit[site], 0xE9, "a successful pop falls through to `jmp done`");
    }
}
