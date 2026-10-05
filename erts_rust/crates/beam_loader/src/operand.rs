//! Typed `.beam` operands and the register/label identifiers they reference.
//!
//! One `.beam` operand is one *tag/length* byte followed by an optional payload
//! (`beam_file.c:167-240`); the full tag table, the variable-length integer
//! encoding and the `z` sub-tags are documented in the crate `CONTEXT.md`
//! ("Operand tags"). This module holds **only** the decoded types — the decoder
//! itself lives in [`crate::decode::decode_operand`] (reusing
//! [`beam_codec::Reader`], the shared bounds-checked big-endian cursor).
//!
//! `Atom` operands hold an interned [`beam_term::Atom`] (an index into the
//! crate/`beam_rt`-owned table), never the wire index; `Label` is a module-local
//! code address; `Typed` is the normalized `z,5` `tr` annotated register (the
//! type index is kept separate so the interpreter may ignore it and a JIT may
//! consume it).
use beam_term::Atom;

/// A load-time register operand.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reg {
    /// X register `n` — a slot in the process's register file (`x0`, `x1`, …).
    X(u32),
    /// Y register `n` — a slot in the current stack frame (`y0`, `y1`, …).
    Y(u32),
}

/// An index into [`crate::ir::Module::types`].
///
/// `TypeIndex(0)` is the "any type" entry. The index comes from the `Type` chunk;
/// when that chunk is absent or incompatible the loader's [`crate::ir::TypeTable`]
/// is a fallback and a `z,5` `tr` operand collapses to a plain [`Reg`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct TypeIndex(pub u32);

/// One decoded `z,3` allocation-list entry.
///
/// `kind` and `number` are the raw sub-tags from the file (heap/stack/floats/…);
/// `words` is this entry's contribution to the instruction's heap-word total.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AllocKind {
    /// The allocation kind sub-tag (`z,3` first `u` of the pair).
    pub kind: u32,
    /// The count for that kind (`z,3` second `u` of the pair).
    pub number: u32,
    /// This entry's contribution to the heap-word total.
    pub words: u32,
}

/// A module-local code address.
///
/// `Label(0)` is the failure sentinel [`NO_FAIL`] (the compiler's `f,0`), which
/// means "raise on failure" rather than "branch to label 0".
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct Label(pub u32);

/// The `f,0` failure sentinel: `0` means *raise*, not *jump to label 0*.
pub const NO_FAIL: Label = Label(0);

/// A typed `.beam` operand (the decoded form of one operand field).
#[derive(Clone, PartialEq, Debug)]
pub enum Operand {
    /// An untagged/signed integer operand (`u`/`i` tags) that fits a BEAM small int.
    Integer(i64),
    /// An atom operand (`a` tag) — the interned atom, or [`Atom::MAX`] for the
    /// `a,0` NIL placeholder.
    Atom(Atom),
    /// The atom-index-0 / NIL placeholder (`a,0`).
    Nil,
    /// An X register operand (`x` tag).
    X(u32),
    /// A Y register (stack slot) operand (`y` tag).
    Y(u32),
    /// A label operand (`f` tag); `f,0` yields [`NO_FAIL`].
    Label(Label),
    /// A character operand (`h` tag; defined but never emitted by the compiler).
    Character(u32),
    /// A float register operand (`z,2`).
    FloatReg(u32),
    /// A static-literal operand (`z,4`) — an index into the module's
    /// [`LiteralArea`](crate::literal::LiteralArea). Resolve it with
    /// [`Module::literal`](crate::ir::Module::literal).
    Literal(u32),
    /// A `z,5` `tr` typed register: the register plus its `Type` chunk index.
    Typed {
        /// The underlying `x`/`y` register.
        reg: Reg,
        /// The `Type` chunk index annotating the register.
        ty: TypeIndex,
    },
    /// An argument list (`z,1`): the sub-operands of a `gc_bif*`/`call_*`/… argument
    /// vector, in order.
    List(Vec<Operand>),
    /// An allocation list (`z,3`): the decoded `{kind, number}` entries.
    Allocation(Vec<AllocKind>),
}
