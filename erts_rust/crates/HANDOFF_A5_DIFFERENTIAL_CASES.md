# HANDOFF — land three A.5 inbound-signal differential cases

**Status:** ready to paste; **delete this file once landed.**
**Owner of the target directory:** `erts_rust/tools/differential` (a **sibling** of this node —
a nested agent may not write there, which is the only reason this is a handoff).

The runtime work these cases pin is **complete and merged** in this node: all four inbound
signal families agree with the oracle both rust↔real-`erl` and rust↔rust
(`beam_rt/tests/dist_two_vm.rs` 24 cases, `tests/dist_two_rust.rs` 12 cases). The harness itself
therefore needs **no** change — only three corpus entries, two table rows each, and the deletion
of one stale paragraph.

## 1. Where the cases go

**`src/corpus/dist_extra.rs`**, ids `dist_extra/…` — that is the widened live two-node self-pair
group, and its guards are table-driven (`MEASURED`/`IDS`), so a new case costs one literal plus
two table rows.

`src/corpus/dist.rs` is the helper-based `dist_probe` loopback group and pins
`assert_eq!(cases.len(), 5)` at `:323`; use it only if the exact `dist/…` id strings are
required (then bump that number to 8 and extend the id array at `:327-333`; **no** `case_action`
arm is needed — `case_action` is only called in the peer-less branch).

Supported shape (the only one the harness allows): both clients carry the same argv, the peer is
started from *the side's own program* (`harness::observe_side` →
`runtime::spawn_peer`), so it is oracle↔oracle vs rust↔rust; every argv carries `{nonce}`; only
the **client's** stdout is compared.

## 2. Case 1 — `dist_extra/inbound_link_exit_delivers_the_peers_reason`

Pins both landed halves: the peer's `DOP_LINK` is **recorded** (the client's own
`process_info(self(), links)` gains a pid) and the linked peer's death delivers
`{'EXIT', Pid, boom}` with the peer's **real reason** — never `noproc`. The
ack → `links` → `{die}` handshake removes the "peer died before the link was observed" race, so
no wall-clock sleep orders anything.

```rust
/// The **inbound-link** peer: spawn a linker process that links to the client (`DOP_LINK` on
/// the wire *before* its ack, which carries `link/1`'s own return value), then — on the
/// client's `{die}` — exit `boom`, so the client's trap delivers the peer's real reason. The
/// spawned process keeps the peer VM alive while the exit signal is written.
const PEER_LINK_EXIT: &str = r#"try register(dxpeer, self()), timer:sleep(200), io:format("PEER-READY ~p~n", [node()]), receive {link_me, From} -> C = spawn(fun() -> L = erlang:link(From), erlang:send(From, {linked, L}), receive die -> exit(boom) after 25000 -> exit(no_die) end end), receive {die} -> erlang:send(C, die) after 25000 -> halt(2) end after 25000 -> halt(2) end, timer:sleep(3000), halt(0) catch Cls:Rs -> io:format("PEEREXC ~p ~p~n", [Cls, Rs]), halt(1) end."#;

/// The **inbound-link** client: trap exits, hand its pid over, wait for the peer's `link/1`
/// ack, watch its own `links` list gain a pid (the recorded inbound link), then ask the peer
/// to die and classify the `{'EXIT', Pid, Reason}` — reason `boom`, pid foreign.
const SCRIPT_LINK_EXIT: &str = r#"try register(dxcli, self()), Peer = list_to_atom("dxpeer{nonce}" ++ lists:nthtail(length("dxcli{nonce}"), atom_to_list(node()))), Pong = case lists:any(fun(_) -> case net_adm:ping(Peer) of pong -> true; _ -> timer:sleep(50), false end end, lists:seq(1, 40)) of true -> pong; false -> pang end, process_flag(trap_exit, true), erlang:send({dxpeer, Peer}, {link_me, self()}), Ack = receive A -> A after 10000 -> timeout end, Acked = is_tuple(Ack) andalso tuple_size(Ack) =:= 2 andalso element(1, Ack) =:= list_to_atom("linked") andalso element(2, Ack) =:= true, Linked = lists:any(fun(_) -> timer:sleep(50), case process_info(self(), links) of {links, L} -> length(L) >= 1; _ -> false end end, lists:seq(1, 40)), erlang:send({dxpeer, Peer}, {die}), Msg = receive X -> X after 20000 -> timeout end, Ok = is_tuple(Msg) andalso tuple_size(Msg) =:= 3, io:format("pong=~p acked=~p links=~p exit=~p tag=~p reason=~p foreign=~p~n", [Pong, Acked, Linked, Ok, Ok andalso element(1, Msg) =:= list_to_atom("EXIT"), Ok andalso element(3, Msg) =:= boom, Ok andalso is_pid(element(2, Msg)) andalso erlang:node(element(2, Msg)) =:= Peer]), halt(0) catch Cls:Rs -> io:format("EXC ~p ~p~n", [Cls, Rs]), halt(1) end."#;

        Case {
            id: "dist_extra/inbound_link_exit_delivers_the_peers_reason",
            description: "a peer's inbound link is recorded (the client's own links list gains \
                          a pid) and the linked peer's death delivers {'EXIT',Pid,boom} with \
                          the peer's real reason, never noproc",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Command(CommandRun {
                oracle: CommandSpec {
                    program: CommandProgram::Oracle,
                    args: &["-noshell", "-sname", CLIENT_NODE, "-eval", SCRIPT_LINK_EXIT],
                },
                rust: CommandSpec {
                    program: CommandProgram::Rust,
                    args: &["-noshell", "-sname", CLIENT_NODE, "-eval", SCRIPT_LINK_EXIT],
                },
                peer: Some(PeerRun {
                    args: &["-noshell", "-sname", PEER_NODE, "-eval", PEER_LINK_EXIT],
                    ready_prefix: "PEER-READY",
                }),
            }),
            golden: Golden {
                stdout: Some(
                    "pong=pong acked=true links=true exit=true tag=true reason=true foreign=true",
                ),
                exit_code: 0,
            },
        },
```

## 3. Case 2 — `dist_extra/inbound_monitor_gets_a_down_when_the_watched_process_dies`

The peer arms `erlang:monitor(process, ClientPid)` — the **inbound** `DOP_MONITOR_P` — acks
with its own `is_reference(Ref)`, and the client then lets the local process die with a **boxed**
reason `{down_reason, 42}`. The peer classifies the `{'DOWN', Ref, process, Pid, Reason}` it
receives and reports booleans back, so no pid/ref is ever rendered. Ack-then-go ordering
guarantees the watcher was armed before the process dies (a `noproc` DOWN would show as
`reason=false`).

```rust
/// The **inbound-monitor** peer: arm a monitor on the client-side process (`DOP_MONITOR_P`),
/// ack with the monitor's own reference check, then classify the `DOWN` it receives and send
/// the booleans back to the client's registered name.
const PEER_MONITOR: &str = r#"try register(dxpeer, self()), timer:sleep(200), io:format("PEER-READY ~p~n", [node()]), receive {watch, Pid} -> Ref = erlang:monitor(process, Pid), N = erlang:node(Pid), erlang:send({dxcli, N}, {watching, is_reference(Ref)}), D = receive X -> X after 25000 -> timeout end, Ok = is_tuple(D) andalso tuple_size(D) =:= 5, erlang:send({dxcli, N}, {saw_down, Ok, Ok andalso element(1, D) =:= list_to_atom("DOWN"), Ok andalso element(3, D) =:= process, Ok andalso is_reference(element(2, D)), Ok andalso is_pid(element(4, D)), Ok andalso element(5, D) =:= {down_reason, 42}}) after 25000 -> halt(2) end, timer:sleep(3000), halt(0) catch Cls:Rs -> io:format("PEEREXC ~p ~p~n", [Cls, Rs]), halt(1) end."#;

/// The **inbound-monitor** client: spawn a doomed process, hand its pid to the peer, wait for
/// the peer's monitor ack, let the process die with a boxed reason, then print the peer's
/// classification of the `DOWN` it got — every field a boolean.
const SCRIPT_MONITOR: &str = r#"try register(dxcli, self()), Peer = list_to_atom("dxpeer{nonce}" ++ lists:nthtail(length("dxcli{nonce}"), atom_to_list(node()))), Pong = case lists:any(fun(_) -> case net_adm:ping(Peer) of pong -> true; _ -> timer:sleep(50), false end end, lists:seq(1, 40)) of true -> pong; false -> pang end, Doomed = spawn(fun() -> receive {go} -> exit({down_reason, 42}) after 15000 -> ok end end), erlang:send({dxpeer, Peer}, {watch, Doomed}), Ack = receive A -> A after 10000 -> timeout end, Acked = is_tuple(Ack) andalso tuple_size(Ack) =:= 2 andalso element(1, Ack) =:= list_to_atom("watching") andalso element(2, Ack) =:= true, erlang:send(Doomed, {go}), Msg = receive X -> X after 20000 -> timeout end, Ok = is_tuple(Msg) andalso tuple_size(Msg) =:= 7 andalso element(1, Msg) =:= list_to_atom("saw_down"), io:format("pong=~p acked=~p down=~p tag=~p type=~p ref=~p pid=~p reason=~p~n", [Pong, Acked, Ok, Ok andalso element(2, Msg), Ok andalso element(3, Msg), Ok andalso element(4, Msg), Ok andalso element(5, Msg), Ok andalso element(6, Msg), Ok andalso element(7, Msg)]), halt(0) catch Cls:Rs -> io:format("EXC ~p ~p~n", [Cls, Rs]), halt(1) end."#;

        Case {
            id: "dist_extra/inbound_monitor_gets_a_down_when_the_watched_process_dies",
            description: "an inbound DOP_MONITOR_P on a local process yields a \
                          {'DOWN',Ref,process,Pid,Reason} for the remote watcher when that \
                          process dies, with the boxed reason intact",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Command(CommandRun {
                oracle: CommandSpec {
                    program: CommandProgram::Oracle,
                    args: &["-noshell", "-sname", CLIENT_NODE, "-eval", SCRIPT_MONITOR],
                },
                rust: CommandSpec {
                    program: CommandProgram::Rust,
                    args: &["-noshell", "-sname", CLIENT_NODE, "-eval", SCRIPT_MONITOR],
                },
                peer: Some(PeerRun {
                    args: &["-noshell", "-sname", PEER_NODE, "-eval", PEER_MONITOR],
                    ready_prefix: "PEER-READY",
                }),
            }),
            golden: Golden {
                stdout: Some(
                    "pong=pong acked=true down=true tag=true type=true ref=true pid=true \
                     reason=true",
                ),
                exit_code: 0,
            },
        },
```

## 4. Case 3 — `dist_extra/inbound_exit2_kill_is_untrappable_killed`

The peer sends an **inbound** `DOP_EXIT2` (`erlang:exit(Pid, kill)`) with **no link**, so the
case also pins that `exit/2` needs none. The doomed local process sets
`process_flag(trap_exit, true)` and would report a trap back as `{trapped, …}`; because `kill`
is untrappable it dies instead, and the client's **local** monitor sees
`{'DOWN',Ref,process,Pid,killed}`. `trapped=false` is a positive assertion that nothing arrived
as a trappable exit.

```rust
/// The **inbound-exit2** peer: fire `erlang:exit(Pid, kill)` at the client-side process — no
/// link is ever made, so only the `DOP_EXIT2` path can terminate it.
const PEER_KILL: &str = r#"try register(dxpeer, self()), timer:sleep(200), io:format("PEER-READY ~p~n", [node()]), receive {kill_it, Pid} -> erlang:exit(Pid, kill) after 25000 -> halt(2) end, timer:sleep(3000), halt(0) catch Cls:Rs -> io:format("PEEREXC ~p ~p~n", [Cls, Rs]), halt(1) end."#;

/// The **inbound-exit2** client: a doomed process that traps exits (so a *trappable* signal
/// would leave it alive and reporting `{trapped, …}`), a local monitor on it, and the pid
/// handed to the peer. The one message it classifies is either the `DOWN` (`kill` ⇒
/// untrappable `killed`) or the trap report.
const SCRIPT_KILL: &str = r#"try register(dxcli, self()), Peer = list_to_atom("dxpeer{nonce}" ++ lists:nthtail(length("dxcli{nonce}"), atom_to_list(node()))), Pong = case lists:any(fun(_) -> case net_adm:ping(Peer) of pong -> true; _ -> timer:sleep(50), false end end, lists:seq(1, 40)) of true -> pong; false -> pang end, Doomed = spawn(fun() -> process_flag(trap_exit, true), M = receive T -> T after 25000 -> no_signal end, erlang:send(dxcli, {trapped, M}) end), erlang:monitor(process, Doomed), erlang:send({dxpeer, Peer}, {kill_it, Doomed}), Msg = receive X -> X after 20000 -> timeout end, Down = is_tuple(Msg) andalso tuple_size(Msg) =:= 5, Trapped = is_tuple(Msg) andalso tuple_size(Msg) =:= 2 andalso element(1, Msg) =:= list_to_atom("trapped"), io:format("pong=~p trapped=~p down=~p tag=~p type=~p ref=~p pid=~p reason=~p~n", [Pong, Trapped, Down, Down andalso element(1, Msg) =:= list_to_atom("DOWN"), Down andalso element(3, Msg) =:= process, Down andalso is_reference(element(2, Msg)), Down andalso is_pid(element(4, Msg)), Down andalso element(5, Msg) =:= killed]), halt(0) catch Cls:Rs -> io:format("EXC ~p ~p~n", [Cls, Rs]), halt(1) end."#;

        Case {
            id: "dist_extra/inbound_exit2_kill_is_untrappable_killed",
            description: "an inbound DOP_EXIT2 with reason kill terminates the local process \
                          even though it traps exits: the watcher's DOWN reason is killed and \
                          no trappable exit is ever delivered",
            contracts: &[Contract::Distribution],
            kind: CaseKind::Command(CommandRun {
                oracle: CommandSpec {
                    program: CommandProgram::Oracle,
                    args: &["-noshell", "-sname", CLIENT_NODE, "-eval", SCRIPT_KILL],
                },
                rust: CommandSpec {
                    program: CommandProgram::Rust,
                    args: &["-noshell", "-sname", CLIENT_NODE, "-eval", SCRIPT_KILL],
                },
                peer: Some(PeerRun {
                    args: &["-noshell", "-sname", PEER_NODE, "-eval", PEER_KILL],
                    ready_prefix: "PEER-READY",
                }),
            }),
            golden: Golden {
                stdout: Some(
                    "pong=pong trapped=false down=true tag=true type=true ref=true pid=true \
                     reason=true",
                ),
                exit_code: 0,
            },
        },
```

`CLIENT_NODE` / `PEER_NODE` are the existing nonce-derived consts at `dist_extra.rs:178-181`.

## 5. Table rows (both, same order)

Append to `MEASURED` (after `:714`) and to `IDS` (after `:733`):

```rust
        (
            "dist_extra/inbound_link_exit_delivers_the_peers_reason",
            "pong=pong acked=true links=true exit=true tag=true reason=true foreign=true",
        ),
        (
            "dist_extra/inbound_monitor_gets_a_down_when_the_watched_process_dies",
            "pong=pong acked=true down=true tag=true type=true ref=true pid=true reason=true",
        ),
        (
            "dist_extra/inbound_exit2_kill_is_untrappable_killed",
            "pong=pong trapped=false down=true tag=true type=true ref=true pid=true reason=true",
        ),
```

```rust
        "dist_extra/inbound_link_exit_delivers_the_peers_reason",
        "dist_extra/inbound_monitor_gets_a_down_when_the_watched_process_dies",
        "dist_extra/inbound_exit2_kill_is_untrappable_killed",
```

Plus three rows in the module-doc "What each case pins" table (`:51-67`).

## 6. The goldens above are EXPECTED, not measured — measure them

The only legitimate source of a `Golden` is the oracle's own stdout. Build both release
binaries (the runtime resolver prefers `target/release`), then per case:

```sh
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml -p differential -p beam_rt --release
erts_rust/target/release/differential run dist_extra/inbound_link_exit_delivers_the_peers_reason
erts_rust/target/release/differential run dist_extra/inbound_monitor_gets_a_down_when_the_watched_process_dies
erts_rust/target/release/differential run dist_extra/inbound_exit2_kill_is_untrappable_killed
```

`run <id> --json` gives `cases[0].oracle.stdout` as exact bytes; strip the trailing `\n` and
that string **is** the golden. If a measured line differs from the expected one above, take the
measured line and investigate the difference — never adjust a runtime assertion to match.

A side that exits `3` classifies as `unsupported` (a recorded gap, never a mismatch); an
agreeing case needs **nothing** in `known-gaps.txt`.

## 7. Guards to touch, in order

| # | `src/corpus/dist_extra.rs` | action |
|---|---|---|
| 1 | `:678` `MEASURED` | add 3 rows |
| 2 | `:718` `IDS` | add the same 3 ids in registry order (`the_group_is_a_set_of_a5_self_pairs` `:740`) |
| 3 | `:851` `no_golden_carries_a_volatile_observable` | no edit; verify no `@ < > {nonce} ms timeout`, no leading/trailing space |
| 4 | `:873` `every_script_is_an_evaluable_bounded_probe` | no edit; scripts already satisfy `try `/`catch Cls:Rs ->`/` after `/`timer:sleep(`/`halt(0)`/`register(dxcli, …)`/`register(dxpeer, …)` |
| 5 | `:773` `every_node_name_is_nonce_derived` | no edit; every argv carries `{nonce}` |
| 6 | `:832` `every_golden_is_the_measured_oracle_line` | satisfied by #1 |
| 7 | `:913` payload envelope, `:966` cookie | no edit |
| 8 | `:51-67` module-doc table | add 3 rows |
| 9 | `:99-126` stale paragraph | **delete** the three now-false claims (see §8) |
| 10 | `src/corpus/mod.rs:1091` `command_cases_are_registered_and_well_formed` | no edit |
| 11 | `tests/oracle.rs:313-360` `command_cases_reproduce_their_golden_under_the_oracle` | no edit; it auto-spawns an oracle peer and re-checks each golden |

Nothing pins the corpus total; `GAPS.md`'s header and the `3322` figures in
`tools/CONTEXT.md` / `tools/differential/CONTEXT.md` are refreshed after a sweep
(`differential merge --from <dir> --allowlist known-gaps.txt --gaps GAPS.md`, never a serial
`suite --gaps`).

## 8. Doc lines that are now factually wrong (delete, don't annotate)

`src/corpus/dist_extra.rs` lines ~99-126 assert the pre-fix state and contradict the merged
runtime:

1. `:99-109` "**`link/1` + `DOP_EXIT` — no direction works**" → both directions work and are
   pinned (`dist_two_vm` 24 / `dist_two_rust` 12, `live_signals.rs`, `live_remote_families.rs`).
   Keep only the still-true fragments: `unlink/1`'s unemittable ack and the ordering residue
   note (now closed).
2. `:110-114` "**`erlang:exit(RemotePid, Reason)` — `unsupported: bif`, and `kill` is silently
   dropped**" → remote `exit/2` is live both ways over the full matrix; `kill` is untrappable
   and rewritten to `killed`.
3. `:115-122` "**`erlang:monitor/2` of a remote process — no direction works**" → the monitor
   family works and is pinned both ways; only the `noproc` reply for an **already-dead**
   target remains a typed gap (no reply channel in `beam_dist`'s `InboundSink`).
4. `:123-126` pid identity round trip / inbound alias → `IDENTITY true` is pinned and an inbound
   `DOP_ALIAS_SEND` is delivered. (Keep the *separate*, still-true fact that this runtime cannot
   **render** a remote pid/ref.)

Also stale in the same vein: `divergences.md:722-736` (the "seventeen self-pair shapes"
preamble and the three bullets at `:726-728`).

## 9. Optional follow-ups found while measuring (not blocking)

- `two_vm`'s `monitor_then_demonitor/1` fixture renders the victim through `pid_text/1`, so the
  demonitor sub-arm is unmeasurable against a `beam_rt` peer (rendering an external pid is a
  deliberate typed gap). Fix the fixture to assert by identity and the arm becomes measurable.
- `src/dist_client/CONTEXT.md` has some pre-existing stale `client.rs:NNN` line references
  outside the sections the ordering fix touched.
- `rustfmt --check` is not clean repo-wide (`tests/support/{dist,two_vm,pair}.rs` plus two
  pre-existing `assert_eq!` sites in `dist_two_rust.rs`); CI does not gate on fmt.
