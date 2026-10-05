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
use beam_term::prelude::Eterm;

/// The number of x-registers (`erl_vm.h` `MAX_REG`).
pub const MAX_REG: usize = 1024;

/// The maximum number of arguments (`erl_vm.h` `MAX_ARG`).
pub const MAX_ARG: usize = 255;

/// The x-register file: `MAX_REG` registers plus the live prefix length.
///
/// Only `regs[0..live]` holds live BEAM values; the rest is undefined scratch and is not
/// part of the root set. Callers box this value (it is a full 8 KiB on 64-bit).
//
// TODO(impl): implement `XRegs` (all bodies are stubs).
pub struct XRegs {
    regs: [Eterm; MAX_REG],
    live: usize,
}

impl XRegs {
    /// A register file initialised to all-`NIL` with nothing live.
    pub fn new() -> XRegs {
        unimplemented!("stub — to be implemented")
    }

    /// The number of live registers (`x[0..live)`).
    pub fn live(&self) -> usize {
        unimplemented!("stub — to be implemented")
    }

    /// Sets the live prefix length, rejecting anything above [`MAX_REG`] with `badarg`.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `n > MAX_REG`.
    pub fn set_live(&mut self, n: usize) -> Result<()> {
        unimplemented!("stub — to be implemented")
    }

    /// Reads register `i`, requiring it to be inside the live prefix.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `i` is not a live register index.
    pub fn get(&self, i: usize) -> Result<Eterm> {
        unimplemented!("stub — to be implemented")
    }

    /// Writes register `i`, requiring it to be inside the live prefix.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when `i` is not a live register index.
    pub fn set(&mut self, i: usize, v: Eterm) -> Result<()> {
        unimplemented!("stub — to be implemented")
    }

    /// The whole register file (`x[0..MAX_REG]`).
    pub fn as_slice(&self) -> &[Eterm] {
        unimplemented!("stub — to be implemented")
    }

    /// The live registers (`x[0..live]`).
    pub fn live_slice(&self) -> &[Eterm] {
        unimplemented!("stub — to be implemented")
    }

    /// The live registers (`x[0..live]`), mutably.
    pub fn live_slice_mut(&mut self) -> &mut [Eterm] {
        unimplemented!("stub — to be implemented")
    }
}

/// The Erlang process stack (the `E`/`S` region of the legacy PCB).
//
// TODO(impl): implement `Stack` (all bodies are stubs).
pub struct Stack {
    words: Vec<Eterm>,
}

impl Stack {
    /// An empty stack.
    pub fn new() -> Stack {
        unimplemented!("stub — to be implemented")
    }

    /// An empty stack with room for `n` words.
    pub fn with_capacity(n: usize) -> Stack {
        unimplemented!("stub — to be implemented")
    }

    /// The number of words on the stack.
    pub fn len(&self) -> usize {
        unimplemented!("stub — to be implemented")
    }

    /// Whether the stack holds no words.
    pub fn is_empty(&self) -> bool {
        unimplemented!("stub — to be implemented")
    }

    /// The stack words, bottom first.
    pub fn as_slice(&self) -> &[Eterm] {
        unimplemented!("stub — to be implemented")
    }

    /// The stack words, mutably.
    pub fn as_mut_slice(&mut self) -> &mut [Eterm] {
        unimplemented!("stub — to be implemented")
    }

    /// Pushes `word` on top of the stack.
    pub fn push(&mut self, word: Eterm) {
        unimplemented!("stub — to be implemented")
    }

    /// Pops the top word, or fails with `term not found` when the stack is empty.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::TermNotFound` when the stack is empty.
    pub fn pop(&mut self) -> Result<Eterm> {
        unimplemented!("stub — to be implemented")
    }

    /// Drops every stack word.
    pub fn clear(&mut self) {
        unimplemented!("stub — to be implemented")
    }

    /// Keeps only the bottom `n` words.
    pub fn truncate(&mut self, n: usize) {
        unimplemented!("stub — to be implemented")
    }
}

/// The complete root-set container: the x-register file plus the Erlang stack.
//
// TODO(impl): implement `Registers` (all bodies are stubs).
pub struct Registers {
    x: XRegs,
    stack: Stack,
}

impl Registers {
    /// Empty registers: all-`NIL` x-registers, nothing live, an empty stack.
    pub fn new() -> Registers {
        unimplemented!("stub — to be implemented")
    }

    /// The x-register file.
    pub fn x(&self) -> &XRegs {
        unimplemented!("stub — to be implemented")
    }

    /// The x-register file, mutably.
    pub fn x_mut(&mut self) -> &mut XRegs {
        unimplemented!("stub — to be implemented")
    }

    /// The Erlang stack.
    pub fn stack(&self) -> &Stack {
        unimplemented!("stub — to be implemented")
    }

    /// The Erlang stack, mutably.
    pub fn stack_mut(&mut self) -> &mut Stack {
        unimplemented!("stub — to be implemented")
    }

    /// Appends the root words to `out` — `x[0..live)` first, then every stack word — and
    /// returns the split point (`x_live`), the number of leading x-register words written.
    pub fn gather_roots(&self, out: &mut Vec<Eterm>) -> usize {
        unimplemented!("stub — to be implemented")
    }

    /// Writes the (relocated) root words back: the first `x_live` into `x[0..live)` and the
    /// remainder into the stack, exactly reversing [`Registers::gather_roots`].
    pub fn scatter_roots(&mut self, roots: &[Eterm], x_live: usize) {
        unimplemented!("stub — to be implemented")
    }
}
