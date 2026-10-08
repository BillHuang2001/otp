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

//! The root-set containers: the x-register file and the Erlang stack.
//!
//! These are the flat, shape-only containers the crate-layer **root-set contract** names
//! (workspace `../CONTEXT.md` §4/§5): a collection roots `x_reg_array[0..Live)` **plus the
//! whole Erlang stack**, and there are **no frame liveness maps** — every stack word is a
//! root and is scanned. [`Registers::gather_roots`] produces exactly that flat slice and
//! [`Registers::scatter_roots`] writes the reloaded words back after a collection.
//!
//! Legacy C ground truth (read-only): `erts/emulator/beam/erl_vm.h` (`MAX_REG`, `MAX_ARG`)
//! and the `registers`/`E`/`HTOP` accessors of `erl_process.h`.

use beam_common::Result;
use beam_term::prelude::{Eterm, NIL};

use crate::error;

/// The number of x-registers (`erl_vm.h` `MAX_REG`).
pub const MAX_REG: usize = 1024;

/// The maximum number of arguments (`erl_vm.h` `MAX_ARG`).
pub const MAX_ARG: usize = 255;

/// The x-register file: `MAX_REG` registers plus the live prefix length.
///
/// Only `regs[0..live]` holds live BEAM values; the rest is undefined scratch and is not
/// part of the root set. Callers box this value (it is a full 8 KiB on 64-bit).
pub struct XRegs {
    regs: [Eterm; MAX_REG],
    live: usize,
}

impl XRegs {
    /// A register file initialised to all-`NIL` with nothing live.
    pub fn new() -> XRegs {
        XRegs {
            regs: [NIL; MAX_REG],
            live: 0,
        }
    }

    /// The number of live registers (`x[0..live)`).
    pub fn live(&self) -> usize {
        self.live
    }

    /// Sets the live prefix length, rejecting anything above [`MAX_REG`] with `badarg`.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `n > MAX_REG`.
    pub fn set_live(&mut self, n: usize) -> Result<()> {
        if n > MAX_REG {
            return Err(error::bad_pid());
        }
        self.live = n;
        Ok(())
    }

    /// Reads register `i`, requiring it to be inside the live prefix.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `i` is not a live register index.
    pub fn get(&self, i: usize) -> Result<Eterm> {
        if i >= self.live {
            return Err(error::bad_pid());
        }
        Ok(self.regs[i])
    }

    /// Writes register `i`, requiring it to be inside the live prefix.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `i` is not a live register index.
    pub fn set(&mut self, i: usize, v: Eterm) -> Result<()> {
        if i >= self.live {
            return Err(error::bad_pid());
        }
        self.regs[i] = v;
        Ok(())
    }

    /// The whole register file (`x[0..MAX_REG]`).
    pub fn as_slice(&self) -> &[Eterm] {
        &self.regs
    }

    /// The live registers (`x[0..live]`).
    pub fn live_slice(&self) -> &[Eterm] {
        &self.regs[..self.live]
    }

    /// The live registers (`x[0..live]`), mutably.
    pub fn live_slice_mut(&mut self) -> &mut [Eterm] {
        &mut self.regs[..self.live]
    }
}

/// The Erlang process stack (the `E`/`S` region of the legacy PCB).
pub struct Stack {
    words: Vec<Eterm>,
}

impl Stack {
    /// An empty stack.
    pub fn new() -> Stack {
        Stack { words: Vec::new() }
    }

    /// An empty stack with room for `n` words.
    pub fn with_capacity(n: usize) -> Stack {
        Stack {
            words: Vec::with_capacity(n),
        }
    }

    /// The number of words on the stack.
    pub fn len(&self) -> usize {
        self.words.len()
    }

    /// Whether the stack holds no words.
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// The stack words, bottom first.
    pub fn as_slice(&self) -> &[Eterm] {
        &self.words
    }

    /// The stack words, mutably.
    pub fn as_mut_slice(&mut self) -> &mut [Eterm] {
        &mut self.words
    }

    /// Pushes `word` on top of the stack.
    pub fn push(&mut self, word: Eterm) {
        self.words.push(word);
    }

    /// Pops the top word, or fails with `term not found` when the stack is empty.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when the stack is empty.
    pub fn pop(&mut self) -> Result<Eterm> {
        self.words.pop().ok_or_else(error::unknown_process)
    }

    /// Drops every stack word.
    pub fn clear(&mut self) {
        self.words.clear();
    }

    /// Keeps only the bottom `n` words.
    pub fn truncate(&mut self, n: usize) {
        self.words.truncate(n);
    }
}

/// The complete root-set container: the x-register file plus the Erlang stack.
pub struct Registers {
    x: XRegs,
    stack: Stack,
}

impl Registers {
    /// Empty registers: all-`NIL` x-registers, nothing live, an empty stack.
    pub fn new() -> Registers {
        Registers {
            x: XRegs::new(),
            stack: Stack::new(),
        }
    }

    /// The x-register file.
    pub fn x(&self) -> &XRegs {
        &self.x
    }

    /// The x-register file, mutably.
    pub fn x_mut(&mut self) -> &mut XRegs {
        &mut self.x
    }

    /// The Erlang stack.
    pub fn stack(&self) -> &Stack {
        &self.stack
    }

    /// The Erlang stack, mutably.
    pub fn stack_mut(&mut self) -> &mut Stack {
        &mut self.stack
    }

    /// Appends the root words to `out` — `x[0..live)` first, then every stack word — and
    /// returns the split point (`x_live`), the number of leading x-register words written.
    pub fn gather_roots(&self, out: &mut Vec<Eterm>) -> usize {
        let x_live = self.x.live();
        out.extend_from_slice(self.x.live_slice());
        out.extend_from_slice(self.stack.as_slice());
        x_live
    }

    /// Writes the (relocated) root words back: the first `x_live` into `x[0..live)` and the
    /// remainder into the stack, exactly reversing [`Registers::gather_roots`].
    pub fn scatter_roots(&mut self, roots: &[Eterm], x_live: usize) {
        // Defensive clamping: `gather_roots` always reports a split consistent with
        // `roots` and the collector preserves the slice length, so these `min`s are
        // identities in practice — but they keep this infallible method total (no panic)
        // even if a caller hands it a mismatched slice.
        let x_count = x_live.min(self.x.live()).min(roots.len());
        self.x.live_slice_mut()[..x_count].copy_from_slice(&roots[..x_count]);

        let rest = &roots[x_count..];
        let stack_count = rest.len().min(self.stack.len());
        if stack_count > 0 {
            self.stack.as_mut_slice()[..stack_count]
                .copy_from_slice(&rest[..stack_count]);
        }
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the root-set containers: live bounds and the `get`/`set` range
    //! errors, stack push/pop, and the `gather_roots`/`scatter_roots` round-trip
    //! (including the split point).

    use beam_common::BeamError;
    use beam_term::prelude::{make_small, NIL};

    use super::*;

    #[test]
    fn xregs_start_empty_and_enforce_the_live_prefix() {
        let mut x = XRegs::new();
        assert_eq!(x.live(), 0);
        assert_eq!(x.as_slice().len(), MAX_REG);
        assert!(x.as_slice().iter().all(|&w| w == NIL));
        assert!(x.live_slice().is_empty());

        // Nothing is live yet: both a read and a write are out of range.
        assert!(matches!(x.get(0), Err(BeamError::BadArg)));
        assert!(matches!(x.set(0, make_small(1)), Err(BeamError::BadArg)));

        x.set_live(2).expect("2 <= MAX_REG");
        assert_eq!(x.live(), 2);
        assert_eq!(x.live_slice().len(), 2);

        // Index 1 is live, index 2 is not.
        x.set(1, make_small(7)).expect("index 1 is live");
        assert_eq!(x.get(1).expect("index 1 is live"), make_small(7));
        assert!(matches!(x.get(2), Err(BeamError::BadArg)));
        assert!(matches!(x.set(2, NIL), Err(BeamError::BadArg)));

        // Shrinking the live prefix hides the word again (it is still physically there).
        x.set_live(1).expect("1 <= MAX_REG");
        assert!(matches!(x.get(1), Err(BeamError::BadArg)));
    }

    #[test]
    fn set_live_rejects_anything_above_max_reg() {
        let mut x = XRegs::new();
        x.set_live(MAX_REG).expect("exactly MAX_REG is allowed");
        assert_eq!(x.live(), MAX_REG);
        assert!(matches!(x.set_live(MAX_REG + 1), Err(BeamError::BadArg)));
    }

    #[test]
    fn live_slice_mut_sees_only_the_live_prefix() {
        let mut x = XRegs::new();
        x.set_live(3).expect("3 <= MAX_REG");
        x.live_slice_mut()[0] = make_small(11);
        assert_eq!(x.get(0).expect("live"), make_small(11));
        assert_eq!(x.live_slice().len(), 3);
    }

    #[test]
    fn stack_push_pop_and_bounds() {
        let mut stack = Stack::new();
        assert!(stack.is_empty());
        assert_eq!(stack.len(), 0);
        assert!(matches!(stack.pop(), Err(BeamError::TermNotFound)));

        stack.push(make_small(1));
        stack.push(make_small(2));
        assert_eq!(stack.len(), 2);
        assert_eq!(stack.as_slice(), &[make_small(1), make_small(2)]);

        assert_eq!(stack.pop().expect("non-empty"), make_small(2));
        assert_eq!(stack.pop().expect("non-empty"), make_small(1));
        assert!(matches!(stack.pop(), Err(BeamError::TermNotFound)));

        stack.push(make_small(3));
        stack.push(make_small(4));
        stack.truncate(1);
        assert_eq!(stack.as_slice(), &[make_small(3)]);
        stack.clear();
        assert!(stack.is_empty());

        let sized = Stack::with_capacity(16);
        assert!(sized.is_empty());
    }

    #[test]
    fn gather_and_scatter_round_trip_including_the_split_point() {
        let mut regs = Registers::new();
        regs.x_mut().set_live(2).expect("2 <= MAX_REG");
        regs.x_mut().set(0, make_small(10)).expect("live");
        regs.x_mut().set(1, make_small(20)).expect("live");
        regs.stack_mut().push(make_small(30));
        regs.stack_mut().push(make_small(40));

        let mut roots = vec![make_small(-1)]; // caller-owned capacity is preserved
        let x_live = regs.gather_roots(&mut roots);
        assert_eq!(x_live, 2);
        assert_eq!(
            &roots[1..],
            &[make_small(10), make_small(20), make_small(30), make_small(40)]
        );

        // Simulate the collector relocating every root: rewrite the two live x-registers
        // and the two stack words.
        let relocated = [
            make_small(110),
            make_small(210),
            make_small(310),
            make_small(410),
        ];
        roots[1..].copy_from_slice(&relocated);
        regs.scatter_roots(&roots[1..], x_live);

        assert_eq!(regs.x().get(0).expect("live"), make_small(110));
        assert_eq!(regs.x().get(1).expect("live"), make_small(210));
        assert_eq!(
            regs.stack().as_slice(),
            &[make_small(310), make_small(410)]
        );
        // Registers beyond the live prefix are untouched.
        assert!(regs.x().get(2).is_err());
    }

    #[test]
    fn scatter_with_no_stack_words_is_a_register_only_write() {
        let mut regs = Registers::new();
        regs.x_mut().set_live(1).expect("1 <= MAX_REG");
        let mut roots = Vec::new();
        let x_live = regs.gather_roots(&mut roots);
        assert_eq!(roots, vec![NIL]);
        assert_eq!(x_live, 1);

        regs.scatter_roots(&[make_small(99)], x_live);
        assert_eq!(regs.x().get(0).expect("live"), make_small(99));
        assert!(regs.stack().is_empty());
    }
}
