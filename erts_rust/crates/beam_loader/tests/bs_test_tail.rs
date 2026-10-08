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

//! The **`bs_test_tail2`** generic opcode (121) — reachable from the system `erlc`
//! only when the `ssa_opt_bs_ensure` optimization is disabled.
//!
//! # Reachability
//!
//! By default `beam_ssa_opt`'s `ssa_opt_bs_ensure` pass annotates every
//! binary-matching block (`bs_ensure_opt => true`), and `beam_ssa_codegen` then runs
//! `bs_translate/1`, folding each `bs_test_tail` into a `bs_match` `test_tail` /
//! `ensure_exactly` command — so the generic `bs_test_tail2` opcode is *never* emitted
//! (verified by compiling a dozen bitstring patterns, none of which contained it).
//!
//! The `NoBsMatch` path (`beam_ssa_codegen.erl:133,224-227`) skips `bs_translate` when
//! that annotation is absent. Compiling with **`+no_ssa_opt`** (which disables the whole
//! `beam_ssa_opt` pass, `compile.erl:1999`) leaves the standalone
//! `bs_test_tail2 Fail Ctx Bits` instruction in the `.beam` — this is how the committed
//! fixture `tests/fixtures/bstail.beam` is produced.
//!
//! # What this proves
//!
//! The committed fixture lowers opcode 121 to the typed [`Instr::BsTestTail2`], never
//! to [`Instr::Generic`]. The assertions need no OTP install (the `.beam` is committed).

use beam_loader::{Instr, Label, Module, Opcode, Operand, NO_FAIL};

/// Loads the committed `bstail.beam` oracle fixture (`+no_ssa_opt`).
fn load_bstail() -> Module {
    beam_loader::load_file(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/bstail.beam"
    ))
    .expect("the bstail oracle fixture must load")
}

/// Every lowered instruction of `module`, in function/stream order.
fn instructions(module: &Module) -> Vec<&Instr> {
    module
        .functions
        .iter()
        .flat_map(|function| function.code.iter())
        .collect()
}

#[test]
fn bstail_lowers_bs_test_tail2_typed() {
    let module = load_bstail();

    let tails: Vec<(Label, Operand, u32)> = instructions(&module)
        .iter()
        .filter_map(|instr| match instr {
            Instr::BsTestTail2 { fail, ctx, bits } => Some((*fail, ctx.clone(), *bits)),
            _ => None,
        })
        .collect();
    assert!(
        !tails.is_empty(),
        "bstail.beam (compiled `+no_ssa_opt`) must contain `bs_test_tail2`"
    );

    let generic = instructions(&module)
        .iter()
        .filter(|instr| {
            matches!(instr, Instr::Generic { op, .. } if *op == Opcode::bs_test_tail2)
        })
        .count();
    assert_eq!(generic, 0, "`bs_test_tail2` must not lower to Instr::Generic");

    // The `<<X:8>>` pattern reads one byte and then requires exactly 0 bits to remain.
    let (fail, ctx, bits) = &tails[0];
    assert_ne!(*fail, NO_FAIL, "`bs_test_tail2` must carry a real failure label");
    assert_eq!(*ctx, Operand::X(1), "the match context is register X(1)");
    assert_eq!(*bits, 0, "`<<X:8>>` leaves exactly 0 bits to test");

    // The failure label must be a label of the function that contains the instruction.
    let f = module
        .function(module.atom_table.lookup("f").expect("`f` is interned"), 1)
        .expect("bstail defines f/1");
    assert!(
        f.labels.contains_key(fail),
        "the `bs_test_tail2` fail label {fail:?} must belong to f/1"
    );
}
