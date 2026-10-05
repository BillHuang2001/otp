//! Lowering: raw chunks + raw instructions → the typed IR.
//!
//! This module owns the semantic step between the wire level ([`crate::chunk`],
//! [`crate::decode`]) and the frozen IR ([`crate::ir`]):
//!
//! * [`group_functions`] splits the module-wide instruction stream at `func_info`
//!   boundaries and attaches each function's `func_label` / `entry_label` (the file
//!   sequence is `label FuncLabel | [line] | func_info M F A | label EntryLabel |
//!   body…`; `beam_file.c:2502-2589`).
//! * [`build_blocks`] decomposes a function body into label-delimited basic blocks
//!   and builds the label → instruction-index map.
//! * [`build_module`] assembles the [`Module`]: interned atoms, exports, imports,
//!   functions, the type/line tables and the raw data chunks.
//!
//! The per-opcode vocabulary mapping (one decoded instruction → one IR [`Instr`])
//! lives in the private submodule `instr`, which keeps this file to the
//! grouping/assembly responsibilities.
//!
//! Every fallible step reports `BeamError::BadArg` through [`crate::error::bad_file`]
//! (or `BeamError::SystemLimit` for a size bound); the module never panics and never
//! `unwrap()`s file data.
use std::collections::BTreeMap;

use beam_common::Result;
use beam_term::Atom;

use crate::chunk::Chunks;
use crate::decode::RawInstr;
use crate::error::bad_file;
use crate::ir::{Block, Export, Function, Import, Instr, Module};
use crate::operand::Label;

mod instr;
use instr::lower_instr;
/// Assembles the IR [`Module`] from the parsed chunks and the decoded instructions.
///
/// `atom_table` is the interning context the module's atoms are resolved through
/// (seeding it with `atom.names` for preloaded/positional atoms is `beam_rt`'s
/// job). `raw_instrs` is the whole-module instruction stream from
/// [`crate::decode::decode_stream`]; `chunks` supplies the tables and data chunks.
///
/// The `AtU8` names are re-interned through `atom_table` in wire order; interning is
/// idempotent, so this returns exactly the [`Atom`]s the decoder already embedded in
/// `raw_instrs` and `Module.atoms` stays consistent with them.
pub fn build_module(
    chunks: &Chunks<'_>,
    raw_instrs: &[RawInstr],
    mut atom_table: beam_term::AtomTable,
) -> Result<Module> {
    // `atoms[0]` is the NIL placeholder; `atoms[1]` is the module name. Without the
    // module name there is nothing to load.
    if chunks.atoms.len() < 2 {
        return Err(bad_file("atom table has no module name"));
    }

    let mut atoms: Vec<Atom> = Vec::with_capacity(chunks.atoms.len());
    atoms.push(Atom::MAX);
    for name in &chunks.atoms[1..] {
        atoms.push(atom_table.intern(name));
    }
    let name = atoms[1];

    // `ExpT`/`ImpT` carry wire atom indices; map them through `atoms` (file order is
    // preserved — exports are deliberately *not* sorted).
    let mut exports = Vec::with_capacity(chunks.exports.len());
    for raw in &chunks.exports {
        exports.push(Export {
            function: atom_at(&atoms, raw.function)?,
            arity: raw.arity,
            label: Label(raw.label),
        });
    }

    let mut imports = Vec::with_capacity(chunks.imports.len());
    for raw in &chunks.imports {
        imports.push(Import {
            module: atom_at(&atoms, raw.module)?,
            function: atom_at(&atoms, raw.function)?,
            arity: raw.arity,
        });
    }

    let functions = group_functions(raw_instrs)?;

    Ok(Module {
        name,
        atoms,
        atom_table,
        exports,
        imports,
        functions,
        code_version: chunks.code_version,
        types: chunks.types.clone(),
        line: chunks.line.clone().unwrap_or_default(),
        literals: chunks.literals.clone(),
        strings: chunks.strings.clone(),
        attributes: chunks.attributes.clone(),
        compile_info: chunks.compile_info.clone(),
    })
}

/// Resolves a wire atom index through the module's `atoms` map.
fn atom_at(atoms: &[Atom], index: u32) -> Result<Atom> {
    atoms
        .get(index as usize)
        .copied()
        .ok_or_else(|| bad_file("atom index out of range"))
}

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
    use crate::ir::ImportRef;
    use crate::opcode::Opcode;
    use crate::operand::{Operand, NO_FAIL};
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
        let instrs = vec![
            label(1),
            func_info(module, function, 1),
            raw(Opcode::r#return, vec![]),
        ];
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
    fn unmodelled_opcodes_round_trip_as_generic() {
        let instrs = vec![
            label(1),
            func_info(Atom::MAX, Atom::MAX, 0),
            label(2),
            raw(
                Opcode::allocate_zero,
                vec![Operand::Integer(1), Operand::Integer(2)],
            ),
            raw(Opcode::r#return, vec![]),
        ];
        let f = &group_functions(&instrs).unwrap()[0];
        assert_eq!(
            f.code[3],
            Instr::Generic {
                op: Opcode::allocate_zero,
                operands: vec![Operand::Integer(1), Operand::Integer(2)],
                offset: 0,
            }
        );
    }

    #[test]
    fn a_test_instruction_with_a_non_label_failure_operand_is_rejected() {
        // `is_eq_exact` expects `Fail Arg…`; a plain integer is not a label.
        let instrs = vec![
            label(1),
            func_info(Atom::MAX, Atom::MAX, 0),
            label(2),
            raw(
                Opcode::is_eq_exact,
                vec![Operand::Integer(1), Operand::X(0), Operand::Integer(0)],
            ),
            raw(Opcode::r#return, vec![]),
        ];
        assert!(matches!(
            group_functions(&instrs).unwrap_err(),
            BeamError::BadArg
        ));
    }

    #[test]
    fn a_move_with_missing_operands_is_rejected() {
        let instrs = vec![
            label(1),
            func_info(Atom::MAX, Atom::MAX, 0),
            label(2),
            raw(Opcode::r#move, vec![Operand::X(0)]),
            raw(Opcode::r#return, vec![]),
        ];
        assert!(matches!(
            group_functions(&instrs).unwrap_err(),
            BeamError::BadArg
        ));
    }

    // ---- the newly modelled opcode families ------------------------------

    /// Lowers one instruction and unwraps the result (test shorthand).
    fn lower(opcode: Opcode, operands: Vec<Operand>) -> Instr {
        lower_instr(&raw(opcode, operands)).unwrap()
    }

    #[test]
    fn lowers_jump_init_yregs_and_put_tuple2() {
        assert_eq!(
            lower(Opcode::jump, vec![Operand::Label(Label(7))]),
            Instr::Jump { target: Label(7) }
        );
        assert_eq!(
            lower(
                Opcode::init_yregs,
                vec![Operand::List(vec![Operand::Y(0), Operand::Y(1)])]
            ),
            Instr::InitYregs {
                registers: vec![Operand::Y(0), Operand::Y(1)]
            }
        );
        assert_eq!(
            lower(
                Opcode::put_tuple2,
                vec![
                    Operand::X(0),
                    Operand::List(vec![Operand::X(1), Operand::Integer(2)]),
                ]
            ),
            Instr::PutTuple2 {
                dst: Operand::X(0),
                elements: vec![Operand::X(1), Operand::Integer(2)],
            }
        );
    }

    #[test]
    fn lowers_get_tuple_element_and_select_tuple_arity() {
        assert_eq!(
            lower(
                Opcode::get_tuple_element,
                vec![Operand::X(1), Operand::Integer(2), Operand::Y(0)],
            ),
            Instr::GetTupleElement {
                source: Operand::X(1),
                element: 2,
                dst: Operand::Y(0),
            }
        );
        assert_eq!(
            lower(
                Opcode::select_tuple_arity,
                vec![
                    Operand::X(0),
                    Operand::Label(NO_FAIL),
                    Operand::List(vec![
                        Operand::Integer(2),
                        Operand::Label(Label(3)),
                        Operand::Integer(3),
                        Operand::Label(Label(4)),
                    ]),
                ],
            ),
            Instr::SelectTupleArity {
                tuple: Operand::X(0),
                fail: NO_FAIL,
                destinations: vec![
                    (Operand::Integer(2), Label(3)),
                    (Operand::Integer(3), Label(4)),
                ],
            }
        );
        // An odd-length alternative list is a structural mismatch.
        assert!(matches!(
            lower_instr(&raw(
                Opcode::select_tuple_arity,
                vec![
                    Operand::X(0),
                    Operand::Label(Label(1)),
                    Operand::List(vec![Operand::Integer(2)]),
                ],
            ))
            .unwrap_err(),
            BeamError::BadArg
        ));
    }

    #[test]
    fn lowers_the_list_instructions() {
        assert_eq!(
            lower(
                Opcode::put_list,
                vec![Operand::X(0), Operand::Nil, Operand::X(1)],
            ),
            Instr::PutList {
                head: Operand::X(0),
                tail: Operand::Nil,
                dst: Operand::X(1),
            }
        );
        assert_eq!(
            lower(
                Opcode::get_list,
                vec![Operand::X(0), Operand::X(1), Operand::Y(0)],
            ),
            Instr::GetList {
                source: Operand::X(0),
                head: Operand::X(1),
                tail: Operand::Y(0),
            }
        );
        assert_eq!(
            lower(Opcode::get_hd, vec![Operand::X(0), Operand::Y(1)]),
            Instr::GetHd {
                source: Operand::X(0),
                head: Operand::Y(1),
            }
        );
        assert_eq!(
            lower(Opcode::get_tl, vec![Operand::X(0), Operand::Y(1)]),
            Instr::GetTl {
                source: Operand::X(0),
                tail: Operand::Y(1),
            }
        );
    }

    #[test]
    fn lowers_call_fun_and_call_fun2() {
        assert_eq!(
            lower(Opcode::call_fun, vec![Operand::Integer(2)]),
            Instr::CallFun { arity: 2 }
        );
        // A statically known target keeps the raw fun-table index; the atom forms
        // (`safe`/`unsafe`) are raw operands too.
        let (safe, _) = atoms();
        assert_eq!(
            lower(
                Opcode::call_fun2,
                vec![Operand::Atom(safe), Operand::Integer(1), Operand::X(1)],
            ),
            Instr::CallFun2 {
                tag: Operand::Atom(safe),
                arity: 1,
                func: Operand::X(1),
            }
        );
        assert_eq!(
            lower(
                Opcode::call_fun2,
                vec![Operand::Integer(9), Operand::Integer(0), Operand::X(0)],
            ),
            Instr::CallFun2 {
                tag: Operand::Integer(9),
                arity: 0,
                func: Operand::X(0),
            }
        );
    }

    #[test]
    fn lowers_apply_and_make_fun3() {
        assert_eq!(
            lower(Opcode::apply, vec![Operand::Integer(3)]),
            Instr::Apply { arity: 3 }
        );
        let last = vec![Operand::Integer(2), Operand::Integer(1)];
        assert_eq!(
            lower(Opcode::apply_last, last),
            Instr::ApplyLast {
                arity: 2,
                deallocate: 1,
            }
        );
        assert_eq!(
            lower(
                Opcode::make_fun3,
                vec![
                    Operand::Integer(5),
                    Operand::X(0),
                    Operand::List(vec![Operand::Y(0), Operand::Y(1)]),
                ],
            ),
            Instr::MakeFun3 {
                index: 5,
                dst: Operand::X(0),
                env: vec![Operand::Y(0), Operand::Y(1)],
            }
        );
    }

    #[test]
    fn lowers_the_bif_family() {
        // `bif0 Bif Dst`.
        assert_eq!(
            lower(Opcode::bif0, vec![Operand::Integer(7), Operand::X(0)]),
            Instr::Bif0 {
                bif: ImportRef(7),
                dst: Operand::X(0),
            }
        );
        // `bif2 Fail Bif Arg Arg Dst` — arity is derived from the operand count.
        assert_eq!(
            lower(
                Opcode::bif2,
                vec![
                    Operand::Label(Label(1)),
                    Operand::Integer(3),
                    Operand::X(0),
                    Operand::X(1),
                    Operand::X(2),
                ],
            ),
            Instr::Bif {
                arity: 2,
                bif: ImportRef(3),
                fail: Label(1),
                args: vec![Operand::X(0), Operand::X(1)],
                dst: Operand::X(2),
            }
        );
        // A non-label failure operand is a structural mismatch.
        assert!(matches!(
            lower_instr(&raw(
                Opcode::bif1,
                vec![
                    Operand::Integer(0),
                    Operand::Integer(3),
                    Operand::X(0),
                    Operand::X(1),
                ],
            ))
            .unwrap_err(),
            BeamError::BadArg
        ));
    }

    #[test]
    fn lowers_the_exception_instructions() {
        assert_eq!(
            lower(Opcode::badmatch, vec![Operand::X(0)]),
            Instr::Badmatch {
                value: Operand::X(0)
            }
        );
        assert_eq!(
            lower(Opcode::case_end, vec![Operand::X(1)]),
            Instr::CaseEnd {
                value: Operand::X(1)
            }
        );
        assert_eq!(lower(Opcode::if_end, vec![]), Instr::IfEnd);
        let try_operands = vec![Operand::Y(0), Operand::Label(Label(2))];
        assert_eq!(
            lower(Opcode::r#try, try_operands),
            Instr::Try {
                frame: Operand::Y(0),
                fail: Label(2),
            }
        );
        assert_eq!(
            lower(Opcode::try_end, vec![Operand::Y(0)]),
            Instr::TryEnd {
                frame: Operand::Y(0)
            }
        );
        assert_eq!(
            lower(Opcode::catch_end, vec![Operand::Y(0)]),
            Instr::CatchEnd {
                frame: Operand::Y(0)
            }
        );
        assert_eq!(
            lower(Opcode::try_case, vec![Operand::Y(0)]),
            Instr::TryCase {
                frame: Operand::Y(0)
            }
        );
        assert_eq!(
            lower(Opcode::try_case_end, vec![Operand::X(0)]),
            Instr::TryCaseEnd {
                value: Operand::X(0)
            }
        );
        let catch_operands = vec![Operand::Y(0), Operand::Label(Label(3))];
        assert_eq!(
            lower(Opcode::catch, catch_operands),
            Instr::Catch {
                frame: Operand::Y(0),
                fail: Label(3),
            }
        );
        assert_eq!(
            lower(Opcode::raise, vec![Operand::Y(1), Operand::Y(2)]),
            Instr::Raise {
                trace: Operand::Y(1),
                value: Operand::Y(2),
            }
        );
        assert_eq!(
            lower(Opcode::badrecord, vec![Operand::X(0)]),
            Instr::Badrecord {
                value: Operand::X(0)
            }
        );
        assert_eq!(
            lower(Opcode::build_stacktrace, vec![]),
            Instr::BuildStacktrace
        );
        assert_eq!(lower(Opcode::raw_raise, vec![]), Instr::RawRaise);
    }

    #[test]
    fn lowers_the_float_instructions() {
        assert_eq!(
            lower(Opcode::fmove, vec![Operand::FloatReg(0), Operand::X(1)]),
            Instr::Fmove {
                src: Operand::FloatReg(0),
                dst: Operand::X(1),
            }
        );
        assert_eq!(
            lower(Opcode::fconv, vec![Operand::X(0), Operand::FloatReg(1)]),
            Instr::Fconv {
                src: Operand::X(0),
                dst: Operand::FloatReg(1),
            }
        );
        assert_eq!(
            lower(
                Opcode::fadd,
                vec![
                    Operand::Label(Label(4)),
                    Operand::FloatReg(0),
                    Operand::FloatReg(1),
                    Operand::FloatReg(2),
                ],
            ),
            Instr::Fadd {
                fail: Label(4),
                lhs: Operand::FloatReg(0),
                rhs: Operand::FloatReg(1),
                dst: Operand::FloatReg(2),
            }
        );
        assert_eq!(
            lower(
                Opcode::fdiv,
                vec![
                    Operand::Label(NO_FAIL),
                    Operand::FloatReg(0),
                    Operand::FloatReg(1),
                    Operand::FloatReg(0),
                ],
            ),
            Instr::Fdiv {
                fail: NO_FAIL,
                lhs: Operand::FloatReg(0),
                rhs: Operand::FloatReg(1),
                dst: Operand::FloatReg(0),
            }
        );
        assert_eq!(
            lower(
                Opcode::fnegate,
                vec![
                    Operand::Label(NO_FAIL),
                    Operand::FloatReg(0),
                    Operand::FloatReg(1),
                ],
            ),
            Instr::Fnegate {
                fail: NO_FAIL,
                src: Operand::FloatReg(0),
                dst: Operand::FloatReg(1),
            }
        );
    }

    #[test]
    fn lowers_the_receive_instructions() {
        let (tag, _) = atoms();
        assert_eq!(lower(Opcode::send, vec![]), Instr::Send);
        assert_eq!(lower(Opcode::timeout, vec![]), Instr::Timeout);
        assert_eq!(lower(Opcode::remove_message, vec![]), Instr::RemoveMessage);
        assert_eq!(
            lower(
                Opcode::loop_rec,
                vec![Operand::Label(Label(1)), Operand::X(0)],
            ),
            Instr::LoopRec {
                fail: Label(1),
                dst: Operand::X(0),
            }
        );
        assert_eq!(
            lower(Opcode::loop_rec_end, vec![Operand::Label(Label(2))]),
            Instr::LoopRecEnd { target: Label(2) }
        );
        assert_eq!(
            lower(Opcode::wait, vec![Operand::Label(Label(3))]),
            Instr::Wait { target: Label(3) }
        );
        assert_eq!(
            lower(
                Opcode::wait_timeout,
                vec![Operand::Label(Label(4)), Operand::Integer(1000)],
            ),
            Instr::WaitTimeout {
                target: Label(4),
                time: Operand::Integer(1000),
            }
        );
        assert_eq!(
            lower(Opcode::recv_marker_reserve, vec![Operand::Y(0)]),
            Instr::RecvMarkerReserve {
                marker: Operand::Y(0)
            }
        );
        assert_eq!(
            lower(Opcode::recv_marker_bind, vec![Operand::Y(0), Operand::X(0)]),
            Instr::RecvMarkerBind {
                marker: Operand::Y(0),
                reference: Operand::X(0),
            }
        );
        assert_eq!(
            lower(Opcode::recv_marker_clear, vec![Operand::X(0)]),
            Instr::RecvMarkerClear {
                reference: Operand::X(0)
            }
        );
        assert_eq!(
            lower(Opcode::recv_marker_use, vec![Operand::Atom(tag)]),
            Instr::RecvMarkerUse {
                reference: Operand::Atom(tag)
            }
        );
    }

    #[test]
    fn lowers_the_line_markers() {
        assert_eq!(
            lower(
                Opcode::executable_line,
                vec![Operand::Integer(2), Operand::Integer(9)],
            ),
            Instr::ExecutableLine {
                location: 2,
                index: 9,
            }
        );
        let (kind, _) = atoms();
        assert_eq!(
            lower(
                Opcode::debug_line,
                vec![
                    Operand::Atom(kind),
                    Operand::Integer(3),
                    Operand::Integer(7),
                    Operand::Integer(1),
                ],
            ),
            Instr::DebugLine {
                kind: Operand::Atom(kind),
                location: 3,
                index: 7,
                live: 1,
            }
        );
    }

    #[test]
    fn lowers_has_map_fields_and_get_map_elements() {
        let (key, _) = atoms();
        assert_eq!(
            lower(
                Opcode::has_map_fields,
                vec![
                    Operand::Label(Label(1)),
                    Operand::X(0),
                    Operand::List(vec![Operand::Atom(key), Operand::Integer(1)]),
                ],
            ),
            Instr::HasMapFields {
                fail: Label(1),
                source: Operand::X(0),
                keys: vec![Operand::Atom(key), Operand::Integer(1)],
            }
        );
        assert_eq!(
            lower(
                Opcode::get_map_elements,
                vec![
                    Operand::Label(Label(2)),
                    Operand::X(0),
                    Operand::List(vec![Operand::Atom(key), Operand::X(1)]),
                ],
            ),
            Instr::GetMapElements {
                fail: Label(2),
                source: Operand::X(0),
                pairs: vec![(Operand::Atom(key), Operand::X(1))],
            }
        );
        // An odd-length key/value list is a structural mismatch.
        assert!(matches!(
            lower_instr(&raw(
                Opcode::get_map_elements,
                vec![
                    Operand::Label(Label(2)),
                    Operand::X(0),
                    Operand::List(vec![Operand::Atom(key)]),
                ],
            ))
            .unwrap_err(),
            BeamError::BadArg
        ));
    }

    #[test]
    fn lowers_put_map_assoc_and_put_map_exact() {
        let (key, value) = atoms();
        let operands = |fail: Label, live: u32| {
            vec![
                Operand::Label(fail),
                Operand::X(0),
                Operand::X(1),
                Operand::Integer(i64::from(live)),
                Operand::List(vec![Operand::Atom(key), Operand::Atom(value)]),
            ]
        };
        assert_eq!(
            lower(Opcode::put_map_assoc, operands(NO_FAIL, 2)),
            Instr::PutMapAssoc {
                fail: NO_FAIL,
                source: Operand::X(0),
                dst: Operand::X(1),
                live: 2,
                pairs: vec![(Operand::Atom(key), Operand::Atom(value))],
            }
        );
        assert_eq!(
            lower(Opcode::put_map_exact, operands(Label(5), 3)),
            Instr::PutMapExact {
                fail: Label(5),
                source: Operand::X(0),
                dst: Operand::X(1),
                live: 3,
                pairs: vec![(Operand::Atom(key), Operand::Atom(value))],
            }
        );
    }

    #[test]
    fn lowers_update_record() {
        let (hint, _) = atoms();
        assert_eq!(
            lower(
                Opcode::update_record,
                vec![
                    Operand::Atom(hint),
                    Operand::Integer(4),
                    Operand::X(0),
                    Operand::X(1),
                    Operand::List(vec![
                        Operand::Integer(2),
                        Operand::X(2),
                        Operand::Integer(3),
                        Operand::Y(0),
                    ]),
                ],
            ),
            Instr::UpdateRecord {
                hint: Operand::Atom(hint),
                size: 4,
                src: Operand::X(0),
                dst: Operand::X(1),
                updates: vec![
                    (Operand::Integer(2), Operand::X(2)),
                    (Operand::Integer(3), Operand::Y(0)),
                ],
            }
        );
    }

    #[test]
    fn lowers_the_binary_match_instructions() {
        assert_eq!(
            lower(
                Opcode::bs_start_match3,
                vec![
                    Operand::Label(Label(1)),
                    Operand::X(0),
                    Operand::Integer(2),
                    Operand::X(1),
                ],
            ),
            Instr::BsStartMatch3 {
                fail: Label(1),
                bin: Operand::X(0),
                live: 2,
                dst: Operand::X(1),
            }
        );
        // `bs_start_match4` keeps its failure operand raw (an atom `no_fail`).
        let (no_fail, _) = atoms();
        assert_eq!(
            lower(
                Opcode::bs_start_match4,
                vec![
                    Operand::Atom(no_fail),
                    Operand::Integer(1),
                    Operand::X(0),
                    Operand::X(1),
                ],
            ),
            Instr::BsStartMatch4 {
                fail: Operand::Atom(no_fail),
                live: 1,
                src: Operand::X(0),
                dst: Operand::X(1),
            }
        );
        assert_eq!(
            lower(
                Opcode::bs_match,
                vec![
                    Operand::Label(NO_FAIL),
                    Operand::X(0),
                    Operand::List(vec![Operand::Integer(1), Operand::Integer(2)]),
                ],
            ),
            Instr::BsMatch {
                fail: NO_FAIL,
                ctx: Operand::X(0),
                commands: vec![Operand::Integer(1), Operand::Integer(2)],
            }
        );
        assert_eq!(
            lower(
                Opcode::bs_match_string,
                vec![
                    Operand::Label(NO_FAIL),
                    Operand::X(0),
                    Operand::Integer(16),
                    Operand::Integer(0),
                ],
            ),
            Instr::BsMatchString {
                fail: NO_FAIL,
                ctx: Operand::X(0),
                bits: 16,
                offset: 0,
            }
        );
        assert_eq!(
            lower(
                Opcode::bs_get_tail,
                vec![Operand::X(0), Operand::X(1), Operand::Integer(2)],
            ),
            Instr::BsGetTail {
                ctx: Operand::X(0),
                dst: Operand::X(1),
                live: 2,
            }
        );
        assert_eq!(
            lower(
                Opcode::bs_get_position,
                vec![Operand::X(0), Operand::Y(1), Operand::Integer(2)],
            ),
            Instr::BsGetPosition {
                ctx: Operand::X(0),
                dst: Operand::Y(1),
                live: 2,
            }
        );
        let pos = vec![Operand::X(0), Operand::Y(1)];
        assert_eq!(
            lower(Opcode::bs_set_position, pos),
            Instr::BsSetPosition {
                ctx: Operand::X(0),
                pos: Operand::Y(1),
            }
        );
        assert_eq!(
            lower(
                Opcode::bs_get_integer2,
                vec![
                    Operand::Label(Label(1)),
                    Operand::X(0),
                    Operand::Integer(2),
                    Operand::X(1),
                    Operand::Integer(8),
                    Operand::Integer(0),
                    Operand::X(2),
                ],
            ),
            Instr::BsGetInteger2 {
                fail: Label(1),
                ctx: Operand::X(0),
                live: 2,
                size: Operand::X(1),
                unit: 8,
                flags: 0,
                dst: Operand::X(2),
            }
        );
        assert_eq!(
            lower(
                Opcode::bs_get_binary2,
                vec![
                    Operand::Label(NO_FAIL),
                    Operand::X(0),
                    Operand::Integer(1),
                    Operand::Atom(Atom::MAX),
                    Operand::Integer(8),
                    Operand::Integer(0),
                    Operand::X(2),
                ],
            ),
            Instr::BsGetBinary2 {
                fail: NO_FAIL,
                ctx: Operand::X(0),
                live: 1,
                size: Operand::Atom(Atom::MAX),
                unit: 8,
                flags: 0,
                dst: Operand::X(2),
            }
        );
        assert_eq!(
            lower(
                Opcode::bs_get_float2,
                vec![
                    Operand::Label(NO_FAIL),
                    Operand::X(0),
                    Operand::Integer(1),
                    Operand::Integer(64),
                    Operand::Integer(1),
                    Operand::Integer(0),
                    Operand::FloatReg(0),
                ],
            ),
            Instr::BsGetFloat2 {
                fail: NO_FAIL,
                ctx: Operand::X(0),
                live: 1,
                size: Operand::Integer(64),
                unit: 1,
                flags: 0,
                dst: Operand::FloatReg(0),
            }
        );
        assert_eq!(
            lower(
                Opcode::bs_skip_bits2,
                vec![
                    Operand::Label(NO_FAIL),
                    Operand::X(0),
                    Operand::Integer(8),
                    Operand::Integer(1),
                    Operand::Integer(0),
                ],
            ),
            Instr::BsSkipBits2 {
                fail: NO_FAIL,
                ctx: Operand::X(0),
                size: Operand::Integer(8),
                unit: 1,
                flags: 0,
            }
        );
        assert_eq!(
            lower(
                Opcode::bs_test_tail2,
                vec![Operand::Label(Label(2)), Operand::X(0), Operand::Integer(0)],
            ),
            Instr::BsTestTail2 {
                fail: Label(2),
                ctx: Operand::X(0),
                bits: 0,
            }
        );
    }

    #[test]
    fn lowers_the_utf_and_binary_building_instructions() {
        assert_eq!(
            lower(
                Opcode::bs_get_utf8,
                vec![
                    Operand::Label(NO_FAIL),
                    Operand::X(0),
                    Operand::Integer(1),
                    Operand::Integer(0),
                    Operand::X(1),
                ],
            ),
            Instr::BsGetUtf8 {
                fail: NO_FAIL,
                ctx: Operand::X(0),
                live: 1,
                flags: 0,
                dst: Operand::X(1),
            }
        );
        assert_eq!(
            lower(
                Opcode::bs_get_utf16,
                vec![
                    Operand::Label(NO_FAIL),
                    Operand::X(0),
                    Operand::Integer(1),
                    Operand::Integer(0),
                    Operand::X(1),
                ],
            ),
            Instr::BsGetUtf16 {
                fail: NO_FAIL,
                ctx: Operand::X(0),
                live: 1,
                flags: 0,
                dst: Operand::X(1),
            }
        );
        assert_eq!(
            lower(
                Opcode::bs_get_utf32,
                vec![
                    Operand::Label(NO_FAIL),
                    Operand::X(0),
                    Operand::Integer(1),
                    Operand::Integer(0),
                    Operand::X(1),
                ],
            ),
            Instr::BsGetUtf32 {
                fail: NO_FAIL,
                ctx: Operand::X(0),
                live: 1,
                flags: 0,
                dst: Operand::X(1),
            }
        );
        assert_eq!(
            lower(
                Opcode::bs_skip_utf8,
                vec![
                    Operand::Label(NO_FAIL),
                    Operand::X(0),
                    Operand::Integer(1),
                    Operand::Integer(0),
                ],
            ),
            Instr::BsSkipUtf8 {
                fail: NO_FAIL,
                ctx: Operand::X(0),
                live: 1,
                flags: 0,
            }
        );
        assert_eq!(
            lower(
                Opcode::bs_skip_utf16,
                vec![
                    Operand::Label(NO_FAIL),
                    Operand::X(0),
                    Operand::Integer(1),
                    Operand::Integer(0),
                ],
            ),
            Instr::BsSkipUtf16 {
                fail: NO_FAIL,
                ctx: Operand::X(0),
                live: 1,
                flags: 0,
            }
        );
        assert_eq!(
            lower(
                Opcode::bs_skip_utf32,
                vec![
                    Operand::Label(NO_FAIL),
                    Operand::X(0),
                    Operand::Integer(1),
                    Operand::Integer(0),
                ],
            ),
            Instr::BsSkipUtf32 {
                fail: NO_FAIL,
                ctx: Operand::X(0),
                live: 1,
                flags: 0,
            }
        );
        assert_eq!(
            lower(Opcode::bs_init_writable, vec![]),
            Instr::BsInitWritable
        );
        assert_eq!(
            lower(
                Opcode::bs_create_bin,
                vec![
                    Operand::Label(NO_FAIL),
                    Operand::Integer(3),
                    Operand::Integer(1),
                    Operand::Integer(8),
                    Operand::X(0),
                    Operand::List(vec![Operand::Atom(Atom::MAX), Operand::Integer(0)]),
                ],
            ),
            Instr::BsCreateBin {
                fail: NO_FAIL,
                alloc: 3,
                live: 1,
                unit: 8,
                dst: Operand::X(0),
                segments: vec![Operand::Atom(Atom::MAX), Operand::Integer(0)],
            }
        );
    }

    #[test]
    fn lowers_the_module_markers() {
        assert_eq!(lower(Opcode::nif_start, vec![]), Instr::NifStart);
        assert_eq!(lower(Opcode::on_load, vec![]), Instr::OnLoad);
    }

    #[test]
    fn a_named_variant_with_a_structurally_wrong_operand_is_rejected() {
        // `put_tuple2` expects an argument list as its second operand.
        assert!(matches!(
            lower_instr(&raw(
                Opcode::put_tuple2,
                vec![Operand::X(0), Operand::Integer(1)],
            ))
            .unwrap_err(),
            BeamError::BadArg
        ));
        // `get_tuple_element` expects the element index to be an integer.
        assert!(matches!(
            lower_instr(&raw(
                Opcode::get_tuple_element,
                vec![Operand::X(0), Operand::X(1), Operand::X(2)],
            ))
            .unwrap_err(),
            BeamError::BadArg
        ));
        // `jump` expects a label.
        assert!(matches!(
            lower_instr(&raw(Opcode::jump, vec![Operand::X(0)])).unwrap_err(),
            BeamError::BadArg
        ));
        // A missing operand is reported, never a panic.
        assert!(matches!(
            lower_instr(&raw(Opcode::get_hd, vec![Operand::X(0)])).unwrap_err(),
            BeamError::BadArg
        ));
    }
}
