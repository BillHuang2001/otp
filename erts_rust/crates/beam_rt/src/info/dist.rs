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

//! The `dist` family of [`super::system_info_value`]: the distribution-state keys
//! `dist` and `dist_ctrl`.
//!
//! Both keys describe the wire state of a live distribution node. This runtime is
//! **not distributed** unless `-name`/`-sname` configured a node, and each key is
//! answered exactly in the state the oracle's own value describes truthfully for it —
//! the state of a `nonode@nohost` runtime, or of a configured node that has accepted no
//! link yet:
//!
//! | key | shape | answered when | value |
//! |---|---|---|---|
//! | `dist` | binary | this runtime is not a node | `<<"=node:'nonode@nohost'\n=no_distribution\n">>` |
//! | `dist_ctrl` | list | this runtime is not a node, or has no established link | `[]` |
//!
//! Otherwise the key is the typed **capability gap** ([`crate::error::unsupported`]): a
//! live wire dump (`dist`'s `=node:…`/`=proc:…` sections, `dist_ctrl`'s per-controller
//! entries) needs the `net_kernel` protocol and the connection machinery this runtime
//! does not implement, and reporting a fabricated dump would be a lie about a different
//! emulator.
//!
//! Oracle evidence (OTP 29.1.1 / erts-17.1): a bare `erl` reports
//! `byte_size(erlang:system_info(dist)) == 39` with exactly the bytes above, and
//! `erlang:system_info(dist_ctrl) == []`.

use beam_common::Result;

use super::InfoValue;
use crate::error;
use crate::runtime::Runtime;

/// `system_info(dist)` for a node with no distribution: the oracle's own 39 bytes
/// `=node:'nonode@nohost'\n=no_distribution\n` (each `\n` is a real `0x0a` byte; the
/// oracle's `~p` renders them as the two characters `\` `n`).
const NO_DISTRIBUTION: &[u8] = b"=node:'nonode@nohost'\n=no_distribution\n";

/// The `system_info/1` value of a `dist`-family key, or `None` when `key` is not ours.
///
/// A key of this family is answered only in a state the oracle's own value describes
/// truthfully for this runtime (no live distribution; for `dist_ctrl`, no established
/// link). Any other state is the typed capability gap, never a fabricated dump.
pub(crate) fn try_value(key: &str, rt: &Runtime) -> Option<Result<InfoValue>> {
    let node = rt.dist_node();
    match key {
        "dist" => Some(match node {
            // Not a node: exactly the binary the oracle reports for `nonode@nohost`.
            None => Ok(InfoValue::Binary(NO_DISTRIBUTION.to_vec())),
            // A configured node's wire dump is out of reach (no `net_kernel` protocol).
            Some(_) => Err(error::unsupported(
                "erlang:system_info(dist): this runtime does not dump live distribution state",
            )),
        }),
        "dist_ctrl" => Some(match node {
            // No node, or a node with no accepted link: the oracle reports `[]`.
            None => Ok(InfoValue::List(Vec::new())),
            Some(node) if node.connections().is_empty() => Ok(InfoValue::List(Vec::new())),
            // A live controller's entry describes the wire, which this runtime cannot.
            Some(_) => Err(error::unsupported(
                "erlang:system_info(dist_ctrl): this runtime models no distribution controllers",
            )),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuntimeConfig;
    use crate::info::test_support::runtime;

    #[test]
    fn a_non_distributed_runtime_reports_the_oracle_nonode_binary() {
        let rt = runtime();
        assert_eq!(
            try_value("dist", &rt).unwrap().unwrap(),
            InfoValue::Binary(NO_DISTRIBUTION.to_vec())
        );
        // The oracle's `byte_size/1` is 39.
        assert_eq!(NO_DISTRIBUTION.len(), 39);
    }

    #[test]
    fn a_non_distributed_runtime_reports_no_controllers() {
        let rt = runtime();
        assert_eq!(
            try_value("dist_ctrl", &rt).unwrap().unwrap(),
            InfoValue::List(Vec::new())
        );
    }

    #[test]
    fn a_configured_node_without_a_link_gaps_dist_and_reports_no_controllers() {
        // A runtime carrying a `-name`/`-sname` node cannot reproduce the oracle's live wire
        // dump, so `dist` is the typed gap rather than a fabricated answer. `dist_ctrl`, on
        // the other hand, is still `[]`: the oracle itself answers `[]` for a node with no
        // accepted link (measured: `erl -name infodist@localhost` reports a 66-byte `dist`
        // and `dist_ctrl` = `[]`).
        // A node is created only when the listener binds; skip loudly if it cannot (the
        // oracle that produced the values above is likewise an external facility).
        let config = RuntimeConfig::default().with_node_name("info_dist@localhost");
        let Ok(rt) = Runtime::new(config) else {
            eprintln!(
                "SKIP a_configured_node_without_a_link_gaps_dist_and_reports_no_controllers: \
                 could not bind a listener"
            );
            return;
        };
        let error = try_value("dist", &rt)
            .unwrap_or_else(|| panic!("dist belongs to this family"))
            .expect_err("dist must be a capability gap for a live node");
        assert!(
            crate::error::unsupported_reason(&error).is_some(),
            "dist must classify as unsupported, got {error:?}"
        );
        assert_eq!(
            try_value("dist_ctrl", &rt).unwrap().unwrap(),
            InfoValue::List(Vec::new())
        );
    }

    #[test]
    fn other_keys_are_not_ours() {
        let rt = runtime();
        assert!(try_value("version", &rt).is_none());
        assert!(try_value("nonsense", &rt).is_none());
    }
}
