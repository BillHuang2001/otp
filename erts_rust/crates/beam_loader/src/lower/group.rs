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

//! Function grouping and basic-block decomposition.
//!
//! The wire-level pieces ([`crate::chunk`], [`crate::decode`]) hand the lowering
//! step one flat instruction stream for the whole module; this module is the
//! semantic step that cuts it into functions and basic blocks:
//!
//! * [`group_functions`] splits the stream at `func_info` boundaries and attaches
//!   each function's `func_label` / `entry_label` (the file sequence is
//!   `label FuncLabel | [line] | func_info M F A | label EntryLabel | body…`;
//!   `beam_file.c:2502-2589`).
//! * [`build_blocks`] decomposes a function body into label-delimited basic blocks
//!   and builds the label → instruction-index map.
//!
//! Both are re-exported from [`crate::lower`], whose `build_module` assembles the
//! result into the IR [`Module`](crate::ir::Module). Every fallible step reports
//! `BeamError::BadArg` through [`crate::error::bad_file`]; the module never panics
//! and never `unwrap()`s file data.
use std::collections::BTreeMap;

use beam_common::Result;
use beam_term::Atom;

use crate::decode::RawInstr;
use crate::error::bad_file;
use crate::ir::{Block, Function, Instr};
use crate::operand::Label;

use super::instr::lower_instr;

/// Splits the module-wide instruction stream into functions at `func_info` boundaries.
///
/// Each returned [`Function`] carries the `func_info` module/name/arity, the
/// `func_label` (the label before `func_info`) and the `entry_label` (the label
/// after it, which is what [`crate::ir::Export::label`] points to), its lowered
/// body and its basic blocks. A stream with no `func_info`, or with a `func_info`
/// not preceded by its label or not followed by the entry label, yields
/// `BeamError::BadArg`.
pub fn group_functions(instrs: &[RawInstr]) -> Result<Vec<Function>> {
    // Lower the whole module stream once; the function bodies are slices of it.
    let mut lowered = Vec::with_capacity(instrs.len());
    for raw in instrs {
        lowered.push(lower_instr(raw)?);
    }

    // One `func_info` boundary, with the label that introduces the function.
    struct Cut {
        func_label: Label,
        func_label_idx: usize,
        entry_label: Label,
        module: Atom,
        name: Atom,
        arity: u32,
    }

    let mut cuts: Vec<Cut> = Vec::new();
    let mut last_label: Option<(Label, usize)> = None;
    for (idx, instr) in lowered.iter().enumerate() {
        match instr {
            Instr::Label(label) => last_label = Some((*label, idx)),
            Instr::FuncInfo {
                module,
                function,
                arity,
            } => {
                let (func_label, func_label_idx) = last_label
                    .ok_or_else(|| bad_file("func_info without a preceding label"))?;
                // Only line markers may sit between the function label and the
                // `func_info` that defines the function (`beam_file.c:2502-2589`).
                if lowered[func_label_idx + 1..idx]
                    .iter()
                    .any(|instr| !matches!(instr, Instr::Line(_)))
                {
                    return Err(bad_file("func_info is not immediately preceded by its label"));
                }
                // The entry label follows `func_info` immediately.
                let entry_label = match lowered.get(idx + 1) {
                    Some(Instr::Label(label)) => *label,
                    _ => return Err(bad_file("func_info is not followed by an entry label")),
                };
                cuts.push(Cut {
                    func_label,
                    func_label_idx,
                    entry_label,
                    module: *module,
                    name: *function,
                    arity: *arity,
                });
            }
            _ => {}
        }
    }

    if cuts.is_empty() {
        return Err(bad_file("module has no func_info"));
    }

    let mut functions = Vec::with_capacity(cuts.len());
    for (i, cut) in cuts.iter().enumerate() {
        // A function's body runs up to the next function's label; the last one runs
        // to the end of the stream (so it carries the trailing `int_code_end`).
        let end = cuts
            .get(i + 1)
            .map_or(lowered.len(), |next| next.func_label_idx);
        let code = lowered[cut.func_label_idx..end].to_vec();
        // The one peephole pass: `deallocate N; return` -> `DeallocateReturn(N)`.
        // `blocks`/`labels` must be derived from the FUSED code so a block's
        // `start..end` slice indexes the same vector the engine dispatches on.
        let code = super::fuse::fuse(code);
        let (blocks, labels) = build_blocks(&code);
        functions.push(Function {
            func_label: cut.func_label,
            entry_label: cut.entry_label,
            module: cut.module,
            name: cut.name,
            arity: cut.arity,
            code,
            blocks,
            labels,
        });
    }
    Ok(functions)
}

/// Decomposes a function body into label-delimited basic blocks.
///
/// Returns the blocks (each `code[start..end]` beginning at its label) and the
/// label → instruction-index map. The map is derived from the `Instr::Label`
/// markers; the blocks are the runs between consecutive labels.
pub fn build_blocks(code: &[Instr]) -> (Vec<Block>, BTreeMap<Label, usize>) {
    let mut starts: Vec<(Label, usize)> = Vec::new();
    let mut labels = BTreeMap::new();

    for (idx, instr) in code.iter().enumerate() {
        if let Instr::Label(label) = instr {
            labels.insert(*label, idx);
            starts.push((*label, idx));
        }
    }

    let mut blocks = Vec::with_capacity(starts.len());
    for (i, (label, start)) in starts.iter().enumerate() {
        let end = starts.get(i + 1).map_or(code.len(), |(_, next)| *next);
        blocks.push(Block {
            label: *label,
            start: *start,
            end,
        });
    }

    (blocks, labels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opcode::Opcode;
    use crate::operand::Operand;
    use beam_common::BeamError;

    /// One decoded instruction with the given opcode and operands.
    fn raw(opcode: Opcode, operands: Vec<Operand>) -> RawInstr {
        RawInstr {
            opcode,
            operands,
            offset: 0,
        }
    }

    /// The `label L` instruction as the compiler writes it: a `u` operand.
    fn label(l: u32) -> RawInstr {
        raw(Opcode::label, vec![Operand::Integer(i64::from(l))])
    }

    /// A `func_info M F A` instruction.
    fn func_info(module: Atom, function: Atom, arity: u32) -> RawInstr {
        raw(
            Opcode::func_info,
            vec![
                Operand::Atom(module),
                Operand::Atom(function),
                Operand::Integer(i64::from(arity)),
            ],
        )
    }

    fn atoms() -> (Atom, Atom) {
        let mut table = beam_term::AtomTable::new();
        (table.intern("m"), table.intern("f"))
    }

    #[test]
    fn build_blocks_splits_at_every_label() {
        let code = vec![
            Instr::Label(Label(1)),
            Instr::Move {
                src: Operand::X(0),
                dst: Operand::X(1),
            },
            Instr::Label(Label(2)),
            Instr::Return,
        ];
        let (blocks, labels) = build_blocks(&code);
        assert_eq!(
            blocks,
            vec![
                Block {
                    label: Label(1),
                    start: 0,
                    end: 2
                },
                Block {
                    label: Label(2),
                    start: 2,
                    end: 4
                },
            ]
        );
        assert_eq!(labels.get(&Label(1)), Some(&0));
        assert_eq!(labels.get(&Label(2)), Some(&2));
    }

    #[test]
    fn build_blocks_yields_nothing_without_labels() {
        let code = vec![Instr::Return];
        let (blocks, labels) = build_blocks(&code);
        assert!(blocks.is_empty());
        assert!(labels.is_empty());
    }

    #[test]
    fn build_blocks_of_an_empty_body_is_empty() {
        let (blocks, labels) = build_blocks(&[]);
        assert!(blocks.is_empty());
        assert!(labels.is_empty());
    }

    #[test]
    fn group_functions_cuts_at_func_info_and_keeps_both_labels() {
        let (module, function) = atoms();
        let instrs = vec![
            label(1),
            raw(Opcode::line, vec![Operand::Integer(1)]),
            func_info(module, function, 2),
            label(2),
            raw(Opcode::r#return, vec![]),
        ];
        let functions = group_functions(&instrs).unwrap();
        assert_eq!(functions.len(), 1);
        let f = &functions[0];
        assert_eq!(f.func_label, Label(1));
        assert_eq!(f.entry_label, Label(2));
        assert_eq!(f.module, module);
        assert_eq!(f.name, function);
        assert_eq!(f.arity, 2);
        assert_eq!(
            f.code,
            vec![
                Instr::Label(Label(1)),
                Instr::Line(1),
                Instr::FuncInfo {
                    module,
                    function,
                    arity: 2
                },
                Instr::Label(Label(2)),
                Instr::Return,
            ]
        );
        assert_eq!(f.blocks.len(), 2);
    }

    #[test]
    fn group_functions_rejects_a_func_info_without_a_label() {
        let (module, function) = atoms();
        let instrs = vec![func_info(module, function, 1), label(2)];
        assert!(matches!(
            group_functions(&instrs).unwrap_err(),
            BeamError::BadArg
        ));
    }

    #[test]
    fn group_functions_rejects_a_func_info_not_immediately_after_its_label() {
        let (module, function) = atoms();
        // A real instruction sits between the label and the `func_info`.
        let instrs = vec![
            label(1),
            raw(Opcode::r#return, vec![]),
            func_info(module, function, 1),
            label(2),
        ];
        assert!(matches!(
            group_functions(&instrs).unwrap_err(),
            BeamError::BadArg
        ));
    }

    #[test]
    fn group_functions_rejects_a_func_info_without_an_entry_label() {
        let (module, function) = atoms();
        let instrs = vec![label(1), func_info(module, function, 1)];
        assert!(matches!(
            group_functions(&instrs).unwrap_err(),
            BeamError::BadArg
        ));
    }

    #[test]
    fn group_functions_rejects_a_stream_without_func_info() {
        let instrs = vec![label(1), raw(Opcode::r#return, vec![])];
        assert!(matches!(
            group_functions(&instrs).unwrap_err(),
            BeamError::BadArg
        ));
    }
}
