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
//! The **exception / catch-frame** group.
//!
//! Owns the 9 exception variants:
//!
//! `Badmatch, CaseEnd, IfEnd, TryCaseEnd, Badrecord, Raise` (the raising
//! opcodes) and `TryCase, TryEnd, CatchEnd` (the handler opcodes that consume a
//! catch frame).
//!
//! # Mechanic
//!
//! Every row states its claim through the **landed** exception rather than through
//! an uncaught one: a `Try { frame, fail }` pushed in `setup` gives the raise a live
//! catch frame, so the engine unwinds to the handler with C's `handle_error`
//! register convention (`x(0) = THE_NON_VALUE`, `x(1) = reason`, `x(2) = trace`,
//! `x(3) = class atom`) and the frame stays in place for the handler opcode. With
//! `fail = FAIL_SINK_LABEL` the landed reason is observed as
//! [`Expect::Branched`](super::super::Expect::Branched) (the fail sink returns the
//! distinct `probe_fail` mark, which no success path can produce).
//!
//! The frame slot is [`FRAME_SLOT`] (`X(9)`): `Instr::Try` only *writes* its frame
//! operand, and an x-register needs no `Allocate`, so no row here pushes a call
//! frame. The tag atoms (`badmatch`, `case_clause`, `if_clause`, `try_clause`,
//! `badrecord`) and the class atom (`error`) are all in the canonical
//! [`ProbeAtoms`](super::super::ProbeAtoms) universe, so the family resolves their
//! names through `ctx.atoms(module)` (whose default is the module's own table) —
//! no host hook is needed for any row.
//!
//! # Coverage note
//!
//! `catch_end` has a second, richer shape: a **landed** exception is folded into
//! the oracle's `{'EXIT',{Reason,Stacktrace}}` (or `{'EXIT',Reason}` / the thrown
//! term). One row per variant cannot express both, so this group probes the
//! pass-through path and the three-way fold is pinned by the dedicated suite
//! `tests/exceptions.rs` (plus the in-crate unit tests under
//! `src/families/exceptions/tests/`).

use beam_loader::{Instr, Operand};
use beam_term::{make_atom, make_small, NIL};

use super::super::{aux_label, Expect, Probe, ProbeAtoms, FAIL_SINK_LABEL};

/// The catch-frame register every row of this group uses (`X(9)`).
///
/// C stores the frame in a Y slot, but this harness deliberately avoids a Y operand
/// (that would need an `Allocate`): a `try` frame register is just a term slot, and
/// `X(9)` is outside every argument list here.
const FRAME_SLOT: u32 = 9;

/// The value the `Badmatch` row fails to match (`{badmatch,7}`).
const BADMATCH_VALUE: i64 = 7;
/// The value the `CaseEnd` row matched no clause against (`{case_clause,2}`).
const CASE_VALUE: i64 = 2;
/// The value the `TryCaseEnd` row re-raises (`{try_clause,4}`).
const TRY_VALUE: i64 = 4;
/// The value the `Badrecord` row rejects (`{badrecord,3}`).
const BADRECORD_VALUE: i64 = 3;
/// The value the `CatchEnd` row's normal path passes through unchanged.
const CATCH_VALUE: i64 = 7;
/// The value the `Raise` row's seeded raise fails to match (`{badmatch,1}`).
const RERAISE_SEED: i64 = 1;

/// The 9 exception rows (one per variant this group owns).
pub fn rows(atoms: &ProbeAtoms) -> Vec<Probe> {
    vec![
        badmatch_row(),
        case_end_row(),
        if_end_row(),
        try_case_end_row(),
        badrecord_row(),
        try_case_row(atoms),
        try_end_row(),
        catch_end_row(),
        raise_row(),
    ]
}

/// A prologue that arms one live catch frame whose handler is the fail sink.
///
/// The raise therefore lands in that frame and the run finishes at
/// [`FAIL_SINK_LABEL`] — observability without pinning a heap-built reason term.
fn armed_frame() -> Vec<Instr> {
    vec![Instr::Try {
        frame: Operand::X(FRAME_SLOT),
        fail: FAIL_SINK_LABEL,
    }]
}

/// `badmatch Value` — raise `{badmatch,Value}` (class `error`).
fn badmatch_row() -> Probe {
    Probe::new(
        Instr::Badmatch {
            value: Operand::Integer(BADMATCH_VALUE),
        },
        Expect::Branched,
    )
    .setup(armed_frame())
    .note("a live try frame catches {badmatch,7}: the reason lands in x(1) and the run ends at the fail sink")
}

/// `case_end Value` — raise `{case_clause,Value}` (class `error`).
fn case_end_row() -> Probe {
    Probe::new(
        Instr::CaseEnd {
            value: Operand::Integer(CASE_VALUE),
        },
        Expect::Branched,
    )
    .setup(armed_frame())
    .note("a live try frame catches {case_clause,2}: the reason lands in x(1) and the run ends at the fail sink")
}

/// `if_end` — raise the bare atom `if_clause` (class `error`).
fn if_end_row() -> Probe {
    Probe::new(Instr::IfEnd, Expect::Branched)
        .setup(armed_frame())
        .note("a live try frame catches the bare atom if_clause: it lands as x(1) and the run ends at the fail sink")
}

/// `try_case_end Value` — re-raise an unmatched try-clause value as
/// `{try_clause,Value}` (class `error`).
fn try_case_end_row() -> Probe {
    Probe::new(
        Instr::TryCaseEnd {
            value: Operand::Integer(TRY_VALUE),
        },
        Expect::Branched,
    )
    .setup(armed_frame())
    .note("a live try frame catches {try_clause,4}: the reason lands in x(1) and the run ends at the fail sink")
}

/// `badrecord Value` — raise `{badrecord,Value}` (class `error`).
fn badrecord_row() -> Probe {
    Probe::new(
        Instr::Badrecord {
            value: Operand::Integer(BADRECORD_VALUE),
        },
        Expect::Branched,
    )
    .setup(armed_frame())
    .note("a live try frame catches {badrecord,3}: the reason lands in x(1) and the run ends at the fail sink")
}

/// `try_case Frame` — the **exceptional** entry of a `try`: pop the frame, blank the
/// slot, require `x(0) == THE_NON_VALUE` and move the class atom from `x(3)` into
/// `x(0)`.
///
/// The prologue arms a frame whose handler is `aux_label(0)`*and* lands an exception
/// in it (`badmatch 1`), so the machine is in exactly the state the opcode defines:
/// the landed registers plus one live frame. The handler label sits immediately
/// before the variant, so execution continues into `try_case` as the compiled code
/// would.
fn try_case_row(atoms: &ProbeAtoms) -> Probe {
    Probe::new(
        Instr::TryCase {
            frame: Operand::X(FRAME_SLOT),
        },
        Expect::Returned(Some(make_atom(atoms.named("error")))),
    )
    .setup(vec![
        Instr::Try {
            frame: Operand::X(FRAME_SLOT),
            fail: aux_label(0),
        },
        Instr::Badmatch {
            value: Operand::Integer(RERAISE_SEED),
        },
        Instr::Label(aux_label(0)),
    ])
    .note("the landed {badmatch,1} leaves x(0)=THE_NON_VALUE and x(3)=error; try_case pops the frame and returns the class atom")
}

/// `try_end Frame` — the **normal** exit of a `try`: pop the frame and blank the slot.
///
/// Nothing in the prologue raises, so no register was written: the probe's registers
/// still hold their run-entry value (`NIL`), which the success sink returns.
fn try_end_row() -> Probe {
    Probe::new(
        Instr::TryEnd {
            frame: Operand::X(FRAME_SLOT),
        },
        Expect::Returned(Some(NIL)),
    )
    .setup(armed_frame())
    .note("try_end pops the armed frame, blanks x(9) and falls through; the untouched registers still hold NIL")
}

/// `catch_end Frame` — the **normal** result of an old-style `catch` passes through
/// unchanged (`x(0)` is not `THE_NON_VALUE`, so no fold happens).
fn catch_end_row() -> Probe {
    Probe::new(
        Instr::CatchEnd {
            frame: Operand::X(FRAME_SLOT),
        },
        Expect::Returned(Some(make_small(CATCH_VALUE))),
    )
    .setup(vec![
        Instr::Try {
            frame: Operand::X(FRAME_SLOT),
            fail: FAIL_SINK_LABEL,
        },
        Instr::Move {
            src: Operand::Integer(CATCH_VALUE),
            dst: Operand::X(0),
        },
    ])
    .note("catch_end pops the armed frame and passes the plain result in x(0)=7 through (no catch marker, so no 'EXIT' fold)")
}

/// `raise Trace Value` — re-raise with the class of the exception **being handled**
/// (`x(1)` = the landed reason, `x(2)` = the landed raw trace).
///
/// The prologue lands an exception in a frame handled by `aux_label(0)` (which arms
/// `pending_class` = `error`), then the handler's `try_case` consumes that frame
/// while leaving `x(1)`/`x(2)` set, and a fresh `try` arms a frame handled by the
/// fail sink for the re-raise to land in. A re-raise onto its own handler would loop
/// (the frame is never popped by a raise), hence the second frame.
fn raise_row() -> Probe {
    Probe::new(
        Instr::Raise {
            trace: Operand::X(2),
            value: Operand::X(1),
        },
        Expect::Branched,
    )
    .setup(vec![
        Instr::Try {
            frame: Operand::X(FRAME_SLOT),
            fail: aux_label(0),
        },
        Instr::Badmatch {
            value: Operand::Integer(RERAISE_SEED),
        },
        Instr::Label(aux_label(0)),
        Instr::TryCase {
            frame: Operand::X(FRAME_SLOT),
        },
        Instr::Try {
            frame: Operand::X(FRAME_SLOT),
            fail: FAIL_SINK_LABEL,
        },
    ])
    .note("raise re-raises x(1) with the landing's pending class error into a freshly armed frame, so the run ends at the fail sink")
}
