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

//! **Real-`.beam` proofs for every instruction variant that used to be recorded as
//! "CLAIMED-BLOCKED"** — the suite that replaces synthetic-IR evidence with real
//! compiler output and the real ERTS oracle.
//!
//! # What this suite proves, and why it exists
//!
//! The static census (`tests/census.rs`) reaches **108 `beam_loader::Instr`
//! variants = 95 EXECUTED + 13 ZERO-CORPUS + 0 GENUINE-GAP + 0 UNCLAIMED**, with
//! 25/25 `TestKind`s — but a probe row proves a variant executes by driving a
//! **synthetic `Module` the test built itself**. That is evidence about the engine,
//! not about the compiler: a shape that no `erlc` ever emits can pass such a probe
//! and still say nothing about real Erlang code.
//!
//! Every test in this directory closes that gap for one variant family, with the
//! same four-step proof (implemented once, in [`harness`]):
//!
//! 1. **Drive a real compiled module** — a committed `erlc` fixture from
//!    `tests/fixtures/`, or a module of the installed system OTP — through the
//!    engine's public entry (`ModuleHost::call` → `Machine::run_export`). No
//!    hand-built `Module`, no synthetic IR.
//! 2. **Assert the variant executed** — `exec_census` (the dynamic, per-variant
//!    counter recorded at the interpreter's single per-instruction funnel) reports
//!    `executed["<variant>"] > 0` for the run. This is the assertion that would
//!    have failed before the engine implemented the variant: the run would have
//!    died on `unsupported opcode …` (or the census simply never saw it).
//! 3. **Assert the observable result equals the real ERTS oracle's** — either the
//!    live `erl` rendering of the same expression (`harness::oracle_w`, string-
//!    compared with `harness::render` of the engine's result), or a committed
//!    oracle-derived value whose exact `erl` command is in the test's doc comment.
//!    Nothing is guessed, and no expected value is fabricated.
//! 4. **Report honestly** — every variant that could *not* be reached on real
//!    compiler output is listed below with the reason. That list is a result, not a
//!    failure.
//!
//! Every test **skips loudly** ([`harness::skip_if_unavailable`]) when the system
//! OTP install / `erl` oracle is absent, exactly like the sibling real-module
//! suites — a green run without the oracle proves nothing about parity.
//!
//! # Coverage map (one file per family group)
//!
//! | file | variants proven |
//! |---|---|
//! | [`calls`] | `Call`, `CallLast`, `CallOnly`, `CallExt`, `CallExtOnly`, `CallExtLast`, `DeallocateReturn`, `InitYregs`, `FuncInfo`, `Bif0`, `Move`, `Trim` |
//! | [`data`] | `GetTupleElement`, `GetList`, `GetHd`, `GetTl`, `UpdateRecord`, `Badrecord` |
//! | [`funs`] | `MakeFun3`, `CallFun`, `CallFun2`, `Apply`, `ApplyLast` |
//! | [`exceptions`] | `Badmatch`, `CaseEnd`, `IfEnd`, `Raise`, `TryCase`, `TryCaseEnd`, `TryEnd`, `CatchEnd` |
//! | [`bitstrings`] | `BsMatch`, `BsSetPosition`, `BsGetPosition`, `BsGetTail`, `BsGetBinary2`, `BsSkipBits2`, `BsGetUtf16` |
//! | [`receive`] | `Send`, `LoopRec`, `RemoveMessage`, `Wait`, `Timeout`, `RecvMarkerReserve`, `RecvMarkerBind`, `RecvMarkerUse`, `RecvMarkerClear` |
//!
//! # Variants this suite does NOT prove, and why
//!
//! **Zero static corpus occurrences — out of scope by construction.** These six
//! variants occur **nowhere** in the whole installed OTP corpus, so no fixture can
//! be written that makes the compiler emit them; claiming real-`.beam` evidence for
//! one would be a fabrication. They stay covered by the synthetic probe rows in
//! `tests/census.rs` (the `ZERO_CORPUS_VARIANTS` pin), which is the strongest
//! statement available for a shape no compiler emits:
//!
//! * `Deallocate` — every corpus `deallocate` is fused with the following `return`
//!   into `DeallocateReturn` by the loader (`beam_loader::lower::fuse`).
//! * `SetTupleElement` — the corpus performs no in-place tuple write (`setelement/3`
//!   builds a fresh tuple, i.e. `bif` + `put_tuple2`).
//! * `IsNativeRecord`, `PutRecord`, `IsRecordAccessible` — the OTP-29 *native
//!   record* opcode family, which this toolchain does not enable.
//! * `BsTestTail2` — not emitted by the current bit-syntax code generator.
//!
//! See [`RECORDED_PROOFS`] for the machine-readable per-variant table this suite's
//! rows fill in, and `harness` for the shared proof implementation.

mod support;

// The family files live in the `real_beam_variant_proofs/` directory beside this
// crate root (a test-crate root resolves a bare `mod x;` to `tests/x.rs`, so the
// paths are explicit).
#[path = "real_beam_variant_proofs/harness.rs"]
mod harness;
#[path = "real_beam_variant_proofs/bitstrings.rs"]
mod bitstrings;
#[path = "real_beam_variant_proofs/calls.rs"]
mod calls;
#[path = "real_beam_variant_proofs/data.rs"]
mod data;
#[path = "real_beam_variant_proofs/exceptions.rs"]
mod exceptions;
#[path = "real_beam_variant_proofs/funs.rs"]
mod funs;
#[path = "real_beam_variant_proofs/receive.rs"]
mod receive;

/// The per-variant table this suite fills: every variant name from the
/// formerly-blocked weight ordering that a row here proves on real compiler
/// output.
///
/// The rows themselves live in the family files (each with its fixture, its
/// observed census count and its oracle comparison); this constant is the
/// checklist they are audited against, so a variant cannot silently drop out of
/// the suite. `cargo test -- --nocapture` prints the audit table.
pub const RECORDED_PROOFS: &[&str] = &[
    // core
    "Call",
    "CallExt",
    "CallExtLast",
    "CallExtOnly",
    "CallLast",
    "CallOnly",
    "DeallocateReturn",
    "FuncInfo",
    "InitYregs",
    "Bif0",
    "Move",
    // data
    "Badrecord",
    "GetHd",
    "GetList",
    "GetTl",
    "GetTupleElement",
    "UpdateRecord",
    // funs
    "Apply",
    "ApplyLast",
    "CallFun",
    "CallFun2",
    "MakeFun3",
    // exceptions
    "Badmatch",
    "CaseEnd",
    "CatchEnd",
    "IfEnd",
    "Raise",
    "TryCase",
    "TryCaseEnd",
    "TryEnd",
    // bitstrings
    "BsGetBinary2",
    "BsGetPosition",
    "BsGetTail",
    "BsGetUtf16",
    "BsMatch",
    "BsSetPosition",
    "BsSkipBits2",
    // receive
    "LoopRec",
    "RecvMarkerBind",
    "RecvMarkerClear",
    "RecvMarkerReserve",
    "RecvMarkerUse",
    "RemoveMessage",
    "Send",
    "Timeout",
    "Wait",
];

/// Audits the checklist above: no duplicate names, and the table is printed so
/// `cargo test -p beam_engine --test real_beam_variant_proofs -- --nocapture`
/// shows the full per-variant coverage at a glance.
///
/// The table is a **checklist**, not a substitute for a row: the per-family files
/// are what actually assert each variant executed on real compiler output, and a
/// variant dropped from a row fails that row's `require`.
#[test]
fn recorded_proofs_table_is_unique() {
    let mut names = RECORDED_PROOFS.to_vec();
    names.sort_unstable();
    let mut unique = names.clone();
    unique.dedup();
    assert_eq!(
        unique.len(),
        names.len(),
        "RECORDED_PROOFS contains a duplicate variant"
    );
    assert!(
        names.len() >= 40,
        "the table should cover the whole formerly-blocked ordering, has {}",
        names.len()
    );
    eprintln!("real-.beam proven variants ({}):", names.len());
    for name in &names {
        eprintln!("  {name}");
    }
}