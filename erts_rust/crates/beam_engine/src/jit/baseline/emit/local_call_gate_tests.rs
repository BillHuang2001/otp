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

use beam_loader::{Instr, Label, Module, Module as BeamModule};

use crate::context::MinimalContext;
use crate::jit::baseline::tests::build_module;
use crate::interpreter::LabelMap;
use crate::jit::emitter::{CodeBuffer, Label as CodeLabel, Reg};
use crate::machine::{LocalVerdicts, Machine, Verdict};

use crate::jit::baseline::emit::emit_local_call_gate;
use crate::jit::baseline::emit::RUNTIME_SLOT_DISP;
use crate::jit::baseline::call::CALL_STORED;

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

// ---------------------------------------------------------------------------
// The gate at the frame-preserving `Instr::Call` site
// ---------------------------------------------------------------------------
//
// `Instr::Call` is the third gate user (after `call_last`/`call_only`) and the only
// one whose two arms differ by a *helper*, not by a probe/branch shape: the reject arm
// runs today's seam-consulting `jit_call_store(_linked)` and the accepted arm runs the
// seam-skipping `jit_call_store_in_machine(_linked)`. Both arms answer `CALL_STORED`,
// so the `cmp`/`jcc` and the compile-time-selected exit after them are identical.
//
// What the pin below proves, from the emitted bytes of a real two-function module:
// the reject arm still passes the instruction's `arity` (five helper arguments) and the
// accepted arm no longer does (four) — the `arity` immediate existed only for the seam.

/// `f/1` calls `g/1` once, frame-preserving and non-tail, then returns `g`'s value.
fn gated_call_module() -> BeamModule {
    build_module(
        "gated_call",
        &[],
        vec![
            (
                "f",
                1,
                vec![
                    Instr::Label(Label(1)),
                    Instr::Label(Label(2)),
                    Instr::Allocate {
                        stack_need: beam_loader::HeapNeed::Words(0),
                        live: 1,
                    },
                    Instr::Call {
                        arity: 1,
                        target: Label(3),
                    },
                    Instr::DeallocateReturn(0),
                ],
            ),
            (
                "g",
                1,
                vec![
                    Instr::Label(Label(3)),
                    Instr::CallOnly {
                        arity: 1,
                        target: Label(4),
                    },
                    Instr::Label(Label(4)),
                    Instr::Return,
                ],
            ),
        ],
        &[("f", 1, Label(2))],
    )
}

/// Every offset at which `movabs r11, <imm64>` occurs in `bytes`, with its immediate.
///
/// `mov_ri` is always the ten-byte `movabs` form, so `r11`'s encoding is `[0x49, 0xBB,
/// imm64]`. The scan is the only way to identify a helper call in emitted code — the
/// artifact carries no relocation table — and it has to accept **any** of the four
/// `jit_call_store*` helpers, because which pair a site uses is a compile-time decision
/// (`linker.is_compiled`) this test must not pin.
fn r11_immediates(bytes: &[u8]) -> Vec<(usize, u64)> {
    let mut sites = Vec::new();
    let mut index = 0;
    while index + 10 <= bytes.len() {
        if bytes[index] == 0x49 && bytes[index + 1] == 0xBB {
            let mut word = [0_u8; 8];
            word.copy_from_slice(&bytes[index + 2..index + 10]);
            sites.push((index, u64::from_le_bytes(word)));
            index += 10;
        } else {
            index += 1;
        }
    }
    sites
}

/// One entry of [`call_helpers`]: the helper's address, whether it is the
/// **seam-consulting** variant, whether it is the **linked** variant, and its name.
type CallHelper = (u64, bool, bool, &'static str);

/// The four call helpers, one per `(seam kind, linked)` pair.
fn call_helpers() -> [CallHelper; 4] {
    use crate::jit::baseline::call::{
        jit_call_store, jit_call_store_in_machine, jit_call_store_in_machine_linked,
        jit_call_store_linked,
    };
    [
        (jit_call_store as *const () as u64, true, false, "jit_call_store"),
        (
            jit_call_store_linked as *const () as u64,
            true,
            true,
            "jit_call_store_linked",
        ),
        (
            jit_call_store_in_machine as *const () as u64,
            false,
            false,
            "jit_call_store_in_machine",
        ),
        (
            jit_call_store_in_machine_linked as *const () as u64,
            false,
            true,
            "jit_call_store_in_machine_linked",
        ),
    ]
}

/// The one emitted call of `bytes` whose helper is the `consult`/`skip` seam kind,
/// answering its offset, whether that helper is the linked variant, and its name.
/// Exactly one must exist: which *linked-ness* a site uses is the compile-time
/// `linker.is_compiled` decision this test must not pin, but the seam kind is the arm.
fn single_helper_site(bytes: &[u8], consult: bool) -> (usize, bool, &'static str) {
    let found: Vec<(usize, bool, &'static str)> = r11_immediates(bytes)
        .into_iter()
        .filter_map(|(at, immediate)| {
            call_helpers()
                .into_iter()
                .find(|(address, seam, _, _)| *address == immediate && *seam == consult)
                .map(|(_, _, linked, name)| (at, linked, name))
        })
        .collect();
    assert_eq!(
        found.len(),
        1,
        "the `call` site must emit exactly one seam-{} helper, found {found:?}",
        if consult { "consulting" } else { "skipping" }
    );
    found[0]
}

/// Decode the `count` `movabs` immediates that end exactly at `end`, in argument order
/// (`rsi`, `rdx`, `rcx`, `r8`, `r9`), answering them and the offset they start at.
fn helper_args_before(bytes: &[u8], end: usize, count: usize) -> (Vec<u64>, usize) {
    /// The five integer argument registers `emit_helper_invoke` fills, in ABI order.
    const ARG_OPCODES: [[u8; 2]; 5] = [
        [0x48, 0xBE], // rsi
        [0x48, 0xBA], // rdx
        [0x48, 0xB9], // rcx
        [0x49, 0xB8], // r8
        [0x49, 0xB9], // r9
    ];
    let mut start = end;
    let mut args = vec![0_u64; count];
    for slot in (0..count).rev() {
        assert!(
            start >= 10,
            "the helper's {count} argument immediates must precede offset {end}"
        );
        let at = start - 10;
        assert_eq!(
            [bytes[at], bytes[at + 1]],
            ARG_OPCODES[slot],
            "argument {slot} of a {count}-argument helper call must be a `movabs` to its own \
             register"
        );
        let mut word = [0_u8; 8];
        word.copy_from_slice(&bytes[at + 2..at + 10]);
        args[slot] = u64::from_le_bytes(word);
        start = at;
    }
    (args, start)
}

/// **The `Instr::Call` reject arm keeps its `arity` argument; the accepted arm drops it.**
///
/// The reject arm is the unchanged helper call, so it must still carry the interpreter's
/// own five arguments `[func, pc, callee, callee_pc, arity]`; the accepted arm skips the
/// seam, which is the only consumer of `arity`, so it carries four. Both arms call the
/// *linked* helper (`f` and `g` are separate functions in this artifact).
#[test]
fn the_call_site_arms_differ_only_by_the_arity_argument() {
    let module = gated_call_module();
    let labels = crate::interpreter::build_label_map(&module);
    let compiled = crate::jit::baseline::emit::compile_module(&module)
        .expect("the two-function fixture compiles eagerly");
    let (start, end) = compiled.code_address_range();
    assert_eq!(
        end - start,
        compiled.code_len(),
        "the artifact's address range is its emitted byte count"
    );
    // SAFETY: the artifact's sealed mapping is live and readable for `code_len()` bytes,
    // and the slice borrows a mapping this test keeps alive to the end of the scope.
    let bytes = unsafe { std::slice::from_raw_parts(start as *const u8, compiled.code_len()) }
        .to_vec();

    let call_pc = module.functions[0]
        .code
        .iter()
        .position(|instr| matches!(instr, Instr::Call { .. }))
        .expect("the fixture carries one frame-preserving `call`") as u32;
    let (callee, callee_pc) = labels
        .resolve(Label(3))
        .expect("the fixture's `call` names its callee by label");

    let (reject_at, reject_linked, reject_name) = single_helper_site(&bytes, true);
    let (accepted_at, accepted_linked, accepted_name) = single_helper_site(&bytes, false);
    assert!(
        reject_at < accepted_at,
        "the reject arm ({reject_name}) precedes the accepted arm ({accepted_name})"
    );
    assert_eq!(
        reject_linked, accepted_linked,
        "both arms choose the same linked-ness (a compile-time `linker.is_compiled` decision): \
         got {reject_name} and {accepted_name}"
    );

    // The reject arm: five arguments, `arity` last.
    let (args, args_start) = helper_args_before(&bytes, reject_at, 5);
    assert_eq!(
        args,
        vec![0, call_pc as u64, callee as u64, callee_pc as u64, 1],
        "the reject arm passes the interpreter's own [func, pc, callee, callee_pc, arity]"
    );
    // The argument block is preceded by the `rt` load from the block's own stack slot:
    // `mov <reg>, [rsp + 0]` (a `mod=10` disp32 ModRM with `rm = 100` and the `rsp` SIB).
    let rt_load = &bytes[args_start - 8..args_start];
    assert_eq!(
        (rt_load[0], rt_load[1], rt_load[3], &rt_load[4..8]),
        (0x48, 0x8B, 0x24, &[0u8, 0, 0, 0][..]),
        "the argument block is preceded by the `rt` load from the block's own stack slot, \
         got {rt_load:?}"
    );
    assert_eq!(
        rt_load[2] >> 6,
        0b10,
        "the `rt` load addresses [rsp+disp32], got ModRM {:#04x}",
        rt_load[2]
    );
    assert_eq!(
        rt_load[2] & 0b111,
        0b100,
        "the `rt` load's ModRM carries the `rsp` SIB byte, got ModRM {:#04x}",
        rt_load[2]
    );

    // The accepted arm: four arguments — no `arity` remaining, and nothing before them
    // that could be a fifth.
    let (args, args_start) = helper_args_before(&bytes, accepted_at, 4);
    assert_eq!(
        args,
        vec![0, call_pc as u64, callee as u64, callee_pc as u64],
        "the accepted arm carries only what the store + transfer halves need"
    );
    assert_ne!(
        bytes[args_start - 2..args_start],
        [0x49, 0xB9],
        "the accepted arm emits no fifth (`r9`) argument immediate"
    );

    // Both arms are followed by the same `movabs r11, CALL_STORED` / `cmp rax, r11` / `jcc`,
    // which is what makes the emitted exit after them shared.
    // Each arm's post-call discriminator is located by scanning for the
    // `movabs r11, CALL_STORED` word rather than by assuming a fixed offset: the
    // helper address is `movabs r11` + `call r11`, and the encoder's argument
    // loads vary with the register cache.
    for (site, name) in [(reject_at, reject_name), (accepted_at, accepted_name)] {
        let window = &bytes[site..(site + 64).min(bytes.len())];
        let stored = CALL_STORED.to_le_bytes();
        let word_at = window
            .windows(10)
            .position(|w| w[..2] == [0x49, 0xBB] && w[2..] == stored)
            .unwrap_or_else(|| {
                panic!(
                    "the {name} arm emits a `movabs r11, CALL_STORED` after its helper call, got {window:?}"
                )
            });
        // `cmp r/m64, r64` (`0x39 /r`) with ModRM `0xD8` is `cmp rax, r11`; the REX
        // prefix carries `W=1` and `R=1` only when the encoder needs the extended
        // register, so the R bit is not part of the invariant.
        assert_eq!(
            window[word_at + 11..word_at + 13],
            [0x39, 0xD8][..],
            "the {name} arm's `CALL_STORED` is followed by `cmp rax, r11`"
        );
        let rex = window[word_at + 10];
        assert_eq!(
            rex & 0b1111_1011,
            0x48,
            "the {name} arm's `cmp` is a 64-bit form (REX.W), got {rex:#04x}"
        );
    }
}

/// The gate is emitted **in front of** both arms at the `Instr::Call` site: the snippet
/// from the block entry to the reject helper contains the gate's own reads, so a `call`
/// whose verdict is memoised never reaches either helper — the property the accepted arm
/// exists for.
#[test]
fn the_call_site_emits_the_gate_before_both_arms() {
    let module = gated_call_module();
    let compiled = crate::jit::baseline::emit::compile_module(&module)
        .expect("the two-function fixture compiles eagerly");
    let (start, end) = compiled.code_address_range();
    assert_eq!(end - start, compiled.code_len());
    // SAFETY: as above — a live, readable sealed mapping of exactly `code_len()` bytes.
    let bytes = unsafe { std::slice::from_raw_parts(start as *const u8, compiled.code_len()) };

    let (reject_at, _, reject_name) = single_helper_site(bytes, true);
    // The gate's five reject conditions are five conditional branches, and every one of
    // them precedes the reject-arm helper (the gate is emitted before either arm).
    let jccs = bytes[..reject_at]
        .windows(2)
        .filter(|window| window[0] == 0x0F && window[1] & 0xF0 == 0x80)
        .count();
    assert!(
        jccs >= 5,
        "the gate's five reject branches ({jccs} found) precede the reject helper {reject_name}"
    );
}
