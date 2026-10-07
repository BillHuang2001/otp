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
//! * [`context`] — [`BifContext`] and the optional
//!   `output`/`sender`/`tables`/`fun_caller`/`port` seams.
//! * [`error`] — the `badarg`/`badarith`/`badarity`/`system_limit` constructors and
//!   the public `error`/`exit`/`throw` raise builders.
//! * `access` — the crate's **only** `unsafe` module: safe wrappers over
//!   `beam_term`'s raw (header-dereferencing) term accessors.
//! * the BIF modules (`arith`, `bool`, `compare`, `type_test`, `term_ops`, `proc`,
//!   `port`, `reason`, `nif`, `io`) — the Milestone-1 core BIF list, each
//!   registering its implementations through its own `register` entry point.
//! * [`ets`] — the [`TableRegistry`] bridge over `beam_ets::Table`; the table handle
//!   is an on-heap `Ref` thing (the M1 substitution for a real `tid()`).

#![deny(unsafe_code)]

mod access;
mod arith;
mod binary;
mod binconv;
mod bits;
mod bool;
mod compare;
mod context;
mod convert;
mod dict;
mod error;
mod ets;
mod io;
mod lists;
mod maps;
mod math;
mod nif;
mod numeric;
mod phash;
mod port;
mod print;
mod proc;
mod reason;
mod records;
mod registry;
mod string;
mod term_ops;
mod time;
mod type_test;
mod unicode;

pub use crate::context::{
    AtomInterner, BifContext, BytesSink, DictHook, FunCaller, LocalNode, NifLoadError, NifLoader,
    NullSink, OutputSink, PortHook, RecordDefinitionView, RecordFieldView, RecordSource, SendHook,
    StdoutSink,
};
pub use crate::error::{raise_error, raise_exit, raise_throw};
pub use crate::ets::TableRegistry;
pub use crate::reason::{exit_reason_from_term, reason_term_from_exit_reason};
pub use crate::registry::{default_registry, BifFn, BifRegistry};

/// The conventional glob-import surface for downstream crates.
///
/// `use beam_bifs::prelude::*;` brings the [`BifRegistry`] seam, the
/// [`BifContext`] and its sinks, the raise builders, the reason-term mapping and
/// [`TableRegistry`] into scope.
pub mod prelude {
    pub use crate::{
        default_registry, exit_reason_from_term, raise_error, raise_exit, raise_throw,
        reason_term_from_exit_reason, AtomInterner, BifContext, BifFn, BifRegistry, BytesSink,
        DictHook, FunCaller, LocalNode, NifLoadError, NifLoader, NullSink, OutputSink, PortHook,
        RecordDefinitionView, RecordFieldView, RecordSource, SendHook, StdoutSink, TableRegistry,
    };
}
