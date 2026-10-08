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

//! The typed BEAM instruction vocabulary: [`Instr`] and [`TestKind`].
//!
//! This module owns the loader's *semantic* instruction surface — the typed form
//! of every generic opcode the loader models. It is re-exported from
//! [`crate::ir`] (`pub use crate::instr::*`), so the frozen IR paths
//! (`crate::ir::Instr`, `crate::ir::TestKind`, `crate::Instr`,
//! `beam_loader::prelude::*`) are unchanged and the engine still matches on one
//! vocabulary.
//!
//! * [`Instr`] — one named variant per modelled opcode, plus the lossless
//!   [`Instr::Generic`] fallback for opcodes the loader deliberately does *not*
//!   model (see `lower` for the rule and the crate `CONTEXT.md` for the list).
//! * [`TestKind`] — the classification of the `is_*`/`test_arity` test family;
//!   `Instr::Test` carries it plus the failure label and the tested operands.
//!
//! # Operand order
//!
//! Every variant's field order is the **file's** operand order, derived from
//! `erts/emulator/beam/emu/ops.tab` (+ `genop.tab`'s `@spec` lines) and
//! cross-checked against `beam_disasm:file/1` on real compiler output — operands
//! are never re-ordered for convenience, so a variant reads exactly like the
//! disassembly of the instruction it models.
//!
//! # Why one flat enum
//!
//! `Instr` is a single flat data enum (like [`crate::opcode`]) rather than
//! split across submodules: an enum cannot be spread over modules, and splitting
//! the *variants* would destroy the one-name surface downstream matches on.
//!
//! # Size budget
//!
//! Every instruction of a loaded module lives in the code array the interpreter
//! walks, so `size_of::<Instr>()` is a dispatch-cost constant: the per-instruction
//! `match` reads the discriminant and the taken variant's fields out of the same
//! cache line(s), and a larger enum means a sparser array. The enums are therefore
//! size-pinned by the `const` guards at the bottom of this file — `Instr` at 64
//! bytes (one cache line) and `Operand` at 16 (two words).
//!
//! Three rules keep them there, and all three are load-time-only costs:
//!
//! * **Hot variants stay inline and small.** The control/register/BIF set the
//!   interpreter runs constantly — `Move`/`Swap`/`Test`/`SelectVal`/`Jump`, the
//!   `GcBif`/`Bif`/`Bif0`/`Call*`/`Allocate*`/`Return` families and the whole
//!   receive family — is never boxed.
//! * **Operand-heavy cold variants are boxed.** A variant with three or more
//!   operands *and* an update list (the native-record family, `update_record`, the
//!   map writes, the `bs_get_*2` segment readers) carries them in a boxed bundle
//!   (`MapWriteArgs`, `PutRecordArgs`, `BsGetArgs`, …) instead of inline.
//! * **`Operand` carries no inline `Vec`.** Its three decode-time-only variants
//!   (`BigInteger`/`List`/`Allocation`) hold boxed payloads; the leaf forms — the
//!   only ones a loaded [`Module`](crate::ir::Module) contains — are two words.
use crate::ir::{Atom, ImportRef, LineRef};
use crate::opcode::Opcode;
use crate::operand::{HeapNeed, Label, Operand};

/// The `is_*` family of test instructions.
///
/// The opcode → kind mapping is the loader's job (the opcode roster carries the
/// op name; the kind is the semantic classification the engine matches on). A
/// classified opcode always lowers to [`Instr::Test`], whose `args` are the test's
/// operands in file order (the failure label is *not* part of `args`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TestKind {
    /// `is_lt/3` — `Fail Arg1 Arg2`.
    IsLt,
    /// `is_ge/3` — `Fail Arg1 Arg2`.
    IsGe,
    /// `is_eq/3` — `Fail Arg1 Arg2`.
    IsEq,
    /// `is_ne/3` — `Fail Arg1 Arg2`.
    IsNe,
    /// `is_eq_exact/3` — `Fail Arg1 Arg2`.
    IsEqExact,
    /// `is_ne_exact/3` — `Fail Arg1 Arg2`.
    IsNeExact,
    /// `is_integer/2` — `Fail Arg1`.
    IsInteger,
    /// `is_float/2` — `Fail Arg1`.
    IsFloat,
    /// `is_number/2` — `Fail Arg1`.
    IsNumber,
    /// `is_atom/2` — `Fail Arg1`.
    IsAtom,
    /// `is_pid/2` — `Fail Arg1`.
    IsPid,
    /// `is_reference/2` — `Fail Arg1`.
    IsReference,
    /// `is_port/2` — `Fail Arg1`.
    IsPort,
    /// `is_nil/2` — `Fail Arg1`.
    IsNil,
    /// `is_binary/2` — `Fail Arg1`.
    IsBinary,
    /// `is_bitstr/2` — `Fail Arg1`.
    IsBitstr,
    /// `is_list/2` — `Fail Arg1`.
    IsList,
    /// `is_nonempty_list/2` — `Fail Arg1`.
    IsNonemptyList,
    /// `is_tuple/2` — `Fail Arg1`.
    IsTuple,
    /// `is_map/2` — `Fail Arg1`.
    IsMap,
    /// `is_boolean/2` — `Fail Arg1`.
    IsBoolean,
    /// `is_function/2` — `Fail Arg1`.
    IsFunction,
    /// `is_function2/3` — `Fail Arg1 Arity`, i.e. a function of the given arity.
    IsFunction2,
    /// `is_tagged_tuple/4` — `Fail Src Arity Tag`, i.e. a tuple of the given size
    /// whose first element is `Tag` (the compiler's record/tag test).
    IsTaggedTuple,
    /// `test_arity/3` — `Fail Arg1 Arity`.
    TestArity,
}

impl TestKind {
    /// Classifies a test opcode into its [`TestKind`], or `None` if the opcode is
    /// not one of the `is_*`/`test_arity` family.
    pub fn from_opcode(op: Opcode) -> Option<TestKind> {
        Some(match op {
            Opcode::is_lt => TestKind::IsLt,
            Opcode::is_ge => TestKind::IsGe,
            Opcode::is_eq => TestKind::IsEq,
            Opcode::is_ne => TestKind::IsNe,
            Opcode::is_eq_exact => TestKind::IsEqExact,
            Opcode::is_ne_exact => TestKind::IsNeExact,
            Opcode::is_integer => TestKind::IsInteger,
            Opcode::is_float => TestKind::IsFloat,
            Opcode::is_number => TestKind::IsNumber,
            Opcode::is_atom => TestKind::IsAtom,
            Opcode::is_pid => TestKind::IsPid,
            Opcode::is_reference => TestKind::IsReference,
            Opcode::is_port => TestKind::IsPort,
            Opcode::is_nil => TestKind::IsNil,
            Opcode::is_binary => TestKind::IsBinary,
            Opcode::is_bitstr => TestKind::IsBitstr,
            Opcode::is_list => TestKind::IsList,
            Opcode::is_nonempty_list => TestKind::IsNonemptyList,
            Opcode::is_tuple => TestKind::IsTuple,
            Opcode::is_map => TestKind::IsMap,
            Opcode::is_boolean => TestKind::IsBoolean,
            Opcode::is_function => TestKind::IsFunction,
            Opcode::is_function2 => TestKind::IsFunction2,
            Opcode::is_tagged_tuple => TestKind::IsTaggedTuple,
            Opcode::test_arity => TestKind::TestArity,
            // Every other opcode — including the obsolete `is_constant/2`, the
            // `bs_get_*`/`bs_skip_*` families (which carry a match context and so
            // get their own variants) and the OTP-29 native-record tests — is not
            // a plain test instruction the engine matches on.
            _ => return None,
        })
    }
}

/// A lowered BEAM instruction.
///
/// Named variants cover the opcode set the compiler emits; an opcode the loader
/// deliberately does not model is preserved losslessly as [`Instr::Generic`], so
/// nothing is lost and the engine/JIT can extend without a format change.
///
/// The variant order mirrors the families of
/// `erts/emulator/beam/emu/ops.tab`; within a family a variant's fields are in
/// the file's operand order.
#[derive(Clone, PartialEq, Debug)]
pub enum Instr {
    // ---- module / function framing ---------------------------------------
    /// `label Lbl` — marks the start of a basic block.
    Label(Label),
    /// A source-location marker (from the `Line` chunk).
    Line(LineRef),
    /// `func_info M F A` — defines the enclosing function.
    FuncInfo {
        /// The function's module.
        module: Atom,
        /// The function's name.
        function: Atom,
        /// The function's arity.
        arity: u32,
    },
    /// `int_code_end` — terminates the instruction stream for a module.
    IntCodeEnd,

    // ---- moving / register shuffling --------------------------------------
    /// `move Src Dst`.
    Move {
        /// The source operand.
        src: Operand,
        /// The destination operand.
        dst: Operand,
    },
    /// `swap A B` — exchange two registers.
    Swap {
        /// One register.
        a: Operand,
        /// The other register.
        b: Operand,
    },

    // ---- calls -------------------------------------------------------------
    /// `call Arity Label` — a local call preserving the frame.
    Call {
        /// The callee arity.
        arity: u32,
        /// The local target label.
        target: Label,
    },
    /// `call_last Arity Label Deallocate` — a local tail call.
    CallLast {
        /// The callee arity.
        arity: u32,
        /// The local target label.
        target: Label,
        /// The number of stack slots to deallocate first.
        deallocate: u32,
    },
    /// `call_only Arity Label` — a local tail call that deallocates the frame.
    CallOnly {
        /// The callee arity.
        arity: u32,
        /// The local target label.
        target: Label,
    },
    /// `call_ext Arity Bif` — an external call preserving the frame.
    CallExt {
        /// The callee arity.
        arity: u32,
        /// The imported call target.
        target: ImportRef,
    },
    /// `call_ext_only Arity Bif` — an external tail call.
    CallExtOnly {
        /// The callee arity.
        arity: u32,
        /// The imported call target.
        target: ImportRef,
    },
    /// `call_ext_last Arity Bif Deallocate` — an external tail call.
    CallExtLast {
        /// The callee arity.
        arity: u32,
        /// The imported call target.
        target: ImportRef,
        /// The number of stack slots to deallocate first.
        deallocate: u32,
    },
    /// `bif0 Bif Dst` — a non-garbage-collecting BIF call without arguments.
    Bif0 {
        /// The imported `{module,function,arity}` to call.
        bif: ImportRef,
        /// The destination operand.
        dst: Operand,
    },
    /// `bif1`/`bif2`/`bif3` — a non-garbage-collecting BIF call.
    ///
    /// The file's operand order is `Fail Bif Arg… Dst`; `arity` is kept explicit
    /// (it is `args.len()`) so the engine can read it without counting.
    Bif {
        /// The number of arguments (the BIF's arity).
        arity: u8,
        /// The imported `{module,function,arity}` to call.
        bif: ImportRef,
        /// The failure label ([`crate::operand::NO_FAIL`] means "raise on failure").
        fail: Label,
        /// The argument operands.
        args: Vec<Operand>,
        /// The destination operand.
        dst: Operand,
    },
    /// `gc_bif1`/`gc_bif2`/`gc_bif3` — a garbage-collecting BIF call.
    GcBif {
        /// The number of arguments (the BIF's arity).
        arity: u8,
        /// The imported `{module,function,arity}` to call.
        bif: ImportRef,
        /// The failure label ([`crate::operand::NO_FAIL`] means "raise on failure").
        fail: Label,
        /// The number of live X registers at the call.
        live: u32,
        /// The argument operands.
        args: Vec<Operand>,
        /// The destination operand.
        dst: Operand,
    },
    /// `call_fun Arity` — call the fun in `x(Arity)` with arguments `x(0)..x(Arity-1)`.
    CallFun {
        /// The arity of the fun being called.
        arity: u32,
    },
    /// `call_fun2 Tag Arity Func` — call the fun in `Func`.
    ///
    /// `tag` is the compiler's knowledge hint: an atom (`safe`/`unsafe`) or, when
    /// the compiler knows the fun statically, the file's fun-table index (an
    /// integer operand naming a `FunT` entry, resolvable through
    /// [`Module::fun`](crate::ir::Module::fun)).
    CallFun2 {
        /// The fun tag: an atom (`safe`/`unsafe`) or a fun-table index.
        tag: Operand,
        /// The arity of the fun being called.
        arity: u32,
        /// The register holding the fun.
        func: Operand,
    },
    /// `apply Arity` — apply a fun/`M,F,A` with `Arity` arguments in `x(0)..`.
    Apply {
        /// The number of arguments to apply.
        arity: u32,
    },
    /// `apply_last Arity Deallocate` — a tail `apply`.
    ApplyLast {
        /// The number of arguments to apply.
        arity: u32,
        /// The number of stack slots to deallocate first.
        deallocate: u32,
    },

    // ---- funs --------------------------------------------------------------
    /// `make_fun3 Index Dst Env` — build a closure from the `FunT` entry `Index`.
    ///
    /// `index` is the file's fun-table *index* (the compiler's "old index");
    /// resolve it to the lambda's descriptor with
    /// [`Module::fun`](crate::ir::Module::fun) (or the whole table with
    /// [`Module::funs`](crate::ir::Module::funs)).
    MakeFun3 {
        /// The fun-table (`FunT`) index of the closure being built.
        index: u32,
        /// The destination operand.
        dst: Operand,
        /// The free variables captured by the closure, in order.
        env: Vec<Operand>,
    },

    // ---- tests / branching -------------------------------------------------
    /// An `is_*`/`test_arity` test instruction.
    Test {
        /// The classified test.
        kind: TestKind,
        /// The failure label ([`crate::operand::NO_FAIL`] means "raise on failure").
        fail: Label,
        /// The tested operand(s), in file order.
        args: Vec<Operand>,
    },
    /// `select_val Value Fail {Value,Label}*` — a switch on an exact value.
    SelectVal {
        /// The scrutinee operand.
        value: Operand,
        /// The failure label ([`crate::operand::NO_FAIL`] means "raise on failure").
        fail: Label,
        /// The `(value, label)` alternatives.
        pairs: Vec<(Operand, Label)>,
    },
    /// `select_tuple_arity Tuple Fail {Arity,Label}*` — a switch on a tuple's size.
    SelectTupleArity {
        /// The tested tuple.
        tuple: Operand,
        /// The failure label ([`crate::operand::NO_FAIL`] means "raise on failure").
        fail: Label,
        /// The `(arity, label)` alternatives.
        destinations: Vec<(Operand, Label)>,
    },
    /// `jump Label` — an unconditional jump.
    Jump {
        /// The jump target.
        target: Label,
    },

    // ---- stack frames ------------------------------------------------------
    /// `allocate StackNeed Live` — allocate a stack frame.
    ///
    /// `stack_need` is the frame's stack-word need in either encoding (a plain
    /// count or a `z,3` allocation list) — see [`HeapNeed`].
    Allocate {
        /// The frame's stack-word need.
        stack_need: HeapNeed,
        /// The number of live X registers.
        live: u32,
    },
    /// `allocate_heap StackNeed HeapNeed Live` — allocate frame + heap.
    ///
    /// Both needs accept either encoding (a plain count or a `z,3` allocation
    /// list) — see [`HeapNeed`].
    AllocateHeap {
        /// The frame's stack-word need.
        stack_need: HeapNeed,
        /// The heap-word need.
        heap_need: HeapNeed,
        /// The number of live X registers.
        live: u32,
    },
    /// `test_heap HeapNeed Live` — ensure heap headroom.
    ///
    /// `heap_need` accepts either encoding (a plain count or a `z,3` allocation
    /// list) — see [`HeapNeed`].
    TestHeap {
        /// The heap-word need.
        heap_need: HeapNeed,
        /// The number of live X registers.
        live: u32,
    },
    /// `init_yregs {Yregs}` — initialize the listed Y registers to `[]`.
    InitYregs {
        /// The Y registers to clear, in order.
        registers: Vec<Operand>,
    },
    /// `deallocate N` — pop `N` stack slots.
    Deallocate(u32),
    /// `trim N Remaining` — trim the stack to `Remaining` after popping `N`.
    Trim {
        /// The number of stack slots to pop.
        n: u32,
        /// The number of stack slots to keep.
        remaining: u32,
    },
    /// `return` — return from the current function.
    Return,

    // ---- tuples ------------------------------------------------------------
    /// `put_tuple2 Dst {Elements}` — build a tuple from `Elements`.
    PutTuple2 {
        /// The destination operand.
        dst: Operand,
        /// The tuple elements, in order.
        elements: Vec<Operand>,
    },
    /// `get_tuple_element Source Element Dst` — fetch one tuple element.
    GetTupleElement {
        /// The source tuple operand.
        source: Operand,
        /// The 0-based element index.
        element: u32,
        /// The destination operand.
        dst: Operand,
    },

    // ---- lists -------------------------------------------------------------
    /// `put_list Head Tail Dst` — build a cons cell.
    PutList {
        /// The head (car) operand.
        head: Operand,
        /// The tail (cdr) operand.
        tail: Operand,
        /// The destination operand.
        dst: Operand,
    },
    /// `get_list Source Head Tail` — split a cons cell.
    GetList {
        /// The source cons cell.
        source: Operand,
        /// The destination for the head (car).
        head: Operand,
        /// The destination for the tail (cdr).
        tail: Operand,
    },
    /// `get_hd Source Head` — fetch the head (car) of a cons cell.
    GetHd {
        /// The source cons cell.
        source: Operand,
        /// The destination for the head (car).
        head: Operand,
    },
    /// `get_tl Source Tail` — fetch the tail (cdr) of a cons cell.
    GetTl {
        /// The source cons cell.
        source: Operand,
        /// The destination for the tail (cdr).
        tail: Operand,
    },

    // ---- maps --------------------------------------------------------------
    /// `get_map_elements Fail Map {Key,Dst}*` — fetch map elements.
    GetMapElements {
        /// The failure label ([`crate::operand::NO_FAIL`] means "raise on failure").
        fail: Label,
        /// The source map operand.
        source: Operand,
        /// The `(key, destination)` pairs, in file order.
        pairs: Vec<(Operand, Operand)>,
    },
    /// `has_map_fields Fail Map {Key}*` — test for the presence of map keys.
    HasMapFields {
        /// The failure label ([`crate::operand::NO_FAIL`] means "raise on failure").
        fail: Label,
        /// The source map operand.
        source: Operand,
        /// The keys that must be present.
        keys: Vec<Operand>,
    },
    /// `put_map_assoc Fail Src Dst Live {Key,Value}*` — update/build a map.
    ///
    /// The operands live in a boxed [`MapWriteArgs`]; both map writes share the type
    /// because their operand shape is identical.
    PutMapAssoc(Box<MapWriteArgs>),
    /// `put_map_exact Fail Src Dst Live {Key,Value}*` — update a map, failing on
    /// a key the source map does not have.
    ///
    /// The operands live in a boxed [`MapWriteArgs`] (see [`Instr::PutMapAssoc`]).
    PutMapExact(Box<MapWriteArgs>),

    // ---- records -----------------------------------------------------------
    /// `update_record Hint Size Src Dst {Index,Value}*` — copy a tuple with
    /// elements replaced.
    ///
    /// `hint` is the compiler's reuse hint (`copy`/`reuse`); `size` is the arity
    /// of the tuple being copied. The operands live in a boxed
    /// [`UpdateRecordArgs`].
    UpdateRecord(Box<UpdateRecordArgs>),
    /// `is_any_native_record Fail Src` — the `erlang:is_record/1` guard (generic
    /// opcode 186).
    ///
    /// Branches to `Fail` iff `Src` is **not** a native record: a boxed term whose
    /// header carries the `RECORD` subtag (`is_record($Src)` in the C body
    /// `erts/emulator/beam/emu/instrs.tab:1323`; operand shape `f xy` in
    /// `erts/emulator/beam/emu/ops.tab:1634`). It is deliberately **not** a
    /// [`TestKind`] — `beam_engine` matches `TestKind` exhaustively, so classifying
    /// it there would break that surface, whereas a new `Instr` variant is additive.
    ///
    ///
    /// One of the OTP-29 native-record family (generic opcodes 186-193), each
    /// lowered to its own typed variant below; the C ground truth is
    /// `erts/emulator/beam/erl_record.c`.
    IsAnyNativeRecord {
        /// The failure label ([`crate::operand::NO_FAIL`] means "no explicit fail
        /// target").
        fail: Label,
        /// The source x/y register holding the term to test.
        src: Operand,
    },
    /// `is_native_record Fail Src Mod Name` — like [`Instr::IsAnyNativeRecord`] but
    /// additionally requiring the record to be `Name` as defined in `Mod` (generic
    /// opcode 187; `erl_is_native_record`, `emu/instrs.tab:1329`; operand shape
    /// `f xy a a` in `emu/ops.tab:1639`).
    IsNativeRecord {
        /// The failure label.
        fail: Label,
        /// The source x/y register holding the candidate record.
        src: Operand,
        /// The module the record is required to be defined in.
        module: Operand,
        /// The required record name.
        name: Operand,
    },
    /// `get_record_elements Fail Src {FieldName,Register}*` — read the named fields
    /// of the native record in `Src` into the listed registers (generic opcode 188;
    /// `erl_get_record_elements`, `emu/instrs.tab:1347`; `Fail Src Size Matches=*` in
    /// `emu/ops.tab:1641`, where `Size` is the match count derived from the list).
    /// Branches to `Fail` if any field name is absent from the record.
    GetRecordElements {
        /// The failure label.
        fail: Label,
        /// The source x/y register holding the record.
        src: Operand,
        /// The `(field name, destination register)` pairs, in file order.
        fields: Vec<(Operand, Operand)>,
    },
    /// `put_record Fail Id Src Dst Live {FieldName,Value}*` — create (`Src` is `nil`)
    /// or update (`Src` is a register) a native record (generic opcode 189;
    /// `erl_create_native_record_emu`/`erl_update_native_record`,
    /// `emu/instrs.tab:1373,1418`; `Fail Id Src Dst Live Updates=*` in
    /// `emu/ops.tab:1648-1654`). `Id` is the record's compile-time identity (an atom,
    /// or a `{Module,Name}` literal for a foreign record); failure branches to `Fail`.
    ///
    /// The operands live in a boxed [`PutRecordArgs`].
    PutRecord(Box<PutRecordArgs>),
    /// `is_record_accessible Fail Src Scope` — test whether the fields of the native
    /// record in `Src` are accessible from the executing module (generic opcode 190;
    /// `erl_is_record_accessible`/`erl_is_wildcard_record_accessible`,
    /// `emu/instrs.tab:1335,1341`; `f xy a` in `emu/ops.tab:1672-1678`). `Scope` is
    /// the atom `external` (must be exported) or the current module.
    IsRecordAccessible {
        /// The failure label.
        fail: Label,
        /// The source x/y register holding the record.
        src: Operand,
        /// The accessibility scope (`external`, or the current module).
        scope: Operand,
    },
    /// `get_record_field Fail Src Id Field Dst` — read the single field `Field` of the
    /// native record in `Src` into `Dst` (generic opcode 191;
    /// `erl_get_local_record_field`/`erl_get_record_field`, `emu/instrs.tab:1433,1446`;
    /// `Fail Src Id Field Dst` in `emu/ops.tab:1680-1686`). Branches to `Fail` if the
    /// field cannot be extracted; `Id` is the record identity (atom or `{Module,Name}`
    /// literal).
    ///
    /// The operands live in a boxed [`GetRecordFieldArgs`].
    GetRecordField(Box<GetRecordFieldArgs>),
    /// `get_record_elements_id Fail Id Src {FieldName,Register}*` — like
    /// [`Instr::GetRecordElements`] but carrying the record identity `Id` so field
    /// positions can be bound at load time (generic opcode 192, OTP 30; operand shape
    /// `Fail Id Src Elements` per `lib/compiler/src/beam_disasm.erl:1424`). `Id` is an
    /// atom, a `{Module,Name}` literal, or `[]` for an anonymous match.
    GetRecordElementsId {
        /// The failure label.
        fail: Label,
        /// The record identity (atom, `{Module,Name}` literal, or `[]`).
        id: Operand,
        /// The source x/y register holding the record.
        src: Operand,
        /// The `(field name, destination register)` pairs, in file order.
        fields: Vec<(Operand, Operand)>,
    },
    /// `update_record_id Fail Hint Id Src Dst Live {FieldName,Value}*` — update a
    /// native record, carrying the record identity `Id` and the reuse `Hint` like
    /// [`Instr::UpdateRecord`] (generic opcode 193, OTP 30; operand shape
    /// `Fail Hint Id Src Dst Live Updates` per `lib/compiler/src/beam_disasm.erl:1428`).
    ///
    /// The operands live in a boxed [`UpdateRecordIdArgs`].
    UpdateRecordId(Box<UpdateRecordIdArgs>),

    // ---- exceptions --------------------------------------------------------
    /// `badmatch Value` — raise a `{badmatch,Value}` error.
    Badmatch {
        /// The operand that failed to match.
        value: Operand,
    },
    /// `case_end Value` — raise a `{case_clause,Value}` error.
    CaseEnd {
        /// The operand that matched no clause.
        value: Operand,
    },
    /// `if_end` — raise an `if_clause` error.
    IfEnd,
    /// `try Frame Fail` — push the try/catch frame described by `Frame`.
    Try {
        /// The register holding the catch frame.
        frame: Operand,
        /// The label the exception handler starts at.
        fail: Label,
    },
    /// `catch Frame Fail` — push the catch frame described by `Frame`.
    Catch {
        /// The register holding the catch frame.
        frame: Operand,
        /// The label the exception handler starts at.
        fail: Label,
    },
    /// `try_case Frame` — enter a try's handler, unpacking the raised class/reason.
    TryCase {
        /// The register holding the catch frame.
        frame: Operand,
    },
    /// `try_end Frame` — pop the try/catch frame.
    TryEnd {
        /// The register holding the catch frame.
        frame: Operand,
    },
    /// `try_case_end Value` — re-raise the value of an unmatched try clause.
    TryCaseEnd {
        /// The operand to re-raise.
        value: Operand,
    },
    /// `catch_end Frame` — pop the catch frame and build the caught value.
    CatchEnd {
        /// The register holding the catch frame.
        frame: Operand,
    },
    /// `raise Trace Value` — raise `Value` with the raw stacktrace `Trace`.
    Raise {
        /// The raw stacktrace operand.
        trace: Operand,
        /// The exception value operand.
        value: Operand,
    },
    /// `badrecord Value` — raise a `{badrecord,Value}` error.
    Badrecord {
        /// The operand that is not a valid record.
        value: Operand,
    },
    /// `build_stacktrace` — cook the raw stacktrace in `x(0)`.
    BuildStacktrace,
    /// `raw_raise` — raise using the class/value/raw-stacktrace in `x(0..2)`.
    RawRaise,

    // ---- floating point ----------------------------------------------------
    /// `fmove Src Dst` — move between a float register and a value register.
    Fmove {
        /// The source: a float register, value register or float literal.
        src: Operand,
        /// The destination: a float register or value register.
        dst: Operand,
    },
    /// `fconv Src Dst` — convert a number to a float register.
    Fconv {
        /// The source number.
        src: Operand,
        /// The destination float register.
        dst: Operand,
    },
    /// `fadd Fail Lhs Rhs Dst` — floating-point addition.
    Fadd {
        /// The failure label ([`crate::operand::NO_FAIL`] means "raise on failure").
        fail: Label,
        /// The left operand (a float register).
        lhs: Operand,
        /// The right operand (a float register).
        rhs: Operand,
        /// The destination float register.
        dst: Operand,
    },
    /// `fsub Fail Lhs Rhs Dst` — floating-point subtraction.
    Fsub {
        /// The failure label ([`crate::operand::NO_FAIL`] means "raise on failure").
        fail: Label,
        /// The left operand (a float register).
        lhs: Operand,
        /// The right operand (a float register).
        rhs: Operand,
        /// The destination float register.
        dst: Operand,
    },
    /// `fmul Fail Lhs Rhs Dst` — floating-point multiplication.
    Fmul {
        /// The failure label ([`crate::operand::NO_FAIL`] means "raise on failure").
        fail: Label,
        /// The left operand (a float register).
        lhs: Operand,
        /// The right operand (a float register).
        rhs: Operand,
        /// The destination float register.
        dst: Operand,
    },
    /// `fdiv Fail Lhs Rhs Dst` — floating-point division.
    Fdiv {
        /// The failure label ([`crate::operand::NO_FAIL`] means "raise on failure").
        fail: Label,
        /// The left operand (a float register).
        lhs: Operand,
        /// The right operand (a float register).
        rhs: Operand,
        /// The destination float register.
        dst: Operand,
    },
    /// `fnegate Fail Src Dst` — floating-point negation.
    Fnegate {
        /// The failure label ([`crate::operand::NO_FAIL`] means "raise on failure").
        fail: Label,
        /// The operand (a float register).
        src: Operand,
        /// The destination float register.
        dst: Operand,
    },

    // ---- messages / receive -------------------------------------------------
    /// `send` — send `x(1)` to `x(0)`.
    Send,
    /// `loop_rec Fail Dst` — fetch the next message into `Dst` or jump to `Fail`.
    LoopRec {
        /// The label to jump to when the mailbox is empty.
        fail: Label,
        /// The destination operand for the message.
        dst: Operand,
    },
    /// `loop_rec_end Target` — advance the save pointer and jump back.
    LoopRecEnd {
        /// The label to jump back to.
        target: Label,
    },
    /// `wait Target` — suspend the process, resuming at `Target`.
    Wait {
        /// The label to resume at.
        target: Label,
    },
    /// `wait_timeout Target Time` — set up a receive timeout.
    WaitTimeout {
        /// The label to resume at when the timeout fires.
        target: Label,
        /// The timeout in milliseconds.
        time: Operand,
    },
    /// `timeout` — cancel the receive timeout.
    Timeout,
    /// `remove_message` — unlink the current message from the mailbox.
    RemoveMessage,
    /// `recv_marker_reserve Dst` — create a receive marker.
    RecvMarkerReserve {
        /// The destination operand for the marker.
        marker: Operand,
    },
    /// `recv_marker_bind Marker Reference` — bind a marker to a reference.
    RecvMarkerBind {
        /// The marker operand.
        marker: Operand,
        /// The reference operand.
        reference: Operand,
    },
    /// `recv_marker_clear Reference` — clear the marker bound to `Reference`.
    RecvMarkerClear {
        /// The reference operand.
        reference: Operand,
    },
    /// `recv_marker_use Reference` — continue matching from `Reference`'s marker.
    RecvMarkerUse {
        /// The reference operand.
        reference: Operand,
    },

    // ---- binary syntax -----------------------------------------------------
    /// `bs_start_match3 Fail Bin Live Dst` — start a binary match sequence.
    BsStartMatch3 {
        /// The failure label ([`crate::operand::NO_FAIL`] means "raise on failure").
        fail: Label,
        /// The source binary operand.
        bin: Operand,
        /// The number of live X registers.
        live: u32,
        /// The destination operand for the match context.
        dst: Operand,
    },
    /// `bs_start_match4 Fail Live Src Dst` — start a binary match sequence.
    ///
    /// `fail` is a label, or the atoms `no_fail`/`resume` when the compiler knows
    /// the match cannot fail / the source is already a match context.
    BsStartMatch4 {
        /// The failure label, or the atom `no_fail`/`resume`.
        fail: Operand,
        /// The number of live X registers.
        live: u32,
        /// The source binary operand.
        src: Operand,
        /// The destination operand for the match context.
        dst: Operand,
    },
    /// `bs_match Fail Ctx {Commands}` — match fixed-size binary segments.
    ///
    /// `commands` is the raw flattened command list (`ensure_at_least`,
    /// `ensure_exactly`, `integer`, `binary`, `skip`, `get_tail`, `=:=`); decoding
    /// the command records is the engine's job.
    BsMatch {
        /// The failure label ([`crate::operand::NO_FAIL`] means "raise on failure").
        fail: Label,
        /// The match context operand.
        ctx: Operand,
        /// The flattened command list.
        commands: Vec<Operand>,
    },
    /// `bs_match_string Fail Ctx Bits Offset` — compare the next bits with a string.
    BsMatchString {
        /// The failure label ([`crate::operand::NO_FAIL`] means "raise on failure").
        fail: Label,
        /// The match context operand.
        ctx: Operand,
        /// The number of bits to compare.
        bits: u32,
        /// The byte offset of the string in the `StrT` chunk's string table.
        offset: u32,
    },
    /// `bs_get_tail Ctx Dst Live` — bind `Dst` to the remaining bits of `Ctx`.
    BsGetTail {
        /// The match context operand.
        ctx: Operand,
        /// The destination operand.
        dst: Operand,
        /// The number of live X registers.
        live: u32,
    },
    /// `bs_get_position Ctx Dst Live` — save the match position into `Dst`.
    BsGetPosition {
        /// The match context operand.
        ctx: Operand,
        /// The destination operand.
        dst: Operand,
        /// The number of live X registers.
        live: u32,
    },
    /// `bs_set_position Ctx Pos` — restore the match position of `Ctx`.
    BsSetPosition {
        /// The match context operand.
        ctx: Operand,
        /// The saved position operand.
        pos: Operand,
    },
    /// `bs_get_integer2 Fail Ctx Live Size Unit Flags Dst` — match an integer.
    ///
    /// The operands live in a boxed [`BsGetArgs`], shared with
    /// [`Instr::BsGetFloat2`]/[`Instr::BsGetBinary2`] (identical operand shape).
    BsGetInteger2(Box<BsGetArgs>),
    /// `bs_get_float2 Fail Ctx Live Size Unit Flags Dst` — match a float.
    ///
    /// The operands live in a boxed [`BsGetArgs`] (see [`Instr::BsGetInteger2`]).
    BsGetFloat2(Box<BsGetArgs>),
    /// `bs_get_binary2 Fail Ctx Live Size Unit Flags Dst` — match a binary.
    ///
    /// The operands live in a boxed [`BsGetArgs`] (see [`Instr::BsGetInteger2`]).
    BsGetBinary2(Box<BsGetArgs>),
    /// `bs_skip_bits2 Fail Ctx Size Unit Flags` — skip bits of a match context.
    BsSkipBits2 {
        /// The failure label ([`crate::operand::NO_FAIL`] means "raise on failure").
        fail: Label,
        /// The match context operand.
        ctx: Operand,
        /// The segment size (a register or an integer).
        size: Operand,
        /// The segment unit (bits per element).
        unit: u32,
        /// The segment flag bits (little/signed/native).
        flags: u32,
    },
    /// `bs_test_tail2 Fail Ctx Bits` — succeed only if exactly `Bits` remain.
    BsTestTail2 {
        /// The failure label ([`crate::operand::NO_FAIL`] means "raise on failure").
        fail: Label,
        /// The match context operand.
        ctx: Operand,
        /// The number of remaining bits to test for.
        bits: u32,
    },
    /// `bs_get_utf8 Fail Ctx Live Flags Dst` — match a UTF-8 character.
    BsGetUtf8 {
        /// The failure label ([`crate::operand::NO_FAIL`] means "raise on failure").
        fail: Label,
        /// The match context operand.
        ctx: Operand,
        /// The number of live X registers.
        live: u32,
        /// The segment flag bits.
        flags: u32,
        /// The destination operand.
        dst: Operand,
    },
    /// `bs_get_utf16 Fail Ctx Live Flags Dst` — match a UTF-16 character.
    BsGetUtf16 {
        /// The failure label ([`crate::operand::NO_FAIL`] means "raise on failure").
        fail: Label,
        /// The match context operand.
        ctx: Operand,
        /// The number of live X registers.
        live: u32,
        /// The segment flag bits.
        flags: u32,
        /// The destination operand.
        dst: Operand,
    },
    /// `bs_get_utf32 Fail Ctx Live Flags Dst` — match a UTF-32 character.
    BsGetUtf32 {
        /// The failure label ([`crate::operand::NO_FAIL`] means "raise on failure").
        fail: Label,
        /// The match context operand.
        ctx: Operand,
        /// The number of live X registers.
        live: u32,
        /// The segment flag bits.
        flags: u32,
        /// The destination operand.
        dst: Operand,
    },
    /// `bs_skip_utf8 Fail Ctx Live Flags` — skip a UTF-8 character.
    BsSkipUtf8 {
        /// The failure label ([`crate::operand::NO_FAIL`] means "raise on failure").
        fail: Label,
        /// The match context operand.
        ctx: Operand,
        /// The number of live X registers.
        live: u32,
        /// The segment flag bits.
        flags: u32,
    },
    /// `bs_skip_utf16 Fail Ctx Live Flags` — skip a UTF-16 character.
    BsSkipUtf16 {
        /// The failure label ([`crate::operand::NO_FAIL`] means "raise on failure").
        fail: Label,
        /// The match context operand.
        ctx: Operand,
        /// The number of live X registers.
        live: u32,
        /// The segment flag bits.
        flags: u32,
    },
    /// `bs_skip_utf32 Fail Ctx Live Flags` — skip a UTF-32 character.
    BsSkipUtf32 {
        /// The failure label ([`crate::operand::NO_FAIL`] means "raise on failure").
        fail: Label,
        /// The match context operand.
        ctx: Operand,
        /// The number of live X registers.
        live: u32,
        /// The segment flag bits.
        flags: u32,
    },
    /// `bs_init_writable` — start a match context over a writable binary.
    BsInitWritable,
    /// `bs_create_bin Fail Alloc Live Unit Dst {Segments}` — build a binary.
    ///
    /// `segments` is the raw flattened segment descriptor list (type, size, unit,
    /// flags and value operands per segment); decoding the segments is the
    /// engine's job.
    BsCreateBin {
        /// The failure label ([`crate::operand::NO_FAIL`] means "raise on failure").
        fail: Label,
        /// The number of heap words needed for the result.
        alloc: u32,
        /// The number of live X registers.
        live: u32,
        /// The default segment unit.
        unit: u32,
        /// The destination operand.
        dst: Operand,
        /// The flattened segment descriptor list.
        segments: Vec<Operand>,
    },

    // ---- debug / line information ------------------------------------------
    /// `executable_line Location Index` — mark an executable line.
    ExecutableLine {
        /// The source location (an index into [`crate::ir::LineTable::items`]).
        location: LineRef,
        /// The compiler's instruction index for the line.
        index: u32,
    },
    /// `debug_line Kind Location Index Live` — mark a breakpoint location.
    DebugLine {
        /// The breakpoint kind (the atom `entry` or `line`).
        kind: Operand,
        /// The source location (an index into [`crate::ir::LineTable::items`]).
        location: LineRef,
        /// The compiler's instruction index.
        index: u32,
        /// The number of live X registers.
        live: u32,
    },
    /// `nif_start` — marks the start of a function listed in `-nifs()`.
    NifStart,
    /// `on_load` — marks the `-on_load/1` entry point.
    OnLoad,

    // ---- fallback ----------------------------------------------------------
    /// An opcode the loader deliberately does not model, preserved losslessly.
    ///
    /// The residual set is the retired/transform-only `genop.tab` entries the
    /// compiler never emits: [`Opcode::set_tuple_element`] plus the `-`-prefixed
    /// obsolete entries (`is_constant/2`, `make_fun2/1`, `allocate_zero/2`, the
    /// legacy `bs_*` forms, …). The compiler emits none of them, so the census over
    /// the installed stdlib/kernel/compiler modules is empty
    /// (`tests/generic_census.rs`) — no module lowers to `Generic` in practice.
    ///
    /// This variant is reached only from the single catch-all arm in `lower`
    /// (`src/lower/instr.rs`), which is **opcode-keyed**: every other arm either
    /// lowers to a named variant or returns a typed error on an operand-shape
    /// mismatch, so no opcode lowers to `Generic` conditionally.
    Generic {
        /// The opcode.
        op: Opcode,
        /// The decoded operands.
        operands: Vec<Operand>,
        /// The byte offset of the opcode in the `Code` stream.
        offset: u32,
    },
}

// ---- boxed operand bundles ------------------------------------------------
//
// The operand-heavy, non-hot instructions below carry their operands in a boxed
// bundle instead of inline. `Operand` is two words and a `Vec` is three, so a
// variant with three operands plus an update list would otherwise be ~80 bytes —
// large enough to set the size of the *whole* enum, which would inflate every
// instruction in the code array and widen the interpreter's dispatch match. Boxed
// variants pay one pointer chase when they execute; the hot control/register/BIF
// variants stay inline (see the `# Size budget` section in the module docs).

/// The operands of [`Instr::PutMapAssoc`]/[`Instr::PutMapExact`].
#[derive(Clone, PartialEq, Debug)]
pub struct MapWriteArgs {
    /// The failure label ([`crate::operand::NO_FAIL`] means "raise on failure").
    pub fail: Label,
    /// The source map operand.
    pub source: Operand,
    /// The destination operand.
    pub dst: Operand,
    /// The number of live X registers at the update.
    pub live: u32,
    /// The `(key, value)` pairs, in file order.
    pub pairs: Vec<(Operand, Operand)>,
}

/// The operands of [`Instr::UpdateRecord`].
#[derive(Clone, PartialEq, Debug)]
pub struct UpdateRecordArgs {
    /// The compiler's reuse hint (`copy`/`reuse`).
    pub hint: Operand,
    /// The arity of the tuple being copied.
    pub size: u32,
    /// The source tuple operand.
    pub src: Operand,
    /// The destination operand.
    pub dst: Operand,
    /// The `(index, value)` replacements, in file order.
    pub updates: Vec<(Operand, Operand)>,
}

/// The operands of [`Instr::PutRecord`].
#[derive(Clone, PartialEq, Debug)]
pub struct PutRecordArgs {
    /// The failure label.
    pub fail: Label,
    /// The record identity (atom, `{Module,Name}` literal, or register).
    pub id: Operand,
    /// `nil` to create a fresh record, or the register holding the record to update.
    pub src: Operand,
    /// The destination register.
    pub dst: Operand,
    /// The number of live x registers at the instruction.
    pub live: u32,
    /// The `(field name, value)` pairs to set, in file order.
    pub updates: Vec<(Operand, Operand)>,
}

/// The operands of [`Instr::UpdateRecordId`].
#[derive(Clone, PartialEq, Debug)]
pub struct UpdateRecordIdArgs {
    /// The failure label.
    pub fail: Label,
    /// The compiler's reuse hint (`copy`/`reuse`/`inplace`).
    pub hint: Operand,
    /// The record identity (atom, `{Module,Name}` literal, or `[]`).
    pub id: Operand,
    /// The source x/y register holding the record.
    pub src: Operand,
    /// The destination register.
    pub dst: Operand,
    /// The number of live x registers at the instruction.
    pub live: u32,
    /// The `(field name, value)` pairs to set, in file order.
    pub updates: Vec<(Operand, Operand)>,
}

/// The operands of [`Instr::GetRecordField`].
#[derive(Clone, PartialEq, Debug)]
pub struct GetRecordFieldArgs {
    /// The failure label.
    pub fail: Label,
    /// The source x/y register holding the record.
    pub src: Operand,
    /// The record identity (atom, `{Module,Name}` literal, or register).
    pub id: Operand,
    /// The field name to read.
    pub field: Operand,
    /// The destination register.
    pub dst: Operand,
}

/// The operands of [`Instr::BsGetInteger2`]/[`Instr::BsGetFloat2`]/
/// [`Instr::BsGetBinary2`] — the three share one operand shape.
#[derive(Clone, PartialEq, Debug)]
pub struct BsGetArgs {
    /// The failure label ([`crate::operand::NO_FAIL`] means "raise on failure").
    pub fail: Label,
    /// The match context operand.
    pub ctx: Operand,
    /// The number of live X registers.
    pub live: u32,
    /// The segment size (a register or an integer).
    pub size: Operand,
    /// The segment unit (bits per element).
    pub unit: u32,
    /// The segment flag bits (little/signed/native).
    pub flags: u32,
    /// The destination operand.
    pub dst: Operand,
}

// ---- size guards ----------------------------------------------------------
//
// Compile-time pins on the size budget the interpreter's dispatch depends on (see
// the module docs). They are `<=` (not `==`) so a *smaller* layout is never an
// error, but any change that re-inflates the enums — a new inline `Vec` payload or
// a fourth `Operand` field on a variant — fails the build here instead of silently
// costing cache lines. Raising a bound is a deliberate, reviewable act.
const _: () = assert!(core::mem::size_of::<Instr>() <= 64);
const _: () = assert!(core::mem::size_of::<Operand>() <= 16);
const _: () = assert!(core::mem::size_of::<HeapNeed>() <= 24);