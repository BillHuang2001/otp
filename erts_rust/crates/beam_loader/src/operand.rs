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
use beam_common::{BeamError, Result};
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

/// The decode-time payload of [`Operand::BigInteger`].
///
/// A wide `i`-tagged integer field (see [`Operand::BigInteger`]) is the one decoded
/// operand that carries a variable-length byte payload. It is boxed — and this is
/// the *only* reason it is a separate type — so that `Operand` stays a two-word
/// value: the payload never reaches a loaded [`Module`](crate::ir::Module)
/// ([`crate::lower`] rewrites it to a synthesized [`Operand::Literal`]), so keeping
/// it inline would inflate every operand of every instruction on the hot path.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BigInt {
    /// Whether the packed field is negative (the top bit of its most significant
    /// byte, `beam_file.c:2174`).
    pub negative: bool,
    /// The **big-endian** magnitude, normalized exactly like the C (at most one
    /// leading zero byte is removed; two would be a corrupt field,
    /// `beam_file.c:2186-2203`). Never empty.
    pub magnitude: Vec<u8>,
}

/// A typed `.beam` operand (the decoded form of one operand field).
///
/// # Layout
///
/// `Operand` sits inside every [`Instr`](crate::ir::Instr) operand field and is
/// therefore size-sensitive: the interpreter's per-instruction `match` and the code
/// array's density both scale with it. The three *decode-time-only* variants
/// ([`Operand::BigInteger`], [`Operand::List`], [`Operand::Allocation`]) are the only
/// ones whose payload holds a `Vec`, and each is boxed so the enum is two words
/// instead of four. **None of the three survives lowering** — a loaded
/// [`Module`](crate::ir::Module)'s operands are only the leaf forms — so their
/// boxed payload is a load-time concern, never a runtime indirection.
///
/// The boxed variants have no public tuple/struct payload, so they are built through
/// [`Operand::big_integer`], [`Operand::list`] and [`Operand::alloc`]; match them as
/// `Operand::List(items)` with `items: &Box<Vec<Operand>>` (use `.as_slice()`).
///
/// The payloads are `Box<Vec<_>>` rather than `Box<[_]>` deliberately: a boxed
/// *slice* is a fat pointer (two words), which would put the largest variant payload
/// at 16 bytes and re-inflate the enum to three words.
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
    /// A wide signed-integer operand (the `i` tag) whose value does not fit a BEAM
    /// small integer.
    ///
    /// The C reader (`beamreader_read_tagged`, `beam_file.c:209-236`) inlines an `i`
    /// value only while the sign-extended magnitude still satisfies `IS_SSMALL`;
    /// otherwise the value is kept as its raw big-endian field, and
    /// `marshal_integer` (`beam_file.c:2134-2232`) turns it into a bignum **static
    /// literal** (`beamfile_add_literal`) that the instruction references by index.
    ///
    /// This variant is the loader's *decode-time* form of that field: [`crate::lower`]
    /// performs the same conversion, appending the bignum term to the module's
    /// literal area and rewriting the operand to [`Operand::Literal`]`(idx)`. A
    /// loaded [`Module`](crate::ir::Module) therefore never carries this variant —
    /// the engine resolves the synthesized literal through
    /// [`Module::literal`](crate::ir::Module::literal) like any other literal, so no
    /// engine change is needed. Build it with [`Operand::big_integer`].
    BigInteger(Box<BigInt>),
    /// An argument list (`z,1`): the sub-operands of a `gc_bif*`/`call_*`/… argument
    /// vector, in order. Build it with [`Operand::list`].
    List(Box<Vec<Operand>>),
    /// An allocation list (`z,3`): the decoded `{kind, number}` entries. Build it
    /// with [`Operand::alloc`].
    Allocation(Box<Vec<AllocKind>>),
}

impl Operand {
    /// Builds the decode-time wide-integer operand from its sign and magnitude.
    ///
    /// The arguments are exactly [`BigInt`]'s fields; the payload is boxed so
    /// `Operand` stays two words (see the type's "Layout" section).
    pub fn big_integer(negative: bool, magnitude: Vec<u8>) -> Operand {
        Operand::BigInteger(Box::new(BigInt {
            negative,
            magnitude,
        }))
    }

    /// Builds a `z,1` argument-list operand from its sub-operands.
    pub fn list(items: Vec<Operand>) -> Operand {
        Operand::List(Box::new(items))
    }

    /// Builds a `z,3` allocation-list operand from its decoded entries.
    pub fn alloc(entries: Vec<AllocKind>) -> Operand {
        Operand::Allocation(Box::new(entries))
    }
}

/// The heap/stack-need operand of `test_heap`/`allocate`/`allocate_heap`.
///
/// The compiler writes a frame/test need *either* as a bare word count (a `u`
/// integer operand, e.g. `{test_heap,5,4}`) *or*, when the instruction also
/// allocates floats or funs, as a `z,3` allocation list (e.g.
/// `{test_heap,{alloc,[{words,1},{floats,0},{funs,1}]},1}`). Both encodings denote
/// a heap/stack-word total; `erts/emulator/beam/emu/ops.tab` declares the
/// allocation-list capability for `allocate Ns Live`, `allocate_heap Ns Nh Live`
/// and `test_heap Nh Live`.
///
/// The IR keeps both forms (see the `test_heap`/`allocate*` variants of
/// [`Instr`](crate::ir::Instr)): [`HeapNeed::words`] recovers the total the engine
/// needs for its heap-overflow check, while [`HeapNeed::Alloc`] preserves the
/// per-kind breakdown (a JIT or a precise allocator may want the floats/funs
/// split). Decode an operand with [`HeapNeed::from_operand`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HeapNeed {
    /// A plain heap/stack-word count (the `u` integer form).
    Words(u32),
    /// A decoded `z,3` allocation list.
    ///
    /// The word total is the sum of the entries' [`AllocKind::words`].
    Alloc(Vec<AllocKind>),
}

impl HeapNeed {
    /// The total number of heap/stack words the operand reserves.
    ///
    /// The sum saturates: each entry's word count is already bounded by the
    /// decoder, so an overflow is unreachable for a well-formed image; saturation
    /// keeps this infallible rather than panicking on a crafted one.
    pub fn words(&self) -> u32 {
        match self {
            HeapNeed::Words(n) => *n,
            HeapNeed::Alloc(entries) => entries
                .iter()
                .fold(0u32, |total, entry| total.saturating_add(entry.words)),
        }
    }

    /// Decodes a heap/stack-need operand in either encoding.
    ///
    /// `Operand::Integer(n)` (`n >= 0`) ⇒ [`HeapNeed::Words`];
    /// `Operand::Allocation(entries)` ⇒ [`HeapNeed::Alloc`]. Any other operand is a
    /// structural mismatch (a non-integer where a `u` operand is required, or an
    /// operand list where an allocation list is required) and yields
    /// [`BeamError::BadArg`].
    pub fn from_operand(operand: &Operand) -> Result<HeapNeed> {
        match operand {
            Operand::Integer(value) if *value >= 0 && *value <= i64::from(u32::MAX) => {
                Ok(HeapNeed::Words(*value as u32))
            }
            Operand::Allocation(entries) => Ok(HeapNeed::Alloc(entries.to_vec())),
            _ => Err(BeamError::BadArg),
        }
    }
}
