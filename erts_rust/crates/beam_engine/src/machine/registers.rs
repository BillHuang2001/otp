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

impl Machine {
    /// Read X register `n`, or `None` if it is outside the register file.
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
    /// Handing out the mutable slice **escapes the register file**: the caller may
    /// write any slot, which the [`Machine::set_x`] high-water mark cannot observe, so
    /// this marks the files escaped and the next [`Machine::reset`] refills them in
    /// full (see [`Machine::regs_escaped`]).
    #[inline]
    pub fn x_mut(&mut self) -> &mut [Eterm] {
        self.regs_escaped = true;
        &mut self.x
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
    /// separate accessor calls — and for why handing the slice out escapes the file
    /// (and so forces the next [`Machine::reset`] to refill it in full).
    #[inline]
    pub fn fr_mut(&mut self) -> &mut [Eterm] {
        self.regs_escaped = true;
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
    /// through it — a native JIT block (which stores straight into `machine.fr`
    /// through the raw pointer the generated code holds) or a host handed a raw
    /// mutable slice ([`Machine::fr_mut`]/[`Machine::x_mut`]/
    /// [`Machine::with_roots`](crate::machine::Machine::with_roots)) — sets the
    /// crate-internal `regs_escaped` flag instead of raising the mark, so in that
    /// window the mark may name fewer slots than are actually dirty. A caller that
    /// needs the honest upper bound in that window must account for it itself (e.g.
    /// conservatively treat the whole FR file as live, or track whether it took a raw
    /// slice). A caller that never runs a native block and never takes a raw slice
    /// sees a mark that is **exact** after the ordinary interpret seams, because
    /// every FR write goes through [`Machine::set_fr`].
    #[inline]
    pub fn fr_mark(&self) -> u32 {
        (self.fr_mark as usize).min(MAX_FLOAT_REGISTER as usize) as u32
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
    /// longer bound the dirty region. [`Machine::x_mut`]/[`Machine::fr_mut`]/
    /// [`Machine::with_roots`](crate::machine::Machine::with_roots) set the same flag for
    /// a host handed a raw slice.
    #[inline]
    pub(crate) fn mark_regs_escaped(&mut self) {
        self.regs_escaped = true;
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
    /// * the refill is **bounded by the written high-water mark**:
    ///   [`Machine::x_mark`]/[`Machine::fr_mark`] record the leading slots a register-
    ///   write seam has actually stored into since the last refill, so a run that
    ///   touched two x-registers and no FR only pays for those two words plus the FR
    ///   file's zero. A writer that cannot report its slots (a native JIT block, a host
    ///   handed a raw slice) sets [`Machine::regs_escaped`] instead, and this reset then
    ///   refills both files in full — a stale register would be a WRONG RESULT, so the
    ///   marks are never trusted past what the seams could prove.
    #[inline]
    pub fn reset(&mut self) {
        // The dirty region of each file: the tracked high-water mark, or the whole file
        // when a writer that could not report its slots has run. Everything outside the
        // region is still `NIL` from the previous refill (or the initial `resize`), so
        // skipping it is exactly equivalent to refilling it.
        let escaped = self.regs_escaped;
        let n = self.max_register as usize;
        if self.x.len() != n {
            // A fresh (or resized) file is `NIL`-filled by `resize` itself.
            self.x.clear();
            self.x.resize(n, Eterm::default());
            self.x_mark = 0;
        }
        let x_hi = if escaped {
            n
        } else {
            (self.x_mark as usize).min(n)
        };
        fill_nil(&mut self.x[..x_hi]);
        self.x_mark = 0;

        let fr_len = MAX_FLOAT_REGISTER as usize;
        if self.fr.len() != fr_len {
            self.fr.clear();
            self.fr.resize(fr_len, Eterm::default());
            self.fr_mark = 0;
        }
        let fr_hi = if escaped {
            fr_len
        } else {
            (self.fr_mark as usize).min(fr_len)
        };
        fill_nil(&mut self.fr[..fr_hi]);
        self.fr_mark = 0;
        self.regs_escaped = false;

        self.stack.clear();
        self.frames.clear();
        self.catch_frames.clear();
        self.pending_exception = None;
        // The class atoms belong to the module being run, so a reset (the start of
        // every `run_*`) invalidates them; the exceptions family reinstalls them.
        self.class_atoms = UNRESOLVED_CLASS_ATOMS;
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
        // The raise-time snapshot belongs to the run that is starting too: it is the
        // pre-unwind frame chain of whichever raise last happened, and a fresh run
        // begins with no raise in flight. As with `contexts`, `resume_outcome`
        // deliberately does **not** reset, so a raise that suspended the process
        // keeps its snapshot until the process resumes and raises again.
        self.raise_snapshot.clear();
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
    fn a_root_handoff_escapes_the_register_files() {
        // `with_roots` hands a receiver mutable slices of both files, so the marks
        // cannot be trusted afterwards: the next reset must refill in full. Prove it by
        // escaping, writing a deep slot through the handed slice, and resetting — a
        // mark-bounded refill would leave the write behind.
        let mut machine = Machine::new();
        machine.with_roots(u32::MAX, |roots| {
            roots.x[800] = Eterm::from_raw(0x5A5A);
            roots.fr[801] = Eterm::from_raw(0x5B5B);
        });
        assert_eq!(machine.x(800), Some(Eterm::from_raw(0x5A5A)));
        assert_eq!(machine.get_fr(801), Some(Eterm::from_raw(0x5B5B)));
        machine.reset();
        assert_eq!(machine.x(800), Some(Eterm::default()));
        assert_eq!(machine.get_fr(801), Some(Eterm::default()));
        assert!(machine.x.iter().all(|slot| *slot == Eterm::default()));
        assert!(machine.fr.iter().all(|slot| *slot == Eterm::default()));
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
