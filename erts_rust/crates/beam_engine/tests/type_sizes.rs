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

//! Frozen IR memory-layout guards, observed from the **public** types.
//!
//! The interpreter's dispatch loop walks a `Vec` of typed IR instructions, so the
//! per-instruction footprint is a cache-line budget, not an implementation detail.
//! The *authoritative* pins on that budget are the compile-time assertions in the
//! loader crate (`beam_loader/src/instr.rs`):
//!
//! ```ignore
//! const _: () = assert!(core::mem::size_of::<Instr>()      <= 64);
//! const _: () = assert!(core::mem::size_of::<Operand>()    <= 16);
//! const _: () = assert!(core::mem::size_of::<HeapNeed>()   <= 24);
//! ```
//!
//! Those `const` blocks are what actually enforce the invariant **at build time**
//! (a re-inflated enum fails the build); this test is the *runtime observation* that
//! the same public types `beam_engine` consumes still satisfy the budget, so the
//! engine's dependents see the widths they were designed against. It is deliberately
//! a `<=` check, mirroring the guards: a smaller layout is never an error.
//!
//! Measured sizes on this target (x86-64): `Instr` = 64, `Operand` = 16,
//! `HeapNeed` = 24 — all within the 64 / 16 / 24 budget. Run with
//! `cargo test -p beam_engine --test type_sizes -- --nocapture` to print them.

use beam_loader::{HeapNeed, Instr, Operand};

/// The frozen size budget the loader's compile-time guards pin.
const INSTR_BUDGET: usize = 64;
const OPERAND_BUDGET: usize = 16;
const HEAP_NEED_BUDGET: usize = 24;

/// The public IR types still fit the frozen size budget the loader pins.
#[test]
fn public_ir_types_fit_the_frozen_size_budget() {
    let instr = core::mem::size_of::<Instr>();
    let operand = core::mem::size_of::<Operand>();
    let heap_need = core::mem::size_of::<HeapNeed>();

    println!("beam_loader::Instr    = {instr} bytes (budget {INSTR_BUDGET})");
    println!("beam_loader::Operand  = {operand} bytes (budget {OPERAND_BUDGET})");
    println!("beam_loader::HeapNeed = {heap_need} bytes (budget {HEAP_NEED_BUDGET})");

    assert!(
        instr <= INSTR_BUDGET,
        "beam_loader::Instr is {instr} bytes, over the {INSTR_BUDGET}-byte budget \
         pinned by beam_loader/src/instr.rs"
    );
    assert!(
        operand <= OPERAND_BUDGET,
        "beam_loader::Operand is {operand} bytes, over the {OPERAND_BUDGET}-byte budget \
         pinned by beam_loader/src/instr.rs"
    );
    assert!(
        heap_need <= HEAP_NEED_BUDGET,
        "beam_loader::HeapNeed is {heap_need} bytes, over the {HEAP_NEED_BUDGET}-byte budget \
         pinned by beam_loader/src/instr.rs"
    );
}
