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

//! The delivery behind `enif_send` (contract A.3): a NIF's message reaches another Erlang
//! process through the runtime's **own** send seam.
//!
//! # Why this is a `context` module and not part of the NIF host
//!
//! A `send` is not a second copier: it is [`RuntimeContext::send_message`], the one seam every
//! other sender in the runtime funnels through — the compiled `!`, `erlang:send/2`, the `-eval`
//! `!` and the port/timer deliveries. That seam needs a whole [`RuntimeContext`] (the runtime, the
//! process table, and a sender identity), and a `RuntimeContext`'s fields are `pub(super)` to this
//! module; the NIF host ([`crate::nif`]) and its services provider
//! ([`crate::nif::RuntimeNifServices`], which holds the `&mut Runtime`) are outside that
//! visibility, and a delivery initiated from a `load`/`unload` callback has no runtime to borrow
//! at all. So the NIF host resolves *who is sending* and this module builds the nested context and
//! runs the delivery.
//!
//! # The nested context
//!
//! It is the same shape [`SendBridge`](crate::context::bridges::SendBridge) builds for
//! `erlang:send/2`: the sender's pid as the mailbox owner (so the message envelope's `from` is the
//! calling process, as the C's `from = c_p ? c_p->common.id : am_undefined` sets it) and the
//! **slice's own** process table when the call runs inside a scheduler slice, else the runtime's.
//! Two differences from that bridge, both deliberate:
//!
//! * `may_pump_scheduler` is always `false`: a NIF call is inside a scheduler slice (or inside a
//!   `-eval` run, which drives its own pump), and re-entering the scheduler from under a NIF would
//!   run a second process from within the first one's slice.
//! * the output sink, the executing-module name and the pass engines are not carried: a delivery
//!   prints nothing, loads no module and executes no code.
//!
//! The delivery itself resolves the destination through the shared
//! [`classify_send_destination`](crate::context::send::classify_send_destination), so an
//! `enif_send` to a local pid, to a reference that is a live alias, and to a remote node behave
//! exactly as the corresponding compiled send does; the message is copied into the receiver's heap
//! by `beam_message`'s copier and the receiver is woken through the runtime's resume-on-send wake
//! list (the pass's `run_ready` absorbs it at the end of the slice the NIF is running in).

use beam_engine::ExecutionContext;
use beam_process::{ProcessId, ProcessTable};
use beam_term::Eterm;

use super::RuntimeContext;
use crate::runtime::Runtime;

/// Delivers `msg` to the process `to` as `from`, answering the C's `enif_send` code
/// (`erl_nif.c:860-1093`): `true` when the message is queued, `false` when it is not.
///
/// `mailbox` is the caller-supplied process table when the NIF call runs inside a scheduler
/// slice; `None` means "resolve against the runtime's own
/// [`processes`](crate::Runtime::processes)", the same choice
/// [`RuntimeContext::for_process`]/[`RuntimeContext::new`] make.
///
/// # What each answer means
///
/// * `true` — [`RuntimeContext::send_message`] returned `Ok`, i.e. the message is in the receiver's
///   mailbox (a NIF is never handed a "maybe").
/// * `false` — the not-queued answer, which the C reports for exactly one reason on the normal
///   scheduler path: `erts_proc_lookup(receiver)` found no process (`:876-877`). Here the same
///   answer also covers a destination the send seam rejects (`badarg` for a term that names no
///   process — a non-pid, a port), a remote destination this node cannot reach (the C's
///   `enif_send` has no distribution arm, so `0` is the oracle's answer there too), and a failed
///   copy (the C aborts the emulator; a runtime whose error strategy is return-based reports "not
///   delivered" instead, and says so on the debug-trace channel rather than swallowing it).
pub(crate) fn deliver(
    runtime: &mut Runtime,
    mailbox: Option<&mut ProcessTable>,
    from: ProcessId,
    to: Eterm,
    msg: Eterm,
) -> bool {
    match mailbox {
        Some(table) => deliver_through(RuntimeContext::for_process(runtime, table, from), to, msg),
        None => {
            let mut ctx = RuntimeContext::new(runtime);
            // `RuntimeContext::new` runs as the runtime's own process; a NIF call runs as the
            // process the host was built for, and never pumps the scheduler (see the module docs).
            ctx.mailbox_pid = from;
            ctx.may_pump_scheduler = false;
            deliver_through(ctx, to, msg)
        }
    }
}

/// Runs the delivery on `ctx` and folds its outcome into the C's `1`/`0`.
fn deliver_through(mut ctx: RuntimeContext<'_>, to: Eterm, msg: Eterm) -> bool {
    match ctx.send_message(to, msg) {
        Ok(()) => true,
        Err(error) => {
            // Observability only (a `BEAM_RT_BOOT_TRACE` line), never a behavior: the ABI's only
            // failure channel is the `0`, so the reason is recorded instead of being lost.
            if crate::debug_trace::enabled() {
                crate::debug_trace::trace(format_args!(
                    "enif_send: delivery failed ({error:?})"
                ));
            }
            false
        }
    }
}
