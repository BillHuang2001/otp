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
//!   `beam_file.c:2502-2589`). It applies no post-lowering rule of its own.
//! * [`group_functions_checked`] is the same split with the rules of
//!   [`super::validate`] applied **as each body is emitted**: one traversal of the
//!   lowered stream produces the code, the blocks, the label map and the verdict,
//!   where [`build_blocks`] and `validate::validate_code` used to add three more
//!   walks. [`crate::lower::build_module`] takes this route.
//! * [`build_blocks`] decomposes an already-lowered function body into
//!   label-delimited basic blocks and builds the label → instruction-index map.
//!
//! Every fallible step reports `BeamError::BadArg` through [`crate::error::bad_file`];
//! the module never panics and never `unwrap()`s file data.
use std::collections::BTreeMap;

use beam_common::Result;
use beam_term::Atom;

use crate::decode::RawInstr;
use crate::error::bad_file;
use crate::ir::{Block, Function, Instr};
use crate::opcode::Opcode;
use crate::operand::Label;

use super::instr::lower_instr;
use super::validate::{self, DefinedLabels, ValidateContext};

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
///
/// This is the **unvalidated** route: unlike [`group_functions_checked`] it applies no
/// label/import/line rule, it only requires that every instruction lowers.
pub fn group_functions(instrs: &[RawInstr]) -> Result<Vec<Function>> {
    group(instrs, &mut None)
}

/// [`group_functions`] with the post-lowering rules applied in the same pass.
///
/// Every instruction is validated by [`validate::validate_instr`] as it lands in a
/// body, and every `Instr::Label` is put through [`DefinedLabels::define`] (the
/// `0 < label < label_count` and the module-wide "defined more than once" rules) while
/// it opens a block — so grouping, block construction and validation share ONE
/// traversal instead of the three (`emit_fused`, `build_blocks`, `validate_code`) that
/// separate passes cost.
///
/// The failures are the same `BadArg` failures, with one documented difference in
/// *order* only: the rules are applied in stream order, as the C applies them while it
/// emits code (`jit/asm_load.c`), whereas the former `validate_code` pass checked every
/// label definition before any per-instruction rule. Two independent failures in one
/// image can therefore be reported in the other order; both are `badfile` to Erlang.
/// The one exception is the `func_info` arity bound — a `SystemLimit` — which is still
/// applied per instruction, and `crate::lower::build_module` reapplies it after
/// grouping through [`validate::validate_module`].
pub(crate) fn group_functions_checked(
    instrs: &[RawInstr],
    ctx: &ValidateContext<'_>,
) -> Result<Vec<Function>> {
    let mut check = Some(Check {
        ctx,
        defined: DefinedLabels::new(ctx.code_version.label_count),
    });
    group(instrs, &mut check)
}

/// The state the checked route carries across a whole module: the module's validation
/// inputs, and the one label-definition table.
///
/// The table is module-wide, not per body, because the C keeps one label array per
/// module (`asm_load.c:523-528`): a repeat across two functions is the same collision
/// as a repeat inside one body.
struct Check<'c, 'm> {
    ctx: &'c ValidateContext<'m>,
    defined: DefinedLabels,
}

/// The shape of one body's fused emit, computed in a single pass over its raw
/// instructions: how many instructions [`emit_fused`] produces and how many blocks
/// (label definitions) it opens.
///
/// Both counts size an allocation, so an inexact one is a memory regression, never a
/// wrong instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BodyShape {
    /// The length of the fused body, i.e. `body.len()` minus the fused pairs.
    instructions: usize,
    /// The number of `label` instructions in the body, i.e. its block count.
    blocks: usize,
}

/// The one structural pass over a raw body: the two counts [`BodyShape`] holds.
///
/// [`lower_instr`] maps `Opcode::deallocate` to [`Instr::Deallocate`], `Opcode::r#return`
/// to [`Instr::Return`], `Opcode::label` to [`Instr::Label`] and every other opcode to
/// exactly one instruction, so a lowered body is element-for-element its raw body: the
/// adjacent `deallocate`/`return` pairs sit at the same positions (and can never
/// overlap, since `deallocate` cannot also be the `return` half of a pair) and the
/// labels are the raw labels. `windows(2)` visits every instruction — as the first
/// element of its pair, and the last one as the second element of the final pair — so
/// one traversal answers both questions.
fn body_shape(body: &[RawInstr]) -> BodyShape {
    let mut shape = BodyShape {
        instructions: body.len(),
        blocks: 0,
    };
    for pair in body.windows(2) {
        if pair[0].opcode == Opcode::deallocate && pair[1].opcode == Opcode::r#return {
            shape.instructions -= 1;
        }
        if pair[0].opcode == Opcode::label {
            shape.blocks += 1;
        }
    }
    if let Some(last) = body.last() {
        if last.opcode == Opcode::label {
            shape.blocks += 1;
        }
    }
    shape
}

/// The shared grouping walk: find the `func_info` cuts, then emit every body.
///
/// `check` is `None` on the unchecked route, and `Some` (one module-wide state) on the
/// checked one; see [`group_functions`] / [`group_functions_checked`].
fn group(instrs: &[RawInstr], check: &mut Option<Check<'_, '_>>) -> Result<Vec<Function>> {
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
        // The body's two counts, from one pass: [`emit_fused`] applies the peephole rule
        // as it lowers, so it emits `body.len() - pairs(body)` instructions, and it opens
        // one block per label. Both allocations are therefore exact and never grow.
        let shape = body_shape(body);
        let mut code: Vec<Instr> = Vec::with_capacity(shape.instructions);
        let mut blocks: Vec<Block> = Vec::with_capacity(shape.blocks);
        let mut labels: BTreeMap<Label, usize> = BTreeMap::new();
        emit_fused(body, &mut code, check, &mut blocks, &mut labels)?;
        // The capacity is exact by construction. If a future change breaks that, the
        // retained body carries slack (a memory regression, never a wrong
        // instruction), so the invariant is asserted here in debug builds and pinned
        // over the whole install by `tests/real_modules.rs::slack_tables` — a
        // `shrink_to_fit` here would instead put a second, transient copy of every
        // function's instruction vector back into the boot peak.
        debug_assert_eq!(code.len(), code.capacity());
        // On the checked route the block/label tables were built as the instructions
        // landed (`blocks`/`labels` above); on the unchecked one they are derived now.
        let (blocks, labels) = if check.is_some() {
            close_blocks(&mut blocks, code.len());
            debug_assert_eq!(blocks.len(), blocks.capacity());
            (blocks, labels)
        } else {
            debug_assert!(blocks.is_empty(), "the unchecked emit opens no block");
            build_blocks(&code)
        };
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
/// `out` must have room for exactly `body.len() - pairs(body)` instructions — the
/// length of the fused sequence — so it never reallocates and the body it becomes has
/// no slack. The rule itself is [`super::fuse::fuses`], the same predicate
/// [`super::fuse::fuse_in_place`] applies, so a body lowered here is
/// element-for-element the body that pass produces: [`lower_instr`] lowers
/// `Opcode::deallocate`/`Opcode::r#return` to [`Instr::Deallocate`]/[`Instr::Return`]
/// and every other opcode to exactly one instruction, so the lowered adjacency the
/// predicate tests is the raw body's.
///
/// Fusing during the emit rather than afterwards is what lets the one allocation be
/// exact: emitting the pre-fusion sequence would overrun a capacity sized for the
/// post-fusion one (by exactly the fused pair count) and grow to twice the final
/// size. Errors are reported in stream order — an instruction is lowered in the
/// iteration that visits it, whether or not its predecessor is still pending.
///
/// When `check` is `Some` (the checked route) each instruction is validated as it is
/// emitted and a label definition is folded into `blocks`/`labels` at the same moment,
/// which is what removes the second and third walks; `blocks` must then have room for
/// exactly `body_shape(body).blocks` entries.
fn emit_fused(
    body: &[RawInstr],
    out: &mut Vec<Instr>,
    check: &mut Option<Check<'_, '_>>,
    blocks: &mut Vec<Block>,
    labels: &mut BTreeMap<Label, usize>,
) -> Result<()> {
    // The lowered instruction whose successor decides whether the pair fuses. Only a
    // `Deallocate` can start a pair, but keeping `Option<Instr>` unconditional is what
    // makes this exactly the adjacency `fuse_in_place` sees.
    let mut pending: Option<Instr> = None;
    for raw in body {
        let instr = lower_instr(raw)?;
        match pending.take() {
            None => pending = Some(instr),
            Some(prev) => match super::fuse::fuses(&prev, &instr) {
                Some(n) => push(out, Instr::DeallocateReturn(n), check, blocks, labels)?,
                None => {
                    push(out, prev, check, blocks, labels)?;
                    pending = Some(instr);
                }
            },
        }
    }
    if let Some(prev) = pending {
        push(out, prev, check, blocks, labels)?;
    }
    Ok(())
}

/// Emits one already-lowered instruction.
///
/// On the checked route it is validated first ([`validate::validate_instr`], the same
/// checks in the same `bad_file` forms the standalone pass applied) and, when it is a
/// label definition, it closes the previous block and opens its own. Validation before
/// the push is deliberate: an instruction that fails is never added to the body.
fn push(
    out: &mut Vec<Instr>,
    instr: Instr,
    check: &mut Option<Check<'_, '_>>,
    blocks: &mut Vec<Block>,
    labels: &mut BTreeMap<Label, usize>,
) -> Result<()> {
    if let Some(check) = check {
        validate::validate_instr(&instr, check.ctx.line, check.ctx.import_count)?;
        if let Instr::Label(label) = &instr {
            check
                .defined
                .define(label.0, check.ctx.code_version.label_count)?;
            open_block(blocks, labels, *label, out.len());
        }
    }
    out.push(instr);
    Ok(())
}

/// Opens the block a `label` definition starts: closes the previous one at `idx`,
/// records the label's instruction index and pushes the new block.
///
/// Shared by [`build_blocks`] and the checked emit, so the two decompositions cannot
/// drift.
fn open_block(
    blocks: &mut Vec<Block>,
    labels: &mut BTreeMap<Label, usize>,
    label: Label,
    idx: usize,
) {
    if let Some(previous) = blocks.last_mut() {
        previous.end = idx;
    }
    labels.insert(label, idx);
    blocks.push(Block {
        label,
        start: idx,
        end: idx,
    });
}

/// Closes the last open block at the end of the body.
fn close_blocks(blocks: &mut [Block], code_len: usize) {
    if let Some(last) = blocks.last_mut() {
        last.end = code_len;
    }
}

/// Decomposes a function body into label-delimited basic blocks.
///
/// Returns the blocks (each `code[start..end]` beginning at its label) and the
/// label → instruction-index map. The map is derived from the `Instr::Label`
/// markers; the blocks are the runs between consecutive labels.
pub fn build_blocks(code: &[Instr]) -> (Vec<Block>, BTreeMap<Label, usize>) {
    // One exact allocation: a body opens one block per `label` definition.
    let mut blocks: Vec<Block> = Vec::with_capacity(
        code.iter()
            .filter(|instr| matches!(instr, Instr::Label(_)))
            .count(),
    );
    let mut labels = BTreeMap::new();
    for (idx, instr) in code.iter().enumerate() {
        if let Instr::Label(label) = instr {
            open_block(&mut blocks, &mut labels, *label, idx);
        }
    }
    close_blocks(&mut blocks, code.len());
    (blocks, labels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{CodeVersion, LineTable};
    use crate::operand::Operand;
    use beam_common::BeamError;

    /// One decoded instruction with the given opcode and operands.
    fn raw(opcode: Opcode, operands: Vec<Operand>) -> RawInstr {
        // `.into()` keeps this helper agnostic to the operand container's type.
        RawInstr {
            opcode,
            operands: operands.into(),
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

    /// `n` line items, i.e. the undefined location plus `n - 1` real ones.
    fn line_table(n: usize) -> LineTable {
        LineTable {
            flags: 0,
            instruction_count: 0,
            items: vec![crate::ir::LineEntry { file: 0, line: 0 }; n],
            files: Vec::new(),
        }
    }

    /// The validation inputs of a module whose `Code` header declares `label_count`
    /// labels and `function_count` functions, with `line_items` line items and
    /// `import_count` imports.
    ///
    /// The line table is leaked so the returned context can borrow it for the whole
    /// test; it is a few hundred bytes, and the test process exits with it.
    fn ctx(
        label_count: u32,
        function_count: u32,
        line_items: usize,
        import_count: usize,
    ) -> ValidateContext<'static> {
        let line: &'static LineTable = Box::leak(Box::new(line_table(line_items)));
        ValidateContext {
            code_version: CodeVersion {
                format_number: 0,
                max_opcode: 181,
                label_count,
                function_count,
            },
            line,
            import_count,
        }
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
    fn build_blocks_allocates_its_blocks_exactly() {
        let code = vec![
            Instr::Label(Label(1)),
            Instr::Return,
            Instr::Label(Label(2)),
            Instr::Return,
            Instr::Label(Label(3)),
            Instr::Return,
        ];
        let (blocks, _) = build_blocks(&code);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks.len(), blocks.capacity());
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
        let shape = body_shape(body);
        let mut emitted: Vec<Instr> = Vec::with_capacity(shape.instructions);
        emit_fused(
            body,
            &mut emitted,
            &mut None,
            &mut Vec::new(),
            &mut BTreeMap::new(),
        )
        .unwrap();
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
        let mut labels = 0usize;
        for pattern in 0..4u32.pow(LEN as u32) {
            let body: Vec<RawInstr> = (0..LEN)
                .map(|i| alphabet(((pattern >> (2 * i)) & 0b11) as u8))
                .collect();
            let (reference, emitted) = both_routes(&body);
            assert_eq!(emitted, reference, "pattern {pattern:06b}");
            bodies += 1;
            let shape = body_shape(&body);
            assert_eq!(shape.instructions, emitted.len(), "pattern {pattern:06b}");
            fused_pairs += LEN - shape.instructions;
            labels += shape.blocks;
        }
        assert_eq!(bodies, 4096);
        // The sweep really did cover the fusion path, not just copy-throughs.
        assert!(fused_pairs > 0);
        // …and the label count, so `BodyShape::blocks` is not vacuously exact.
        assert!(labels > 0);
    }

    #[test]
    fn body_shape_counts_labels_and_fused_pairs_in_one_pass() {
        assert_eq!(
            body_shape(&[]),
            BodyShape {
                instructions: 0,
                blocks: 0
            }
        );
        // A single label is the last (and only) element — the `windows(2)` edge.
        assert_eq!(
            body_shape(&[label(1)]),
            BodyShape {
                instructions: 1,
                blocks: 1
            }
        );
        let body = vec![label(1), deallocate(0), ret(), label(2), ret()];
        assert_eq!(
            body_shape(&body),
            BodyShape {
                instructions: 4,
                blocks: 2
            }
        );
        // A label as the very last instruction is counted exactly once.
        let trailing = vec![ret(), label(2)];
        assert_eq!(
            body_shape(&trailing),
            BodyShape {
                instructions: 2,
                blocks: 1
            }
        );
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
            emit_fused(
                &body,
                &mut out,
                &mut None,
                &mut Vec::new(),
                &mut BTreeMap::new()
            ),
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
            assert_eq!(f.blocks.len(), f.blocks.capacity());
        }
    }

    // -----------------------------------------------------------------------
    // The checked route: one pass for lowering, blocks and validation
    // -----------------------------------------------------------------------

    /// A two-function stream whose first body has three labels and one fusable pair.
    fn two_function_stream(module: Atom, function: Atom) -> Vec<RawInstr> {
        vec![
            label(1),
            func_info(module, function, 1),
            label(2),
            deallocate(1),
            ret(),
            label(3),
            raw(Opcode::line, vec![Operand::Integer(7)]),
            func_info(module, function, 2),
            label(4),
            ret(),
        ]
    }

    #[test]
    fn checked_grouping_yields_the_same_code_blocks_and_labels_as_the_unchecked_one() {
        let (module, function) = atoms();
        let instrs = two_function_stream(module, function);
        let unchecked = group_functions(&instrs).unwrap();
        // Eight line items: the stream's `line 7` reference resolves.
        let context = ctx(5, 2, 8, 0);
        let checked = group_functions_checked(&instrs, &context).unwrap();

        assert_eq!(checked.len(), unchecked.len());
        for (checked, unchecked) in checked.iter().zip(&unchecked) {
            assert_eq!(checked.code, unchecked.code);
            assert_eq!(checked.blocks, unchecked.blocks);
            assert_eq!(checked.labels, unchecked.labels);
            // …and both are exactly the standalone decomposition.
            let (blocks, labels) = build_blocks(&checked.code);
            assert_eq!(checked.blocks, blocks);
            assert_eq!(checked.labels, labels);
            // The incremental block table is allocated exactly, too.
            assert_eq!(checked.blocks.len(), checked.blocks.capacity());
            assert_eq!(checked.code.len(), checked.code.capacity());
        }
        assert_eq!(checked[0].blocks.len(), 2);
        assert_eq!(checked[1].blocks.len(), 2);
    }

    #[test]
    fn checked_grouping_rejects_a_label_definition_of_zero() {
        let (module, function) = atoms();
        let instrs = vec![label(0), func_info(module, function, 0), label(2), ret()];
        let context = ctx(3, 1, 0, 0);
        assert!(matches!(
            group_functions_checked(&instrs, &context).unwrap_err(),
            BeamError::BadArg
        ));
    }

    #[test]
    fn checked_grouping_rejects_a_label_at_or_above_label_count() {
        let (module, function) = atoms();
        let instrs = vec![label(1), func_info(module, function, 0), label(4), ret()];
        // Labels 1..3 are the declared space, so 4 is out of range.
        let context = ctx(4, 1, 0, 0);
        assert!(matches!(
            group_functions_checked(&instrs, &context).unwrap_err(),
            BeamError::BadArg
        ));
        // Control: the same stream with room for label 4 loads.
        let context = ctx(5, 1, 0, 0);
        group_functions_checked(&instrs, &context).unwrap();
    }

    #[test]
    fn checked_grouping_rejects_a_duplicate_label_inside_one_body() {
        let (module, function) = atoms();
        let instrs = vec![
            label(1),
            func_info(module, function, 0),
            label(2),
            ret(),
            label(2),
            ret(),
        ];
        let context = ctx(4, 1, 0, 0);
        assert!(matches!(
            group_functions_checked(&instrs, &context).unwrap_err(),
            BeamError::BadArg
        ));
    }

    #[test]
    fn checked_grouping_rejects_a_duplicate_label_across_two_functions() {
        // The label table is module-wide, exactly like the C's one array per module.
        let (module, function) = atoms();
        let instrs = vec![
            label(1),
            func_info(module, function, 0),
            label(2),
            ret(),
            label(2),
            func_info(module, function, 1),
            label(3),
            ret(),
        ];
        let context = ctx(5, 2, 0, 0);
        assert!(matches!(
            group_functions_checked(&instrs, &context).unwrap_err(),
            BeamError::BadArg
        ));
        // Control: a distinct second function label loads.
        let instrs = vec![
            label(1),
            func_info(module, function, 0),
            label(2),
            ret(),
            label(3),
            func_info(module, function, 1),
            label(4),
            ret(),
        ];
        group_functions_checked(&instrs, &context).unwrap();
    }

    #[test]
    fn the_duplicate_rule_of_the_checked_route_applies_without_a_code_header() {
        // `label_count == 0` (a hand-built module) skips the range rule but not the
        // duplicate one — the tree fallback path.
        let (module, function) = atoms();
        let instrs = vec![label(1), func_info(module, function, 0), label(1), ret()];
        let context = ctx(0, 1, 0, 0);
        assert!(matches!(
            group_functions_checked(&instrs, &context).unwrap_err(),
            BeamError::BadArg
        ));
        // A zero label is legal without a header.
        let instrs = vec![label(0), func_info(module, function, 0), label(2), ret()];
        group_functions_checked(&instrs, &context).unwrap();
    }

    #[test]
    fn checked_grouping_rejects_an_import_index_past_the_import_table() {
        let (module, function) = atoms();
        // `call_ext 0, 7`: an import index of 7 with no imports at all.
        let instrs = vec![
            label(1),
            func_info(module, function, 0),
            label(2),
            raw(
                Opcode::call_ext,
                vec![Operand::Integer(0), Operand::Integer(7)],
            ),
            ret(),
        ];
        let context = ctx(3, 1, 0, 0);
        assert!(matches!(
            group_functions_checked(&instrs, &context).unwrap_err(),
            BeamError::BadArg
        ));
        // An eight-entry import table: index 7 resolves.
        let context = ctx(3, 1, 0, 8);
        group_functions_checked(&instrs, &context).unwrap();
    }

    #[test]
    fn checked_grouping_rejects_a_line_reference_past_the_item_table() {
        let (module, function) = atoms();
        let instrs = vec![
            label(1),
            func_info(module, function, 0),
            label(2),
            raw(Opcode::line, vec![Operand::Integer(5)]),
            ret(),
        ];
        let context = ctx(3, 1, 2, 0);
        assert!(matches!(
            group_functions_checked(&instrs, &context).unwrap_err(),
            BeamError::BadArg
        ));
        // An absent line table skips the rule, exactly as `add_line_entry` does.
        let context = ctx(3, 1, 0, 0);
        group_functions_checked(&instrs, &context).unwrap();
    }

    #[test]
    fn the_checked_route_validates_a_fused_instruction_once() {
        // A `deallocate`/`return` pair is validated as the fused
        // `DeallocateReturn`; neither half carries an import or line reference, so the
        // pair that survives the fuse is exactly as valid as the two it replaces.
        let (module, function) = atoms();
        let instrs = vec![
            label(1),
            func_info(module, function, 0),
            label(2),
            deallocate(1),
            ret(),
        ];
        let context = ctx(3, 1, 0, 0);
        let functions = group_functions_checked(&instrs, &context).unwrap();
        assert_eq!(
            functions[0].code,
            vec![
                Instr::Label(Label(1)),
                Instr::FuncInfo {
                    module,
                    function,
                    arity: 0
                },
                Instr::Label(Label(2)),
                Instr::DeallocateReturn(1),
            ]
        );
    }
}
