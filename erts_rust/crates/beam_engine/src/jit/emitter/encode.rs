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

//! x86-64 instruction encoders for [`CodeBuffer`](super::CodeBuffer).
//!
//! The `impl CodeBuffer` block here is the instruction-encoding surface the
//! baseline JIT composes blocks from: data movement, integer arithmetic and
//! logic, shifts, stack and control flow, plus the filler bytes. Every method
//! is a thin wrapper over the raw byte-emission primitives owned by [`super`]
//! (the `emit_*`/`patch_*` helpers); this file holds no `unsafe` code. See the
//! parent module docs for the supported instruction subset and the calling
//! convention.

use beam_common::{BeamError, Result};

use super::{ext, modrm_rr, CodeBuffer, Cond, Label, Reg};

impl CodeBuffer {
    // -- data movement -------------------------------------------------------

    /// `mov dst, src` (64-bit register to register).
    pub fn mov_rr(&mut self, dst: Reg, src: Reg) -> Result<()> {
        self.emit_rex(true, ext(src), 0, ext(dst))?;
        self.emit_u8(0x89)?;
        self.emit_u8(modrm_rr(src, dst))
    }

    /// `mov dst, imm` — the `movabs` form, carrying a full 64-bit immediate.
    pub fn mov_ri(&mut self, dst: Reg, imm: i64) -> Result<()> {
        self.emit_rex(true, 0, 0, ext(dst))?;
        self.emit_u8(0xB8 + (dst.code() & 0x7))?;
        self.emit_i64(imm)
    }

    /// `mov dst, [base + disp]` — a 64-bit load.
    pub fn mov_rm(&mut self, dst: Reg, base: Reg, disp: i32) -> Result<()> {
        self.emit_mov_rm(true, dst, base, disp)
    }

    /// `mov dst32, [base + disp]` — a **32-bit** load that clears the destination's
    /// upper 32 bits.
    ///
    /// Same `8B /r` opcode as [`Self::mov_rm`] with the `REX.W` bit **clear**: x86-64
    /// defines every 32-bit register write to zero-extend into the 64-bit register,
    /// so `dst` reads as `(loaded u32) as u64` — never a sign-extended or stale high
    /// half. A `REX` prefix is still emitted when a register at or above `r8` is
    /// involved (`REX.R`/`REX.B`), just without `W`.
    pub fn mov_rm32(&mut self, dst: Reg, base: Reg, disp: i32) -> Result<()> {
        self.emit_mov_rm(false, dst, base, disp)
    }

    /// `movzx dst, byte [base + disp]` — an 8-bit load zero-extended into the 64-bit
    /// destination (`REX.W 0F B6 /r`).
    ///
    /// The point of the instruction is the **zero** extension: the byte is read
    /// unsigned, so a source byte of `0xFF` yields `0xFF`, not `-1` (a sign-extending
    /// `movsx` would produce `0xFFFF_FFFF_FFFF_FFFF`). The upper 56 bits of `dst` are
    /// 0 afterwards, and only the addressed byte is read — the neighbouring bytes are
    /// not touched (`REX.W` selects the 64-bit destination width; the `0F B6` opcode
    /// fixes the 8-bit source width).
    pub fn movzx_byte(&mut self, dst: Reg, base: Reg, disp: i32) -> Result<()> {
        self.emit_movzx(0xB6, dst, base, disp)
    }

    /// `movzx dst, word [base + disp]` — a 16-bit load zero-extended into the 64-bit
    /// destination (`REX.W 0F B7 /r`).
    ///
    /// As with [`Self::movzx_byte`] the extension is unsigned: a source word of
    /// `0xFFFF` yields `0xFFFF`, not `-1`. The upper 48 bits of `dst` are 0
    /// afterwards, and only the two addressed bytes are read.
    pub fn movzx_word(&mut self, dst: Reg, base: Reg, disp: i32) -> Result<()> {
        self.emit_movzx(0xB7, dst, base, disp)
    }

    /// Emits the register-from-memory `mov` (`8B /r`) in the width `w` selects:
    /// `REX.W` for the 64-bit form, a bare `REX` (or none) for the 32-bit one.
    fn emit_mov_rm(&mut self, w: bool, dst: Reg, base: Reg, disp: i32) -> Result<()> {
        self.emit_rex(w, ext(dst), 0, ext(base))?;
        self.emit_u8(0x8B)?;
        self.emit_mem_modrm(dst, base, disp)
    }

    /// Emits the `movzx` family: `REX.W` + the `0F` opcode `op`
    /// (`B6` = byte, `B7` = word) + the `ModRM`/`disp32` memory operand.
    fn emit_movzx(&mut self, op: u8, dst: Reg, base: Reg, disp: i32) -> Result<()> {
        self.emit_rex(true, ext(dst), 0, ext(base))?;
        self.emit_u8(0x0F)?;
        self.emit_u8(op)?;
        self.emit_mem_modrm(dst, base, disp)
    }

    /// `mov [base + disp], src` — a 64-bit store.
    pub fn mov_mr(&mut self, base: Reg, disp: i32, src: Reg) -> Result<()> {
        self.emit_rex(true, ext(src), 0, ext(base))?;
        self.emit_u8(0x89)?;
        self.emit_mem_modrm(src, base, disp)
    }

    /// `lea dst, [base + disp]` — the address of `base + disp`.
    pub fn lea(&mut self, dst: Reg, base: Reg, disp: i32) -> Result<()> {
        self.emit_rex(true, ext(dst), 0, ext(base))?;
        self.emit_u8(0x8D)?;
        self.emit_mem_modrm(dst, base, disp)
    }

    // -- integer arithmetic and logic ---------------------------------------

    /// Emits a `ModRM`-register-form two-operand opcode (`dst` is `r/m`, `src` is
    /// `reg`, `REX.W`).
    fn emit_rr(&mut self, opcode: u8, dst: Reg, src: Reg) -> Result<()> {
        self.emit_rex(true, ext(src), 0, ext(dst))?;
        self.emit_u8(opcode)?;
        self.emit_u8(modrm_rr(src, dst))
    }

    /// `add dst, src`.
    pub fn add_rr(&mut self, dst: Reg, src: Reg) -> Result<()> {
        self.emit_rr(0x01, dst, src)
    }

    /// `sub dst, src`.
    pub fn sub_rr(&mut self, dst: Reg, src: Reg) -> Result<()> {
        self.emit_rr(0x29, dst, src)
    }

    /// `cmp a, b` — sets the flags for `a - b` (no operand is written).
    pub fn cmp_rr(&mut self, a: Reg, b: Reg) -> Result<()> {
        self.emit_rr(0x39, a, b)
    }

    /// `test a, b` — sets the flags for `a & b` (no operand is written).
    pub fn test_rr(&mut self, a: Reg, b: Reg) -> Result<()> {
        self.emit_rr(0x85, a, b)
    }

    /// `and dst, src`.
    pub fn and_rr(&mut self, dst: Reg, src: Reg) -> Result<()> {
        self.emit_rr(0x21, dst, src)
    }

    /// `or dst, src`.
    pub fn or_rr(&mut self, dst: Reg, src: Reg) -> Result<()> {
        self.emit_rr(0x09, dst, src)
    }

    /// `xor dst, src`.
    pub fn xor_rr(&mut self, dst: Reg, src: Reg) -> Result<()> {
        self.emit_rr(0x31, dst, src)
    }

    /// `imul dst, src` — the two-operand signed multiply (`dst = dst * src`).
    ///
    /// Sets `OF`/`CF` exactly when the low 64 bits are not the sign-extended exact
    /// product, i.e. when the multiplication overflowed — the flag the arithmetic
    /// fast path's `jo` guard reads.
    ///
    /// Unlike `add`/`sub`, the two-operand `imul`'s **destination** is the `ModRM`
    /// `reg` field and its source the `r/m` field (`0F AF /r`, `IMUL r64, r/m64`),
    /// so the operand order here is the reverse of [`Self::emit_rr`]'s.
    pub fn imul_rr(&mut self, dst: Reg, src: Reg) -> Result<()> {
        self.emit_rex(true, ext(dst), 0, ext(src))?;
        self.emit_u8(0x0F)?;
        self.emit_u8(0xAF)?;
        self.emit_u8(modrm_rr(dst, src))
    }

    /// `cqo` — sign-extend `rax` into `rdx:rax` (`REX.W` + `99`).
    ///
    /// The implicit-destination instruction the signed [`Self::idiv_rr`] requires:
    /// it divides the **128-bit** `rdx:rax` by its operand, so the `rax` dividend
    /// must first be sign-extended into `rdx`. A `mov rdx, 0` would be wrong (it
    /// makes a negative dividend a huge unsigned one) and `sar rdx, 63` would
    /// clobber the flags `cqo` also sets.
    pub fn cqo(&mut self) -> Result<()> {
        self.emit_rex(true, 0, 0, 0)?;
        self.emit_u8(0x99)
    }

    /// `idiv reg` — the 64-bit signed divide (`REX.W` + `F7 /7`).
    ///
    /// Divides the 128-bit `rdx:rax` (sign-extended by [`Self::cqo`]) by `divisor`
    /// and leaves the quotient, **truncated toward zero**, in `rax` and the
    /// remainder — which takes the dividend's sign — in `rdx`. That is exactly
    /// Erlang's `div`/`rem` pair, so one divide yields both.
    ///
    /// Two conditions make it raise `#DE` — a CPU trap (SIGFPE), not a flag:
    /// a **zero divisor** and a quotient that does not fit in 64 bits (only
    /// `i64::MIN / -1`). Neither is recoverable here, so the caller must refuse a
    /// zero divisor itself and bound its operands; the arithmetic fast path's
    /// small-int guard does both (see [`super::super::baseline::emit::arith`]).
    pub fn idiv_rr(&mut self, divisor: Reg) -> Result<()> {
        self.emit_rex(true, 0, 0, ext(divisor))?;
        self.emit_u8(0xF7)?;
        self.emit_u8(0xC0 | (0x7 << 3) | (divisor.code() & 0x7))
    }

    // -- shifts --------------------------------------------------------------

    /// `sar dst, imm` — an arithmetic (sign-preserving) shift right.
    pub fn sar_ri(&mut self, dst: Reg, imm: u32) -> Result<()> {
        self.emit_shift_ri(7, dst, imm)
    }

    /// `shr dst, imm` — a **logical** (zero-filling) shift right by an immediate
    /// (`REX.W C1 /5 ib`).
    ///
    /// The counterpart of [`Self::sar_ri`]: the vacated high bits are filled with 0,
    /// so a negative value shifts as the unsigned bit pattern it is (a `shr` by 1 of
    /// `-1` is `i64::MAX`, where a `sar` by 1 stays `-1`). The count is a `u8`
    /// because the encoding's immediate is one byte.
    pub fn shr_ri(&mut self, dst: Reg, imm: u8) -> Result<()> {
        self.emit_shift_ri(5, dst, u32::from(imm))
    }

    /// `shl dst, imm` — a logical shift left.
    pub fn shl_ri(&mut self, dst: Reg, imm: u32) -> Result<()> {
        self.emit_shift_ri(4, dst, imm)
    }

    /// `shr dst, cl` — a logical shift right by the count in `cl` (`REX.W D3 /5`).
    ///
    /// The caller is responsible for the count: `cl`, the low byte of `rcx`, must
    /// already hold it (the other 56 bits of `rcx` are ignored). The CPU masks the
    /// count to 6 bits for a 64-bit operand, so a count of 64 shifts by 0 and a count
    /// of 65 shifts by 1 — it is never out of range and never traps.
    pub fn shr_cl(&mut self, dst: Reg) -> Result<()> {
        self.emit_shift_cl(5, dst)
    }

    /// `shl dst, cl` — a logical shift left by the count in `cl` (`REX.W D3 /4`).
    ///
    /// Same count contract as [`Self::shr_cl`]: `cl` holds the count and the CPU masks
    /// it to 6 bits for a 64-bit operand.
    pub fn shl_cl(&mut self, dst: Reg) -> Result<()> {
        self.emit_shift_cl(4, dst)
    }

    /// Emits the `C1 /n ib` shift form: `REX.W`, the opcode, a `ModRM` whose `reg`
    /// field is the shift kind (`4` = left, `5` = logical right, `7` = arithmetic
    /// right), and the 8-bit count.
    ///
    /// Only this immediate form is supported by this helper: a count above
    /// `u8::MAX` would need the `cl`-form (`D3 /n`), so it is rejected as an API
    /// misuse (`BadArg`) rather than encoded — [`Self::shr_cl`]/[`Self::shl_cl`] are
    /// the encoders for a run-time count.
    fn emit_shift_ri(&mut self, op: u8, dst: Reg, imm: u32) -> Result<()> {
        let imm = u8::try_from(imm).map_err(|_| BeamError::BadArg)?;
        self.emit_rex(true, 0, 0, ext(dst))?;
        self.emit_u8(0xC1)?;
        self.emit_u8(0xC0 | ((op & 0x7) << 3) | (dst.code() & 0x7))?;
        self.emit_u8(imm)
    }

    /// Emits the `D3 /n` shift-by-`cl` form: `REX.W`, the opcode, and a `ModRM` whose
    /// `reg` field is the shift kind and whose `r/m` field is the shifted register.
    fn emit_shift_cl(&mut self, op: u8, dst: Reg) -> Result<()> {
        self.emit_rex(true, 0, 0, ext(dst))?;
        self.emit_u8(0xD3)?;
        self.emit_u8(0xC0 | ((op & 0x7) << 3) | (dst.code() & 0x7))
    }

    // -- stack ---------------------------------------------------------------

    /// `push reg`.
    pub fn push_r(&mut self, reg: Reg) -> Result<()> {
        if ext(reg) != 0 {
            self.emit_u8(0x41)?;
        }
        self.emit_u8(0x50 + (reg.code() & 0x7))
    }

    /// `pop reg`.
    pub fn pop_r(&mut self, reg: Reg) -> Result<()> {
        if ext(reg) != 0 {
            self.emit_u8(0x41)?;
        }
        self.emit_u8(0x58 + (reg.code() & 0x7))
    }

    // -- control flow --------------------------------------------------------

    /// `jmp label` (`rel32`).
    pub fn jmp(&mut self, label: Label) -> Result<()> {
        self.emit_u8(0xE9)?;
        self.emit_rel32(label)
    }

    /// `jcc label` (`rel32`) — jump when `cond` holds.
    pub fn jcc(&mut self, cond: Cond, label: Label) -> Result<()> {
        self.emit_u8(0x0F)?;
        self.emit_u8(0x80 + cond.code())?;
        self.emit_rel32(label)
    }

    /// `call label` (`rel32`, direct).
    pub fn call_label(&mut self, label: Label) -> Result<()> {
        self.emit_u8(0xE8)?;
        self.emit_rel32(label)
    }

    /// `jmp reg` (indirect, through a register).
    ///
    /// The encoding is `FF /4`: the same ModRM shape [`call_r`](Self::call_r) uses with
    /// the `/4` opcode extension instead of `/2`. A block uses it for the one transfer
    /// whose target is only known at run time — a **return** whose continuation resolves to
    /// a block entry of the function that is already running (see
    /// [`emit_return`](super::super::baseline::emit) and the tier's `resume_entry` slot) —
    /// which is a tail transfer: it leaves the block's native frame first, so the target's
    /// own prologue rebuilds one and the native stack does not grow.
    pub fn jmp_r(&mut self, reg: Reg) -> Result<()> {
        if ext(reg) != 0 {
            self.emit_u8(0x41)?;
        }
        self.emit_u8(0xFF)?;
        self.emit_u8(0xC0 | (0x4 << 3) | (reg.code() & 0x7))
    }

    /// `call reg` (indirect, through a register).
    pub fn call_r(&mut self, reg: Reg) -> Result<()> {
        if ext(reg) != 0 {
            self.emit_u8(0x41)?;
        }
        self.emit_u8(0xFF)?;
        self.emit_u8(0xC0 | (0x2 << 3) | (reg.code() & 0x7))
    }

    /// `ret`.
    pub fn ret(&mut self) -> Result<()> {
        self.emit_u8(0xC3)
    }

    // -- filler --------------------------------------------------------------

    /// `nop`.
    pub fn nop(&mut self) -> Result<()> {
        self.emit_u8(0x90)
    }

    /// `int3` — a debugger breakpoint / trap.
    pub fn int3(&mut self) -> Result<()> {
        self.emit_u8(0xCC)
    }
}
