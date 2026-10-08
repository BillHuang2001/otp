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

    use super::*;

    /// `extern "C" fn(i64, i64) -> i64` (System V: `rdi`, `rsi` → `rax`).
    type BinOp = extern "C" fn(i64, i64) -> i64;

    /// The raw permissions string (`rw-p`, `r-xp`, …) of the mapping containing
    /// `addr`, read from `/proc/self/maps`; `None` if it cannot be determined
    /// (e.g. a non-Linux host).
    fn mapping_perms(addr: usize) -> Option<String> {
        let maps = std::fs::read_to_string("/proc/self/maps").ok()?;
        for line in maps.lines() {
            let Some((range, rest)) = line.split_once(' ') else {
                continue;
            };
            let Some((start, end)) = range.split_once('-') else {
                continue;
            };
            let (Ok(start), Ok(end)) = (
                usize::from_str_radix(start, 16),
                usize::from_str_radix(end, 16),
            ) else {
                continue;
            };
            if start <= addr && addr < end {
                return rest.split_whitespace().next().map(str::to_string);
            }
        }
        None
    }

    #[test]
    fn register_codes_are_the_architectural_encodings() {
        for (i, reg) in Reg::ALL.into_iter().enumerate() {
            assert_eq!(reg.code() as usize, i);
        }
        assert_eq!(Reg::Rsp.code(), 4);
        assert_eq!(Reg::R12.code(), 12);
        for cond in [Cond::E, Cond::Ne, Cond::L, Cond::Ge, Cond::Le, Cond::G] {
            assert_eq!(cond.code() & 0b1111, cond.code());
        }
        assert_eq!(Cond::E.code(), 0x4);
        assert_eq!(Cond::G.code(), 0xF);
    }

    #[test]
    fn capacity_is_a_whole_number_of_pages() {
        let buf = CodeBuffer::new().unwrap();
        assert_eq!(buf.capacity() % page_size(), 0);
        assert!(buf.capacity() >= DEFAULT_CAPACITY);
        assert_eq!(buf.len(), 0);
        assert!(buf.is_empty());
        assert!(!buf.is_sealed());
        // A requested zero capacity still yields one page.
        let tiny = CodeBuffer::with_capacity(0).unwrap();
        assert_eq!(tiny.capacity(), page_size());
    }

    #[test]
    fn emits_add() {
        let mut buf = CodeBuffer::new().unwrap();
        buf.mov_rr(Reg::Rax, Reg::Rdi).unwrap();
        buf.add_rr(Reg::Rax, Reg::Rsi).unwrap();
        buf.ret().unwrap();
        buf.seal().unwrap();

        let add: BinOp = buf.as_fn_ptr();
        assert_eq!(add(2, 3), 5);
        assert_eq!(add(-4, 10), 6);
        assert_eq!(add(0, 0), 0);
        assert_eq!(add(i64::MIN, i64::MAX), -1);
    }

    #[test]
    fn emits_imul_sar_and_shl() {
        // `g(a, b) = ((a * b) >>a 4) << 4` — the three encoders the `erlang:'*'/2`
        // fast path's untag/multiply/re-tag sequence is built from.
        let mut buf = CodeBuffer::new().unwrap();
        buf.mov_rr(Reg::Rax, Reg::Rdi).unwrap();
        buf.imul_rr(Reg::Rax, Reg::Rsi).unwrap();
        buf.sar_ri(Reg::Rax, 4).unwrap();
        buf.shl_ri(Reg::Rax, 4).unwrap();
        buf.ret().unwrap();
        buf.seal().unwrap();

        let g: BinOp = buf.as_fn_ptr();
        assert_eq!(g(6, 7), 32); // 42 >> 4 == 2, << 4 == 32
        assert_eq!(g(-6, 7), -48); // -42 >>a 4 == -3 (sign-preserving), << 4 == -48
        assert_eq!(g(0, -9), 0);
        // The low 64 bits of a product that overflows are exact (and `imul` reports
        // the overflow in `OF`); rejecting such a product is the fast path's guard's
        // job, not the encoder's.
        assert_eq!(g(i64::MAX, 2), -16); // (-2) >>a 4 == -1, << 4 == -16
        // A shift count above the 8-bit immediate is refused, never mis-encoded.
        let mut buf = CodeBuffer::new().unwrap();
        assert!(buf.sar_ri(Reg::Rax, 256).is_err());
        assert!(buf.shl_ri(Reg::Rax, u32::MAX).is_err());
    }

    #[test]
    fn emits_writes_to_r8_and_r9() {
        let mut buf = CodeBuffer::new().unwrap();
        buf.mov_rr(Reg::R8, Reg::Rdi).unwrap();
        buf.mov_rr(Reg::R9, Reg::Rsi).unwrap();
        buf.add_rr(Reg::R8, Reg::R9).unwrap();
        buf.mov_rr(Reg::Rax, Reg::R8).unwrap();
        buf.ret().unwrap();
        buf.seal().unwrap();

        let f: BinOp = buf.as_fn_ptr();
        assert_eq!(f(20, 22), 42);
        assert_eq!(f(-1, -2), -3);
    }

    #[test]
    fn emits_six_argument_sum_through_r8_and_r9() {
        // The 5th and 6th integer arguments arrive in `r8`/`r9`, so this exercises
        // `REX.R` on the source operands.
        let mut buf = CodeBuffer::new().unwrap();
        buf.mov_rr(Reg::Rax, Reg::Rdi).unwrap();
        for src in [Reg::Rsi, Reg::Rdx, Reg::Rcx, Reg::R8, Reg::R9] {
            buf.add_rr(Reg::Rax, src).unwrap();
        }
        buf.ret().unwrap();
        buf.seal().unwrap();

        let sum: extern "C" fn(i64, i64, i64, i64, i64, i64) -> i64 = buf.as_fn_ptr();
        assert_eq!(sum(1, 2, 3, 4, 5, 6), 21);
        assert_eq!(sum(-1, -2, -3, -4, -5, -6), -21);
    }

    #[test]
    fn emits_r12_base_which_forces_a_sib_byte() {
        // `r12` is callee-saved, so it is preserved around the call; its use as an
        // address base forces a `SIB` byte in the `mov_rm`/`mov_mr` encodings.
        let mut buf = CodeBuffer::new().unwrap();
        buf.push_r(Reg::R12).unwrap();
        buf.mov_rr(Reg::R12, Reg::Rdi).unwrap();
        buf.mov_rm(Reg::Rax, Reg::R12, 8).unwrap(); // rax = p[1]
        buf.mov_rm(Reg::Rcx, Reg::R12, 0).unwrap(); // rcx = p[0]
        buf.add_rr(Reg::Rax, Reg::Rcx).unwrap(); // rax = p[0] + p[1]
        buf.mov_mr(Reg::R12, 16, Reg::Rax).unwrap(); // p[2] = sum
        buf.pop_r(Reg::R12).unwrap();
        buf.ret().unwrap();
        buf.seal().unwrap();

        let f: extern "C" fn(*mut i64) -> i64 = buf.as_fn_ptr();
        let mut data = [20i64, 22, 0];
        assert_eq!(f(data.as_mut_ptr()), 42);
        assert_eq!(data, [20, 22, 42]);
    }

    #[test]
    fn emits_rsp_base_which_forces_a_sib_byte() {
        let mut buf = CodeBuffer::new().unwrap();
        buf.mov_ri(Reg::Rcx, 32).unwrap();
        buf.sub_rr(Reg::Rsp, Reg::Rcx).unwrap();
        buf.mov_mr(Reg::Rsp, 0, Reg::Rdi).unwrap(); // [rsp] = a
        buf.mov_rm(Reg::Rax, Reg::Rsp, 0).unwrap(); // rax = [rsp]
        buf.mov_ri(Reg::Rdx, 5).unwrap();
        buf.add_rr(Reg::Rax, Reg::Rdx).unwrap();
        buf.mov_ri(Reg::Rcx, 32).unwrap();
        buf.add_rr(Reg::Rsp, Reg::Rcx).unwrap();
        buf.ret().unwrap();
        buf.seal().unwrap();

        let f: extern "C" fn(i64) -> i64 = buf.as_fn_ptr();
        assert_eq!(f(10), 15);
        assert_eq!(f(-5), 0);
    }

    #[test]
    fn emits_rbp_base_which_is_a_plain_disp32_not_rip_relative() {
        // `rbp` is released from its frame-pointer role by the baseline JIT (it is one
        // of its cache registers), so the encoder must also handle it as a memory
        // *base*. The uniform `mod=10` `disp32` form sidesteps the `mod=00`/`rbp`
        // "RIP-relative" special case, so `[rbp + disp]` reads a base+displacement
        // instead of resolving a PC-relative address. `push`/`pop` around the use keep
        // the Rust caller's `rbp` intact.
        let mut buf = CodeBuffer::new().unwrap();
        buf.push_r(Reg::Rbp).unwrap();
        buf.mov_rr(Reg::Rbp, Reg::Rdi).unwrap(); // rbp = p
        buf.mov_rm(Reg::Rax, Reg::Rbp, 8).unwrap(); // rax = p[1]  ([rbp + 8])
        buf.pop_r(Reg::Rbp).unwrap();
        buf.ret().unwrap();
        buf.seal().unwrap();

        let f: extern "C" fn(*const i64) -> i64 = buf.as_fn_ptr();
        let data = [10i64, 42];
        assert_eq!(f(data.as_ptr()), 42);
    }

    #[test]
    fn emits_lea() {
        let mut buf = CodeBuffer::new().unwrap();
        buf.mov_rr(Reg::Rax, Reg::Rdi).unwrap();
        buf.lea(Reg::Rax, Reg::Rax, 8).unwrap(); // rax = &p[1]
        buf.mov_rm(Reg::Rax, Reg::Rax, 0).unwrap(); // rax = p[1]
        buf.ret().unwrap();
        buf.seal().unwrap();

        let f: extern "C" fn(*const i64) -> i64 = buf.as_fn_ptr();
        let data = [10i64, 42];
        assert_eq!(f(data.as_ptr()), 42);
    }

    #[test]
    fn emits_conditional_branch_selecting_a_constant() {
        // `max(a, b)`: keep `a` unless the signed comparison says `b` is greater.
        let mut buf = CodeBuffer::new().unwrap();
        let done = buf.label();
        buf.mov_rr(Reg::Rax, Reg::Rdi).unwrap();
        buf.cmp_rr(Reg::Rdi, Reg::Rsi).unwrap();
        buf.jcc(Cond::Ge, done).unwrap();
        buf.mov_rr(Reg::Rax, Reg::Rsi).unwrap();
        buf.bind(done).unwrap();
        buf.ret().unwrap();
        buf.seal().unwrap();

        let max: BinOp = buf.as_fn_ptr();
        assert_eq!(max(3, 7), 7);
        assert_eq!(max(7, 3), 7);
        assert_eq!(max(-5, -9), -5);
        assert_eq!(max(4, 4), 4);
    }

    #[test]
    fn emits_call_through_a_register() {
        // The callee returns `a - b`; the caller forwards its own arguments
        // (`rdi`/`rsi`) to it through `call rax`. `push rbx` keeps the stack
        // 16-byte aligned across the call and preserves the callee-saved `rbx`.
        let mut callee = CodeBuffer::new().unwrap();
        callee.mov_rr(Reg::Rax, Reg::Rdi).unwrap();
        callee.sub_rr(Reg::Rax, Reg::Rsi).unwrap();
        callee.ret().unwrap();
        callee.seal().unwrap();
        let callee_fn: BinOp = callee.as_fn_ptr();
        let callee_addr = callee_fn as usize as i64;

        let mut caller = CodeBuffer::new().unwrap();
        caller.push_r(Reg::Rbx).unwrap();
        caller.mov_ri(Reg::Rax, callee_addr).unwrap();
        caller.call_r(Reg::Rax).unwrap();
        caller.pop_r(Reg::Rbx).unwrap();
        caller.ret().unwrap();
        caller.seal().unwrap();

        let call: BinOp = caller.as_fn_ptr();
        assert_eq!(call(10, 4), 6);
        assert_eq!(call(4, 10), -6);
        // `callee` is still alive here: it must outlive every call through it.
        assert!(callee.is_sealed());
    }

    #[test]
    fn patches_a_forward_jump() {
        // `mov rax, 1; jmp done; mov rax, 2; done: ret` must skip the second move.
        let mut buf = CodeBuffer::new().unwrap();
        let done = buf.label();
        buf.mov_ri(Reg::Rax, 1).unwrap();
        buf.jmp(done).unwrap();
        buf.mov_ri(Reg::Rax, 2).unwrap();
        buf.bind(done).unwrap();
        buf.ret().unwrap();
        buf.seal().unwrap();

        let f: extern "C" fn() -> i64 = buf.as_fn_ptr();
        assert_eq!(f(), 1);
    }

    #[test]
    fn patches_a_forward_conditional_jump() {
        // `mov rax, 10; mov rcx, 5; cmp rcx, rcx; je taken; mov rax, 7; taken: ret`
        // — `cmp rcx, rcx` is equal, so the conditional jump is taken.
        let mut buf = CodeBuffer::new().unwrap();
        let taken = buf.label();
        buf.mov_ri(Reg::Rax, 10).unwrap();
        buf.mov_ri(Reg::Rcx, 5).unwrap();
        buf.cmp_rr(Reg::Rcx, Reg::Rcx).unwrap();
        buf.jcc(Cond::E, taken).unwrap();
        buf.mov_ri(Reg::Rax, 7).unwrap();
        buf.bind(taken).unwrap();
        buf.ret().unwrap();
        buf.seal().unwrap();

        let f: extern "C" fn() -> i64 = buf.as_fn_ptr();
        assert_eq!(f(), 10);
    }

    #[test]
    fn patches_a_backward_jump() {
        // Counts `n` down to zero, accumulating one per iteration:
        // the `jmp loop` is a backward reference, patched the moment it is emitted.
        let mut buf = CodeBuffer::new().unwrap();
        let loop_top = buf.label();
        let end = buf.label();
        buf.mov_ri(Reg::Rax, 0).unwrap();
        buf.mov_rr(Reg::Rcx, Reg::Rdi).unwrap();
        buf.bind(loop_top).unwrap();
        buf.test_rr(Reg::Rcx, Reg::Rcx).unwrap();
        buf.jcc(Cond::E, end).unwrap();
        buf.mov_ri(Reg::Rdx, 1).unwrap();
        buf.sub_rr(Reg::Rcx, Reg::Rdx).unwrap();
        buf.mov_ri(Reg::Rdx, 1).unwrap();
        buf.add_rr(Reg::Rax, Reg::Rdx).unwrap();
        buf.jmp(loop_top).unwrap();
        buf.bind(end).unwrap();
        buf.ret().unwrap();
        buf.seal().unwrap();

        let countdown: extern "C" fn(i64) -> i64 = buf.as_fn_ptr();
        assert_eq!(countdown(5), 5);
        assert_eq!(countdown(0), 0);
        assert_eq!(countdown(100), 100);
    }

    #[test]
    fn seals_w_a_forward_reference_correctly() {
        // A `call` to a later label resolves at `bind` time.
        let mut buf = CodeBuffer::new().unwrap();
        let sub = buf.label();
        buf.push_r(Reg::Rbx).unwrap();
        buf.mov_ri(Reg::Rax, 1000).unwrap();
        buf.call_label(sub).unwrap();
        buf.pop_r(Reg::Rbx).unwrap();
        buf.ret().unwrap();
        buf.bind(sub).unwrap();
        buf.mov_rr(Reg::Rcx, Reg::Rdi).unwrap();
        buf.add_rr(Reg::Rax, Reg::Rcx).unwrap();
        buf.ret().unwrap();
        buf.seal().unwrap();

        let f: extern "C" fn(i64) -> i64 = buf.as_fn_ptr();
        assert_eq!(f(23), 1023);
    }

    #[test]
    fn sealing_with_an_unbound_label_is_a_typed_error() {
        let mut buf = CodeBuffer::new().unwrap();
        let dangling = buf.label();
        buf.jmp(dangling).unwrap();
        assert!(matches!(buf.seal(), Err(BeamError::BadArg)));
        // Binding it afterwards lets the buffer seal.
        buf.bind(dangling).unwrap();
        buf.ret().unwrap();
        assert!(buf.seal().is_ok());
    }

    #[test]
    fn emitting_after_sealing_is_a_typed_error() {
        let mut buf = CodeBuffer::new().unwrap();
        buf.ret().unwrap();
        buf.seal().unwrap();
        assert!(matches!(buf.nop(), Err(BeamError::BadArg)));
        assert!(matches!(buf.ret(), Err(BeamError::BadArg)));
        // Sealing again is an idempotent success.
        assert!(buf.seal().is_ok());
    }

    #[test]
    fn seal_is_w_xor_x() {
        let mut buf = CodeBuffer::new().unwrap();
        buf.mov_rr(Reg::Rax, Reg::Rdi).unwrap();
        buf.ret().unwrap();
        let addr = buf.code_ptr() as usize;

        // Before sealing (where `/proc/self/maps` is available): writable, not
        // executable.
        if let Some(perms) = mapping_perms(addr) {
            assert!(perms.starts_with("rw"), "unsealed perms were {perms:?}");
            assert!(!perms.contains('x'), "unsealed perms were {perms:?}");
        }

        buf.seal().unwrap();

        if let Some(perms) = mapping_perms(addr) {
            assert!(perms.starts_with("r-x"), "sealed perms were {perms:?}");
            assert!(!perms.contains('w'), "sealed perms were {perms:?}");
        }
    }

    #[test]
    fn exhausted_capacity_is_a_typed_error() {
        let mut buf = CodeBuffer::with_capacity(0).unwrap();
        let limit = buf.capacity();
        let mut overflow = None;
        for _ in 0..=limit {
            if let Err(err) = buf.nop() {
                overflow = Some(err);
                break;
            }
        }
        assert!(
            matches!(overflow, Some(BeamError::SystemLimit)),
            "a full code buffer must report `SystemLimit`"
        );
        assert_eq!(buf.len(), limit);
    }

    #[test]
    fn labels_are_buffer_local_and_bound_once() {
        let mut a = CodeBuffer::new().unwrap();
        let mut b = CodeBuffer::new().unwrap();
        let foreign = b.label();

        let mine = a.label();
        a.bind(mine).unwrap();
        // Binding the same label twice is rejected.
        assert!(matches!(a.bind(mine), Err(BeamError::BadArg)));
        // A label from another buffer is not a valid target.
        assert!(matches!(a.jmp(foreign), Err(BeamError::BadArg)));
        assert!(matches!(a.bind(foreign), Err(BeamError::BadArg)));
    }

    #[test]
    fn dropping_a_sealed_buffer_is_clean() {
        // Repeatedly map, seal and release several executable buffers; a leaked or
        // double-unmapped mapping would trip the `munmap` assertion in `Drop`.
        for i in 0..16 {
            let mut buf = CodeBuffer::new().unwrap();
            buf.mov_ri(Reg::Rax, i).unwrap();
            buf.ret().unwrap();
            buf.seal().unwrap();
            let f: extern "C" fn() -> i64 = buf.as_fn_ptr();
            assert_eq!(f(), i);
            drop(buf);
        }
    }

    #[test]
    fn int3_and_nop_emit_one_byte_each() {
        let mut buf = CodeBuffer::new().unwrap();
        buf.nop().unwrap();
        buf.int3().unwrap();
        assert_eq!(buf.len(), 2);
    }
