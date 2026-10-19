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

//! Contract **A.5** — the *widened* live two-node surface.
//!
//! The `dist/` group pins the handshake, node visibility, one `erpc:call/4` term image, a
//! UTF-8 atom and the first two-VM self-pair. This group widens that with the rest of the
//! `DOP_*` traffic a real link carries — the message families (`DOP_SEND`/`DOP_REG_SEND`),
//! repeated and ordered exchanges, pid externalization, payload round trips, and the
//! **refusal** paths (`DOP` handshake rejections that must answer `pang` instead of hanging).
//!
//! # Mechanism: every case is a two-VM **self-pair**
//!
//! [`CaseKind::Command`](super::CaseKind) runs its two sides *sequentially* and compares only
//! each child's own stdout + exit status, so no case can carry a message **between** the
//! compared runtimes (see `dist.rs`'s module docs). Each case here is therefore a
//! **self-pair**, exactly like `dist/live_self_pair_handshake_and_message`: the side under
//! test is a *client* that talks to a peer started from that same side's program, which the
//! harness spawns via [`PeerRun`] (`super::PeerRun` has no program field, so a cross-runtime
//! pair is structurally impossible). Only the **client's** stdout is compared.
//!
//! Every node name is nonce-derived ([`super::PEER_NONCE_PLACEHOLDER`]): the client is
//! `-sname dxcli{nonce}`, the peer is `-sname dxpeer{nonce}`, and the client derives the
//! peer's name from **its own** (`node()`) — `"dxpeer{nonce}" ++
//! lists:nthtail(length("dxcli{nonce}"), atom_to_list(node()))` — so no real node name (and
//! no host name) ever reaches a golden. `epmd`'s name space is machine-global, so a fixed
//! name would collide with other worktrees and with the parallel per-case sweep.
//!
//! Every printed field is a **VM-stable classification**: a boolean, a small integer, an
//! atom. Node names, pids, ports, references, `creation`s and timings never reach stdout.
//!
//! # What each case pins
//!
//! | Case | Pins |
//! |---|---|
//! | `net_kernel_connect_node` | the explicit `net_kernel:connect_node/1` request (`true`) followed by `net_adm:ping/1` ⇒ `pong` |
//! | `ping_a_missing_node` | a **refused-by-absence** handshake answers `pang` (and the reachable peer still answers `pong`) |
//! | `cookie_mismatch` | a **refused-by-cookie** handshake answers `pang` on both sides |
//! | `pid_crosses_the_wire_as_a_foreign_pid` | a pid sent to the peer really arrives as a **pid of the sender's node**: `is_pid/1` true, its node is the sender's `node()`, and it is **not** the receiver's own node |
//! | `send_to_a_pid_and_to_a_registered_name` | `DOP_SEND` and `DOP_REG_SEND` interleaved on one link arrive **in order** with their payloads intact |
//! | `five_ordered_round_trips` | five request/response round trips: all five replies arrive, in order, tagged with their own sequence number |
//! | `atom_list_payload` | ten distinct atoms cross the link and survive: the far side's `phash2/1` and `byte_size(term_to_binary/1)` match the sender's |
//! | `list_payload` | an 80-element integer list survives the same way |
//! | `binary_payload` | a 1 KiB binary survives the same way |
//! | `send_to_a_missing_remote_name` | a `DOP_REG_SEND` to a **non-registered** name on the peer is dropped (the send returns the message) and the link keeps working |
//! | `send_to_a_dead_remote_pid` | a `DOP_SEND` to an **exited** process on the peer is dropped and the link keeps working |
//!
//! The payload identity check is deliberately *relational*: the peer computes `phash2/1` and
//! `byte_size(term_to_binary/1)` of what it received and the client compares them with its
//! own values, so the golden is `hash_ok=true etf_ok=true` — no hash constant and no ETF
//! image is baked into the corpus, and a truncated or re-encoded payload shows up as `false`.
//!
//! # Clients are `-eval` scripts, and the oracle's `erl_eval` is the binding constraint
//!
//! The oracle half runs under `erl -noshell -eval`, i.e. `erl_eval`, which rejects some
//! shapes *silently* (no stdout, exit `1` — a `hidden` corpus-drift trap, because it looks
//! like a runtime crash). Measured while building this group:
//!
//! * a **quoted atom in a `receive` pattern** (`receive {'EXIT', P, R} -> … end`) does not
//!   evaluate, and neither does a pattern that mentions an **already-bound variable**
//!   (`receive {Tag, P, R} -> …` with `Tag` bound). The working shape is a bare-variable
//!   receive (`Msg = receive M -> M after … end`) followed by `is_tuple/1` + `element/2`
//!   classification — which is what every script here does;
//! * a pattern with a **fresh** variable and a literal atom (`{who, From}`) is fine;
//! * the catch clause must not reuse a variable the body binds — every script here catches
//!   as `catch Cls:Rs ->`, never `C:R` (a body that binds `C` makes the whole expression
//!   unevaluable);
//! * a **second** `register/2` in one expression raises `badarg` under `erl_eval`
//!   (`register(a, self()), register(b, self())`), so a client can own only one registered
//!   name — which is why the registered-name cases all use the single name `dxcli`.
//!
//! # Candidates that were measured and deliberately NOT pinned
//!
//! Each of these was measured by hand against the system `erl` (OTP 29.1.1 / erts-17.1) with
//! the same self-pair shape, and **disagreed** — landing it would be a `mismatch`. The
//! details live in `divergences.md` §A.5 (which is the ledger; the owning crate is named
//! there); the short version, so no future agent re-measures them:
//!
//! * **`link/1` + `DOP_EXIT` across nodes.** Client traps exits, links to a peer-side
//!   process that exits with `byname`. Oracle: `{'EXIT', Pid, byname}`. Rust: **no signal**
//!   within 8 s (and at its own termination the runtime logs `dropping remote exit signal …
//!   distribution: no established connection to node …`), i.e. the exit signal is neither
//!   delivered nor routable.
//! * **`erlang:monitor/2` of a remote process** (`DOP_MONITOR_P`/`DOP_MONITOR_P_EXIT`).
//!   Oracle: `{'DOWN', Ref, process, Pid, byname}`. Rust: no `DOWN` when the *peer* side
//!   (the accepted link's local process) is the monitored one; the reverse direction —
//!   Rust client monitoring an *oracle* peer's process — does answer `byname`, so the gap is
//!   the listener half's outbound monitor-down. `demonitor` shares that path and is
//!   unpinnable for the same reason.
//! * **A pid round trip that must be identity-preserving.** `Pid =:= self()` after the pid
//!   went to the peer and back: `true` on the oracle, **`false`** on the Rust runtime (the
//!   local pid and node compare `=:=` neither before nor after the trip), which is also why
//!   an `alias/0` reference sent to the peer and written to by the peer is never delivered
//!   (`DOP_ALIAS_SEND` from a listener is dropped: the returned reference image does not
//!   identify the same local alias). `pid_crosses_the_wire_as_a_foreign_pid` pins only the
//!   *shape* half, which agrees.
//! * **Large payloads.** The inbound term size is bounded on the Rust side: a 100-element
//!   integer list and a 1 KiB binary round-trip, but a **128**-element list and a **128 KiB**
//!   binary kill the receiving VM (`beam_rt: error scheduler: process … has no engine entry
//!   point`) and a 1 MiB binary gets no reply at all. The landed payload cases therefore stay
//!   at 80 elements / 1 KiB (see `the_payload_cases_stay_inside_the_measured_envelope`).
//! * **`-hidden`.** A peer started with `-hidden` is `nodes() = 0, nodes(hidden) = 1` on the
//!   oracle and `nodes() = 1, nodes(hidden) = 0` on the Rust runtime.
//! * **`erlang:disconnect_node/1`** is `beam_rt: unsupported: erlang:disconnect_node/1 is a
//!   known BIF that is not implemented` (exit 3), and **`erlang:spawn_request/5`** —
//!   everything `erpc:call/4` builds on — is `unsupported` the same way (so an `erpc:call/4`
//!   of a *plain* function is unpinnable in the self-pair shape; `dist/live_term_delivery`
//!   pins the oracle's `erpc` image through the `dist_probe` helper instead).
//! * **`erlang:monitor_node/2` on a node that does not exist** answers `true` on both sides,
//!   but it is a local bookkeeping answer with no traffic behind it, so it was left out as
//!   too weak to be worth a case.
//! * **`group_leader(RemotePid, self())`** could not be measured: the oracle's `-eval`
//!   wrapper would not evaluate (a silent exit `1`), so there is no oracle observation to
//!   pin — and an unpinnable route is not a divergence either. (The Rust runtime answers
//!   `badarg` for a remote leader; unknown on the oracle.)
//!
//! # Cost
//!
//! Each case boots two VMs of the side's own runtime, so it costs a few seconds; the
//! `cookie_mismatch` case costs the most (~16 s on the Rust side, because every refused
//! handshake there waits out the runtime's 5 s connect timeout instead of failing at the
//! rejection status — itself recorded in `divergences.md`).

use super::{Case, CaseKind, CommandProgram, CommandRun, CommandSpec, Contract, Golden, PeerRun};

/// The name every client in this group registers itself under (the `DOP_REG_SEND` target of
/// the peers' replies).
const CLIENT_NAME: &str = "dxcli";

/// The name every peer registers itself under (the `DOP_REG_SEND` target the clients send
/// to, which is also what makes a client's `send` auto-dial an as-yet-unconnected peer).
const PEER_NAME: &str = "dxpeer";

/// The client node's `-sname` local part; `{nonce}` is resolved per run.
const CLIENT_NODE: &str = "dxcli{nonce}";

/// The peer node's `-sname` local part; `{nonce}` is resolved per run.
const PEER_NODE: &str = "dxpeer{nonce}";

/// The cookie literal a `cookie_mismatch` peer runs with — never the client's, and never
/// compared (only the resulting `pang` is).
const PEER_COOKIE: &str = "dist_extra_peer_cookie";

// Every client script below inlines the same three-step prelude — own the registered name
// `dxcli`, derive the peer's node name from **this** node so no name can reach a golden
// (`"dxpeer{nonce}" ++ lists:nthtail(length("dxcli{nonce}"), atom_to_list(node()))`), and
// `net_adm:ping/1` the peer in a bounded retry (a peer publishes to `epmd` on its first
// scheduler pass, which can lag its ready line). It is a plain comment rather than a shared
// `const` because each script has to stay a single `-eval` line, and the cookie client is the
// one script that registers no name (it never gets a reply).

/// The peer halves. Each registers [`PEER_NAME`], announces readiness, then serves one bounded
/// exchange — every receive carries an `after`, so a peer nobody talks to exits instead of
/// lingering. A peer's stdout is **not** compared; only its ready line matters.
const PEER_IDLE: &str = r#"try register(dxpeer, self()), timer:sleep(200), io:format("PEER-READY ~p~n", [node()]), timer:sleep(20000), halt(0) catch Cls:Rs -> io:format("PEEREXC ~p ~p~n", [Cls, Rs]), halt(1) end."#;

/// Serves only the ready line and stays up for long enough that a refused handshake is a
/// refusal and not a vanished peer (the `cookie_mismatch` client needs several attempts).
const PEER_IDLE_LONG: &str = r#"try register(dxpeer, self()), timer:sleep(200), io:format("PEER-READY ~p~n", [node()]), timer:sleep(120000), halt(0) catch Cls:Rs -> io:format("PEEREXC ~p ~p~n", [Cls, Rs]), halt(1) end."#;

/// Reports what it sees of the sender's pid (all four fields are booleans or the sender's own
/// node name, which the client turns into a boolean).
const PEER_PID: &str = r#"try register(dxpeer, self()), timer:sleep(200), io:format("PEER-READY ~p~n", [node()]), receive {who, From} -> erlang:send(From, {saw_pid, is_pid(From), erlang:node(From), erlang:node(From) =/= node()}) after 10000 -> halt(2) end, timer:sleep(3000), halt(0) catch Cls:Rs -> io:format("PEEREXC ~p ~p~n", [Cls, Rs]), halt(1) end."#;

/// Replies once to the sender's **pid** (`DOP_SEND`), once to the client's **registered
/// name** (`DOP_REG_SEND`), and once more to the pid, so the client can check both ordering
/// and payload routing.
const PEER_PID_AND_NAME: &str = r#"try register(dxpeer, self()), timer:sleep(200), io:format("PEER-READY ~p~n", [node()]), receive {who, From} -> FN = erlang:node(From), erlang:send(From, {to_pid, 1}), erlang:send({dxcli, FN}, {to_name, 2}), erlang:send(From, {to_pid, 3}) after 10000 -> halt(2) end, timer:sleep(3000), halt(0) catch Cls:Rs -> io:format("PEEREXC ~p ~p~n", [Cls, Rs]), halt(1) end."#;

/// Echoes a sequence number back for each of the five round trips, in the order they arrive.
const PEER_SEQ: &str = r#"try register(dxpeer, self()), timer:sleep(200), io:format("PEER-READY ~p~n", [node()]), receive {seq, F1, 1} -> erlang:send(F1, {seq_back, 1}) after 10000 -> halt(2) end, receive {seq, F2, 2} -> erlang:send(F2, {seq_back, 2}) after 10000 -> halt(2) end, receive {seq, F3, 3} -> erlang:send(F3, {seq_back, 3}) after 10000 -> halt(2) end, receive {seq, F4, 4} -> erlang:send(F4, {seq_back, 4}) after 10000 -> halt(2) end, receive {seq, F5, 5} -> erlang:send(F5, {seq_back, 5}) after 10000 -> halt(2) end, timer:sleep(3000), halt(0) catch Cls:Rs -> io:format("PEEREXC ~p ~p~n", [Cls, Rs]), halt(1) end."#;

/// Measures the payload it received **on the far side** (`phash2/1` and the ETF byte size), so
/// the client compares two independent observations of the same term instead of a constant.
const PEER_PAYLOAD: &str = r#"try register(dxpeer, self()), timer:sleep(200), io:format("PEER-READY ~p~n", [node()]), receive {payload, From, Term} -> erlang:send(From, {payload_back, erlang:phash2(Term), byte_size(term_to_binary(Term))}) after 10000 -> halt(2) end, timer:sleep(3000), halt(0) catch Cls:Rs -> io:format("PEEREXC ~p ~p~n", [Cls, Rs]), halt(1) end."#;

/// Answers a later ping only, so the client can prove the link survived a dropped message.
const PEER_STILL_HERE: &str = r#"try register(dxpeer, self()), timer:sleep(200), io:format("PEER-READY ~p~n", [node()]), receive {ping_me, From} -> erlang:send(From, {still_here, ok}) after 10000 -> halt(2) end, timer:sleep(3000), halt(0) catch Cls:Rs -> io:format("PEEREXC ~p ~p~n", [Cls, Rs]), halt(1) end."#;

/// Spawns a process that exits immediately, reports its pid, and then answers a later ping —
/// the target of a `DOP_SEND` to a **dead** remote pid.
const PEER_DEAD_PID: &str = r#"try register(dxpeer, self()), timer:sleep(200), io:format("PEER-READY ~p~n", [node()]), Gone = spawn(fun() -> ok end), timer:sleep(100), receive {dead_pid, From} -> erlang:send(From, {gone, Gone}), receive {ping_me, F2} -> erlang:send(F2, {still_here, ok}) after 10000 -> halt(2) end after 10000 -> halt(2) end, timer:sleep(3000), halt(0) catch Cls:Rs -> io:format("PEEREXC ~p ~p~n", [Cls, Rs]), halt(1) end."#;

/// The `net_kernel:connect_node/1` client: the explicit connect request, then the ping.
const SCRIPT_CONNECT_NODE: &str = r#"try register(dxcli, self()), Peer = list_to_atom("dxpeer{nonce}" ++ lists:nthtail(length("dxcli{nonce}"), atom_to_list(node()))), C = (catch net_kernel:connect_node(Peer)), Pong = case lists:any(fun(_) -> case net_adm:ping(Peer) of pong -> true; _ -> timer:sleep(50), false end end, lists:seq(1, 40)) of true -> pong; false -> pang end, io:format("connect=~p pong=~p~n", [C, Pong]), halt(0) catch Cls:Rs -> io:format("EXC ~p ~p~n", [Cls, Rs]), halt(1) end."#;

/// The missing-node client: the reachable peer answers `pong`, a node nobody runs answers
/// `pang`. The missing name is `nosuchdx` + this node's nonce and host, so it can never be a
/// real node on the machine.
const SCRIPT_MISSING_NODE: &str = r#"try register(dxcli, self()), Peer = list_to_atom("dxpeer{nonce}" ++ lists:nthtail(length("dxcli{nonce}"), atom_to_list(node()))), Pong = case lists:any(fun(_) -> case net_adm:ping(Peer) of pong -> true; _ -> timer:sleep(50), false end end, lists:seq(1, 40)) of true -> pong; false -> pang end, Missing = list_to_atom("nosuchdx" ++ lists:nthtail(length("dxcli"), atom_to_list(node()))), MissingAns = net_adm:ping(Missing), io:format("pong=~p missing=~p~n", [Pong, MissingAns]), halt(0) catch Cls:Rs -> io:format("EXC ~p ~p~n", [Cls, Rs]), halt(1) end."#;

/// The cookie-mismatch client: it runs with its own cookie (see the case's argv), so the
/// handshake is refused and `net_adm:ping/1` must answer `pang`. Three bounded attempts (a
/// refusal is deterministic; the retries only absorb a peer whose `epmd` publication lags).
const SCRIPT_COOKIE_MISMATCH: &str = r#"try Peer = list_to_atom("dxpeer{nonce}" ++ lists:nthtail(length("dxcli{nonce}"), atom_to_list(node()))), Ans = case lists:any(fun(_) -> case net_adm:ping(Peer) of pong -> true; _ -> timer:sleep(100), false end end, lists:seq(1, 3)) of true -> pong; false -> pang end, io:format("ping=~p~n", [Ans]), halt(0) catch Cls:Rs -> io:format("EXC ~p ~p~n", [Cls, Rs]), halt(1) end."#;

/// The pid client: send `self()`, and turn the peer's four observations into booleans.
const SCRIPT_PID: &str = r#"try register(dxcli, self()), Peer = list_to_atom("dxpeer{nonce}" ++ lists:nthtail(length("dxcli{nonce}"), atom_to_list(node()))), Pong = case lists:any(fun(_) -> case net_adm:ping(Peer) of pong -> true; _ -> timer:sleep(50), false end end, lists:seq(1, 40)) of true -> pong; false -> pang end, erlang:send({dxpeer, Peer}, {who, self()}), Msg = receive X -> X after 10000 -> timeout end, Ok = is_tuple(Msg) andalso tuple_size(Msg) =:= 4 andalso element(1, Msg) =:= list_to_atom("saw_pid"), io:format("pong=~p tag=~p far_ispid=~p far_foreign=~p far_node_matches=~p~n", [Pong, Ok, Ok andalso element(2, Msg), Ok andalso element(4, Msg), Ok andalso element(3, Msg) =:= node()]), halt(0) catch Cls:Rs -> io:format("EXC ~p ~p~n", [Cls, Rs]), halt(1) end."#;

/// The interleaved client: three messages from the peer (`DOP_SEND`, `DOP_REG_SEND`,
/// `DOP_SEND`) must arrive in that order, each with its own payload.
const SCRIPT_PID_AND_NAME: &str = r#"try register(dxcli, self()), Peer = list_to_atom("dxpeer{nonce}" ++ lists:nthtail(length("dxcli{nonce}"), atom_to_list(node()))), Pong = case lists:any(fun(_) -> case net_adm:ping(Peer) of pong -> true; _ -> timer:sleep(50), false end end, lists:seq(1, 40)) of true -> pong; false -> pang end, erlang:send({dxpeer, Peer}, {who, self()}), M1 = receive A -> A after 10000 -> timeout end, M2 = receive B -> B after 10000 -> timeout end, M3 = receive D -> D after 10000 -> timeout end, TP = list_to_atom("to_pid"), TN = list_to_atom("to_name"), io:format("pong=~p order=~p pid_num=~p name_num=~p last_num=~p~n", [Pong, is_tuple(M1) andalso element(1, M1) =:= TP andalso element(1, M2) =:= TN andalso element(1, M3) =:= TP, is_tuple(M1) andalso element(2, M1), is_tuple(M2) andalso element(2, M2), is_tuple(M3) andalso element(2, M3)]), halt(0) catch Cls:Rs -> io:format("EXC ~p ~p~n", [Cls, Rs]), halt(1) end."#;

/// The sequencing client: five request/response round trips, checked for delivery count,
/// tags and order.
const SCRIPT_SEQ: &str = r#"try register(dxcli, self()), Peer = list_to_atom("dxpeer{nonce}" ++ lists:nthtail(length("dxcli{nonce}"), atom_to_list(node()))), Pong = case lists:any(fun(_) -> case net_adm:ping(Peer) of pong -> true; _ -> timer:sleep(50), false end end, lists:seq(1, 40)) of true -> pong; false -> pang end, erlang:send({dxpeer, Peer}, {seq, self(), 1}), R1 = receive A -> A after 10000 -> timeout end, erlang:send({dxpeer, Peer}, {seq, self(), 2}), R2 = receive B -> B after 10000 -> timeout end, erlang:send({dxpeer, Peer}, {seq, self(), 3}), R3 = receive C1 -> C1 after 10000 -> timeout end, erlang:send({dxpeer, Peer}, {seq, self(), 4}), R4 = receive D -> D after 10000 -> timeout end, erlang:send({dxpeer, Peer}, {seq, self(), 5}), R5 = receive E -> E after 10000 -> timeout end, Tag = list_to_atom("seq_back"), All = [R1, R2, R3, R4, R5], io:format("pong=~p n=~p all_tags=~p order=~p~n", [Pong, length(All), lists:all(fun(X) -> is_tuple(X) andalso element(1, X) =:= Tag end, All), length(All) =:= 5 andalso element(2, R1) =:= 1 andalso element(2, R2) =:= 2 andalso element(2, R3) =:= 3 andalso element(2, R4) =:= 4 andalso element(2, R5) =:= 5]), halt(0) catch Cls:Rs -> io:format("EXC ~p ~p~n", [Cls, Rs]), halt(1) end."#;

/// The payload client: send one term, compare the peer's `phash2/1` and ETF byte size with
/// the sender's own. `Term` is the only difference between the three payload cases (ten
/// atoms, an 80-element list, a 1 KiB binary).
const SCRIPT_ATOM_PAYLOAD: &str = r#"try register(dxcli, self()), Peer = list_to_atom("dxpeer{nonce}" ++ lists:nthtail(length("dxcli{nonce}"), atom_to_list(node()))), Pong = case lists:any(fun(_) -> case net_adm:ping(Peer) of pong -> true; _ -> timer:sleep(50), false end end, lists:seq(1, 40)) of true -> pong; false -> pang end, Term = lists:map(fun(I) -> list_to_atom("dist_atom_" ++ integer_to_list(I)) end, lists:seq(1, 10)), erlang:send({dxpeer, Peer}, {payload, self(), Term}), Msg = receive X -> X after 10000 -> timeout end, Ok = is_tuple(Msg) andalso tuple_size(Msg) =:= 3 andalso element(1, Msg) =:= list_to_atom("payload_back"), io:format("pong=~p tag=~p hash_ok=~p etf_ok=~p~n", [Pong, Ok, Ok andalso element(2, Msg) =:= erlang:phash2(Term), Ok andalso element(3, Msg) =:= byte_size(term_to_binary(Term))]), halt(0) catch Cls:Rs -> io:format("EXC ~p ~p~n", [Cls, Rs]), halt(1) end."#;

const SCRIPT_LIST_PAYLOAD: &str = r#"try register(dxcli, self()), Peer = list_to_atom("dxpeer{nonce}" ++ lists:nthtail(length("dxcli{nonce}"), atom_to_list(node()))), Pong = case lists:any(fun(_) -> case net_adm:ping(Peer) of pong -> true; _ -> timer:sleep(50), false end end, lists:seq(1, 40)) of true -> pong; false -> pang end, Term = lists:seq(1, 80), erlang:send({dxpeer, Peer}, {payload, self(), Term}), Msg = receive X -> X after 10000 -> timeout end, Ok = is_tuple(Msg) andalso tuple_size(Msg) =:= 3 andalso element(1, Msg) =:= list_to_atom("payload_back"), io:format("pong=~p tag=~p hash_ok=~p etf_ok=~p~n", [Pong, Ok, Ok andalso element(2, Msg) =:= erlang:phash2(Term), Ok andalso element(3, Msg) =:= byte_size(term_to_binary(Term))]), halt(0) catch Cls:Rs -> io:format("EXC ~p ~p~n", [Cls, Rs]), halt(1) end."#;

const SCRIPT_BINARY_PAYLOAD: &str = r#"try register(dxcli, self()), Peer = list_to_atom("dxpeer{nonce}" ++ lists:nthtail(length("dxcli{nonce}"), atom_to_list(node()))), Pong = case lists:any(fun(_) -> case net_adm:ping(Peer) of pong -> true; _ -> timer:sleep(50), false end end, lists:seq(1, 40)) of true -> pong; false -> pang end, Term = list_to_binary(lists:duplicate(1024, 7)), erlang:send({dxpeer, Peer}, {payload, self(), Term}), Msg = receive X -> X after 10000 -> timeout end, Ok = is_tuple(Msg) andalso tuple_size(Msg) =:= 3 andalso element(1, Msg) =:= list_to_atom("payload_back"), io:format("pong=~p tag=~p hash_ok=~p etf_ok=~p~n", [Pong, Ok, Ok andalso element(2, Msg) =:= erlang:phash2(Term), Ok andalso element(3, Msg) =:= byte_size(term_to_binary(Term))]), halt(0) catch Cls:Rs -> io:format("EXC ~p ~p~n", [Cls, Rs]), halt(1) end."#;

/// The missing-name client: a send to a name nobody registered on the peer must be dropped
/// (the send itself still returns the message) and the link must still carry the next one.
const SCRIPT_MISSING_NAME: &str = r#"try register(dxcli, self()), Peer = list_to_atom("dxpeer{nonce}" ++ lists:nthtail(length("dxcli{nonce}"), atom_to_list(node()))), Pong = case lists:any(fun(_) -> case net_adm:ping(Peer) of pong -> true; _ -> timer:sleep(50), false end end, lists:seq(1, 40)) of true -> pong; false -> pang end, S = (catch erlang:send({nosuchname, Peer}, {dropped, 1})), erlang:send({dxpeer, Peer}, {ping_me, self()}), Msg = receive X -> X after 10000 -> timeout end, io:format("pong=~p send_ok=~p survived=~p~n", [Pong, is_tuple(S), is_tuple(Msg) andalso element(1, Msg) =:= list_to_atom("still_here")]), halt(0) catch Cls:Rs -> io:format("EXC ~p ~p~n", [Cls, Rs]), halt(1) end."#;

/// The dead-pid client: send to a pid of a process that has already exited on the peer, then
/// prove the link still works.
const SCRIPT_DEAD_PID: &str = r#"try register(dxcli, self()), Peer = list_to_atom("dxpeer{nonce}" ++ lists:nthtail(length("dxcli{nonce}"), atom_to_list(node()))), Pong = case lists:any(fun(_) -> case net_adm:ping(Peer) of pong -> true; _ -> timer:sleep(50), false end end, lists:seq(1, 40)) of true -> pong; false -> pang end, erlang:send({dxpeer, Peer}, {dead_pid, self()}), Msg = receive X -> X after 10000 -> timeout end, Gone = element(2, Msg), S = (catch erlang:send(Gone, {boo})), erlang:send({dxpeer, Peer}, {ping_me, self()}), Msg2 = receive Y -> Y after 10000 -> timeout end, io:format("pong=~p gone_ispid=~p send_ok=~p survived=~p~n", [Pong, is_pid(Gone), is_tuple(S), is_tuple(Msg2) andalso element(1, Msg2) =:= list_to_atom("still_here")]), halt(0) catch Cls:Rs -> io:format("EXC ~p ~p~n", [Cls, Rs]), halt(1) end."#;

/// The widened A.5 live two-node group: eleven self-pair cases over the `DOP_*` message
/// families, the payload shapes and the refusal paths.
pub(crate) fn group() -> Vec<Case> {
    vec![
        Case {
            id: "dist_extra/net_kernel_connect_node",
            description: "net_kernel:connect_node/1 establishes the link (true) and the same \
                          peer then answers net_adm:ping/1 with pong",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Command(CommandRun {
                oracle: CommandSpec {
                    program: CommandProgram::Oracle,
                    args: &["-noshell", "-sname", CLIENT_NODE, "-eval", SCRIPT_CONNECT_NODE],
                },
                rust: CommandSpec {
                    program: CommandProgram::Rust,
                    args: &["-noshell", "-sname", CLIENT_NODE, "-eval", SCRIPT_CONNECT_NODE],
                },
                peer: Some(PeerRun {
                    args: &["-noshell", "-sname", PEER_NODE, "-eval", PEER_IDLE],
                    ready_prefix: "PEER-READY",
                }),
            }),
            golden: Golden {
                stdout: Some("connect=true pong=pong"),
                exit_code: 0,
            },
        },
        Case {
            id: "dist_extra/ping_a_missing_node",
            description: "a reachable peer answers pong while a node nobody runs answers pang",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Command(CommandRun {
                oracle: CommandSpec {
                    program: CommandProgram::Oracle,
                    args: &["-noshell", "-sname", CLIENT_NODE, "-eval", SCRIPT_MISSING_NODE],
                },
                rust: CommandSpec {
                    program: CommandProgram::Rust,
                    args: &["-noshell", "-sname", CLIENT_NODE, "-eval", SCRIPT_MISSING_NODE],
                },
                peer: Some(PeerRun {
                    args: &["-noshell", "-sname", PEER_NODE, "-eval", PEER_IDLE],
                    ready_prefix: "PEER-READY",
                }),
            }),
            golden: Golden {
                stdout: Some("pong=pong missing=pang"),
                exit_code: 0,
            },
        },
        Case {
            id: "dist_extra/cookie_mismatch",
            description: "a peer started with a different cookie refuses the handshake, so \
                          net_adm:ping/1 answers pang on both sides",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Command(CommandRun {
                oracle: CommandSpec {
                    program: CommandProgram::Oracle,
                    args: &[
                        "-noshell",
                        "-setcookie",
                        PEER_COOKIE,
                        "-sname",
                        CLIENT_NODE,
                        "-eval",
                        SCRIPT_COOKIE_MISMATCH,
                    ],
                },
                rust: CommandSpec {
                    program: CommandProgram::Rust,
                    args: &[
                        "-noshell",
                        "-setcookie",
                        PEER_COOKIE,
                        "-sname",
                        CLIENT_NODE,
                        "-eval",
                        SCRIPT_COOKIE_MISMATCH,
                    ],
                },
                peer: Some(PeerRun {
                    args: &[
                        "-noshell",
                        "-setcookie",
                        "dist_extra_client_cookie",
                        "-sname",
                        PEER_NODE,
                        "-eval",
                        PEER_IDLE_LONG,
                    ],
                    ready_prefix: "PEER-READY",
                }),
            }),
            golden: Golden {
                stdout: Some("ping=pang"),
                exit_code: 0,
            },
        },
        Case {
            id: "dist_extra/pid_crosses_the_wire_as_a_foreign_pid",
            description: "a pid sent to the peer arrives there as a pid of the sender's own \
                          node (is_pid true, that node, and not the receiver's)",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Command(CommandRun {
                oracle: CommandSpec {
                    program: CommandProgram::Oracle,
                    args: &["-noshell", "-sname", CLIENT_NODE, "-eval", SCRIPT_PID],
                },
                rust: CommandSpec {
                    program: CommandProgram::Rust,
                    args: &["-noshell", "-sname", CLIENT_NODE, "-eval", SCRIPT_PID],
                },
                peer: Some(PeerRun {
                    args: &["-noshell", "-sname", PEER_NODE, "-eval", PEER_PID],
                    ready_prefix: "PEER-READY",
                }),
            }),
            golden: Golden {
                stdout: Some(
                    "pong=pong tag=true far_ispid=true far_foreign=true far_node_matches=true",
                ),
                exit_code: 0,
            },
        },
        Case {
            id: "dist_extra/send_to_a_pid_and_to_a_registered_name",
            description: "DOP_SEND, DOP_REG_SEND and DOP_SEND interleaved on one link arrive \
                          in order with their payloads",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Command(CommandRun {
                oracle: CommandSpec {
                    program: CommandProgram::Oracle,
                    args: &["-noshell", "-sname", CLIENT_NODE, "-eval", SCRIPT_PID_AND_NAME],
                },
                rust: CommandSpec {
                    program: CommandProgram::Rust,
                    args: &["-noshell", "-sname", CLIENT_NODE, "-eval", SCRIPT_PID_AND_NAME],
                },
                peer: Some(PeerRun {
                    args: &["-noshell", "-sname", PEER_NODE, "-eval", PEER_PID_AND_NAME],
                    ready_prefix: "PEER-READY",
                }),
            }),
            golden: Golden {
                stdout: Some("pong=pong order=true pid_num=1 name_num=2 last_num=3"),
                exit_code: 0,
            },
        },
        Case {
            id: "dist_extra/five_ordered_round_trips",
            description: "five request/response round trips over one link: every reply \
                          arrives, in order, tagged with its own sequence number",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Command(CommandRun {
                oracle: CommandSpec {
                    program: CommandProgram::Oracle,
                    args: &["-noshell", "-sname", CLIENT_NODE, "-eval", SCRIPT_SEQ],
                },
                rust: CommandSpec {
                    program: CommandProgram::Rust,
                    args: &["-noshell", "-sname", CLIENT_NODE, "-eval", SCRIPT_SEQ],
                },
                peer: Some(PeerRun {
                    args: &["-noshell", "-sname", PEER_NODE, "-eval", PEER_SEQ],
                    ready_prefix: "PEER-READY",
                }),
            }),
            golden: Golden {
                stdout: Some("pong=pong n=5 all_tags=true order=true"),
                exit_code: 0,
            },
        },
        Case {
            id: "dist_extra/atom_list_payload",
            description: "ten distinct atoms cross the link: the far side's phash2/1 and ETF \
                          byte size match the sender's",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Command(CommandRun {
                oracle: CommandSpec {
                    program: CommandProgram::Oracle,
                    args: &["-noshell", "-sname", CLIENT_NODE, "-eval", SCRIPT_ATOM_PAYLOAD],
                },
                rust: CommandSpec {
                    program: CommandProgram::Rust,
                    args: &["-noshell", "-sname", CLIENT_NODE, "-eval", SCRIPT_ATOM_PAYLOAD],
                },
                peer: Some(PeerRun {
                    args: &["-noshell", "-sname", PEER_NODE, "-eval", PEER_PAYLOAD],
                    ready_prefix: "PEER-READY",
                }),
            }),
            golden: Golden {
                stdout: Some("pong=pong tag=true hash_ok=true etf_ok=true"),
                exit_code: 0,
            },
        },
        Case {
            id: "dist_extra/list_payload",
            description: "an 80-element integer list crosses the link: the far side's \
                          phash2/1 and ETF byte size match the sender's",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Command(CommandRun {
                oracle: CommandSpec {
                    program: CommandProgram::Oracle,
                    args: &["-noshell", "-sname", CLIENT_NODE, "-eval", SCRIPT_LIST_PAYLOAD],
                },
                rust: CommandSpec {
                    program: CommandProgram::Rust,
                    args: &["-noshell", "-sname", CLIENT_NODE, "-eval", SCRIPT_LIST_PAYLOAD],
                },
                peer: Some(PeerRun {
                    args: &["-noshell", "-sname", PEER_NODE, "-eval", PEER_PAYLOAD],
                    ready_prefix: "PEER-READY",
                }),
            }),
            golden: Golden {
                stdout: Some("pong=pong tag=true hash_ok=true etf_ok=true"),
                exit_code: 0,
            },
        },
        Case {
            id: "dist_extra/binary_payload",
            description: "a 1 KiB binary crosses the link: the far side's phash2/1 and ETF \
                          byte size match the sender's",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Command(CommandRun {
                oracle: CommandSpec {
                    program: CommandProgram::Oracle,
                    args: &["-noshell", "-sname", CLIENT_NODE, "-eval", SCRIPT_BINARY_PAYLOAD],
                },
                rust: CommandSpec {
                    program: CommandProgram::Rust,
                    args: &["-noshell", "-sname", CLIENT_NODE, "-eval", SCRIPT_BINARY_PAYLOAD],
                },
                peer: Some(PeerRun {
                    args: &["-noshell", "-sname", PEER_NODE, "-eval", PEER_PAYLOAD],
                    ready_prefix: "PEER-READY",
                }),
            }),
            golden: Golden {
                stdout: Some("pong=pong tag=true hash_ok=true etf_ok=true"),
                exit_code: 0,
            },
        },
        Case {
            id: "dist_extra/send_to_a_missing_remote_name",
            description: "a send to a name nobody registered on the peer is dropped and the \
                          link still carries the next message",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Command(CommandRun {
                oracle: CommandSpec {
                    program: CommandProgram::Oracle,
                    args: &["-noshell", "-sname", CLIENT_NODE, "-eval", SCRIPT_MISSING_NAME],
                },
                rust: CommandSpec {
                    program: CommandProgram::Rust,
                    args: &["-noshell", "-sname", CLIENT_NODE, "-eval", SCRIPT_MISSING_NAME],
                },
                peer: Some(PeerRun {
                    args: &["-noshell", "-sname", PEER_NODE, "-eval", PEER_STILL_HERE],
                    ready_prefix: "PEER-READY",
                }),
            }),
            golden: Golden {
                stdout: Some("pong=pong send_ok=true survived=true"),
                exit_code: 0,
            },
        },
        Case {
            id: "dist_extra/send_to_a_dead_remote_pid",
            description: "a send to a pid of an already-exited process on the peer is dropped \
                          and the link still carries the next message",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Command(CommandRun {
                oracle: CommandSpec {
                    program: CommandProgram::Oracle,
                    args: &["-noshell", "-sname", CLIENT_NODE, "-eval", SCRIPT_DEAD_PID],
                },
                rust: CommandSpec {
                    program: CommandProgram::Rust,
                    args: &["-noshell", "-sname", CLIENT_NODE, "-eval", SCRIPT_DEAD_PID],
                },
                peer: Some(PeerRun {
                    args: &["-noshell", "-sname", PEER_NODE, "-eval", PEER_DEAD_PID],
                    ready_prefix: "PEER-READY",
                }),
            }),
            golden: Golden {
                stdout: Some("pong=pong gone_ispid=true send_ok=true survived=true"),
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

    /// The measured goldens, one per case id: each is the **oracle's** own stdout line,
    /// measured by running the case (`differential run <id>` prints it as `oracle: … stdout=…`).
    const MEASURED: &[(&str, &str)] = &[
        ("dist_extra/net_kernel_connect_node", "connect=true pong=pong"),
        ("dist_extra/missing_node_pang", ""),
        ("dist_extra/ping_a_missing_node", "pong=pong missing=pang"),
        ("dist_extra/cookie_mismatch", "ping=pang"),
        (
            "dist_extra/pid_crosses_the_wire_as_a_foreign_pid",
            "pong=pong tag=true far_ispid=true far_foreign=true far_node_matches=true",
        ),
        (
            "dist_extra/send_to_a_pid_and_to_a_registered_name",
            "pong=pong order=true pid_num=1 name_num=2 last_num=3",
        ),
        ("dist_extra/five_ordered_round_trips", "pong=pong n=5 all_tags=true order=true"),
        ("dist_extra/atom_list_payload", "pong=pong tag=true hash_ok=true etf_ok=true"),
        ("dist_extra/list_payload", "pong=pong tag=true hash_ok=true etf_ok=true"),
        ("dist_extra/binary_payload", "pong=pong tag=true hash_ok=true etf_ok=true"),
        (
            "dist_extra/send_to_a_missing_remote_name",
            "pong=pong send_ok=true survived=true",
        ),
        (
            "dist_extra/send_to_a_dead_remote_pid",
            "pong=pong gone_ispid=true send_ok=true survived=true",
        ),
    ];

    /// The ids in registry order (the corpus order is part of the report).
    const IDS: &[&str] = &[
        "dist_extra/net_kernel_connect_node",
        "dist_extra/ping_a_missing_node",
        "dist_extra/cookie_mismatch",
        "dist_extra/pid_crosses_the_wire_as_a_foreign_pid",
        "dist_extra/send_to_a_pid_and_to_a_registered_name",
        "dist_extra/five_ordered_round_trips",
        "dist_extra/atom_list_payload",
        "dist_extra/list_payload",
        "dist_extra/binary_payload",
        "dist_extra/send_to_a_missing_remote_name",
        "dist_extra/send_to_a_dead_remote_pid",
    ];

    /// Every case is a `Command` **self-pair**: oracle side the resolved `erl`, Rust side the
    /// runtime itself (the point is that *the runtime* does the two-VM traffic), a peer leg,
    /// and `A.5` as the only contract.
    #[test]
    fn the_group_is_a_set_of_a5_self_pairs() {
        let cases = group();
        assert_eq!(cases.len(), IDS.len(), "every recorded case is in the registry");
        let ids: Vec<&str> = cases.iter().map(|case| case.id).collect();
        assert_eq!(ids, IDS);
        for case in &cases {
            assert_eq!(case.contracts, &[Contract::Distribution], "{}", case.id);
            assert_eq!(case.golden.exit_code, 0, "{}", case.id);
            assert!(!case.description.is_empty(), "{}", case.id);
            let CaseKind::Command(run) = &case.kind else {
                panic!("{} must be a Command case", case.id);
            };
            assert!(matches!(run.oracle.program, CommandProgram::Oracle), "{}", case.id);
            assert!(
                matches!(run.rust.program, CommandProgram::Rust),
                "{}: the runtime itself does the two-VM traffic",
                case.id
            );
            assert_eq!(
                run.rust.args, run.oracle.args,
                "{}: both sides run the same client argv (only the program differs)",
                case.id
            );
            let peer = run.peer.unwrap_or_else(|| panic!("{} must carry a peer leg", case.id));
            assert_eq!(peer.ready_prefix, "PEER-READY", "{}", case.id);
            assert!(!run.oracle_command_line().is_empty(), "{}", case.id);
        }
    }

    /// Every node name is per-invocation: all three argvs carry the placeholder, and the
    /// client's own expression derives the peer's name from the *same* placeholder (so a real
    /// node name can never reach a golden).
    #[test]
    fn every_node_name_is_nonce_derived() {
        for case in group() {
            let CaseKind::Command(run) = &case.kind else {
                panic!("{} must be a Command case", case.id);
            };
            let peer = run.peer.expect("a peer leg");
            for args in [run.oracle.args, run.rust.args, peer.args] {
                assert!(
                    args.iter().any(|arg| arg.contains(PEER_NONCE_PLACEHOLDER)),
                    "{}: every arg list must carry the nonce placeholder",
                    case.id
                );
            }
            // The client's `-sname` value and the peer's must differ (different nodes) …
            let client_node = args_of(run.oracle.args, "-sname");
            let peer_node = args_of(peer.args, "-sname");
            assert_ne!(client_node, peer_node, "{}", case.id);
            assert!(client_node.contains(PEER_NONCE_PLACEHOLDER), "{}", case.id);
            assert!(peer_node.contains(PEER_NONCE_PLACEHOLDER), "{}", case.id);
            // … and the client's expression must name the peer through the placeholder too.
            let expr = args_of(run.oracle.args, "-eval");
            assert!(
                expr.contains("dxpeer{nonce}"),
                "{}: the client derives the peer name from the nonce",
                case.id
            );
            assert!(
                expr.contains("length(\"dxcli{nonce}\")"),
                "{}: the derivation is relative to the client's own node name",
                case.id
            );
        }
    }

    /// The substitution really resolves every occurrence in both halves and leaves the rest of
    /// the argv alone — the mechanism the two node names depend on.
    #[test]
    fn expand_nonce_resolves_the_client_and_peer_halves() {
        let client = expand_nonce(&["-noshell", "-sname", super::CLIENT_NODE], "abc123");
        assert_eq!(client, vec!["-noshell", "-sname", "dxcliabc123"]);
        let peer = expand_nonce(&["-noshell", "-sname", super::PEER_NODE], "abc123");
        assert_eq!(peer, vec!["-noshell", "-sname", "dxpeerabc123"]);
        // The derivation `length("dxcli{nonce}")` measures the *expanded* client name, which is
        // what makes `nthtail/2` skip exactly it.
        let expr = expand_nonce(
            &[r#"Peer = list_to_atom("dxpeer{nonce}" ++ lists:nthtail(length("dxcli{nonce}"), atom_to_list(node())))"#],
            "abc123",
        );
        assert_eq!(
            expr[0],
            r#"Peer = list_to_atom("dxpeerabc123" ++ lists:nthtail(length("dxcliabc123"), atom_to_list(node())))"#
        );
        // `abc123` is a stand-in: the real nonce is `peer_nonce`'s output (tested there).
        assert!(client[2].starts_with("dxcli"));
    }

    /// Each golden is the **measured oracle line** — no invented value, and the table above is
    /// the single place the numbers live.
    #[test]
    fn every_golden_is_the_measured_oracle_line() {
        for case in group() {
            let Some((_, expected)) = MEASURED
                .iter()
                .find(|(id, _)| *id == case.id)
                .copied()
                .filter(|(_, golden)| !golden.is_empty())
            else {
                continue;
            };
            assert_eq!(
                case.golden.stdout,
                Some(expected),
                "{}: the golden must be the oracle's measured stdout line",
                case.id
            );
        }
    }

    /// No golden may carry an order-unstable or per-VM-start observable: no node name (`@`), no
    /// pid/port/ref rendering (`<…>`), no placeholder, and no timing.
    #[test]
    fn no_golden_carries_a_volatile_observable() {
        for case in group() {
            let golden = case.golden.stdout.expect("every case asserts a stdout line");
            for forbidden in ["@", "<", ">", PEER_NONCE_PLACEHOLDER, "node", "ms", "timeout"] {
                assert!(
                    !golden.contains(forbidden),
                    "{}: the golden {golden:?} must not contain {forbidden:?}",
                    case.id
                );
            }
            assert!(!golden.starts_with(' ') && !golden.ends_with(' '), "{}", case.id);
            assert!(!golden.ends_with('\n'), "{}: a golden is a line, not a line + \\n", case.id);
        }
        assert_eq!(MEASURED.len(), IDS.len(), "one measured line per case");
    }

    /// The clients are `-eval` scripts under the oracle's `erl_eval`, whose silent-rejection
    /// shapes are a corpus-drift trap (no stdout, exit `1`): catch as `Cls:Rs` (never `C:R`,
    /// which collides with a body binding), bound every `receive`, and never enclose a
    /// `receive` pattern with a quoted atom or an already-bound variable (the clients classify
    /// a bare-variable receive with `is_tuple/1` + `element/2` instead).
    #[test]
    fn every_script_is_an_evaluable_bounded_probe() {
        for case in group() {
            let CaseKind::Command(run) = &case.kind else {
                panic!("{} must be a Command case", case.id);
            };
            let peer = run.peer.expect("a peer leg");
            let client = args_of(run.oracle.args, "-eval");
            assert!(client.starts_with("try "), "{}", case.id);
            assert!(client.contains("catch Cls:Rs ->"), "{}: catch as Cls:Rs", case.id);
            assert!(peer.contains("catch Cls:Rs ->"), "{}: catch as Cls:Rs", case.id);
            for script in [client, peer] {
                assert!(!script.contains("receive {"), "{}: no tuple pattern in a receive", case.id);
                assert!(!script.contains("receive {'"), "{}: no quoted-atom pattern", case.id);
                assert!(script.contains(" after "), "{}: every receive is bounded", case.id);
                assert!(script.contains("halt(0)"), "{}", case.id);
                assert!(!script.contains("nosuchname{nonce}"), "{}", case.id);
            }
            // The registered names are the two the scripts agree on.
            assert!(client.contains("register(dxcli, self())"), "{}", case.id);
            assert!(peer.contains("register(dxpeer, self())"), "{}", case.id);
        }
        assert_eq!(super::CLIENT_NAME, "dxcli");
        assert_eq!(super::PEER_NAME, "dxpeer");
    }

    /// The payload cases stay inside the **measured** agreeing envelope: a 100-element list and
    /// a 1 KiB binary round-trip, while a 128-element list and a 128 KiB binary kill the
    /// receiving Rust VM (see the module docs and `divergences.md` §A.5). A future widening of
    /// the payloads must be re-measured, so the sizes are pinned here.
    #[test]
    fn the_payload_cases_stay_inside_the_measured_envelope() {
        let cases = group();
        let list = cases
            .iter()
            .find(|case| case.id == "dist_extra/list_payload")
            .expect("the list-payload case");
        let CaseKind::Command(run) = &list.kind else {
            panic!("list_payload must be a Command case");
        };
        assert!(
            args_of(run.oracle.args, "-eval").contains("lists:seq(1, 80)"),
            "the list payload is 80 elements (100 is the measured ceiling, 128 fails)"
        );
        let binary = cases
            .iter()
            .find(|case| case.id == "dist_extra/binary_payload")
            .expect("the binary-payload case");
        let CaseKind::Command(run) = &binary.kind else {
            panic!("binary_payload must be a Command case");
        };
        assert!(
            args_of(run.oracle.args, "-eval").contains("lists:duplicate(1024, 7)"),
            "the binary payload is 1 KiB (128 KiB is the measured failure)"
        );
    }

    /// The cookie case is the only one whose two halves run with **different** cookies, and its
    /// peer outlives the client's retries.
    #[test]
    fn the_cookie_case_runs_both_halves_with_different_cookies() {
        let cases = group();
        let cookie = cases
            .iter()
            .find(|case| case.id == "dist_extra/cookie_mismatch")
            .expect("the cookie case");
        let CaseKind::Command(run) = &cookie.kind else {
            panic!("cookie_mismatch must be a Command case");
        };
        let client_cookie = args_of(run.oracle.args, "-setcookie");
        let peer = run.peer.expect("a peer leg");
        let peer_cookie = args_of(peer.args, "-setcookie");
        assert_ne!(client_cookie, peer_cookie, "the cookies must differ");
        assert_eq!(client_cookie, super::PEER_COOKIE);
        assert!(
            peer.args.iter().any(|arg| arg.contains("PEER_IDLE_LONG").not()),
            "the peer script is carried as an argv, not a name"
        );
        for other in &cases {
            if other.id == cookie.id {
                continue;
            }
            let CaseKind::Command(run) = &other.kind else {
                panic!("{} must be a Command case", other.id);
            };
            assert!(
                !run.oracle.args.contains(&"-setcookie"),
                "{}: only the cookie case sets a cookie",
                other.id
            );
        }
    }

    /// The value that follows `flag` in an argv (the corpus keeps `-sname`/`-eval`/`-setcookie`
    /// as separate argv words).
    fn args_of(args: &[&'static str], flag: &str) -> &'static str {
        let index = args
            .iter()
            .position(|arg| *arg == flag)
            .unwrap_or_else(|| panic!("no {flag} in {args:?}"));
        args[index + 1]
    }
}
