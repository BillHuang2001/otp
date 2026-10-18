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
//! The **core call / BIF / apply / frame** group.
//!
//! Owns the 15 `Instr` variants the interpreter executes as *core* arms of
//! `src/interpreter/dispatch.rs` — the control flow that has no instruction family:
//!
//! `FuncInfo, Call, CallLast, CallExt, CallExtOnly, CallExtLast, Bif0, CallFun,
//! CallFun2, Apply, ApplyLast, MakeFun3, InitYregs, Deallocate, DeallocateReturn`.
//!
//! Mechanic: a call stores its continuation in the **current** frame's CP slot and
//! a `return` reads it back, so a non-tail call row allocates a frame
//! (`Instr::Allocate`) in its prologue and pops it (`Instr::Deallocate`) in its
//! epilogue; the callee is an aux block (`aux_label(0)`) ending in `Instr::Return`.
//! A BIF/import row installs a **discriminating** `bif` hook (it serves exactly one
//! `module:function/arity` and rejects everything else), so the pinned value proves
//! *which* callee the instruction reached — not merely that some hook answered. The
//! `call_fun*` rows take the value-returning `apply_fun` fallback through an
//! `apply_hook` over a real `make_fun3`-built closure, and the `apply*` rows put the
//! `M`/`F` **atom terms** in `x(arity)`/`x(arity+1)` (the `fixed_apply` shape).

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_loader::{FunDescriptor, HeapNeed, ImportRef, Instr, Operand, TestKind};
use beam_term::{make_atom, make_small, Eterm, NIL};

use super::super::{aux_label, Expect, Probe, ProbeAtoms, FAIL_SINK_LABEL, SUCCESS_LABEL};

/// The value the `erlang:self/0` stand-in BIF answers (`ImportRef(0)` of every
/// generated probe module).
const BIF_VALUE: i64 = 7;
/// The value the `erlang:abs/1` stand-in BIF answers (the `apply`/`apply_last` rows).
const ABS_VALUE: i64 = 9;
/// The argument the `apply*` rows hand to `erlang:abs/1` in `x(0)`.
const ABS_ARG: i64 = 7;
/// The value the `Call`/`CallLast` local callee leaf puts in `x(0)`.
const LEAF_VALUE: i64 = 42;
/// The value the closure-apply hook answers for a well-formed `call_fun*`.
const APPLIED_VALUE: i64 = 42;
/// The value an `apply_hook` answers when the closure/arguments are **not** the
/// ones the row built (a distinct wrong value, so the census detail names it).
const MISAPPLIED_VALUE: i64 = -1;
/// The free variable the `make_fun3` rows capture.
const CAPTURED: i64 = 7;
/// The argument the `CallFun` row passes (replacing the captured value in `x(0)`).
const CALL_FUN_ARG: i64 = 10;
/// The argument the `CallFun2` row passes.
const CALL_FUN2_ARG: i64 = 11;
/// The `old_uniq` of the `FunT` descriptor every fun row declares.
const FUN_UNIQ: u32 = 0xCAFE;
/// The declared (total) arity of that descriptor: one argument plus the capture.
const FUN_ARITY: u32 = 2;

/// The 15 core rows.
pub fn rows(atoms: &ProbeAtoms) -> Vec<Probe> {
    vec![
        call_row(),
        call_last_row(),
        call_ext_row(),
        call_ext_only_row(),
        call_ext_last_row(),
        bif0_row(),
        call_fun_row(atoms),
        call_fun2_row(atoms),
        apply_row(atoms),
        apply_last_row(atoms),
        make_fun3_row(atoms),
        init_yregs_row(),
        deallocate_row(),
        deallocate_return_row(),
        func_info_row(atoms),
    ]
}

// ---------------------------------------------------------------------------
// Shared prologue/epilogue/aux-block builders
// ---------------------------------------------------------------------------

/// An `allocate` of a frame with `stack_words` Y slots (no heap request).
fn allocate(stack_words: u32) -> Instr {
    Instr::Allocate {
        stack_need: HeapNeed::Words(stack_words),
        live: 0,
    }
}

/// `move <integer> x(register)`.
fn move_int(value: i64, register: u32) -> Instr {
    Instr::Move {
        src: Operand::Integer(value),
        dst: Operand::X(register),
    }
}

/// `move <atom> x(register)` — how the `apply*` rows seed the `M`/`F` operands.
fn move_atom(atoms: &ProbeAtoms, name: &str, register: u32) -> Instr {
    Instr::Move {
        src: Operand::Atom(atoms.named(name)),
        dst: Operand::X(register),
    }
}

/// `move x(from) x(to)`.
fn move_x(from: u32, to: u32) -> Instr {
    Instr::Move {
        src: Operand::X(from),
        dst: Operand::X(to),
    }
}

/// The aux leaf block `aux_label(0)`: `move <value> x(0); return`.
///
/// The `return` reads whatever continuation the **current** frame holds, which is
/// why the local-call rows own a frame.
fn leaf_block(value: i64) -> Vec<Instr> {
    vec![
        Instr::Label(aux_label(0)),
        move_int(value, 0),
        Instr::Return,
    ]
}

/// The `FunT` descriptor every fun row declares: a one-argument lambda capturing
/// one free variable (`arity` 2, `num_free` 1 ⇒ the closure's header arity is 1),
/// whose body is the aux block [`lambda_block`] emits.
fn lambda_descriptor(atoms: &ProbeAtoms) -> FunDescriptor {
    FunDescriptor {
        function: atoms.named("lambda"),
        arity: FUN_ARITY,
        label: aux_label(0),
        index: 0,
        num_free: 1,
        uniq: FUN_UNIQ,
    }
}

/// The lambda body the descriptor's label names (never entered by these rows: the
/// closure is applied through the host's `apply_fun`, which answers directly).
fn lambda_block() -> Vec<Instr> {
    vec![
        Instr::Label(aux_label(0)),
        move_int(0, 0),
        Instr::Return,
    ]
}

// ---------------------------------------------------------------------------
// The discriminating BIF hook
// ---------------------------------------------------------------------------

/// A **discriminating** `bif` hook policy: it serves exactly one
/// `erlang:<function>/<arity>` callee — and, when `arg` is declared, only when the
/// single argument is that integer — answering `value`. Anything else is
/// [`BeamError::BadArg`], so a row that reaches the wrong callee (or the right one
/// with the wrong operands) contradicts its pinned expectation instead of quietly
/// "passing" on a total hook.
struct Serve {
    /// The function-name component of the one served callee.
    function: &'static str,
    /// The arity of the one served callee.
    arity: u32,
    /// The single argument the callee must be given, when it takes one.
    arg: Option<i64>,
    /// The small integer the served callee answers.
    value: i64,
}

impl Serve {
    /// Answer `(module, function, arity, args)` if and only if it is the declared
    /// callee called with the declared arguments.
    fn answer(&self, module: &str, function: &str, arity: u32, args: &[Eterm]) -> Result<Eterm> {
        let callee_matches = module == "erlang" && function == self.function && arity == self.arity;
        let args_match = match self.arg {
            None => args.is_empty(),
            Some(arg) => args.len() == 1 && args[0] == make_small(arg),
        };
        if callee_matches && args_match {
            Ok(make_small(self.value))
        } else {
            Err(BeamError::BadArg)
        }
    }
}

/// The hook that serves only `erlang:self/0` (`ImportRef(0)`).
fn self_hook() -> impl FnMut(&str, &str, u32, &[Eterm]) -> Result<Eterm> {
    move |module, function, arity, args| {
        Serve {
            function: "self",
            arity: 0,
            arg: None,
            value: BIF_VALUE,
        }
        .answer(module, function, arity, args)
    }
}

/// The hook that serves only `erlang:abs/1`, and only for the declared argument.
fn abs_hook() -> impl FnMut(&str, &str, u32, &[Eterm]) -> Result<Eterm> {
    move |module, function, arity, args| {
        Serve {
            function: "abs",
            arity: 1,
            arg: Some(ABS_ARG),
            value: ABS_VALUE,
        }
        .answer(module, function, arity, args)
    }
}

// ---------------------------------------------------------------------------
// Local calls
// ---------------------------------------------------------------------------

/// `call Arity Label` — a **local** non-tail call.
///
/// The callee is the aux leaf `[move 42 x(0); return]`; `return` reads the
/// continuation `call` stored in the **current** frame's CP slot, so the prologue
/// allocates that frame (`Instr::Return` only *reads* it — without the
/// `Allocate`/`Deallocate` pair there is no frame to store into) and the epilogue
/// pops it again before the success sink's own `return` finishes the run on the
/// bottom frame's `Halt`.
fn call_row() -> Probe {
    Probe::new(
        Instr::Call {
            arity: 0,
            target: aux_label(0),
        },
        Expect::Returned(Some(make_small(LEAF_VALUE))),
    )
    .setup(vec![allocate(0)])
    .epilogue(vec![Instr::Deallocate(0)])
    .block(leaf_block(LEAF_VALUE))
    .note("a local call stores its continuation in the current frame's CP slot; the aux leaf returns x(0)=42")
}

/// `call_last Arity Label Deallocate` — a local **tail** call.
///
/// The frame is popped while *entering* the callee (C's `$deallocate(D);
/// $DISPATCH_REL`), so the aux leaf's `return` follows the frame that is current
/// after the pop — the run's bottom frame, whose continuation is `Halt` — and the
/// leaf's `x(0)` is the run's result. The prologue's `allocate` is what gives the
/// pop something of the declared size to pop; storing nothing is the whole point of
/// the `call_last` shape (a `Call`-style continuation would never be read).
fn call_last_row() -> Probe {
    Probe::new(
        Instr::CallLast {
            arity: 0,
            target: aux_label(0),
            deallocate: 0,
        },
        Expect::Returned(Some(make_small(LEAF_VALUE))),
    )
    .setup(vec![allocate(0)])
    .block(leaf_block(LEAF_VALUE))
    .note("call_last pops its frame on entry; the tail callee's return follows the caller's Halt continuation, x(0)=42")
}

// ---------------------------------------------------------------------------
// Import (`call_ext*`) and `bif0` dispatches
// ---------------------------------------------------------------------------

/// `call_ext Arity Bif` — an **import** call, dispatched through the host's
/// [`ExecutionContext::call_bif`](beam_engine::ExecutionContext::call_bif).
///
/// Import 0 of the generated probe module is always `erlang:self/0`, so this row
/// needs a `bif` hook that serves exactly that callee plus the pinned answer.
fn call_ext_row() -> Probe {
    Probe::new(
        Instr::CallExt {
            arity: 0,
            target: ImportRef(0),
        },
        Expect::Returned(Some(make_small(BIF_VALUE))),
    )
    .bif(Box::new(self_hook()))
    .note("import 0 is erlang:self/0; the bif hook serves only that callee and answers 7")
}

/// `call_ext_only Arity Bif` — an **import tail** call.
///
/// Nothing is stored: the callee's completion follows the frame's *existing*
/// continuation (the run's `Halt`), so the pinned value is the import's answer.
fn call_ext_only_row() -> Probe {
    Probe::new(
        Instr::CallExtOnly {
            arity: 0,
            target: ImportRef(0),
        },
        Expect::Returned(Some(make_small(BIF_VALUE))),
    )
    .bif(Box::new(self_hook()))
    .note("call_ext_only stores no continuation; the served erlang:self/0 answer (7) ends the run")
}

/// `call_ext_last Arity Bif Deallocate` — an import tail call that pops its frame
/// on the **success** path only (C's `i_call_ext_last` is `call_light_bif` |
/// `deallocate_return D`), so the prologue owns a frame of exactly that size.
fn call_ext_last_row() -> Probe {
    Probe::new(
        Instr::CallExtLast {
            arity: 0,
            target: ImportRef(0),
            deallocate: 0,
        },
        Expect::Returned(Some(make_small(BIF_VALUE))),
    )
    .setup(vec![allocate(0)])
    .bif(Box::new(self_hook()))
    .note("call_ext_last pops its frame after the served erlang:self/0 answers 7, then follows the Halt continuation")
}

/// `bif0 Bif Dst` — a non-garbage-collecting import call with no arguments and no
/// fail label (so a callee failure would be a raise, never a branch).
fn bif0_row() -> Probe {
    Probe::new(
        Instr::Bif0 {
            bif: ImportRef(0),
            dst: Operand::X(0),
        },
        Expect::Returned(Some(make_small(BIF_VALUE))),
    )
    .bif(Box::new(self_hook()))
    .note("bif0 stores the served erlang:self/0 answer (7) straight into x(0)")
}

// ---------------------------------------------------------------------------
// Closures: make_fun3 + call_fun/call_fun2
// ---------------------------------------------------------------------------

/// `call_fun Arity` — call the closure in `x(arity)` with the arguments in
/// `x(0..arity)`.
///
/// `make_fun3` first builds a real one-argument closure (header arity 1, one free
/// variable) so the shape check passes, then `x(0)` is overwritten with the call's
/// own argument. The host's `resolve_local_fun` seam declines (this context owns no
/// module registry), so the **value-returning** `apply_fun` fallback answers — and
/// the hook verifies it was handed the closure the row built (uniq/index/arity/
/// capture) plus the declared argument before answering 42.
fn call_fun_row(atoms: &ProbeAtoms) -> Probe {
    Probe::new(
        Instr::CallFun { arity: 1 },
        Expect::Returned(Some(make_small(APPLIED_VALUE))),
    )
    .setup(vec![
        allocate(0),
        move_int(CAPTURED, 0),
        Instr::MakeFun3 {
            index: 0,
            dst: Operand::X(1),
            env: vec![Operand::X(0)],
        },
        move_int(CALL_FUN_ARG, 0),
    ])
    .epilogue(vec![Instr::Deallocate(0)])
    .funs(vec![lambda_descriptor(atoms)])
    .apply_hook(Box::new(move |fun, args| {
        let well_formed = terms::is_fun(fun)
            && terms::fun_arity(fun).ok() == Some(1)
            && terms::fun_uniq(fun).ok() == Some(FUN_UNIQ as u64)
            && terms::fun_index(fun).ok() == Some(0)
            && terms::fun_free(fun).ok() == Some(&[make_small(CAPTURED)][..])
            && args == [make_small(CALL_FUN_ARG)];
        Ok(make_small(if well_formed {
            APPLIED_VALUE
        } else {
            MISAPPLIED_VALUE
        }))
    }))
    .block(lambda_block())
    .note("x(1) holds the make_fun3 closure (uniq 0xCAFE, index 0, arity 1, capture [7]); the apply_fun fallback answers 42")
}

/// `call_fun2 Tag Arity Func` — call the closure held in the `Func` operand.
///
/// The `Tag` hint is the compiler's knowledge (`safe`/`unsafe`, or a `FunT` index)
/// and is ignored by the engine, so the row spells the `unsafe` atom and pins that
/// the *operand* register is what is called.
fn call_fun2_row(atoms: &ProbeAtoms) -> Probe {
    Probe::new(
        Instr::CallFun2 {
            tag: Operand::Atom(atoms.named("unsafe")),
            arity: 1,
            func: Operand::X(2),
        },
        Expect::Returned(Some(make_small(APPLIED_VALUE))),
    )
    .setup(vec![
        allocate(0),
        move_int(CAPTURED, 0),
        Instr::MakeFun3 {
            index: 0,
            dst: Operand::X(2),
            env: vec![Operand::X(0)],
        },
        move_int(CALL_FUN2_ARG, 0),
    ])
    .epilogue(vec![Instr::Deallocate(0)])
    .funs(vec![lambda_descriptor(atoms)])
    .apply_hook(Box::new(move |fun, args| {
        let well_formed = terms::is_fun(fun)
            && terms::fun_arity(fun).ok() == Some(1)
            && args == [make_small(CALL_FUN2_ARG)];
        Ok(make_small(if well_formed {
            APPLIED_VALUE
        } else {
            MISAPPLIED_VALUE
        }))
    }))
    .block(lambda_block())
    .note("call_fun2 calls the closure in x(2) (the `unsafe` tag is a hint); the apply_fun fallback answers 42")
}

/// `make_fun3 Index Dst Env` — build the closure described by `FunT` entry 0.
///
/// The result is a heap `ErlangFun` block, which no `Expect` can pin as a value, so
/// the row declares the **shape** (`returns`) and names the exact closure the block
/// must carry in its note; the census renders it as `<fun>`.
fn make_fun3_row(atoms: &ProbeAtoms) -> Probe {
    Probe::new(
        Instr::MakeFun3 {
            index: 0,
            dst: Operand::X(1),
            env: vec![Operand::X(0)],
        },
        Expect::Returned(None),
    )
    .setup(vec![move_int(CAPTURED, 0)])
    .epilogue(vec![move_x(1, 0)])
    .funs(vec![lambda_descriptor(atoms)])
    .block(lambda_block())
    .note("builds the FunT closure <fun> (uniq 0xCAFE, index 0, header arity 1, num_free 1, free [7]) in x(1)")
}

// ---------------------------------------------------------------------------
// `apply` / `apply_last`
// ---------------------------------------------------------------------------

/// The `fixed_apply` prologue: the argument in `x(0)`, the module atom in
/// `x(arity)` and the function atom in `x(arity+1)` (for `arity` = 1).
fn fixed_apply_prologue(atoms: &ProbeAtoms) -> Vec<Instr> {
    vec![
        move_int(ABS_ARG, 0),
        move_atom(atoms, "erlang", 1),
        move_atom(atoms, "abs", 2),
    ]
}

/// `apply Arity` — apply the `M`/`F` **atom terms** in `x(arity)`/`x(arity+1)` to the
/// arguments in `x(0..arity)`.
///
/// The target atoms are real (they are in the canonical probe universe), so the row
/// reaches the `fixed_apply` dispatch; the host's cross-module seam declines and the
/// call lands on the synchronous `call_bif` route, which the discriminating hook
/// serves only for `erlang:abs/1` **with the declared argument**.
fn apply_row(atoms: &ProbeAtoms) -> Probe {
    Probe::new(
        Instr::Apply { arity: 1 },
        Expect::Returned(Some(make_small(ABS_VALUE))),
    )
    .setup(fixed_apply_prologue(atoms))
    .bif(Box::new(abs_hook()))
    .note("apply resolves M=erlang, F=abs from x(1)/x(2) and dispatches erlang:abs(7) to the bif hook, which answers 9")
}

/// `apply_last Arity Deallocate` — the tail form: the frame is popped **before** the
/// dispatch, and a synchronous result then follows the popped frame's continuation.
fn apply_last_row(atoms: &ProbeAtoms) -> Probe {
    let mut setup = vec![allocate(0)];
    setup.extend(fixed_apply_prologue(atoms));
    Probe::new(
        Instr::ApplyLast {
            arity: 1,
            deallocate: 0,
        },
        Expect::Returned(Some(make_small(ABS_VALUE))),
    )
    .setup(setup)
    .bif(Box::new(abs_hook()))
    .note("apply_last pops its frame then dispatches erlang:abs(7) through the bif hook (9) and follows the Halt continuation")
}

// ---------------------------------------------------------------------------
// Frame instructions
// ---------------------------------------------------------------------------

/// `init_yregs {Yregs}` — initialise the listed Y slots to `[]` (`make_blank`).
///
/// Emitted right after `allocate`, so the prologue owns a one-slot frame; the
/// epilogue reads that slot back into `x(0)` to pin the blank value.
fn init_yregs_row() -> Probe {
    Probe::new(
        Instr::InitYregs {
            registers: vec![Operand::Y(0)],
        },
        Expect::Returned(Some(NIL)),
    )
    .setup(vec![allocate(1)])
    .epilogue(vec![Instr::Move {
        src: Operand::Y(0),
        dst: Operand::X(0),
    }])
    .note("init_yregs blanks Y(0) to [] (NIL), read back into x(0)")
}

/// `deallocate N` — pop the innermost frame after checking it owns `N` Y slots.
///
/// The run continues on the frame that is then current; the success sink's `return`
/// finishes it on the bottom frame's `Halt` with the value the prologue left in
/// `x(0)`, which proves the pop (and only the pop) happened.
fn deallocate_row() -> Probe {
    Probe::new(
        Instr::Deallocate(0),
        Expect::Returned(Some(make_small(LEAF_VALUE))),
    )
    .setup(vec![allocate(0), move_int(LEAF_VALUE, 0)])
    .note("deallocate(0) pops the empty frame the prologue allocated and the run returns x(0)=42")
}

/// `deallocate N; return` — the loader's fused superinstruction (`lower::fuse`),
/// exactly the two constituents in order.
fn deallocate_return_row() -> Probe {
    Probe::new(
        Instr::DeallocateReturn(1),
        Expect::Returned(Some(make_small(LEAF_VALUE))),
    )
    .setup(vec![allocate(1), move_int(LEAF_VALUE, 0)])
    .note("deallocate_return(1) pops the one-slot frame and returns through its Halt continuation, x(0)=42")
}

/// `func_info Module Function Arity` — C's `i_func_info_IaaI`, which the compiler
/// only ever reaches by a **jump from a failed head clause** and which therefore
/// unconditionally raises class `error` with the bare atom `function_clause`.
///
/// # Why the expectation is a landed frame, not `Expect::Raises`
///
/// [`Expect::Raises`] compares the raised **class** and the reason **atom name**, and
/// it reads that name off the raise's
/// [`ExitReason`](beam_common::ExitReason): an *uncaught* engine raise always leaves
/// the machine as `ExitReason::Structured`, with the reason **term** riding
/// [`Raise::live_reason`](beam_common::Raise::live_reason) — so an uncaught
/// `function_clause` reports the reason name `<structured>`, and no row can pin the
/// atom that way. This row therefore observes the same raise from a **live catch
/// frame**, which is also what the oracle's own `try … catch C:R -> {C,R}` sees: the
/// handler `try_case`s the landing (so `x(0)` = the class atom, `x(1)` = the reason),
/// then *guards both* — the reason must be exactly `function_clause` and the class
/// exactly `error`, else the run takes the distinct fail sink — before moving the
/// reason atom into `x(0)` as the pinned result.
fn func_info_row(atoms: &ProbeAtoms) -> Probe {
    let reason = Operand::Atom(atoms.named("function_clause"));
    let class = Operand::Atom(atoms.named("error"));
    Probe::new(
        Instr::FuncInfo {
            module: atoms.module(),
            function: atoms.function(),
            arity: 1,
        },
        Expect::Returned(Some(make_atom(atoms.named("function_clause")))),
    )
    .function_arity(1)
    .setup(vec![
        allocate(1),
        Instr::Try {
            frame: Operand::Y(0),
            fail: aux_label(0),
        },
    ])
    .block(vec![
        Instr::Label(aux_label(0)),
        // `try_case` pops the catch frame and moves the landed class atom into x(0)
        // (x(1) keeps the reason, x(3) the class).
        Instr::TryCase {
            frame: Operand::Y(0),
        },
        // Guard BOTH halves of the raise: the reason atom, then the class atom.
        Instr::Test {
            kind: TestKind::IsEqExact,
            fail: FAIL_SINK_LABEL,
            args: vec![Operand::X(1), reason],
        },
        Instr::Test {
            kind: TestKind::IsEqExact,
            fail: FAIL_SINK_LABEL,
            args: vec![Operand::X(3), class],
        },
        // The pinned result: the reason atom the raise actually carried.
        move_x(1, 0),
        Instr::Jump {
            target: SUCCESS_LABEL,
        },
    ])
    .note("func_info raises error:function_clause; a live catch frame lands it and the guards pin both the reason and the class")
}
