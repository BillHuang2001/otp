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

//! The `epmd` lookup an outbound link dials through, and the `node@host` name helpers.
//!
//! The address is [`crate::dist::epmd_addr`] — the runtime's single reading of `$ERL_EPMD_PORT` —
//! so a node's `ALIVE2` registration and its outbound `PORT2_REQ` lookups always aim at the same
//! daemon.

use beam_common::Result;

/// The host part of a `node@host` name.
pub(super) fn host_part(full: &str) -> String {
    match full.split_once('@') {
        Some((_, host)) => host.to_string(),
        None => full.to_string(),
    }
}

/// The distribution listen port of the peer registered as `epmd_name`, resolved on
/// **the** `epmd` this node speaks to.
///
/// The address is [`crate::dist::epmd_addr`] — the runtime's single reading of
/// `$ERL_EPMD_PORT` — so a node's `ALIVE2` registration
/// ([`crate::dist::DistNode::register_once`]) and its outbound `PORT2_REQ` lookups
/// always aim at the same daemon (OTP's `erl_epmd:get_epmd_port/0` semantics: a bare
/// number is a loopback port, a `host:port` value verbatim, unset the well-known
/// `127.0.0.1:4369`). An unregistered name stays the typed
/// `Io(NotFound)` [`DistClient::connect`]'s contract promises.
pub(super) fn lookup_dist_port(epmd_name: &[u8]) -> Result<u16> {
    lookup_dist_port_at(&crate::dist::epmd_addr(), epmd_name)
}

/// [`lookup_dist_port`] over an **explicit** `epmd` address — the testable core.
///
/// The address is a parameter because the process environment is global: a test
/// points it at a fake `epmd` on an ephemeral loopback port instead of mutating
/// `$ERL_EPMD_PORT` (this crate never `set_var`s in a test).
pub(super) fn lookup_dist_port_at(addr: &str, epmd_name: &[u8]) -> Result<u16> {
    beam_dist::epmd::lookup_port_at(addr, epmd_name)
}
