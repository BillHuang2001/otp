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

//! Lowering: raw chunks + raw instructions → the typed IR.
//!
//! This module owns the semantic step between the wire level ([`crate::chunk`],
//! [`crate::decode`]) and the frozen IR ([`crate::ir`]):
//!
//! * [`build_module`] assembles the [`Module`]: interned atoms, exports, imports,
//!   functions, fun descriptors, the type/line tables, the decoded literal area and
//!   the raw data chunks.
//! * the sibling [`group`] module (re-exported here) splits the module-wide
//!   instruction stream into functions ([`group_functions`]) and decomposes a body
//!   into label-delimited basic blocks ([`build_blocks`]).
//! * the private `instr` module maps one decoded instruction to one IR [`Instr`].
//!
//! Every fallible step reports `BeamError::BadArg` through [`crate::error::bad_file`]
//! (or `BeamError::SystemLimit` for a size bound); the module never panics and never
//! `unwrap()`s file data.
use std::sync::Arc;

use beam_common::{BeamError, Result};
use beam_term::{make_atom, make_small, Atom, Eterm, NIL};

use crate::chunk::record::{RawRecord, RawRecordDefault};
use crate::chunk::Chunks;
use crate::decode::RawInstr;
use crate::error::bad_file;
use crate::ir::{Export, Function, FunDescriptor, Import, Module, RecordDefinition, RecordField};
use crate::literal::LiteralArea;
use crate::operand::{Label, Operand};

mod group;
mod instr;

pub use group::{build_blocks, group_functions};/// Assembles the IR [`Module`] from the parsed chunks and the decoded instructions.
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

    // `LocT` (the JIT-only local-function table) shares the `ExpT` layout, so it is
    // mapped exactly like the exports — the label is the local function's entry label.
    // The interpreter never consults it; it is carried so the recognized chunk is
    // visible on the IR rather than silently dropped.
    let mut locals = Vec::with_capacity(chunks.locals.len());
    for raw in &chunks.locals {
        locals.push(Export {
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

    // The C loader turns a wide `i` operand into a bignum **static literal**
    // (`marshal_integer` → `beamfile_add_literal`, `beam_file.c:2210-2220`) that the
    // instruction references by literal index. Do the same here: each such operand is
    // pointed at a literal appended after the file's own `LitT` entries. Nothing is
    // copied and nothing is appended when the module has no wide operand.
    let mut synthesized: Vec<Vec<u8>> = Vec::new();
    let rewritten = rewrite_wide_integers(raw_instrs, chunks.literals.len(), &mut synthesized)?;
    let raw_instrs: &[RawInstr] = rewritten.as_deref().unwrap_or(raw_instrs);

    // The `Recs` native-record definitions: wire atom indices become interned atoms,
    // and a bignum field default is appended to the *same* synthesized-literal set as a
    // wide instruction operand, so it is materialized by the machinery below.
    let pending_records =
        build_records(&chunks.records, &atoms, chunks.literals.len(), &mut synthesized)?;

    let functions = group_functions(raw_instrs)?;

    // `FunT` carries wire atom indices; the fun-table index (`make_fun3`'s operand)
    // is the entry's *position*, so file order is preserved.
    let mut funs = Vec::with_capacity(chunks.funs.len());
    for raw in &chunks.funs {
        let label = Label(raw.label);
        if !has_label(&functions, label) {
            return Err(bad_file("fun descriptor label is not a label in the module"));
        }
        funs.push(FunDescriptor {
            function: atom_at(&atoms, raw.function)?,
            arity: raw.arity,
            label,
            index: raw.index,
            num_free: raw.num_free,
            uniq: raw.old_uniq,
        });
    }

    // The literals are decoded into terms here, through the module's own atom table
    // (so a literal's atoms share the module's identity), *before* that table is
    // moved into the `Module`. The synthesized wide-integer literals share the arena
    // and continue the index space (`rewrite_wide_integers` numbered them from
    // `chunks.literals.len()`), exactly like the C's growing literal table.
    let literals = if synthesized.is_empty() {
        LiteralArea::decode(&chunks.literals, &mut atom_table)?
    } else {
        let mut payloads = chunks.literals.clone();
        payloads.append(&mut synthesized);
        LiteralArea::decode(&payloads, &mut atom_table)?
    };
    let literals = Arc::new(literals);

    // Literal-backed record defaults (`z,4` and bignums) resolve only now that the
    // area they point into exists.
    let records = resolve_records(pending_records, &literals)?;

    // The `Attr` chunk is a module attribute property list encoded as an external term
    // (`beam_asm.erl:307-310`); decode it into a real term through the module's own
    // atom table (so its keys share the module's atom identity) — the value
    // `module_info(attributes)` serves. A malformed payload is a typed error.
    let attribute_table = crate::attributes::AttributeTable::decode(
        chunks.attributes.as_deref(),
        &mut atom_table,
    )?;

    Ok(Module {
        name,
        atoms,
        atom_table,
        exports,
        locals,
        imports,
        functions,
        funs,
        records,
        code_version: chunks.code_version,
        types: chunks.types.clone(),
        line: chunks.line.clone().unwrap_or_default(),
        literals,
        strings: chunks.strings.clone(),
        attributes: chunks.attributes.clone(),
        attribute_table: Arc::new(attribute_table),
        compile_info: chunks.compile_info.clone(),
        meta: chunks.meta.clone(),
    })
}

/// Whether `label` is a code label defined in one of the module's functions.
///
/// The `FunT` labels address lambda bodies, which are ordinary labelled blocks, so a
/// descriptor whose label exists nowhere in the code is a malformed table.
fn has_label(functions: &[Function], label: Label) -> bool {
    functions
        .iter()
        .any(|function| function.labels.contains_key(&label))
}

/// Resolves a wire atom index through the module's `atoms` map.
fn atom_at(atoms: &[Atom], index: u32) -> Result<Atom> {
    atoms
        .get(index as usize)
        .copied()
        .ok_or_else(|| bad_file("atom index out of range"))
}

/// Points every wide-integer operand of `instrs` at a synthesized static literal.
///
/// `base` is the number of the module's own `LitT` literals, so the synthesized ones
/// are numbered `base`, `base + 1`, … in operand order — the same order in which
/// their ETF payloads are appended to `synthesized`. A literal area decoded from
/// `chunks.literals` followed by `synthesized` therefore resolves every rewritten
/// [`Operand::Literal`] index.
///
/// Returns `None` — copying nothing — when the module has no wide-integer operand;
/// a literal index that would not fit `u32` is [`BeamError::SystemLimit`].
fn rewrite_wide_integers(
    instrs: &[RawInstr],
    base: usize,
    synthesized: &mut Vec<Vec<u8>>,
) -> Result<Option<Vec<RawInstr>>> {
    let count: usize = instrs
        .iter()
        .map(|instr| instr.operands.iter().map(count_wide_integers).sum::<usize>())
        .sum();
    if count == 0 {
        return Ok(None);
    }
    // `Operand::Literal` carries a `u32` index, so the extended literal count must fit.
    if base.saturating_add(count) > u32::MAX as usize {
        return Err(BeamError::SystemLimit);
    }

    let mut next = base as u32;
    let mut rewritten = Vec::with_capacity(instrs.len());
    for instr in instrs {
        let mut instr = instr.clone();
        for operand in &mut instr.operands {
            rewrite_wide_integer(operand, &mut next, synthesized);
        }
        rewritten.push(instr);
    }
    Ok(Some(rewritten))
}

/// The number of wide-integer operands in `operand`, including an argument list's.
fn count_wide_integers(operand: &Operand) -> usize {
    match operand {
        Operand::BigInteger { .. } => 1,
        Operand::List(items) => items.iter().map(count_wide_integers).sum(),
        _ => 0,
    }
}

/// Replaces every wide-integer operand (recursively) with a synthesized
/// [`Operand::Literal`] reference, appending its ETF payload to `synthesized`.
fn rewrite_wide_integer(operand: &mut Operand, next: &mut u32, synthesized: &mut Vec<Vec<u8>>) {
    match operand {
        Operand::BigInteger {
            negative,
            magnitude,
        } => {
            synthesized.push(big_integer_payload(*negative, magnitude));
            *operand = Operand::Literal(*next);
            *next += 1;
        }
        Operand::List(items) => {
            for item in items {
                rewrite_wide_integer(item, next, synthesized);
            }
        }
        _ => {}
    }
}

/// The external-term payload of a synthesized bignum literal.
///
/// Layout pinned to the wire (`external.c`) and to the system `erl`'s own output:
/// `VERSION_MAGIC`, the `SMALL_BIG_EXT`/`LARGE_BIG_EXT` tag, the **byte** count (1
/// byte for `SMALL_BIG_EXT`, 4 big-endian bytes for `LARGE_BIG_EXT`), the sign byte
/// (`0` positive, `1` negative) and then the magnitude **least significant byte
/// first**.
fn big_integer_payload(negative: bool, magnitude: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(magnitude.len() + 8);
    payload.push(beam_codec::VERSION_MAGIC);
    if magnitude.len() <= usize::from(u8::MAX) {
        payload.push(beam_codec::SMALL_BIG_EXT);
        payload.push(magnitude.len() as u8);
    } else {
        payload.push(beam_codec::LARGE_BIG_EXT);
        payload.extend_from_slice(&(magnitude.len() as u32).to_be_bytes());
    }
    payload.push(u8::from(negative));
    payload.extend(magnitude.iter().rev());
    payload
}

/// A record field's default before its literal-backed values are resolved.
enum PendingDefault {
    /// The field has no default.
    Absent,
    /// The default is already a materialized term (a small integer or an atom).
    Term(Eterm),
    /// The default is the literal at this index in the module's literal area (a `z,4`
    /// file reference, or a synthesized bignum).
    Literal(u32),
}

/// A record definition whose literal-backed defaults are not yet resolved.
struct PendingRecord {
    /// The record-name atom.
    name: Atom,
    /// Whether the record is exported.
    is_exported: bool,
    /// The `(field name, default)` pairs, in declaration order.
    fields: Vec<(Atom, PendingDefault)>,
}

/// Maps the raw `Recs` records onto interned atoms, appending every bignum default's
/// ETF payload to `synthesized`.
///
/// `literal_base` is the number of the module's own `LitT` literals, so a synthesized
/// bignum is numbered `literal_base + synthesized.len()` — the same index-space rule
/// [`rewrite_wide_integers`] follows for instruction operands, and the reason the
/// literal bumps must be appended *before* the area is decoded.
fn build_records(
    raw: &[RawRecord],
    atoms: &[Atom],
    literal_base: usize,
    synthesized: &mut Vec<Vec<u8>>,
) -> Result<Vec<PendingRecord>> {
    let mut next = literal_base + synthesized.len();
    let mut records = Vec::with_capacity(raw.len());
    for record in raw {
        let name = atom_at(atoms, record.name)?;
        let mut fields = Vec::with_capacity(record.fields.len());
        for field in &record.fields {
            let field_name = atom_at(atoms, field.name)?;
            let default = match &field.default {
                RawRecordDefault::Absent => PendingDefault::Absent,
                RawRecordDefault::Integer(value) => PendingDefault::Term(make_small(*value)),
                // Wire index 0 is the NIL placeholder, i.e. the default `nil`/`[]`.
                RawRecordDefault::Atom(0) => PendingDefault::Term(NIL),
                RawRecordDefault::Atom(index) => {
                    PendingDefault::Term(make_atom(atom_at(atoms, *index)?))
                }
                RawRecordDefault::Literal(index) => PendingDefault::Literal(*index),
                RawRecordDefault::BigInteger {
                    negative,
                    magnitude,
                } => {
                    if next > u32::MAX as usize {
                        return Err(BeamError::SystemLimit);
                    }
                    synthesized.push(big_integer_payload(*negative, magnitude));
                    let index = next as u32;
                    next += 1;
                    PendingDefault::Literal(index)
                }
            };
            fields.push((field_name, default));
        }
        records.push(PendingRecord {
            name,
            is_exported: record.is_exported,
            fields,
        });
    }
    Ok(records)
}

/// Resolves the literal-backed record defaults against the built literal area.
fn resolve_records(
    pending: Vec<PendingRecord>,
    literals: &LiteralArea,
) -> Result<Vec<RecordDefinition>> {
    let mut records = Vec::with_capacity(pending.len());
    for record in pending {
        let mut fields = Vec::with_capacity(record.fields.len());
        for (name, default) in record.fields {
            let default = match default {
                PendingDefault::Absent => None,
                PendingDefault::Term(term) => Some(term),
                PendingDefault::Literal(index) => Some(
                    literals
                        .get(index)
                        .ok_or_else(|| bad_file("record default literal index is out of range"))?,
                ),
            };
            fields.push(RecordField { name, default });
        }
        records.push(RecordDefinition {
            name: record.name,
            is_exported: record.is_exported,
            fields,
        });
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    //! Module-assembly tests: the lowering of every modelled opcode (and the
    //! structural rejections) driven through [`group_functions`]; the grouping and
    //! block-decomposition unit tests live in the sibling [`group`] module.

    use super::instr::lower_instr;
    use super::*;
    use crate::chunk::table::{RawExport, RawFun};
    use crate::ir::{CodeVersion, ImportRef, Instr, TypeTable};
    use crate::opcode::Opcode;
    use crate::operand::{Operand, NO_FAIL};
    use beam_codec::term::{flatmap_parts, tuple_arity, tuple_element};
    use beam_common::BeamError;
    use beam_term::prelude::atom_val;

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
    fn lowers_bs_create_bin_with_an_allocation_list_alloc() {
        // `bs_create_bin`'s `Alloc` operand is written as a bare word count *or* as
        // a `z,3` allocation list — the compiler hoists a block's pending heap need
        // into the preceding `bs_create_bin` (`beam_ssa_codegen.erl:342-352`). This
        // is the exact operand of `bsalloc.beam`'s `pack/2`
        // (`{alloc,[{words,4},{floats,0},{funs,1}]}`); 89 real OTP modules (incl.
        // `dets_v9`, `shell_docs`) hit this form, and a bare-integer-only lowering
        // rejected the whole module with `BadArg`.
        assert_eq!(
            lower(
                Opcode::bs_create_bin,
                vec![
                    Operand::Label(NO_FAIL),
                    Operand::Allocation(vec![
                        crate::operand::AllocKind {
                            kind: 0,
                            number: 4,
                            words: 4,
                        },
                        crate::operand::AllocKind {
                            kind: 1,
                            number: 0,
                            words: 0,
                        },
                        crate::operand::AllocKind {
                            kind: 2,
                            number: 1,
                            words: 1,
                        },
                    ]),
                    Operand::Integer(2),
                    Operand::Integer(8),
                    Operand::X(2),
                    Operand::List(vec![Operand::Atom(Atom::MAX), Operand::Integer(1)]),
                ],
            ),
            Instr::BsCreateBin {
                fail: NO_FAIL,
                // 4 heap words + 0 floats + 1 fun (the loader's `number`-words
                // placeholder for a fun entry, `decode::allocation_entry`); exactly
                // what the real loader produces (`marshal_allocation_list`,
                // `beam_file.c:2234-2284`).
                alloc: 5,
                live: 2,
                unit: 8,
                dst: Operand::X(2),
                segments: vec![Operand::Atom(Atom::MAX), Operand::Integer(1)],
            }
        );
    }

    #[test]
    fn a_bs_create_bin_with_a_non_integer_non_alloc_alloc_operand_is_rejected() {
        // A register is neither a word count nor an allocation list.
        assert!(matches!(
            lower_instr(&raw(
                Opcode::bs_create_bin,
                vec![
                    Operand::Label(NO_FAIL),
                    Operand::X(0),
                    Operand::Integer(1),
                    Operand::Integer(8),
                    Operand::X(0),
                    Operand::List(vec![]),
                ],
            ))
            .unwrap_err(),
            BeamError::BadArg
        ));
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

    // ---- module assembly: literals and fun descriptors --------------------

    /// The committed `coverage.beam` fixture: `LitT` of three literals
    /// (`{error,division_by_zero}`, `{error,badarith}`, `#{a=>1,b=>2}`) and a
    /// one-entry `FunT` (the closure `make_adder/1` returns).
    const COVERAGE: &[u8] = include_bytes!("../tests/fixtures/coverage.beam");

    /// A minimal chunk set holding one 0-arity function, used to drive
    /// [`build_module`] with a chosen `FunT` / `LitT` table.
    fn minimal_chunks(funs: Vec<RawFun>, literals: Vec<Vec<u8>>) -> Chunks<'static> {
        Chunks {
            atoms: vec![String::new(), "m".to_string(), "f".to_string()],
            code: None,
            exports: Vec::new(),
            locals: Vec::new(),
            imports: Vec::new(),
            funs,
            records: Vec::new(),
            line: None,
            types: TypeTable::default(),
            literals,
            strings: Vec::new(),
            attributes: None,
            compile_info: None,
            meta: None,
            code_version: CodeVersion::default(),
            code_stream: &[],
        }
    }

    /// The instruction stream of `minimal_chunks`' single function (`label 1`,
    /// `func_info`, `label 2`, `return`).
    fn minimal_stream() -> Vec<RawInstr> {
        vec![
            label(1),
            func_info(Atom::MAX, Atom::MAX, 0),
            label(2),
            raw(Opcode::r#return, vec![]),
        ]
    }

    /// The fixture's real chunk set. Its `Code` stream is *not* loaded here: it
    /// contains a `test_heap` with a `funs` allocation entry, which the
    /// instruction lowering (`lower::instr`) does not model yet — a loader gap
    /// unrelated to literals/funs. The `LitT`/`FunT`/`AtU8` tables are the real
    /// ones, so the lowering under test is exercised on real data.
    fn coverage_chunks() -> Chunks<'static> {
        let container = crate::container::parse(COVERAGE).unwrap();
        crate::chunk::parse(&container).unwrap()
    }

    /// Interns `chunks.atoms` the way [`crate::load`] does (index 0 = NIL).
    fn coverage_atoms(chunks: &Chunks<'_>) -> (Vec<Atom>, beam_term::AtomTable) {
        let mut table = beam_term::AtomTable::new();
        let mut atoms = vec![Atom::MAX];
        for name in &chunks.atoms[1..] {
            atoms.push(table.intern(name));
        }
        (atoms, table)
    }

    #[test]
    fn the_coverage_fixture_resolves_its_static_literals() {
        // The real `LitT` of `coverage.beam`: `{error,division_by_zero}`,
        // `{error,badarith}`, `#{a=>1,b=>2}`.
        let chunks = coverage_chunks();
        assert_eq!(chunks.literals.len(), 3);
        // Isolate the literal table: the fixture's fun descriptor addresses label 37,
        // which `minimal_stream` does not define (see the fun test below).
        let chunks = Chunks {
            funs: Vec::new(),
            ..chunks
        };
        let (_, table) = coverage_atoms(&chunks);
        let instrs = minimal_stream();
        let module = build_module(&chunks, &instrs, table).unwrap();

        assert_eq!(module.literals().len(), 3);
        assert!(!module.literals().is_empty());
        assert!(module.literals().heap().is_some());
        assert_eq!(module.literal(3), None, "an index past the table is None");

        // 0: `{error,division_by_zero}`.
        let first = module.literal(0).unwrap();
        assert_eq!(tuple_arity(first).unwrap(), 2);
        assert_eq!(
            module.atom_name(atom_val(tuple_element(first, 0).unwrap())),
            Some("error")
        );
        assert_eq!(
            module.atom_name(atom_val(tuple_element(first, 1).unwrap())),
            Some("division_by_zero")
        );

        // 1: `{error,badarith}`.
        let second = module.literal(1).unwrap();
        assert_eq!(tuple_arity(second).unwrap(), 2);
        assert_eq!(
            module.atom_name(atom_val(tuple_element(second, 1).unwrap())),
            Some("badarith")
        );

        // 2: `#{a=>1,b=>2}`.
        let third = module.literal(2).unwrap();
        let (keys, values) = flatmap_parts(third).unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(values.len(), 2);
        assert_eq!(module.atom_name(atom_val(keys[0])), Some("a"));
        assert_eq!(module.atom_name(atom_val(keys[1])), Some("b"));
    }

    #[test]
    fn the_coverage_fixture_resolves_its_fun_descriptor() {
        // The real one-entry `FunT` of `coverage.beam`; the instruction stream is a
        // stand-in that carries the descriptor's label (37) so it resolves.
        let chunks = coverage_chunks();
        assert_eq!(chunks.funs.len(), 1);
        let fun_label = Label(chunks.funs[0].label);
        let (_, table) = coverage_atoms(&chunks);
        let instrs = vec![
            label(1),
            func_info(Atom::MAX, Atom::MAX, 0),
            label(2),
            // The lambda body's entry label from the `FunT` entry.
            RawInstr {
                opcode: Opcode::label,
                operands: vec![Operand::Integer(i64::from(chunks.funs[0].label))],
                offset: 0,
            },
            raw(Opcode::r#return, vec![]),
        ];
        let module = build_module(&chunks, &instrs, table).unwrap();

        assert_eq!(module.funs().len(), 1);
        let fun = module.fun(0).expect("the fixture's single lambda");
        assert_eq!(module.atom_name(fun.function), Some("-make_adder/1-fun-0-"));
        assert_eq!(fun.arity, 2);
        assert_eq!(fun.label, fun_label);
        assert_eq!(fun.index, 0);
        assert_eq!(fun.num_free, 1);
        assert_eq!(fun.uniq, 3_533_555);
        assert_eq!(module.fun(1), None, "an index past the table is None");

        // The descriptor's label really is a labelled block of the module…
        assert!(module.functions[0].labels.contains_key(&fun.label));
    }
    #[test]
    fn the_empty_literal_table_of_the_slice_fixture_yields_an_empty_area() {
        let slice = include_bytes!("../tests/fixtures/slice.beam");
        let module = crate::load(slice).unwrap();

        assert!(module.literals().is_empty());
        assert_eq!(module.literal(0), None);
        assert!(module.funs().is_empty());
        assert_eq!(module.fun(0), None);
    }

    #[test]
    fn a_fun_descriptor_with_an_unknown_label_is_rejected() {
        let funs = vec![RawFun {
            function: 2,
            arity: 1,
            label: 99, // not a label in the module
            index: 0,
            num_free: 0,
            old_uniq: 0,
        }];
        let err = build_module(
            &minimal_chunks(funs, Vec::new()),
            &minimal_stream(),
            beam_term::AtomTable::new(),
        )
        .unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn a_fun_descriptor_with_a_known_label_is_lowered() {
        let funs = vec![RawFun {
            function: 2,
            arity: 1,
            label: 2, // `label 2` in `minimal_stream`
            index: 0,
            num_free: 3,
            old_uniq: 7,
        }];
        let module = build_module(
            &minimal_chunks(funs, Vec::new()),
            &minimal_stream(),
            beam_term::AtomTable::new(),
        )
        .unwrap();

        let fun = module.fun(0).unwrap();
        assert_eq!(module.atom_name(fun.function), Some("f"));
        assert_eq!((fun.arity, fun.label, fun.index, fun.num_free, fun.uniq), (1, Label(2), 0, 3, 7));
        assert_eq!(module.fun(1), None);
    }

    #[test]
    fn a_module_without_a_locals_table_has_no_locals() {
        // `minimal_chunks` carries no `LocT` entries, so the lowered module's
        // local-function table is empty (it is an additive field, never `None`).
        let module = build_module(
            &minimal_chunks(Vec::new(), Vec::new()),
            &minimal_stream(),
            beam_term::AtomTable::new(),
        )
        .unwrap();
        assert!(module.locals.is_empty());
    }

    #[test]
    fn a_locals_entry_is_lowered_like_an_export() {
        // A `LocT` entry maps its wire atom index through the atom table exactly like
        // an `ExpT` entry; the label is the local function's entry label.
        let chunks = Chunks {
            locals: vec![RawExport {
                function: 2, // atom "f"
                arity: 3,
                label: 2,
            }],
            ..minimal_chunks(Vec::new(), Vec::new())
        };
        let module = build_module(&chunks, &minimal_stream(), beam_term::AtomTable::new()).unwrap();
        assert_eq!(module.locals.len(), 1);
        let local = &module.locals[0];
        assert_eq!(module.atom_name(local.function), Some("f"));
        assert_eq!((local.arity, local.label), (3, Label(2)));
    }

    #[test]
    fn a_locals_entry_with_an_out_of_range_atom_is_rejected() {
        let chunks = Chunks {
            locals: vec![RawExport {
                function: 99, // beyond the three atoms of `minimal_chunks`
                arity: 0,
                label: 2,
            }],
            ..minimal_chunks(Vec::new(), Vec::new())
        };
        let err =
            build_module(&chunks, &minimal_stream(), beam_term::AtomTable::new()).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn a_malformed_literal_fails_the_load() {
        let err = build_module(
            &minimal_chunks(Vec::new(), vec![vec![1, 2, 3]]),
            &minimal_stream(),
            beam_term::AtomTable::new(),
        )
        .unwrap_err();
        assert!(matches!(err, BeamError::BadArg), "got {err:?}");
    }

    // ---- wide (big-integer) instruction operands -------------------------

    #[test]
    fn a_wide_integer_operand_becomes_a_synthesized_static_literal() {
        // A `gc_bif` whose argument is the decoder's `Operand::BigInteger` form of
        // `2^64 - 1` (8 magnitude bytes). The loader must point it at a static
        // literal numbered *after* the module's own `LitT` entries (`base = 4`),
        // exactly like the C's `marshal_integer` → `beamfile_add_literal`.
        let instrs = vec![raw(
            Opcode::is_eq_exact,
            vec![Operand::BigInteger {
                negative: false,
                magnitude: vec![0xFF; 8],
            }],
        )];
        let mut synthesized = Vec::new();
        let rewritten = rewrite_wide_integers(&instrs, 4, &mut synthesized)
            .unwrap()
            .expect("a wide operand forces a rewrite");
        assert_eq!(rewritten[0].operands, vec![Operand::Literal(4)]);
        assert_eq!(
            synthesized,
            vec![vec![
                beam_codec::VERSION_MAGIC,
                beam_codec::SMALL_BIG_EXT,
                8,
                0,
                255,
                255,
                255,
                255,
                255,
                255,
                255,
                255,
            ]]
        );
    }

    #[test]
    fn a_wide_integer_operand_becomes_a_static_literal_end_to_end() {
        // A module with one own `LitT` literal (`0`) and one instruction carrying a
        // wide operand; the synthesized literal continues the numbering (`base = 1`).
        let stream = vec![
            label(1),
            func_info(Atom::MAX, Atom::MAX, 0),
            label(2),
            raw(
                Opcode::allocate_zero,
                vec![Operand::BigInteger {
                    negative: false,
                    magnitude: vec![0xFF; 8],
                }],
            ),
            raw(Opcode::r#return, vec![]),
        ];
        let module = build_module(
            &minimal_chunks(Vec::new(), vec![vec![131, 97, 0]]),
            &stream,
            beam_term::AtomTable::new(),
        )
        .unwrap();
        assert_eq!(module.literals().len(), 2);
        assert_eq!(
            module.functions[0].code[3],
            Instr::Generic {
                op: Opcode::allocate_zero,
                operands: vec![Operand::Literal(1)],
                offset: 0,
            }
        );
        // It resolves to the `2^64 - 1` bignum: positive, one machine digit.
        let (negative, digits) = beam_codec::term::big_parts(module.literal(1).unwrap()).unwrap();
        assert!(!negative);
        assert_eq!(digits, [u64::MAX]);
    }

    #[test]
    fn rewriting_wide_integers_is_a_no_op_without_one() {
        let instrs = vec![raw(Opcode::r#move, vec![Operand::X(0), Operand::X(1)])];
        let mut synthesized = Vec::new();
        assert!(
            rewrite_wide_integers(&instrs, 0, &mut synthesized)
                .unwrap()
                .is_none(),
            "no wide operand must copy nothing"
        );
        assert!(synthesized.is_empty());
    }

    #[test]
    fn a_wide_integer_inside_an_argument_list_is_rewritten() {
        let instrs = vec![raw(
            Opcode::gc_bif2,
            vec![Operand::List(vec![
                Operand::Integer(1),
                Operand::BigInteger {
                    negative: true,
                    magnitude: vec![0xFF; 9],
                },
            ])],
        )];
        let mut synthesized = Vec::new();
        let rewritten = rewrite_wide_integers(&instrs, 0, &mut synthesized)
            .unwrap()
            .expect("rewrite");
        assert_eq!(
            rewritten[0].operands,
            vec![Operand::List(vec![Operand::Integer(1), Operand::Literal(0)])]
        );
        // `SMALL_BIG_EXT`, a 9-byte magnitude, sign 1 (`negative`), all `FF`.
        assert_eq!(
            synthesized,
            vec![vec![131u8, 110, 9, 1, 255, 255, 255, 255, 255, 255, 255, 255, 255]]
        );
    }

    #[test]
    fn big_integer_payload_uses_the_large_form_for_a_wide_magnitude() {
        let payload = big_integer_payload(true, &vec![0u8; 300]);
        assert_eq!(payload[0], beam_codec::VERSION_MAGIC);
        assert_eq!(payload[1], beam_codec::LARGE_BIG_EXT);
        assert_eq!(&payload[2..6], &300u32.to_be_bytes());
        assert_eq!(payload[6], 1, "negative sign byte");
        assert_eq!(payload.len(), 7 + 300);
    }
}
