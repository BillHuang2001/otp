//! # `beam_engine` — typed-IR execution engine (interpreter + JIT tiers)
//!
//! Executes the **frozen typed, block-structured IR** produced by `beam_loader`: a
//! **direct-threaded interpreter** over the Milestone-1 opcode subset, plus the
//! **optional JIT tiers** (`jit_beamasm`, `jit_cranelift`) that are
//! *measurement-gated* and kept **off the critical path** (not implemented in M1).
//! Implements the Stage-1 engine half of Part II §H with the IR contract of §E and
//! the interpreter/JIT trade-offs of §F (`../../CONTEXT.md` §4).
//!
//! This crate is on the **critical path of the vertical slice**: it loads a real
//! `.beam` (via `beam_loader`) and runs it end to end.
//!
//! The engine depends only on `beam_loader` (+ `beam_term`/`beam_common`) — never
//! on the Stage-5 runtime core. Everything the runtime must supply (BIF dispatch,
//! heap/GC, process/stack limits) is abstracted behind the
//! [`ExecutionContext`] trait; `beam_rt` provides the concrete implementation.
//! This keeps the Stage-1/Stage-5 DAG acyclic and lets the engine be tested with a
//! minimal context.
//!
//! ## Status
//!
//! The crate is **implemented** for its Milestone-1 scope: the interpreter
//! dispatch loop and the operand read/write bodies are filled in, [`Machine`]
//! runs a real `.beam` (via `beam_loader`) end to end, and the vertical-slice
//! integration test exercises it. The documented coverage gaps (floats, literals,
//! funs, maps, bitstrings, in-VM exceptions, GC) are unchanged from the design in
//! this crate's `CONTEXT.md` and each returns a typed [`beam_common::BeamError`].
//!
//! The crate is `#![deny(unsafe_code)]`; the **only** exception is the private
//! `predicates` module, which wraps the `beam_term` boxed-term predicates
//! (`unsafe fn` because they dereference a header word) behind SAFETY-annotated
//! helpers.
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod context;
pub mod error;
pub mod interpreter;
pub mod jit;
pub mod machine;
pub mod operand;
#[allow(unsafe_code)]
mod predicates;
pub use context::{ExecutionContext, MinimalContext};
pub use error::{bad_register, unknown_label, unsupported};
pub use jit::{available_tiers, JitBackend};
pub use machine::{Machine, MAX_REGISTER};

/// The conventional glob-import surface for downstream crates.
///
/// `use beam_engine::prelude::*;` brings the [`ExecutionContext`] seam and the
/// [`Machine`] core (with its register-file bound [`MAX_REGISTER`]) into scope.
pub mod prelude {
    pub use crate::context::{ExecutionContext, MinimalContext};
    pub use crate::machine::{Machine, MAX_REGISTER};
}
