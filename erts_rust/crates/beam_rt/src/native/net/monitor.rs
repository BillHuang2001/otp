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

//! `erlang:monitor_node/2` (contract A.5): a node's **connection** state as one latch per
//! subscribing process. The store and the notifications are the runtime's
//! ([`crate::Runtime::monitor_node_add`]/[`crate::Runtime::monitor_node_remove`] over
//! [`crate::Runtime::node_monitors`], emitted by the scheduler pass's
//! [`crate::Runtime::service_node_monitors`]); this file only decides the argument shapes and
//! seeds the latch with the node's state **now**, which is what implements the no-replay rule.

use beam_common::{BeamError, Result};
use beam_term::Eterm;

use crate::context::RuntimeContext;

impl RuntimeContext<'_> {
    // -- erlang:monitor_node/2 ------------------------------------------------

    /// `erlang:monitor_node(Node, Bool)`: subscribe (`true`) or unsubscribe (`false`) the calling
    /// process to `Node`'s **connection** state, answering the atom `true`.
    ///
    /// Oracle facts (measured on OTP 29.1.1 / erts-17.1, and the reason this is not the surface it
    /// looks like):
    ///
    /// * The answer is the atom `true` for **every atom** `Node` — `node()` itself and
    ///   `nonode@nohost` included, and a node this runtime has never connected to. There is no
    ///   `badarg` for "your own node" (a claim sometimes made about this BIF; it is measurably
    ///   false) and none for a node with no link.
    /// * `badarg` only for a **non-atom** `Node`, or a `Bool` that is not the atom `true`/`false`.
    /// * No replay: subscribing to a node that is **already** connected yields no immediate
    ///   `nodeup`; one arrives only for a connection established **after** the subscription, and
    ///   a `nodedown` when the node's last link retires.
    ///
    /// The store is the runtime's ([`Runtime::monitor_node_add`] /
    /// [`Runtime::monitor_node_remove`] over [`Runtime::node_monitors`]) and the notifications are
    /// emitted by the scheduler pass's [`Runtime::service_node_monitors`]; this body only decides
    /// the argument shapes and seeds the latch with the node's state **now** — which is exactly
    /// what implements the no-replay rule. `erlang:monitor_node/3` (the `Info`-carrying form) is
    /// deliberately not served: it stays `beam_bifs`'s typed capability gap.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a non-atom `Node` (or an atom this runtime's namespace cannot
    /// name) and for a `Bool` other than `true`/`false`.
    pub(in crate::native) fn monitor_node_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let node = *args.first().ok_or(BeamError::BadArg)?;
        let flag = *args.get(1).ok_or(BeamError::BadArg)?;
        // A non-atom `Node` is the C's `badarg`; an atom always names something here, because a
        // loaded module's atom table is a snapshot of this runtime's shared namespace (interning
        // only appends), so the name lookup only fails for a non-atom.
        let name = self.atom_name(node).ok_or(BeamError::BadArg)?;
        let on = match self.atom_name(flag).as_deref() {
            Some("true") => true,
            Some("false") => false,
            _ => return Err(BeamError::BadArg),
        };
        let pid = self.pid();
        if on {
            let saw_connected = self.runtime.dist_is_connected(&name);
            self.runtime.monitor_node_add(pid, node, saw_connected);
        } else {
            self.runtime.monitor_node_remove(pid, node);
        }
        self.atom("true")
    }
}
