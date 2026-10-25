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

//! The per-module **prepared bitstring program** cache.
//!
//! `bs_create_bin` and `bs_match` both carry their operands as a **flat, untyped
//! descriptor list** (`[Type, Index, Unit, Flags, Value, Size]` groups and command
//! words respectively). Decoding that list is not free: every element re-resolves each
//! descriptor's type atom through [`Module::atom_name`] and a `&str` match, re-derives
//! the flags, and re-reads the size/unit operands — and a compiled binary-comprehension
//! loop re-enters the very same instruction once per element. Measured on
//! `tools/bench`'s `binary_append` (16 384 iterations of one `bs_create_bin` with five
//! segments), that walk is the largest bitstring-local cost.
//!
//! So the descriptors are **pre-resolved once per `(module image, function, code
//! offset)`** into a typed program ([`CreateProgram`](super::create::prepared::CreateProgram),
//! [`MatchProgram`](super::matching::prepared::MatchProgram)) and executed thereafter.
//! The decode of a segment becomes a tag, the flags a `u32`, the width a plain `usize`,
//! and the atom-name comparison disappears from the execution path entirely.
//!
//! # The prepared program is a pure function of the loaded module
//!
//! A program is derived from the instruction's own descriptor list plus the module's
//! atom table and literal/string tables — never from a run, a machine or a host hook —
//! so caching it cannot go stale while the image lives. [`prepare`](Program) never
//! invents a value: a shape it cannot pre-resolve (an unknown segment/command type, a
//! truncated list, a *dynamic* unit, a malformed `string` range, a segment width whose
//! `$BS_SAFE_MUL` overflows) is filed as [`Program::Generic`], and the family then runs
//! the **unchanged** generic walk, which reproduces the exact typed error (or the exact
//! dynamic behaviour) it produced before this cache existed. There is no fabricated
//! program and no substituted result.
//!
//! # The key is an identity only for an image that has a content identity
//!
//! `(function, code offset)` names one instruction **of a loaded `.beam`** — one
//! instruction occupies one code slot — so two executions of one key are two executions
//! of one instruction. A *synthetic* image (`Module::default()`, the hand-built shape a
//! test or an engine benchmark uses) has no such property: it freely re-uses `(0, 0)`
//! for many different descriptor lists, and its all-zero `Module::checksum` is the
//! documented marker of exactly that lack of content identity (`machine/label_cache.rs`).
//! So `bs_match` caches a program only for an image whose checksum is not all-zero
//! (`matching::prepared`); a synthetic image keeps the generic walk, where a hit on
//! another instruction's program — a wrong answer, not merely a slow one — cannot arise.
//!
//! # Where it lives, and why
//!
//! One [`BitstrPrograms`] per **module context**, held by the machine's context
//! registry (`machine::Contexts`) in a table **parallel** to the contexts themselves,
//! plus the machine-owned one for context `0` (the run's entry module, which the
//! machine never owns) — exactly the placement and lifetime of the local-call verdict
//! table (`machine/local_callee.rs`), and for the same reason: a module context *is*
//! the module-image identity, deduplicated by `Arc::ptr_eq`, so a `Module::clone` or a
//! separate load of the same `.beam` is a different context and therefore a different
//! table.
//!
//! The table is **not** inside `ModuleContext`: it is handed to the executing family by
//! value for the duration of one instruction, and a `ModuleContext` is reached through
//! a shared `Arc` — so keeping it there costs an `Arc` clone/drop (two atomics) and a
//! `RefCell` borrow guard *per bitstring instruction*. In a parallel `Vec` owned by the
//! registry the hand-over is two plain struct moves, reached through `&mut self` with no
//! reference count touched ([`Machine::with_bitstr_programs`]).
//!
//! [`BitstrPrograms::owner`] additionally records the image address the table was filled
//! for, so a lookup re-validates the table against whatever module it is executing, and
//! [`ProgramWitness::describes`] extends that witness (digest + atom namespace) so the
//! entry table can outlive one run when the next run re-enters the same image
//! ([`Machine::bind_entry_programs`](crate::Machine)).
//!
//! It is **machine-owned by construction**, never part of the cross-run compiled-block
//! cache (`jit/cache/`): that cache is shared between machines and is filled by
//! `JitBackend::compile`, which receives no `ExecutionContext` — and a prepared program
//! is per-image data that must never answer for another image.
//!
//! # The lookup
//!
//! A direct-mapped table of [`PROGRAM_SLOTS`] slots, indexed by a multiplicative hash
//! of the packed `(function, code offset)` key: one multiply, one mask, one
//! bounds-checked slot read and one key compare, with the same latency and the same
//! (always-taken-on-the-hot-path) branch on every execution. A collision **replaces**
//! the slot, so two instructions that map to one slot re-prepare on each other's
//! behalf — a re-decode, never a wrong answer, because a program is only ever used
//! after its own key was compared equal.
//!
//! The table is handed to the family **by value** for the duration of one instruction
//! ([`Machine::with_bitstr_programs`](crate::Machine::with_bitstr_programs)), so
//! the executing code holds the programs locally and the borrow checker allows the
//! `&mut Machine` the segment writers need — no reference count is touched and no
//! slot is borrowed across the call. One call resolves the executing context's **id**
//! once, swaps that slot (`Contexts::take_programs`/`put_programs`, or the machine's own
//! field for context `0`) and files the table back on every ordinary exit, so a table
//! lost that way (an unwind) is simply re-prepared.

use beam_loader::Module;
use beam_term::atom::AtomSnapshot;

use super::create::prepared::CreateProgram;
use super::matching::prepared::MatchProgram;
/// The number of slots in one module's direct-mapped program table.
///
/// One cache line's worth of entries covers every instruction a real module has that
/// carries a bitstring descriptor list (a compiled comprehension loop has *one*), and
/// the table is only allocated for a module that has at least one such instruction.
const PROGRAM_SLOTS: usize = 64;

/// The vacant-slot key: [`program_key`] can never produce it, because a packed key's
/// two halves are a function index and a code offset and both are far below
/// `u32::MAX` — the same argument the machine's packed `current_code` relies on
/// (`machine::CURRENT_CODE_NONE`).
const VACANT: u64 = u64::MAX;

/// What the cache holds for one instruction address.
pub(crate) enum Program {
    /// The instruction's descriptor list could not be pre-resolved (a malformed or
    /// dynamic shape): the family runs its original generic walk, which reproduces
    /// the exact typed error — or the exact dynamic behaviour — it always produced.
    ///
    /// Cached too, so a shape that can never be prepared is not re-examined on every
    /// execution.
    Generic,
    /// A `bs_create_bin` segment program.
    Create(Box<CreateProgram>),
    /// A `bs_match` command program.
    Match(Box<MatchProgram>),
}

/// The packed cache key of the instruction at `(func, pc)`.
#[inline(always)]
pub(crate) fn program_key(func: u32, pc: u32) -> u64 {
    ((func as u64) << 32) | pc as u64
}

/// One module's prepared bitstring programs, addressed by the instruction's
/// `(function, code offset)`.
pub(crate) struct BitstrPrograms {
    /// The address of the module image this table was filled from — the identity
    /// witness. A table whose owner is not the module being executed is discarded
    /// before its first use, so an entry can never answer for another image (the
    /// entry context `0` is re-pointed at a new module on the next run without the
    /// table being rebuilt by hand).
    ///
    /// One word, compared by every lookup: the table is handed over **by value** once
    /// per bitstring instruction, so its size is a per-instruction cost — the
    /// run-boundary witness that needs three fields lives outside it
    /// ([`ProgramWitness`]).
    owner: usize,
    /// `PROGRAM_SLOTS` direct-mapped `(key, program)` slots, vacant until first
    /// allocation so a module with no bitstring instruction pays nothing.
    slots: Vec<(u64, Program)>,
}

impl BitstrPrograms {
    /// An **empty** table — the state before a module's first bitstring instruction.
    ///
    /// Every lookup on it prepares (there is nothing to answer with), which is exactly
    /// a cache that is not collecting.
    pub(crate) fn empty() -> BitstrPrograms {
        BitstrPrograms {
            owner: 0,
            slots: Vec::new(),
        }
    }

    /// Drop every prepared program and the owner witness (a fresh run).
    pub(crate) fn clear(&mut self) {
        *self = BitstrPrograms::empty();
    }

    /// Whether the table holds any prepared program at all (only a filled table is
    /// worth retaining across a run boundary).
    pub(crate) fn is_filled(&self) -> bool {
        !self.slots.is_empty()
    }

    /// The slot `key` hashes to.
    #[inline(always)]
    fn slot_index(key: u64) -> usize {
        // The golden-ratio multiplicative hash, high bits taken: a packed
        // `(func, pc)` key is a small number in its low half, so a low-bit mask would
        // cluster every instruction of one function; the top bits do not.
        ((key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40) as usize) & (PROGRAM_SLOTS - 1)
    }

    /// The prepared program at `key`, preparing it with `prepare` on a miss.
    ///
    /// `module` is the image being executed; a mismatched owner discards the whole
    /// table first (see [`BitstrPrograms::owner`]). A hit costs one multiply, one
    /// masks and one compare; the closure — the actual descriptor walk — runs only when
    /// the slot is vacant or holds another key.
    #[inline]
    pub(crate) fn get_or_prepare(
        &mut self,
        key: u64,
        module: &Module,
        prepare: impl FnOnce() -> Program,
    ) -> &Program {
        let owner = module as *const Module as usize;
        if self.owner != owner {
            self.owner = owner;
            self.slots.clear();
        }
        if self.slots.is_empty() {
            self.slots = (0..PROGRAM_SLOTS).map(|_| (VACANT, Program::Generic)).collect();
        }
        let slot = &mut self.slots[Self::slot_index(key)];
        if slot.0 != key {
            *slot = (key, prepare());
        }
        &slot.1
    }
    /// The number of slots the table holds (`0` before its first allocation) —
    /// test-only, so a test can prove the table is allocated lazily.
    #[cfg(test)]
    pub(crate) fn allocated_slots(&self) -> usize {
        self.slots.len()
    }
}

/// The **image witness** of a prepared-program table that outlives one run — the
/// identity its retention across a `reset` is justified by
/// ([`Machine::bind_entry_programs`](crate::Machine)).
///
/// It is deliberately *not* stored inside [`BitstrPrograms`]: the table is handed over
/// by value once per bitstring instruction, so it is kept to the one word
/// ([`BitstrPrograms::owner`]) the hot lookup compares, while these three fields are read
/// only at a run entry.
///
/// It is deliberately stricter than that one-word owner check, because a retained table
/// is no longer guarded by "the run that filled it is still in flight":
///
/// * the **address** says the programs' numbers (function indices, code offsets) belong
///   to this allocation, so a freed image's recycled address cannot answer;
/// * the **digest** is [`Module::checksum`] — an MD5 over the
///   `Atom`·`AtU8`·`Code`·`StrT`·`ImpT`·`ExpT`·`FunT`·`LitT`·`Meta`·`Recs`·`DbgB` chunk
///   subset, i.e. **exactly** the chunks a prepared program is derived from (the
///   descriptor list lives in `Code`, the segment/command type atoms in `AtU8`, a
///   `string` segment's bytes in `StrT`, a literal width in `LitT`);
/// * the **atom snapshot** says the descriptors resolve through the same index→name
///   mapping — the same companion the label-map cache keys on, and for the same reason: a
///   `Module::clone` (or a second load of one `.beam`) numbers atoms through its own
///   table.
///
/// An image with **no** content identity (an all-zero [`Module::checksum`] — the synthetic
/// shape a hand-built test image has, `machine/label_cache.rs`) is never retained:
/// `(function, code offset)` is not an identity there, so the same key may name a
/// different instruction's descriptor list from one run to the next.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProgramWitness {
    owner: usize,
    digest: [u8; 16],
    atoms: AtomSnapshot,
}

impl ProgramWitness {
    /// The witness of `module`.
    pub(crate) fn of(module: &Module) -> ProgramWitness {
        ProgramWitness {
            owner: module as *const Module as usize,
            digest: module.checksum(),
            atoms: module.atom_table.snapshot(),
        }
    }

    /// Whether a table this witness was recorded for may still answer for `module`.
    pub(crate) fn describes(&self, module: &Module) -> bool {
        self.digest != [0u8; 16] && *self == ProgramWitness::of(module)
    }
}