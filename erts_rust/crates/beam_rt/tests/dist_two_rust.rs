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

//! **Two `beam_rt` nodes** (contract A.5): the runtime's own distribution on *both* ends
//! of a link — its acceptor and its connector, its inbound decode and its outbound encode,
//! with no system ERTS anywhere in the picture.
//!
//! [`tests/dist_two_vm.rs`](../dist_two_vm.rs) is the same evidence against a **system
//! `erl`** peer, and its harness (`tests/support/dist.rs`) is reused here unchanged: two
//! live VMs, one private `epmd`, `setsid`-isolated children, every claim a line one VM
//! printed about the other. Only the peer's **kind** differs
//! ([`PeerKind::Rust`](support::dist::PeerKind)), so the two suites are the same protocol
//! run against the two halves of the same contract:
//!
//! | suite | peer | what a pass proves |
//! |---|---|---|
//! | `dist_two_vm` | system `erl` | this runtime's wire format and signals agree with **ERTS** |
//! | `dist_two_rust` (this file) | `beam_rt` | this runtime's wire format and signals agree with **itself** |
//!
//! Self-agreement is not a tautology, and that is the point of this file: the two VMs run
//! the same code but they do **not** run the same role. One is the `run` action's target
//! (the *responder*, reached by a `-pa`-loaded fixture), the other a second process with
//! its own `epmd` name, its own atom table, its own node table and its own creation. A
//! term therefore has to be encoded by one VM's encoder and decoded by the other VM's
//! decoder, an identifier minted here has to become an *external* identifier there and
//! back to an internal one when it returns, and a signal has to be routed by the sender's
//! link table and applied by the receiver's signal queue. Any bit-level asymmetry between
//! the two roles — an encode/decode round trip that is not an involution, a pid whose
//! internal word does not survive the trip, a signal family served on one side of the wire
//! but not the other — is visible here and invisible to a suite that only ever talks to
//! ERTS, because a real peer is a fixture that is *known* to be right.
//!
//! # A `beam_rt` peer cannot render a *remote* identifier — so the assertions are
//! # term-identity, not text
//!
//! `erlang:pid_to_list/1` of a pid that arrived from another node, and `~p`/`~w` of an
//! external pid or reference, are a typed capability gap in this runtime: an external
//! identifier's leading component is the *receiving* VM's node-table index, which no term
//! carries, so `beam_bifs`'s `external_channel_gap` answers `error:unsupported_bif` and the
//! printer has no external arm. A capability gap is **uncatchable by design** (its reason
//! atom is never interned, so a program's own `try … catch` cannot swallow it), which
//! means the fixture's `pid_text/1`/`ref_text/1` fallbacks do **not** rescue a `beam_rt`
//! peer: printing one would terminate that VM with exit 3.
//!
//! Every actor and responder this file drives therefore asserts identity by **pattern
//! match and `=:=`** and prints only booleans, atoms and reasons:
//!
//! ```text
//! EXIT_MATCH pid=true reason=boom          % the clause bound the linked pid
//! EXIT_IDENTITY true                       % the guard's own `V =:= Victim`
//! DOWN_MATCH ref=bound pid=true reason=…   % `{'DOWN', MRef, process, Victim, R}` matched
//! IDENTITY true                            % the echoed pid IS `self()`
//! ```
//!
//! That is **equal in strength** to the oracle suite's canonical-text comparisons —
//! `pid_to_list` text is only a rendering of an identity, and here the identity itself is
//! compared — and **different in shape**: `dist_two_vm` compares `<node.number.serial>`
//! strings (with the node-table index normalized to the node name), this file compares the
//! terms. Where a pid or reference crosses the wire twice (out and back), the identity
//! check is also a bit-level check: a word that came back different *is* a different pid.
//!
//! The one rendering this file does assert is a **local** one — our own pid as our own
//! `~p` prints it (`SELF`/`ECHO`, [`our_pid_survives_a_round_trip_through_a_rust_peer`]):
//! the term goes out to the peer and comes back, and it must land in this runtime's mailbox
//! as the very same `Eterm` word, i.e. the pid's node part decoded back into the internal
//! form. A renderer divergence there is a *decode* divergence.
//!
//! # What the two VMs must hold up (each is exercised below)
//!
//! * **the connector meets the acceptor, between two `beam_rt` nodes.** Our node dials the
//!   peer (`net_adm:ping/1` → `Runtime::dist_connect` → `DistClient`) while the peer's
//!   `DistNode` accepts — the first time both halves of *this* runtime face each other.
//!   The reverse direction is exercised by every other test in this file: each actor's
//!   first act is `net_adm:ping/1` of *our* node, i.e. this runtime's connector against
//!   this runtime's acceptor with the roles swapped.
//! * **`DOP_REG_SEND`/`DOP_SEND` both ways**, `DOP_LINK` + `DOP_EXIT`, `DOP_MONITOR_P` +
//!   `DOP_MONITOR_P_EXIT`, `DOP_DEMONITOR_P`, `DOP_ALIAS_SEND` — routed by the sender's
//!   `DistLinks` (one table over both halves) and honoured by the receiver.
//! * **the inbound half**: the *peer* acting on a process **we** host — its `link/1` and
//!   `exit/2` (`DOP_LINK` + the `links` item it produces, `DOP_EXIT`, `DOP_EXIT2`) and its
//!   `erlang:monitor/2` (`DOP_MONITOR_P` + the inbound `DOP_MONITOR_P_EXIT`) reaching our own
//!   victims, and our own local monitor reading back the death of a non-trapping one.
//!   Sections 1-7 all have the relationship initiated from the side that hosts it; sections
//!   8-12 are the mirror, and the mirror is a different code path (this runtime's *inbound*
//!   receipt primitives rather than its outbound routing).
//! * **identifier round trips**: a pid inside `{'EXIT', Pid, Reason}` and inside
//!   `{'DOWN', Ref, process, Pid, Reason}` must be `=:=` the pid that was handed over
//!   earlier, and a pid echoed through the peer must come back `=:=` `self()`.
//!
//! # The ordering rule a control signal relies on: a message write drains the queue first
//!
//! Two inbound families are observable only if a control signal the peer creates is applied
//! **before** the next message the peer sends on the same link: the `links` item an inbound
//! `DOP_LINK` produces (the peer's `link/1` then a barrier), and the `monitored_by` item an
//! inbound `DOP_MONITOR_P` produces (the peer's `monitor/2` then a `{monitoring, …}` notice,
//! after which our victim dies). This runtime's `DistSignalRouter` can only *queue* a
//! process-table control signal — it is flushed at the client's next `pump` — while a message
//! to a remote destination is written at the call, so the write is where the ordering exists:
//! `DistClient`'s three message sends (`send_reg_send`/`send_message_pid`/`send_alias`) drain
//! the link's queued control signals (`flush_queued_controls`) before writing their own
//! frame, exactly as ERTS writes a control synchronously when the signal is created under the
//! link lock. The rule and its pinning test are `src/dist_client/CONTEXT.md`'s; both families
//! are pinned below, in both directions (here and against a real `erl` peer in
//! [`tests/dist_two_vm.rs`](../dist_two_vm.rs)).
//!
//! The monitor family's **demonitor** sub-arm is *not measurable* on a `beam_rt` peer: the
//! fixture's `monitor_then_demonitor/1` renders the victim with `pid_text/1`, and
//! `erlang:pid_to_list/1` of an external pid is an **uncatchable** capability gap (below), so
//! the peer dies with exit 3 `beam_rt: unsupported: bif` before the relationship is armed.
//! There is no `ask_victim_rust/2`-based demonitor actor yet.
//!
//! # Running it
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
//!     -p beam_rt --test dist_two_rust -- --ignored --nocapture --test-threads=1
//! ```
//!
//! To run one of them, append `--exact <test-name>` after the `--`. `--nocapture` matters:
//! every child's transcript is echoed on failure *and* on every loud skip. Every test is a
//! two-VM session of a few seconds — the demonitor negative control deliberately spends 2 s
//! in its own expiring window — and `--test-threads=1` keeps a loaded host's timings honest.
//!
//! Nothing here needs the system Erlang toolchain at **run** time (both VMs are this
//! binary), but the fixture is still **compiled** by `erlc` at test time, so a host without
//! `erlc`/`epmd`/`setsid`/`kill` reports a **loud skip** rather than a pass.

// The two-VM harness is compiled into every test target; this target only uses a subset of
// it, so the unused helpers would otherwise warn here.
#[allow(dead_code)]
mod support;

use support::dist::PeerKind;
use support::pair::Pair;

/// This suite's name — every loud skip and every failure message names it.
const BANNER: &str = "dist_two_rust";

// ---------------------------------------------------------------------------------------
// The responders our node runs (`run <fixture.beam> <spec> "<peer node>"`), one per test
// ---------------------------------------------------------------------------------------

/// [`two_rust_nodes_ping_each_other`]: `net_adm:ping/1` in both directions.
const RR_PING_SPEC: &str = "two_vm:rr_ping/1";
/// [`a_rust_peer_links_to_our_process_and_our_exit_reaches_it`]: the peer links to a victim
/// we host, and our node's `DOP_EXIT` reaches it.
const PEER_DRIVES_SPEC: &str = "two_vm:peer_drives/1";
/// [`our_monitor_of_a_rust_peer_process_receives_the_down`]: our `DOP_MONITOR_P` of the
/// peer's victim, and the inbound `DOP_MONITOR_P_EXIT`.
const RV_MONITOR_SPEC: &str = "two_vm:rv_monitor/1";
/// [`our_demonitor_of_a_rust_peer_process_suppresses_the_down`]: the monitor's negative
/// control — `demonitor(Ref, [flush])` before the death.
const RV_DEMONITOR_SPEC: &str = "two_vm:rv_demonitor/1";
/// [`our_remote_exit_kill_kills_a_process_on_a_rust_peer`]: our `erlang:exit/2`.
const RV_EXIT_KILL_SPEC: &str = "two_vm:rv_exit_kill/1";
/// [`an_alias_created_on_one_rust_node_delivers_from_the_other`]: our alias, the peer's
/// send.
const RR_OUR_ALIAS_SPEC: &str = "two_vm:rr_our_alias/1";
/// [`our_pid_survives_a_round_trip_through_a_rust_peer`]: our `self()` out and back.
const IDENTITY_SPEC: &str = "two_vm:identity/1";
/// [`a_rust_peers_death_travels_inbound_and_our_trapping_process_traps_its_compound_reason`]:
/// the **peer** links to a victim *we* host and then dies of a compound reason.
const RR_PEER_LINK_DEATH_SPEC: &str = "two_vm:peer_link_death/1";
/// [`a_rust_peers_death_travels_inbound_and_kills_our_non_trapping_process_with_its_reason`]:
/// the same relationship with `trap_exit` **off**, read back through our own local monitor.
const RR_PEER_LINK_DEATH_PLAIN_SPEC: &str = "two_vm:peer_link_death_plain/1";
/// [`a_rust_peers_exit_calls_travel_inbound_and_split_the_matrix_by_trapping_and_kill`]: the
/// peer's `erlang:exit/2` over the whole `{plain,kill} x {trapping,not}` matrix.
const RR_PEER_EXIT2_SPEC: &str = "two_vm:peer_exit2/1";
/// [`a_rust_peers_link_shows_up_in_the_links_item_and_clears_on_unlink`]: the peer's
/// `link/1`, a barrier, its `unlink/1` and a second barrier, against a victim we host — the
/// responder reports that victim's `process_info(Victim, links)` item after each barrier.
const RR_PEER_DRIVES_LINKS_SPEC: &str = "two_vm:peer_drives_links/1";
/// [`a_rust_peers_monitor_of_our_process_gets_a_down_carrying_the_boxed_reason`]: the peer's
/// `erlang:monitor/2` of a victim we host, our victim's death of `{boom,[1,2,3]}`, and the
/// `{'DOWN', …}` the peer forwards back to us.
const RR_PEER_MONITORS_SPEC: &str = "two_vm:peer_monitors/1";

// ---------------------------------------------------------------------------------------
// 1. The handshake, in both directions
// ---------------------------------------------------------------------------------------

/// **`net_adm:ping/1` answers `pong` in both directions between two `beam_rt` nodes.**
///
/// This is the first time this runtime's *connector* meets this runtime's *acceptor*: our
/// responder (`rr_ping/1`) dials the peer **before any link exists** — nothing has been
/// sent to the peer, so `DistLinks` has no entry and the dial is the only thing that can
/// create one — the peer's `DistNode` accepts, and the `'N'` handshake completes between
/// the two halves of the same implementation.
///
/// The reverse direction is the peer's own `net_adm:ping/1`, run *after* it has answered
/// our greeting (so its outcome cannot be confused with our dial's): the peer's connector
/// against our acceptor. `PING_OURS pong` and `PING_BACK pong` are therefore two
/// independent verdicts, one per direction, each produced by the VM that made the call.
///
/// A `pang` in either place is a real finding and the test fails on it — the failure names
/// the direction and dumps both transcripts — never smoothed over. (Every other test in
/// this file starts with the peer's `net_adm:ping/1` of *our* node and asserts its `PONG`
/// line, so the reverse direction is covered seven more times.)
#[test]
#[ignore = "two VMs: two beam_rt nodes plus erlc/epmd/setsid/kill; run with --ignored --nocapture"]
fn two_rust_nodes_ping_each_other() {
    let Some(pair) = Pair::start(
        PeerKind::Rust,
        BANNER,
        "rr_ping",
        RR_PING_SPEC,
        "actor_rr_ping",
    ) else {
        return; // every precondition already reported its loud skip
    };
    // Our connector → the peer's acceptor. Nothing had been sent to the peer, so this dial
    // is what creates the link (a failed `net_adm:ping/1` is `pang`, never a raise, so the
    // responder's bounded retry loop covers the peer's startup).
    pair.runtime_equals("PING_OURS ", "PING_OURS pong");
    // Our greeting reached the peer's registered name over the link our dial opened, and
    // what it carried was a pid (the peer cannot render it, so it checks `is_pid/1`).
    pair.actor_equals("HELLO_IS_PID ", "HELLO_IS_PID true");
    // The reverse direction: the peer's connector → our acceptor.
    pair.actor_equals("PING_BACK ", "PING_BACK pong");
    // The peer's verdict, and our acknowledgement of it.
    pair.runtime_equals("PEER_SAW ", "PEER_SAW both_directions");
    pair.actor_equals("BYE ", "BYE done");
    pair.runtime_equals("HANDSHAKE_DONE", "HANDSHAKE_DONE");
    pair.assert_absent(
        &pair.actor,
        &["NOVISITOR", "PINGFAIL"],
        "the greeting and both pings completed inside their bounds",
    );
    // Both exit codes, the peer's `ACTOR_DONE` and our `done` result.
    pair.finish();
}

// ---------------------------------------------------------------------------------------
// 2-5. The signal families, with the peer hosting the process we act on, and our node
//      hosting the process the peer acts on
// ---------------------------------------------------------------------------------------

/// **A `beam_rt` peer links to a process *we* host, and our `DOP_EXIT` reaches it.**
///
/// The actor traps exits, `link/1`s a victim our responder spawned, and waits for a barrier
/// that proves our node recorded the `DOP_LINK` before the victim is told to die. Our node
/// must then route a `DOP_EXIT` back over the peer-dialled link with the pid and the
/// **reason term** intact.
///
/// The reason is compound (`{shutdown,{custom,7}}`) so the assertion covers the value, not
/// merely the event: a bare atom would pass for any atom. `EXIT_IDENTITY true` is the
/// actor's own `V =:= Victim` guard — the pid shipped to the peer as a term and came back
/// inside `{'EXIT', Pid, Reason}`, so a decode that produced a *different* pid fails here
/// rather than being masked by a rendering. Our own `VICTIM_DOWN` is the local witness
/// that the victim really died inside the actor's bounded wait.
#[test]
#[ignore = "two VMs: two beam_rt nodes plus erlc/epmd/setsid/kill; run with --ignored --nocapture"]
fn a_rust_peer_links_to_our_process_and_our_exit_reaches_it() {
    let Some(pair) = Pair::start(
        PeerKind::Rust,
        BANNER,
        "rr_link",
        PEER_DRIVES_SPEC,
        "actor_link_and_trap",
    ) else {
        return;
    };
    // The victim's pid, printed by **us** (a local pid of ours, so `<0.number.serial>`).
    let victim = pair.runtime_text("VICTIM ");
    assert!(
        victim.starts_with('<') && victim.ends_with('>'),
        "our responder prints a local pid: {victim}\n{}",
        pair.diagnostics()
    );
    // The peer's connector → our acceptor (its first act).
    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    // The pid travelled to the peer as a term; it cannot render it, so it proves the type.
    pair.actor_equals("VICTIM_IS_PID ", "VICTIM_IS_PID true");
    // It trapped, linked, and our node processed the `DOP_LINK` before the death: the
    // barrier is a same-link round trip, so `BARRIER ok` orders the two.
    pair.actor_equals("LINK_ARMED", "LINK_ARMED");
    pair.actor_equals("BARRIER ok", "BARRIER ok");
    // The exit our node routed: the guard's own `=:=`, then the reason *value*.
    pair.actor_equals("EXIT_IDENTITY ", "EXIT_IDENTITY true");
    pair.actor_equals("EXIT_REASON ", "EXIT_REASON {shutdown,{custom,7}}");
    // Our own witness of the same death, with the same reason term.
    pair.runtime_equals("VICTIM_DOWN ", "VICTIM_DOWN {shutdown,{custom,7}}");
    pair.runtime_equals("PEER_SAW ", "PEER_SAW seen");
    pair.actor_equals("BYE ", "BYE done");
    pair.runtime_equals("HANDSHAKE_DONE", "HANDSHAKE_DONE");
    pair.assert_absent(
        &pair.actor,
        &["EXIT_OTHER_PID", "EXIT_TIMEOUT", "NOVICTIM"],
        "the trapped exit matched the linked victim inside the bound",
    );
    pair.finish();
}

/// **Our `erlang:monitor/2` of a `beam_rt` peer's process receives the `DOWN`** — the
/// inbound `DOP_MONITOR_P_EXIT` between two `beam_rt` nodes.
///
/// Our responder mints the reference, prints it (a **local** reference, so ours to render),
/// and waits in a clause that **binds** it — `{'DOWN', MRef, process, Victim, Reason}` — so
/// a DOWN carrying any other reference falls through to the catch-all and is reported as
/// `DOWN_OTHER`. The peer proves the `DOP_MONITOR_P` landed (`PEER_MONITORED_BY true`, read
/// off its own `process_info(Victim, monitored_by)`) before it dies the victim, so the
/// DOWN cannot be a race.
///
/// The two identity claims are the reference (ours, echoed back through the peer's
/// `DOP_MONITOR_P_EXIT`) and the pid (the peer's victim, which we were handed as a term).
/// The peer's own `WITNESS_*` lines are its independent view of the death.
#[test]
#[ignore = "two VMs: two beam_rt nodes plus erlc/epmd/setsid/kill; run with --ignored --nocapture"]
fn our_monitor_of_a_rust_peer_process_receives_the_down() {
    let Some(pair) = Pair::start(
        PeerKind::Rust,
        BANNER,
        "rr_monitor",
        RV_MONITOR_SPEC,
        "actor_rv_monitor",
    ) else {
        return;
    };
    let reference = pair.runtime_text("REF ");
    // The peer's connector → our acceptor, then its own view of the victim it hosts.
    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    let victim = pair.actor_text("VICTIM_HOSTED ");
    let witness = pair.actor_text("WITNESS_REF ");
    // The peer observed our `DOP_MONITOR_P` in the victim's own `monitored_by` list.
    pair.actor_equals("PEER_MONITORED_BY ", "PEER_MONITORED_BY true");
    pair.actor_equals("DIE_FORWARDED ", "DIE_FORWARDED boom");
    // The clause that matched had the reference bound, and the pid is the victim's.
    pair.runtime_equals("DOWN_MATCH ", "DOWN_MATCH ref=bound pid=true reason=boom");
    // The whole term, with the reference **we** minted and the pid **the peer** rendered:
    // both VMs' facts in one line, so the term and the rendering agree.
    pair.runtime_equals(
        "DOWN_TERM ",
        &format!("DOWN_TERM {{'DOWN',{reference},process,{victim},boom}}"),
    );
    // The peer's independent witness of the same death (both identifiers are local there).
    pair.actor_equals("WITNESS_DOWN ", "WITNESS_DOWN reason=boom");
    pair.actor_equals(
        "WITNESS_TERM ",
        &format!("WITNESS_TERM {{'DOWN',{witness},process,{victim},boom}}"),
    );
    pair.runtime_equals("PEER_SAW ", "PEER_SAW seen");
    pair.actor_equals("BYE ", "BYE done");
    pair.runtime_equals("HANDSHAKE_DONE", "HANDSHAKE_DONE");
    pair.assert_absent(
        &pair.runtime,
        &["DOWN_OTHER", "DOWN_TIMEOUT"],
        "the DOWN matched our own reference and the monitored pid",
    );
    pair.finish();
}

/// **The negative control for the monitor**: `erlang:demonitor(Ref, [flush])` before the
/// death suppresses the `DOWN` completely.
///
/// Non-vacuity needs the peer to watch the relationship *change* on its own process, which
/// it does: our pid **appears** in the victim's `monitored_by` (`PEER_MONITOR_SEEN true`),
/// our node demonitors, and the pid is **gone** again (`PEER_MONITOR_GONE true`) before the
/// victim dies (`VICTIM_DIED reason=boom`). So `NODOWN` is a demonstrated absence: this
/// runtime *did* have a monitor of that process, and it removed it — the window it expires
/// in starts at the death notice, not at the demonitor, so an armed monitor's DOWN would
/// have to land inside it.
///
/// The mutation that proves the control is not vacuous is recorded in the test's own
/// comment: with the `erlang:demonitor/2` call removed from the fixture's `rv_demonitor/1`,
/// this test fails on `NODOWN` (an `UNEXPECTED_DOWN` arrives instead) — see the report.
#[test]
#[ignore = "two VMs: two beam_rt nodes plus erlc/epmd/setsid/kill; run with --ignored --nocapture"]
fn our_demonitor_of_a_rust_peer_process_suppresses_the_down() {
    let Some(pair) = Pair::start(
        PeerKind::Rust,
        BANNER,
        "rr_demonitor",
        RV_DEMONITOR_SPEC,
        "actor_rv_demonitor",
    ) else {
        return;
    };
    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    let _victim = pair.actor_text("VICTIM_HOSTED ");
    let _witness = pair.actor_text("WITNESS_REF ");
    // The reference we minted and then dropped.
    let reference = pair.runtime_text("REF ");
    // The peer's own demonstration that the monitor existed and then did not.
    pair.actor_equals("PEER_MONITOR_SEEN ", "PEER_MONITOR_SEEN true");
    pair.runtime_equals("DEMONITORED", "DEMONITORED");
    pair.actor_equals("PEER_MONITOR_GONE ", "PEER_MONITOR_GONE true");
    pair.actor_equals("DIE_FORWARDED ", "DIE_FORWARDED boom");
    // The bounded window, started at the death notice, expired with nothing in it.
    pair.runtime_equals("NODOWN", "NODOWN");
    // ..., and the death really did happen inside that window (the peer's own witness).
    pair.actor_equals("VICTIM_DIED ", "VICTIM_DIED reason=boom");
    pair.actor_equals("BYE ", "BYE done");
    pair.runtime_equals("HANDSHAKE_DONE", "HANDSHAKE_DONE");
    pair.assert_absent(
        &pair.runtime,
        &["DOWN_TERM", "DOWN_MATCH", "UNEXPECTED_DOWN", "DOWN_TIMEOUT"],
        "we demonstrated the monitor before the victim died, so no DOWN may arrive",
    );
    pair.assert_no_substring(
        &pair.runtime,
        "'DOWN'",
        "our side demonstrated its monitor and then removed it, so no DOWN term may appear",
    );
    pair.assert_no_substring(
        &pair.actor,
        "'DOWN'",
        "the peer's own witness prints only the reason, so no DOWN term may appear",
    );
    // Our reference occurs exactly once in our whole transcript: its own `REF` line.
    let occurrences = pair
        .runtime
        .transcript()
        .iter()
        .filter(|line| line.contains(&reference))
        .count();
    assert_eq!(
        occurrences, 1,
        "our reference {reference} must appear only in its own `REF` line, never in a DOWN\n{}",
        pair.diagnostics()
    );
    pair.finish();
}

/// **Our `erlang:exit(Victim, kill)` kills a process on a `beam_rt` peer.**
///
/// `exit/2` answers `true` on our side and the peer's own `spawn_monitor`ed witness reports
/// the death — ERTS rewrites the untrappable `kill` to the reason `killed`
/// (`erts/../erl_proc_sig_queue.c`'s exit-policy arm), so `killed` is the reason the peer
/// must print. Our node holds no link to the victim here, so nothing comes back to us: the
/// peer's witness is the whole assertion, and it asserts **terms**, not text (the witness
/// reference and the victim pid are both the peer's own local identifiers).
#[test]
#[ignore = "two VMs: two beam_rt nodes plus erlc/epmd/setsid/kill; run with --ignored --nocapture"]
fn our_remote_exit_kill_kills_a_process_on_a_rust_peer() {
    let Some(pair) = Pair::start(
        PeerKind::Rust,
        BANNER,
        "rr_kill",
        RV_EXIT_KILL_SPEC,
        "actor_rv_exit_kill",
    ) else {
        return;
    };
    pair.runtime_equals("EXIT_SENT ", "EXIT_SENT true");
    // The peer's connector → our acceptor, then the victim it hosts.
    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    let victim = pair.actor_text("VICTIM_HOSTED ");
    let witness = pair.actor_text("WITNESS_REF ");
    // The peer's own witness: the reason ERTS substitutes for `kill`, and the whole
    // `{'DOWN', Ref, process, Pid, killed}` — the reference it minted, the pid it spawned.
    pair.actor_equals("WITNESS_DOWN ", "WITNESS_DOWN reason=killed");
    pair.actor_equals(
        "WITNESS_TERM ",
        &format!("WITNESS_TERM {{'DOWN',{witness},process,{victim},killed}}"),
    );
    pair.runtime_equals("PEER_SAW ", "PEER_SAW killed");
    pair.actor_equals("BYE ", "BYE done");
    pair.runtime_equals("HANDSHAKE_DONE", "HANDSHAKE_DONE");
    pair.assert_absent(
        &pair.actor,
        &["WITNESS_OTHER", "WITNESS_TIMEOUT"],
        "the peer's witness matched its own reference and the victim",
    );
    pair.finish();
}

// ---------------------------------------------------------------------------------------
// 6-7. The two round trips: a reference created here and used there, a pid of ours out and
//      back
// ---------------------------------------------------------------------------------------

/// **An alias created on one `beam_rt` node delivers a message sent from the other** — the
/// inbound `DOP_ALIAS_SEND` (the C's `erts_proc_sig_send_dist_altact_msg`, `dist.c:2563`).
///
/// Our responder mints the alias with `erlang:alias/1`, prints the reference it holds (a
/// local reference: ours to render) and hands the **term** to the peer over the wire; the
/// peer sends `{payload, self(), 4242}` to it. The payload must arrive verbatim in *our*
/// mailbox, resolved from the alias reference to its owner — `PAYLOAD_N 4242` — and its
/// sender must be the very pid we handed the alias to (`PAYLOAD_FROM_PEER true`, a `=:=`
/// over a pid that crossed the wire twice: out in a greeting, back inside the payload).
///
/// The peer asserts the send's own return value (`ALIAS_SENT {payload,<its own pid>,4242}`):
/// sending to a reference is a real send, not a `badarg`, and `!` answers the message term.
#[test]
#[ignore = "two VMs: two beam_rt nodes plus erlc/epmd/setsid/kill; run with --ignored --nocapture"]
fn an_alias_created_on_one_rust_node_delivers_from_the_other() {
    let Some(pair) = Pair::start(
        PeerKind::Rust,
        BANNER,
        "rr_alias_in",
        RR_OUR_ALIAS_SPEC,
        "actor_rr_our_alias",
    ) else {
        return;
    };
    // The alias reference we minted, printed by us (`#Ref<0.…>`).
    let reference = pair.runtime_text("ALIAS ");
    assert!(
        reference.starts_with("#Ref<"),
        "our alias is a local reference, so we can render it: {reference}\n{}",
        pair.diagnostics()
    );
    // The peer's own `self()`, printed by the VM that owns it (a local pid there, so it
    // renders as `<0.Number.Serial>`) — the `ALIAS_SENT` expectation below is *derived* from
    // this string, never fabricated.
    let actor_pid = pair.actor_text("SELF ");
    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    // The peer cannot render a reference it received, so it proves the type instead.
    pair.actor_equals("ALIAS_IS_REF ", "ALIAS_IS_REF true");
    // The send's own return value: a send to an alias reference is a real send, and `!`
    // answers the message term (`erlang:send/2`) — with the **peer's own pid** in the
    // sender slot, the very pid the responder then `=:=`-compares against (`PAYLOAD_FROM_PEER`).
    pair.actor_equals(
        "ALIAS_SENT ",
        &format!("ALIAS_SENT {{payload,{actor_pid},4242}}"),
    );
    // The payload, verbatim, out of our own mailbox — and its sender is the peer's pid.
    pair.runtime_equals("PAYLOAD_N ", "PAYLOAD_N 4242");
    pair.runtime_equals("PAYLOAD_FROM_PEER ", "PAYLOAD_FROM_PEER true");
    pair.runtime_equals("PEER_SAW ", "PEER_SAW alias_sent");
    pair.actor_equals("BYE ", "BYE done");
    pair.runtime_equals("HANDSHAKE_DONE", "HANDSHAKE_DONE");
    pair.assert_absent(
        &pair.runtime,
        &["ALIAS_TIMEOUT", "NOVISITOR", "NOALIAS"],
        "the alias send was delivered to its owner",
    );
    // The reference appears only where we printed it: nothing echoed it back (an alias is
    // not a monitor, so no DOWN names it).
    let occurrences = pair
        .runtime
        .transcript()
        .iter()
        .filter(|line| line.contains(&reference))
        .count();
    assert_eq!(
        occurrences, 1,
        "our alias reference belongs to its own `ALIAS` line only\n{}",
        pair.diagnostics()
    );
    pair.finish();
}

/// **Our pid survives a round trip through a `beam_rt` peer**: the responder sends
/// `self()`, the peer echoes the *same term* back, and the responder compares it with
/// `=:=` and prints both renderings.
///
/// `IDENTITY true` is the assertion. It is the observable a broken inbound pid decode would
/// falsify: a pid of *this* node that arrives from the wire must decode back into the
/// internal form (channel `0`, our own creation) — anything else is a different `Eterm`
/// word, so `=:=` fails and the test fails. `ECHO` is our own `~p` of what came back, and it
/// must be the very text we printed for `SELF`; `SELF_NODE`/`ECHO_NODE` are both our node,
/// printed by us. (The peer prints only `ECHO_IS_PID` — a pid it received is *external*
/// there and it cannot render one.)
///
/// The same identity question on the **signal** path is asserted by
/// [`a_rust_peer_links_to_our_process_and_our_exit_reaches_it`] (`EXIT_IDENTITY true`) and
/// [`our_monitor_of_a_rust_peer_process_receives_the_down`] (`pid=true`): there the pid
/// rides inside a `{'EXIT', …}`/`{'DOWN', …}` term, here it rides inside an ordinary
/// message. Same decoder, three shapes.
#[test]
#[ignore = "two VMs: two beam_rt nodes plus erlc/epmd/setsid/kill; run with --ignored --nocapture"]
fn our_pid_survives_a_round_trip_through_a_rust_peer() {
    let Some(pair) = Pair::start(
        PeerKind::Rust,
        BANNER,
        "rr_identity",
        IDENTITY_SPEC,
        "actor_echo_identity",
    ) else {
        return;
    };
    let me = pair.runtime_text("SELF ");
    pair.runtime_equals("SELF_NODE ", &format!("SELF_NODE {}", pair.rust_node));
    // The peer's connector → our acceptor, then the pid it received (as a term).
    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    pair.actor_equals("ECHO_IS_PID ", "ECHO_IS_PID true");
    pair.actor_equals("ECHO_SENT", "ECHO_SENT");
    // What came back is `=:=` our own pid — and renders as the very text we printed.
    pair.runtime_equals("IDENTITY ", "IDENTITY true");
    pair.runtime_equals("ECHO ", &format!("ECHO {me}"));
    pair.runtime_equals("ECHO_NODE ", &format!("ECHO_NODE {}", pair.rust_node));
    pair.actor_equals("BYE ", "BYE done");
    pair.runtime_equals("HANDSHAKE_DONE", "HANDSHAKE_DONE");
    pair.assert_absent(
        &pair.runtime,
        &["PARENT_TIMEOUT", "NOIDENTITY"],
        "the pid came back and matched before the bound",
    );
    pair.finish();
}

// ---------------------------------------------------------------------------------------
// 8-12. The inbound half: the **peer** acts on a process *we* host
//
//   Sections 1-7 all have our node (or the peer) acting on a process that lives on the
//   *peer*. These five are the mirror — the peer's `link/1` and `exit/2` aimed at a victim
//   our own node hosts, which is the direction ERTS calls the *inbound*
//   `DOP_LINK`/`DOP_EXIT`/`DOP_EXIT2` (`dist.c:2224`, `:2760`, `:2824`), plus the two
//   families whose *record* has to be visible before the peer's next message: the `links`
//   item an inbound `DOP_LINK` installs (section 11) and the `monitored_by` item an inbound
//   `DOP_MONITOR_P` installs (section 12). `tests/dist_two_vm.rs` pins the same five against
//   a real `erl` peer; here the peer is this runtime, so the wire has this runtime's encoder
//   on **both** ends.
//
//   All four inbound families measured in each direction are therefore covered here. The
//   ordering the two *record* families need is the write-path rule the module docs state: a
//   message write drains the link's queued control signals first, so a control signal created
//   earlier can never be overtaken by a later message on the same link.
// ---------------------------------------------------------------------------------------

/// **A `beam_rt` peer's death travels *inbound* and our trapping process traps its
/// compound reason** — the receive side of `DOP_EXIT` (`dist.c:2760`).
///
/// The peer links to a victim our responder spawned, waits for a barrier that proves our
/// node applied the `DOP_LINK` (the ack is a same-link round trip), and then dies of
/// `{boom,[1,2,3]}`. Our victim traps exits, so the signal must reach it as
/// `{'EXIT', PeerPid, {boom,[1,2,3]}}`.
///
/// Three identity claims, all term-level:
///
/// * `EXIT_PID true` — the trapped message's sender is `=:=` the pid the peer reported
///   separately from the one the wire carried inside the exit: two arrivals of one external
///   pid must decode to one `Eterm` word.
/// * `EXIT_REASON {boom,[1,2,3]}` — the *reason term itself*, not a shape that resembles
///   it: a bare atom would pass for any atom, so the compound value is what is asserted.
/// * `LINKER_DOWN {boom,[1,2,3]}` — the peer's own witness that its arm really died of the
///   reason it chose, so our trapped term cannot be explained by an arm that never died.
#[test]
#[ignore = "two VMs: two beam_rt nodes plus erlc/epmd/setsid/kill; run with --ignored --nocapture"]
fn a_rust_peers_death_travels_inbound_and_our_trapping_process_traps_its_compound_reason() {
    let Some(pair) = Pair::start(
        PeerKind::Rust,
        BANNER,
        "rr_peer_link_death",
        RR_PEER_LINK_DEATH_SPEC,
        "actor_link_death_boxed",
    ) else {
        return;
    };
    // The peer's connector → our acceptor (its first act), and the pid it received: our
    // victim is an *external* pid there, so it proves the type rather than rendering it.
    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    pair.actor_equals("VICTIM_IS_PID ", "VICTIM_IS_PID true");
    // Our own view of the peer's arm: a pid (never text — the peer's pid is external here).
    pair.runtime_equals("LINKER_IS_PID ", "LINKER_IS_PID true");
    // `LINKED` is the peer's own `{'EXIT'}` arm reporting the link call's success...
    pair.actor_equals("LINKED", "LINKED");
    // ...and our node routed the exit the peer's death produced: the sender's identity
    // first, then the reason term.
    pair.runtime_equals("EXIT_PID ", "EXIT_PID true");
    pair.runtime_equals("EXIT_REASON ", "EXIT_REASON {boom,[1,2,3]}");
    // The peer's witness of its own death, with the very same reason term.
    pair.actor_equals("LINKER_DOWN ", "LINKER_DOWN {boom,[1,2,3]}");
    pair.runtime_equals("PEER_SAW ", "PEER_SAW seen");
    pair.actor_equals("BYE ", "BYE done");
    pair.runtime_equals("HANDSHAKE_DONE", "HANDSHAKE_DONE");
    pair.assert_absent(
        &pair.runtime,
        &["EXIT_TIMEOUT", "EXIT_OTHER_PID", "NOLINKER"],
        "the trapped exit matched the linked pid inside the bound",
    );
    pair.assert_absent(
        &pair.actor,
        &["NOLINK", "LINKER_TIMEOUT", "PINGFAIL", "NOVISITOR"],
        "the peer armed its link and reported its own death",
    );
    pair.finish();
}

/// **The same inbound link death with `trap_exit` off**: the signal *terminates* our
/// process, and the reason it terminates with is the peer's own — never `noproc` (a link
/// the receiver never recorded) and never `killed` (the untrappable-exit reason, which a
/// link exit does not use).
///
/// The victim is a `spawn`ed child (the responder must outlive it to report) and its death
/// is read back through the responder's **own local monitor**, so `DOWN_REASON boom` is our
/// own observation of our own process — not the peer's claim about it. The peer's
/// `LINKER_DOWN boom` is the independent half: its arm really did die of `boom`.
#[test]
#[ignore = "two VMs: two beam_rt nodes plus erlc/epmd/setsid/kill; run with --ignored --nocapture"]
fn a_rust_peers_death_travels_inbound_and_kills_our_non_trapping_process_with_its_reason() {
    let Some(pair) = Pair::start(
        PeerKind::Rust,
        BANNER,
        "rr_peer_link_death_plain",
        RR_PEER_LINK_DEATH_PLAIN_SPEC,
        "actor_link_death_atom",
    ) else {
        return;
    };
    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    pair.actor_equals("VICTIM_IS_PID ", "VICTIM_IS_PID true");
    pair.runtime_equals("LINKER_IS_PID ", "LINKER_IS_PID true");
    pair.actor_equals("LINKED", "LINKED");
    // Our own local monitor's DOWN: the clause bound the reference we minted and the pid we
    // spawned (a `DOWN` naming anything else falls through to `DOWN_OTHER`).
    pair.runtime_equals("DOWN_PID ", "DOWN_PID true");
    pair.runtime_equals("DOWN_REASON ", "DOWN_REASON boom");
    pair.actor_equals("LINKER_DOWN ", "LINKER_DOWN boom");
    pair.runtime_equals("PEER_SAW ", "PEER_SAW seen");
    pair.actor_equals("BYE ", "BYE done");
    pair.runtime_equals("HANDSHAKE_DONE", "HANDSHAKE_DONE");
    pair.assert_absent(
        &pair.runtime,
        &["DOWN_TIMEOUT", "DOWN_OTHER", "NOLINKER"],
        "the victim's death reached our own monitor with the peer's reason",
    );
    pair.assert_absent(
        &pair.actor,
        &["NOLINK", "LINKER_TIMEOUT", "PINGFAIL", "NOVISITOR"],
        "the peer armed its link and reported its own death",
    );
    pair.finish();
}

/// **A `beam_rt` peer's `erlang:exit/2` calls travel inbound and split the whole matrix by
/// trapping and `kill`** — the inbound `DOP_EXIT2` (`dist.c:2824`), which is *not* the link
/// exit: it is the signal `exit/2` produces, so it must apply the same untrappable-`kill`
/// rewrite ERTS applies locally.
///
/// Four victims our responder spawned — two non-trapping, two trapping — and the peer calls
/// `exit(Victim, boom)` and `exit(Victim, kill)` against one of each. The four rows the
/// responder must print are the C's exit policy, one per cell of the matrix:
///
/// ```text
/// EXIT2 plain_boom down boom                  % non-trapping, plain reason: dies of it
/// EXIT2 plain_kill down killed                % non-trapping, kill: the rewrite to `killed`
/// EXIT2 trap_boom trapped boom from_peer=true % trapping, plain reason: the message
/// EXIT2 trap_kill down killed                 % trapping, kill: still dies, still `killed`
/// ```
///
/// `from_peer=true` is the trapped message's sender `=:=` the pid the peer's greeting
/// carried, so the trapped signal is demonstrably the peer's. `TRAP_BOOM_ALIVE true` is the
/// survivor answering a round-trip probe — a trapped process is *alive*, not merely
/// un-reaped — and the peer's own `EXIT2 <label> true` lines are `exit/2`'s return value,
/// which is `true` for a live target on both runtimes.
#[test]
#[ignore = "two VMs: two beam_rt nodes plus erlc/epmd/setsid/kill; run with --ignored --nocapture"]
fn a_rust_peers_exit_calls_travel_inbound_and_split_the_matrix_by_trapping_and_kill() {
    let Some(pair) = Pair::start(
        PeerKind::Rust,
        BANNER,
        "rr_peer_exit2",
        RR_PEER_EXIT2_SPEC,
        "actor_exit2_matrix",
    ) else {
        return;
    };
    // Four references we minted, one per victim — the monitor behind each row below.
    pair.runtime_equals(
        "MONITORS_ARE_REFS ",
        "MONITORS_ARE_REFS [true,true,true,true]",
    );
    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    // The four victim pids arrived at the peer as pids (it cannot render one).
    pair.actor_equals(
        "VICTIMS_ARE_PIDS ",
        "VICTIMS_ARE_PIDS [true,true,true,true]",
    );
    // `exit/2`'s own return value, one per call, in the order the peer makes them.
    pair.actor_equals("EXIT2 plain_boom ", "EXIT2 plain_boom true");
    pair.actor_equals("EXIT2 plain_kill ", "EXIT2 plain_kill true");
    pair.actor_equals("EXIT2 trap_boom ", "EXIT2 trap_boom true");
    pair.actor_equals("EXIT2 trap_kill ", "EXIT2 trap_kill true");
    // Our side's four outcomes. The collector accepts the receipts in any order (they come
    // from five different senders) and prints them always in this one.
    pair.runtime_equals("EXIT2 plain_boom ", "EXIT2 plain_boom down boom");
    pair.runtime_equals("EXIT2 plain_kill ", "EXIT2 plain_kill down killed");
    pair.runtime_equals(
        "EXIT2 trap_boom ",
        "EXIT2 trap_boom trapped boom from_peer=true",
    );
    pair.runtime_equals("EXIT2 trap_kill ", "EXIT2 trap_kill down killed");
    pair.runtime_equals("TRAP_BOOM_ALIVE ", "TRAP_BOOM_ALIVE true");
    pair.runtime_equals("PEER_SAW ", "PEER_SAW seen");
    pair.actor_equals("BYE ", "BYE done");
    pair.runtime_equals("HANDSHAKE_DONE", "HANDSHAKE_DONE");
    pair.assert_absent(
        &pair.runtime,
        &["EXIT2_TIMEOUT"],
        "every one of the four exit signals produced a receipt inside the bound",
    );
    pair.assert_no_substring(
        &pair.runtime,
        "missing",
        "every label matched a receipt, so no row reported a missing outcome",
    );
    pair.finish();
}

// ---------------------------------------------------------------------------------------
// 11-12. The two inbound families whose **record** must be visible ahead of the peer's next
//        message: the `links` item an inbound `DOP_LINK` installs, and the `monitored_by`
//        item an inbound `DOP_MONITOR_P` installs
//
//   Both are the mirror of a `dist_two_vm.rs` case over the same responder/actor pair, and
//   both read a *record* rather than an event, so they pin the write-path ordering rule the
//   module docs state: the peer creates the control signal (its `link/1`, its `monitor/2`),
//   then writes a message on the same link (a barrier, a `{monitoring, …}` notice), and our
//   node must already have applied the control signal when that message is processed.
// ---------------------------------------------------------------------------------------

/// **A `beam_rt` peer's `link/1` shows up in the victim's `links` item, and its `unlink/1`
/// clears it** — the inbound `DOP_LINK`/`DOP_UNLINK_ID` as *our own* process reports them,
/// with the peer's barrier message never overtaking the control signal that precedes it.
///
/// The mirror of
/// [`dist_two_vm.rs`'s `a_real_erl_peers_link_shows_up_in_the_links_item_and_clears_on_unlink`](../dist_two_vm.rs),
/// over the same responder (`two_vm:peer_drives_links/1`) and the same actor
/// (`link_and_unlink/1`). The peer `link/1`s the victim, takes a barrier round trip, then
/// `unlink/1`s it and takes a second one; the responder reports the victim's
/// `process_info(Victim, links)` item after each ack. The victim is **never killed** — it
/// answers a round-trip probe after the second barrier (`VICTIM_ALIVE`) — so the only thing
/// that differs between the two reports is the peer's own link, and an emptied item cannot
/// be explained by a dead process.
///
/// The claim is `LINKS_FRESH 0 []` (the empty baseline, taken before the peer did anything)
/// → `LINKS_LINKED 1 [true]` → `LINKS_UNLINKED 0 []`. Every report is the item's **shape**,
/// `length/1` plus one `is_pid/1` per element: the entry the peer arms is an *external* pid,
/// which this runtime cannot render at all (`beam_bifs`'s `external_channel_gap`, an
/// uncatchable capability gap), so printing one would kill this VM with exit 3.
#[test]
#[ignore = "two VMs: two beam_rt nodes plus erlc/epmd/setsid/kill; run with --ignored --nocapture"]
fn a_rust_peers_link_shows_up_in_the_links_item_and_clears_on_unlink() {
    let Some(pair) = Pair::start(
        PeerKind::Rust,
        BANNER,
        "rr_links_item",
        RR_PEER_DRIVES_LINKS_SPEC,
        "link_and_unlink",
    ) else {
        return;
    };
    // The pid whose item is reported: one of ours, so it renders as `<0.number.serial>`
    // (a *local* pid — the peer is the one that sees it as remote).
    let victim = pair.runtime_text("VICTIM ");
    assert!(
        victim.starts_with("<0."),
        "the victim must be a local pid of ours, printed as `<0.number.serial>`, not `{victim}`\n{}",
        pair.diagnostics()
    );
    // Before the peer acts: a freshly spawned child has no links at all.
    pair.runtime_equals("LINKS_FRESH ", "LINKS_FRESH 0 []");

    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    // The peer holds a pid term (a remote pid is unprintable there, so it proves the type).
    pair.actor_equals("LINK_VICTIM_IS_PID ", "LINK_VICTIM_IS_PID true");
    pair.actor_equals("LINKED", "LINKED");
    pair.actor_equals("BARRIER ", "BARRIER ok");
    // The peer's `link/1` was applied **before** the barrier it wrote right after the call:
    // exactly one entry, and it is a pid.
    pair.runtime_equals("LINKS_LINKED ", "LINKS_LINKED 1 [true]");

    pair.actor_equals("UNLINKED", "UNLINKED");
    pair.actor_equals("BARRIER ", "BARRIER ok");
    // … and its `unlink/1` cleared it again, with the victim still running.
    pair.runtime_equals("LINKS_UNLINKED ", "LINKS_UNLINKED 0 []");
    pair.actor_equals("VICTIM_ALIVE", "VICTIM_ALIVE");

    pair.runtime_equals("PEER_SAW ", "PEER_SAW links_reported");
    pair.actor_equals("BYE ", "BYE done");
    pair.runtime_equals("HANDSHAKE_DONE", "HANDSHAKE_DONE");
    // Every report read a real `{links, …}` item (a missing one prints `… NOITEM …`), and the
    // session took exactly the two barrier round trips the protocol defines.
    pair.assert_no_substring(
        &pair.runtime,
        "NOITEM",
        "every `links` report was taken from a real `process_info/2` reply",
    );
    pair.assert_absent(
        &pair.runtime,
        &["LINKS_LATER", "PARENT_TIMEOUT"],
        "the session took exactly the two barrier round trips the protocol defines",
    );
    pair.finish();
}

/// **A `beam_rt` peer's `erlang:monitor/2` of our process really arms, and its
/// `{'DOWN', …}` carries the boxed reason back over the wire** — the inbound
/// `DOP_MONITOR_P` record and the outbound `DOP_MONITOR_P_EXIT` between two `beam_rt` nodes.
///
/// The mirror of
/// [`dist_two_vm.rs`'s `a_real_erl_peers_monitor_of_our_process_gets_a_down_carrying_the_boxed_reason`](../dist_two_vm.rs),
/// over the same responder (`two_vm:peer_monitors/1`) and the same actor
/// (`actor_monitor_down/1`). `MONITORED_BY 1 [true]` is what makes the case non-vacuous in
/// the other direction: our node really recorded the peer's `DOP_MONITOR_P` against the
/// victim — the item is a **shape** (its length plus one `is_pid/1` per element), because the
/// watcher is an external pid this runtime cannot render — *before* the victim died, so the
/// `{'DOWN', …}` below cannot be an accident of ordering.
///
/// The peer then forwards the whole tuple back, and every slot is asserted here: the size,
/// the head, that the reference is a reference (the peer minted it, so it can be compared
/// with nothing we hold), the kind, the object (`=:=` our own pid after its round trip
/// through the peer) and the reason term itself.
#[test]
#[ignore = "two VMs: two beam_rt nodes plus erlc/epmd/setsid/kill; run with --ignored --nocapture"]
fn a_rust_peers_monitor_of_our_process_gets_a_down_carrying_the_boxed_reason() {
    let Some(pair) = Pair::start(
        PeerKind::Rust,
        BANNER,
        "rr_inbound_monitor",
        RR_PEER_MONITORS_SPEC,
        "actor_monitor_down",
    ) else {
        return;
    };
    // Our node recorded the peer's monitor of the victim before the victim died — read off
    // the victim's own `monitored_by` item, as a shape.
    pair.runtime_equals("MONITORED_BY ", "MONITORED_BY 1 [true]");
    // The forwarded `{'DOWN', Ref, process, OurPid, Reason}`, slot by slot.
    pair.runtime_equals("DOWN_SIZE ", "DOWN_SIZE 5");
    pair.runtime_equals("DOWN_HEAD ", "DOWN_HEAD true");
    pair.runtime_equals("DOWN_REF ", "DOWN_REF true");
    pair.runtime_equals("DOWN_KIND ", "DOWN_KIND true");
    pair.runtime_equals("DOWN_OBJ ", "DOWN_OBJ true");
    pair.runtime_equals("DOWN_REASON ", "DOWN_REASON {boom,[1,2,3]}");
    pair.actor_equals("PONG ", &format!("PONG {}", pair.actor_node));
    pair.actor_equals("VICTIM_IS_PID ", "VICTIM_IS_PID true");
    // `erlang:monitor/2` answered a reference (the peer's own, which it binds in its clause).
    pair.actor_equals("MONITORING ", "MONITORING true");
    // The peer's own view of the reason it received (its observation, not the assertion).
    pair.actor_equals("PEER_DOWN_REASON ", "PEER_DOWN_REASON {boom,[1,2,3]}");
    pair.assert_absent(
        &pair.runtime,
        &["SAW_DOWN_TIMEOUT", "NOMONITOR"],
        "the peer's monitor fired and its DOWN reached us",
    );
    pair.runtime_equals("PEER_SAW ", "PEER_SAW seen");
    pair.actor_equals("BYE ", "BYE done");
    pair.runtime_equals("HANDSHAKE_DONE", "HANDSHAKE_DONE");
    pair.finish();
}
