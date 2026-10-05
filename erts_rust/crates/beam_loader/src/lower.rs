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
use std::collections::BTreeMap;

use beam_common::Result;

use crate::chunk::Chunks;
use crate::decode::RawInstr;
use crate::ir::{Block, Function, Instr, Module};
use crate::operand::Label;

/// Assembles the IR [`Module`] from the parsed chunks and the decoded instructions.
///
/// `atom_table` is the interning context the module's atoms are resolved through
/// (seeding it with `atom.names` for preloaded/positional atoms is `beam_rt`'s
/// job). `raw_instrs` is the whole-module instruction stream from
/// [`crate::decode::decode_stream`]; `chunks` supplies the tables and data chunks.
// TODO(beam_loader-impl): intern the `AtU8` names through `atom_table`, map the raw
// TODO(beam_loader-impl): export/import atom indices to interned atoms, call
// TODO(beam_loader-impl): `group_functions` on `raw_instrs`, and assemble the
// TODO(beam_loader-impl): `Module` (name, atoms, tables, literals, strings,
// TODO(beam_loader-impl): attributes, compile info, code version).
pub fn build_module(
    chunks: &Chunks<'_>,
    raw_instrs: &[RawInstr],
    atom_table: beam_term::AtomTable,
) -> Result<Module> {
    let _ = (chunks, raw_instrs, atom_table);
    Err(beam_common::BeamError::BadArg)
}

/// Splits the module-wide instruction stream into functions at `func_info` boundaries.
///
/// Each returned [`Function`] carries the `func_info` module/name/arity, the
/// `func_label` (the label before `func_info`) and the `entry_label` (the label
/// after it, which is what [`crate::ir::Export::label`] points to), its lowered
/// body and its basic blocks. A stream with no `func_info`, or with a `func_info`
/// not preceded by its two labels, yields `BeamError::BadArg`.
// TODO(beam_loader-impl): lower each `RawInstr` to `Instr` (mapping the M1 opcode
// TODO(beam_loader-impl): subset to the named variants, everything else to
// TODO(beam_loader-impl): `Instr::Generic`), then cut at each `func_info` and call
// TODO(beam_loader-impl): `build_blocks` for each function body.
pub fn group_functions(instrs: &[RawInstr]) -> Result<Vec<Function>> {
    let _ = instrs;
    Err(beam_common::BeamError::BadArg)
}

/// Decomposes a function body into label-delimited basic blocks.
///
/// Returns the blocks (each `code[start..end]` beginning at its label) and the
/// label → instruction-index map. The map is derived from the `Instr::Label`
/// markers; the blocks are the runs between consecutive labels.
// TODO(beam_loader-impl): record each `Instr::Label`'s index in the map and slice
// TODO(beam_loader-impl): `code` into one `Block` per label.
pub fn build_blocks(code: &[Instr]) -> (Vec<Block>, BTreeMap<Label, usize>) {
    let _ = code;
    (Vec::new(), BTreeMap::new())
}
