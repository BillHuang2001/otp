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

//! The **line-table opcodes**: `executable_line` (183) and `debug_line` (184).
//!
//! Both are emitted only under a compiler flag, so the two committed oracle fixtures
//! pin them down with real `.beam` files produced by the system `erlc`:
//!
//! * `tests/fixtures/linecov.beam` — `linecov.erl` compiled with `+line_coverage`;
//!   the compiler emits an `executable_line/2` at every executable source line (the
//!   information `cover`/`eunit` need to map an instruction index back to a line).
//! * `tests/fixtures/dbgline.beam` — `dbgline.erl` compiled with `+beam_debug_info`;
//!   the compiler emits a `debug_line/4` marker (the debugger/`beam_debug_info`
//!   breakpoint location, carrying the live-register count).
//!
//! # What this proves
//!
//! * Each opcode lowers to its **typed** variant ([`Instr::ExecutableLine`] /
//!   [`Instr::DebugLine`]) and never to the [`Instr::Generic`] fallback.
//! * The `Line` table (`Module::line`) is **non-trivial** — it carries real file names
//!   and real line items — and every `executable_line`/`debug_line`/`line` location
//!   resolves inside it, which is what the runtime needs for `current_location` and
//!   for building stack traces.
//! * Every function's [`Instr::FuncInfo`] identity (module / function / arity) is
//!   preserved, so a stack-trace frame can name the running function.
//!
//! These assertions need no OTP install (the `.beam` files are committed), so the test
//! never skips.

use beam_loader::{Instr, LineRef, Module, Opcode};

/// Loads the committed `linecov.beam` oracle fixture (`+line_coverage`).
fn load_linecov() -> Module {
    beam_loader::load_file(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/linecov.beam"
    ))
    .expect("the linecov oracle fixture must load")
}

/// Loads the committed `dbgline.beam` oracle fixture (`+beam_debug_info`).
fn load_dbgline() -> Module {
    beam_loader::load_file(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/dbgline.beam"
    ))
    .expect("the dbgline oracle fixture must load")
}

/// Every lowered instruction of `module`, in function/stream order.
fn instructions(module: &Module) -> Vec<&Instr> {
    module
        .functions
        .iter()
        .flat_map(|function| function.code.iter())
        .collect()
}

/// How many instructions in `module` still hit the [`Instr::Generic`] fallback for
/// the opcode `op`.
fn generic_count(module: &Module, op: Opcode) -> usize {
    instructions(module)
        .iter()
        .filter(|instr| matches!(instr, Instr::Generic { op: seen, .. } if *seen == op))
        .count()
}

/// Asserts every line location reference of `module` resolves inside `Module::line`,
/// and that at least one of them points at a real (non-zero) source line.
fn assert_locations_resolve(module: &Module, label: &str) -> usize {
    let items = &module.line.items;
    assert!(
        items.len() > 1,
        "{label} must carry a non-trivial line table (found {} item(s))",
        items.len()
    );

    let mut real_lines = 0usize;
    let mut check = |location: LineRef, kind: &str| {
        let index = location as usize;
        assert!(
            index < items.len(),
            "{label}: {kind} location {location} is out of range (0..{})",
            items.len()
        );
        if items[index].line > 0 {
            real_lines += 1;
        }
    };

    for instruction in instructions(module) {
        match instruction {
            Instr::ExecutableLine { location, .. } => check(*location, "executable_line"),
            Instr::DebugLine { location, .. } => check(*location, "debug_line"),
            Instr::Line(location) => check(*location, "line"),
            _ => {}
        }
    }
    real_lines
}

/// Asserts each function's `func_info` identity is preserved in its instruction stream.
fn assert_func_info_preserved(module: &Module, label: &str) {
    for function in &module.functions {
        let infos: Vec<&Instr> = function
            .code
            .iter()
            .filter(|instr| matches!(instr, Instr::FuncInfo { .. }))
            .collect();
        assert_eq!(
            infos.len(),
            1,
            "{label}: function {} must carry exactly one func_info",
            function.arity
        );
        match infos[0] {
            Instr::FuncInfo {
                module: m,
                function: f,
                arity,
            } => {
                assert_eq!(*m, module.name, "{label}: func_info module is preserved");
                assert_eq!(*f, function.name, "{label}: func_info function is preserved");
                assert_eq!(*arity, function.arity, "{label}: func_info arity is preserved");
            }
            other => panic!("expected Instr::FuncInfo, got {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn linecov_lowers_executable_line_typed() {
    let module = load_linecov();

    let executable: Vec<(LineRef, u32)> = instructions(&module)
        .iter()
        .filter_map(|instr| match instr {
            Instr::ExecutableLine { location, index } => Some((*location, *index)),
            _ => None,
        })
        .collect();
    assert!(
        !executable.is_empty(),
        "linecov.beam (compiled `+line_coverage`) must contain `executable_line`"
    );
    assert_eq!(
        generic_count(&module, Opcode::executable_line),
        0,
        "no `executable_line` may lower to Instr::Generic"
    );

    // The `Line` table is non-trivial and every location resolves inside it.
    assert_eq!(module.line.flags & 1, 1, "`+line_coverage` sets the Line flag bit 0");
    assert!(module.line.instruction_count > 0);
    let real = assert_locations_resolve(&module, "linecov");
    assert!(real > 0, "linecov must resolve at least one real source line");
    assert!(
        module.line.files.iter().any(|file| file == "linecov.erl"),
        "linecov's line table must name `linecov.erl` (found {:?})",
        module.line.files
    );

    assert_func_info_preserved(&module, "linecov");
}

#[test]
fn dbgline_lowers_debug_line_typed() {
    let module = load_dbgline();

    let debug: Vec<(LineRef, u32)> = instructions(&module)
        .iter()
        .filter_map(|instr| match instr {
            Instr::DebugLine { location, index, .. } => Some((*location, *index)),
            _ => None,
        })
        .collect();
    assert!(
        !debug.is_empty(),
        "dbgline.beam (compiled `+beam_debug_info`) must contain `debug_line`"
    );
    assert_eq!(
        generic_count(&module, Opcode::debug_line),
        0,
        "no `debug_line` may lower to Instr::Generic"
    );

    // The breakpoint kind is the `entry`/`line` atom, not a raw integer.
    for instruction in instructions(&module) {
        if let Instr::DebugLine { kind, .. } = instruction {
            match kind {
                beam_loader::Operand::Atom(atom) => {
                    let name = module.atom_name(*atom).expect("the kind atom is interned");
                    assert!(
                        name == "entry" || name == "line",
                        "unexpected debug_line kind atom `{name}`"
                    );
                }
                other => panic!("debug_line kind must be an atom, got {other:?}"),
            }
        }
    }

    let real = assert_locations_resolve(&module, "dbgline");
    assert!(real > 0, "dbgline must resolve at least one real source line");
    assert!(
        module.line.files.iter().any(|file| file == "dbgline.erl"),
        "dbgline's line table must name `dbgline.erl` (found {:?})",
        module.line.files
    );

    assert_func_info_preserved(&module, "dbgline");
}
