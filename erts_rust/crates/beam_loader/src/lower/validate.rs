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

//! Post-lowering validation of the code stream.
//!
//! The legacy loader validates the *assembled* code — not just the chunk framing —
//! while it emits instructions (`erts/emulator/beam/jit/asm_load.c`). Those checks
//! are accept/reject-only: the oracle's Erlang-observable outcome is the bare atom
//! `badfile` (`code:load_binary/3`), so every failure here is [`bad_file`]
//! (`BeamError::BadArg`), except the arity bound, where this crate's documented
//! convention is [`BeamError::SystemLimit`].
//!
//! This module is one pass over the lowered [`Function`]s, run by
//! [`crate::lower::build_module`] once grouping has produced them:
//!
//! * **Label definitions** — `asm_load.c:512-521` ("invalid label num %u
//!   (0 < label < %u)") and `:523-528` ("label %d defined more than once"). The
//!   `label` operand of a `label` instruction is a plain `u` integer the C never
//!   range-checks anywhere else; `beam_file.c:429` states the same `label > 0` rule
//!   for the lambda table. A module built by hand (`CodeVersion::label_count == 0`,
//!   i.e. no `Code` header) has no label space to check against, so the *range* and
//!   zero rules are skipped there; the duplicate rule is label-space independent and
//!   always applies.
//! * **Function count** — `asm_load.c:625-629` ("too many functions in module
//!   (header said %u)") and `:737-743` ("too few functions (%u) in module (header
//!   said %u)"): the number of `func_info` instructions must equal the `Code`
//!   header's `function_count`. Skipped when the header says 0 (a hand-built
//!   module).
//! * **`func_info` arity** — `asm_load.c:636-639` ("too many arguments: %d"):
//!   `arity <= MAX_ARG` (255, `erl_vm.h:48`).
//! * **Import-table indices** — `asm_load.c:532-548` ("invalid import table index
//!   %d") for the `e`/`b` operand types: every `ImportRef` an instruction carries
//!   must index `Module::imports`. (The C's "import %d not a BIF" membership test is
//!   deliberately *not* reproduced here — that knowledge belongs to `beam_bifs`, and
//!   the loader never learns `bif.tab` numbering.)
//! * **Line-table indices** — `asm_load.c:324-341` ("line instruction index overflow
//!   (%u/%u)", "line instruction table overflow (%u/%u)") reached from both
//!   `op_i_line_I` and `op_i_debug_line_IIt`: every `LineRef` must index
//!   [`LineTable::items`]. `LineRef` 0 is the undefined location and stays valid;
//!   with no line table at all the C returns early without checking
//!   (`add_line_entry`'s `if (!stp->beam.lines.item_count) return 0`), so an absent
//!   table skips the check here too.
//!
//! Deliberately *not* validated here (each is documented in the crate `CONTEXT.md`
//! under Known Issues): the `DbgB` debug-item index a `debug_line` carries (the chunk
//! is never parsed), an undefined label *target* (a release-build C only checks the
//! definition side, `beam_file.c:2324-2331` checks the `f` operand range in this
//! crate's decoder), the `bs_match` sub-instruction vocabulary, and the obsolete
//! roster opcodes that lower to [`Instr::Generic`].
use std::collections::BTreeSet;

use beam_common::{BeamError, Result};

use crate::error::bad_file;
use crate::ir::{CodeVersion, Function, Instr, LineRef, LineTable};
use crate::operand::Label;

/// The largest function arity the runtime accepts: the C's `MAX_ARG`
/// (`erts/emulator/beam/erl_vm.h:48`, "Max number of arguments allowed").
const MAX_ARG: u32 = 255;

/// Validates the lowered code stream against the module's own tables.
///
/// `code_version` is the `Code` header (its `label_count` / `function_count` are the
/// declared sizes the stream must match), `line` is the module's line table (whose
/// `items` every [`LineRef`](crate::ir::LineRef) must index) and `import_count` is
/// `Module::imports.len()`.
pub(crate) fn validate_code(
    functions: &[Function],
    code_version: CodeVersion,
    line: &LineTable,
    import_count: usize,
) -> Result<()> {
    validate_labels(functions, code_version.label_count)?;
    validate_function_count(functions.len(), code_version.function_count)?;
    for function in functions {
        for instr in &function.code {
            validate_instr(instr, line, import_count)?;
        }
    }
    Ok(())
}

/// Rejects an invalid or re-defined `label` definition.
///
/// `asm_load.c:512-521`: a defined label must satisfy `0 < label < label_count`
/// (the C's own test is `last_label < 0 || last_label >= label_count`, i.e. the
/// error text's `0 < label` bound plus the `labels[]` array size); `:523-528`: a
/// label may be defined only once for the whole module (the C keeps one label array
/// per module, not per function).
fn validate_labels(functions: &[Function], label_count: u32) -> Result<()> {
    let mut defined: BTreeSet<u32> = BTreeSet::new();
    for function in functions {
        for instr in &function.code {
            let Instr::Label(label) = instr else {
                continue;
            };
            // A hand-built module carries no `Code` header, hence no label space to
            // bound a definition against (`CodeVersion::default()`).
            if label_count != 0 && (label.0 == 0 || label.0 >= label_count) {
                return Err(bad_file("invalid label num (0 < label < label_count)"));
            }
            if !defined.insert(label.0) {
                return Err(bad_file("label defined more than once"));
            }
        }
    }
    Ok(())
}

/// Rejects a stream whose `func_info` count disagrees with the `Code` header.
///
/// `asm_load.c:625-629` reports a `func_info` past `function_count` as "too many
/// functions in module (header said %u)" and `:737-743` reports a short count at
/// `int_code_end` as "too few functions (%u) in module (header said %u)"; both are
/// the same equality. A header of 0 is a hand-built module with no declared size.
fn validate_function_count(functions: usize, function_count: u32) -> Result<()> {
    if function_count != 0 && functions as u64 != u64::from(function_count) {
        return Err(bad_file("function count disagrees with the Code header"));
    }
    Ok(())
}

/// Validates one instruction's arity, import index and line references.
///
/// `asm_load.c:636-639` bounds a `func_info`'s arity by `MAX_ARG`; `:532-548` bounds
/// the `e`/`b` import operands by the import table; `:324-341` (from both the `line`
/// and `debug_line` cases) bounds every location reference by the line item table.
fn validate_instr(instr: &Instr, line: &LineTable, import_count: usize) -> Result<()> {
    match instr {
        Instr::FuncInfo { arity, .. } => {
            if *arity > MAX_ARG {
                // `beam_file.c:636` is the C's `MAX_ARG` ("Max number of arguments
                // allowed", `erl_vm.h:48`); an arity above it is a size bound, which
                // this crate reports as `SystemLimit` by convention.
                return Err(BeamError::SystemLimit);
            }
        }
        Instr::CallExt { target, .. }
        | Instr::CallExtOnly { target, .. }
        | Instr::CallExtLast { target, .. }
        | Instr::Bif0 { bif: target, .. }
        | Instr::Bif { bif: target, .. }
        | Instr::GcBif { bif: target, .. } => {
            if target.0 as usize >= import_count {
                return Err(bad_file("invalid import table index"));
            }
        }
        Instr::Line(location) | Instr::ExecutableLine { location, .. } => {
            validate_line_ref(*location, line)?;
        }
        Instr::DebugLine { location, .. } => validate_line_ref(*location, line)?,
        _ => {}
    }
    Ok(())
}

/// Rejects a location reference past the module's line item table.
///
/// `add_line_entry` (`asm_load.c:324-341`) returns without checking when the module
/// has no line items at all — which is what this crate holds when the `Line` chunk is
/// absent or unusable ([`LineTable::default`], an empty `items`). Otherwise the index
/// must be `< items.len()`; item 0 is the always-present "undefined location"
/// (`beam_file.c:481-489`), so it always stays valid.
fn validate_line_ref(location: LineRef, line: &LineTable) -> Result<()> {
    if line.items.is_empty() {
        return Ok(());
    }
    if location as usize >= line.items.len() {
        return Err(bad_file("line instruction index overflow"));
    }
    Ok(())
}

/// Whether `label` is a label a `FunT` descriptor may name.
///
/// Warms the caller about the one rule the C states outside `asm_load.c`
/// (`beam_file.c:429`'s `LoadAssert(label > 0 && label < label_count)`); the
/// definition-side checks live in [`validate_labels`].
pub(crate) fn is_defined_label(functions: &[Function], label: Label) -> bool {
    label.0 != 0
        && functions
            .iter()
            .any(|function| function.labels.contains_key(&label))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Block, LineEntry};
    use crate::operand::NO_FAIL;

    /// A function whose body is `code`, with no label map of its own (the checks
    /// under test walk `code`, not the derived map).
    fn function(code: Vec<Instr>) -> Function {
        Function {
            func_label: Label(1),
            entry_label: Label(2),
            module: beam_term::Atom::MAX,
            name: beam_term::Atom::MAX,
            arity: 0,
            code,
            blocks: Vec::<Block>::new(),
            labels: std::collections::BTreeMap::new(),
        }
    }

    /// `n` line items, i.e. the undefined location plus `n - 1` real ones.
    fn line_table(n: usize) -> LineTable {
        LineTable {
            flags: 0,
            instruction_count: 0,
            items: vec![LineEntry { file: 0, line: 0 }; n],
            files: Vec::new(),
        }
    }

    fn header(label_count: u32, function_count: u32) -> CodeVersion {
        CodeVersion {
            format_number: 0,
            max_opcode: 181,
            label_count,
            function_count,
        }
    }

    fn func_info(arity: u32) -> Instr {
        Instr::FuncInfo {
            module: beam_term::Atom::MAX,
            function: beam_term::Atom::MAX,
            arity,
        }
    }

    #[test]
    fn accepts_a_well_formed_stream() {
        let functions = vec![function(vec![
            Instr::Label(Label(1)),
            func_info(2),
            Instr::Label(Label(2)),
            Instr::Line(0),
            Instr::Return,
        ])];
        validate_code(&functions, header(3, 1), &line_table(1), 0).unwrap();
    }

    #[test]
    fn rejects_a_label_definition_of_zero() {
        let functions = vec![function(vec![Instr::Label(Label(0)), func_info(0)])];
        assert!(matches!(
            validate_code(&functions, header(3, 1), &LineTable::default(), 0).unwrap_err(),
            BeamError::BadArg
        ));
    }

    #[test]
    fn rejects_a_label_definition_at_or_above_label_count() {
        let functions = vec![function(vec![Instr::Label(Label(3)), func_info(0)])];
        // `label_count` 3 means labels 1 and 2 are the only valid definitions.
        assert!(matches!(
            validate_code(&functions, header(3, 1), &LineTable::default(), 0).unwrap_err(),
            BeamError::BadArg
        ));
        let control = vec![function(vec![
            Instr::Label(Label(2)),
            func_info(0),
        ])];
        validate_code(&control, header(3, 1), &LineTable::default(), 0).unwrap();
    }

    #[test]
    fn rejects_a_duplicate_label_definition_across_functions() {
        // The C keeps one label array per module, so a repeat across two functions is
        // the same collision as a repeat inside one body.
        let functions = vec![
            function(vec![Instr::Label(Label(1)), func_info(0)]),
            function(vec![Instr::Label(Label(1)), func_info(0)]),
        ];
        assert!(matches!(
            validate_code(&functions, header(4, 2), &LineTable::default(), 0).unwrap_err(),
            BeamError::BadArg
        ));
    }

    #[test]
    fn the_duplicate_rule_applies_without_a_code_header() {
        let functions = vec![function(vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(1)),
            func_info(0),
        ])];
        assert!(matches!(
            validate_code(&functions, CodeVersion::default(), &LineTable::default(), 0)
                .unwrap_err(),
            BeamError::BadArg
        ));
    }

    #[test]
    fn a_zero_label_count_skips_the_label_range_rule() {
        // A hand-built module has no `Code` header, so it has no label space to bound
        // its definitions against — the range (and zero) rules are skipped.
        let functions = vec![function(vec![Instr::Label(Label(0)), func_info(0)])];
        validate_code(&functions, CodeVersion::default(), &LineTable::default(), 0).unwrap();
    }

    #[test]
    fn rejects_a_function_count_that_disagrees_with_the_header() {
        // Too few functions.
        let one = vec![function(vec![Instr::Label(Label(1)), func_info(0)])];
        assert!(matches!(
            validate_code(&one, header(2, 2), &LineTable::default(), 0).unwrap_err(),
            BeamError::BadArg
        ));
        // A header of 0 declares nothing, so a hand-built module is not compared.
        validate_code(&one, header(2, 0), &LineTable::default(), 0).unwrap();
        // Too many functions.
        let two = vec![
            function(vec![Instr::Label(Label(1)), func_info(0)]),
            function(vec![Instr::Label(Label(2)), func_info(0)]),
        ];
        assert!(matches!(
            validate_code(&two, header(3, 1), &LineTable::default(), 0).unwrap_err(),
            BeamError::BadArg
        ));
    }

    #[test]
    fn rejects_a_func_info_arity_above_the_runtime_maximum() {
        let functions = vec![function(vec![func_info(MAX_ARG + 1)])];
        assert!(matches!(
            validate_code(&functions, header(2, 1), &LineTable::default(), 0).unwrap_err(),
            BeamError::SystemLimit
        ));
        // `MAX_ARG` itself is legal.
        let control = vec![function(vec![func_info(MAX_ARG)])];
        validate_code(&control, header(2, 1), &LineTable::default(), 0).unwrap();
    }

    #[test]
    fn rejects_every_import_reference_past_the_import_table() {
        let a = || crate::ir::ImportRef(1);
        let carriers = [
            Instr::CallExt {
                arity: 0,
                target: a(),
            },
            Instr::CallExtOnly {
                arity: 0,
                target: a(),
            },
            Instr::CallExtLast {
                arity: 0,
                target: a(),
                deallocate: 0,
            },
            Instr::Bif0 {
                bif: a(),
                dst: crate::operand::Operand::X(0),
            },
            Instr::Bif {
                arity: 0,
                bif: a(),
                fail: NO_FAIL,
                args: Vec::new(),
                dst: crate::operand::Operand::X(0),
            },
            Instr::GcBif {
                arity: 0,
                bif: a(),
                fail: NO_FAIL,
                live: 0,
                args: Vec::new(),
                dst: crate::operand::Operand::X(0),
            },
        ];
        for instr in carriers {
            let what = format!("{instr:?}");
            let functions = vec![function(vec![instr])];
            // A one-entry import table: index 1 is out of range.
            assert!(
                matches!(
                    validate_code(&functions, header(2, 1), &LineTable::default(), 1).unwrap_err(),
                    BeamError::BadArg
                ),
                "{what} must be rejected"
            );
        }
        // Control: index 0 of a one-entry table is valid.
        let functions = vec![function(vec![Instr::CallExt {
            arity: 0,
            target: crate::ir::ImportRef(0),
        }])];
        validate_code(&functions, header(2, 1), &LineTable::default(), 1).unwrap();
    }

    #[test]
    fn rejects_a_line_reference_past_the_item_table() {
        for instr in [
            Instr::Line(2),
            Instr::ExecutableLine {
                location: 2,
                index: 0,
            },
            Instr::DebugLine {
                kind: crate::operand::Operand::Atom(beam_term::Atom::MAX),
                location: 2,
                index: 1,
                live: 0,
            },
        ] {
            let what = format!("{instr:?}");
            let functions = vec![function(vec![instr])];
            // Two items: 0 (the undefined location) and 1.
            assert!(
                matches!(
                    validate_code(&functions, header(2, 1), &line_table(2), 0).unwrap_err(),
                    BeamError::BadArg
                ),
                "{what} must be rejected"
            );
        }
        // The last valid index and the undefined location both pass.
        let functions = vec![function(vec![Instr::Line(0), Instr::Line(1)])];
        validate_code(&functions, header(2, 1), &line_table(2), 0).unwrap();
    }

    #[test]
    fn an_absent_line_table_skips_the_line_reference_rule() {
        // `add_line_entry` returns 0 without checking when the module has no line
        // items, which is exactly an absent/unusable `Line` chunk here.
        let functions = vec![function(vec![Instr::Line(9_999)])];
        validate_code(&functions, header(2, 1), &LineTable::default(), 0).unwrap();
    }

    #[test]
    fn a_fun_label_of_zero_is_not_a_defined_label() {
        let mut functions = vec![function(Vec::new())];
        functions[0].labels.insert(Label(2), 0);
        assert!(!is_defined_label(&functions, Label(0)));
        assert!(is_defined_label(&functions, Label(2)));
        assert!(!is_defined_label(&functions, Label(3)));
    }
}
