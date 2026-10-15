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
//!   spill             ; the block's WRITTEN cached x-registers back into
//!                     ; `machine.x` — MANDATORY: this is the tier's whole
//!                     ; liveness / root-set model, so a linked exit must never
//!                     ; skip it (see [`super::spill`])
//!   mov rdi, [rsp+RUNTIME_SLOT_DISP]  ; `rt` for the successor's prologue
//!   lea rsp, [rsp+24] ; drop the `rt` slot
//!   pop rbx ; pop rbp ; pop r12 … pop r15   ; leave the frame, restoring the
//!                                           ; caller's callee-saved registers
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
//!   lea rsp, [rsp+24] ; pop rbx … pop r15   ; leave the frame
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

use super::super::runtime::{encode_target, jit_tail_call, TailHelper};
use super::{spill, Allocation, SpillSite, RUNTIME_SLOT_DISP, SAVED_REGISTERS};

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
    /// may never skip it. The linked path additionally reloads this block's `rt`
    /// into `rdi` (the block ABI's first argument), because a successor entered by a
    /// `jmp` gets no `call` to set it.
    pub(super) fn emit_exit(
        &self,
        buffer: &mut CodeBuffer,
        allocation: &Allocation,
        done: CodeLabel,
        func: u32,
        pc: u32,
    ) -> Result<()> {
        match self.entries.get(&(func, pc)) {
            Some(&target) => {
                spill(buffer, allocation, SpillSite::Exit)?;
                // `rt` for the successor's prologue: it stores `rdi` into *its* `rt`
                // slot and loads `rbx` from it. `rdi` is not one of the registers the
                // teardown pops, so the reloaded value survives the teardown and the
                // jump.
                buffer.mov_rm(Reg::Rdi, Reg::Rsp, RUNTIME_SLOT_DISP)?;
                emit_frame_teardown(buffer)?;
                buffer.jmp(target)
            }
            None => {
                spill(buffer, allocation, SpillSite::Exit)?;
                buffer.mov_ri(Reg::Rax, encode_target(func, pc))?;
                buffer.jmp(done)
            }
        }
    }

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
    /// successor's prologue reloads `machine.x`.
    pub(super) fn emit_tail_exit(
        &self,
        buffer: &mut CodeBuffer,
        allocation: &Allocation,
        done: CodeLabel,
        func: u32,
        pc: u32,
        deallocate: u32,
    ) -> Result<()> {
        let &target = self.entries.get(&(func, pc)).ok_or_else(|| {
            crate::error::unsupported("jit: tail-call target has no compiled block")
        })?;
        let word = encode_target(func, pc);

        // The root set first: the successor's prologue loads `machine.x`, and a failed
        // pop returns to a driver that owns the same root set. `jit_tail_call` reads no
        // x-register, so the spill order relative to the call is free — this one keeps
        // the exit shaped like every other exit in the tier.
        spill(buffer, allocation, SpillSite::Exit)?;
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
        emit_frame_teardown(buffer)?;
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
    pub(super) fn emit_tail_exit_to_driver(
        &self,
        buffer: &mut CodeBuffer,
        allocation: &Allocation,
        done: CodeLabel,
        func: u32,
        pc: u32,
        deallocate: u32,
    ) -> Result<()> {
        let word = encode_target(func, pc);

        // The root set first (the exit's contract, see [`super::spill`]), then `rt` and
        // the helper's two value arguments: the resolved successor word and the frame
        // size the pop must match.
        spill(buffer, allocation, SpillSite::Exit)?;
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

/// Drop the block's stack frame: release the `rt` slot and restore every
/// callee-saved register the prologue pushed, leaving `rsp` on the **original
/// return address** (so a following `ret` — or a linked jump into a successor whose
/// own `ret` does it — returns to the driver).
///
/// The exact inverse of [`super::emit_block`]'s prologue, defined once so the two
/// exits that need it (the shared `done` epilogue and a linked jump) can never
/// drift apart.
pub(super) fn emit_frame_teardown(buffer: &mut CodeBuffer) -> Result<()> {
    buffer.lea(Reg::Rsp, Reg::Rsp, 24)?;
    for reg in SAVED_REGISTERS.iter().rev() {
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
        encoded(|buffer| spill(buffer, allocation, SpillSite::Exit).unwrap())
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

    /// `emit_frame_teardown`, as emitted.
    fn teardown_bytes() -> Vec<u8> {
        encoded(|buffer| emit_frame_teardown(buffer).unwrap())
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
            .emit_exit(&mut buffer, &allocation, done, 0, 7)
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
        expected.extend(teardown_bytes());
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
            .emit_exit(&mut buffer, &allocation, done, 0, 7)
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
        let teardown = teardown_bytes();
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
            .emit_exit(&mut buffer, &allocation, done, 0, 7)
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
                .emit_exit(&mut buffer, &allocation, done, func, pc)
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
                let teardown = teardown_bytes();
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
            .emit_tail_exit(&mut buffer, &allocation, done, 0, 7, 3)
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
        let teardown = teardown_bytes();
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
            .emit_tail_exit(&mut buffer, &allocation, done, 0, 7, 3)
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
            .emit_tail_exit_to_driver(&mut buffer, &allocation, done, 0, 7, 3)
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
        let teardown = teardown_bytes();
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
        // The frame window is untouched: no `lea rsp, [rsp+24]` either.
        let lea_release = encoded(|buffer| buffer.lea(Reg::Rsp, Reg::Rsp, 24).unwrap());
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
