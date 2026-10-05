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
//!
//! The instruction vocabulary itself — [`Instr`] and [`TestKind`] — lives in
//! [`crate::instr`] and is re-exported here, so `crate::ir::Instr` is this module's
//! canonical path for the engine while the enum itself stays in one flat module.
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::literal::LiteralArea;
use crate::operand::Label;

pub use crate::instr::*;

/// One machine word of the frozen term representation
/// ([`beam_term::Eterm`](beam_term::prelude::Eterm)).
///
/// Re-exported here because it is the type of a resolved static literal
/// ([`Module::literal`]).
pub use beam_term::Eterm;

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

/// One `FunT` chunk entry: the descriptor of a lambda (`fun … end` / a local fun).
///
/// The descriptor is what `make_fun3` (and, with a statically known target,
/// `call_fun2`) refers to: the instruction carries the **fun-table index**, which
/// resolves through [`Module::fun`] — the index of the entry in
/// [`Module::funs`], i.e. its position in the file's `FunT` chunk
/// (`beam_asm.erl`'s `beam_dict:lambda/3` hands out exactly that number).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FunDescriptor {
    /// The name of the lambda's anonymous function.
    pub function: Atom,
    /// The lambda's arity (the number of *non-captured* arguments).
    pub arity: u32,
    /// The code label of the lambda body.
    pub label: Label,
    /// The index within the enclosing function's fun list.
    pub index: u32,
    /// The number of free variables the lambda captures.
    pub num_free: u32,
    /// The unique identifier of the lambda (the file's `old_uniq` field).
    pub uniq: u32,
}

/// A fully loaded and lowered module — the loader's output and the engine's input.
///
/// A `Module` is `Clone` (cheap: the literal area is shared behind an [`Arc`]) and
/// `Debug`, but it is **neither `Send` nor `Sync`**: [`LiteralArea`] owns a
/// [`beam_alloc::Heap`] holding raw term pointers, so a loaded module is confined
/// to the thread that loaded it (see [`crate::literal`]).
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
    /// The `FunT` lambda descriptors; `make_fun3`'s index addresses this vector.
    pub funs: Vec<FunDescriptor>,
    /// The `Code` chunk header.
    pub code_version: CodeVersion,
    /// The decoded `Type` chunk (or its fallback).
    pub types: TypeTable,
    /// The decoded `Line` chunk.
    pub line: LineTable,
    /// The `LitT` literals, decoded into terms once at load time.
    pub literals: Arc<LiteralArea>,
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
            funs: Vec::new(),
            code_version: CodeVersion::default(),
            types: TypeTable::default(),
            line: LineTable::default(),
            literals: Arc::new(LiteralArea::default()),
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

    /// The static literal at `index` (an `Operand::Literal(index)` reference).
    ///
    /// This is how the interpreter resolves the `z,4` literal operand of e.g.
    /// `move`/`put_literal`. Returns `None` for an index outside the `LitT` table.
    pub fn literal(&self, index: u32) -> Option<Eterm> {
        self.literals.get(index)
    }

    /// The module's decoded static-literal area.
    pub fn literals(&self) -> &LiteralArea {
        &self.literals
    }

    /// The lambda descriptor at `index` (a `make_fun3`/`call_fun2` fun-table index).
    ///
    /// Returns `None` for an index outside the `FunT` table.
    pub fn fun(&self, index: u32) -> Option<&FunDescriptor> {
        self.funs.get(index as usize)
    }

    /// The module's lambda descriptors, in `FunT` (fun-table index) order.
    pub fn funs(&self) -> &[FunDescriptor] {
        &self.funs
    }
}