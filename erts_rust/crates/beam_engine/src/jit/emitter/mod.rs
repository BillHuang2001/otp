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

//! x86-64 machine-code emitter and executable code buffer for the baseline JIT.
//!
//! This module is the low-level half of the `BaselineJit` tier (Part II §F): it
//! turns calls like [`CodeBuffer::mov_rr`] / [`CodeBuffer::jcc`] into real x86-64
//! instruction encodings, accumulates them in an executable mapping, resolves
//! forward/backward [`Label`] references, and hands back a callable function
//! pointer. It is a **standalone building block**: it knows nothing about the
//! typed IR, the [`Machine`](crate::machine::Machine) or the tier seam — a later
//! task lowers `beam_loader` IR onto this encoder. Nothing here is wired into the
//! interpreter today.
//!
//! # W^X policy
//!
//! The buffer is created with `mmap(PROT_READ | PROT_WRITE)` and is flipped to
//! `PROT_READ | PROT_EXEC` by [`CodeBuffer::seal`] with `mprotect`. The mapping is
//! **never** writable and executable at the same time: while it is writable no
//! instruction may be executed from it, and once it is executable it can no longer
//! be patched (every emitting call after [`seal`](CodeBuffer::seal) is a typed
//! error rather than a fault). This is the choice the design docs favour (Part II
//! §F) and it means the emitter never has to build an RWX region.
//!
//! # Calling convention
//!
//! The emitter assumes the **System V AMD64** ABI, the convention Erlang/OTP and
//! this workspace target on `x86_64-unknown-linux-gnu`: integer/pointer arguments
//! in `rdi`, `rsi`, `rdx`, `rcx`, `r8`, `r9`, further arguments on the stack, the
//! return value in `rax`, `rbx`/`rbp`/`r12`–`r15` callee-saved and the rest
//! caller-saved. The emitter does not itself implement a prologue/epilogue or
//! save callee-saved registers — that is the codegen layer's job; the unit tests
//! below show the discipline (`push`/`pop` around a clobbered `r12`).
//!
//! # Supported instruction subset
//!
//! Exactly the subset the baseline JIT needs to (a) call a native block,
//! (b) shuttle terms between the machine's x-register array and machine registers
//! and (c) do native control flow. Every encoding is 64-bit-operand
//! (`REX.W`) unless noted:
//!
//! * data movement — [`mov_rr`], [`mov_ri`] (`movabs`, full 64-bit immediate),
//!   [`mov_rm`] (load `[base + disp32]`), [`mov_mr`] (store), [`lea`];
//! * integer arithmetic/logic — [`add_rr`], [`sub_rr`], [`cmp_rr`], [`test_rr`],
//!   [`and_rr`], [`or_rr`], [`xor_rr`];
//! * stack — [`push_r`], [`pop_r`];
//! * control flow — [`jmp`] (`rel32`), [`jcc`] (`rel32`), [`call_label`]
//!   (`rel32`), [`call_r`] (indirect), [`ret`];
//! * filler — [`nop`], [`int3`].
//!
//! Everything else (immediates in `mov r/m64, imm32`, memory-operand arithmetic,
//! indexed addressing, the SSE/AVX space, `syscall`, …) is deliberately **absent**
//! — an unencodable instruction must be added as a new, tested method rather than
//! guessed at. The encoder does handle the encoding details a wider subset would
//! trip over: the `REX.W` prefix plus the `REX.R`/`REX.B` extension bits for
//! `r8`–`r15`, the `SIB` byte that an `rsp`/`r12` base (or would-be index) forces,
//! and a uniform `mod=10` `disp32` that sidesteps the `rbp`/`r13` “`mod=00` means
//! RIP-relative” special case.
//!
//! # Unsafe zone
//!
//! This is the crate's designated **JIT unsafe zone**: it owns the only
//! executable-memory mapping and the only raw writes into it. The crate root keeps
//! `#![deny(unsafe_code)]`; the module-local `#![allow(unsafe_code)]` below is the
//! single new exception (alongside the pre-existing `predicates` and `terms`
//! modules). Every `unsafe` block carries a `// SAFETY:` rationale.
#![allow(unsafe_code)]

use core::mem;
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};

use beam_common::{BeamError, Result};

// The x86-64 instruction encoders (`impl CodeBuffer`) live in a sibling file;
// the buffer lifecycle, label/fixup machinery and raw byte-emission
// primitives stay in this file. See the module docs above.
mod encode;

#[cfg(test)]
mod tests;

/// Source of the per-buffer identity that keeps [`Label`]s buffer-local.
///
/// Never zero (zero is reserved for a hypothetical "no buffer" label).
static NEXT_BUFFER_NONCE: AtomicU32 = AtomicU32::new(1);
/// Default capacity, in bytes, of a new [`CodeBuffer`] (64 KiB).
///
/// Large enough for a substantial block of generated code; the mapping is rounded
/// up to a whole number of OS pages regardless.
pub const DEFAULT_CAPACITY: usize = 64 * 1024;

/// Page size assumed when `sysconf(_SC_PAGESIZE)` cannot tell us (4 KiB).
const FALLBACK_PAGE_SIZE: usize = 4096;

/// The machine's page size, or [`FALLBACK_PAGE_SIZE`] if it cannot be queried.
fn page_size() -> usize {
    // SAFETY: `sysconf` with `_SC_PAGESIZE` has no preconditions; it returns the
    // page size or -1.
    let value = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if value > 0 {
        value as usize
    } else {
        FALLBACK_PAGE_SIZE
    }
}

/// Rounds `n` up to a non-zero whole number of pages.
fn round_up_to_page(n: usize) -> usize {
    let page = page_size();
    n.max(1).div_ceil(page) * page
}

/// One of the 16 x86-64 general-purpose registers.
///
/// The discriminant is the register's architectural encoding (0–15); the low three
/// bits live in the `ModRM`/`SIB` bytes and the high bit becomes a `REX`
/// extension bit. `rsp`/`r12` are numbered 4/12 and therefore carry the “`rm`/base
/// field is `100` ⇒ a `SIB` byte follows” rule the encoder implements.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[repr(u8)]
pub enum Reg {
    /// `rax` — accumulator, first integer return register.
    Rax = 0,
    /// `rcx` — fourth integer argument, the `count` register for shifts.
    Rcx = 1,
    /// `rdx` — third integer argument.
    Rdx = 2,
    /// `rbx` — callee-saved general register.
    Rbx = 3,
    /// `rsp` — stack pointer (never usable as an index; forces a `SIB` as a base).
    Rsp = 4,
    /// `rbp` — callee-saved frame pointer.
    Rbp = 5,
    /// `rsi` — second integer argument.
    Rsi = 6,
    /// `rdi` — first integer argument.
    Rdi = 7,
    /// `r8` — fifth integer argument.
    R8 = 8,
    /// `r9` — sixth integer argument.
    R9 = 9,
    /// `r10` — caller-saved scratch.
    R10 = 10,
    /// `r11` — caller-saved scratch.
    R11 = 11,
    /// `r12` — callee-saved (forces a `SIB` as a base).
    R12 = 12,
    /// `r13` — callee-saved (a base needs an explicit displacement).
    R13 = 13,
    /// `r14` — callee-saved.
    R14 = 14,
    /// `r15` — callee-saved.
    R15 = 15,
}

impl Reg {
    /// Every general-purpose register, in architectural encoding order.
    pub const ALL: [Reg; 16] = [
        Reg::Rax,
        Reg::Rcx,
        Reg::Rdx,
        Reg::Rbx,
        Reg::Rsp,
        Reg::Rbp,
        Reg::Rsi,
        Reg::Rdi,
        Reg::R8,
        Reg::R9,
        Reg::R10,
        Reg::R11,
        Reg::R12,
        Reg::R13,
        Reg::R14,
        Reg::R15,
    ];

    /// The register's architectural encoding (0–15).
    pub fn code(self) -> u8 {
        self as u8
    }
}

/// A conditional-jump condition code (`Jcc`).
///
/// The discriminant is the x86-64 `tttn` condition encoding; `jcc` adds it to the
/// `0x80` second opcode byte. The variants are named after the **signed** and
/// **unsigned** interpretations the BEAM comparison ops want: `L`/`Ge`/`Le`/`G`
/// for signed, `B`/`Ae`/`Be`/`A` for unsigned, plus the equality/parity/sign set.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[repr(u8)]
pub enum Cond {
    /// Overflow (`jo`).
    O = 0x0,
    /// No overflow (`jno`).
    No = 0x1,
    /// Below / carry (unsigned `<`, `jb`).
    B = 0x2,
    /// Above or equal / no carry (unsigned `>=`, `jae`).
    Ae = 0x3,
    /// Equal / zero (`je`).
    E = 0x4,
    /// Not equal / not zero (`jne`).
    Ne = 0x5,
    /// Below or equal (unsigned `<=`, `jbe`).
    Be = 0x6,
    /// Above (unsigned `>`, `ja`).
    A = 0x7,
    /// Sign (`js`).
    S = 0x8,
    /// No sign (`jns`).
    Ns = 0x9,
    /// Parity even (`jp`).
    P = 0xA,
    /// Parity odd (`jnp`).
    Np = 0xB,
    /// Less (signed `<`, `jl`).
    L = 0xC,
    /// Greater or equal (signed `>=`, `jge`).
    Ge = 0xD,
    /// Less or equal (signed `<=`, `jle`).
    Le = 0xE,
    /// Greater (signed `>`, `jg`).
    G = 0xF,
}

impl Cond {
    /// The condition's `tttn` encoding (0–15).
    pub fn code(self) -> u8 {
        self as u8
    }
}

/// A code label inside one [`CodeBuffer`].
///
/// Labels are created by [`CodeBuffer::label`] and fixed to a position by
/// [`CodeBuffer::bind`]. A label carries the identity of the buffer that created
/// it, so passing one buffer's label to another is rejected with a typed error
/// rather than silently patching an unrelated instruction.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct Label {
    /// Index into the owning buffer's binding table.
    id: u32,
    /// Identity of the owning buffer.
    nonce: u32,
}
/// A pending `rel32` reference to a not-yet-bound label.
///
/// `at` is the offset of the 4-byte displacement field inside the code buffer; the
/// displacement is filled in when the label is bound (or immediately, when the
/// label was already bound — a backward reference).
#[derive(Clone, Copy, Debug)]
struct Fixup {
    label: Label,
    at: usize,
}

/// An executable memory region that machine code is emitted into.
///
/// See the module documentation for the W^X policy and the calling convention.
/// A buffer starts writable and non-executable; [`seal`](CodeBuffer::seal) flips
/// it to readable+executable, after which [`as_fn_ptr`](CodeBuffer::as_fn_ptr)
/// yields a callable pointer. The mapping is released with `munmap` in
/// [`Drop`].
///
/// Every fallible operation returns [`beam_common::Result`]: a failed `mmap`/
/// `mprotect` is a [`BeamError::Io`], a buffer whose fixed capacity is exhausted is
/// a [`BeamError::SystemLimit`], and misuse (an unbound label at seal time, a
/// foreign or doubly-bound label, emitting after sealing) is a
/// [`BeamError::BadArg`]. Nothing here panics.
pub struct CodeBuffer {
    /// Start of the `mmap`ped region (page-aligned; never null while alive).
    base: *mut u8,
    /// Length of the mapping in bytes (a whole number of pages).
    size: usize,
    /// Number of bytes emitted so far.
    len: usize,
    /// Whether the region has been flipped to read+execute.
    sealed: bool,
    /// Identity handed to every [`Label`] this buffer creates.
    nonce: u32,
    /// Per-label bound offset, indexed by [`Label`].
    labels: Vec<Option<usize>>,
    /// Forward references awaiting their label's `bind`.
    fixups: Vec<Fixup>,
}

impl CodeBuffer {
    /// Creates a buffer with [`DEFAULT_CAPACITY`] bytes of code space.
    pub fn new() -> Result<Self> {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Creates a buffer with at least `capacity` bytes of code space.
    ///
    /// The mapping is rounded up to a whole number of OS pages (and is always at
    /// least one page).
    pub fn with_capacity(capacity: usize) -> Result<Self> {
        let size = round_up_to_page(capacity);
        // SAFETY: an anonymous, private `mmap` has no preconditions beyond a
        // non-zero length and valid flags; it either returns a fresh,
        // exclusively-owned, page-aligned mapping or `MAP_FAILED`, handled below.
        let mapped = unsafe {
            libc::mmap(
                ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if mapped == libc::MAP_FAILED {
            return Err(BeamError::Io(std::io::Error::last_os_error()));
        }
        Ok(CodeBuffer {
            base: mapped.cast::<u8>(),
            size,
            len: 0,
            sealed: false,
            nonce: NEXT_BUFFER_NONCE.fetch_add(1, Ordering::Relaxed),
            labels: Vec::new(),
            fixups: Vec::new(),
        })
    }

    /// The number of bytes emitted so far.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing has been emitted yet.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The mapping's capacity in bytes (a whole number of pages).
    pub fn capacity(&self) -> usize {
        self.size
    }

    /// Whether the buffer has been flipped to read+execute.
    pub fn is_sealed(&self) -> bool {
        self.sealed
    }

    /// The address of the first emitted byte.
    ///
    /// Only meaningful in a process that has no [`CodeBuffer`] alive at the same
    /// address; the pointer is valid until the buffer is dropped.
    pub fn code_ptr(&self) -> *const u8 {
        self.base
    }

    /// Reinterprets the start of the buffer as a callable function pointer of type
    /// `F` (e.g. `extern "C" fn(i64, i64) -> i64`).
    ///
    /// The caller must have called [`seal`](CodeBuffer::seal) first — calling into
    /// a still-writable (non-executable) mapping would fault — and must only call
    /// `F` with arguments matching the ABI the emitted code expects (see the
    /// module docs). This is a *safe* function only in the sense that the
    /// transmute itself is; the soundness of the emitted code is the codegen
    /// layer's contract. `F` must be a function-pointer type (a debug assertion
    /// checks its width).
    pub fn as_fn_ptr<F>(&self) -> F {
        debug_assert_eq!(
            mem::size_of::<F>(),
            mem::size_of::<*const u8>(),
            "as_fn_ptr requires a function-pointer type"
        );
        let code: *const u8 = self.base;
        // SAFETY: `F` is a function-pointer type (asserted to be pointer-width
        // above) and `code` is the base of the executable mapping, so both are the
        // same size and the transmute reinterprets one pointer as the other.
        unsafe { mem::transmute_copy(&code) }
    }

    /// Creates a fresh, unbound label in this buffer.
    pub fn label(&mut self) -> Label {
        let id = self.labels.len() as u32;
        self.labels.push(None);
        Label {
            id,
            nonce: self.nonce,
        }
    }
    /// Binds `label` to the current end of the code stream, patching every pending
    /// reference to it.
    ///
    /// Binding an already-bound label, or a label that does not belong to this
    /// buffer, is a [`BeamError::BadArg`]; so is binding after
    /// [`seal`](CodeBuffer::seal) (the mapping is no longer writable).
    pub fn bind(&mut self, label: Label) -> Result<()> {
        if self.sealed {
            return Err(sealed_error());
        }
        let slot = self.label_slot(label)?;
        if slot.is_some() {
            return Err(BeamError::BadArg);
        }
        let target = self.len;
        let index = label.id as usize;
        self.labels[index] = Some(target);
        // Patch the forward references this `bind` resolves; keep the rest.
        let pending = mem::take(&mut self.fixups);
        for fixup in pending {
            if fixup.label == label {
                let disp = rel32(target, fixup.at)?;
                self.patch_i32(fixup.at, disp)?;
            } else {
                self.fixups.push(fixup);
            }
        }
        Ok(())
    }

    /// Flips the mapping from read+write to read+execute (`mprotect`), making the
    /// emitted code callable.
    ///
    /// Fails with [`BeamError::BadArg`] if any label is still unbound (the code is
    /// incomplete) and with [`BeamError::Io`] if `mprotect` fails. Sealing an
    /// already-sealed buffer is a successful no-op.
    pub fn seal(&mut self) -> Result<()> {
        if self.sealed {
            return Ok(());
        }
        if self.labels.iter().any(Option::is_none) {
            return Err(unbound_label_error());
        }
        // SAFETY: `base`/`size` are exactly the region returned by
        // `with_capacity`'s `mmap`, `base` is page-aligned (mmap guarantee) and
        // `size` is a whole number of pages, so the whole mapping may be
        // re-protected as read+execute.
        let rc = unsafe {
            libc::mprotect(
                self.base.cast(),
                self.size,
                libc::PROT_READ | libc::PROT_EXEC,
            )
        };
        if rc != 0 {
            return Err(BeamError::Io(std::io::Error::last_os_error()));
        }
        self.sealed = true;
        Ok(())
    }

    // -- encoding primitives -------------------------------------------------

    /// Reserves `n` more bytes, or reports why it cannot.
    fn ensure(&self, n: usize) -> Result<()> {
        if self.sealed {
            return Err(sealed_error());
        }
        if self.len + n > self.size {
            return Err(BeamError::SystemLimit);
        }
        Ok(())
    }

    /// Appends one byte.
    fn emit_u8(&mut self, byte: u8) -> Result<()> {
        self.emit_bytes(&[byte])
    }

    /// Appends `n` little-endian bytes.
    fn emit_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.ensure(bytes.len())?;
        // SAFETY: `ensure(n)` proved `len + n <= size` and that the mapping is
        // writable, so the destination range is in-bounds; `bytes` is a distinct
        // slice (a stack array or a caller-owned buffer), so the two ranges cannot
        // overlap.
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), self.base.add(self.len), bytes.len());
        }
        self.len += bytes.len();
        Ok(())
    }

    /// Appends a little-endian `i32`.
    fn emit_i32(&mut self, value: i32) -> Result<()> {
        self.emit_bytes(&value.to_le_bytes())
    }

    /// Appends a little-endian `i64`.
    fn emit_i64(&mut self, value: i64) -> Result<()> {
        self.emit_bytes(&value.to_le_bytes())
    }

    /// Overwrites the four bytes at `at` (a deferred `rel32` fixup).
    fn patch_i32(&mut self, at: usize, value: i32) -> Result<()> {
        if self.sealed || at + 4 > self.len {
            return Err(BeamError::BadArg);
        }
        // SAFETY: `at + 4 <= len <= size` and the mapping is writable, so the
        // destination is in-bounds; `write_unaligned` imposes no alignment
        // requirement on a displacement field.
        unsafe { ptr::write_unaligned(self.base.add(at).cast::<i32>(), value) };
        Ok(())
    }

    /// Emits a `REX` prefix when one is needed (`REX.W` and/or an extension bit).
    fn emit_rex(&mut self, w: bool, r: u8, x: u8, b: u8) -> Result<()> {
        let rex = 0x40 | ((w as u8) << 3) | ((r & 1) << 2) | ((x & 1) << 1) | (b & 1);
        if rex != 0x40 {
            self.emit_u8(rex)?;
        }
        Ok(())
    }

    /// Emits `ModRM` (`mod=10`) plus a `SIB` byte when the base is `rsp`/`r12`.
    ///
    /// The displacement is always a `disp32`, which keeps the encoding uniform and
    /// avoids the `mod=00`/`rbp`/`r13` RIP-relative special case.
    fn emit_mem_modrm(&mut self, reg: Reg, base: Reg, disp: i32) -> Result<()> {
        let base_low = base.code() & 0x7;
        self.emit_u8(0x80 | ((reg.code() & 0x7) << 3) | base_low)?;
        if base_low == 0b100 {
            // `rm == 100` selects the SIB byte; `index == 100` means "no index",
            // so only the base (and scale 0) is meaningful.
            self.emit_u8(0x20 | base_low)?;
        }
        self.emit_i32(disp)
    }

    /// Emits a `rel32` field for `label`, patching it at once for a backward
    /// reference and recording a [`Fixup`] for a forward one.
    fn emit_rel32(&mut self, label: Label) -> Result<()> {
        let at = self.len;
        self.emit_i32(0)?;
        match self.label_slot(label)? {
            Some(target) => {
                let disp = rel32(target, at)?;
                self.patch_i32(at, disp)
            }
            None => {
                self.fixups.push(Fixup { label, at });
                Ok(())
            }
        }
    }

    /// Looks up `label`'s binding, rejecting a label from another buffer.
    fn label_slot(&self, label: Label) -> Result<Option<usize>> {
        if label.nonce != self.nonce {
            return Err(BeamError::BadArg);
        }
        self.labels
            .get(label.id as usize)
            .copied()
            .ok_or(BeamError::BadArg)
    }
}

impl Drop for CodeBuffer {
    fn drop(&mut self) {
        if self.base.is_null() {
            return;
        }
        // SAFETY: `base`/`size` are exactly the pair `with_capacity` mapped and
        // this is the only place the mapping is unmapped (a `CodeBuffer` owns it
        // exclusively and is consumed here), so `munmap` runs exactly once.
        let rc = unsafe { libc::munmap(self.base.cast(), self.size) };
        debug_assert_eq!(rc, 0, "munmap must succeed for a live code mapping");
    }
}

/// The `REX` extension bit (high bit) of a register.
fn ext(reg: Reg) -> u8 {
    (reg.code() >> 3) & 1
}

/// A `ModRM` byte for the register-to-register form (`mod=11`).
fn modrm_rr(reg: Reg, rm: Reg) -> u8 {
    0xC0 | ((reg.code() & 0x7) << 3) | (rm.code() & 0x7)
}

/// The `rel32` displacement from a `rel32` field at `at` to `target`.
///
/// A `rel32` is measured from the end of the instruction, i.e. from `at + 4`.
fn rel32(target: usize, at: usize) -> Result<i32> {
    i32::try_from(target as i64 - (at as i64 + 4)).map_err(|_| BeamError::SystemLimit)
}

/// Emitting into a buffer that has already been sealed (and is no longer
/// writable) is an API misuse.
fn sealed_error() -> BeamError {
    BeamError::BadArg
}

/// Sealing a buffer that still has an unbound label would yield incomplete code.
fn unbound_label_error() -> BeamError {
    BeamError::BadArg
}
