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

//! Reference **aliases** as `send` destinations — the shared resolution behind
//! `Alias ! Msg`.
//!
//! An `erlang:alias/1` (or `erlang:monitor/3` `{alias, Mode}`) reference is a valid `send`
//! destination whose message goes to the **alias owner** — the process that created the alias
//! ([`ProcessTable::resolve_alias`]). Both of this crate's send entry points need that one
//! lookup, so it lives here once:
//!
//! * the compiled `Dest ! Msg` instruction, whose `send` arm is served by
//!   [`RuntimeContext::send_message_with_off_heap`](super::RuntimeContext) — this is what
//!   `gen_server:reply/2`'s `Alias ! {Tag, Reply}` compiles to, and therefore what unblocks the
//!   whole `gen_send`/`gen:call` reply family;
//! * the `erlang:send/2` / `erlang:'!'/2` BIF, served by
//!   [`SendBridge`](super::bridges::SendBridge) — `erlang:send(Alias, Msg)`.
//!
//! # The C's arm this reproduces
//!
//! `do_send` routes **any** internal reference through `send_altact_message`
//! (`../../erts/emulator/beam/bif.c:2478-2492`) rather than through the pid path, and the
//! receiver's `handle_altact_msg` (`../../erts/emulator/beam/erl_proc_sig_queue.c:6009-6112`)
//! looks the reference up in the owner's monitor tree: a reference that carries the alias flag
//! resolves to the alias **owner** (and an `ERTS_ML_STATE_ALIAS_ONCE` alias is deleted by the
//! delivery), while one that does not is *dropped*.
//!
//! # Divergence from the oracle (`None` on a non-alias reference)
//!
//! [`MonitorRef::from_eterm`] rejects a non-reference, and
//! [`ProcessTable::resolve_alias`] answers `None` for a live-owner-less or non-alias
//! reference, so `None` covers **both** "not a reference at all" and "a reference that is
//! not a live alias" — and the callers turn it into the typed `badarg` (the answer for a
//! destination that is not a destination, preserving the pre-alias behaviour of a reference
//! send). The C never fails an *internal reference* send: it drops a reference that is not a
//! live alias and still reports success. Probed against the system oracle:
//!
//! ```none
//! 1> catch (make_ref() ! x).                       %% x   (dropped, no error)
//! 2> A = alias(), erlang:unalias(A), catch (A ! z).%% z   (dropped, no error)
//! ```
//!
//! Distinguishing "is a reference" from "is a pid" at the call sites (one extra
//! `MonitorRef::from_eterm(to).is_ok()` test) is what adopting the C's answer exactly would
//! take; this module deliberately keeps the callers' answer unchanged instead.
//!
//! # The **provenance bridge** (how a destination is classified)
//!
//! A reference term carries an `id` word, but this runtime has **two independent** reference
//! id spaces — [`Runtime::refs`](crate::Runtime) (fed by `erlang:make_ref/0`, the NIF
//! `make_ref`, the timers and `seq_trace`) and the [`ProcessTable`]'s own allocator (fed by
//! every `erlang:monitor/2,3` and `erlang:alias/1`). Both start at `0`, so the same id can
//! name a bare `make_ref/0` term **and** a live alias, and resolving a destination by its
//! parsed id alone would deliver to the wrong place.
//!
//! So the destination is classified by **provenance first**, through the runtime's term-keyed
//! registry ([`Runtime::local_reference`](crate::Runtime)):
//!
//! * `Some(Some(reference))` — the term is a monitor/alias reference this runtime minted: the
//!   classification is authoritative, and **only** this shape may be looked up as a live
//!   alias ([`ProcessTable::resolve_alias`]).
//! * `Some(None)` — a bare runtime-minted reference (`make_ref/0`, a timer, a `seq_trace`
//!   ref): never an alias, so `None`.
//! * `None` — the term was **not** minted here (a reference decoded from an external term, or
//!   the copy of one received in a message): its provenance is unknown, so fall back to the id
//!   the term encodes. That keeps a send to an alias that came back through `binary_to_term/1`
//!   or in a `{'DOWN', …}` message working.
//!
//! The lookup is read-only and holds no borrow: the caller resolves (immutable), delivers
//! (mutable) and consumes (mutable) strictly in sequence, never across.

use beam_process::{MonitorRef, ProcessId, ProcessTable};
use beam_term::Eterm;

use crate::runtime::Runtime;

/// The **alias owner** a `send` destination names, together with the reference that named it.
///
/// `Some((owner, reference))` means `to` is an internal reference that is a **live alias**:
/// the message goes to `owner`, and the `reference` must be handed to
/// [`ProcessTable::alias_reply_delivered`] *after* a successful delivery so a
/// `reply_demonitor` alias is consumed by the reply it received (the C's alias-message site).
///
/// `None` means `to` is not a reference at all, or is a reference that names no live alias —
/// a bare `make_ref/0`/timer/`seq_trace` reference (never an alias), a plain monitor
/// reference, or an alias whose owner has died. The callers keep their existing typed `badarg`
/// for that case (see the module docs' divergence note).
///
/// `to` is classified by **provenance** (`runtime`, see the module docs): a term this runtime
/// minted as a monitor/alias reference is resolved through [`ProcessTable::resolve_alias`],
/// a bare runtime-minted reference is rejected outright, and a term of unknown provenance
/// falls back to the id it encodes.
pub(super) fn alias_destination(
    runtime: &Runtime,
    table: &ProcessTable,
    to: Eterm,
) -> Option<(ProcessId, MonitorRef)> {
    match runtime.local_reference(to) {
        // Authoritative: a reference this runtime minted as a monitor/alias.
        Some(Some(reference)) => table.resolve_alias(reference).map(|owner| (owner, reference)),
        // Authoritative: a bare runtime-minted reference is never an alias.
        Some(None) => None,
        // Unknown provenance: fall back to the id the term encodes.
        None => {
            let reference = MonitorRef::from_eterm(to).ok()?;
            table.resolve_alias(reference).map(|owner| (owner, reference))
        }
    }
}
