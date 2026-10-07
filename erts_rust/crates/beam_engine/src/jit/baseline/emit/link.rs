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
//!   spill             ; the cached x-registers back into `machine.x` — MANDATORY:
//!                     ; this is the tier's whole liveness / root-set model, so a
//!                     ; linked exit must never skip it (see [`super::spill`])
//!   lea rsp, [rsp+24] ; drop the `rt` slot
//!   pop rbx ; pop rbp ; pop r12 … pop r15   ; leave the frame, restoring the
//!                                           ; caller's callee-saved registers
//!   jmp <successor entry>   ; the successor's own prologue rebuilds its frame; the
//!                           ; original return address is untouched, so *its* `ret`
//!                           ; returns to the driver
//! ```
//!
//! The return word is deliberately **not** computed: the successor is responsible
//! for the word the driver eventually sees, exactly as if the driver had decoded
//! the branch target and called the successor itself.
//!
//! # What is *not* linked
//!
//! `(func, pc)` is linked only when it is the start of a block this compilation
//! emitted — including blocks of **other functions** of the same module (`fib/1`'s
//! tail call into `fib/3`), because every block is a standalone frame. Every other
//! continuation keeps today's documented deopt: spill, materialise the
//! `encode_target` word in `rax`, and jump to the block epilogue so the driver
//! decodes it and takes one interpreter step (an address inside a block, an empty
//! block's start, the end of a function's code, a label the module does not define).
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

use crate::jit::emitter::{CodeBuffer, Label as CodeLabel, Reg};

use super::super::runtime::encode_target;
use super::{spill, Allocation, SAVED_REGISTERS};

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

    /// Emit the block exit that transfers control to `(func, pc)`.
    ///
    /// A **linked** successor (`(func, pc)` is a compiled block's entry) is reached
    /// by leaving this block's frame and jumping straight to that block's entry;
    /// anything else is the documented return-to-driver deopt (spill, the
    /// `encode_target` word in `rax`, `jmp done`).
    ///
    /// The cached x-registers are spilled on **both** paths, before the frame is
    /// torn down — that spill *is* the tier's liveness/root-set model, so an exit
    /// may never skip it.
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
                spill(buffer, allocation)?;
                emit_frame_teardown(buffer)?;
                buffer.jmp(target)
            }
            None => {
                spill(buffer, allocation)?;
                buffer.mov_ri(Reg::Rax, encode_target(func, pc))?;
                buffer.jmp(done)
            }
        }
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

    /// The last five bytes of `code` read as a `jmp rel32`, as
    /// `(site offset, displacement)`.
    fn trailing_jump(code: &[u8]) -> (usize, i32) {
        let site = code.len() - 5;
        assert_eq!(code[site], 0xE9, "the exit must end in a `jmp rel32`");
        let rel = i32::from_le_bytes(code[site + 1..site + 5].try_into().unwrap());
        (site, rel)
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
        let jmp_site = buffer.len() - 5;
        // A filler byte between the exit and the successor's entry, so the
        // displacement is non-trivial (a `jmp` to the very next byte would be 0).
        buffer.ret().unwrap();
        let target_at = buffer.len();
        buffer.bind(target).unwrap();
        buffer.bind(done).unwrap();
        buffer.seal().unwrap();

        let code = emitted(&buffer);
        let (site, rel) = trailing_jump(&code);
        assert_eq!(site, jmp_site);
        assert_eq!(
            rel,
            target_at as i32 - (jmp_site + 4) as i32,
            "the direct jump must land on the successor's entry"
        );
        // The frame is torn down before the jump (the `lea rsp, [rsp+24]` the
        // prologue's `lea rsp, [rsp-24]` is the inverse of).
        assert_eq!(&code[..2], &[0x48, 0x8D], "`lea rsp, [rsp+24]`");
        // And the return word is *not* materialised: no `movabs rax, imm64`.
        assert!(
            !code.windows(2).any(|w| w == [0x48, 0xB8]),
            "a linked exit never materialises a return word in rax"
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
        let end = buffer.len();
        buffer.bind(done).unwrap();
        buffer.seal().unwrap();

        let code = emitted(&buffer);
        let (site, rel) = trailing_jump(&code);
        assert_eq!(site, end - 5);
        assert_eq!(rel, 0, "the epilogue follows the exit immediately");
        // The word is materialised by a `movabs rax, encode_target(0, 7)`.
        assert_eq!(&code[end - 10..end - 8], &[0x48, 0xB8]);
        let word = i64::from_le_bytes(code[end - 8..end].try_into().unwrap());
        assert_eq!(word, encode_target(0, 7));
    }

    #[test]
    fn every_exit_spills_the_cached_registers_before_leaving_the_frame() {
        // The spill is the tier's liveness model: with one cached x-register (x0 in
        // r12) the exit must store it to `[rbx + 0]` *before* the frame teardown.
        let mut buffer = CodeBuffer::new().unwrap();
        let target = buffer.label();
        let done = buffer.label();
        let mut entries = BTreeMap::new();
        entries.insert((0u32, 0u32), target);
        let linker = Linker::new(&entries);
        let allocation = Allocation {
            slots: vec![(0u32, Reg::R12)],
        };

        linker
            .emit_exit(&mut buffer, &allocation, done, 0, 0)
            .unwrap();
        buffer.bind(target).unwrap();
        buffer.bind(done).unwrap();
        buffer.seal().unwrap();

        let code = emitted(&buffer);
        // `mov [rbx + 0], r12` — the spill store, first.
        assert_eq!(&code[..7], &[0x44, 0x89, 0xA3, 0x00, 0x00, 0x00, 0x00]);
        // `lea rsp, [rsp+24]` immediately after it.
        assert_eq!(&code[7..9], &[0x48, 0x8D]);
        assert_eq!(code[9], 0xA4, "`lea rsp, [rsp+disp32]` (SIB form)");
        // `pop rbx` is the first restore (the pushes run in `SAVED_REGISTERS` order,
        // the pops in reverse).
        assert_eq!(code[13], 24, "the `rt` slot displacement");
        assert!(code[17..].contains(&0x5B), "`pop rbx`");
        // And the exit is the spill, the teardown and the jump — nothing else.
        assert_eq!(code.len(), 7 + 8 + 10 + 5);
    }
}
