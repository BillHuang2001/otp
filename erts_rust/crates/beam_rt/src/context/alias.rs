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
//! # Known hazard: two reference id spaces
//!
//! `MonitorRef` is a **bare id**, and the runtime mints reference terms from **two independent**
//! allocators: `erlang:make_ref/0` goes through
//! [`Runtime::refs_mut`](crate::Runtime::refs_mut)
//! (`crate::native::process::mint_reference`), while every monitor *and* every alias mints from
//! the [`ProcessTable`]'s own `RefAllocator` (`ProcessTable::monitor_*`,
//! `ProcessTable::create_alias`). Both allocators start at id `0`, so the same id can name a
//! `make_ref/0` term **and** a live alias: a send to that `make_ref/0` term would then resolve
//! as the alias and be delivered to the alias owner — a wrong delivery rather than the C's
//! silent drop. (Measured: a `make_ref/0` term minted first and an alias minted first collide on
//! id `0`.)
//!
//! A reference the **table** minted can never collide with another table-minted reference, so
//! only runtime-minted (`make_ref/0`, and a reference decoded out of an external term) terms are
//! exposed. Within this node there is no sound way to prove a term's provenance (the runtime's
//! `references` map records bare `make_ref/0` terms too), so the fix belongs to the owners of
//! the two allocators: mint `make_ref/0` from the **table's** allocator (one id space, like the
//! C), or key the alias lookup by the reference *term* the way
//! [`Runtime::references`](crate::Runtime::references) keys its monitor map.

use beam_process::{MonitorRef, ProcessId, ProcessTable};
use beam_term::Eterm;

/// The **alias owner** a `send` destination names, together with the reference that named it.
///
/// `Some((owner, reference))` means `to` is an internal reference that is a **live alias**:
/// the message goes to `owner`, and the `reference` must be handed to
/// [`ProcessTable::alias_reply_delivered`] *after* a successful delivery so a
/// `reply_demonitor` alias is consumed by the reply it received (the C's alias-message site).
///
/// `None` means `to` is not a reference at all, or is a reference that names no live alias —
/// `ProcessTable::resolve_alias` already answers `None` for a plain monitor reference and for
/// an alias whose owner has died. The callers keep their existing typed `badarg` for that case
/// (see the module docs' divergence note).
///
/// The lookup is read-only and holds no borrow: the caller resolves (immutable), delivers
/// (mutable) and consumes (mutable) strictly in sequence, never across.
pub(super) fn alias_destination(
    table: &ProcessTable,
    to: Eterm,
) -> Option<(ProcessId, MonitorRef)> {
    let reference = MonitorRef::from_eterm(to).ok()?;
    table.resolve_alias(reference).map(|owner| (owner, reference))
}
