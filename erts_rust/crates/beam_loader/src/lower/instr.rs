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

//! The instruction-lowering match: one decoded [`RawInstr`] → one IR [`Instr`].
//!
//! This is the semantic core of the crate's lowering step, split out of
//! [`crate::lower`] so each file stays a readable unit (`../CONTEXT.md`,
//! "Files ~1000 lines"): `lower` owns function grouping, basic blocks and the
//! module assembly, this module owns the per-opcode vocabulary mapping.
//!
//! [`lower_instr`] maps every opcode the compiler emits to its named
//! [`Instr`] variant; an opcode the loader deliberately does not model is
//! preserved losslessly as [`Instr::Generic`]. Operand order is the file's
//! (`erts/emulator/beam/emu/ops.tab` + `genop.tab`'s `@spec` lines), never
//! re-ordered for convenience. A structurally mismatched operand (a non-label
//! where an `f` operand is required, a non-integer where a `u` operand is
//! required) yields `BeamError::BadArg` — the module never panics and never
//! `unwrap()`s file data.
use beam_common::{BeamError, Result};
use beam_term::Atom;

use crate::decode::RawInstr;
use crate::error::bad_file;
use crate::ir::{
    BsGetArgs, GetRecordFieldArgs, ImportRef, Instr, MapWriteArgs, PutRecordArgs, TestKind,
    UpdateRecordArgs, UpdateRecordIdArgs,
};
use crate::opcode::Opcode;
use crate::operand::{HeapNeed, Label, Operand};

/// Lowers one decoded [`RawInstr`] into the IR [`Instr`].
///
/// Every opcode the compiler emits maps to its named variant; every other opcode is
/// preserved losslessly as [`Instr::Generic`]. Operand order is the file's
/// (`ops.tab`), e.g. `move Src Dst` and `gc_bif* Fail Live Bif Arg… Dst`. A
/// structurally mismatched operand (a non-label where a `f` operand is required, a
/// non-integer where a `u` operand is required) yields `BeamError::BadArg`.
pub(super) fn lower_instr(raw: &RawInstr) -> Result<Instr> {
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
        Opcode::select_val => Ok(Instr::SelectVal {
            value: op(ops, 0)?,
            fail: as_label(operand(ops, 1)?)?,
            pairs: value_label_pairs(operand(ops, 2)?)?,
        }),
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
        // `allocate Ns Live`, `allocate_heap Ns Nh Live`, `test_heap Nh Live` —
        // the need operands accept **either** a plain word count (`u`) or a `z,3`
        // allocation list (`{alloc,[{words,N},{floats,N},{funs,N}]}`), so they are
        // lowered through `HeapNeed` (which keeps the breakdown and recovers the
        // word total). `erts/emulator/beam/emu/ops.tab` declares both forms.
        Opcode::allocate => Ok(Instr::Allocate {
            stack_need: HeapNeed::from_operand(operand(ops, 0)?)?,
            live: as_u32(operand(ops, 1)?)?,
        }),
        Opcode::allocate_heap => Ok(Instr::AllocateHeap {
            stack_need: HeapNeed::from_operand(operand(ops, 0)?)?,
            heap_need: HeapNeed::from_operand(operand(ops, 1)?)?,
            live: as_u32(operand(ops, 2)?)?,
        }),
        Opcode::test_heap => Ok(Instr::TestHeap {
            heap_need: HeapNeed::from_operand(operand(ops, 0)?)?,
            live: as_u32(operand(ops, 1)?)?,
        }),
        Opcode::deallocate => Ok(Instr::Deallocate(as_u32(operand(ops, 0)?)?)),
        Opcode::trim => Ok(Instr::Trim {
            n: as_u32(operand(ops, 0)?)?,
            remaining: as_u32(operand(ops, 1)?)?,
        }),
        Opcode::r#return => Ok(Instr::Return),

        // ---- control flow: jumps, frames, tuples, lists ----------------------
        Opcode::jump => Ok(Instr::Jump {
            target: as_label(operand(ops, 0)?)?,
        }),
        // `init_yregs {Yregs}` — the registers arrive as one `z,1` argument list.
        Opcode::init_yregs => Ok(Instr::InitYregs {
            registers: list(operand(ops, 0)?)?.to_vec(),
        }),
        // `put_tuple2 Dst {Elements}` — OTP 22 replaced the `put_tuple`/`put` pair.
        Opcode::put_tuple2 => Ok(Instr::PutTuple2 {
            dst: op(ops, 0)?,
            elements: list(operand(ops, 1)?)?.to_vec(),
        }),
        Opcode::get_tuple_element => Ok(Instr::GetTupleElement {
            source: op(ops, 0)?,
            element: as_u32(operand(ops, 1)?)?,
            dst: op(ops, 2)?,
        }),
        // `set_tuple_element Element Tuple Offset` — the file's operand order is
        // `s S P` (`emu/ops.tab:245`); `beam_disasm` resolves it as
        // `{set_tuple_element,Src,Dst,{u,Off}}` (`lib/compiler/src/beam_disasm.erl:1017`).
        // Retained for loading OTP≤28 images (the OTP-29 compiler no longer emits it).
        // `offset` is stored **raw**, the same compiler "Position" value
        // [`Instr::GetTupleElement::element`] holds — never scaled by 8 here.
        Opcode::set_tuple_element => Ok(Instr::SetTupleElement {
            element: op(ops, 0)?,
            tuple: op(ops, 1)?,
            offset: as_u32(operand(ops, 2)?)?,
        }),
        // `select_tuple_arity Tuple Fail {Arity,Label}*` — the alternatives arrive as
        // one flattened `z,1` argument list, exactly like `select_val`.
        Opcode::select_tuple_arity => Ok(Instr::SelectTupleArity {
            tuple: op(ops, 0)?,            fail: as_label(operand(ops, 1)?)?,
            destinations: value_label_pairs(operand(ops, 2)?)?,
        }),
        Opcode::put_list => Ok(Instr::PutList {
            head: op(ops, 0)?,
            tail: op(ops, 1)?,
            dst: op(ops, 2)?,
        }),
        Opcode::get_list => Ok(Instr::GetList {
            source: op(ops, 0)?,
            head: op(ops, 1)?,
            tail: op(ops, 2)?,
        }),
        Opcode::get_hd => Ok(Instr::GetHd {
            source: op(ops, 0)?,
            head: op(ops, 1)?,
        }),
        Opcode::get_tl => Ok(Instr::GetTl {
            source: op(ops, 0)?,
            tail: op(ops, 1)?,
        }),

        // ---- calls and funs ---------------------------------------------------
        Opcode::call_fun => Ok(Instr::CallFun {
            arity: as_u32(operand(ops, 0)?)?,
        }),
        // `call_fun2 Tag Arity Func` — `Tag` is the atom `safe`/`unsafe` or, when the
        // target fun is known statically, the file's fun-table (`FunT`) index.
        Opcode::call_fun2 => Ok(Instr::CallFun2 {
            tag: op(ops, 0)?,
            arity: as_u32(operand(ops, 1)?)?,
            func: op(ops, 2)?,
        }),
        Opcode::apply => Ok(Instr::Apply {
            arity: as_u32(operand(ops, 0)?)?,
        }),
        Opcode::apply_last => Ok(Instr::ApplyLast {
            arity: as_u32(operand(ops, 0)?)?,
            deallocate: as_u32(operand(ops, 1)?)?,
        }),
        // `make_fun3 Index Dst {Env}` (`make_fun2` is obsolete and never emitted).
        Opcode::make_fun3 => Ok(Instr::MakeFun3 {
            index: as_u32(operand(ops, 0)?)?,
            dst: op(ops, 1)?,
            env: list(operand(ops, 2)?)?.to_vec(),
        }),

        // ---- non-garbage-collecting BIF calls ---------------------------------
        // `bif0 Bif Dst` — the BIF operand is the index into the `ImpT` chunk.
        Opcode::bif0 => Ok(Instr::Bif0 {
            bif: ImportRef(as_u32(operand(ops, 0)?)?),
            dst: op(ops, 1)?,
        }),
        // `bif{1,2,3} Fail Bif Arg… Dst` — the same layout as `gc_bif*`, minus the
        // live-register count.
        Opcode::bif1 | Opcode::bif2 | Opcode::bif3 => {
            if ops.len() < 3 {
                return Err(bad_file("bif has too few operands"));
            }
            let args = ops[2..ops.len() - 1].to_vec();
            let arity = u8::try_from(args.len()).map_err(|_| BeamError::SystemLimit)?;
            Ok(Instr::Bif {
                arity,
                bif: ImportRef(as_u32(operand(ops, 1)?)?),
                fail: as_label(operand(ops, 0)?)?,
                args,
                dst: ops[ops.len() - 1].clone(),
            })
        }

        // ---- exceptions -------------------------------------------------------
        Opcode::badmatch => Ok(Instr::Badmatch { value: op(ops, 0)? }),
        Opcode::case_end => Ok(Instr::CaseEnd { value: op(ops, 0)? }),
        Opcode::if_end => Ok(Instr::IfEnd),
        Opcode::r#try => Ok(Instr::Try {
            frame: op(ops, 0)?,
            fail: as_label(operand(ops, 1)?)?,
        }),
        Opcode::catch => Ok(Instr::Catch {
            frame: op(ops, 0)?,
            fail: as_label(operand(ops, 1)?)?,
        }),
        Opcode::try_case => Ok(Instr::TryCase { frame: op(ops, 0)? }),
        Opcode::try_end => Ok(Instr::TryEnd { frame: op(ops, 0)? }),
        Opcode::try_case_end => Ok(Instr::TryCaseEnd { value: op(ops, 0)? }),
        Opcode::catch_end => Ok(Instr::CatchEnd { frame: op(ops, 0)? }),
        Opcode::raise => Ok(Instr::Raise {
            trace: op(ops, 0)?,
            value: op(ops, 1)?,
        }),
        Opcode::badrecord => Ok(Instr::Badrecord { value: op(ops, 0)? }),
        Opcode::build_stacktrace => Ok(Instr::BuildStacktrace),
        Opcode::raw_raise => Ok(Instr::RawRaise),

        // ---- floating point ---------------------------------------------------
        Opcode::fmove => Ok(Instr::Fmove {
            src: op(ops, 0)?,
            dst: op(ops, 1)?,
        }),
        Opcode::fconv => Ok(Instr::Fconv {
            src: op(ops, 0)?,
            dst: op(ops, 1)?,
        }),
        // `f{add,sub,mul,div} Fail Lhs Rhs Dst` — four opcodes, one operand layout.
        Opcode::fadd | Opcode::fsub | Opcode::fmul | Opcode::fdiv => {
            let fail = as_label(operand(ops, 0)?)?;
            let lhs = op(ops, 1)?;
            let rhs = op(ops, 2)?;
            let dst = op(ops, 3)?;
            Ok(match raw.opcode {
                Opcode::fadd => Instr::Fadd {
                    fail,
                    lhs,
                    rhs,
                    dst,
                },
                Opcode::fsub => Instr::Fsub {
                    fail,
                    lhs,
                    rhs,
                    dst,
                },
                Opcode::fmul => Instr::Fmul {
                    fail,
                    lhs,
                    rhs,
                    dst,
                },
                _ => Instr::Fdiv {
                    fail,
                    lhs,
                    rhs,
                    dst,
                },
            })
        }
        Opcode::fnegate => Ok(Instr::Fnegate {
            fail: as_label(operand(ops, 0)?)?,
            src: op(ops, 1)?,
            dst: op(ops, 2)?,
        }),

        // ---- line markers, receive and send -----------------------------------
        Opcode::executable_line => Ok(Instr::ExecutableLine {
            location: as_u32(operand(ops, 0)?)?,
            index: as_u32(operand(ops, 1)?)?,
        }),
        Opcode::debug_line => Ok(Instr::DebugLine {
            kind: op(ops, 0)?,
            location: as_u32(operand(ops, 1)?)?,
            index: as_u32(operand(ops, 2)?)?,
            live: as_u32(operand(ops, 3)?)?,
        }),
        Opcode::send => Ok(Instr::Send),
        Opcode::loop_rec => Ok(Instr::LoopRec {
            fail: as_label(operand(ops, 0)?)?,
            dst: op(ops, 1)?,
        }),
        Opcode::loop_rec_end => Ok(Instr::LoopRecEnd {
            target: as_label(operand(ops, 0)?)?,
        }),
        Opcode::wait => Ok(Instr::Wait {
            target: as_label(operand(ops, 0)?)?,
        }),
        Opcode::wait_timeout => Ok(Instr::WaitTimeout {
            target: as_label(operand(ops, 0)?)?,
            time: op(ops, 1)?,
        }),
        Opcode::timeout => Ok(Instr::Timeout),
        Opcode::remove_message => Ok(Instr::RemoveMessage),
        Opcode::recv_marker_reserve => Ok(Instr::RecvMarkerReserve {
            marker: op(ops, 0)?,
        }),
        Opcode::recv_marker_bind => Ok(Instr::RecvMarkerBind {
            marker: op(ops, 0)?,
            reference: op(ops, 1)?,
        }),
        Opcode::recv_marker_clear => Ok(Instr::RecvMarkerClear {
            reference: op(ops, 0)?,
        }),
        Opcode::recv_marker_use => Ok(Instr::RecvMarkerUse {
            reference: op(ops, 0)?,
        }),

        // ---- maps and records -------------------------------------------------
        Opcode::get_map_elements => Ok(Instr::GetMapElements {
            fail: as_label(operand(ops, 0)?)?,
            source: op(ops, 1)?,
            pairs: key_value_pairs(operand(ops, 2)?)?,
        }),
        Opcode::has_map_fields => Ok(Instr::HasMapFields {
            fail: as_label(operand(ops, 0)?)?,
            source: op(ops, 1)?,
            keys: list(operand(ops, 2)?)?.to_vec(),
        }),
        Opcode::put_map_assoc => Ok(Instr::PutMapAssoc(Box::new(MapWriteArgs {
            fail: as_label(operand(ops, 0)?)?,
            source: op(ops, 1)?,
            dst: op(ops, 2)?,
            live: as_u32(operand(ops, 3)?)?,
            pairs: key_value_pairs(operand(ops, 4)?)?,
        }))),
        Opcode::put_map_exact => Ok(Instr::PutMapExact(Box::new(MapWriteArgs {
            fail: as_label(operand(ops, 0)?)?,
            source: op(ops, 1)?,
            dst: op(ops, 2)?,
            live: as_u32(operand(ops, 3)?)?,
            pairs: key_value_pairs(operand(ops, 4)?)?,
        }))),
        // `update_record Hint Size Src Dst {Index,Value}*` — `Hint` is the atom
        // `copy`/`reuse`, `Size` the arity of the tuple being copied.
        Opcode::update_record => Ok(Instr::UpdateRecord(Box::new(UpdateRecordArgs {
            hint: op(ops, 0)?,
            size: as_u32(operand(ops, 1)?)?,
            src: op(ops, 2)?,
            dst: op(ops, 3)?,
            updates: key_value_pairs(operand(ops, 4)?)?,
        }))),
        // ---- native records (OTP 29, generic opcodes 186-193) ------------------
        // One source opcode lowers to one typed variant carrying the file's raw
        // decoded operands (`erts/emulator/beam/emu/ops.tab`), like the `bs_*`
        // family. `is_any_native_record Fail Src` is the `is_record/1` guard
        // (`ops.tab:1634`).
        Opcode::is_any_native_record => Ok(Instr::IsAnyNativeRecord {
            fail: as_label(operand(ops, 0)?)?,
            src: op(ops, 1)?,
        }),
        // `is_native_record Fail Src Mod Name` (`ops.tab:1636`, C `f xy a a`).
        Opcode::is_native_record => Ok(Instr::IsNativeRecord {
            fail: as_label(operand(ops, 0)?)?,
            src: op(ops, 1)?,
            module: op(ops, 2)?,
            name: op(ops, 3)?,
        }),
        // `get_record_elements Fail Src {FieldName,Register}*` (`ops.tab:1641`).
        Opcode::get_record_elements => Ok(Instr::GetRecordElements {
            fail: as_label(operand(ops, 0)?)?,
            src: op(ops, 1)?,
            fields: key_value_pairs(operand(ops, 2)?)?,
        }),
        // `put_record Fail Id Src Dst Live {FieldName,Value}*` (`ops.tab:1648-1654`).
        Opcode::put_record => Ok(Instr::PutRecord(Box::new(PutRecordArgs {
            fail: as_label(operand(ops, 0)?)?,
            id: op(ops, 1)?,
            src: op(ops, 2)?,
            dst: op(ops, 3)?,
            live: as_u32(operand(ops, 4)?)?,
            updates: key_value_pairs(operand(ops, 5)?)?,
        }))),
        // `is_record_accessible Fail Src Scope` (`ops.tab:1672-1678`).
        Opcode::is_record_accessible => Ok(Instr::IsRecordAccessible {
            fail: as_label(operand(ops, 0)?)?,
            src: op(ops, 1)?,
            scope: op(ops, 2)?,
        }),
        // `get_record_field Fail Src Id Field Dst` (`ops.tab:1680-1686`).
        Opcode::get_record_field => Ok(Instr::GetRecordField(Box::new(GetRecordFieldArgs {
            fail: as_label(operand(ops, 0)?)?,
            src: op(ops, 1)?,
            id: op(ops, 2)?,
            field: op(ops, 3)?,
            dst: op(ops, 4)?,
        }))),
        // `get_record_elements_id Fail Id Src {FieldName,Register}*` (OTP 30;
        // `beam_disasm.erl:1424`).
        Opcode::get_record_elements_id => Ok(Instr::GetRecordElementsId {
            fail: as_label(operand(ops, 0)?)?,
            id: op(ops, 1)?,
            src: op(ops, 2)?,
            fields: key_value_pairs(operand(ops, 3)?)?,
        }),
        // `update_record_id Fail Hint Id Src Dst Live {FieldName,Value}*` (OTP 30;
        // `beam_disasm.erl:1428`).
        Opcode::update_record_id => Ok(Instr::UpdateRecordId(Box::new(UpdateRecordIdArgs {
            fail: as_label(operand(ops, 0)?)?,
            hint: op(ops, 1)?,
            id: op(ops, 2)?,
            src: op(ops, 3)?,
            dst: op(ops, 4)?,
            live: as_u32(operand(ops, 5)?)?,
            updates: key_value_pairs(operand(ops, 6)?)?,
        }))),

        // ---- binary syntax ----------------------------------------------------
        Opcode::bs_start_match3 => Ok(Instr::BsStartMatch3 {
            fail: as_label(operand(ops, 0)?)?,
            bin: op(ops, 1)?,
            live: as_u32(operand(ops, 2)?)?,
            dst: op(ops, 3)?,
        }),
        // `bs_start_match4 Fail Live Src Dst` — `Fail` is a label, or the atom
        // `no_fail`/`resume`, so it stays a raw operand rather than a label.
        Opcode::bs_start_match4 => Ok(Instr::BsStartMatch4 {
            fail: op(ops, 0)?,
            live: as_u32(operand(ops, 1)?)?,
            src: op(ops, 2)?,
            dst: op(ops, 3)?,
        }),
        // `bs_match Fail Ctx {Commands}` — the commands stay a flat operand list.
        Opcode::bs_match => Ok(Instr::BsMatch {
            fail: as_label(operand(ops, 0)?)?,
            ctx: op(ops, 1)?,
            commands: list(operand(ops, 2)?)?.to_vec(),
        }),
        // `bs_match_string Fail Ctx Bits Offset` — `Offset` indexes the `StrT` table.
        Opcode::bs_match_string => Ok(Instr::BsMatchString {
            fail: as_label(operand(ops, 0)?)?,
            ctx: op(ops, 1)?,
            bits: as_u32(operand(ops, 2)?)?,
            offset: as_u32(operand(ops, 3)?)?,
        }),
        Opcode::bs_get_tail => Ok(Instr::BsGetTail {
            ctx: op(ops, 0)?,
            dst: op(ops, 1)?,
            live: as_u32(operand(ops, 2)?)?,
        }),
        Opcode::bs_get_position => Ok(Instr::BsGetPosition {
            ctx: op(ops, 0)?,
            dst: op(ops, 1)?,
            live: as_u32(operand(ops, 2)?)?,
        }),
        Opcode::bs_set_position => Ok(Instr::BsSetPosition {
            ctx: op(ops, 0)?,
            pos: op(ops, 1)?,
        }),
        // `bs_get_{integer,float,binary}2 Fail Ctx Live Size Unit Flags Dst` — three
        // opcodes, one operand layout.
        Opcode::bs_get_integer2 | Opcode::bs_get_float2 | Opcode::bs_get_binary2 => {
            let args = Box::new(BsGetArgs {
                fail: as_label(operand(ops, 0)?)?,
                ctx: op(ops, 1)?,
                live: as_u32(operand(ops, 2)?)?,
                size: op(ops, 3)?,
                unit: as_u32(operand(ops, 4)?)?,
                flags: as_u32(operand(ops, 5)?)?,
                dst: op(ops, 6)?,
            });
            Ok(match raw.opcode {
                Opcode::bs_get_integer2 => Instr::BsGetInteger2(args),
                Opcode::bs_get_float2 => Instr::BsGetFloat2(args),
                _ => Instr::BsGetBinary2(args),
            })
        }
        Opcode::bs_skip_bits2 => Ok(Instr::BsSkipBits2 {
            fail: as_label(operand(ops, 0)?)?,
            ctx: op(ops, 1)?,
            size: op(ops, 2)?,
            unit: as_u32(operand(ops, 3)?)?,
            flags: as_u32(operand(ops, 4)?)?,
        }),
        Opcode::bs_test_tail2 => Ok(Instr::BsTestTail2 {
            fail: as_label(operand(ops, 0)?)?,
            ctx: op(ops, 1)?,
            bits: as_u32(operand(ops, 2)?)?,
        }),
        // `bs_get_utf{8,16,32} Fail Ctx Live Flags Dst`.
        Opcode::bs_get_utf8 | Opcode::bs_get_utf16 | Opcode::bs_get_utf32 => {
            let fail = as_label(operand(ops, 0)?)?;
            let ctx = op(ops, 1)?;
            let live = as_u32(operand(ops, 2)?)?;
            let flags = as_u32(operand(ops, 3)?)?;
            let dst = op(ops, 4)?;
            Ok(match raw.opcode {
                Opcode::bs_get_utf8 => Instr::BsGetUtf8 {
                    fail,
                    ctx,
                    live,
                    flags,
                    dst,
                },
                Opcode::bs_get_utf16 => Instr::BsGetUtf16 {
                    fail,
                    ctx,
                    live,
                    flags,
                    dst,
                },
                _ => Instr::BsGetUtf32 {
                    fail,
                    ctx,
                    live,
                    flags,
                    dst,
                },
            })
        }
        // `bs_skip_utf{8,16,32} Fail Ctx Live Flags`.
        Opcode::bs_skip_utf8 | Opcode::bs_skip_utf16 | Opcode::bs_skip_utf32 => {
            let fail = as_label(operand(ops, 0)?)?;
            let ctx = op(ops, 1)?;
            let live = as_u32(operand(ops, 2)?)?;
            let flags = as_u32(operand(ops, 3)?)?;
            Ok(match raw.opcode {
                Opcode::bs_skip_utf8 => Instr::BsSkipUtf8 {
                    fail,
                    ctx,
                    live,
                    flags,
                },
                Opcode::bs_skip_utf16 => Instr::BsSkipUtf16 {
                    fail,
                    ctx,
                    live,
                    flags,
                },
                _ => Instr::BsSkipUtf32 {
                    fail,
                    ctx,
                    live,
                    flags,
                },
            })
        }
        Opcode::bs_init_writable => Ok(Instr::BsInitWritable),
        // `bs_create_bin Fail Alloc Live Unit Dst {Segments}` — the segments stay a
        // flat operand list (type/size/unit/flags/value per segment). Like the frame
        // instructions, `Alloc` is written *either* as a bare word count *or* as a
        // `z,3` allocation list (the compiler hoists a block's allocation into the
        // `bs_create_bin` it precedes, `beam_ssa_codegen.erl`), so it is decoded
        // through `HeapNeed`. The IR keeps the word total, exactly what the real
        // loader produces: it marshals an allocation list to the sum of its heap
        // words (`marshal_allocation_list`, `beam_file.c:2234-2284`).
        Opcode::bs_create_bin => Ok(Instr::BsCreateBin {
            fail: as_label(operand(ops, 0)?)?,
            alloc: HeapNeed::from_operand(operand(ops, 1)?)?.words(),
            live: as_u32(operand(ops, 2)?)?,
            unit: as_u32(operand(ops, 3)?)?,
            dst: op(ops, 4)?,
            segments: list(operand(ops, 5)?)?.to_vec(),
        }),

        // ---- module-level markers ---------------------------------------------
        Opcode::nif_start => Ok(Instr::NifStart),
        Opcode::on_load => Ok(Instr::OnLoad),

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

/// Fetches the operand at `index` as an owned value.
///
/// The IR variants own their operands, so the arms that move an operand into a
/// field clone it through this helper (the counterpart of [`operand`]).
fn op(ops: &[Operand], index: usize) -> Result<Operand> {
    Ok(operand(ops, index)?.clone())
}

/// Requires a `z,1` argument-list operand and borrows its elements.
///
/// A list operand holds both the element count and the elements (the decoder
/// consumes the `u` count inside the `z,1` sub-tag), so the elements are all a
/// caller needs; a non-list operand is a structural mismatch.
fn list(operand: &Operand) -> Result<&[Operand]> {
    match operand {
        Operand::List(items) => Ok(items.as_slice()),
        _ => Err(bad_file("expected an argument-list operand")),
    }
}

/// Splits a flattened `{Value,Label}*` alternative list into `(value, label)`
/// pairs — the shared shape of `select_val` and `select_tuple_arity`.
fn value_label_pairs(operand: &Operand) -> Result<Vec<(Operand, Label)>> {
    let items = list(operand)?;
    if items.len() % 2 != 0 {
        return Err(bad_file("select alternative list has an odd length"));
    }
    let mut pairs = Vec::with_capacity(items.len() / 2);
    for pair in items.chunks_exact(2) {
        pairs.push((pair[0].clone(), as_label(&pair[1])?));
    }
    Ok(pairs)
}

/// Splits a flattened `{Key,Value}*` list into `(key, value)` pairs — the shared
/// shape of the map/record update and lookup instructions.
fn key_value_pairs(operand: &Operand) -> Result<Vec<(Operand, Operand)>> {
    let items = list(operand)?;
    if items.len() % 2 != 0 {
        return Err(bad_file("map/record update list has an odd length"));
    }
    let mut pairs = Vec::with_capacity(items.len() / 2);
    for pair in items.chunks_exact(2) {
        pairs.push((pair[0].clone(), pair[1].clone()));
    }
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    //! Heap/stack-need lowering tests.
    //!
    //! The frame opcodes (`test_heap`, `allocate`, `allocate_heap`) accept the need
    //! operand in *either* the plain-count or the `z,3` allocation-list encoding
    //! (`erts/emulator/beam/emu/ops.tab`); these tests pin both, that the
    //! allocation breakdown survives, and that a real module using the
    //! allocation-list form now loads end to end.

    use super::*;
    use crate::load::load;
    use crate::operand::AllocKind;

    /// One decoded instruction with the given opcode and operands.
    fn raw(opcode: Opcode, operands: Vec<Operand>) -> RawInstr {
        RawInstr {
            opcode,
            operands,
            offset: 0,
        }
    }

    /// A decoded `z,3` allocation entry (`kind`, `number`, `words`), matching what
    /// [`crate::decode`] produces.
    fn entry(kind: u32, number: u32, words: u32) -> AllocKind {
        AllocKind {
            kind,
            number,
            words,
        }
    }

    #[test]
    fn a_plain_integer_need_lowers_to_words() {
        assert_eq!(
            lower_instr(&raw(
                Opcode::test_heap,
                vec![Operand::Integer(5), Operand::Integer(4)],
            ))
            .unwrap(),
            Instr::TestHeap {
                heap_need: HeapNeed::Words(5),
                live: 4,
            }
        );
    }

    #[test]
    fn an_allocation_list_need_lowers_to_alloc_preserving_entries() {
        // `{test_heap,{alloc,[{words,1},{floats,0},{funs,1}]},1}` (the fixture form);
        // one fun is `ERL_FUN_SIZE` = 2 words.
        let entries = vec![entry(0, 1, 1), entry(1, 0, 0), entry(2, 1, 2)];
        assert_eq!(
            lower_instr(&raw(
                Opcode::test_heap,
                vec![Operand::alloc(entries.clone()), Operand::Integer(1)],
            ))
            .unwrap(),
            Instr::TestHeap {
                heap_need: HeapNeed::Alloc(entries),
                live: 1,
            }
        );
    }

    #[test]
    fn allocate_and_allocate_heap_accept_both_need_forms() {
        // `allocate Ns Live` — plain count.
        assert_eq!(
            lower_instr(&raw(
                Opcode::allocate,
                vec![Operand::Integer(1), Operand::Integer(2)],
            ))
            .unwrap(),
            Instr::Allocate {
                stack_need: HeapNeed::Words(1),
                live: 2,
            }
        );
        // `allocate Ns Live` — allocation list.
        let ns = vec![entry(0, 3, 3)];
        assert_eq!(
            lower_instr(&raw(
                Opcode::allocate,
                vec![Operand::alloc(ns.clone()), Operand::Integer(2)],
            ))
            .unwrap(),
            Instr::Allocate {
                stack_need: HeapNeed::Alloc(ns),
                live: 2,
            }
        );
        // `allocate_heap Ns Nh Live` — both plain.
        assert_eq!(
            lower_instr(&raw(
                Opcode::allocate_heap,
                vec![
                    Operand::Integer(4),
                    Operand::Integer(6),
                    Operand::Integer(3),
                ],
            ))
            .unwrap(),
            Instr::AllocateHeap {
                stack_need: HeapNeed::Words(4),
                heap_need: HeapNeed::Words(6),
                live: 3,
            }
        );
        // `allocate_heap Ns Nh Live` — mixed: an allocation-list stack need and a
        // plain heap need.
        let ns = vec![entry(0, 1, 1), entry(1, 2, 4)];
        assert_eq!(
            lower_instr(&raw(
                Opcode::allocate_heap,
                vec![
                    Operand::alloc(ns.clone()),
                    Operand::Integer(6),
                    Operand::Integer(3),
                ],
            ))
            .unwrap(),
            Instr::AllocateHeap {
                stack_need: HeapNeed::Alloc(ns),
                heap_need: HeapNeed::Words(6),
                live: 3,
            }
        );
    }

    #[test]
    fn heap_need_words_recovers_the_total() {
        assert_eq!(HeapNeed::Words(7).words(), 7);
        assert_eq!(HeapNeed::Alloc(vec![]).words(), 0);
        // `{alloc,[{words,1},{floats,0},{funs,1}]}` → 1 + 0 + (2 × 1) = 3.
        assert_eq!(
            HeapNeed::Alloc(vec![entry(0, 1, 1), entry(1, 0, 0), entry(2, 1, 2)]).words(),
            3
        );
        // `{alloc,[{words,3},{floats,1},{funs,0}]}` → 3 + 2 + 0 = 5.
        assert_eq!(
            HeapNeed::Alloc(vec![entry(0, 3, 3), entry(1, 1, 2), entry(2, 0, 0)]).words(),
            5
        );
    }

    #[test]
    fn a_structurally_wrong_need_operand_is_rejected() {
        // A register is not a need operand.
        assert!(matches!(
            HeapNeed::from_operand(&Operand::X(0)),
            Err(BeamError::BadArg)
        ));
        // An argument list (`z,1`) is not an allocation list (`z,3`).
        assert!(matches!(
            HeapNeed::from_operand(&Operand::list(vec![Operand::Integer(1)])),
            Err(BeamError::BadArg)
        ));
        // A negative integer is not a word count.
        assert!(matches!(
            HeapNeed::from_operand(&Operand::Integer(-1)),
            Err(BeamError::BadArg)
        ));
        // A short instruction (no `live`) is rejected, like any other arity mismatch.
        assert!(matches!(
            lower_instr(&raw(Opcode::test_heap, vec![Operand::Integer(5)])),
            Err(BeamError::BadArg)
        ));
    }

    /// The committed `coverage.beam` fixture (`tests/fixtures/coverage.beam`),
    /// compiled with the system `erlc`; `tests/fixtures/README.md` documents its
    /// regeneration. It exercises the `z,3` allocation-list form of `test_heap`
    /// that plain-integer lowering used to reject.
    const COVERAGE_BEAM: &[u8] = include_bytes!("../../tests/fixtures/coverage.beam");
    /// The committed `slice.beam` fixture (the vertical-slice module).
    const SLICE_BEAM: &[u8] = include_bytes!("../../tests/fixtures/slice.beam");

    #[test]
    fn the_committed_fixtures_load_end_to_end() {
        assert!(
            load(COVERAGE_BEAM).is_ok(),
            "coverage.beam must load end to end"
        );
        assert!(load(SLICE_BEAM).is_ok(), "slice.beam must load end to end");
    }

    #[test]
    fn the_coverage_fixture_reports_both_test_heap_forms() {
        let module = load(COVERAGE_BEAM).unwrap();

        // Every alloc-form `test_heap` in the module, as `(entries, word total)`.
        let mut alloc_forms: Vec<(Vec<AllocKind>, u32)> = Vec::new();
        let mut plain_count = 0usize;
        for function in &module.functions {
            for instr in &function.code {
                if let Instr::TestHeap { heap_need, .. } = instr {
                    match heap_need {
                        HeapNeed::Words(_) => plain_count += 1,
                        HeapNeed::Alloc(entries) => {
                            alloc_forms.push((entries.clone(), heap_need.words()));
                        }
                    }
                }
            }
        }

        // The fixture has three plain-integer `test_heap`s and exactly two
        // allocation-list ones (see `beam_disasm` in `tests/fixtures/README.md`).
        assert_eq!(plain_count, 3, "plain-integer test_heap count");
        assert_eq!(alloc_forms.len(), 2, "allocation-list test_heap count");

        let mut saw_funs = false;
        let mut saw_floats = false;
        for (entries, total) in &alloc_forms {
            match total {
                3 => {
                    // `{alloc,[{words,1},{floats,0},{funs,1}]}` → 1 + 0 + 2.
                    assert_eq!(*entries, vec![entry(0, 1, 1), entry(1, 0, 0), entry(2, 1, 2)]);
                    saw_funs = true;
                }
                5 => {
                    // `{alloc,[{words,3},{floats,1},{funs,0}]}` → 3 + 2 + 0.
                    assert_eq!(*entries, vec![entry(0, 3, 3), entry(1, 1, 2), entry(2, 0, 0)]);
                    saw_floats = true;
                }
                other => panic!("unexpected allocation-list word total {other}"),
            }
        }
        assert!(saw_funs, "the funs-capturing allocation form is missing");
        assert!(saw_floats, "the floats-capturing allocation form is missing");
    }

    #[test]
    fn is_any_native_record_lowers_to_its_typed_variant() {
        // `is_any_native_record f xy` (`ops.tab:1634`): an `f` failure label then an
        // x/y source register — the file's operand order.
        assert_eq!(
            lower_instr(&raw(
                Opcode::is_any_native_record,
                vec![Operand::Label(Label(2)), Operand::X(0)],
            ))
            .unwrap(),
            Instr::IsAnyNativeRecord {
                fail: Label(2),
                src: Operand::X(0),
            }
        );
        // The source may equally be a Y register.
        assert_eq!(
            lower_instr(&raw(
                Opcode::is_any_native_record,
                vec![Operand::Label(Label(7)), Operand::Y(3)],
            ))
            .unwrap(),
            Instr::IsAnyNativeRecord {
                fail: Label(7),
                src: Operand::Y(3),
            }
        );
        // The `f,0` failure sentinel is a valid label.
        assert_eq!(
            lower_instr(&raw(
                Opcode::is_any_native_record,
                vec![Operand::Label(crate::operand::NO_FAIL), Operand::X(1)],
            ))
            .unwrap(),
            Instr::IsAnyNativeRecord {
                fail: crate::operand::NO_FAIL,
                src: Operand::X(1),
            }
        );
    }

    #[test]
    fn is_any_native_record_rejects_a_wrongly_typed_operand() {
        // The failure operand is a label (`f`), never a register.
        assert!(matches!(
            lower_instr(&raw(
                Opcode::is_any_native_record,
                vec![Operand::X(0), Operand::X(1)],
            )),
            Err(BeamError::BadArg)
        ));
        // A structurally short instruction (no source) is rejected, like any
        // other arity mismatch.
        assert!(matches!(
            lower_instr(&raw(
                Opcode::is_any_native_record,
                vec![Operand::Label(Label(2))],
            )),
            Err(BeamError::BadArg)
        ));
    }

    /// The atoms used by the native-record tests, interned into one shared table so
    /// the `a` operands are realistic.
    fn record_atoms() -> Vec<Atom> {
        let mut table = beam_term::AtomTable::new();
        vec![
            table.intern("rec"),
            table.intern("recdef"),
            table.intern("a"),
            table.intern("b"),
            table.intern("copy"),
            table.intern("external"),
        ]
    }

    #[test]
    fn native_record_family_lowers_to_typed_variants() {
        let at = record_atoms();
        let (rec, recdef, fa, fb, copy, external) = (at[0], at[1], at[2], at[3], at[4], at[5]);

        // `is_native_record Fail Src Mod Name` (`ops.tab:1636`): Fail, Src, Mod, Name.
        assert_eq!(
            lower_instr(&raw(
                Opcode::is_native_record,
                vec![
                    Operand::Label(Label(7)),
                    Operand::X(0),
                    Operand::Atom(recdef),
                    Operand::Atom(rec),
                ],
            ))
            .unwrap(),
            Instr::IsNativeRecord {
                fail: Label(7),
                src: Operand::X(0),
                module: Operand::Atom(recdef),
                name: Operand::Atom(rec),
            }
        );

        // `get_record_elements Fail Src {FieldName,Register}*` (`ops.tab:1641`): the
        // field list is the file's flattened `[Name, Reg, …]`.
        assert_eq!(
            lower_instr(&raw(
                Opcode::get_record_elements,
                vec![
                    Operand::Label(Label(3)),
                    Operand::X(0),
                    Operand::list(vec![
                        Operand::Atom(fb),
                        Operand::X(2),
                        Operand::Atom(fa),
                        Operand::X(1),
                    ]),
                ],
            ))
            .unwrap(),
            Instr::GetRecordElements {
                fail: Label(3),
                src: Operand::X(0),
                fields: vec![
                    (Operand::Atom(fb), Operand::X(2)),
                    (Operand::Atom(fa), Operand::X(1)),
                ],
            }
        );

        // `put_record Fail Id Src Dst Live {FieldName,Value}*` (`ops.tab:1648-1654`):
        // the create form has `Src = nil` and an atom `Id`.
        assert_eq!(
            lower_instr(&raw(
                Opcode::put_record,
                vec![
                    Operand::Label(crate::operand::NO_FAIL),
                    Operand::Atom(rec),
                    Operand::Nil,
                    Operand::X(0),
                    Operand::Integer(1),
                    Operand::list(vec![
                        Operand::Atom(fa),
                        Operand::Integer(1),
                        Operand::Atom(fb),
                        Operand::Integer(2),
                    ]),
                ],
            ))
            .unwrap(),
            Instr::PutRecord(Box::new(PutRecordArgs {
                fail: crate::operand::NO_FAIL,
                id: Operand::Atom(rec),
                src: Operand::Nil,
                dst: Operand::X(0),
                live: 1,
                updates: vec![
                    (Operand::Atom(fa), Operand::Integer(1)),
                    (Operand::Atom(fb), Operand::Integer(2)),
                ],
            }))
        );

        // `is_record_accessible Fail Src Scope` (`ops.tab:1672-1678`): the scope atom
        // `external` (or the current module).
        assert_eq!(
            lower_instr(&raw(
                Opcode::is_record_accessible,
                vec![
                    Operand::Label(Label(9)),
                    Operand::X(0),
                    Operand::Atom(external),
                ],
            ))
            .unwrap(),
            Instr::IsRecordAccessible {
                fail: Label(9),
                src: Operand::X(0),
                scope: Operand::Atom(external),
            }
        );

        // `get_record_field Fail Src Id Field Dst` (`ops.tab:1680-1686`): a foreign
        // record's `Id` is a `{Module,Name}` literal.
        assert_eq!(
            lower_instr(&raw(
                Opcode::get_record_field,
                vec![
                    Operand::Label(crate::operand::NO_FAIL),
                    Operand::X(0),
                    Operand::Literal(1),
                    Operand::Atom(fa),
                    Operand::X(0),
                ],
            ))
            .unwrap(),
            Instr::GetRecordField(Box::new(GetRecordFieldArgs {
                fail: crate::operand::NO_FAIL,
                src: Operand::X(0),
                id: Operand::Literal(1),
                field: Operand::Atom(fa),
                dst: Operand::X(0),
            }))
        );

        // `get_record_elements_id Fail Id Src Elements` (opcode 192, OTP 30).
        assert_eq!(
            lower_instr(&raw(
                Opcode::get_record_elements_id,
                vec![
                    Operand::Label(Label(4)),
                    Operand::Atom(rec),
                    Operand::Y(1),
                    Operand::list(vec![Operand::Atom(fa), Operand::X(1)]),
                ],
            ))
            .unwrap(),
            Instr::GetRecordElementsId {
                fail: Label(4),
                id: Operand::Atom(rec),
                src: Operand::Y(1),
                fields: vec![(Operand::Atom(fa), Operand::X(1))],
            }
        );

        // `update_record_id Fail Hint Id Src Dst Live Updates` (opcode 193, OTP 30).
        assert_eq!(
            lower_instr(&raw(
                Opcode::update_record_id,
                vec![
                    Operand::Label(crate::operand::NO_FAIL),
                    Operand::Atom(copy),
                    Operand::Literal(2),
                    Operand::X(0),
                    Operand::X(0),
                    Operand::Integer(2),
                    Operand::list(vec![Operand::Atom(fa), Operand::Integer(7)]),
                ],
            ))
            .unwrap(),
            Instr::UpdateRecordId(Box::new(UpdateRecordIdArgs {
                fail: crate::operand::NO_FAIL,
                hint: Operand::Atom(copy),
                id: Operand::Literal(2),
                src: Operand::X(0),
                dst: Operand::X(0),
                live: 2,
                updates: vec![(Operand::Atom(fa), Operand::Integer(7))],
            }))
        );
    }

    #[test]
    fn native_record_family_rejects_wrongly_typed_operands() {
        let at = record_atoms();
        let (rec, fa) = (at[0], at[2]);

        // The failure operand is a label (`f`), never a register.
        assert!(matches!(
            lower_instr(&raw(
                Opcode::is_native_record,
                vec![
                    Operand::X(0),
                    Operand::X(1),
                    Operand::Atom(rec),
                    Operand::Atom(rec),
                ],
            )),
            Err(BeamError::BadArg)
        ));
        // An odd-length record-element list cannot be split into pairs.
        assert!(matches!(
            lower_instr(&raw(
                Opcode::get_record_elements,
                vec![
                    Operand::Label(Label(1)),
                    Operand::X(0),
                    Operand::list(vec![Operand::Atom(fa), Operand::X(1), Operand::Atom(fa)]),
                ],
            )),
            Err(BeamError::BadArg)
        ));
        // `Live` must be an unsigned integer, not a register.
        assert!(matches!(
            lower_instr(&raw(
                Opcode::put_record,
                vec![
                    Operand::Label(Label(0)),
                    Operand::Atom(rec),
                    Operand::Nil,
                    Operand::X(0),
                    Operand::X(0),
                    Operand::list(vec![Operand::Atom(fa), Operand::Integer(1)]),
                ],
            )),
            Err(BeamError::BadArg)
        ));
        // The record elements must arrive as an argument list (`z,1`), not a register.
        assert!(matches!(
            lower_instr(&raw(
                Opcode::get_record_elements_id,
                vec![
                    Operand::Label(Label(2)),
                    Operand::Atom(rec),
                    Operand::X(0),
                    Operand::X(1),
                ],
            )),
            Err(BeamError::BadArg)
        ));
        // A structurally short `update_record_id` is rejected like any arity mismatch.
        assert!(matches!(
            lower_instr(&raw(
                Opcode::update_record_id,
                vec![Operand::Label(Label(0)), Operand::Atom(rec)],
            )),
            Err(BeamError::BadArg)
        ));
    }
}
