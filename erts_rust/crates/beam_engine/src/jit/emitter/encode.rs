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
        self.emit_rex(true, ext(dst), 0, ext(base))?;
        self.emit_u8(0x8B)?;
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

    // -- shifts --------------------------------------------------------------

    /// `sar dst, imm` — an arithmetic (sign-preserving) shift right.
    pub fn sar_ri(&mut self, dst: Reg, imm: u32) -> Result<()> {
        self.emit_shift_ri(7, dst, imm)
    }

    /// `shl dst, imm` — a logical shift left.
    pub fn shl_ri(&mut self, dst: Reg, imm: u32) -> Result<()> {
        self.emit_shift_ri(4, dst, imm)
    }

    /// Emits the `C1 /n ib` shift form: `REX.W`, the opcode, a `ModRM` whose `reg`
    /// field is the shift kind (`4` = left, `7` = arithmetic right), and the 8-bit
    /// count.
    ///
    /// Only this immediate form is supported: a count above `u8::MAX` would need
    /// the `cl`-form (`D3 /n`), so it is rejected as an API misuse (`BadArg`)
    /// rather than encoded.
    fn emit_shift_ri(&mut self, op: u8, dst: Reg, imm: u32) -> Result<()> {
        let imm = u8::try_from(imm).map_err(|_| BeamError::BadArg)?;
        self.emit_rex(true, 0, 0, ext(dst))?;
        self.emit_u8(0xC1)?;
        self.emit_u8(0xC0 | ((op & 0x7) << 3) | (dst.code() & 0x7))?;
        self.emit_u8(imm)
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
