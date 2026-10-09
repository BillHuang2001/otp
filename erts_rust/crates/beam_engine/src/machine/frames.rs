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

//! The call-frame model: continuations, Y slots and the frame stack.
//!
//! A frame is the triple [`Frame`] `{ continuation, y_base, y_count }`: its
//! continuation-pointer slot is modeled as a typed [`Continuation`] rather than a
//! raw stack word (see [`crate::machine`]), and its Y slots are the
//! `y_count` words of the Erlang stack starting at `y_base`. This module owns the
//! primitives the dispatch loop builds on: the Y-slot accessors
//! ([`Machine::y`]/[`Machine::set_y`]), the frame push/pop/trim operations, the
//! continuation accessors, the heap request seam ([`Machine::ensure_heap`]) and
//! the caller-chain introspection ([`Machine::resumption_contexts`] /
//! [`Machine::resumption_addresses`]).

use beam_common::Result;
use beam_term::Eterm;

use crate::context::ExecutionContext;
use crate::error;

use super::Machine;

/// Where a frame returns to.
///
/// The interpreter stores this in each frame's continuation-pointer slot instead
/// of a raw stack word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Continuation {
    /// Return to code offset `pc` in the function at index `func` **of module
    /// context `context`**.
    ///
    /// The context is recorded with the address because a continuation is written
    /// by the *function being suspended*, not by the frame that holds it: a
    /// cross-module `call_ext` stores `{func, pc + 1, context}` into the live frame,
    /// and a frame-less function's non-tail call stores its own module's context
    /// into the caller's frame. On `return` the machine therefore follows *this*
    /// context, which is what lets control come back into the caller's module. A
    /// context equal to the machine's current one is an ordinary intra-module
    /// [`Step::Goto`](crate::Step).
    Address {
        /// The function index (into [`Module::functions`](beam_loader::Module::functions)
        /// of the context's module).
        func: u32,
        /// The code offset within that function.
        pc: u32,
        /// The module context the address belongs to (`0` = the run's entry
        /// module, otherwise a [`ModuleContext`](super::ModuleContext) id).
        context: u32,
    },
    /// The bottom frame: returning halts execution and yields `x(0)`.
    Halt,
}

/// One call frame: the continuation-pointer slot plus the base of its Y slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Frame {
    /// The continuation this frame returns to.
    pub(crate) continuation: Continuation,
    /// Index into the machine stack of the frame's first Y slot.
    pub(crate) y_base: usize,
    /// The number of Y slots this frame owns (Y slots `y_base .. y_base + y_count`).
    pub(crate) y_count: u32,
}

impl Machine {
    /// Read Y slot `k` of the **current** frame, or `None` if there is no frame or
    /// `k` is outside the frame's Y slots.
    ///
    /// Public because a suspended frame is a host-visible fact: the runtime recovers
    /// a `receive … after T` deadline whose `T` the compiler kept in a **Y register**
    /// (a runtime variable) through this accessor — see
    /// [`Machine::wait_deadline`], which reads a parked `wait_timeout`'s
    /// [`Operand::Y(n)`](beam_loader::Operand::Y). Nothing here writes the frame, so
    /// reading is always safe for a host holding the machine between slices.
    #[inline]
    pub fn y(&self, k: u32) -> Option<Eterm> {
        let frame = self.frames.last()?;
        if k >= frame.y_count {
            return None;
        }
        self.stack.get(frame.y_base + k as usize).copied()
    }

    /// Mutable access to Y slot `k` of the **current** frame, under exactly
    /// [`Machine::y`]'s bound (no frame, or `k` outside the frame's Y slots ⇒
    /// `None`).
    ///
    /// The write half of the pair a host needs to *relocate* a slot it read — a
    /// host that moves a term while a process is suspended updates the frame
    /// through here rather than through the raw stack, so the bound check stays in
    /// one place (see [`Machine::stack_mut`] for the whole-stack form).
    #[inline]
    pub fn y_mut(&mut self, k: u32) -> Option<&mut Eterm> {
        let frame = self.frames.last()?;
        if k >= frame.y_count {
            return None;
        }
        self.stack.get_mut(frame.y_base + k as usize)
    }

    /// Write `v` into Y slot `k` of the current frame.
    ///
    /// Returns [`crate::error::bad_register`] if there is no frame or `k` is
    /// outside the frame's Y slots.
    ///
    /// `#[inline(always)]`-marked: it is the store half of `write_operand` (itself
    /// always-inlined) and, with only `#[inline]`, stayed out of line (measured: it
    /// was a ~2.8% self-time symbol on the BIF-free list walk, which stores one Y
    /// slot per element).
    #[inline(always)]
    pub(crate) fn set_y(&mut self, k: u32, v: Eterm) -> Result<()> {
        let (y_base, y_count) = match self.frames.last() {
            Some(frame) => (frame.y_base, frame.y_count),
            None => return Err(error::bad_register(k)),
        };
        if k >= y_count {
            return Err(error::bad_register(k));
        }
        match self.stack.get_mut(y_base + k as usize) {
            Some(slot) => {
                *slot = v;
                Ok(())
            }
            None => Err(error::bad_register(k)),
        }
    }

    /// The number of call frames currently on the frame stack.
    #[inline]
    pub fn frame_depth(&self) -> usize {
        self.frames.len()
    }

    /// The number of words currently on the Erlang stack.
    #[inline]
    pub fn stack_depth(&self) -> usize {
        self.stack.len()
    }

    /// The current frame's continuation.
    #[inline]
    pub(crate) fn continuation(&self) -> Result<Continuation> {
        match self.frames.last() {
            Some(frame) => Ok(frame.continuation),
            None => Err(error::unsupported("run: no current frame")),
        }
    }

    /// Overwrite the current frame's continuation-pointer slot.
    #[inline]
    pub(crate) fn set_continuation(&mut self, continuation: Continuation) -> Result<()> {
        match self.frames.last_mut() {
            Some(frame) => {
                frame.continuation = continuation;
                Ok(())
            }
            None => Err(error::unsupported("run: no current frame")),
        }
    }

    /// The halted frame's result: `x(0)`.
    #[inline]
    pub(crate) fn finish(&self) -> Result<Eterm> {
        self.x(0).ok_or_else(|| error::bad_register(0))
    }

    /// Push a frame reserving `y_count` Y slots (above the continuation-pointer slot).
    ///
    /// A zero-Y frame (the bottom frame `[`Machine::run_label_outcome`] pushes, and
    /// every frame-less function entry) skips the stack `resize` entirely: the
    /// stack length is already the frame's `y_base`, so there is nothing to write.
    #[inline]
    pub(crate) fn push_frame(&mut self, y_count: u32) -> Result<()> {
        let y_base = self.stack.len();
        if y_count != 0 {
            self.stack
                .resize(y_base + y_count as usize, Eterm::default());
        }
        self.frames.push(Frame {
            continuation: Continuation::Halt,
            y_base,
            y_count,
        });
        Ok(())
    }

    /// Pop the innermost frame (and its Y slots).
    #[inline]
    pub(crate) fn pop_frame(&mut self) -> Result<()> {
        match self.frames.pop() {
            Some(frame) => {
                self.stack.truncate(frame.y_base);
                Ok(())
            }
            None => Err(error::unsupported("pop_frame: no frame")),
        }
    }

    /// Pop the innermost frame after checking that it owns exactly `n` Y slots.
    ///
    /// Used by `deallocate`/`call_last`/`call_ext_last`; a mismatched size is a
    /// typed error, never a silent stack corruption.
    #[inline]
    pub(crate) fn pop_frame_checked(&mut self, n: u32) -> Result<()> {
        let y_count = match self.frames.last() {
            Some(frame) => frame.y_count,
            None => return Err(error::unsupported("deallocate: no frame")),
        };
        if y_count != n {
            return Err(error::unsupported("deallocate: frame size mismatch"));
        }
        self.pop_frame()
    }

    /// `trim N` — shrink the current frame to its **highest** Y slots.
    ///
    /// Reproduces C's `i_trim` (`../../erts/emulator/beam/emu/instrs.tab:593`):
    /// `E += N`, which moves the frame's base *up* by `N` and therefore
    /// **discards the lowest `N` Y slots**, renumbering the survivors
    /// (`y(k) → y(k - N)`). The compiler agrees: `beam_trim.erl` treats
    /// `trim N` as eliminating `y(0) .. y(N-1)` and remaps the rest by `-N`
    /// (`Eliminated = [{y,N} || N <- seq(0, Trim-1)]`).
    ///
    /// The frame keeps `y_count - N` slots and its `y_base` is unchanged; only the
    /// innermost frame's slots (the tail of the stack) are affected.
    #[inline]
    pub(crate) fn trim(&mut self, n: u32) -> Result<()> {
        let n_usize = n as usize;
        let (y_base, y_count) = match self.frames.last() {
            Some(frame) => (frame.y_base, frame.y_count),
            None => return Err(error::unsupported("trim: no frame")),
        };
        if n > y_count {
            return Err(error::unsupported("trim: n exceeds frame size"));
        }
        let keep = y_count as usize - n_usize;
        if n_usize > self.stack.len() {
            return Err(error::unsupported("trim: stack underflow"));
        }
        // Drop the lowest `n` Y slots and shift the survivors down so `y(k)`
        // addresses `y(k + n)` of the pre-trim frame.
        if n_usize > 0 {
            self.stack
                .copy_within(y_base + n_usize..y_base + y_count as usize, y_base);
        }
        self.stack.truncate(y_base + keep);
        if let Some(frame) = self.frames.last_mut() {
            frame.y_count = keep as u32;
        }
        Ok(())
    }

    /// Ensure `heap_need` heap words are available, handing the context the exact
    /// root set (`x[0..live)` plus the whole stack) so a Stage-4 collector can move
    /// terms in place.
    #[inline]
    pub(crate) fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<()> {
        let (x, stack) = (&mut self.x, &mut self.stack);
        ctx.ensure_heap(heap_need, live, x, stack)
    }

    /// The `(function index, code offset, module context)` of every live call frame
    /// that is currently **suspended in a call**, innermost frame first.
    ///
    /// A frame's continuation-pointer slot holds the address its function resumes
    /// at once the call it is waiting for returns ([`Instr::Call`](beam_loader::Instr)
    /// writes it through [`Machine::set_continuation`]), so this sequence is the
    /// engine's replacement for the C runtime's **saved raw stacktrace**: the chain
    /// of callers, innermost first — the same order C's `build_stacktrace` emits
    /// (`../../erts/emulator/beam/beam_common.c:1274-1288`). The context recorded
    /// with each address is the module the `(func, pc)` belongs to (`0` = the run's
    /// entry module, otherwise a [`ModuleContext`](super::ModuleContext) id), which
    /// is what lets a caller resolve each frame against its **own** module instead
    /// of naming it in the running one (a caller frame reached through a cross-module
    /// `call_ext` carries an address in *another* module).
    ///
    /// A frame sitting at [`Continuation::Halt`] contributes nothing: that is the
    /// bottom frame (pushed by [`Machine::run_label`]) and any frame whose function
    /// has **not** yet made a call — in particular the frame of the function that
    /// is executing right now, whose own location the dispatch loop already knows
    /// and supplies itself (see `crate::families::exceptions`).
    ///
    /// Read-only introspection (no state is changed); the callers are the
    /// Milestone-1 stacktrace builder and the module-context-aware
    /// [`Machine::current_stacktrace`](crate::machine::Machine::current_stacktrace).
    pub fn resumption_contexts(&self) -> Vec<(u32, u32, u32)> {
        self.frames
            .iter()
            .rev()
            .filter_map(|frame| match frame.continuation {
                Continuation::Address { func, pc, context } => Some((func, pc, context)),
                Continuation::Halt => None,
            })
            .collect()
    }

    /// The `(function index, code offset)` of every live call frame suspended in a
    /// call, innermost frame first — [`Machine::resumption_contexts`] with the module
    /// context dropped.
    ///
    /// Resolving every address this returns against a **single** module is only
    /// correct when they all belong to it; a caller looking at a chain that may have
    /// crossed a cross-module `call_ext` must use [`Machine::resumption_contexts`]
    /// and resolve each frame in the module its context names.
    pub fn resumption_addresses(&self) -> Vec<(u32, u32)> {
        self.resumption_contexts()
            .into_iter()
            .map(|(func, pc, _context)| (func, pc))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{Continuation, Machine};
    use beam_common::Result;
    use beam_term::Eterm;

    use crate::context::ExecutionContext;

    #[test]
    fn push_and_pop_frames_track_the_stack() {
        let mut machine = Machine::new();
        assert!(machine.push_frame(2).is_ok());
        assert_eq!(machine.frame_depth(), 1);
        assert_eq!(machine.stack_depth(), 2);
        assert!(machine.push_frame(1).is_ok());
        assert_eq!(machine.frame_depth(), 2);
        assert_eq!(machine.stack_depth(), 3);
        assert!(machine.pop_frame().is_ok());
        assert_eq!(machine.frame_depth(), 1);
        assert_eq!(machine.stack_depth(), 2);
        // Popping the last frame empties the stack.
        assert!(machine.pop_frame().is_ok());
        assert_eq!(machine.frame_depth(), 0);
        assert_eq!(machine.stack_depth(), 0);
        // Popping past the bottom is a typed error, not a panic.
        assert!(machine.pop_frame().is_err());
    }

    #[test]
    fn y_slots_are_scoped_to_the_current_frame() {
        let mut machine = Machine::new();
        machine.push_frame(2).unwrap();
        machine.set_y(0, Eterm::from_raw(0xAA)).unwrap();
        machine.set_y(1, Eterm::from_raw(0xBB)).unwrap();
        assert_eq!(machine.y(0), Some(Eterm::from_raw(0xAA)));
        assert_eq!(machine.y(1), Some(Eterm::from_raw(0xBB)));
        // Out of the frame's Y slots.
        assert_eq!(machine.y(2), None);
        assert!(machine.set_y(2, Eterm::default()).is_err());

        // A second frame sees only its own slots.
        machine.push_frame(1).unwrap();
        assert_eq!(machine.y(0), Some(Eterm::default()));
        assert_eq!(machine.y(1), None);
        machine.set_y(0, Eterm::from_raw(0xCC)).unwrap();
        machine.pop_frame().unwrap();
        assert_eq!(machine.y(0), Some(Eterm::from_raw(0xAA)));
    }

    #[test]
    fn y_without_a_frame_is_none() {
        let machine = Machine::new();
        assert_eq!(machine.y(0), None);
    }

    #[test]
    fn pop_frame_checked_validates_the_size() {
        let mut machine = Machine::new();
        machine.push_frame(3).unwrap();
        assert!(machine.pop_frame_checked(2).is_err());
        assert_eq!(machine.frame_depth(), 1);
        assert!(machine.pop_frame_checked(3).is_ok());
        assert_eq!(machine.frame_depth(), 0);
        assert!(machine.pop_frame_checked(0).is_err());
    }

    #[test]
    fn trim_discards_the_lowest_y_slots_and_renumbers() {
        // C's `i_trim` does `E += N`: the frame base moves *up*, so the lowest
        // `N` Y slots are discarded and the survivors are renumbered down
        // (`y(k) -> y(k - N)`). `beam_trim.erl` eliminates `y(0)..y(N-1)`.
        let mut machine = Machine::new();
        machine.push_frame(4).unwrap();
        machine.set_y(0, Eterm::from_raw(1)).unwrap();
        machine.set_y(2, Eterm::from_raw(3)).unwrap();
        machine.set_y(3, Eterm::from_raw(4)).unwrap();
        machine.trim(2).unwrap();
        assert_eq!(machine.stack_depth(), 2);
        // Old y2 (`3`) is now y0; old y3 (`4`) is now y1; old y0/y1 are gone.
        assert_eq!(machine.y(0), Some(Eterm::from_raw(3)));
        assert_eq!(machine.y(1), Some(Eterm::from_raw(4)));
        assert_eq!(machine.y(2), None);
        assert!(machine.trim(3).is_err());
    }

    #[test]
    fn ensure_heap_forwards_the_root_set() {
        struct Probe(usize);
        impl ExecutionContext for Probe {
            fn call_bif(&mut self, _m: &str, _f: &str, _a: u32, _args: &[Eterm]) -> Result<Eterm> {
                Err(beam_common::BeamError::BadArg)
            }
            fn ensure_heap(
                &mut self,
                _need: u32,
                live: u32,
                x_regs: &mut [Eterm],
                stack: &mut [Eterm],
            ) -> Result<()> {
                self.0 = live as usize + x_regs.len() + stack.len();
                Ok(())
            }
        }
        let mut machine = Machine::with_max_register(8);
        machine.push_frame(2).unwrap();
        let mut ctx = Probe(0);
        machine.ensure_heap(16, 3, &mut ctx).unwrap();
        assert_eq!(ctx.0, 3 + 8 + 2);
    }

    #[test]
    fn resumption_addresses_walk_the_callers_innermost_first() {
        let mut machine = Machine::new();
        // A frame-less machine has nothing suspended in a call.
        assert!(machine.resumption_addresses().is_empty());

        // The bottom frame halts, so it contributes nothing.
        machine.push_frame(0).unwrap();
        assert!(machine.resumption_addresses().is_empty());

        // F (frame 1) called G: F resumes at (7, 3) once G returns.
        machine.push_frame(0).unwrap();
        machine
            .set_continuation(Continuation::Address { func: 7, pc: 3, context: 0 })
            .unwrap();
        assert_eq!(machine.resumption_addresses(), vec![(7, 3)]);

        // G (frame 2) — the frame of the function executing right now — has not
        // made a call yet, so it contributes nothing and the caller stays first.
        machine.push_frame(1).unwrap();
        assert_eq!(machine.resumption_addresses(), vec![(7, 3)]);

        // G then called H: the caller chain is now H's caller (G at (9, 1)) then
        // G's caller (F at (7, 3)) — innermost first.
        machine
            .set_continuation(Continuation::Address { func: 9, pc: 1, context: 0 })
            .unwrap();
        assert_eq!(machine.resumption_addresses(), vec![(9, 1), (7, 3)]);

        // Popping the innermost frame drops its entry.
        machine.pop_frame().unwrap();
        assert_eq!(machine.resumption_addresses(), vec![(7, 3)]);
    }
}
