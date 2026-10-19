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

//! The host half of `enif_send` (contract A.3): what [`NifHostImpl`]'s [`NifHost::send`] hook
//! resolves before it reaches the runtime.
//!
//! The C function's information lives in two places, and this module is the join:
//!
//! 1. **who is sending.** The ABI hands the hook `from: Option<Eterm>` — the pid owning the
//!    `msg_env` the message terms belong to, or `None` for the calling process
//!    ([`NifHost::send`]). The C's sender is always the calling process (`from = c_p ?
//!    c_p->common.id : am_undefined`, `erl_nif.c:1006`), and this host always has that pid, so
//!    `None` resolves to it. The pid is then turned into a [`ProcessId`] — the identity the
//!    runtime's send seam takes.
//! 2. **the delivery.** [`NifHostServices::send`] carries it to the runtime; the concrete provider
//!    ([`RuntimeNifServices`]) runs it through the runtime's own send seam
//!    ([`crate::context::nif_send::deliver`]), which copies the message into the receiver's heap,
//!    enqueues it and wakes a receiver blocked in `receive`.
//!
//! # The `msg_env` argument needs no handling
//!
//! Every message env this host allocates ([`NifHost::alloc_env`]) is a live
//! [`NifEnv`](beam_abi::NifEnv) over **this same host**, so [`NifHost::make_copy`] is the identity
//! and a term built on a message env is already a term of the host heap the delivery copies out
//! of. That is why the ABI routes the `msg_env` form and the `NULL` form through one hook.
//!
//! # Answers
//!
//! The C's only two outcomes, never a third: `Some(true)` / `Some(false)` from the provider, which
//! `beam_abi`'s `enif_send` renders as `1` / `0`. Three paths answer without a provider delivery and
//! none fabricates success:
//!
//! * **no provider, inside a `load` callback** — the load/unload host ([`NifHostImpl`] built with
//!   `services: None` by [`NifBridge::load_nif`](crate::context::bridges::NifBridge)) has no runtime
//!   to reach, so the host serves `enif_send` out of its own callback-scoped pending-send window
//!   ([`NifHostImpl::with_load_sends`]): the send is queued and answers the C's `1` for an internal
//!   local pid, and `NifBridge::load_nif` delivers it through [`deliver`] the moment the callback
//!   returns — still inside the `erlang:load_nif/2` call, so the message is in the receiver's mailbox
//!   before Erlang continues.
//! * **no provider, outside that window** — the `unload`/resource-`dtor` callback (whose window closed
//!   with the load callback, and which no drain point would honour) and the scratch host keep the
//!   trait default `None`, which `enif_send` renders as `0`. A destination naming a **dead** local pid
//!   is the one honest residual inside the window: it is accepted (`1`) where the C answers `0`,
//!   because the liveness relation lives in a process table the load host cannot reach — the drain
//!   then reports the undelivered message on the debug-trace channel.
//! * **a `from` that is not an internal pid** — no such term can be built by this host (its
//!   `self_pid` is always a local pid), and a pid the runtime cannot name names no sender, so the
//!   answer is the ABI's `0` rather than a delivery attributed to nobody.

use beam_process::ProcessId;
use beam_term::Eterm;

/// Resolves `enif_send`'s sender — the body of [`NifHost::send`] for [`NifHostImpl`] up to the
/// provider, which stays in `src/nif.rs` (a trait's methods live in one `impl` block).
///
/// `self_pid` is the host's own process (the C's `c_p->common.id`); `from` is the ABI's optional
/// override, which is that same pid in every ordinary case — a NIF call always sends from the
/// process it runs as.
///
/// `None` only when the sender term names no process: no such term can be built by this host (its
/// `self_pid` is always a local pid), so a `None` here is the ABI's `0` — never a delivery
/// attributed to nobody, and never a fabricated `1`.
pub(crate) fn sender_pid(self_pid: Eterm, from: Option<Eterm>) -> Option<ProcessId> {
    ProcessId::from_eterm(from.unwrap_or(self_pid)).ok()
}