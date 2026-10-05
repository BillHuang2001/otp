//! # `beam_bifs` — BIF registry (`bif.tab` surface) and implementations
//!
//! Built-in functions, the `bif.tab` surface of the Rust ERTS rewrite.
//!
//! Implements compatibility contract Part I §A.7 (the OTP app contracts exposed to
//! `erlang`/`erts_internal`/`prim_*`) and the Stage-5 row of Part II §H. The BIF
//! table is source data (`../../erts/emulator/beam/bif.tab`) — regenerated from it,
//! never invented, never hand-patched into generated output.
//!
//! ## Failure is return-based
//!
//! A BIF returns [`beam_common::Result`]: a bad argument is
//! [`BeamError::BadArg`](beam_common::BeamError::BadArg), arithmetic on a
//! non-number is [`BeamError::BadArith`](beam_common::BeamError::BadArith) and an
//! out-of-range size is
//! [`BeamError::SystemLimit`](beam_common::BeamError::SystemLimit). The engine turns
//! these into a BEAM raise (the exception tag in `x(0)`) — there is no
//! `setjmp`/`longjmp` and no silent failure (workspace `CONTEXT.md` §3).
//!
//! ## Layout
//!
//! * [`registry`] — the `(module, function, arity) → impl` seam ([`BifRegistry`],
//!   [`BifFn`]) that the engine/`beam_rt` resolves against.
//! * [`context`] — [`BifContext`] and the optional `output`/`sender`/`tables` seams.
//! * [`error`] — the `badarg`/`badarith`/`badarity`/`system_limit` constructors and
//!   the public `error`/`exit`/`throw` raise builders.
//! * `access` — the crate's **only** `unsafe` module: safe wrappers over
//!   `beam_term`'s raw (header-dereferencing) term accessors.
//! * the BIF modules (`arith`, `compare`, `type_test`, `term_ops`, `proc`,
//!   `reason`, `io`) — implemented in a follow-up step; today each exposes only its
//!   `register` entry point.
//! * [`ets`] — the [`TableRegistry`] bridge over `beam_ets::Table`; the table handle
//!   is an on-heap `Ref` thing (the M1 substitution for a real `tid()`).

#![deny(unsafe_code)]

mod access;
mod arith;
mod compare;
mod context;
mod error;
mod ets;
mod io;
mod print;
mod proc;
mod reason;
mod registry;
mod term_ops;
mod type_test;

pub use crate::context::{BifContext, BytesSink, NullSink, OutputSink, SendHook, StdoutSink};
pub use crate::error::{raise_error, raise_exit, raise_throw};
pub use crate::ets::TableRegistry;
pub use crate::registry::{default_registry, BifFn, BifRegistry};

/// The conventional glob-import surface for downstream crates.
///
/// `use beam_bifs::prelude::*;` brings the [`BifRegistry`] seam, the
/// [`BifContext`] and its sinks, the raise builders and [`TableRegistry`] into
/// scope.
pub mod prelude {
    pub use crate::{
        default_registry, raise_error, raise_exit, raise_throw, BifContext, BifFn, BifRegistry,
        BytesSink, NullSink, OutputSink, SendHook, StdoutSink, TableRegistry,
    };
}
