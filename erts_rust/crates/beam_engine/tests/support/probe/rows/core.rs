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
//! `src/interpreter.rs` — the control flow that has no instruction family:
//!
//! `FuncInfo, Call, CallLast, CallExt, CallExtOnly, CallExtLast, Bif0, CallFun,
//! CallFun2, Apply, ApplyLast, MakeFun3, InitYregs, Deallocate, DeallocateReturn`.
//!
//! Mechanic: a call stores its continuation in the **current** frame's CP slot and
//! a `return` reads it back, so a non-tail call row allocates a frame
//! (`Instr::Allocate`) in its prologue and pops it (`Instr::Deallocate`) in its
//! epilogue; the callee is an aux block (`aux_label(0)`) ending in `Instr::Return`.
//! A BIF/import row installs a `bif` hook (a `call_ext*`/`bif*`/`apply` route) and
//! pins the returned term.

use beam_loader::{HeapNeed, ImportRef, Instr, Operand};
use beam_term::make_small;

use super::super::{aux_label, Expect, Probe, ProbeAtoms};

/// The value an import-row's `bif` hook answers (`erlang:self/0`'s stand-in).
const BIF_VALUE: i64 = 7;
/// The value the `Call` row's callee leaf puts in `x(0)`.
const LEAF_VALUE: i64 = 42;

/// The `Call`/`CallExt` smoke rows: the two mechanics every other converted row
/// builds on (a local call through an aux leaf block, and an import dispatched to a
/// host `bif` hook).
pub fn rows(_atoms: &ProbeAtoms) -> Vec<Probe> {
    vec![call_row(), call_ext_row()]
}

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
    .setup(vec![Instr::Allocate {
        stack_need: HeapNeed::Words(0),
        live: 0,
    }])
    .epilogue(vec![Instr::Deallocate(0)])
    .block(vec![
        Instr::Label(aux_label(0)),
        Instr::Move {
            src: Operand::Integer(LEAF_VALUE),
            dst: Operand::X(0),
        },
        Instr::Return,
    ])
    .note("a local call stores its continuation in the current frame's CP slot; the aux leaf returns x(0)=42")
}

/// `call_ext Arity Bif` — an **import** call, dispatched through the host's
/// [`ExecutionContext::call_bif`](beam_engine::ExecutionContext::call_bif).
///
/// Import 0 of the generated probe module is always `erlang:self/0`, so this row
/// needs nothing but a `bif` hook and pins the hook's answer.
fn call_ext_row() -> Probe {
    Probe::new(
        Instr::CallExt {
            arity: 0,
            target: ImportRef(0),
        },
        Expect::Returned(Some(make_small(BIF_VALUE))),
    )
    .bif(Box::new(|module, function, arity, args| {
        let _ = (module, function, arity, args);
        Ok(make_small(BIF_VALUE))
    }))
    .note("import 0 is erlang:self/0; the bif hook answers 7")
}
