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
//! cannot express. This group therefore drives each case as a [`CaseKind::Command`] case, in
//! two shapes:
//!
//! * the **`dist_probe` shape** ([`CaseKind::Command`] without a peer leg): the **oracle**
//!   side spawns the system `erl` with a script that starts a real second node
//!   (`peer:start_link/1`) and prints the observable of interest; the **Rust** side spawns
//!   the external `dist_probe` helper (`["loopback", <action>]`), which performs the same
//!   loopback bottom-up (its own `epmd` registration, handshake and term exchange) and prints
//!   exactly one line — `pong`, the peer count, or the canonical ETF hex image of the
//!   delivered reply.
//! * the **self-pair shape** (`dist/live_self_pair_handshake_and_message`, a [`PeerRun`] on the
//!   case): each side is itself a **client** that talks to a peer of *its own* program, which
//!   the harness starts for it. See *Two-VM self-pairs* below.
//!
//! Both sides print one stdout line and exit `0`, and only **stdout + exit status** are
//! compared (stderr is not), exactly like a module-run case. The helper reports an honest
//! capability gap the usual way — `dist_probe: unsupported: <reason>` on stderr and exit `3`
//! — which the harness classifies as `unsupported`, never a mismatch; a **missing** helper is
//! a loud `skipped`, never a fabricated pass.
//!
//! ## Node names and `epmd` isolation
//!
//! The `dist_probe` cases each start a fixed node (`distoraN`) plus a fixed peer
//! (`distpeerN`): they never collide with each other, and they run in one lane (the same
//! `epmd`-contention consideration that serializes the other distribution cases), so a
//! concurrent sweep of the corpus is the one thing they cannot tolerate.
//!
//! The self-pair case instead derives **every** node name from a per-invocation nonce
//! ([`super::PEER_NONCE_PLACEHOLDER`]: a short hash of the case id + the process id + a
//! process-wide counter + the side). `epmd`'s name space is machine-global and shared with
//! every other VM on the host — other worktrees, other agents, and the 16-way parallel
//! per-case sweep — so a fixed name is not an option there: two concurrent runs of this case
//! never share a node name, and the oracle and Rust legs of one run never share one either.
//!
//! ## Why only four of the helper's eleven reports are corpus cases
//!
//! `dist_probe loopback <report>` supports eleven reports, but a [`CaseKind::Command`] case
//! must have its **oracle** side *observe* the golden through a public Erlang API — it must
//! not merely restate a value. Only four reports are genuine cross-runtime observables that
//! the system `erl` can reproduce independently:
//!
//! * `ping` — `net_adm:ping/1` of a peer returns `pong`;
//! * `nodes` — the pinged peer appears in `nodes/0`;
//! * `term` — an `erpc:call/4` reply's canonical ETF image;
//! * `utf8` — a non-Latin-1 atom (U+03A9) crosses the wire and comes back, and the oracle's
//!   own `term_to_binary/1` yields the same `SMALL_ATOM_UTF8_EXT` image.
//!
//! The other seven reports (`nodeid`, `handshake`, `badcookie`, `frag`, `alias`, `monitor`,
//! `regsend_missing`) encode **Rust-internal** detail that no public `erl` API can produce:
//! the negotiated `DFLAG_*` capability word (`nodeid`), handshake nonces pinned through the
//! helper's `*_with_challenge` seams (`handshake`), an FNV-1a payload checksum plus the
//! `'E'`/`'F'` fragment-frame count (`frag`), and integer stand-ins for pids / refs / aliases
//! (`alias`, `monitor`) or a refusal frame-tag sequence `sN`/`Nr` (`badcookie`) and a
//! local no-recipient drop (`regsend_missing`). An oracle script could only reproduce such a
//! line by **hard-coding** it — not an oracle observation — so pinning them as corpus cases
//! would be a tautology. They are exercised directly against the helper by the node-local
//! integration test `tests/dist_probe_contract.rs` instead.
//!
//! ## Live **cross-runtime** traffic: why it is not — and cannot be — a single case
//!
//! Traffic *between* the Rust runtime and the system `erl` (connect/handshake, `send` /
//! `register` / `spawn` over the wire, monitor/demonitor, link/exit propagation) is fully
//! measured, and the ERTS→Rust direction **completes**: a system `erl` really connects to a
//! `beam_rt` node and uses it. `crates/beam_rt/tests/dist_two_vm.rs` pins exactly that, with
//! two VMs at once and the traffic asserted on **both** sides (ping, one message each way,
//! `{nodedown, _}` in both directions).
//!
//! It is nevertheless **not expressible as a corpus case**, for one structural reason:
//!
//! * **The two compared sides never coexist.** [`CaseKind::Command`] runs its sides
//!   *sequentially* — `harness::execute` completes the oracle child, then spawns the Rust
//!   side — and the only compared observables are each child's own stdout + exit status. No
//!   case can therefore carry a message from one runtime to the other: an oracle script
//!   printing such a golden could only *hard-code* it (the tautology the `Command`-kind rule
//!   forbids), and letting the oracle child spawn `beam_rt` would need the build artifact's
//!   path baked into an argv — exactly what `runtime::resolve_rust`'s `$BEAM_RT`-else-
//!   workspace-relative resolution exists to avoid (a tree holds only `target/release` or
//!   only `target/debug`, so a baked path is a `skipped`/`mismatch` on the other tree).
//!
//! ## Two-VM self-pairs — what the `PeerRun` formulation makes observable
//!
//! The harness can still pin the *live two-VM* behaviour of each runtime, symmetrically, by
//! making each side a **self-pair**: the side is a **client** that talks to a peer started
//! from **that same side's program** (the oracle leg pairs with an oracle peer, the Rust leg
//! with a Rust peer — [`super::PeerRun`] has no program field, so a cross-runtime pair is
//! structurally impossible). Only the client's own stdout is compared, and every line it
//! prints is a VM-stable classification, never a per-VM-start value:
//!
//! ```text
//! alive=true pong nodes=1 hidden=0 echo=42,86 nodedown down_nodes=0
//! ```
//!
//! i.e. the node is `-sname`-alive; `net_adm:ping/1` of the peer answers `pong`; **exactly one**
//! node is connected and it is **not** hidden (the peer is published — `nodes(hidden)` is
//! empty); a term sent to the peer's registered name comes back twice (`41 → 42`, `43 → 86`,
//! so the exchange is a genuine round trip, not a canned value); and, after the peer is told to
//! stop, `erlang:monitor_node/2` delivers `{nodedown, Peer}` and the node list is empty again.
//!
//! Node names, pids, ports and the host name never reach stdout: the client derives the
//! peer's node name from **its own** (`node()`), so no name has to be baked into a golden.
//! What the group deliberately does **not** pin is the `{nodeup, _}` half of `monitor_node/2`:
//! measured, the system `erl` itself delivers no `nodeup` for a monitor armed before (or after)
//! the connection, so pinning it would pin an OTP-version-dependent absence rather than a
//! shared contract. The peer's own `{nodedown, _}` direction is pinned in-crate by
//! `crates/beam_rt/tests/dist_two_vm.rs`.

use super::{
    Case, CaseKind, CommandProgram, CommandRun, CommandSpec, Contract, Golden, PeerRun,
};

/// Start a peer node, ping it and print `net_adm:ping/1`'s answer.
const SCRIPT_PING: &str = r#"try {ok, Pid, Peer} = peer:start_link(#{name => distpeer1, wait_boot => 30000}), io:format("~p~n", [net_adm:ping(Peer)]), peer:stop(Pid), halt(0) catch C:R -> io:format("EXC ~p ~p~n", [C, R]), halt(1) end."#;

/// Start a peer node, ping it and print how many nodes are visible via `nodes/0`.
const SCRIPT_NODES: &str = r#"try {ok, Pid, Peer} = peer:start_link(#{name => distpeer2, wait_boot => 30000}), _ = net_adm:ping(Peer), io:format("~p~n", [length(nodes())]), peer:stop(Pid), halt(0) catch C:R -> io:format("EXC ~p ~p~n", [C, R]), halt(1) end."#;

/// Start a peer node, ping it, round-trip an `erpc:call/4` and print the canonical ETF hex
/// image of the reply (`binary:encode_hex(term_to_binary(Reply))`).
const SCRIPT_TERM: &str = r#"try {ok, Pid, Peer} = peer:start_link(#{name => distpeer3, wait_boot => 30000}), _ = net_adm:ping(Peer), Reply = erpc:call(Peer, erlang, list_to_tuple, [[1,2,3]]), io:format("~s~n", [binary:encode_hex(term_to_binary(Reply))]), peer:stop(Pid), halt(0) catch C:R -> io:format("EXC ~p ~p~n", [C, R]), halt(1) end."#;

/// Start a peer node, round-trip a **non-Latin-1** atom (`Ω`, U+03A9) through
/// `erpc:call/4` and print its canonical ETF hex image, which uses the
/// `SMALL_ATOM_UTF8_EXT` tag (`83 77 02 CE A9`).
const SCRIPT_UTF8: &str = r#"try {ok, Pid, Peer} = peer:start_link(#{name => distpeer4, host => "localhost", wait_boot => 30000}), Reply = erpc:call(Peer, erlang, list_to_atom, [[937]]), io:format("~s~n", [binary:encode_hex(term_to_binary(Reply))]), peer:stop(Pid), halt(0) catch C:R -> io:format("EXC ~p ~p~n", [C, R]), halt(1) end."#;

/// The **client** half of the self-pair case, run by one side of the case.
///
/// It derives the peer's node name from its **own** node (`-sname diffcli{nonce}` ⇒ the peer
/// is `-sname diffpeer{nonce}` on the same host, so no host name or node name is ever baked
/// into the golden), connects with a **bounded retry** (a peer publishes to `epmd` on its first
/// scheduler pass, which can lag its ready line), and prints exactly one VM-stable line:
///
/// * `alive=true` — the node is a live `-sname` node;
/// * `pong` — `net_adm:ping/1` of the peer succeeded (a real handshake, both directions);
/// * `nodes=1 hidden=0` — exactly one connected node and it is **published**, not hidden;
/// * `echo=42,86` — a term sent to the peer's registered name came back twice
///   (`41 → 42`, `43 → 86`): one message each way, twice over;
/// * `nodedown` / `down_nodes=0` — with `erlang:monitor_node/2` armed, the peer's `halt`
///   delivers `{nodedown, Peer}` and the node list drains again.
const SCRIPT_SELF_PAIR_CLIENT: &str = r#"try register(diffcli, self()), Peer = list_to_atom("diffpeer{nonce}" ++ lists:nthtail(length("diffcli{nonce}"), atom_to_list(node()))), Alive = erlang:is_alive(), Pong = case lists:any(fun(_) -> case net_adm:ping(Peer) of pong -> true; _ -> timer:sleep(50), false end end, lists:seq(1, 40)) of true -> pong; false -> pang end, true = erlang:monitor_node(Peer, true), Nodes = length(nodes()), Hidden = length(nodes(hidden)), erlang:send({echo, Peer}, {diffcli, node(), 41}), Echo1 = receive {echo, V1} -> V1 after 8000 -> timeout end, erlang:send({echo, Peer}, {diffcli, node(), 43}), Echo2 = receive {echo, V2} -> V2 after 8000 -> timeout end, erlang:send({echo, Peer}, {diffcli, node(), stop}), Down = receive {nodedown, Peer} -> nodedown after 20000 -> no_nodedown end, After = length(nodes()), io:format("alive=~p ~p nodes=~p hidden=~p echo=~p,~p ~p down_nodes=~p~n", [Alive, Pong, Nodes, Hidden, Echo1, Echo2, Down, After]), halt(0) catch C:R -> io:format("EXC ~p ~p~n", [C, R]), halt(1) end."#;

/// The **peer** half of the self-pair case: register a name, announce readiness, answer the
/// two echo messages and stop on the third.
///
/// The `timer:sleep/1` before the ready line lets the node's `epmd` publication land first
/// (the client's ping retry covers the rest), and every `receive` is bounded by an `after`, so
/// a peer that is never talked to exits instead of lingering. Its stdout is **not** compared —
/// only the client's is — so nothing here needs to be VM-stable beyond the ready prefix.
const SCRIPT_SELF_PAIR_PEER: &str = r#"register(echo, self()), timer:sleep(200), io:format("PEER-READY ~p~n", [node()]), receive {diffcli, From, 41} -> erlang:send({diffcli, From}, {echo, 42}) after 20000 -> halt(2) end, receive {diffcli, From2, 43} -> erlang:send({diffcli, From2}, {echo, 86}) after 20000 -> halt(2) end, receive {diffcli, _, stop} -> halt(0) after 20000 -> halt(2) end."#;

/// The self-pair node names: `-sname` local parts whose `{nonce}` suffix is substituted per
/// run (see [`super::PEER_NONCE_PLACEHOLDER`]).
const SELF_PAIR_CLIENT_NODE: &str = "diffcli{nonce}";
const SELF_PAIR_PEER_NODE: &str = "diffpeer{nonce}";

/// The A.5 live two-node loopback group: one case per observable (handshake, node
/// visibility, term delivery, non-Latin-1 atom round trip) plus the two-VM self-pair.
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
                peer: None,
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
                peer: None,
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
                peer: None,
            }),
            golden: Golden {
                stdout: Some("836803610161026103"),
                exit_code: 0,
            },
        },
        Case {
            id: "dist/live_utf8_atom_round_trip",
            description: "a non-Latin-1 atom (Ω, U+03A9) round-trips over the wire \
                          (erpc:call/4 of erlang:list_to_atom/1) as the SMALL_ATOM_UTF8_EXT image",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Command(CommandRun {
                oracle: CommandSpec {
                    program: CommandProgram::Oracle,
                    args: &["-noshell", "-sname", "distora4", "-eval", SCRIPT_UTF8],
                },
                rust: CommandSpec {
                    program: CommandProgram::Helper,
                    args: &["loopback", "utf8"],
                },
                peer: None,
            }),
            golden: Golden {
                stdout: Some("837702CEA9"),
                exit_code: 0,
            },
        },
        Case {
            id: "dist/live_self_pair_handshake_and_message",
            description: "two nodes of the same runtime (a client and a peer spawned for it): \
                          handshake, published (not hidden) visibility, one message each way \
                          twice, and {nodedown,_} after the peer stops",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Command(CommandRun {
                oracle: CommandSpec {
                    program: CommandProgram::Oracle,
                    args: &[
                        "-noshell",
                        "-sname",
                        SELF_PAIR_CLIENT_NODE,
                        "-eval",
                        SCRIPT_SELF_PAIR_CLIENT,
                    ],
                },
                rust: CommandSpec {
                    program: CommandProgram::Rust,
                    args: &[
                        "-noshell",
                        "-sname",
                        SELF_PAIR_CLIENT_NODE,
                        "-eval",
                        SCRIPT_SELF_PAIR_CLIENT,
                    ],
                },
                peer: Some(PeerRun {
                    args: &[
                        "-noshell",
                        "-sname",
                        SELF_PAIR_PEER_NODE,
                        "-eval",
                        SCRIPT_SELF_PAIR_PEER,
                    ],
                    ready_prefix: "PEER-READY",
                }),
            }),
            golden: Golden {
                stdout: Some(
                    "alive=true pong nodes=1 hidden=0 echo=42,86 nodedown down_nodes=0",
                ),
                exit_code: 0,
            },
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::group;
    use crate::corpus::{
        CaseKind, CommandProgram, Contract, PEER_NONCE_PLACEHOLDER, expand_nonce,
    };

    #[test]
    fn the_group_pins_the_live_loopback_handshake_visibility_and_term_delivery() {
        let cases = group();
        assert_eq!(cases.len(), 5, "the A.5 live group has five cases");
        let ids: Vec<&str> = cases.iter().map(|case| case.id).collect();
        assert_eq!(
            ids,
            [
                "dist/live_handshake_ping",
                "dist/live_node_visibility",
                "dist/live_term_delivery",
                "dist/live_utf8_atom_round_trip",
                "dist/live_self_pair_handshake_and_message",
            ]
        );
        for case in &cases {
            assert_eq!(case.contracts, &[Contract::Distribution], "{}", case.id);
            assert_eq!(case.golden.exit_code, 0, "{}", case.id);
            let CaseKind::Command(run) = &case.kind else {
                panic!("{} must be a Command case", case.id);
            };
            // The oracle side is always the resolved `erl`.
            assert!(matches!(run.oracle.program, CommandProgram::Oracle), "{}", case.id);
            assert_eq!(run.oracle.args[..2], ["-noshell", "-sname"], "{}", case.id);
            assert!(!run.oracle_command_line().is_empty(), "{}", case.id);
            match run.peer {
                // The `dist_probe` shape: the Rust side is the helper, one node per side.
                None => {
                    assert!(matches!(run.rust.program, CommandProgram::Helper), "{}", case.id);
                    assert_eq!(run.rust.args, ["loopback", case_action(case.id)], "{}", case.id);
                }
                // The self-pair shape: the Rust side is the runtime itself and both sides are
                // identical clients, so the only difference between them is the program.
                Some(peer) => {
                    assert_eq!(case.id, "dist/live_self_pair_handshake_and_message");
                    assert!(matches!(run.rust.program, CommandProgram::Rust), "{}", case.id);
                    assert_eq!(run.rust.args, run.oracle.args, "{}: the same client argv", case.id);
                    assert_eq!(peer.ready_prefix, "PEER-READY", "{}", case.id);
                    // Every node name is per-invocation: a fixed one would collide on the
                    // machine-global `epmd` name space.
                    for args in [run.oracle.args, run.rust.args, peer.args] {
                        assert!(
                            args.iter().any(|arg| arg.contains(PEER_NONCE_PLACEHOLDER)),
                            "{}: every arg list must carry the nonce placeholder",
                            case.id
                        );
                    }
                    // The two halves must not share a node name.
                    assert_ne!(run.oracle.args[2], peer.args[2], "{}", case.id);
                    // The client names the peer through the placeholder too, so the golden
                    // can never contain a real node name.
                    assert!(
                        run.oracle.args[4].contains(PEER_NONCE_PLACEHOLDER),
                        "{}: the client expression derives the peer name from the nonce",
                        case.id
                    );
                }
            }
        }
    }

    /// The self-pair golden is the **oracle's** measured line — every field a VM-stable
    /// classification (no node name, pid, port, hex image or timing ever reaches it).
    #[test]
    fn the_self_pair_golden_is_the_measured_oracle_line() {
        let cases = group();
        let case = cases
            .iter()
            .find(|case| case.id == "dist/live_self_pair_handshake_and_message")
            .expect("the self-pair case");
        assert_eq!(
            case.golden.stdout,
            Some("alive=true pong nodes=1 hidden=0 echo=42,86 nodedown down_nodes=0")
        );
    }

    /// The nonce substitution resolves *every* placeholder occurrence and leaves the rest of
    /// the argv untouched — the mechanism the two node names depend on.
    #[test]
    fn expand_nonce_resolves_every_placeholder() {
        let args = ["-sname", "diffcli{nonce}", "-eval", "X = \"diffpeer{nonce}@h\""];
        assert_eq!(
            expand_nonce(&args, "abc123"),
            vec![
                "-sname".to_string(),
                "diffcliabc123".to_string(),
                "-eval".to_string(),
                "X = \"diffpeerabc123@h\"".to_string(),
            ]
        );
        assert_eq!(
            expand_nonce(&["-noshell".to_string()], "abc123"),
            vec!["-noshell".to_string()],
            "an argv without the placeholder is unchanged"
        );
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

    #[test]
    fn the_utf8_golden_is_the_small_atom_utf8_ext_image() {
        let cases = group();
        let utf8 = cases
            .iter()
            .find(|case| case.id == "dist/live_utf8_atom_round_trip")
            .expect("the utf8-atom round-trip case");
        // `83 77 02 CE A9`: VERSION_MAGIC, SMALL_ATOM_UTF8_EXT, 2-byte length, `Ω` in UTF-8.
        assert_eq!(
            utf8.golden.stdout,
            Some("837702CEA9"),
            "the oracle's binary:encode_hex(term_to_binary(list_to_atom([937])))"
        );
    }

    /// The `loopback` action of a case id (the third component of `dist/live_<action>`).
    fn case_action(id: &str) -> &'static str {
        match id {
            "dist/live_handshake_ping" => "ping",
            "dist/live_node_visibility" => "nodes",
            "dist/live_term_delivery" => "term",
            "dist/live_utf8_atom_round_trip" => "utf8",
            other => panic!("unexpected dist case id {other}"),
        }
    }
}
