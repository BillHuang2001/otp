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

//! Executed pins for the **inline local-call verdict gate**
//! (`emit::block::emit_local_call_gate`): every reject condition really rejects, the one
//! accepted condition really accepts, and the gate commits nothing on either path.
//!
//! The gate is emitted as a standalone snippet whose `reject` and `accepted` labels are
//! bound to two distinguishable constant returns, so "which way did it go" is a value
//! rather than a byte-pattern guess. The verdict slab is a **real** `LocalVerdicts` (built
//! by `LocalVerdicts::for_module` and read through its own `byte_slots` accessor), so the
//! published base/end pair and the byte the gate loads are exactly the ones the seam would
//! read.
//!
//! Reject conditions (`Verdict::Served` and `Verdict::Unknown` included) are the ones that
//! keep today's helper path; acceptance is exactly `byte < Verdict::Served`, i.e.
//! `Declined`/`Unnameable` — "enter this callee in this machine". The machine's own state is
//! asserted unchanged around **every** case, which is the deopt-idempotency obligation: a
//! reject must be observationally identical to the instruction never having run the gate.

use std::cell::Cell;

use beam_loader::Module;

use crate::context::MinimalContext;
use crate::interpreter::LabelMap;
use crate::jit::emitter::{CodeBuffer, Label as CodeLabel, Reg};
use crate::machine::{LocalVerdicts, Machine, Verdict};

use crate::jit::baseline::emit::emit_local_call_gate;
use crate::jit::baseline::emit::RUNTIME_SLOT_DISP;

/// What the emitted snippet answers: which arm the gate reached.
const REJECTED: i64 = 0x1234;
const ACCEPTED: i64 = 0x5678;

/// The callee index the fixtures gate for.
const CALLEE: u32 = 0;

/// Emit the gate for `callee` with both arms bound to a constant return.
fn gate_snippet(callee: u32) -> CodeBuffer {
    let mut buffer = CodeBuffer::new().expect("a fresh code buffer");
    // Reserve the block's `rt` slot exactly as a block prologue does: a block entry has
    // `rsp ≡ 8 (mod 16)`, and one eight-byte push both reserves the slot at `rsp + 0` (where
    // `RUNTIME_SLOT_DISP` points) and restores the 16-byte alignment a helper call site
    // needs. The value is popped on both arms, before the snippet returns.
    buffer.push_r(Reg::R11).expect("reserve the rt slot");
    buffer
        .mov_mr(Reg::Rsp, RUNTIME_SLOT_DISP, Reg::Rdi)
        .expect("store the rt argument into the block slot");
    let reject: CodeLabel = buffer.label();
    let accepted: CodeLabel = buffer.label();
    emit_local_call_gate(&mut buffer, callee as i32, reject, accepted).expect("the gate emits");
    buffer.bind(reject).expect("bind the reject arm");
    buffer.pop_r(Reg::R11).expect("release the rt slot");
    buffer.mov_ri(Reg::Rax, REJECTED).expect("reject answer");
    buffer.ret().expect("reject returns");
    buffer.bind(accepted).expect("bind the accepted arm");
    buffer.pop_r(Reg::R11).expect("release the rt slot");
    buffer.mov_ri(Reg::Rax, ACCEPTED).expect("accepted answer");
    buffer.ret().expect("accepted returns");
    buffer.seal().expect("the snippet seals");
    buffer
}

/// The snippet's bytes (the mapping is readable after `seal`).
fn emitted(buffer: &CodeBuffer) -> Vec<u8> {
    // SAFETY: `code_ptr()` is the start of the buffer's live mapping and `len()` bytes of it
    // have been emitted, so `[ptr, ptr + len)` is a valid, initialized range; the slice
    // borrows nothing that is mutated while it lives.
    unsafe { std::slice::from_raw_parts(buffer.code_ptr(), buffer.len()).to_vec() }
}

/// A module with `functions` one-arity functions, so a `LocalVerdicts` sized from it has one
/// slot per callee index the gate can name.
fn fixture(functions: usize) -> Module {
    let mut fixture = crate::machine::test_support::Fixture::new("gate");
    for index in 0..functions {
        fixture.add_function(&format!("f{index}"), 1, Vec::new());
    }
    fixture.finish()
}

/// The published verdict state the gate reads, borrowed from a real table.
#[derive(Clone, Copy)]
struct Published {
    slots: *const u8,
    end: *const u8,
    pending: *const u8,
}

/// Publish `table`'s slab plus the address of `flag` — the shape the driver publishes at
/// every block entry.
fn publish(table: &LocalVerdicts, flag: &Cell<bool>) -> Published {
    let (slots, len) = table.byte_slots();
    Published {
        slots,
        end: slots.wrapping_add(len),
        pending: std::ptr::addr_of!(*flag) as *const u8,
    }
}

/// The machine state the gate must never change.
#[derive(Debug, PartialEq, Eq)]
struct MachineState {
    current_code: u64,
    x0: beam_term::Eterm,
    x_len: usize,
    stack_len: usize,
    frames_len: usize,
    context: u32,
}

fn snapshot(machine: &Machine) -> MachineState {
    MachineState {
        current_code: machine.current_code,
        x0: machine.x[0],
        x_len: machine.x.len(),
        stack_len: machine.stack.len(),
        frames_len: machine.frames.len(),
        context: machine.current_context(),
    }
}

/// Run the gate snippet once with the given publication, asserting it left the machine
/// exactly as it found it, and answer which arm it reached.
fn run_gate(snippet: &CodeBuffer, machine: &mut Machine, published: Published) -> i64 {
    let before = snapshot(machine);
    // SAFETY: the buffer is sealed, so `code_ptr` is the entry of an executable mapping that
    // holds the snippet; its signature is `extern "C" fn(*mut JitRuntime) -> i64` (the gate
    // loads `rt` from the block's own stack slot and answers a constant).
    let f: extern "C" fn(*mut crate::jit::baseline::runtime::JitRuntime<'_>) -> i64 =
        unsafe { std::mem::transmute(snippet.code_ptr()) };
    let module = Module::default();
    let labels = LabelMap::new();
    let mut ctx = MinimalContext::new();
    let current_code = std::ptr::addr_of_mut!(machine.current_code);
    let answer = {
        let mut rt = crate::jit::baseline::runtime::JitRuntime {
            machine: &mut *machine,
            x_regs: machine.x.as_mut_ptr(),
            module: &module,
            entry_module: &module,
            labels: &labels,
            ctx: &mut ctx,
            error: None,
            enter: None,
            suspend: None,
            y_base: std::ptr::null_mut(),
            y_end: std::ptr::null_mut(),
            literals_base: std::ptr::null(),
            literals_end: std::ptr::null(),
            resume_entry: std::ptr::null(),
            tables: crate::jit::baseline::runtime::EntryTables::NONE,
            compiled: std::ptr::null(),
            current_code,
            reduce_hook: None,
            verdict_slots: published.slots,
            verdict_slots_end: published.end,
            verdict_pending: published.pending,
        };
        f(&mut rt as *mut _)
    };
    assert_eq!(
        snapshot(machine),
        before,
        "the gate writes no machine state on either path"
    );
    answer
}

/// **Every reject condition rejects.**
#[test]
fn every_reject_condition_rejects() {
    let module = fixture(2);
    let disarmed = Cell::new(false);
    let mut machine = Machine::new();
    let snippet = gate_snippet(CALLEE);

    // (1) no published pending address at all — the runtime published no verdict state.
    let (slots, len) = LocalVerdicts::for_module(&module).byte_slots();
    assert_eq!(
        run_gate(
            &snippet,
            &mut machine,
            Published {
                slots,
                end: slots.wrapping_add(len),
                pending: std::ptr::null(),
            }
        ),
        REJECTED,
        "a null pending address must reject"
    );

    // (2) an armed invalidation: the seam must consume it, so the gate may not consult the
    //     slab even though the verdict there would be accepted.
    let armed = Cell::new(true);
    let table = LocalVerdicts::for_module(&module);
    table.set(CALLEE, Verdict::Declined);
    assert_eq!(
        run_gate(&snippet, &mut machine, publish(&table, &armed)),
        REJECTED,
        "an armed invalidation must reject so the seam consumes the flag"
    );
    assert!(armed.get(), "the gate never consumes the flag itself");

    // (3) no published slab.
    assert_eq!(
        run_gate(
            &snippet,
            &mut machine,
            Published {
                slots: std::ptr::null(),
                end: std::ptr::null(),
                pending: std::ptr::addr_of!(disarmed) as *const u8,
            }
        ),
        REJECTED,
        "a null slab must reject"
    );

    // (4) a slot at or past the published end — the twin of `table.get(callee)`'s `None`,
    //     whose bound is exclusive.
    let table = LocalVerdicts::for_module(&module);
    table.set(CALLEE, Verdict::Declined);
    assert_eq!(
        run_gate(
            &snippet,
            &mut machine,
            Published {
                slots: table.byte_slots().0,
                end: table.byte_slots().0,
                pending: std::ptr::addr_of!(disarmed) as *const u8,
            }
        ),
        REJECTED,
        "a slot at the published end must reject"
    );

    // (5) the two verdicts that mean "the host is on the other end": `Served` (its code must
    //     not run) and `Unknown` (the seam must derive the identity and ask).
    for verdict in [Verdict::Served, Verdict::Unknown] {
        let table = LocalVerdicts::for_module(&module);
        table.set(CALLEE, verdict);
        assert_eq!(
            run_gate(&snippet, &mut machine, publish(&table, &disarmed)),
            REJECTED,
            "{verdict:?} must reject: the helper has to see it"
        );
    }
}

/// **The one accepted condition accepts**, index-addressed: only `byte < Verdict::Served`
/// (`Declined`/`Unnameable`) enters the callee in this machine, and the byte read is the
/// *callee's own* slot rather than a neighbour's.
#[test]
fn the_enter_in_machine_verdicts_accept_from_the_callees_own_slot() {
    let module = fixture(2);
    let disarmed = Cell::new(false);
    let mut machine = Machine::new();
    let snippet = gate_snippet(CALLEE);

    for verdict in [Verdict::Declined, Verdict::Unnameable] {
        let table = LocalVerdicts::for_module(&module);
        table.set(CALLEE, verdict);
        // The neighbouring slot says the opposite, so a gate reading the wrong slot fails.
        table.set(CALLEE + 1, Verdict::Served);
        assert_eq!(
            run_gate(&snippet, &mut machine, publish(&table, &disarmed)),
            ACCEPTED,
            "{verdict:?} means 'enter this callee in this machine'"
        );
    }

    // The same publication, read for slot 1, must reject: the verdict is the callee's own.
    let table = LocalVerdicts::for_module(&module);
    table.set(CALLEE, Verdict::Declined);
    table.set(CALLEE + 1, Verdict::Served);
    let neighbour = gate_snippet(CALLEE + 1);
    assert_eq!(
        run_gate(&neighbour, &mut machine, publish(&table, &disarmed)),
        REJECTED,
        "the verdict is read at the callee's own function index"
    );
}

/// The gate dials nobody: it either rejects to the existing helper sequence or falls through
/// to the block's static exit, so its own bytes never carry the `jit_local_call` address the
/// ungated probe loads into `r11` before its indirect call.
///
/// The check is the helper's own `movabs` immediate — a ten-byte sequence the emitter
/// produces for that exact helper and nothing else in this snippet — rather than an opcode
/// scan, so a struct-offset change can never make it fail for the wrong reason. That the
/// gate *branches* is what the executed cases above pin: a gate that fell through
/// unconditionally could not answer both `REJECTED` and `ACCEPTED`.
#[test]
fn the_gate_never_loads_the_local_call_helper() {
    let snippet = gate_snippet(CALLEE);
    let bytes = emitted(&snippet);
    let mut probe = CodeBuffer::new().expect("a fresh code buffer");
    probe
        .mov_ri(Reg::R11, crate::jit::baseline::jit_local_call as *const () as i64)
        .expect("the helper's movabs immediate");
    let helper_immediate = emitted(&probe);
    assert!(
        !bytes
            .windows(helper_immediate.len())
            .any(|window| window == helper_immediate.as_slice()),
        "the gate must not carry the helper's address: a reject jumps to the sequence that \
         does, an accept falls through to the block exit"
    );
    assert_ne!(REJECTED, ACCEPTED, "the two arms must be distinguishable");
}
