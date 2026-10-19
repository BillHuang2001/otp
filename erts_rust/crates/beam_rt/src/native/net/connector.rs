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

//! The outbound half of contract A.5 from the Erlang surface: `net_adm:ping/1`,
//! `net_kernel:connect_node/1` and `erl_distribution:start_link/0,1`, plus the two peer-name
//! helpers they share. Both entry points dial through [`crate::Runtime::dist_connect`], whose
//! typed failure **is** their answer — see [`super`]'s module docs for the two details of the
//! real connector reproduced here (the short-name dial and the "a dial failure is the BIF's own
//! answer, not a raise" rule).

use beam_common::{BeamError, Result};
use beam_term::Eterm;

use crate::context::RuntimeContext;

impl RuntimeContext<'_> {
    // -- net_adm:ping/1, net_kernel:connect_node/1 ----------------------------

    /// `net_adm:ping/1`: the atom `pong` when the peer is reachable, else the atom `pang`.
    ///
    /// The wrapper's own shape (`net_adm.erl:184-190`): `pong` for a successful
    /// `net_kernel:connect_node/1`, `pang` for everything else. A runtime that is **not** a
    /// node answers `pang` without dialling anything -- `Runtime::dist_connect` raises for a
    /// non-node, and the BIF has no error channel to carry that.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a **non-atom** argument; the oracle raises
    /// `function_clause` there (a class this runtime cannot produce from a native BIF), so
    /// the typed rejection is the honest substitute rather than a fabricated `pang`.
    pub(in crate::native) fn net_adm_ping_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let peer = *args.first().ok_or(BeamError::BadArg)?;
        let name = self.atom_name(peer).ok_or(BeamError::BadArg)?;
        if self.runtime.node_name().is_none() {
            return self.atom("pang");
        }
        if same_node(&name, &self.local_node_name()) {
            // `net_kernel:connect_node(node())` is `true` without any handshake: a node is
            // connected to itself (`net_kernel.erl`'s `connect_node(Node) when node() =:= Node`).
            return self.atom("pong");
        }
        match self.runtime.dist_connect(connect_name(&name)) {
            Ok(()) => self.atom("pong"),
            // An unreachable/unregistered peer is this BIF's `pang`, not a raise (see the
            // module docs: `connect_node/1` answers `false` for exactly this case).
            Err(_) => self.atom("pang"),
        }
    }

    /// `net_kernel:connect_node/1`: the atom `true` once the peer is connected, `false`
    /// otherwise, and the atom `ignored` on a runtime that is **not alive**.
    ///
    /// `ignored` is the oracle's own answer for every argument on a non-distributed node
    /// (`net_kernel.erl`'s `connect_node/1` clause tests `is_alive()` first), so it is
    /// **not** a capability gap: it is what a dormant VM says.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a **non-atom** argument (the module docs' `function_clause`
    /// note).
    pub(in crate::native) fn net_kernel_connect_node_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let peer = *args.first().ok_or(BeamError::BadArg)?;
        let name = self.atom_name(peer).ok_or(BeamError::BadArg)?;
        if !self.runtime.is_alive() {
            return self.atom("ignored");
        }
        if same_node(&name, &self.local_node_name()) {
            return self.atom("true");
        }
        match self.runtime.dist_connect(connect_name(&name)) {
            Ok(()) => self.atom("true"),
            Err(_) => self.atom("false"),
        }
    }

    // -- erl_distribution:start_link/0,1 --------------------------------------

    /// `erl_distribution:start_link/0,1`: the atom **`ignore`**.
    ///
    /// `kernel_sup`'s distribution child is `{erl_distribution, start_link, []}`
    /// (`lib/kernel/src/kernel.erl:262-268`), and `start_link/1` is the same entry
    /// `erl_distribution:start/1` invokes from `net_kernel:start/1`. Both start a supervisor
    /// — `supervisor:start_link({local, net_sup}, erl_distribution, [Opts])` — whose children
    /// are the **Erlang socket stack**: `net_kernel:protocol_childspecs/0`, `erl_epmd`,
    /// `auth`, `net_kernel` (`erl_distribution.erl:64-100`). This runtime has no
    /// `prim_inet`/`inet_tcp` driver and therefore no Erlang-level socket stack; its
    /// distribution is the native [`crate::dist::DistNode`] the CLI already bound and
    /// published to `epmd` (contract A.5), so the *Erlang* supervisor has nothing to start.
    ///
    /// `ignore` is exactly how `erl_distribution` expresses "nothing to start": its own
    /// `do_start_link([{sname,_},{name,_}])` walks the two flags and, when neither was given,
    /// falls through to `do_start_link([]) -> ignore` (`erl_distribution.erl:107-138`) — the
    /// value the supervisor already sees on the **plain** (no `-sname`/`-name`) route, which
    /// boots to completion. Answering it for the `-sname`/`-name` routes too makes `kernel_sup`
    /// treat `net_sup` as an absent child (the supervisor's own `ignore` contract), so the
    /// `kernel` application starts exactly as it does without a node name.
    ///
    /// # The honest divergence
    ///
    /// On the oracle's `-sname` node this call really starts `net_sup`, `net_kernel` and
    /// `auth`; here those registered names stay `undefined` because the Erlang socket stack
    /// they would run does not exist. It is a native-floor substitution, not a wrong result:
    /// the runtime's own distribution is what `erlang:nodes/0`, `net_adm:ping/1` and
    /// `net_kernel:connect_node/1` already read and drive. See the module docs.
    pub(in crate::native) fn erl_distribution_start_link(&mut self) -> Result<Eterm> {
        self.atom("ignore")
    }
}

/// Whether `peer` (an atom name from `net_adm:ping/1` or `net_kernel:connect_node/1`)
/// names the node `local` (this runtime's own full `node@host` name).
///
/// A peer may be named by its short name (`connect_node(foo)`) or by its full `foo@host`
/// name; both name the same node, so the part before `@` is compared as well.
pub(super) fn same_node(peer: &str, local: &str) -> bool {
    if peer == local {
        return true;
    }
    matches!(local.split_once('@'), Some((short, _)) if peer == short)
}

/// The name a peer is dialled with: the part before `@` for a full `name@host`, the name
/// verbatim for a bare short name.
///
/// This OTP release registers a node with `epmd` under its **short** name whether it was
/// started `-sname foo` or `-name foo@host` (measured with `epmd -names`; `-name
/// foo@host` appears as `foo`), while [`Runtime::dist_connect`] looks the name it is handed
/// up **verbatim** ([`crate::dist::NodeName::parse`] treats a `name@host` value as a long
/// name). Dialling the full name therefore resolves nothing, and the short name is what the
/// real connector queries `epmd` with.
///
/// [`Runtime::dist_connect`]: crate::Runtime::dist_connect
pub(super) fn connect_name(peer: &str) -> &str {
    match peer.split_once('@') {
        Some((short, _)) => short,
        None => peer,
    }
}
