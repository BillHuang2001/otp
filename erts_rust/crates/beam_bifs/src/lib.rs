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
//! * [`bif_tab`] — the parsed `bif.tab` surface: the real table's grammar, and the
//!   [`Census`](bif_tab::Census) of registry-vs-declared coverage. Validation only —
//!   dispatch is unchanged.
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
//! * [`hook_sets`] — the curated "hook-needed" BIF sets ([`MEMORY_HOOK_BIFS`],
//!   [`LOCAL_FUN_IDENTITY_BIFS`] and their allocation-free predicates), so a host
//!   gates the optional `MemoryHook`/`LocalFunIdentity` seams off the authoritative
//!   list instead of a privately re-derived copy.

#![deny(unsafe_code)]

mod access;
mod arith;
pub mod bif_tab;
mod binary;
mod binconv;
mod bits;
mod bool;
mod calendar;
mod checksum;
mod compare;
mod context;
mod convert;
mod dict;
mod error;
mod erts_internal;
mod ets;
mod fun_info;
pub mod hook_sets;
mod io;
mod lists;
mod maps;
mod match_spec;
mod math;
mod modinfo;
mod nif;
mod numeric;
mod os_bif;
mod packet;
mod persistent_term;
mod phash;
mod port;
mod print;
mod proc;
mod proc_reg;
mod reason;
mod records;
mod registry;
mod runtime;
mod string;
mod term_conv;
mod term_ops;
mod time;
mod type_test;
mod unicode;

pub use crate::context::{
    AtomInterner, BifContext, BytesSink, DictHook, FunCaller, LocalFunIdentity, LocalNode,
    MemoryHook, ModuleInfo, NifLoadError, NifLoader, NullSink, OutputSink, PersistentTerm,
    PortHook, ProcessHook, ProcessInfoItem, RecordDefinitionView, RecordFieldView, RecordSource,
    RuntimeHooks, SendHook, StdoutSink,
};
pub use crate::error::{raise_error, raise_exit, raise_throw};
pub use crate::ets::TableRegistry;
pub use crate::hook_sets::{
    needs_local_fun_identity, needs_memory_hook, LOCAL_FUN_IDENTITY_BIFS, MEMORY_HOOK_BIFS,
};
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
        DictHook, FunCaller, LocalFunIdentity, LocalNode, MemoryHook, ModuleInfo, NifLoadError,
        NifLoader, NullSink, OutputSink, PersistentTerm, PortHook, ProcessHook, ProcessInfoItem,
        RecordDefinitionView, RecordFieldView, RecordSource, RuntimeHooks, SendHook, StdoutSink,
        TableRegistry, LOCAL_FUN_IDENTITY_BIFS, MEMORY_HOOK_BIFS, needs_local_fun_identity,
        needs_memory_hook,
    };
}
