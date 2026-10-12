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

//! # `beam_rt` — Rust ERTS runtime binary (boot · CLI · `system_info` · runtime glue)
//!
//! The top of the workspace: the `beam_rt` executable, the boot sequence, the preloaded-module
//! registry skeleton, `system_info/1`, the CLI flag surface (contracts Part I §A.6/A.7/A.8) and the
//! concrete [`ExecutionContext`](beam_engine::ExecutionContext) glue that turns the Stage-1
//! interpreter into a runnable runtime. See `CONTEXT.md` for the frozen API surface.
//!
//! # Layout
//!
//! * [`runtime`] — the [`Runtime`] façade: boot-wiring, the code server and the **vertical slice**
//!   ([`Runtime::run_function`]/[`Runtime::run_module`]) that loads a real `.beam` and runs it.
//! * [`codeserver`] — [`CodeServer`]: the **one shared** atom table, the module cache and the
//!   ordered code path (the multi-module host's foundation).
//! * [`context`] — [`RuntimeContext`], the concrete `ExecutionContext` over the real BIF registry.
//! * [`config`] — the parsed [`RuntimeConfig`], the init arguments ([`InitArgs`]) and the selected
//!   [`Action`].
//! * [`cli`] — the A.6 parser ([`cli::parse`]), the usage text and the [`cli::run`] driver.
//! * [`boot`] — the preloaded-module registry skeleton (A.7), the atom seed and the version banner.
//! * [`info`] — `system_info/1` (A.8) as a heap-free [`InfoValue`] plus its materialization.
//! * `mailbox` — the per-process receive scan state (`MailboxState`: the mailbox cursor
//!   plus the `recv_marker_*` table) that [`Runtime`] keys by pid and
//!   [`RuntimeContext`] drives the receive seam through.
//! * `runner` — the `beam_process::ProcessRunner` behind the scheduler: the engine state
//!   and recorded outcomes of every process [`Runtime::spawn_process`] created.
//! * `timers` — the runtime's **pending timer deadlines** (`erlang:send_after/3`), a
//!   min-heap advanced only by the scheduler loop (`run_scheduler`/`pump_scheduler`).
//! * [`eval`] — the `-eval` Erlang expression parser and `erl_eval`-style evaluator.
//! * [`format`] — term printing, which drives the registered `io:format/2` `~p`/`~w` printer.
//! * [`native`] — the runtime-native BIF floor (`halt`, `display`, `io:put_chars`,
//!   `erlang:apply`, `erlang:system_info`, `init:get_argument`) consulted before the registry.
//! * [`dist`] — the **distributed node** (contract A.5, Phase 1): the `epmd`
//!   registration, the bound distribution listener and the accepting half of the
//!   `'N'` handshake, pumped from [`Runtime`]'s scheduler pass.
//!
//! # Contracts
//!
//! * **Errors are return-based** (workspace `CONTEXT.md` §3): every fallible API returns
//!   [`beam_common::Result`]; there is no crate-local error enum and nothing is swallowed.
//! * **`unsafe`-free by default**: the crate is `#![deny(unsafe_code)]` and the glue is built on the
//!   safe peripheries of the lower crates. Two narrowly-scoped `#[allow(unsafe_code)]` overrides exist:
//!   `context/gc_walk.rs`'s sanctioned `beam_term::layout` object-walk, and `nif.rs`'s `enif_compare`/
//!   `enif_hash` `unsafe extern "C"` calls.
//! * **Call down, never duplicate**: BIFs come from `beam_bifs`, terms/heaps from `beam_term`/
//!   `beam_alloc`, pids from `beam_process`, modules from `beam_loader` and execution from
//!   `beam_engine`. The printer is the registered `io:format/2` BIF, not a second printer.
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod boot;
pub mod cli;
pub mod codeserver;
pub mod config;
pub mod context;
pub mod crash_dump;
pub mod dist;
pub mod error;
pub mod eval;
pub mod format;
pub mod info;
mod mailbox;
mod native;
mod nif;
pub mod perf_map;
pub mod port;
pub mod runtime;
mod runner;
pub mod seq_trace;
mod timers;
pub mod trace;
pub use crate::boot::{boot_banner, PreloadedModules};
pub use crate::cli::{parse, run, EXIT_OK, EXIT_RUNTIME, EXIT_UNSUPPORTED, EXIT_USAGE, USAGE};
pub use crate::codeserver::CodeServer;
pub use crate::config::{
    Action, ArgTerm, EmulatorConfig, InitArgs, RuntimeConfig, DEFAULT_HEAP_WORDS,
};
pub use crate::context::RuntimeContext;
pub use crate::eval::eval;
pub use crate::format::{term_to_string, term_to_string_w};
pub use crate::info::{system_info_value, InfoValue, SUPPORTED_KEYS};
pub use crate::port::{PortName, PortRegistry, RuntimePorts, StreamType, PORT_INFO_ATOMS};
pub use crate::dist::{DistConnection, DistNode, NodeName};
pub use crate::runtime::Runtime;
/// The conventional glob-import surface for downstream crates.
///
/// `use beam_rt::prelude::*;` brings the runtime façade ([`Runtime`]), its code server
/// ([`CodeServer`]), the configuration ([`RuntimeConfig`]/[`Action`]/[`InitArgs`]) and the CLI entry
/// points into scope.
pub mod prelude {
    pub use crate::boot::{boot_banner, PreloadedModules};
    pub use crate::cli::{parse, run, USAGE};
    pub use crate::codeserver::CodeServer;
    pub use crate::config::{Action, ArgTerm, EmulatorConfig, InitArgs, RuntimeConfig};
    pub use crate::context::RuntimeContext;
    pub use crate::dist::{DistConnection, DistNode, NodeName};
    pub use crate::eval::eval;
    pub use crate::format::{term_to_string, term_to_string_w};
    pub use crate::info::{system_info_value, InfoValue};
    pub use crate::port::{PortRegistry, RuntimePorts};
    pub use crate::runtime::Runtime;}
