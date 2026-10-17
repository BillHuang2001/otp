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

//! The concrete [`ExecutionContext`] glue: the engine's seam over the runtime core.
//!
//! The Stage-1 engine knows nothing about BIFs, heaps, processes or ETS — it calls
//! `call_bif`/`ensure_heap`/`alloc_words`/`apply_fun` on the trait and lets the host
//! decide (see `../beam_engine/src/context.rs`). This module is the host:
//! [`RuntimeContext`] borrows the [`Runtime`] (its BIF registry, the shared
//! [`AtomTable`](beam_term::AtomTable), the process heap, the calling
//! [`ProcessId`](beam_process::ProcessId), the ETS `TableRegistry` and the output sink) and
//! adapts them onto the engine.
//!
//! It is the **only** place the engine's seam meets the Stage-5 core; that keeps the
//! Stage-1/Stage-5 dependency DAG acyclic (`../CONTEXT.md`).
//!
//! # The submodules
//!
//! One `impl ExecutionContext for RuntimeContext<'_>` (Rust forbids a second one, E0119) is
//! split **by concern** across the submodules below, each trait method delegating to an
//! inherent helper in the sibling that owns the concern.
//!
//! * [`runtime_context`] — the [`RuntimeContext`] type itself: its fields, the constructors and
//!   the small accessors (`new`/`for_process`/`engines`/`pid`/`enter_module`/…), plus the
//!   relocation-exemption window and the parked-machine root re-sync.
//! * [`dispatch`] — the per-context dispatch memo ([`DispatchMemo`]) and the `uses_*` hook
//!   predicates that select the bridge arm.
//! * [`calls`] — the registered-BIF call path ([`RuntimeContext::call_bif`]'s routing half, the
//!   bridge selection and the NIF short-circuit) incl. the module's three dispatch routes and
//!   the shared-atom-table/predicted-index-commit rationale.
//! * [`pool`] — the **table-less** route's BIF-result store: a container result is copied onto
//!   the runtime's long-lived `-eval` store so the pooled BIF heap can be rewound when no
//!   scheduled process is running to re-home it.
//! * [`send`] — the send/deliver family, the mailbox scan surface and the alias-aware send
//!   destination.
//! * [`apply`] — fun application ([`LocalFunTarget`], `local_fun_target`,
//!   `apply_non_local_fun`) and the callee-admission predicates behind the in-machine
//!   cross-module seam.
//! * [`pump`] — the nested-run pump (`run_export`/`run_export_blocking`/`run_label_blocking`
//!   and their bounded deadline).
//! * [`execution`] — the single `impl ExecutionContext`, incl. the process-heap collector
//!   rationale.
//! * [`gc`]/[`gc_walk`] — the heap-admission point and the unscanned-root valve.
//! * [`position`] — the **in-machine** BIF door that answers `erlang:process_info/1,2`'s
//!   execution-position items for the process that is executing the query.
//! * [`bridges`]/[`fun_identity`]/[`memory_hook`]/[`module_info`]/[`node_hash`]/
//!   [`persistent_term`]/[`process_bridge`]/[`runtime_hooks`]/[`signal`]/[`trace`]/[`alias`] —
//!   the host-seam bridges and the runtime-level facilities they wrap.

mod alias;
mod apply;
mod bridges;
mod calls;
mod dispatch;
mod execution;
mod fun_identity;
pub(crate) mod gc;
pub(crate) mod gc_walk;
mod memory_hook;
mod module_info;
mod node_hash;
mod persistent_term;
mod pool;
mod position;
mod process_bridge;
mod pump;
mod runtime_context;
mod runtime_hooks;
mod send;
mod signal;
mod trace;

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests;

pub use self::runtime_context::RuntimeContext;
pub(crate) use self::position::InMachinePosition;
pub(crate) use self::apply::LocalFunTarget;
pub(crate) use self::bridges::{module_md5, PendingInterner};
pub(crate) use self::persistent_term::PersistentTermStore;
use self::dispatch::DispatchMemo;

// `tests.rs` and `pump.rs`'s inline suite reach the items they assert on through `use
// super::*`, so every name they use stays in this module's namespace for the test build.
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use std::time::{Duration, Instant};

#[cfg(test)]
use beam_common::{BeamError, Result};
#[cfg(test)]
use beam_engine::context::RootSet;
#[cfg(test)]
use beam_engine::{terms, ExecutionContext, Machine};
#[cfg(test)]
use beam_loader::{Function, ImportRef, Instr, Label, Module};
#[cfg(test)]
use beam_process::{MonitorRef, ProcessId, ProcessTable};
#[cfg(test)]
use beam_term::Eterm;

#[cfg(test)]
use crate::runtime::Runtime;

#[cfg(test)]
use self::apply::{
    call_site_reading_imports, callee_observes_its_call_site, function_calls_a_fun_image_bif,
    CallSiteReadingImports,
};
#[cfg(test)]
use self::bridges::NodeIdentity;
#[cfg(test)]
use self::dispatch::{dispatch_slot, DispatchHooks, DISPATCH_MEMO_SLOTS};
#[cfg(test)]
use self::fun_identity::FunIdentityBridge;
#[cfg(test)]
use self::pump::{pump_until_returned, BLOCKING_PUMP_DEADLINE};
#[cfg(test)]
use self::runtime_hooks::{declared_bif_tab, is_builtin};
