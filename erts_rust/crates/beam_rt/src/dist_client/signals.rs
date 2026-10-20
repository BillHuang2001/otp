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

//! The `DOP_*` argument materialisation of a routed [`DistSignal`]: the external pid/reference
//! builders and the checked `DOP_UNLINK_ID` id.
//!
//! The wire tuples themselves are [`super`]'s docs (taken from `erts/emulator/beam/dist.c`);
//! this file only writes a signal's terms onto the client's stable scratch heap.

use beam_alloc::Heap;
use beam_codec::{ref_id_to_numbers, term::make_external_ref};
use beam_common::{BeamError, Result};
use beam_process::{DistSignal, MonitorRef};
use beam_term::prelude::{try_make_small, Eterm};

/// The node a routed signal addresses.
///
/// Every variant but [`DistSignal::MonitorExit`] names its peer in the `to`/`target` element,
/// which **is** the node the control must be written to. `MonitorExit` is the exception: its
/// wire tuple is *watched*-first (`TUPLE5(DOP_MONITOR_P_EXIT, watched, watcher, ref, reason)`,
/// `dist.c:1516`) and the watched pid is this node's **own** process, so the element naming the
/// destination is the **watcher's** node — routing by the watched pid would address this node.
pub(super) fn signal_node(signal: &DistSignal) -> Eterm {
    match signal {
        DistSignal::Link { to, .. }
        | DistSignal::Unlink { to, .. }
        | DistSignal::Exit { to, .. }
        | DistSignal::Exit2 { to, .. } => to.node(),
        DistSignal::Monitor { target, .. } | DistSignal::Demonitor { target, .. } => target.node(),
        // The reverse of the monitor family's order: the peer that must receive the
        // `{'DOWN', …}` is the **watcher** (`beam_process::DistSignal::MonitorExit`).
        DistSignal::MonitorExit { watcher, .. } => watcher.node(),
    }
}

/// The `{DOP_DEMONITOR_P, …, Ref}`/`{DOP_MONITOR_P, …, Ref}` reference element, and the
/// **locally minted** reference of a `{DOP_MONITOR_P_EXIT, …, Ref, …}`
/// ([`beam_process::RemoteWatcherRef::Local`] — a peer-minted one is written back verbatim
/// instead, see `client.rs`'s `encode_signal`).
///
/// The reference **must** go on the wire as an **external** reference
/// (`NEWER_REFERENCE_EXT`, `{node, creation, ids}`), never as the in-heap internal ref
/// thing `MonitorRef::to_eterm` builds: an internal ref's `node` slot is the M1
/// substitution `make_small(0)` (a *small integer*, not an atom — see
/// [`beam_process::MonitorRef::to_eterm`]), which the ETF encoder cannot render as a
/// node name. `beam_dist`'s own live test
/// (`beam_dist/tests/live_interop.rs:1063`) sends exactly this external form to a real
/// `erl` peer, which accepts it and echoes it back in `DOP_MONITOR_P_EXIT`.
///
/// The three 32-bit identifier words are the reference id's **canonical wire image**:
/// [`beam_codec::ref_id_to_numbers`] (the shared `beam_codec::ref_wire` authority, the
/// C's `erts_set_ref_numbers` pack, `ERTS_REF_NUMBERS` words). Using that one mapping —
/// never a crate-local bit layout — is what makes our own reference come back as the
/// *same* id once the inbound decode demotes it
/// (`LocalDeliverySink::wire_monitor_ref_id`).
pub(super) fn monitor_ref_term(
    heap: &mut Heap,
    reference: MonitorRef,
    node: Eterm,
    creation: u32,
) -> Result<Eterm> {
    let ids = ref_id_to_numbers(reference.id());
    make_external_ref(heap, node, &ids, creation)
}

/// The `DOP_UNLINK_ID` id element: a **checked** small integer.
///
/// The C's `DOP_UNLINK_ID` carries a small integer the receiver echoes back in its
/// `DOP_UNLINK_ID_ACK` (`erl_proc_sig_queue.c`), so the id has to be encodable as an
/// immediate — the wire's whole id slot. `make_small` is unchecked (it only
/// `debug_assert!`s its range), so casting a counter into it would panic in a debug build
/// and, in release, wrap into a **different** — possibly negative — id, which a peer would
/// then ack against the wrong unlink. An id past `MIN_SMALL..=MAX_SMALL` is therefore
/// refused with the crate's typed resource-limit error
/// ([`BeamError::SystemLimit`]), exactly as reference-id exhaustion is in
/// `beam_process::MonitorRef::to_eterm`.
///
/// Only reachable in principle: the counter starts at `1` and one id is spent per routed
/// unlink signal, so a node would need `MAX_SMALL` unlinks before it reached the bound.
pub(super) fn unlink_id_term(id: u64) -> Result<Eterm> {
    // `MIN_SMALL` needs no test: a `u64` id is never negative, and `i64::try_from` rejects
    // the whole upper half of the `u64` space.
    let signed = i64::try_from(id).map_err(|_| BeamError::SystemLimit)?;
    try_make_small(signed).map_err(|_| BeamError::SystemLimit)
}
