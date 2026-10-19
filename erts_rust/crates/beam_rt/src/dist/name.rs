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

//! The node's name and its three wire forms.
//!
//! A `-name`/`-sname` value is one string on the command line but three distinct
//! things on the distribution wire: the name this node *is* (`node@host`), the name it
//! advertises inside the `'N'`/challenge handshake messages, and the name `epmd` is told
//! and a `PORT2_REQ` looks up. [`NodeName`] is the one place that splits them, so the
//! accepting half (`super::node`) and the connecting half (`crate::dist_client`) can
//! never disagree about what this node is called.

/// The node's name, split into the three forms the distribution wire needs.
///
/// * `full` — the name this node *is* (`node@host`); what
///   `erlang:node/0` reports for a live node.
/// * `advertised` — the name put on the wire in `Name`/`Challenge`: **always** the
///   full `node@host`, for a long node (`-name`) *and* a short node (`-sname`) —
///   exactly what OTP sends. OTP's connecting side validates the name inside the
///   server's `'N'`/challenge message with `list_to_existing_atom(Ns)` and compares
///   it against the node it dialled, which is always the full `node@host`
///   (`dist_util.erl`'s `recv_challenge_new/2`; both `send_name/4` and
///   `send_challenge/2` put `atom_to_binary(Node)` on the wire). A bare short name
///   is therefore refused: the peer aborts the socket mid-handshake and the
///   accepting side observes a short read.
/// * `epmd` — the name `epmd` is told and a `PORT2_REQ` looks up: the short name
///   for a short node, the full name verbatim for a long node.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NodeName {
    /// The full `node@host` name.
    full: String,
    /// The name advertised in the `'N'`/`'s'` handshake messages: the full
    /// `node@host` name (short names are *not* advertised bare).
    advertised: Vec<u8>,
    /// The name registered with `epmd`.
    epmd: Vec<u8>,
    /// Whether this is a short name (`-sname`).
    short: bool,
}

impl NodeName {
    /// Splits a configured `-name`/`-sname` value into its wire forms.
    ///
    /// A value containing `@` is a long name (verbatim); a bare value is a short
    /// name, whose host part is this host's name (`inet:gethostname/0`). The
    /// **advertised** form is the full `node@host` name in both cases (only
    /// `epmd` is told the short name of a short node) — see [`NodeName`].
    pub fn parse(configured: &str) -> NodeName {
        if configured.contains('@') {
            NodeName {
                full: configured.to_string(),
                advertised: configured.as_bytes().to_vec(),
                epmd: configured.as_bytes().to_vec(),
                short: false,
            }
        } else {
            let full = format!("{configured}@{}", local_hostname());
            NodeName {
                advertised: full.as_bytes().to_vec(),
                full,
                epmd: configured.as_bytes().to_vec(),
                short: true,
            }
        }
    }

    /// The full `node@host` node name.
    pub fn full(&self) -> &str {
        &self.full
    }

    /// The name advertised in the handshake: the **full** `node@host` name, for a
    /// long node (`-name`) and a short node (`-sname`) alike.
    pub fn advertised(&self) -> &[u8] {
        &self.advertised
    }

    /// The name `epmd` registers and a `PORT2_REQ` looks up.
    pub fn epmd(&self) -> &[u8] {
        &self.epmd
    }

    /// Whether this is a short (`-sname`) name.
    pub fn is_short(&self) -> bool {
        self.short
    }
}

/// This host's name, the host part of a short node name — what
/// `inet:gethostname/0` answers.
fn local_hostname() -> String {
    if let Ok(contents) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        let host = contents.trim();
        if !host.is_empty() {
            return host.to_string();
        }
    }
    match std::env::var("HOSTNAME") {
        Ok(host) if !host.is_empty() => host,
        _ => "localhost".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A short (`-sname`) node advertises the **full** `node@host` on the wire while
    /// telling `epmd` the short name: OTP's connecting side compares the challenge
    /// name against the node it dialled (`dist_util.erl`'s `recv_challenge_new/2`),
    /// so a bare advertised name makes a real peer abort the socket.
    #[test]
    fn a_short_name_advertises_the_full_node_name() {
        let name = NodeName::parse("mynode");
        assert!(name.is_short());
        assert_eq!(name.full(), format!("mynode@{}", local_hostname()));
        assert_eq!(
            name.advertised(),
            name.full().as_bytes(),
            "the advertised name is the full `node@host`, never the bare short name"
        );
        assert_eq!(name.epmd(), b"mynode", "epmd is told the short name");
    }

    /// A long (`-name`) node advertises and registers its name verbatim.
    #[test]
    fn a_long_name_advertises_and_registers_verbatim() {
        let name = NodeName::parse("mynode@some.host.example");
        assert!(!name.is_short());
        assert_eq!(name.full(), "mynode@some.host.example");
        assert_eq!(name.advertised(), b"mynode@some.host.example");
        assert_eq!(name.epmd(), b"mynode@some.host.example");
    }
}
