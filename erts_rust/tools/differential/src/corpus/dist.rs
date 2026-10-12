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

//! Contract **A.5** — the **live** distribution two-node loopback.
//!
//! The `wire` / `wire_extra` groups pin the ETF/distribution wire *codec* at the tag level,
//! but a live handshake and a `DOP_*` loopback need **two nodes**, which a single CLI process
//! cannot express. This group therefore drives each case as a [`CaseKind::Command`] case:
//!
//! * the **oracle** side spawns the system `erl` with a script that starts a real second
//!   node (`peer:start_link/1`) and prints the observable of interest;
//! * the **Rust** side spawns the external `dist_probe` helper (`["loopback", <action>]`),
//!   which performs the same loopback bottom-up (its own `epmd` registration, handshake and
//!   term exchange) and prints exactly one line — `pong`, the peer count, or the canonical
//!   ETF hex image of the delivered reply.
//!
//! Both sides print one stdout line and exit `0`, and only **stdout + exit status** are
//! compared (stderr is not), exactly like a module-run case. The helper reports an honest
//! capability gap the usual way — `dist_probe: unsupported: <reason>` on stderr and exit `3`
//! — which the harness classifies as `unsupported`, never a mismatch; a **missing** helper is
//! a loud `skipped`, never a fabricated pass.
//!
//! Each oracle script starts a node named `distoraN` and a peer named `distpeerN`, so the
//! three cases never collide on `epmd`. They run in one lane (the same `epmd`-contention
//! consideration that serializes the other distribution cases).

use super::{Case, CaseKind, CommandProgram, CommandRun, CommandSpec, Contract, Golden};

/// Start a peer node, ping it and print `net_adm:ping/1`'s answer.
const SCRIPT_PING: &str = r#"try {ok, Pid, Peer} = peer:start_link(#{name => distpeer1, wait_boot => 30000}), io:format("~p~n", [net_adm:ping(Peer)]), peer:stop(Pid), halt(0) catch C:R -> io:format("EXC ~p ~p~n", [C, R]), halt(1) end."#;

/// Start a peer node, ping it and print how many nodes are visible via `nodes/0`.
const SCRIPT_NODES: &str = r#"try {ok, Pid, Peer} = peer:start_link(#{name => distpeer2, wait_boot => 30000}), _ = net_adm:ping(Peer), io:format("~p~n", [length(nodes())]), peer:stop(Pid), halt(0) catch C:R -> io:format("EXC ~p ~p~n", [C, R]), halt(1) end."#;

/// Start a peer node, ping it, round-trip an `erpc:call/4` and print the canonical ETF hex
/// image of the reply (`binary:encode_hex(term_to_binary(Reply))`).
const SCRIPT_TERM: &str = r#"try {ok, Pid, Peer} = peer:start_link(#{name => distpeer3, wait_boot => 30000}), _ = net_adm:ping(Peer), Reply = erpc:call(Peer, erlang, list_to_tuple, [[1,2,3]]), io:format("~s~n", [binary:encode_hex(term_to_binary(Reply))]), peer:stop(Pid), halt(0) catch C:R -> io:format("EXC ~p ~p~n", [C, R]), halt(1) end."#;

/// The A.5 live two-node loopback group: one case per observable (handshake, node
/// visibility, term delivery).
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "dist/live_handshake_ping",
            description: "a live two-node handshake: net_adm:ping/1 of a peer node returns pong",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Command(CommandRun {
                oracle: CommandSpec {
                    program: CommandProgram::Oracle,
                    args: &["-noshell", "-sname", "distora1", "-eval", SCRIPT_PING],
                },
                rust: CommandSpec {
                    program: CommandProgram::Helper,
                    args: &["loopback", "ping"],
                },
            }),
            golden: Golden {
                stdout: Some("pong"),
                exit_code: 0,
            },
        },
        Case {
            id: "dist/live_node_visibility",
            description: "a live peer node appears in nodes/0 after net_adm:ping/1",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Command(CommandRun {
                oracle: CommandSpec {
                    program: CommandProgram::Oracle,
                    args: &["-noshell", "-sname", "distora2", "-eval", SCRIPT_NODES],
                },
                rust: CommandSpec {
                    program: CommandProgram::Helper,
                    args: &["loopback", "nodes"],
                },
            }),
            golden: Golden {
                stdout: Some("1"),
                exit_code: 0,
            },
        },
        Case {
            id: "dist/live_term_delivery",
            description: "an ETF-encoded term round-trips over the wire (erpc:call/4 of erlang:list_to_tuple/1)",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Command(CommandRun {
                oracle: CommandSpec {
                    program: CommandProgram::Oracle,
                    args: &["-noshell", "-sname", "distora3", "-eval", SCRIPT_TERM],
                },
                rust: CommandSpec {
                    program: CommandProgram::Helper,
                    args: &["loopback", "term"],
                },
            }),
            golden: Golden {
                stdout: Some("836803610161026103"),
                exit_code: 0,
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::group;
    use crate::corpus::{CaseKind, CommandProgram, Contract};

    #[test]
    fn the_group_pins_the_live_loopback_handshake_visibility_and_term_delivery() {
        let cases = group();
        assert_eq!(cases.len(), 3, "the A.5 live group has three cases");
        let ids: Vec<&str> = cases.iter().map(|case| case.id).collect();
        assert_eq!(
            ids,
            [
                "dist/live_handshake_ping",
                "dist/live_node_visibility",
                "dist/live_term_delivery",
            ]
        );
        for case in &cases {
            assert_eq!(case.contracts, &[Contract::Distribution], "{}", case.id);
            assert_eq!(case.golden.exit_code, 0, "{}", case.id);
            let CaseKind::Command(run) = &case.kind else {
                panic!("{} must be a Command case", case.id);
            };
            // The oracle side is always the resolved `erl`; the Rust side is the helper.
            assert!(matches!(run.oracle.program, CommandProgram::Oracle), "{}", case.id);
            assert!(matches!(run.rust.program, CommandProgram::Helper), "{}", case.id);
            assert_eq!(run.rust.args, ["loopback", case_action(case.id)], "{}", case.id);
            assert_eq!(run.oracle.args[..2], ["-noshell", "-sname"], "{}", case.id);
            assert!(!run.oracle_command_line().is_empty(), "{}", case.id);
        }
    }

    #[test]
    fn the_term_delivery_golden_is_the_canonical_etf_hex_image() {
        let cases = group();
        let term = cases
            .iter()
            .find(|case| case.id == "dist/live_term_delivery")
            .expect("the term-delivery case");
        assert_eq!(
            term.golden.stdout,
            Some("836803610161026103"),
            "the oracle's binary:encode_hex(term_to_binary({{1,2,3}}))"
        );
    }

    /// The `loopback` action of a case id (the third component of `dist/live_<action>`).
    fn case_action(id: &str) -> &'static str {
        match id {
            "dist/live_handshake_ping" => "ping",
            "dist/live_node_visibility" => "nodes",
            "dist/live_term_delivery" => "term",
            other => panic!("unexpected dist case id {other}"),
        }
    }
}
