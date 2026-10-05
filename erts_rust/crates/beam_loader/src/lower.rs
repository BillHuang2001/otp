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
//! Every fallible step reports `BeamError::BadArg` through [`crate::error::bad_file`]
//! (or `BeamError::SystemLimit` for a size bound); the module never panics and never
//! `unwrap()`s file data.
use std::collections::BTreeMap;

use beam_common::{BeamError, Result};
use beam_term::Atom;

use crate::chunk::Chunks;
use crate::decode::RawInstr;
use crate::error::bad_file;
use crate::ir::{Block, Export, Function, Import, ImportRef, Instr, Module, TestKind};
use crate::opcode::Opcode;
use crate::operand::{Label, Operand};

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

/// Lowers one decoded [`RawInstr`] into the IR [`Instr`].
///
/// The Milestone-1 opcode subset maps to named variants; every other opcode is
/// preserved losslessly as [`Instr::Generic`]. Operand order is the file's
/// (`ops.tab`), e.g. `move Src Dst` and `gc_bif* Fail Live Bif Arg… Dst`. A
/// structurally mismatched operand (a non-label where a `f` operand is required, a
/// non-integer where a `u` operand is required) yields `BeamError::BadArg`.
fn lower_instr(raw: &RawInstr) -> Result<Instr> {
    let ops = &raw.operands;
    match raw.opcode {
        // `label/1`: the compiler writes the label *number* with the `u` tag
        // (`encode_arg(Int, …)` in `beam_asm.erl`; `beam_disasm` prints a plain
        // integer), unlike jump/failure targets which use the `f` tag. Both forms
        // denote a label, so both are accepted here.
        Opcode::label => Ok(Instr::Label(label_value(operand(ops, 0)?)?)),
        // The `line` operand is the index into the `Line` chunk items (0 = undefined
        // location), which is exactly the IR's `LineRef`.
        Opcode::line => Ok(Instr::Line(as_u32(operand(ops, 0)?)?)),
        Opcode::func_info => Ok(Instr::FuncInfo {
            module: as_atom(operand(ops, 0)?)?,
            function: as_atom(operand(ops, 1)?)?,
            arity: as_u32(operand(ops, 2)?)?,
        }),
        Opcode::int_code_end => Ok(Instr::IntCodeEnd),
        Opcode::r#move => Ok(Instr::Move {
            src: operand(ops, 0)?.clone(),
            dst: operand(ops, 1)?.clone(),
        }),
        // `gc_bif{1,2,3} Fail Live Bif Arg… Dst` (`erts/emulator/beam/emu/ops.tab`).
        // The BIF operand is the index into the `ImpT` chunk, not a `bif.tab` number.
        Opcode::gc_bif1 | Opcode::gc_bif2 | Opcode::gc_bif3 => {
            if ops.len() < 4 {
                return Err(bad_file("gc_bif has too few operands"));
            }
            let args = ops[3..ops.len() - 1].to_vec();
            let arity = u8::try_from(args.len()).map_err(|_| BeamError::SystemLimit)?;
            Ok(Instr::GcBif {
                arity,
                bif: ImportRef(as_u32(operand(ops, 2)?)?),
                fail: as_label(operand(ops, 0)?)?,
                live: as_u32(operand(ops, 1)?)?,
                args,
                dst: ops[ops.len() - 1].clone(),
            })
        }
        // `select_val Value Fail {Value,Label}*` — the alternatives arrive as one
        // `z,1` argument list, flattened into alternating value/label operands.
        Opcode::select_val => {
            let value = operand(ops, 0)?.clone();
            let fail = as_label(operand(ops, 1)?)?;
            let items = match operand(ops, 2)? {
                Operand::List(items) => items,
                _ => return Err(bad_file("select_val alternatives are not a list")),
            };
            if items.len() % 2 != 0 {
                return Err(bad_file("select_val alternative list has an odd length"));
            }
            let mut pairs = Vec::with_capacity(items.len() / 2);
            for pair in items.chunks_exact(2) {
                pairs.push((pair[0].clone(), as_label(&pair[1])?));
            }
            Ok(Instr::SelectVal { value, fail, pairs })
        }
        Opcode::swap => Ok(Instr::Swap {
            a: operand(ops, 0)?.clone(),
            b: operand(ops, 1)?.clone(),
        }),
        Opcode::call => Ok(Instr::Call {
            arity: as_u32(operand(ops, 0)?)?,
            target: as_label(operand(ops, 1)?)?,
        }),
        Opcode::call_last => Ok(Instr::CallLast {
            arity: as_u32(operand(ops, 0)?)?,
            target: as_label(operand(ops, 1)?)?,
            deallocate: as_u32(operand(ops, 2)?)?,
        }),
        Opcode::call_only => Ok(Instr::CallOnly {
            arity: as_u32(operand(ops, 0)?)?,
            target: as_label(operand(ops, 1)?)?,
        }),
        Opcode::call_ext => Ok(Instr::CallExt {
            arity: as_u32(operand(ops, 0)?)?,
            target: ImportRef(as_u32(operand(ops, 1)?)?),
        }),
        Opcode::call_ext_only => Ok(Instr::CallExtOnly {
            arity: as_u32(operand(ops, 0)?)?,
            target: ImportRef(as_u32(operand(ops, 1)?)?),
        }),
        Opcode::call_ext_last => Ok(Instr::CallExtLast {
            arity: as_u32(operand(ops, 0)?)?,
            target: ImportRef(as_u32(operand(ops, 1)?)?),
            deallocate: as_u32(operand(ops, 2)?)?,
        }),
        Opcode::allocate => Ok(Instr::Allocate {
            stack_need: as_u32(operand(ops, 0)?)?,
            live: as_u32(operand(ops, 1)?)?,
        }),
        Opcode::allocate_heap => Ok(Instr::AllocateHeap {
            stack_need: as_u32(operand(ops, 0)?)?,
            heap_need: as_u32(operand(ops, 1)?)?,
            live: as_u32(operand(ops, 2)?)?,
        }),
        Opcode::test_heap => Ok(Instr::TestHeap {
            heap_need: as_u32(operand(ops, 0)?)?,
            live: as_u32(operand(ops, 1)?)?,
        }),
        Opcode::deallocate => Ok(Instr::Deallocate(as_u32(operand(ops, 0)?)?)),
        Opcode::trim => Ok(Instr::Trim {
            n: as_u32(operand(ops, 0)?)?,
            remaining: as_u32(operand(ops, 1)?)?,
        }),
        Opcode::r#return => Ok(Instr::Return),
        op => {
            // The `is_*`/`test_arity` family: `Test Fail Arg… `. Anything else keeps
            // its opcode and operands verbatim for the engine/JIT to extend into.
            if let Some(kind) = TestKind::from_opcode(op) {
                let fail = as_label(operand(ops, 0)?)?;
                Ok(Instr::Test {
                    kind,
                    fail,
                    args: ops[1..].to_vec(),
                })
            } else {
                Ok(Instr::Generic {
                    op,
                    operands: raw.operands.clone(),
                    offset: raw.offset,
                })
            }
        }
    }
}

/// Fetches the operand at `index`, or reports a structurally short instruction.
fn operand(ops: &[Operand], index: usize) -> Result<&Operand> {
    ops.get(index)
        .ok_or_else(|| bad_file("instruction is missing an operand"))
}

/// Requires a label (`f`) operand — the failure sentinel [`crate::operand::NO_FAIL`]
/// for `f,0` is a valid label.
fn as_label(operand: &Operand) -> Result<Label> {
    match operand {
        Operand::Label(label) => Ok(*label),
        _ => Err(bad_file("expected a label operand")),
    }
}

/// A label *value* from either the `u` integer form the compiler writes for
/// `label/1` or the `f` label form used for jump and failure targets.
fn label_value(operand: &Operand) -> Result<Label> {
    match operand {
        Operand::Label(label) => Ok(*label),
        Operand::Integer(value) if *value >= 0 && *value <= i64::from(u32::MAX) => {
            Ok(Label(*value as u32))
        }
        _ => Err(bad_file("expected a label operand")),
    }
}

/// Requires an unsigned integer (`u`) operand that fits the IR's 32-bit width.
fn as_u32(operand: &Operand) -> Result<u32> {
    match operand {
        Operand::Integer(value) if *value >= 0 && *value <= i64::from(u32::MAX) => {
            Ok(*value as u32)
        }
        _ => Err(bad_file("expected an unsigned integer operand")),
    }
}

/// Requires an atom (`a`) operand.
fn as_atom(operand: &Operand) -> Result<Atom> {
    match operand {
        Operand::Atom(atom) => Ok(*atom),
        _ => Err(bad_file("expected an atom operand")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opcode::Opcode;

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
}
