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

//! The register files: the x-register file and the float-register (FR) file.
//!
//! [`MAX_REGISTER`] and [`MAX_FLOAT_REGISTER`] bound the two files;
//! [`Machine::reset`] restores them (and the rest of the machine state) to its
//! initial, NIL-filled state. The accessors ([`Machine::x`]/[`Machine::set_x`]/
//! [`Machine::get_fr`]/[`Machine::set_fr`]) are bounds-checked, and an
//! out-of-range write is the typed [`crate::error::bad_register`] error — never a
//! panic and never a silently dropped write.

use beam_common::Result;
use beam_term::Eterm;

use crate::error;

use super::catch::UNRESOLVED_CLASS_ATOMS;
use super::Machine;

/// X-register file size (the `beam_emu.c` `MAX_REG` bound).
pub const MAX_REGISTER: u32 = 1024;

/// Float-register (FR) file size.
///
/// The C runtime's float-register file is
/// `union { FloatDef d[MAX_REG]; ... } f_reg_array`
/// (`../../erts/emulator/beam/erl_process.h`) with `#define MAX_REG 1024`
/// (`../../erts/emulator/beam/erl_vm.h:49`), so the FR file mirrors the real FR
/// count (1024) — the same bound as [`MAX_REGISTER`]. The C JIT bounds the index
/// to `0..=1023` (`../../erts/emulator/beam/jit/x86/beam_asm.hpp`).
pub const MAX_FLOAT_REGISTER: u32 = 1024;

/// The all-[`NIL`](beam_term::NIL) template the register files are refilled from,
/// and the word count that [`fill_nil`] copies at a time.
///
/// [`Machine::reset`] rewrites the whole x-register file (`max_register` words) and
/// the whole FR file ([`MAX_FLOAT_REGISTER`] words) on **every** run, so the refill
/// is on the per-run critical path. 16 words is 128 bytes — the widest
/// `copy_from_slice` the optimizer still expands *inline* into wide moves, and it is
/// read repeatedly, so it stays resident in L1.
///
/// It is deliberately **not** a `Machine` field: a per-machine buffer would double
/// the memory traffic (one copy into the buffer, one out of it) and would have to be
/// allocated, whereas a shared read-only table is cached once and read forever.
const NIL_CHUNK_WORDS: usize = 16;

/// The template itself; see [`NIL_CHUNK_WORDS`].
static NIL_CHUNK: [Eterm; NIL_CHUNK_WORDS] = [Eterm::nil(); NIL_CHUNK_WORDS];

/// Overwrite every slot of `slots` with [`beam_term::NIL`].
///
/// The shape is the whole point (the alternative formulations were measured
/// interleaved in one process, refilling the two 1024-word files):
///
/// * a plain `fill` (or `clear` + `resize`) store loop is up to ~1.9x **slower** —
///   the optimizer emits narrow stores for it;
/// * one `memcpy` per file (or a chunk wider than 128 bytes) is **bimodal**: the
///   optimizer turns it into a call to the platform `memcpy`, which is 2.5x faster
///   in some processes and ~1.15x *slower* in others (98 ns vs 375 ns for the two
///   files) depending on the run's code path / address layout — a per-run floor
///   that varies with `memcpy` is not something the interpreter should pay;
/// * this 128-byte chunked copy is the fastest *and* the most stable of the three
///   (177-188 ns vs 279-346 ns for the two files, in every measured run): the copy
///   is expanded inline into wide moves, with no `memcpy` call and no dependence on
///   where the destination happened to be allocated.
///
/// The remainder path (a register file smaller than one chunk, i.e. any
/// `Machine::with_max_register(n)` with `n < NIL_CHUNK_WORDS`) makes the function
/// total: it handles any slice length without panicking. It is also what a
/// mark-bounded refill of a few live registers goes through, so the reduced refill
/// cost stays a straight `chunks_exact_mut` pass plus one short tail fill.
#[inline]
fn fill_nil(slots: &mut [Eterm]) {
    let mut chunks = slots.chunks_exact_mut(NIL_CHUNK_WORDS);
    for chunk in &mut chunks {
        chunk.copy_from_slice(&NIL_CHUNK);
    }
    chunks.into_remainder().fill(Eterm::default());
}

/// One past the last slot of a register file that can hold a non-`NIL` word: the larger
/// of the `set_x`/`set_fr` seam high-water mark and the writer-reported dirty extent,
/// clamped to the file length.
///
/// This is the single rule both [`Machine::reset`]'s refill bound and
/// [`Machine::with_roots`](crate::machine::Machine::with_roots)'s handed prefix use, so
/// the two can never disagree on how much of a file is live.
#[inline]
fn used_extent(len: usize, mark: u32, dirty: u32) -> u32 {
    mark.max(dirty).min(len as u32)
}

impl Machine {    /// Read X register `n`, or `None` if it is outside the register file.
    ///
    /// `#[inline]`-marked: this is the operand seam's innermost accessor, called
    /// several times per interpreted instruction, and the crate is compiled into
    /// many codegen units, so without the hint the call never reaches the dispatch
    /// loop's own unit.
    #[inline]
    pub fn x(&self, n: u32) -> Option<Eterm> {
        self.x.get(n as usize).copied()
    }

    /// The whole X-register file as a mutable slice — the **root-set hand-off**.
    ///
    /// The engine's live roots are `x[0..live)` plus the whole Erlang stack plus the
    /// whole FR file ([`crate::context::RootSet`]). This is the raw single-view
    /// accessor for the X file; a caller that needs the **whole** root set at once
    /// (all three files, disjointly borrowed) must use
    /// [`Machine::with_roots`](crate::machine::Machine::with_roots), because three
    /// separate `&mut self` accessor calls cannot coexist.
    ///
    /// Handing out the mutable slice notes the file's **whole-file dirty extent**: the
    /// caller may write any slot, which the [`Machine::set_x`] high-water mark cannot
    /// observe, so the next [`Machine::reset`] refills the file in full (see
    /// [`Machine::x_dirty`]). A caller that knows which slots it wrote should use a
    /// narrower door ([`Machine::with_roots`](crate::machine::Machine::with_roots) hands
    /// only the prefix that can hold a term).
    #[inline]
    pub fn x_mut(&mut self) -> &mut [Eterm] {
        self.note_x_dirty(self.x.len() as u32);
        &mut self.x
    }

    /// One past the highest X-register slot that can hold a non-[`NIL`](beam_term::NIL)
    /// word: the larger of the [`Machine::x_mark`] seam high-water mark and the
    /// [`Machine::x_dirty`] writer-reported extent, clamped to the file length.
    ///
    /// This is the **writable root prefix** of the file — every slot at or beyond it is
    /// `NIL` by the refill invariant, so it is not a root and cannot be relocated. It is
    /// both what [`Machine::with_roots`](crate::machine::Machine::with_roots) hands a
    /// host and what bounds the next [`Machine::reset`]'s refill, which is what makes the
    /// reset proportional to what a run actually used.
    #[inline]
    pub(crate) fn used_x_extent(&self) -> u32 {
        used_extent(self.x.len(), self.x_mark, self.x_dirty)
    }

    /// The FR-file twin of [`Machine::used_x_extent`].
    #[inline]
    pub(crate) fn used_fr_extent(&self) -> u32 {
        used_extent(self.fr.len(), self.fr_mark, self.fr_dirty)
    }

    /// Extend the X file's dirty extent to at least the exclusive `extent` (clamped to
    /// the file length); a smaller report never shrinks it.
    #[inline]
    pub(crate) fn note_x_dirty(&mut self, extent: u32) {
        let extent = extent.min(self.x.len() as u32);
        if extent > self.x_dirty {
            self.x_dirty = extent;
        }
    }

    /// The FR-file twin of [`Machine::note_x_dirty`].
    #[inline]
    pub(crate) fn note_fr_dirty(&mut self, extent: u32) {
        let extent = extent.min(self.fr.len() as u32);
        if extent > self.fr_dirty {
            self.fr_dirty = extent;
        }
    }

    /// Read float register `n`, or `None` if it is outside the FR file.
    ///
    /// Mirrors [`Machine::x`]: a bounds-checked read of the FR file.
    #[inline]
    pub fn get_fr(&self, n: u32) -> Option<Eterm> {
        self.fr.get(n as usize).copied()
    }

    /// The whole float-register (FR) file as a mutable slice — part of the
    /// **root-set hand-off**.
    ///
    /// The FR file holds `Eterm` **term slots** at Milestone 1, so a float register
    /// can hold a pointer into the process heap and must be a GC root; see
    /// [`Machine::x_mut`] for why the whole-set hand-off goes through
    /// [`Machine::with_roots`](crate::machine::Machine::with_roots) rather than three
    /// separate accessor calls — and for why handing the slice out notes the file's
    /// whole-file dirty extent (and so forces the next [`Machine::reset`] to refill it in
    /// full).
    #[inline]
    pub fn fr_mut(&mut self) -> &mut [Eterm] {
        self.note_fr_dirty(self.fr.len() as u32);
        &mut self.fr
    }
    /// The number of **live leading FR slots** — the exclusive high-water mark of
    /// the float-register file.
    ///
    /// Returns exactly `(self.fr_mark as usize).min(MAX_FLOAT_REGISTER as usize) as
    /// u32`: one past the highest FR slot an FR-write seam ([`Machine::set_fr`]) has
    /// stored into since the last [`Machine::reset`]. `0` means **no live FR slot**
    /// (every slot is `NIL`). It is the FR-file twin of the x-register root-set bound
    /// `x[0..live)`: it lets a host publish/scan only the live leading FR slots
    /// instead of all [`MAX_FLOAT_REGISTER`], since a `set_fr` only ever writes a
    /// leading slot (the marks are raised, never lowered, by an in-range write).
    ///
    /// **Caveat — a slot-escaping writer may understate it.** The mark advances only
    /// through [`Machine::set_fr`]. A writer that can reach the FR file without going
    /// through it — a host handed a raw mutable slice ([`Machine::fr_mut`]/
    /// [`Machine::x_mut`]/[`Machine::with_roots`](crate::machine::Machine::with_roots)) or
    /// a writer that reports an extent ([`Machine::note_fr_dirty`]) — records the
    /// [`Machine::fr_dirty`] extent instead of raising the mark, so in that window the
    /// mark may name fewer slots than are actually dirty (the reset refill and the root
    /// hand-off both use `max(mark, dirty)`, so nothing they do depends on the mark
    /// alone). A caller that needs the honest upper bound in that window must read
    /// [`Machine::used_fr_extent`] or conservatively treat the whole FR file as live. A
    /// caller that never takes a raw slice sees a mark that is **exact** after the
    /// ordinary interpret seams, because every FR write goes through [`Machine::set_fr`].
    #[inline]
    pub fn fr_mark(&self) -> u32 {
        (self.fr_mark as usize).min(MAX_FLOAT_REGISTER as usize) as u32
    }

    /// The number of register words the last [`Machine::reset`] refilled: the x-file words
    /// plus the FR-file words (`x_hi + fr_hi` of that refill).
    ///
    /// A deterministic counter of the reset's actual work, and the figure that shows the
    /// dirty-region refill is proportional to what a run used rather than to the 2048
    /// words of the two files. Diagnostic/test surface only — nothing in the engine reads
    /// it.
    #[inline]
    pub fn refilled_words(&self) -> u32 {
        self.refilled_words
    }
    /// Write `value` into float register `n`.
    ///
    /// Mirrors [`Machine::set_x`]: returns [`crate::error::bad_register`] if `n` is
    /// outside the FR file, and raises [`Machine::fr_mark`] on success so
    /// [`Machine::reset`] refills the slot.
    #[inline]
    pub fn set_fr(&mut self, n: u32, value: Eterm) -> Result<()> {
        match self.fr.get_mut(n as usize) {
            Some(slot) => {
                *slot = value;
                // A *conditional* store (not `self.fr_mark.max(..)`, which always
                // stores): the mark almost never moves in a straight-line body, so the
                // common path must not pay a memory write on top of the register write.
                let hi = n + 1;
                if hi > self.fr_mark {
                    self.fr_mark = hi;
                }
                Ok(())
            }
            None => Err(error::bad_register(n)),
        }
    }

    /// Write `v` into X register `n`.
    ///
    /// Returns [`crate::error::bad_register`] if `n` is outside the register file, and
    /// raises [`Machine::x_mark`] on success so [`Machine::reset`] refills the slot.
    #[inline]
    pub(crate) fn set_x(&mut self, n: u32, v: Eterm) -> Result<()> {
        match self.x.get_mut(n as usize) {
            Some(slot) => {
                *slot = v;
                // See `set_fr`: `n` is in range, so `n + 1` cannot overflow
                // (`max_register` is far below `u32::MAX`), and the mark is only
                // stored when it actually moves.
                let hi = n + 1;
                if hi > self.x_mark {
                    self.x_mark = hi;
                }
                Ok(())
            }
            None => Err(error::bad_register(n)),
        }
    }

    /// Declare that a writer which cannot report the register slots it touched has run,
    /// so the next [`Machine::reset`] must refill **both** register files in full.
    ///
    /// The native driver calls this as soon as a compiled block has executed: generated
    /// code stores straight into `machine.x` through the pointer it holds (and may touch
    /// the FR file), so the [`Machine::x_mark`]/[`Machine::fr_mark`] high-water marks no
    /// longer bound the dirty region. It is the wide form of
    /// [`Machine::note_x_dirty`]/[`Machine::note_fr_dirty`]: a caller that knows the
    /// extent it may have written reports it there instead and pays only that much.
    #[inline]
    pub(crate) fn mark_regs_escaped(&mut self) {
        self.note_x_dirty(self.x.len() as u32);
        self.note_fr_dirty(self.fr.len() as u32);
    }
    /// Reset to the initial state: a fresh (NIL-filled) register file and empty
    /// stack and frame stacks.
    ///
    /// # Cost model
    ///
    /// This is on the per-run critical path (every `run_*` entry resets first), so
    /// it is written as the cheapest *correct* full reset:
    ///
    /// * the register/FR files are **refilled in one pass** by [`fill_nil`] (a
    ///   128-byte chunked copy of [`NIL_CHUNK`]), which is 1.7-2x faster than the
    ///   `clear()` + `resize()` store loop it replaced (measured 150-175 ns vs
    ///   252-345 ns for the two files, interleaved in one process) — see
    ///   [`fill_nil`] for why the shape is what it is;
    /// * a capacity that is already correct is **never reallocated** — the vectors
    ///   are only resized when their length differs from the target (which happens
    ///   once, for the first reset after `Machine::with_max_register`, and never
    ///   again in steady state), so the register base stays put across runs;
    /// * the refill is **bounded by the dirty region**: [`Machine::x_mark`]/
    ///   [`Machine::fr_mark`] record the leading slots a register-write seam has actually
    ///   stored into since the last refill, and [`Machine::x_dirty`]/[`Machine::fr_dirty`]
    ///   record the extent a writer that cannot report its slots ([`Machine::x_mut`], a
    ///   root hand-off, a native block) may have reached. The refill covers
    ///   `max(mark, dirty)` of each file, so a run that touched three x-registers and no
    ///   FR pays three words instead of 2048 — and the patch is *exactly* equivalent to a
    ///   full one, because every slot outside the region is still `NIL` (the initial
    ///   `resize` and every previous reset filled it) and every writer reports.
    #[inline]
    pub fn reset(&mut self) {
        // The dirty region of each file: the larger of the tracked high-water mark and the
        // reported writer extent. Everything outside the region is still `NIL` from the
        // previous refill (or the initial `resize`), so skipping it is exactly equivalent
        // to refilling it.
        let n = self.max_register as usize;
        if self.x.len() != n {
            // A fresh (or resized) file is `NIL`-filled by `resize` itself.
            self.x.clear();
            self.x.resize(n, Eterm::default());
            self.x_mark = 0;
            self.x_dirty = 0;
        }
        let x_hi = (used_extent(n, self.x_mark, self.x_dirty) as usize).min(n);
        fill_nil(&mut self.x[..x_hi]);
        self.x_mark = 0;
        self.x_dirty = 0;
        self.refilled_words = x_hi as u32;

        let fr_len = MAX_FLOAT_REGISTER as usize;
        if self.fr.len() != fr_len {
            self.fr.clear();
            self.fr.resize(fr_len, Eterm::default());
            self.fr_mark = 0;
            self.fr_dirty = 0;
        }
        let fr_hi = (used_extent(fr_len, self.fr_mark, self.fr_dirty) as usize).min(fr_len);
        fill_nil(&mut self.fr[..fr_hi]);
        self.fr_mark = 0;
        self.fr_dirty = 0;
        self.refilled_words += fr_hi as u32;
        self.stack.clear();
        self.frames.clear();
        self.catch_frames.clear();
        self.pending_exception = None;
        // The class atoms belong to the module being run, so a reset (the start of
        // every `run_*`) invalidates them; the exceptions family reinstalls them. The
        // `class_atoms_live` latch records that they were resolved from the live atom
        // namespace for the run in progress, so it is invalidated with them — a stale
        // `true` would make the new run skip the resolution its own namespace needs.
        self.class_atoms = UNRESOLVED_CLASS_ATOMS;
        self.class_atoms_live = false;
        // Likewise the literal area: it belongs to the module being run and is
        // re-bound by `run_loop`.
        self.literals = None;
        self.entry_literals = None;
        // The cross-module call seam's state belongs to the run that is starting: a
        // fresh run begins in its own entry module (context 0) and has entered no
        // callee yet. `resume_outcome` deliberately does **not** reset, so a
        // suspended cross-module frame keeps both its context id and the callee
        // module handle it must be resumed in.
        self.contexts.clear();
        self.context = 0;
        // The local-callee identity memo is address-keyed, so its entries are only valid
        // for the run that filled them: a fresh run must re-derive rather than answer
        // with an image that is no longer the one being executed. This is also what keeps
        // the key ABA-free (no address it holds can be recycled while it holds it). As
        // with `contexts`, `resume_outcome` deliberately does **not** reset, and a
        // suspended machine's modules are still alive.
        self.local_callees.clear();
        // The raise-time snapshot belongs to the run that is starting too: it is the
        // pre-unwind frame chain of whichever raise last happened, and a fresh run
        // begins with no raise in flight. As with `contexts`, `resume_outcome`
        // deliberately does **not** reset, so a raise that suspended the process
        // keeps its snapshot until the process resumes and raises again.
        self.raise_snapshot.clear();
        // The synthesized-callee-head latches belong to the raise in flight too: a
        // fresh run begins with no raise, so neither the one-shot latch nor the
        // live-raise state may survive into it (a stale `true` would make the first
        // cooked trace of the new run prepend an item no raise published).
        self.pending_callee_head = false;
        self.raise_head_item = false;
        // The failed-clause argument list rides the same two latches, so a fresh run
        // begins with neither armed: a stale list would make the first cooked trace of
        // the new run render an argument list no raise in it published (and the term
        // belongs to the run that raised, not to this one).
        self.pending_function_clause_args = None;
        self.raise_function_clause_args = None;
        // The supplied-symbolic-frames latches ride the same two-latch protocol: a
        // fresh run begins with neither armed, so a raise in the new run can never
        // render the frames an earlier run's raise arrived carrying.
        self.pending_supplied_trace = None;
        self.raise_supplied_trace = None;
        // The execution-position introspection belongs to the run that is starting:
        // `step_instruction` re-records `current_code` and `run_loop` re-records
        // `entry_call` (the `$initial_call` analogue).
        self.current_code = None;
        self.entry_call = None;
        // The A.8 tracing gate is re-latched from the context at every run entry
        // (`run_loop_with_map` / the native driver). Clearing it here means a run
        // that never latches it (a bare `step_instruction` in a test) cannot inherit
        // a stale `true` from an earlier traced run.
        self.trace_on = false;
        // The reduction-hook gate is likewise re-latched from the context at every
        // run entry (`Machine::drive`); clearing it here keeps a run that never
        // latches it from inheriting a stale value.
        self.reduce_on = false;
        // The dynamic-census gate is the same kind of per-run latch (a copy of the
        // thread's `exec_census` enable state, taken at the run entry); clearing it
        // here keeps a run that never latches it — a bare `step_instruction` in a
        // test — from inheriting a stale `true` from an earlier counted run.
        self.census_on = false;
    }
}

#[cfg(test)]
mod tests {
    use super::{Machine, MAX_FLOAT_REGISTER, MAX_REGISTER};
    use beam_term::Eterm;

    #[test]
    fn new_has_nil_filled_register_file() {
        let machine = Machine::new();
        assert_eq!(machine.x.len(), MAX_REGISTER as usize);
        assert_eq!(machine.x(0), Some(Eterm::default()));
        assert_eq!(machine.x(MAX_REGISTER - 1), Some(Eterm::default()));
        assert_eq!(machine.x(MAX_REGISTER), None);
        assert_eq!(machine.frame_depth(), 0);
        assert_eq!(machine.stack_depth(), 0);
    }

    #[test]
    fn with_max_register_bounds_the_file() {
        let machine = Machine::with_max_register(4);
        assert_eq!(machine.x, vec![Eterm::default(); 4]);
    }

    #[test]
    fn new_has_nil_filled_float_register_file() {
        let machine = Machine::new();
        assert_eq!(machine.fr.len(), MAX_FLOAT_REGISTER as usize);
        assert!(machine.fr.iter().all(|slot| *slot == Eterm::default()));
        assert_eq!(machine.get_fr(0), Some(Eterm::default()));
        assert_eq!(
            machine.get_fr(MAX_FLOAT_REGISTER - 1),
            Some(Eterm::default())
        );
        assert_eq!(machine.get_fr(MAX_FLOAT_REGISTER), None);
    }

    #[test]
    fn set_fr_get_fr_round_trip_in_range() {
        let mut machine = Machine::new();
        let v = Eterm::from_raw(0x5F5F);
        assert!(machine.set_fr(10, v).is_ok());
        assert_eq!(machine.get_fr(10), Some(v));
        assert_eq!(machine.get_fr(11), Some(Eterm::default()));
    }

    #[test]
    fn fr_access_is_bounds_checked() {
        let mut machine = Machine::new();
        assert_eq!(machine.get_fr(MAX_FLOAT_REGISTER), None);
        let err = machine.set_fr(MAX_FLOAT_REGISTER, Eterm::from_raw(1));
        assert!(matches!(err, Err(beam_common::BeamError::BadArg)));
    }

    #[test]
    fn fr_mark_tracks_the_live_leading_slots() {
        // The FR high-water mark a host reads to publish only the live leading slots:
        // a fresh machine touches none, an in-range write raises it to `n + 1`, a
        // lower write does not lower it, and `reset` clears it.
        let mut machine = Machine::new();
        assert_eq!(machine.fr_mark(), 0, "a fresh machine has no live FR slot");

        assert!(machine.set_fr(5, Eterm::from_raw(0x11)).is_ok());
        assert_eq!(machine.fr_mark(), 6, "set_fr(5) marks six leading slots");

        // A later write to a *lower* index must not lower the high-water mark.
        assert!(machine.set_fr(2, Eterm::from_raw(0x22)).is_ok());
        assert_eq!(machine.fr_mark(), 6, "a lower write keeps the mark");

        // A deeper write raises it, and the top slot saturates at the file size.
        assert!(machine.set_fr(9, Eterm::from_raw(0x33)).is_ok());
        assert_eq!(machine.fr_mark(), 10);
        assert!(machine
            .set_fr(MAX_FLOAT_REGISTER - 1, Eterm::from_raw(0x44))
            .is_ok());
        assert_eq!(machine.fr_mark(), MAX_FLOAT_REGISTER);

        // An out-of-range write is refused and does not touch the mark.
        machine.reset();
        assert!(machine
            .set_fr(MAX_FLOAT_REGISTER, Eterm::from_raw(0x55))
            .is_err());
        assert_eq!(machine.fr_mark(), 0, "an out-of-range write marks nothing");

        // `reset` returns the mark to zero.
        assert!(machine.set_fr(3, Eterm::from_raw(0x66)).is_ok());
        assert_eq!(machine.fr_mark(), 4);
        machine.reset();
        assert_eq!(machine.fr_mark(), 0, "reset clears the mark");
    }

    #[test]
    fn reset_clears_a_dirty_float_register() {
        let mut machine = Machine::new();
        machine.set_fr(3, Eterm::from_raw(0x77)).unwrap();
        machine.set_x(0, Eterm::from_raw(0x88)).unwrap();
        machine.reset();
        assert_eq!(machine.fr.len(), MAX_FLOAT_REGISTER as usize);
        assert!(machine.fr.iter().all(|slot| *slot == Eterm::default()));
        assert_eq!(machine.get_fr(3), Some(Eterm::default()));
        // The x-register behavior is untouched by the FR reset.
        assert_eq!(machine.x.len(), MAX_REGISTER as usize);
        assert_eq!(machine.x(0), Some(Eterm::default()));
    }

    #[test]
    fn set_x_checks_the_bound() {
        let mut machine = Machine::with_max_register(4);
        let v = Eterm::from_raw(0x2A);
        assert!(machine.set_x(3, v).is_ok());
        assert_eq!(machine.x(3), Some(v));
        assert!(machine.set_x(4, v).is_err());
    }

    #[test]
    fn reset_clears_state_and_refills_registers() {
        let mut machine = Machine::with_max_register(3);
        machine.set_x(0, Eterm::from_raw(7)).unwrap();
        machine.push_frame(2).unwrap();
        machine.reset();
        assert_eq!(machine.x, vec![Eterm::default(); 3]);
        assert_eq!(machine.frame_depth(), 0);
        assert_eq!(machine.stack_depth(), 0);
    }

    #[test]
    #[allow(unsafe_code)]
    fn reset_clears_a_register_written_through_the_pointer_a_native_block_holds() {
        // A native baseline block stores straight into `machine.x` through the raw
        // pointer the generated code holds — a pointer taken from the file itself, so
        // the write is invisible to the `set_x` high-water mark. The escape must
        // therefore force a FULL refill: a raw store into register 500 is wiped by the
        // next `reset`, i.e. no high-water-mark shortcut may skip it.
        let mut machine = Machine::new();
        // SAFETY: the store stays inside the register file's allocation
        // (`MAX_REGISTER` live words) and `Eterm` is `repr(transparent)` over a
        // machine word, so the write is in bounds and well-aligned.
        unsafe {
            let base = machine.x_mut().as_mut_ptr();
            base.add(500).write(Eterm::from_raw(0xDEAD_BEEF));
        }
        assert_eq!(machine.x(500), Some(Eterm::from_raw(0xDEAD_BEEF)));
        machine.reset();
        assert_eq!(machine.x(500), Some(Eterm::default()));
        assert!(machine.x.iter().all(|slot| *slot == Eterm::default()));
        assert!(machine.fr.iter().all(|slot| *slot == Eterm::default()));
    }

    #[test]
    fn a_mark_skipped_refill_never_leaves_a_stale_register() {
        // The refill is bounded by the written high-water mark, so the mark must never
        // fall *below* a slot a seam wrote — a stale register is a wrong result. Write
        // a deeper register each round, then a shallow one (so the next mark is small),
        // and assert both files are exactly NIL on the far side of every reset.
        let mut machine = Machine::new();
        for round in 0..12u32 {
            let deep = 3 * round + 1;
            machine.set_x(deep, Eterm::from_raw(0xABC + round as usize)).unwrap();
            machine.set_fr(deep, Eterm::from_raw(0xDEF)).unwrap();
            // A run that also touches only low registers, so the *next* mark is small.
            machine.set_x(0, Eterm::from_raw(7)).unwrap();
            machine.set_fr(0, Eterm::from_raw(7)).unwrap();
            machine.reset();
            assert!(
                machine.x.iter().all(|slot| *slot == Eterm::default()),
                "round {round}: a stale x register survived the mark-bounded refill"
            );
            assert!(
                machine.fr.iter().all(|slot| *slot == Eterm::default()),
                "round {round}: a stale FR register survived the mark-bounded refill"
            );
        }
    }

    #[test]
    fn a_seam_write_is_refilled_wherever_it_lands() {
        // The mark-bounded refill must cover the highest slot a seam wrote, not just
        // the low ones: 700 and 900 are far outside anything a small run touches.
        let mut machine = Machine::new();
        machine.set_x(700, Eterm::from_raw(0x11)).unwrap();
        machine.set_fr(900, Eterm::from_raw(0x22)).unwrap();
        machine.reset();
        assert_eq!(machine.x(700), Some(Eterm::default()));
        assert_eq!(machine.get_fr(900), Some(Eterm::default()));
        assert!(machine.x.iter().all(|slot| *slot == Eterm::default()));
        assert!(machine.fr.iter().all(|slot| *slot == Eterm::default()));
    }

    #[test]
    fn a_root_handoff_hands_the_used_prefix_and_every_slot_it_hands_is_refilled() {
        // `with_roots` hands the receiver the machine's **writable root prefix** of each
        // file — one past the highest slot the run has actually written — not the whole
        // file. The narrowing is sound *by construction*: every slot at or beyond the
        // prefix is NIL by the refill invariant, so it holds no term and cannot be a root,
        // and the host physically cannot write outside the slice it received.
        //
        // Both halves have to hold, so both are asserted: the slice is exactly the used
        // prefix, and every slot in it is refilled even though the writes went around the
        // `set_x`/`set_fr` seams (which never saw them) — the host reports its reach
        // through the `x_dirty`/`fr_dirty` extent instead.
        let mut machine = Machine::new();
        machine.set_x(1, Eterm::from_raw(0x1111)).unwrap();
        machine.set_fr(2, Eterm::from_raw(0x2222)).unwrap();
        let mut handed = (0usize, 0usize);
        machine.with_roots(u32::MAX, |roots| {
            handed = (roots.x.len(), roots.fr.len());
            // The widest write a host can make: every slot it was handed.
            for slot in roots.x.iter_mut() {
                *slot = Eterm::from_raw(0x5A5A);
            }
            for slot in roots.fr.iter_mut() {
                *slot = Eterm::from_raw(0x5B5B);
            }
            assert!(
                roots.x.get_mut(roots.x.len()).is_none(),
                "the x slice stops at the used prefix — a host cannot reach past it"
            );
            assert!(
                roots.fr.get_mut(roots.fr.len()).is_none(),
                "the FR slice stops at the live prefix — a host cannot reach past it"
            );
        });
        // One past the highest written x slot (x1) and FR slot (fr2).
        assert_eq!(
            handed,
            (2, 3),
            "the hand-off is the used prefix of each file, not the whole 1024-word file"
        );
        assert_eq!(machine.x(1), Some(Eterm::from_raw(0x5A5A)));
        assert_eq!(machine.get_fr(2), Some(Eterm::from_raw(0x5B5B)));
        // Only the hand-off is narrow: the files themselves are whole.
        assert_eq!(machine.x.len(), MAX_REGISTER as usize);
        assert_eq!(machine.fr.len(), MAX_FLOAT_REGISTER as usize);

        machine.reset();
        assert!(
            machine.x.iter().all(|slot| *slot == Eterm::default()),
            "a slot written through the handed slice survived the refill"
        );
        assert!(
            machine.fr.iter().all(|slot| *slot == Eterm::default()),
            "an FR slot written through the handed slice survived the refill"
        );
    }

    #[test]
    fn an_extent_bounded_refill_never_leaves_a_stale_register() {
        // The extent path (a writer that reports the slots it may have reached, i.e. a
        // native block or a root hand-off) must be as sound as the mark path: a deep
        // reported extent is refilled in full, and the *next* run — which reports a tiny
        // extent of its own — still leaves every slot of both files NIL.
        let mut machine = Machine::new();
        for round in 0..8u32 {
            let deep = 40 * round + 5;
            // A run that reports a deep extent and writes the slot at its edge.
            machine.note_x_dirty(deep + 1);
            machine.note_fr_dirty(deep + 2);
            machine.x[deep as usize] = Eterm::from_raw(0xC0DE + round as usize);
            machine.fr[deep as usize] = Eterm::from_raw(0xC0DF);
            machine.reset();
            assert!(
                machine.x.iter().all(|slot| *slot == Eterm::default()),
                "round {round}: a stale x register survived the extent-bounded refill"
            );
            assert!(
                machine.fr.iter().all(|slot| *slot == Eterm::default()),
                "round {round}: a stale FR register survived the extent-bounded refill"
            );

            // The shallow run: two x slots and one FR slot, and its own reset must leave
            // both files exactly as a full 2048-word refill would.
            machine.set_x(1, Eterm::from_raw(0x11)).unwrap();
            machine.set_fr(0, Eterm::from_raw(0x22)).unwrap();
            machine.reset();
            assert!(
                machine.x.iter().all(|slot| *slot == Eterm::default()),
                "round {round}: the shallow run left a stale x register"
            );
            assert!(
                machine.fr.iter().all(|slot| *slot == Eterm::default()),
                "round {round}: the shallow run left a stale FR register"
            );
        }
    }

    #[test]
    fn a_three_register_run_refills_three_words_not_the_whole_file() {
        // The deterministic work counter: the refill is proportional to what the run used,
        // not to the 2048 words of the two files.
        let mut machine = Machine::new();
        machine.set_x(0, Eterm::from_raw(1)).unwrap();
        machine.set_x(1, Eterm::from_raw(2)).unwrap();
        machine.set_x(2, Eterm::from_raw(3)).unwrap();
        machine.reset();
        assert_eq!(
            machine.refilled_words(),
            3,
            "three written x registers and no FR slot cost three words, not 2048"
        );
        assert!(machine.x.iter().all(|slot| *slot == Eterm::default()));
        assert!(machine.fr.iter().all(|slot| *slot == Eterm::default()));

        // A run that reports an extent (the native / root-hand-off door) is priced the
        // same way, by the extent it names.
        machine.note_x_dirty(7);
        machine.note_fr_dirty(2);
        machine.reset();
        assert_eq!(machine.refilled_words(), 9, "the reported extent, x7 + fr2");
    }

    #[test]
    fn all_live_hands_at_least_the_written_prefix_and_a_deep_write_is_refilled() {
        // `ALL_LIVE` stays a *hint*: what the host receives is the used prefix of the file,
        // which for a run that has written 500 registers is at least 500 slots — so every
        // live root is inside the slice — and a deep write through it is refilled.
        let mut machine = Machine::new();
        for register in 0..500u32 {
            machine
                .set_x(register, Eterm::from_raw(0x1000 + register as usize))
                .unwrap();
        }
        let handed = machine.with_roots(u32::MAX, |roots| {
            roots.x[499] = Eterm::from_raw(0xDEAD);
            roots.x.len()
        });
        assert!(
            handed >= 500,
            "ALL_LIVE hands every written register, got {handed}"
        );
        assert_eq!(machine.x(499), Some(Eterm::from_raw(0xDEAD)));
        machine.reset();
        assert_eq!(machine.x(499), Some(Eterm::default()));
        assert!(machine.x.iter().all(|slot| *slot == Eterm::default()));
    }
    #[test]
    fn repeated_resets_never_reallocate_the_register_files() {
        // The reset must retain capacity across runs: a `Machine` is reused for
        // every run, and a reallocation would both cost time and move the register
        // base under any live raw pointer.
        let mut machine = Machine::with_max_register(8);
        let x_capacity = machine.x.capacity();
        let fr_capacity = machine.fr.capacity();
        assert!(x_capacity >= 8);
        assert!(fr_capacity >= MAX_FLOAT_REGISTER as usize);
        for _ in 0..16 {
            machine.set_x(0, Eterm::from_raw(1)).unwrap();
            machine.set_fr(0, Eterm::from_raw(1)).unwrap();
            machine.reset();
            assert_eq!(machine.x.capacity(), x_capacity);
            assert_eq!(machine.fr.capacity(), fr_capacity);
            assert_eq!(machine.x.len(), 8);
            assert_eq!(machine.fr.len(), MAX_FLOAT_REGISTER as usize);
        }
    }
}
