//! The frozen typed IR: what `beam_engine` consumes.
//!
//! This is the **frozen public-API shape** the execution-engine backends design
//! against (`../../CONTEXT.md` §4, `../CONTEXT.md` "The seam types"). It is a
//! typed, block-structured representation of a loaded module: a flat instruction
//! vector per function, the label-delimited basic blocks over it, the export/import
//! tables, the compile-time type annotations, and the line/range table.
//!
//! Everything here is data. The loader ([`crate::load`], [`crate::lower`],
//! [`crate::decode`]) *produces* it; the engine and the JIT tiers *consume* it.
//! `Instr` is a named enum for the Milestone-1 opcode subset plus a lossless
//! [`Instr::Generic`] fallback, so an instruction the M1 interpreter does not yet
//! execute still round-trips with its opcode and operands intact.
use std::collections::BTreeMap;

use crate::opcode::Opcode;
use crate::operand::{Label, Operand};

/// A module-local atom: an index into the crate-owned [`beam_term::AtomTable`].
///
/// This is *not* the `AtU8` wire index; [`Module::atoms`] maps a wire index to the
/// interned `Atom`.
pub type Atom = beam_term::Atom;

/// An index into [`Module::line`]'s [`LineTable::items`].
///
/// `items[0]` is always the undefined location `{0, 0}`.
pub type LineRef = u32;

/// An index into [`Module::imports`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct ImportRef(pub u32);

/// One entry of the `ExpT` chunk: an exported `M:F/A` and its entry label.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Export {
    /// The exported function name.
    pub function: Atom,
    /// The exported arity.
    pub arity: u32,
    /// The function **entry** label (`Export.label == Function.entry_label`).
    pub label: Label,
}

/// One entry of the `ImpT` chunk: an imported `M:F/A`.
///
/// `Module.imports` is the single source of BIF/import identity — the loader does
/// **not** encode `bif.tab` numbering (`../CONTEXT.md`, Design Decisions).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Import {
    /// The imported module.
    pub module: Atom,
    /// The imported function.
    pub function: Atom,
    /// The imported arity.
    pub arity: u32,
}

/// The decoded `Code` chunk header.
///
/// `format_number` must be `0` (the current `BEAM_FORMAT_NUMBER`) or the image is
/// rejected.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CodeVersion {
    /// The `Code` chunk format number (must be `0`).
    pub format_number: u32,
    /// The highest opcode number present (the file's opcode-space bound).
    pub max_opcode: u32,
    /// The number of labels in the code.
    pub label_count: u32,
    /// The number of functions in the code.
    pub function_count: u32,
}

/// A label-delimited basic block: `code[start..end]` beginning at `label`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Block {
    /// The label this block starts at.
    pub label: Label,
    /// Inclusive start index into [`Function::code`].
    pub start: usize,
    /// Exclusive end index into [`Function::code`].
    pub end: usize,
}

/// One function of the module.
///
/// The file's canonical sequence is
/// `label FuncLabel | [line] | func_info M F A | label EntryLabel | body…`
/// (`beam_file.c:2502-2589`), so both labels are retained: `func_label` is the
/// label *before* `func_info`, `entry_label` the label *after* it (what
/// [`Export::label`] points to).
#[derive(Clone, PartialEq, Debug)]
pub struct Function {
    /// The label immediately before `func_info`.
    pub func_label: Label,
    /// The label immediately after `func_info` (the function entry point).
    pub entry_label: Label,
    /// The module the function belongs to.
    pub module: Atom,
    /// The function name.
    pub name: Atom,
    /// The function arity.
    pub arity: u32,
    /// The function body (label-delimited within the module-wide stream).
    pub code: Vec<Instr>,
    /// The label-delimited basic-block decomposition of [`Function::code`].
    pub blocks: Vec<Block>,
    /// Every label in the function mapped to its index in [`Function::code`].
    pub labels: BTreeMap<Label, usize>,
}

/// One `Type` chunk entry: a compile-time type annotation for a register.
///
/// `lower`/`upper`/`unit` are present only when the corresponding `raw` bits are
/// set (see the `Type` chunk layout in the crate `CONTEXT.md`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Type {
    /// The raw type word from the chunk.
    pub raw: u16,
    /// The optional lower bound (`raw` bits 13-15).
    pub lower: Option<i64>,
    /// The optional upper bound (`raw` bits 13-15).
    pub upper: Option<i64>,
    /// The optional unit size minus one (`raw` bits 13-15).
    pub unit: Option<u8>,
}

/// The `Type` chunk decoded into the IR, or a fallback.
///
/// When the `Type` chunk is absent or its version is incompatible, the table is a
/// single "any type" entry with `fallback == true`, and `z,5` `tr` operands
/// collapse to plain registers.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TypeTable {
    /// The decoded entries; `entries[0]` is the "any type" entry.
    pub entries: Vec<Type>,
    /// Whether this is the synthesized fallback (no usable `Type` chunk).
    pub fallback: bool,
}

impl Default for TypeTable {
    /// The fallback table: a single "any type" entry.
    fn default() -> TypeTable {
        TypeTable {
            entries: vec![Type::default()],
            fallback: true,
        }
    }
}

/// One `Line` chunk item: the source location of one instruction.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct LineEntry {
    /// Index into [`LineTable::files`].
    pub file: u32,
    /// The 1-based source line.
    pub line: u32,
}

/// The `Line` chunk decoded into the IR.
///
/// `items[0]` is the undefined location `{0, 0}`; `items[i]` gives the location of
/// the `i`-th lowered instruction (`Instr::Line(LineRef)` carries the index).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LineTable {
    /// The raw `Line` chunk flags.
    pub flags: u32,
    /// The number of instructions the `Line` chunk describes.
    pub instruction_count: u32,
    /// The locations; `items[0]` is always the undefined location.
    pub items: Vec<LineEntry>,
    /// The file names; `files[0]` is the synthesized `<module>.erl`.
    pub files: Vec<String>,
}

impl Default for LineTable {
    /// An empty line table (undefined location only, no file names).
    fn default() -> LineTable {
        LineTable {
            flags: 0,
            instruction_count: 0,
            items: Vec::new(),
            files: Vec::new(),
        }
    }
}

/// The `is_*` family of test instructions.
///
/// The opcode → kind mapping is the loader's job (the opcode roster carries the
/// op name; the kind is the semantic classification the engine matches on).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TestKind {
    /// `is_lt/3`.
    IsLt,
    /// `is_ge/3`.
    IsGe,
    /// `is_eq/3`.
    IsEq,
    /// `is_ne/3`.
    IsNe,
    /// `is_eq_exact/3`.
    IsEqExact,
    /// `is_ne_exact/3`.
    IsNeExact,
    /// `is_integer/2`.
    IsInteger,
    /// `is_float/2`.
    IsFloat,
    /// `is_number/2`.
    IsNumber,
    /// `is_atom/2`.
    IsAtom,
    /// `is_pid/2`.
    IsPid,
    /// `is_reference/2`.
    IsReference,
    /// `is_port/2`.
    IsPort,
    /// `is_nil/2`.
    IsNil,
    /// `is_binary/2`.
    IsBinary,
    /// `is_bitstr/2`.
    IsBitstr,
    /// `is_list/2`.
    IsList,
    /// `is_nonempty_list/2`.
    IsNonemptyList,
    /// `is_tuple/2`.
    IsTuple,
    /// `is_map/2`.
    IsMap,
    /// `test_arity/3`.
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
            Opcode::test_arity => TestKind::TestArity,
            // Every other opcode (including the `is_*`-named but non-test
            // `is_constant`/`is_boolean`/`is_function*`/`is_*_record` family) is not
            // a test instruction the engine matches on.
            _ => return None,
        })
    }
}

/// A lowered BEAM instruction.
///
/// Named variants cover the Milestone-1 interpreter subset; every other opcode is
/// preserved losslessly as [`Instr::Generic`], which the engine/JIT can extend
/// without a format change.
#[derive(Clone, PartialEq, Debug)]
pub enum Instr {
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
    /// `move Src Dst`.
    Move {
        /// The source operand.
        src: Operand,
        /// The destination operand.
        dst: Operand,
    },
    /// `gc_bif*` — a garbage-collecting BIF call.
    GcBif {
        /// The number of arguments (the BIF's arity).
        arity: u8,
        /// The imported `{module,function,arity}` to call.
        bif: ImportRef,
        /// The failure label ([`NO_FAIL`] means "raise on failure").
        fail: Label,
        /// The number of live X registers at the call.
        live: u32,
        /// The argument operands.
        args: Vec<Operand>,
        /// The destination operand.
        dst: Operand,
    },
    /// An `is_*`/`test_arity` test instruction.
    Test {
        /// The classified test.
        kind: TestKind,
        /// The failure label ([`NO_FAIL`] means "raise on failure").
        fail: Label,
        /// The tested operand(s).
        args: Vec<Operand>,
    },
    /// `select_val Value Fail {Value,Label}*` — a switch on an exact value.
    SelectVal {
        /// The scrutinee operand.
        value: Operand,
        /// The failure label ([`NO_FAIL`] means "raise on failure").
        fail: Label,
        /// The `(value, label)` alternatives.
        pairs: Vec<(Operand, Label)>,
    },
    /// `swap A B` — exchange two registers.
    Swap {
        /// One register.
        a: Operand,
        /// The other register.
        b: Operand,
    },
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
    /// `allocate StackNeed Live` — allocate a stack frame.
    Allocate {
        /// The number of Y registers (stack slots) needed.
        stack_need: u32,
        /// The number of live X registers.
        live: u32,
    },
    /// `allocate_heap StackNeed HeapNeed Live` — allocate frame + heap.
    AllocateHeap {
        /// The number of Y registers (stack slots) needed.
        stack_need: u32,
        /// The number of heap words needed.
        heap_need: u32,
        /// The number of live X registers.
        live: u32,
    },
    /// `test_heap HeapNeed Live` — ensure heap headroom.
    TestHeap {
        /// The number of heap words needed.
        heap_need: u32,
        /// The number of live X registers.
        live: u32,
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
    /// Any opcode outside the Milestone-1 named subset, preserved losslessly.
    Generic {
        /// The opcode.
        op: Opcode,
        /// The decoded operands.
        operands: Vec<Operand>,
        /// The byte offset of the opcode in the `Code` stream.
        offset: u32,
    },
}

/// A fully loaded and lowered module — the loader's output and the engine's input.
#[derive(Clone, Debug)]
pub struct Module {
    /// The module name (the atom at `AtU8` wire index 1).
    pub name: Atom,
    /// The wire-index → atom map; `atoms[0]` is [`Atom::MAX`] (the NIL placeholder).
    pub atoms: Vec<Atom>,
    /// The owning atom table (interning context for this module's atoms).
    pub atom_table: beam_term::AtomTable,
    /// The `ExpT` exports.
    pub exports: Vec<Export>,
    /// The `ImpT` imports (the single source of BIF identity).
    pub imports: Vec<Import>,
    /// The module's functions.
    pub functions: Vec<Function>,
    /// The `Code` chunk header.
    pub code_version: CodeVersion,
    /// The decoded `Type` chunk (or its fallback).
    pub types: TypeTable,
    /// The decoded `Line` chunk.
    pub line: LineTable,
    /// The `LitT` literal payloads (raw external-term bytes, not decoded).
    pub literals: Vec<Vec<u8>>,
    /// The `StrT` string table bytes.
    pub strings: Vec<u8>,
    /// The raw `Attr` chunk bytes, if present.
    pub attributes: Option<Vec<u8>>,
    /// The raw `CInf` chunk bytes, if present.
    pub compile_info: Option<Vec<u8>>,
}

impl Default for Module {
    /// An empty module: no name, no atoms, no functions.
    fn default() -> Module {
        Module {
            name: Atom::MAX,
            atoms: Vec::new(),
            atom_table: beam_term::AtomTable::new(),
            exports: Vec::new(),
            imports: Vec::new(),
            functions: Vec::new(),
            code_version: CodeVersion::default(),
            types: TypeTable::default(),
            line: LineTable::default(),
            literals: Vec::new(),
            strings: Vec::new(),
            attributes: None,
            compile_info: None,
        }
    }
}

impl Module {
    /// The name of an interned atom, if it is in this module's atom table.
    pub fn atom_name(&self, atom: Atom) -> Option<&str> {
        self.atom_table.name(atom)
    }

    /// The entry label of the exported `function/arity`, if the module exports it.
    pub fn export(&self, function: Atom, arity: u32) -> Option<Label> {
        self.exports
            .iter()
            .find(|export| export.function == function && export.arity == arity)
            .map(|export| export.label)
    }

    /// The function `function/arity`, if the module defines it.
    pub fn function(&self, function: Atom, arity: u32) -> Option<&Function> {
        self.functions
            .iter()
            .find(|f| f.name == function && f.arity == arity)
    }
}