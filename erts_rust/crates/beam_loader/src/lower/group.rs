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
use crate::opcode::Opcode;
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
///
/// The stream is never lowered as a whole: the boundaries are found on the raw
/// instructions, and each body is lowered straight into the one `Vec<Instr>` that
/// becomes [`Function::code`], sized for the post-fusion length. A module-wide
/// intermediate would hold every function's code twice over.
pub fn group_functions(instrs: &[RawInstr]) -> Result<Vec<Function>> {
    // One `func_info` boundary, with the label that introduces the function.
    struct Cut {
        func_label: Label,
        func_label_idx: usize,
        entry_label: Label,
        module: Atom,
        name: Atom,
        arity: u32,
    }

    // Pass 1: locate the cuts on the raw stream. Only the instructions whose IR
    // shape the grouping itself needs are lowered — the `label` that names a
    // function and the `func_info` that defines one.
    let mut cuts: Vec<Cut> = Vec::new();
    let mut last_label: Option<(Label, usize)> = None;
    for (idx, raw) in instrs.iter().enumerate() {
        match raw.opcode {
            Opcode::label => {
                // The only opcode that lowers to `Instr::Label`.
                if let Instr::Label(label) = lower_instr(raw)? {
                    last_label = Some((label, idx));
                }
            }
            Opcode::func_info => {
                // The only opcode that lowers to `Instr::FuncInfo`.
                let (module, function, arity) = match lower_instr(raw)? {
                    Instr::FuncInfo {
                        module,
                        function,
                        arity,
                    } => (module, function, arity),
                    _ => return Err(bad_file("func_info does not lower to func_info")),
                };
                let (func_label, func_label_idx) = last_label
                    .ok_or_else(|| bad_file("func_info without a preceding label"))?;
                // Only line markers may sit between the function label and the
                // `func_info` that defines the function (`beam_file.c:2502-2589`).
                for between in &instrs[func_label_idx + 1..idx] {
                    if !matches!(lower_instr(between)?, Instr::Line(_)) {
                        return Err(bad_file("func_info is not immediately preceded by its label"));
                    }
                }
                // The entry label follows `func_info` immediately.
                let entry_label = match instrs.get(idx + 1).map(lower_instr) {
                    Some(Ok(Instr::Label(label))) => label,
                    Some(Err(err)) => return Err(err),
                    _ => return Err(bad_file("func_info is not followed by an entry label")),
                };
                cuts.push(Cut {
                    func_label,
                    func_label_idx,
                    entry_label,
                    module,
                    name: function,
                    arity,
                });
            }
            _ => {}
        }
    }

    if cuts.is_empty() {
        return Err(bad_file("module has no func_info"));
    }

    // Everything before the first function's label belongs to no function. It is
    // lowered only so a malformed prefix is still rejected.
    for raw in &instrs[..cuts[0].func_label_idx] {
        lower_instr(raw)?;
    }

    let mut functions = Vec::with_capacity(cuts.len());
    for (i, cut) in cuts.iter().enumerate() {
        // A function's body runs up to the next function's label; the last one runs
        // to the end of the stream (so it carries the trailing `int_code_end`).
        let end = cuts
            .get(i + 1)
            .map_or(instrs.len(), |next| next.func_label_idx);
        let body = &instrs[cut.func_label_idx..end];
        // The one allocation of this body, sized to the FUSED length: [`emit_fused`]
        // applies the peephole rule as it lowers, so it emits `body.len() -
        // count_fusable_pairs(body)` instructions and the vector never grows.
        let fused_len = body.len() - count_fusable_pairs(body);
        let mut code: Vec<Instr> = Vec::with_capacity(fused_len);
        emit_fused(body, &mut code)?;
        // The capacity is exact by construction. If a future change breaks that, the
        // retained body carries slack (a memory regression, never a wrong
        // instruction), so the invariant is asserted here in debug builds and pinned
        // over the whole install by `tests/real_modules.rs::slack_tables` — a
        // `shrink_to_fit` here would instead put a second, transient copy of every
        // function's instruction vector back into the boot peak.
        debug_assert_eq!(code.len(), code.capacity());
        // `blocks`/`labels` are derived from the FUSED code so a block's `start..end`
        // slice indexes the same vector the engine dispatches on.
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

/// Lowers `body` into `out`, applying the fuse rule as each pair closes.
///
/// `out` must have room for exactly `body.len() - count_fusable_pairs(body)`
/// instructions — the length of the fused sequence — so it never reallocates and the
/// body it becomes has no slack. The rule itself is
/// [`super::fuse::fuses`], the same predicate [`super::fuse::fuse_in_place`] applies,
/// so a body lowered here is element-for-element the body that pass produces:
/// [`lower_instr`] lowers `Opcode::deallocate`/`Opcode::r#return` to
/// [`Instr::Deallocate`]/[`Instr::Return`] and every other opcode to exactly one
/// instruction, so the lowered adjacency the predicate tests is the raw body's.
///
/// Fusing during the emit rather than afterwards is what lets the one allocation be
/// exact: emitting the pre-fusion sequence would overrun a capacity sized for the
/// post-fusion one (by exactly the fused pair count) and grow to twice the final
/// size. Errors are reported in stream order — an instruction is lowered in the
/// iteration that visits it, whether or not its predecessor is still pending.
fn emit_fused(body: &[RawInstr], out: &mut Vec<Instr>) -> Result<()> {
    // The lowered instruction whose successor decides whether the pair fuses. Only a
    // `Deallocate` can start a pair, but keeping `Option<Instr>` unconditional is what
    // makes this exactly the adjacency `fuse_in_place` sees.
    let mut pending: Option<Instr> = None;
    for raw in body {
        let instr = lower_instr(raw)?;
        match pending.take() {
            None => pending = Some(instr),
            Some(prev) => match super::fuse::fuses(&prev, &instr) {
                Some(n) => out.push(Instr::DeallocateReturn(n)),
                None => {
                    out.push(prev);
                    pending = Some(instr);
                }
            },
        }
    }
    if let Some(prev) = pending {
        out.push(prev);
    }
    Ok(())
}

/// The number of adjacent `deallocate`/`return` pairs in a raw body.
///
/// [`lower_instr`] maps `Opcode::deallocate` to [`Instr::Deallocate`] and nothing
/// else, and `Opcode::r#return` to [`Instr::Return`] and nothing else, while every
/// other opcode produces exactly one instruction — so a lowered body is
/// element-for-element its raw body and the adjacent pairs sit at the same
/// positions. Two pairs can never overlap (`deallocate` cannot also be the `return`
/// half of a pair), so this is exactly the number of instructions [`emit_fused`] (and
/// [`super::fuse::fuse_in_place`]) removes. A wrong count would only make the body's
/// allocation the wrong size, never change the instructions.
fn count_fusable_pairs(body: &[RawInstr]) -> usize {
    body.windows(2)
        .filter(|pair| pair[0].opcode == Opcode::deallocate && pair[1].opcode == Opcode::r#return)
        .count()
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

    /// A `deallocate N` instruction.
    fn deallocate(n: u32) -> RawInstr {
        raw(Opcode::deallocate, vec![Operand::Integer(i64::from(n))])
    }

    /// A `return` instruction.
    fn ret() -> RawInstr {
        raw(Opcode::r#return, vec![])
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

    #[test]
    fn group_functions_fuses_each_body_and_keeps_its_blocks() {
        let (module, function) = atoms();
        let instrs = vec![
            // First function: two `deallocate`/`return` pairs split by a label.
            label(1),
            func_info(module, function, 1),
            label(2),
            deallocate(1),
            ret(),
            label(3),
            deallocate(2),
            ret(),
            // The next function's label is the cut between the two bodies.
            label(4),
            raw(Opcode::line, vec![Operand::Integer(7)]),
            func_info(module, function, 2),
            label(5),
            deallocate(0),
            ret(),
        ];
        let functions = group_functions(&instrs).unwrap();
        assert_eq!(functions.len(), 2);
        assert_eq!(
            functions[0].code,
            vec![
                Instr::Label(Label(1)),
                Instr::FuncInfo {
                    module,
                    function,
                    arity: 1
                },
                Instr::Label(Label(2)),
                Instr::DeallocateReturn(1),
                Instr::Label(Label(3)),
                Instr::DeallocateReturn(2),
            ]
        );
        // The blocks index the FUSED body, so the trailing block ends at its length.
        assert_eq!(
            functions[0].blocks,
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
                Block {
                    label: Label(3),
                    start: 4,
                    end: 6
                },
            ]
        );
        assert_eq!(functions[0].labels.len(), 3);
        assert_eq!(
            functions[1].code,
            vec![
                Instr::Label(Label(4)),
                Instr::Line(7),
                Instr::FuncInfo {
                    module,
                    function,
                    arity: 2
                },
                Instr::Label(Label(5)),
                Instr::DeallocateReturn(0),
            ]
        );
        assert_eq!(functions[1].blocks.len(), 2);
    }

    #[test]
    fn group_functions_rejects_an_unlowerable_instruction_before_the_first_function() {
        let (module, function) = atoms();
        // A `move` with no operands cannot be lowered; it belongs to no function body.
        let instrs = vec![
            raw(Opcode::r#move, vec![]),
            label(1),
            func_info(module, function, 1),
            label(2),
            ret(),
        ];
        assert!(matches!(
            group_functions(&instrs).unwrap_err(),
            BeamError::BadArg
        ));
    }

    #[test]
    fn group_functions_rejects_an_unlowerable_instruction_inside_a_body() {
        let (module, function) = atoms();
        let instrs = vec![
            label(1),
            func_info(module, function, 1),
            label(2),
            raw(Opcode::r#move, vec![]),
            ret(),
        ];
        assert!(matches!(
            group_functions(&instrs).unwrap_err(),
            BeamError::BadArg
        ));
    }

    #[test]
    fn group_functions_reports_the_first_unlowerable_instruction_of_a_body() {
        // Error order inside a body is the stream's: the earlier failure wins.
        let (module, function) = atoms();
        let instrs = vec![
            label(1),
            func_info(module, function, 1),
            label(2),
            raw(Opcode::jump, vec![]),
            raw(Opcode::r#move, vec![]),
        ];
        assert!(matches!(
            group_functions(&instrs).unwrap_err(),
            BeamError::BadArg
        ));
    }

    // -----------------------------------------------------------------------
    // The fused emit: one exact allocation, and the same body `fuse_in_place`
    // produces
    // -----------------------------------------------------------------------

    /// The two routes to a fused body, for `body`: (a) lower every instruction and
    /// run [`crate::lower::fuse::fuse_in_place`] over the result, (b) [`emit_fused`]
    /// into an exactly-sized vector.
    fn both_routes(body: &[RawInstr]) -> (Vec<Instr>, Vec<Instr>) {
        let mut reference: Vec<Instr> = body.iter().map(|r| lower_instr(r).unwrap()).collect();
        crate::lower::fuse::fuse_in_place(&mut reference);
        let mut emitted: Vec<Instr> = Vec::with_capacity(body.len() - count_fusable_pairs(body));
        emit_fused(body, &mut emitted).unwrap();
        // The exactness invariant the whole change rests on: the pre-count is the
        // emitted length, so the vector has no slack and never reallocated.
        assert_eq!(
            emitted.len(),
            emitted.capacity(),
            "the fused emit must fill its allocation exactly"
        );
        (reference, emitted)
    }

    /// One instruction of a body, from a 2-bit alphabet: `deallocate`, `return`,
    /// `label`, `move` — enough to exercise every adjacency the rule can see.
    fn alphabet(kind: u8) -> RawInstr {
        match kind {
            0 => deallocate(1),
            1 => ret(),
            2 => label(2),
            _ => raw(Opcode::r#move, vec![Operand::X(0), Operand::X(1)]),
        }
    }

    #[test]
    fn emit_fused_agrees_with_lower_then_fuse_over_every_short_body() {
        // Exhaustive over all 4^6 = 4096 six-instruction bodies: every position a
        // `deallocate`/`return` pair can occupy, including a pair split by a `Label`,
        // back-to-back pairs, a dangling `deallocate` and no pair at all.
        const LEN: usize = 6;
        let mut bodies = 0usize;
        let mut fused_pairs = 0usize;
        for pattern in 0..4u32.pow(LEN as u32) {
            let body: Vec<RawInstr> = (0..LEN)
                .map(|i| alphabet(((pattern >> (2 * i)) & 0b11) as u8))
                .collect();
            let (reference, emitted) = both_routes(&body);
            assert_eq!(emitted, reference, "pattern {pattern:06b}");
            bodies += 1;
            fused_pairs += count_fusable_pairs(&body);
        }
        assert_eq!(bodies, 4096);
        // The sweep really did cover the fusion path, not just copy-throughs.
        assert!(fused_pairs > 0);
    }

    #[test]
    fn emit_fused_fuses_adjacent_pairs_and_keeps_a_lone_deallocate() {
        let body = vec![
            label(1),
            deallocate(1),
            ret(),
            label(2),
            deallocate(2),
            label(3),
            ret(),
            deallocate(3),
        ];
        let (_, emitted) = both_routes(&body);
        assert_eq!(
            emitted,
            vec![
                Instr::Label(Label(1)),
                Instr::DeallocateReturn(1),
                Instr::Label(Label(2)),
                // The `Label` between these two blocks the fuse.
                Instr::Deallocate(2),
                Instr::Label(Label(3)),
                Instr::Return,
                Instr::Deallocate(3),
            ]
        );
    }

    #[test]
    fn emit_fused_reports_the_first_unlowerable_instruction() {
        let body = vec![deallocate(1), ret(), raw(Opcode::jump, vec![]), label(2)];
        let mut out: Vec<Instr> = Vec::new();
        assert!(matches!(
            emit_fused(&body, &mut out),
            Err(BeamError::BadArg)
        ));
    }

    /// `group_functions` leaves every body's allocation exact — the invariant a
    /// `shrink_to_fit` used to restore (and which put a second copy of every body in
    /// the boot peak).
    #[test]
    fn group_functions_allocates_each_body_exactly() {
        let (module, function) = atoms();
        let instrs = vec![
            label(1),
            func_info(module, function, 1),
            label(2),
            deallocate(1),
            ret(),
            label(3),
            func_info(module, function, 2),
            label(4),
            deallocate(2),
            ret(),
        ];
        for f in group_functions(&instrs).unwrap() {
            assert_eq!(f.code.len(), f.code.capacity());
        }
    }
}
