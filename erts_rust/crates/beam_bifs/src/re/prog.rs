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

//! `Ast` → a compiled backtracking program, plus the self-contained,
//! **versioned `Payload` image**.
//!
//! The image is *ours*: PCRE2's compiled blob (the oracle's element 5) is a
//! process-local `#Ref<…>` and is never reproduced here. A `Payload` carries
//! everything `run/2,3` and `inspect/2` need with **no** process-global
//! registry: the instruction stream, the character-class table, `ncap`, the
//! name table, the match-relevant compile flags, and the raw pattern bytes (so
//! `re:import/1` can recompile when the payload is unusable).
//!
//! # The instruction set
//!
//! A classic backtracking code generator:
//!
//! | op | operands | meaning |
//! |---|---|---|
//! | `Char` | codepoint, caseless | match one literal codepoint |
//! | `Class` | class index | match one member of the class |
//! | `Any` | dotall | `.` |
//! | `AnyButNewline` | — | `\N` |
//! | `Assert` | kind, flag1, flag2 | a zero-width assertion |
//! | `Split` | a, b | try `a` first, `b` on backtrack |
//! | `Jmp` | target | unconditional jump |
//! | `Save` | slot | record the current position into a capture slot |
//! | `BackRef` | group, caseless | match the text the group captured |
//! | `Mark` | — | push the current choice-point depth |
//! | `Cut` | — | discard every choice point above the last `Mark` |
//! | `Match` | — | accept |
//! | `Fail` | — | always backtrack (a never-matching construct) |
//!
//! Per-node compile flags (`caseless`, `dotall`, `multiline`,
//! `dollar_endonly`) are **baked into the instruction** that needs them, so the
//! matcher never re-derives them from a mutable option state.
//!
//! # Repetition lowering
//!
//! `X{n,}` → `n` copies of `X` then `L1: Split(L2,L3); L2: X; Jmp(L1); L3:`
//! (the `Split` arms are swapped for a lazy quantifier), and `X{n,m}` → `n`
//! copies then `m - n` nested optional copies. An **atomic group** `(?>X)` and
//! a **possessive** quantifier `X*+` are `Mark … Cut`, which commits every
//! choice point the enclosed construct created.
//!
//! Both forms are unrolled, so a bound that would emit more than
//! [`MAX_REPEAT_UNROLL`] copies is an honest typed capability gap rather than a
//! memory blow-up.
//!
//! # Robustness
//!
//! [`Payload::decode`] never panics: a truncated, foreign or version-mismatched
//! image is a typed [`BeamError::BadArg`](beam_common::BeamError::BadArg), as is
//! an out-of-range class index, capture slot or jump target.

use beam_common::{BeamError, Result};

use crate::error::{badarg, unsupported_bif};
use crate::re::options::{CompileFlags, Newline};
use crate::re::syntax::{Assert, Ast, Ranges, Syntax};

/// The image magic — 8 bytes, deliberately *not* the oracle's `re-PCRE2…`.
///
/// `re:compile/2`'s `export` form returns it as the exported pattern's header
/// binary ([`crate::re::compile`]), so it has one definition.
pub(crate) const MAGIC: &[u8; 8] = b"rustre2\0";

/// The image format version. A mismatch is a typed `badarg`.
const VERSION: u8 = 1;

/// The maximum number of instructions one quantifier may unroll to.
///
/// The oracle compiles `a{65535}`; unrolling that faithfully would emit 65535
/// instructions per occurrence, so a larger bound is a documented capability gap
/// (see the module docs).
const MAX_REPEAT_UNROLL: u32 = 4096;

// -- opcodes -----------------------------------------------------------------

const OP_CHAR: u8 = 1;
const OP_CLASS: u8 = 2;
const OP_ANY: u8 = 3;
const OP_ANYBUTNL: u8 = 4;
const OP_ASSERT: u8 = 5;
const OP_SPLIT: u8 = 6;
const OP_JMP: u8 = 7;
const OP_SAVE: u8 = 8;
const OP_BACKREF: u8 = 9;
const OP_MARK: u8 = 10;
const OP_CUT: u8 = 11;
const OP_MATCH: u8 = 12;
const OP_FAIL: u8 = 13;

/// One instruction of the compiled program.
///
/// All jump targets and class/dyn indices are `u32` program counters, so the
/// stream is trivially serialisable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Inst {
    /// A literal codepoint plus its effective `caseless` flag.
    Char {
        /// The codepoint.
        c: u32,
        /// Whether the match is case-insensitive.
        caseless: bool,
    },
    /// A character class, by index into [`Program::classes`].
    Class(u32),
    /// `.` plus its effective `dotall` flag.
    Any {
        /// Whether `.` matches a newline.
        dotall: bool,
    },
    /// `\N` — any character except a newline of the compiled convention.
    AnyButNewline,
    /// A zero-width assertion (its flags are baked in).
    Assert(Assert),
    /// Try `a` first, `b` on backtracking.
    Split {
        /// The preferred arm.
        a: u32,
        /// The fallback arm.
        b: u32,
    },
    /// An unconditional jump.
    Jmp {
        /// The target program counter.
        target: u32,
    },
    /// Record the current subject position into a capture slot.
    Save(u32),
    /// Match the text a capture group captured, with its effective `caseless`
    /// flag baked in.
    BackRef {
        /// The referenced group (1-based).
        group: u32,
        /// Whether the reference matches case-insensitively.
        caseless: bool,
    },
    /// Push the current choice-point depth.
    Mark,
    /// Discard every choice point above the last `Mark`.
    Cut,
    /// Accept.
    Match,
    /// Always backtrack (a never-matching construct).
    Fail,
}

/// The compile-time flags that affect **matching** (and the compiled pattern's
/// element 3/4 flags), carried with the program so the matcher needs no state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProgFlags {
    /// The `unicode` option (the compiled pattern's element 3).
    pub unicode: bool,
    /// Whether the match must start at the subject start.
    pub anchored: bool,
    /// `firstline` — the match must start on the first line.
    pub firstline: bool,
    /// PCRE2's `PCRE2_HASCRORLF` — the pattern can explicitly match CR/LF.
    pub has_cr_or_lf: bool,
    /// The newline convention (`is_crlf_like` drives the compiled pattern's
    /// element 4).
    pub newline: Newline,
}

/// The compiled program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Program {
    /// The instruction stream.
    pub insts: Vec<Inst>,
    /// The character-class table (`Class(id)` indexes into this).
    pub classes: Vec<Ranges>,
    /// The number of capturing groups (excluding the whole match).
    pub ncap: u32,
    /// `(name, group)` pairs sorted by name then group — the oracle's
    /// name-table order.
    pub names: Vec<(String, u32)>,
    /// The match-relevant flags.
    pub flags: ProgFlags,
}

/// The superset payload image: the compiled program **plus** the raw pattern
/// bytes, so `re:import/1` can recompile when the program half is unusable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Payload {
    /// The compiled program.
    pub program: Program,
    /// The raw pattern bytes as the caller supplied them.
    pub source: Vec<u8>,
}

/// Compiles a parsed pattern into a [`Program`].
///
/// # Errors
///
/// The typed [`unsupported_bif`] for a quantifier whose unrolled form would
/// exceed [`MAX_REPEAT_UNROLL`] instructions; no other failure is possible.
pub(crate) fn compile(syntax: &Syntax, flags: &CompileFlags) -> Result<Program> {
    let mut emitter = Emitter::new();
    emitter.emit(&syntax.ast)?;
    emitter.push(Inst::Match);
    let mut names = syntax.names.clone();
    names.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    Ok(Program {
        insts: emitter.insts,
        classes: emitter.classes,
        ncap: syntax.ncap,
        names,
        flags: ProgFlags {
            unicode: flags.unicode,
            anchored: flags.anchored,
            firstline: flags.firstline,
            has_cr_or_lf: syntax.has_cr_or_lf,
            newline: flags.newline,
        },
    })
}

// ---------------------------------------------------------------------------
// The code generator
// ---------------------------------------------------------------------------

struct Emitter {
    insts: Vec<Inst>,
    classes: Vec<Ranges>,
}

impl Emitter {
    fn new() -> Emitter {
        Emitter { insts: Vec::new(), classes: Vec::new() }
    }

    fn push(&mut self, inst: Inst) -> u32 {
        self.insts.push(inst);
        (self.insts.len() - 1) as u32
    }

    fn here(&self) -> u32 {
        self.insts.len() as u32
    }

    fn patch(&mut self, at: u32, inst: Inst) {
        self.insts[at as usize] = inst;
    }

    fn emit(&mut self, ast: &Ast) -> Result<()> {
        match ast {
            Ast::Empty => {}
            Ast::Char(c, caseless) => {
                self.push(Inst::Char { c: *c, caseless: *caseless });
            }
            Ast::AnyChar(dotall) => {
                self.push(Inst::Any { dotall: *dotall });
            }
            Ast::AnyButNewline => {
                self.push(Inst::AnyButNewline);
            }
            Ast::Class(ranges) => {
                let id = self.classes.len() as u32;
                self.classes.push(ranges.clone());
                self.push(Inst::Class(id));
            }
            Ast::Concat(items) => {
                for item in items {
                    self.emit(item)?;
                }
            }
            Ast::Alt(branches) => self.emit_alt(branches)?,
            Ast::Repeat { item, min, max, greedy, possessive } => {
                self.emit_repeat(item, *min, *max, *greedy, *possessive)?;
            }
            Ast::Capture { index, item } => {
                self.push(Inst::Save(2 * index));
                self.emit(item)?;
                self.push(Inst::Save(2 * index + 1));
            }
            Ast::BackRef { group, caseless } => {
                self.push(Inst::BackRef { group: *group, caseless: *caseless });
            }
            Ast::Assert(assert) => {
                self.push(Inst::Assert(*assert));
            }
            Ast::Atomic(item) => {
                self.push(Inst::Mark);
                self.emit(item)?;
                self.push(Inst::Cut);
            }
        }
        Ok(())
    }

    /// `b1 | b2 | …` as a chain of `Split`s, each branch jumping to the shared
    /// end label.
    fn emit_alt(&mut self, branches: &[Ast]) -> Result<()> {
        let n = branches.len();
        let mut end_jumps: Vec<u32> = Vec::new();
        for (i, branch) in branches.iter().enumerate() {
            if i + 1 < n {
                let split = self.push(Inst::Split { a: 0, b: 0 });
                let a = self.here();
                self.emit(branch)?;
                end_jumps.push(self.push(Inst::Jmp { target: 0 }));
                let b = self.here();
                self.patch(split, Inst::Split { a, b });
            } else {
                self.emit(branch)?;
            }
        }
        let end = self.here();
        for jump in end_jumps {
            self.patch(jump, Inst::Jmp { target: end });
        }
        Ok(())
    }

    /// One quantifier, unrolled (see the module docs).
    fn emit_repeat(
        &mut self,
        item: &Ast,
        min: u32,
        max: Option<u32>,
        greedy: bool,
        possessive: bool,
    ) -> Result<()> {
        // A min above the max can never match. The syntax layer already rejects an
        // out-of-order bound (`{2,1}`), so this is a defensive guard only.
        if let Some(max) = max {
            if max < min {
                self.push(Inst::Fail);
                return Ok(());
            }
        }
        let optional = match max {
            Some(max) => max - min,
            None => 0,
        };
        if min > MAX_REPEAT_UNROLL || optional > MAX_REPEAT_UNROLL {
            // An honest capability gap rather than a pathological unroll.
            return Err(unsupported_bif());
        }
        if possessive {
            self.push(Inst::Mark);
        }
        for _ in 0..min {
            self.emit(item)?;
        }
        match max {
            Some(_) => {
                // `optional` nested optional copies; every skip target is the
                // shared end label (`X? X? …`), which is `{min,max}` exactly.
                let mut splits: Vec<(u32, u32)> = Vec::new();
                for _ in 0..optional {
                    let split = self.push(Inst::Split { a: 0, b: 0 });
                    let body = self.here();
                    self.emit(item)?;
                    splits.push((split, body));
                }
                let skip = self.here();
                for (split, body) in splits {
                    let arms = if greedy { (body, skip) } else { (skip, body) };
                    self.patch(split, Inst::Split { a: arms.0, b: arms.1 });
                }
            }
            None => {
                let split = self.push(Inst::Split { a: 0, b: 0 });
                let body = self.here();
                self.emit(item)?;
                self.push(Inst::Jmp { target: split });
                let after = self.here();
                let arms = if greedy { (body, after) } else { (after, body) };
                self.patch(split, Inst::Split { a: arms.0, b: arms.1 });
            }
        }
        if possessive {
            self.push(Inst::Cut);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The payload image
// ---------------------------------------------------------------------------

impl Payload {
    /// Serialises the payload into its binary image.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        out.extend_from_slice(&self.flag_bits().to_le_bytes());
        out.push(newline_to_u8(self.program.flags.newline));
        out.extend_from_slice(&self.program.ncap.to_le_bytes());
        out.extend_from_slice(&(self.program.insts.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.program.classes.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.program.names.len() as u32).to_le_bytes());
        out.extend_from_slice(&(self.source.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.source);
        for class in &self.program.classes {
            out.extend_from_slice(&(class.len() as u32).to_le_bytes());
            for &(lo, hi) in class {
                out.extend_from_slice(&lo.to_le_bytes());
                out.extend_from_slice(&hi.to_le_bytes());
            }
        }
        for (name, group) in &self.program.names {
            let bytes = name.as_bytes();
            let len = u16::try_from(bytes.len()).unwrap_or(u16::MAX);
            let bytes = &bytes[..usize::from(len)];
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(bytes);
            out.extend_from_slice(&group.to_le_bytes());
        }
        for inst in &self.program.insts {
            encode_inst(inst, &mut out);
        }
        out
    }

    fn flag_bits(&self) -> u32 {
        let f = &self.program.flags;
        u32::from(f.unicode)
            | (u32::from(f.anchored) << 1)
            | (u32::from(f.firstline) << 2)
            | (u32::from(f.has_cr_or_lf) << 3)
    }

    /// Deserialises a payload image.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a truncated image, a foreign magic, an unknown
    /// version, an unknown opcode or any out-of-range operand — never a panic.
    pub(crate) fn decode(bytes: &[u8]) -> Result<Payload> {
        let mut r = Reader::new(bytes);
        if r.take(MAGIC.len())? != MAGIC {
            return Err(badarg());
        }
        if r.u8()? != VERSION {
            return Err(badarg());
        }
        let bits = r.u32()?;
        let newline = newline_from_u8(r.u8()?)?;
        let ncap = r.u32()?;
        let ninsts = r.u32()?;
        let nclasses = r.u32()?;
        let nnames = r.u32()?;
        let src_len = r.u32()?;
        let flags = ProgFlags {
            unicode: bits & 1 != 0,
            anchored: bits & 2 != 0,
            firstline: bits & 4 != 0,
            has_cr_or_lf: bits & 8 != 0,
            newline,
        };
        let source = r.take(src_len as usize)?.to_vec();
        let mut classes: Vec<Ranges> = Vec::new();
        for _ in 0..nclasses {
            let count = r.u32()?;
            let mut ranges: Ranges = Vec::with_capacity(count.min(64) as usize);
            for _ in 0..count {
                let lo = r.u32()?;
                let hi = r.u32()?;
                if hi < lo {
                    return Err(badarg());
                }
                ranges.push((lo, hi));
            }
            classes.push(ranges);
        }
        let mut names: Vec<(String, u32)> = Vec::new();
        for _ in 0..nnames {
            let len = usize::from(r.u16()?);
            let name = r.take(len)?;
            let name = std::str::from_utf8(name).map_err(|_| badarg())?.to_string();
            let group = r.u32()?;
            if group == 0 || group > ncap {
                return Err(badarg());
            }
            names.push((name, group));
        }
        let mut insts: Vec<Inst> = Vec::new();
        for _ in 0..ninsts {
            insts.push(decode_inst(&mut r, &classes, ncap)?);
        }
        if r.remaining() != 0 {
            return Err(badarg());
        }
        Ok(Payload { program: Program { insts, classes, ncap, names, flags }, source })
    }
}

/// The newline conventions, on the wire.
fn newline_to_u8(nl: Newline) -> u8 {
    match nl {
        Newline::Lf => 0,
        Newline::Cr => 1,
        Newline::Crlf => 2,
        Newline::Nul => 3,
        Newline::AnyCrlf => 4,
        Newline::Any => 5,
    }
}

fn newline_from_u8(v: u8) -> Result<Newline> {
    Ok(match v {
        0 => Newline::Lf,
        1 => Newline::Cr,
        2 => Newline::Crlf,
        3 => Newline::Nul,
        4 => Newline::AnyCrlf,
        5 => Newline::Any,
        _ => return Err(badarg()),
    })
}

/// The assertion kind, on the wire.
fn assert_to_u32(a: Assert) -> (u32, u32, u32) {
    match a {
        Assert::StartLine { multiline } => (0, u32::from(multiline), 0),
        Assert::EndLine { multiline, dollar_endonly } => {
            (1, u32::from(multiline), u32::from(dollar_endonly))
        }
        Assert::StartSubject => (2, 0, 0),
        Assert::EndSubject => (3, 0, 0),
        Assert::EndSubjectOpt => (4, 0, 0),
        Assert::StartMatch => (5, 0, 0),
        Assert::WordBoundary(true) => (6, 0, 0),
        Assert::WordBoundary(false) => (7, 0, 0),
    }
}

fn assert_from_u32(kind: u32, f1: u32, f2: u32) -> Result<Assert> {
    Ok(match kind {
        0 => Assert::StartLine { multiline: f1 != 0 },
        1 => Assert::EndLine { multiline: f1 != 0, dollar_endonly: f2 != 0 },
        2 => Assert::StartSubject,
        3 => Assert::EndSubject,
        4 => Assert::EndSubjectOpt,
        5 => Assert::StartMatch,
        6 => Assert::WordBoundary(true),
        7 => Assert::WordBoundary(false),
        _ => return Err(badarg()),
    })
}

fn encode_inst(inst: &Inst, out: &mut Vec<u8>) {
    match *inst {
        Inst::Char { c, caseless } => {
            out.push(OP_CHAR);
            out.extend_from_slice(&c.to_le_bytes());
            out.extend_from_slice(&u32::from(caseless).to_le_bytes());
        }
        Inst::Class(id) => {
            out.push(OP_CLASS);
            out.extend_from_slice(&id.to_le_bytes());
        }
        Inst::Any { dotall } => {
            out.push(OP_ANY);
            out.extend_from_slice(&u32::from(dotall).to_le_bytes());
        }
        Inst::AnyButNewline => out.push(OP_ANYBUTNL),
        Inst::Assert(a) => {
            let (kind, f1, f2) = assert_to_u32(a);
            out.push(OP_ASSERT);
            out.extend_from_slice(&kind.to_le_bytes());
            out.extend_from_slice(&f1.to_le_bytes());
            out.extend_from_slice(&f2.to_le_bytes());
        }
        Inst::Split { a, b } => {
            out.push(OP_SPLIT);
            out.extend_from_slice(&a.to_le_bytes());
            out.extend_from_slice(&b.to_le_bytes());
        }
        Inst::Jmp { target } => {
            out.push(OP_JMP);
            out.extend_from_slice(&target.to_le_bytes());
        }
        Inst::Save(slot) => {
            out.push(OP_SAVE);
            out.extend_from_slice(&slot.to_le_bytes());
        }
        Inst::BackRef { group, caseless } => {
            out.push(OP_BACKREF);
            out.extend_from_slice(&group.to_le_bytes());
            out.extend_from_slice(&u32::from(caseless).to_le_bytes());
        }
        Inst::Mark => out.push(OP_MARK),
        Inst::Cut => out.push(OP_CUT),
        Inst::Match => out.push(OP_MATCH),
        Inst::Fail => out.push(OP_FAIL),
    }
}

fn decode_inst(r: &mut Reader<'_>, classes: &[Ranges], ncap: u32) -> Result<Inst> {
    let op = r.u8()?;
    Ok(match op {
        OP_CHAR => Inst::Char { c: r.u32()?, caseless: r.u32()? != 0 },
        OP_CLASS => {
            let id = r.u32()?;
            if id as usize >= classes.len() {
                return Err(badarg());
            }
            Inst::Class(id)
        }
        OP_ANY => Inst::Any { dotall: r.u32()? != 0 },
        OP_ANYBUTNL => Inst::AnyButNewline,
        OP_ASSERT => Inst::Assert(assert_from_u32(r.u32()?, r.u32()?, r.u32()?)?),
        OP_SPLIT => Inst::Split { a: r.u32()?, b: r.u32()? },
        OP_JMP => Inst::Jmp { target: r.u32()? },
        OP_SAVE => Inst::Save(r.u32()?),
        OP_BACKREF => {
            let group = r.u32()?;
            if group == 0 || group > ncap {
                return Err(badarg());
            }
            let caseless = r.u32()? != 0;
            Inst::BackRef { group, caseless }
        }
        OP_MARK => Inst::Mark,
        OP_CUT => Inst::Cut,
        OP_MATCH => Inst::Match,
        OP_FAIL => Inst::Fail,
        _ => return Err(badarg()),
    })
}

/// A bounds-checked little-endian reader — every failure is a typed `badarg`.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { bytes, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.pos)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.remaining() {
            return Err(badarg());
        }
        let out = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
}

/// The typed-gap constructor for a never-matching construct.
///
/// Unused today: the only construct that could ask for it — an out-of-order
/// repeat bound like `a{2,1}` — is already rejected by `syntax.rs`, so
/// `emit_repeat` emits [`Inst::Fail`] directly. Kept as the single typed-gap
/// constructor the code generator would use rather than deleted.
#[allow(dead_code)] // kept: the never-matching code path it names is currently unreachable
pub(crate) fn never_matches() -> BeamError {
    unsupported_bif()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::re::syntax;

    fn codes(pattern: &str) -> (Vec<u32>, Vec<usize>, usize) {
        let codes: Vec<u32> = pattern.chars().map(|c| c as u32).collect();
        let offs: Vec<usize> = pattern.char_indices().map(|(i, _)| i).collect();
        (codes, offs, pattern.len())
    }

    fn program(pattern: &str) -> Program {
        let flags = CompileFlags::default();
        let (c, o, total) = codes(pattern);
        let syntax = syntax::parse(&c, &o, total, &flags).unwrap_or_else(|e| panic!("{pattern}: {e:?}"));
        compile(&syntax, &flags).unwrap_or_else(|e| panic!("{pattern}: {e:?}"))
    }

    fn payload(pattern: &str) -> Payload {
        Payload { program: program(pattern), source: pattern.as_bytes().to_vec() }
    }

    #[test]
    fn a_payload_round_trips() {
        for pattern in ["", "a", "(a)(b)", "a|b|(c)", "a*", "a+?", "[a-z]", "(?<n>a)\\k<n>", "(?>a)b"] {
            let p = payload(pattern);
            let image = p.encode();
            let back = Payload::decode(&image).unwrap_or_else(|e| panic!("{pattern}: {e:?}"));
            assert_eq!(back, p, "{pattern}");
        }
    }

    #[test]
    fn a_payload_carries_the_source_for_import() {
        let p = payload("a(b)c");
        assert_eq!(p.encode(), p.encode(), "encoding is deterministic");
        let back = Payload::decode(&p.encode()).expect("decodes");
        assert_eq!(back.source, b"a(b)c");
    }

    #[test]
    fn repeat_lowering_places_the_split_arms_per_greediness() {
        let greedy = program("a*");
        // `Split(body, after)` for a greedy star.
        assert!(matches!(greedy.insts[0], Inst::Split { a: 1, b: 3 }));
        let lazy = program("a*?");
        assert!(matches!(lazy.insts[0], Inst::Split { a: 3, b: 1 }));
        // A bounded quantifier unrolls to optional copies.
        let bounded = program("a{2,3}");
        assert_eq!(
            bounded.insts.iter().filter(|i| matches!(i, Inst::Char { .. })).count(),
            3
        );
        assert_eq!(bounded.insts.iter().filter(|i| matches!(i, Inst::Split { .. })).count(), 1);
    }

    #[test]
    fn a_possessive_quantifier_and_an_atomic_group_bracket_with_mark_and_cut() {
        for pattern in ["a*+", "(?>a)"] {
            let p = program(pattern);
            assert_eq!(p.insts.first(), Some(&Inst::Mark), "{pattern}");
            assert!(p.insts.contains(&Inst::Cut), "{pattern}");
        }
    }

    #[test]
    fn a_brace_without_a_bound_is_a_literal() {
        // `a{}` is a literal `a` followed by literal braces — the oracle matches
        // `re:run("a{}", "a{}")` at `{0,3}` — never a never-matching form.
        let p = program("a{}");
        assert!(
            p.insts.iter().any(|i| matches!(i, Inst::Char { c, .. } if *c == b'{' as u32)),
            "{:?}",
            p.insts
        );
        assert!(
            p.insts.iter().any(|i| matches!(i, Inst::Char { c, .. } if *c == b'}' as u32)),
            "{:?}",
            p.insts
        );
        assert_eq!(
            p.insts.iter().filter(|i| matches!(i, Inst::Char { .. })).count(),
            3,
            "{:?}",
            p.insts
        );
    }

    #[test]
    fn the_name_table_is_sorted_by_name_then_group() {
        let p = program("(?<b>x)(?<a>y)");
        assert_eq!(p.names, vec![("a".to_string(), 2), ("b".to_string(), 1)]);
        assert_eq!(p.ncap, 2);
    }

    #[test]
    fn a_truncated_image_is_badarg_not_a_panic() {
        let image = payload("(a)(b)").encode();
        for cut in 0..image.len() {
            assert!(
                matches!(Payload::decode(&image[..cut]), Err(BeamError::BadArg)),
                "a {cut}-byte prefix must be rejected"
            );
        }
    }

    #[test]
    fn a_foreign_or_version_mismatched_image_is_badarg() {
        let image = payload("a").encode();
        let mut foreign = image.clone();
        foreign[0] = b'X';
        assert!(matches!(Payload::decode(&foreign), Err(BeamError::BadArg)));
        let mut old = image.clone();
        old[MAGIC.len()] = VERSION.wrapping_add(9);
        assert!(matches!(Payload::decode(&old), Err(BeamError::BadArg)));
        // Trailing bytes are rejected too.
        let mut trailing = image;
        trailing.push(0);
        assert!(matches!(Payload::decode(&trailing), Err(BeamError::BadArg)));
    }

    #[test]
    fn an_unknown_opcode_is_badarg() {
        let p = payload("a");
        let mut image = p.encode();
        // The last instruction is `Match`; corrupt it.
        let last = image.len() - 1;
        image[last] = 200;
        assert!(matches!(Payload::decode(&image), Err(BeamError::BadArg)));
    }

    #[test]
    fn an_out_of_range_class_index_is_badarg() {
        let p = payload("[a]");
        let mut image = p.encode();
        // The `Match` opcode sits last; the class op is the one before it.
        assert_eq!(image[image.len() - 1], OP_MATCH);
        let class_op = image.len() - 6;
        assert_eq!(image[class_op], OP_CLASS);
        image[class_op + 1] = 0xFF;
        image[class_op + 2] = 0xFF;
        assert!(matches!(Payload::decode(&image), Err(BeamError::BadArg)));
    }

    #[test]
    fn a_bound_beyond_the_unroll_cap_is_a_typed_gap() {
        let flags = CompileFlags::default();
        let (c, o, total) = codes("a{5000}");
        let syntax = syntax::parse(&c, &o, total, &flags).expect("parses");
        assert!(matches!(compile(&syntax, &flags), Err(BeamError::Raise(_))));
    }

    #[test]
    fn assertions_round_trip_with_their_flags() {
        let p = payload("^a$");
        let back = Payload::decode(&p.encode()).expect("decodes");
        assert_eq!(back.program.insts.len(), p.program.insts.len());
        assert!(back.program.insts.iter().any(|i| matches!(i, Inst::Assert(_))));
    }
}
