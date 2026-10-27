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
//! Pins the [`Transfer`] **return ABI** — the one contract the emitted in-block transfer
//! arms ([`super::emit::frame`], [`super::emit::link`]) rely on when they read the resolved
//! block entry out of `rdx` instead of out of a `JitRuntime` memory slot.
//!
//! `Transfer` is `#[repr(C)]` with exactly two eightbytes of INTEGER class, so the SysV
//! x86-64 C ABI answers it **whole in registers**:
//!
//! ```text
//!   rax = word    ; the block-ABI word for the driver (continuation / HALT / ERROR / ENTER / …)
//!   rdx = entry   ; the resolved native block entry to tail-jump to, or null for "no transfer"
//! ```
//!
//! No sentinel is involved: a null `entry` *is* the "no transfer" answer, because the
//! block-ABI word space overlaps user-space code addresses, so a magic word could not be
//! told apart from a real entry.
//!
//! Three legs, each pinning a different part of that claim:
//!
//! * **shape** — the layout (size, alignment, the two field offsets) that makes the struct
//!   two INTEGER eightbytes rather than, say, a memory-class aggregate;
//! * **Rust caller** — a `Transfer`-returning `extern "C"` function observed from Rust, so a
//!   change to the struct (a third field, a different repr) fails here first;
//! * **emitted caller** — the *same* function called from **emitted code**, which reads both
//!   registers back (`rax` and `rdx` stored into a scratch array) and branches on `rdx`
//!   exactly as the transfer arms do (`test rdx, rdx` / `je`). This is the leg that proves
//!   the register the arms test carries the entry — an emitter or ABI drift that put the
//!   struct in memory would still pass the Rust-caller leg.
//!
//! The tests *execute* the code they emit (the emitter's own suite's discipline), never
//! merely compare bytes.

use super::runtime::{encode_target, Transfer};
use crate::jit::emitter::{CodeBuffer, Cond, Reg};

/// The block-ABI word the fixtures answer: a plain continuation, the shape a *taken*
/// in-block transfer carries in `rax`.
const WORD: i64 = encode_target(3, 7);

/// A non-null `entry` fixture. Its address is never dereferenced — only tested for null
/// and compared for identity — so any live object serves.
static ENTRY: u8 = 0;

extern "C" fn taken() -> Transfer {
    Transfer::entering(WORD, core::ptr::addr_of!(ENTRY))
}

extern "C" fn declined() -> Transfer {
    Transfer::leaving(WORD)
}

/// The emitted leg's signature: `f(slots)` stores a fixture's `rax`/`rdx` into
/// `slots[0]`/`slots[1]`.
type RegSink = extern "C" fn(*mut i64);

/// The emitted leg's other signature: `f()` answers `1` when the fixture's `rdx` was
/// non-null and `0` otherwise — the arms' `test rdx, rdx` discriminator.
type EntryProbe = extern "C" fn() -> i64;

fn address_of(helper: extern "C" fn() -> Transfer) -> i64 {
    helper as *const () as i64
}

/// `mov r11, <helper>` / `call r11` — the same two instructions
/// [`super::emit`](super::emit::emit_helper_call) uses to reach a helper.
fn emit_helper_invoke(buffer: &mut CodeBuffer, helper: extern "C" fn() -> Transfer) {
    buffer.mov_ri(Reg::R11, address_of(helper)).unwrap();
    buffer.call_r(Reg::R11).unwrap();
}

#[test]
fn the_layout_is_two_integer_eightbytes() {
    // Two eightbytes of INTEGER class is exactly what makes the SysV C ABI return the
    // struct in `rax`/`rdx` instead of through a hidden pointer; a third field or a
    // 16-byte-aligned member would move it to the memory class and silently break every
    // emitted arm's `test rdx`.
    assert_eq!(core::mem::size_of::<Transfer>(), 16);
    assert_eq!(core::mem::align_of::<Transfer>(), 8);
    assert_eq!(core::mem::offset_of!(Transfer, word), 0);
    assert_eq!(core::mem::offset_of!(Transfer, entry), 8);
}

#[test]
fn a_rust_caller_reads_both_fields() {
    let transfer = taken();
    assert_eq!(transfer.word, WORD);
    assert_eq!(transfer.entry, core::ptr::addr_of!(ENTRY));
    // The declined answer is the same `word` with a null entry — the "no transfer" case,
    // and the reason no sentinel is needed.
    let leaving = declined();
    assert_eq!(leaving.word, WORD);
    assert!(leaving.entry.is_null());
}

#[test]
fn emitted_code_reads_the_entry_out_of_rdx() {
    // The Rust caller's view could be satisfied by a struct returned in memory; only
    // reading the *registers* back pins `rdx` as the entry's home.
    let fixtures: Vec<extern "C" fn() -> Transfer> = vec![taken, declined];
    for helper in fixtures {
        let mut buffer = CodeBuffer::new().unwrap();
        // `slots` arrives in `rdi`; the fixture takes no arguments but may clobber `rdi`
        // (caller-saved), so park it on the stack across the call. One push leaves `rsp`
        // 16-byte aligned for the call (entry alignment is `rsp % 16 == 8`).
        buffer.push_r(Reg::Rdi).unwrap();
        emit_helper_invoke(&mut buffer, helper);
        buffer.pop_r(Reg::Rdi).unwrap();
        buffer.mov_mr(Reg::Rdi, 0, Reg::Rax).unwrap();
        buffer.mov_mr(Reg::Rdi, 8, Reg::Rdx).unwrap();
        buffer.ret().unwrap();
        buffer.seal().unwrap();

        let sink: RegSink = buffer.as_fn_ptr();
        let mut slots = [-1i64; 2];
        sink(slots.as_mut_ptr());

        let expected = helper();
        assert_eq!(slots[0], expected.word, "rax is the driver word");
        assert_eq!(
            slots[1],
            expected.entry as i64,
            "rdx is the resolved entry (null when there is none)"
        );
    }
}

#[test]
fn emitted_code_branches_on_the_entry_like_the_transfer_arm() {
    // The arms' shape: `test rdx, rdx` / `je` — taken means "tail-jump to rdx", and the
    // declined arm leaves for the driver with `rax`.
    let fixtures: [(extern "C" fn() -> Transfer, bool); 2] = [(taken, true), (declined, false)];
    for (helper, expect_taken) in fixtures {
        let mut buffer = CodeBuffer::new().unwrap();
        emit_helper_invoke(&mut buffer, helper);
        let declined = buffer.label();
        buffer.test_rr(Reg::Rdx, Reg::Rdx).unwrap();
        buffer.jcc(Cond::E, declined).unwrap();
        buffer.mov_ri(Reg::Rax, 1).unwrap();
        buffer.ret().unwrap();
        buffer.bind(declined).unwrap();
        buffer.mov_ri(Reg::Rax, 0).unwrap();
        buffer.ret().unwrap();
        buffer.seal().unwrap();

        let probe: EntryProbe = buffer.as_fn_ptr();
        assert_eq!(
            probe(),
            i64::from(expect_taken),
            "the arm's rdx probe answers taken iff the transfer carries an entry"
        );
    }
}
