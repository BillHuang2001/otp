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

//! The peer-close reply ladder over **real loopback sockets**: a **connection reset** must be
//! distinguishable from a **clean end-of-file**, which is the whole point of the C's three
//! `desc->tcp_add_flags` bits (`inet_drv.c:855`, `:856`, `:862`).
//!
//! The C's rules the suite pins, each derived from the source:
//!
//! * `tcp_recv`'s `ECONNRESET` arm (`:13185`-`:13196`) reports `{error,econnreset}` — and arms
//!   **nothing** — only when `TCP_ADDF_SHOW_ECONNRESET` is set (`{show_econnreset,true}`);
//!   otherwise `tcp_recv_closed` runs, arming only the *send* one-shot (`:12816`).
//! * `tcp_send_or_shutdown_error` (`:13613`) arms `TCP_ADDF_DELAYED_CLOSE_SEND` (`:13662`),
//!   `TCP_ADDF_DELAYED_CLOSE_RECV` (`:13678`) and — for a reset under `{show_econnreset,true}` —
//!   `TCP_ADDF_DELAYED_ECONNRESET` (`:13683`).
//! * the receive ladder (`TCP_REQ_RECV`, `:12279`-`:12288`) answers `econnreset`/`closed` once and
//!   clears **both** one-shots plus the reset bit; the send ladder (`tcp_inet_commandv`,
//!   `:12681`-`:12692`) clears **only** the send one-shot and deliberately leaves the reset bit
//!   armed "for the next receive operation".
//!
//! Every wait is **condition-based under a hang guard** (the RST is awaited with `poll(2)` on the
//! port's own descriptor), never a wall-clock budget tuned for an idle host.
//!
//! # The two discovery routes, and why the ladder must not depend on which one ran
//!
//! In the C a passive socket arms read interest **only** while a `TCP_REQ_RECV` is queued
//! (`sock_select(FD_READ|FD_CLOSE, 1)`, `inet_drv.c:12315`), so the receive that armed it is
//! always the party that reads the end-of-file — and `tcp_recv_closed` answers it through
//! `async_error_am_all(am_closed)` (`:12829`) while arming only the *send* one-shot (`:12816`).
//! This floor's poller is **edge-triggered** and keeps the socket's `READABLE` registration for its
//! whole life, so the **reactor** (`ready_input`) routinely reads the end-of-file first, with no
//! operation to answer. That discovery's `closed`/`econnreset` is still owed to the user, so it is
//! armed as `TCP_ADDF_DELAYED_CLOSE_RECV` for the next `recv` (`ReadReport::OwedToNextRecv`) — and
//! the *user-visible* sequence stays `closed` then `enotconn`, whichever route ran. The
//! `a_reactor_discovered_*` and `the_reactor_rows_agree_with_the_erl_oracle` rows below pin that;
//! the earlier rows pin the route where a `recv` makes the discovery itself.
use super::opcode_tests::{
    assert_agrees, erl_stdout, loopback_arg, op, open_arg, opened_port, oracle_replies, Rig, Rule,
    CONTROL_MAGIC,
};
use super::wire::{ctl_ok, ctl_xerror};
use crate::completion::{CompletionEvent, InetReason, InetReply};
use beam_common::BeamError;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::io::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

/// The `TCP_REQ_RECV`(42) operand of a passive, raw, zero-length, no-deadline receive.
const RECV_OPERAND: [u8; 8] = [0; 8];

/// `{show_econnreset,true}`: the `INET_LOPT_TCP_SHOW_ECONNRESET`(39) five-byte setopts record.
const SHOW_ECONNRESET: [u8; 5] = [39, 0, 0, 0, 1];

/// `{show_econnreset,false}` — the default, spelled out so the "off" rig is explicit.
const HIDE_ECONNRESET: [u8; 5] = [39, 0, 0, 0, 0];

/// `{exit_on_close,false}`: the `INET_LOPT_EXITONCLOSE`(26) five-byte setopts record that selects
/// the C's `desc_close_read` arm instead of `tcp_desc_close` (`inet_drv.c:12824`-`:12827`).
const EXIT_ON_CLOSE_OFF: [u8; 5] = [26, 0, 0, 0, 0];
/// A hang guard for the condition waits: generous, only reached by a genuinely broken driver.
const HANG_GUARD: Duration = Duration::from_secs(20);

/// A live `tcp_inet` port over a **real** loopback connection, with the far end kept here so the
/// test can abort or half-close it.
struct ResetRig {
    port: Rig,
    /// The descriptor the port's driver owns (its `INET_REQ_GETFD` value).
    fd: RawFd,
    /// The far end, held (and eventually closed) by the test.
    peer: Option<TcpStream>,
}

/// Connects a fresh port to a loopback listener and hands back both ends.
fn connected_rig(option: [u8; 5]) -> ResetRig {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let address = listener.local_addr().expect("an address");
    let mut port = opened_port();
    let mut connect = vec![0u8, 0, 3, 232]; // a 1000 ms connect budget
    connect.extend_from_slice(&loopback_arg(address.port()));
    assert_eq!(
        port.control(&op(3), &connect),
        ctl_ok(),
        "the port connects to the loopback listener"
    );
    let (peer, _) = listener.accept().expect("the peer side is accepted");
    let fd_reply = port.control(&op(14), &[]);
    assert_eq!(fd_reply[0], ctl_ok()[0], "INET_REQ_GETFD answers");
    let fd = i32::from_be_bytes([fd_reply[1], fd_reply[2], fd_reply[3], fd_reply[4]]);
    assert_eq!(
        port.control(&op(7), &option),
        ctl_ok(),
        "the `{{show_econnreset, _}}` record is accepted"
    );
    ResetRig {
        port,
        fd,
        peer: Some(peer),
    }
}

/// Waits until `fd` reports something to read (the peer's RST or its end-of-file), under
/// [`HANG_GUARD`]. `poll(2)` observes readiness without consuming it, so the driver's own read
/// still makes the observation.
fn wait_readable(fd: RawFd) {
    let deadline = Instant::now() + HANG_GUARD;
    loop {
        let mut poll_fd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `poll_fd` is one initialized `pollfd` describing an open descriptor this test
        // owns, and `poll(2)` reads exactly that one entry and writes only its `revents`.
        let ready = unsafe { libc::poll(&mut poll_fd, 1, 0) };
        assert!(ready >= 0, "poll(2) failed: {}", std::io::Error::last_os_error());
        if ready > 0 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the peer's close was never observed on the port's descriptor"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Aborts the connection: `SO_LINGER` with a zero timeout makes `close(2)` send an **RST**
/// instead of an orderly FIN, and then waits until the port's socket has seen it.
fn abort_peer(rig: &mut ResetRig) {
    let peer = rig.peer.take().expect("the far end is still held");
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    // SAFETY: `linger` is a fully initialized `struct linger` and the descriptor is an open socket
    // owned by this test; `setsockopt` reads at most `size_of::<linger>()` bytes and retains
    // nothing.
    let rc = unsafe {
        libc::setsockopt(
            peer.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            (&raw const linger).cast(),
            std::mem::size_of::<libc::linger>() as libc::socklen_t,
        )
    };
    assert_eq!(rc, 0, "SO_LINGER is settable");
    drop(peer); // an abortive close: RST
    wait_readable(rig.fd);
}

/// Half-closes the peer's write side: the port's read then sees a clean end-of-file.
fn close_peer_write(rig: &mut ResetRig) {
    let peer = rig.peer.as_ref().expect("the far end is still held");
    peer.shutdown(Shutdown::Write).expect("the peer half-closes");
    wait_readable(rig.fd);
}

/// Hands the observation to the **reactor**: `poll(2)`s the port's poller and dispatches every
/// ready event through the registry — the route `tests/inet.rs`'s
/// `a_reactor_observed_peer_close_releases_the_registration_and_reports_closed` takes — until the
/// driver has released the dead socket's registration, which is what its
/// [`PeerClose`](super::PeerClose) latch does.
fn drive_reactor(rig: &mut ResetRig) {
    let deadline = Instant::now() + HANG_GUARD;
    while !rig.port.poller.is_empty() {
        assert!(
            Instant::now() < deadline,
            "the reactor never observed the peer's close"
        );
        let events = match rig.port.poller.poll(Some(Duration::from_millis(50))) {
            Ok(events) => events,
            Err(BeamError::Io(err)) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => panic!("poll failed: {err:?}"),
        };
        rig.port
            .registry
            .handle_events(&mut rig.port.poller, &events)
            .expect("the ready events are dispatched");
    }
}

/// A `prim_inet`-style command: the two-byte tag length, the ETF-led tag `[131, 97]` and a payload.
/// The tag is what [`crate::DriverContext::deliver_completion`] echoes back as the
/// `{inet_reply, Port, Reply, Tag}` of a `port_command`, so the send ladder's answer is observable.
const TAGGED_COMMAND: [u8; 5] = [0, 2, 131, 97, b'x'];

// -----------------------------------------------------------------------
// The receive ladder
// -----------------------------------------------------------------------

/// `{show_econnreset,true}` + a peer that aborts with an RST: the write that finds the peer gone
/// arms both one-shots plus the reset bit (`inet_drv.c:13662`/`:13678`/`:13683`), so the first
/// `TCP_REQ_RECV` answers `[0]"econnreset"` and the second `[0]"enotconn"`
/// (`:12279`-`:12288`).
#[test]
fn a_reset_under_show_econnreset_answers_econnreset_then_enotconn() {
    let mut rig = connected_rig(SHOW_ECONNRESET);
    abort_peer(&mut rig);
    // The write is what discovers the reset — the oracle's own sequence (a `port_command` after
    // the peer aborted): `send(2)` answers `ECONNRESET`, which latches the close.
    rig.port
        .command(&[0, 0, b'x'])
        .expect("the command is accepted: the close is a reply, not an error");
    assert_eq!(
        rig.port.control(&op(42), &RECV_OPERAND),
        ctl_xerror("econnreset"),
        "the first receive owes the reset"
    );
    assert_eq!(
        rig.port.control(&op(42), &RECV_OPERAND),
        ctl_xerror("enotconn"),
        "the one-shot is spent: the connection is simply gone"
    );
}

/// The same reset with the **default** `{show_econnreset,false}`: `tcp_recv`'s `ECONNRESET` arm
/// takes `tcp_recv_closed` (`inet_drv.c:13185`-`:13196`), so the ladder answers `closed` and then
/// `enotconn`. Pinned so the fix cannot regress the existing reply.
#[test]
fn a_reset_without_show_econnreset_answers_closed_then_enotconn() {
    let mut rig = connected_rig(HIDE_ECONNRESET);
    abort_peer(&mut rig);
    rig.port.command(&[0, 0, b'x']).expect("the command is accepted");
    assert_eq!(
        rig.port.control(&op(42), &RECV_OPERAND),
        ctl_xerror("closed"),
        "without the option a reset is reported as a plain close"
    );
    assert_eq!(
        rig.port.control(&op(42), &RECV_OPERAND),
        ctl_xerror("enotconn")
    );
}

/// A clean end-of-file is `closed` then `enotconn` **whatever** `{show_econnreset}` says: the
/// reset bit is only ever armed by an `ECONNRESET` observation.
#[test]
fn a_clean_end_of_file_answers_closed_then_enotconn_whatever_the_option() {
    for option in [SHOW_ECONNRESET, HIDE_ECONNRESET] {
        let mut rig = connected_rig(option);
        close_peer_write(&mut rig);
        // The **named** `recv` verb reads now, so this call is the one that discovers the
        // end-of-file and answers it itself (`tcp_recv`'s `n == 0` arm, `inet_drv.c:13206`).
        assert_eq!(
            rig.port.control("recv", b""),
            ctl_xerror("closed"),
            "{option:?}: a clean end-of-file is never a reset"
        );
        assert_eq!(
            rig.port.control("recv", b""),
            ctl_xerror("enotconn"),
            "{option:?}: the one-shot is spent"
        );
    }
}

// -----------------------------------------------------------------------
// The asymmetric send/receive ladders
// -----------------------------------------------------------------------

/// The send ladder clears **only** `TCP_ADDF_DELAYED_CLOSE_SEND` and deliberately leaves
/// `TCP_ADDF_DELAYED_ECONNRESET` armed "for the next receive operation" (`inet_drv.c:12685`), so a
/// receive after a send that reported the reset still reports `econnreset`; the receive ladder
/// then clears both one-shots, so the following send is `enotconn` (`:12279`-`:12288`).
#[test]
fn the_send_ladder_leaves_the_receives_reset_one_shot_armed() {
    let mut rig = connected_rig(SHOW_ECONNRESET);
    abort_peer(&mut rig);
    assert_eq!(
        rig.port.control("send", b"x"),
        ctl_xerror("econnreset"),
        "the send that discovers the reset answers `econnreset` (`:13664`-`:13669`)"
    );
    assert_eq!(
        rig.port.control(&op(42), &RECV_OPERAND),
        ctl_xerror("econnreset"),
        "the reset bit survives the send: the receive still reports it"
    );
    assert_eq!(
        rig.port.control("send", b"x"),
        ctl_xerror("enotconn"),
        "the receive consumed the send's one-shot too"
    );
    assert_eq!(
        rig.port.control(&op(42), &RECV_OPERAND),
        ctl_xerror("enotconn")
    );
}

/// The `erlang:port_command/2` route discovers the reset the same way and arms the same receive
/// one-shot — and the dropped bytes it can never deliver are accounted for, so the port's write
/// accounting does not turn the close into a typed diagnostic.
#[test]
fn a_command_that_discovers_the_reset_arms_the_receive_ladder() {
    let mut rig = connected_rig(SHOW_ECONNRESET);
    abort_peer(&mut rig);
    rig.port
        .command(&[0, 0, b'x'])
        .expect("the command is accepted");
    assert!(
        rig.port.completions().is_empty(),
        "an untagged command stages no `{{inet_reply, …}}` (the C cannot decode an empty tag)"
    );
    assert_eq!(
        rig.port.control(&op(42), &RECV_OPERAND),
        ctl_xerror("econnreset")
    );
    assert_eq!(
        rig.port.control(&op(42), &RECV_OPERAND),
        ctl_xerror("enotconn")
    );
}

// -----------------------------------------------------------------------
// The reactor-discovered route (the C's pending-receive discovery)
// -----------------------------------------------------------------------

/// A **clean end-of-file** the **reactor** discovers (peer `shutdown(Both)`, then `ready_input`
/// reads it) is still answered `closed` by the user's first `recv` and `enotconn` by the second.
///
/// The C's basis: the receive that armed the read interest is answered by
/// `async_error_am_all(am_closed)` (`inet_drv.c:12829`) and the *following* one gets the bare
/// `ctl_error(ENOTCONN)` of the `TCP_REQ_RECV` ladder (`:12286`) because `tcp_recv_closed` armed
/// only the send one-shot. Here the reactor consumed the observation, so that `closed` is armed on
/// the receive ladder instead (`PeerClose::observe`'s `ReadReport::OwedToNextRecv`).
#[test]
fn a_reactor_discovered_end_of_file_answers_closed_then_enotconn() {
    let mut rig = connected_rig(HIDE_ECONNRESET);
    rig.peer
        .as_ref()
        .expect("the far end is still held")
        .shutdown(Shutdown::Both)
        .expect("the peer shuts down");
    drive_reactor(&mut rig);
    assert!(
        rig.port.poller.is_empty(),
        "the reactor's discovery released the dead socket's registration"
    );
    assert_eq!(
        rig.port.control(&op(42), &RECV_OPERAND),
        ctl_xerror("closed"),
        "the reactor's discovery owes the user's first receive a `closed`"
    );
    assert_eq!(
        rig.port.control(&op(42), &RECV_OPERAND),
        ctl_xerror("enotconn"),
        "the one-shot is spent: the connection is simply gone"
    );
}

/// The named `recv` verb reads `TCP_ADDF_DELAYED_CLOSE_RECV` through the same `recv_reply` the
/// numeric `TCP_REQ_RECV`(42) guard does, so a reactor-discovered close answers it identically.
#[test]
fn a_reactor_discovered_end_of_file_answers_the_named_recv_verb_the_same_way() {
    let mut rig = connected_rig(HIDE_ECONNRESET);
    rig.peer
        .as_ref()
        .expect("the far end is still held")
        .shutdown(Shutdown::Both)
        .expect("the peer shuts down");
    drive_reactor(&mut rig);
    assert_eq!(
        rig.port.control("recv", b""),
        ctl_xerror("closed"),
        "the one-shot is the driver's, not the caller's, so the verb sees it too"
    );
    assert_eq!(rig.port.control("recv", b""), ctl_xerror("enotconn"));
}

/// A **reset** the reactor discovers is `econnreset` under `{show_econnreset,true}` and `closed`
/// without it — `tcp_recv`'s `ECONNRESET` arm (`inet_drv.c:13185`-`:13196`) picks
/// `tcp_recv_error` (`:12849`, whose `async_error_am_all(error_atom(err))` is the `econnreset`) only
/// while `TCP_ADDF_SHOW_ECONNRESET` is set, and `tcp_recv_closed` otherwise. The second receive is
/// `enotconn` in both cases.
#[test]
fn a_reactor_discovered_reset_answers_econnreset_only_under_show_econnreset() {
    for (option, expected) in [(SHOW_ECONNRESET, "econnreset"), (HIDE_ECONNRESET, "closed")] {
        let mut rig = connected_rig(option);
        abort_peer(&mut rig);
        drive_reactor(&mut rig);
        assert_eq!(
            rig.port.control(&op(42), &RECV_OPERAND),
            ctl_xerror(expected),
            "{option:?}: the reactor's discovery owes the deferred reason"
        );
        assert_eq!(
            rig.port.control(&op(42), &RECV_OPERAND),
            ctl_xerror("enotconn"),
            "{option:?}: the one-shot is spent"
        );
    }
}

/// The route does not change the **send** ladder: after a reactor-discovered clean end-of-file a
/// `port_command` still answers its caller `{error,closed}` (the send one-shot `tcp_recv_closed`
/// arms unconditionally, `inet_drv.c:12816`) and consumes **only** that one — the receive one-shot
/// the reactor armed survives it, exactly as `tcp_inet_commandv`'s arm leaves it (`:12681`-`:12692`
/// clears `TCP_ADDF_DELAYED_CLOSE_SEND` alone).
#[test]
fn a_reactor_discovered_end_of_file_still_answers_a_send_with_closed() {
    let mut rig = connected_rig(HIDE_ECONNRESET);
    close_peer_write(&mut rig);
    drive_reactor(&mut rig);

    rig.port
        .command(&TAGGED_COMMAND)
        .expect("the command is accepted: a close is a reply, not an error");
    let completions = rig.port.completions();
    assert_eq!(completions.len(), 1, "one command, one reply: {completions:?}");
    match &completions[0].event {
        CompletionEvent::InetReply { caller_ref, reply } => {
            assert_eq!(
                reply,
                &InetReply::Error(InetReason::Closed),
                "the send ladder answers the close, never `enotconn`"
            );
            assert_eq!(caller_ref.as_deref(), Some(&[131u8, 97][..]), "the tag is echoed");
        }
        other => panic!("expected an {{inet_reply, …}} completion, got {other:?}"),
    }

    // The send spent its own one-shot only: the receive's is still armed.
    assert_eq!(
        rig.port.control(&op(42), &RECV_OPERAND),
        ctl_xerror("closed"),
        "the send ladder leaves the deferred receive reply armed"
    );
    assert_eq!(rig.port.control(&op(42), &RECV_OPERAND), ctl_xerror("enotconn"));
    assert_eq!(
        rig.port.control("send", b"x"),
        ctl_xerror("enotconn"),
        "and the send's own one-shot is spent"
    );
}

/// The Erlang half of the reactor rows: a passive `gen_tcp` pair with the peer gone, the two
/// `recv` results printed as a term.
///
/// On the oracle's side the C reaches the close through the **pending-receive** discovery (its
/// read interest exists only while a `recv` is queued); on this floor's side the rows above reach
/// it through the **reactor**. The invariant under test is that the two agree, because the replies
/// belong to the same sequence of *user-visible* operations.
fn oracle_recv_sequence_expression(abort: bool, show_econnreset: bool) -> String {
    let option = if show_econnreset {
        "inet:setopts(S,[{show_econnreset,true}]), "
    } else {
        ""
    };
    let close = if abort {
        "inet:setopts(C,[{linger,{true,0}}]), gen_tcp:close(C)"
    } else {
        "gen_tcp:close(C)"
    };
    format!(
        "{{ok,L}} = gen_tcp:listen(0,[binary,{{active,false}}]), \
         {{ok,P}} = inet:port(L), \
         {{ok,C}} = gen_tcp:connect({{127,0,0,1}},P,[binary,{{active,false}}]), \
         {{ok,S}} = gen_tcp:accept(L), \
         {option}{close}, \
         R1 = gen_tcp:recv(S,0), \
         R2 = gen_tcp:recv(S,0), \
         io:format(\"~p\", [[R1,R2]])"
    )
}

/// The three reactor rows against the system `erl`: the Rust side discovers each close with the
/// **reactor** and the oracle with the C's pending-receive machinery, and both must answer the user
/// the same two atoms.
///
/// Measured on OTP 29.1.1 / erts-17.1 (`erl -noshell -eval`, printed literally):
/// `[{error,closed},{error,enotconn}]` for a clean end-of-file,
/// `[{error,econnreset},{error,enotconn}]` for a reset under `{show_econnreset,true}` and
/// `[{error,closed},{error,enotconn}]` for a reset with the option off. The Rust replies are the
/// C's `ctl_reply(INET_REP_ERROR, …)` bytes for the same atoms, so comparing the atoms compares the
/// behaviour. If `erl` is absent the oracle half skips **loudly**; the Rust half always runs.
#[test]
fn the_reactor_rows_agree_with_the_erl_oracle() {
    let rows = [
        ("a clean end-of-file", false, false, "closed"),
        ("a reset under {show_econnreset,true}", true, true, "econnreset"),
        ("a reset with the option off", true, false, "closed"),
    ];
    for (label, abort, show, first) in rows {
        let mut rig = connected_rig(if show { SHOW_ECONNRESET } else { HIDE_ECONNRESET });
        if abort {
            abort_peer(&mut rig);
        } else {
            close_peer_write(&mut rig);
        }
        drive_reactor(&mut rig);
        let rust_first = rig.port.control(&op(42), &RECV_OPERAND);
        let rust_second = rig.port.control(&op(42), &RECV_OPERAND);
        assert_eq!(rust_first, ctl_xerror(first), "{label}: the first receive");
        assert_eq!(rust_second, ctl_xerror("enotconn"), "{label}: the second receive");

        let Some(stdout) = erl_stdout(&oracle_recv_sequence_expression(abort, show)) else {
            continue; // the oracle is unavailable; the check skipped loudly above
        };
        let expected = format!("[{{error,{first}}},{{error,enotconn}}]");
        assert_eq!(stdout.trim(), expected, "{label}: the oracle disagrees");
        eprintln!("oracle row `{label}`: {}", stdout.trim());
    }
}

// -----------------------------------------------------------------------
// The passive teardown the latch performs (`tcp_desc_close` → `erl_inet_close`)
// -----------------------------------------------------------------------
//
// The reply ladder is only half of a passive close. `tcp_recv_closed`/`tcp_recv_error` tear the
// descriptor down (`tcp_desc_close`, `inet_drv.c:12824`-`:12827` and `:12872`-`:12875`) under the
// default `{exit_on_close,true}`, and `tcp_send_or_shutdown_error` does so **unconditionally** on
// a write discovery (`:13670`). `erl_inet_close` (`:5098`-`:5117`) then runs `desc_close`
// (`state &= ~INET_STATE_CONNECTED`, `:5078`) and sets `INET_STATE_CLOSED` (`:5106`), which is
// what changes five control replies. With `{exit_on_close,false}` the C takes `desc_close_read`
// (`:5083`-`:5098`) instead, which only drops read readiness and **never touches `desc->state`** —
// so the connected replies stay correct there, and must keep doing so.
//
// Measured on OTP 29.1.1 / erts-17.1 through a raw `{spawn_driver,"tcp_inet"}` port whose peer is
// a `gen_tcp` socket that has been closed: the default option answers
// `[1,0,0,0,0]`/`[0]"einval"`/`[0]"enotconn"`/`[0]"ebadf"`/`[1,0,0,0,1,0,0,0,1]`/`[1]`/`[1,0,0,0,1]`
// (GETSTATUS, GETFD, PEER, NAME, GETTYPE, OPEN, GETSTATUS) and `{exit_on_close,false}` answers
// `[1,0,0,0,5]`/`[1]++fd`/`[1,1,…]`/`[1,1,…]`/`[1,0,0,0,1,0,0,0,1]`/`[0]"exbadseq"`/`[1,0,0,0,5]`.
// `the_state_rows_agree_with_the_erl_oracle_for_both_exit_on_close_values` re-derives both.

/// The descriptor-state rows both sides are asked after the close, in this order: the C request
/// code and a label carrying what the row pins.
const STATE_ROWS: [(u32, &str); 7] = [
    (16, "GETSTATUS after the close"),
    (14, "GETFD after the close"),
    (4, "PEER after the close"),
    (5, "NAME after the close"),
    (15, "GETTYPE after the close"),
    (1, "OPEN after the close"),
    (16, "GETSTATUS after the re-open"),
];

/// The rows the oracle expression emits **before** [`STATE_ROWS`]: the opening `OPEN`, the
/// `CONNECT`, the `{exit_on_close, …}` `SETOPTS` and the `TCP_REQ_RECV` that makes the C discover
/// the close (`sock_select(FD_READ|FD_CLOSE, 1)`, `inet_drv.c:12315`).
const ORACLE_PREAMBLE_ROWS: usize = 4;

/// [`STATE_ROWS`] against this floor's rig.
fn rust_state_rows(rig: &mut ResetRig) -> Vec<Vec<u8>> {
    let opening = open_arg();
    STATE_ROWS
        .iter()
        .map(|(request, _)| {
            let arg: &[u8] = if *request == 1 { &opening } else { &[] };
            rig.port.control(&op(*request), arg)
        })
        .collect()
}

/// The Erlang half of [`STATE_ROWS`]: a raw `{spawn_driver,"tcp_inet"}` port connected to a
/// passive `gen_tcp` listener whose accepted socket is then closed, asked every row in the same
/// order and printed as `~w|`.
///
/// The C's discovery needs a receive to be pending — its read interest exists only while a
/// `TCP_REQ_RECV` is queued — so the probe issues one and waits for that receive's **own**
/// `{inet_async, …}` completion before reading the descriptor state (that wait is also what lets
/// the poll loop run). Two orderings are load-bearing and are waited for, never assumed:
///
/// * the raw `INET_REQ_CONNECT`(3) completes **asynchronously** (`tcp_inet_ready_output` sets
///   `INET_STATE_CONNECTED` and only then `async_ok`, `inet_drv.c:14284`/`:14287`), and a
///   `TCP_REQ_RECV` issued while the state is still `INET_STATE_CONNECTING` is refused by the
///   `IS_CONNECTED` guard (`:12279`-`:12288`) — which arms **no** read interest (`:12306`/
///   `:12317`), so the peer's end-of-file would never be read and the descriptor would legitimately
///   stay `CONNECTED`. The probe therefore polls `GETSTATUS` until the connect is through, bounded
///   at 2000 polls (≈2 s);
/// * the receive's completion must be awaited **by its own id** — the one its control reply hands
///   back (`[1] ++ <<Id:16>>`, `enq_async`'s `NEW_ASYNC_ID`, `:1549`) — because a wildcard id
///   matches the connect's completion instead and lets the rows run before the close was read.
///
/// Every wait is bounded and **loud**: an abnormal `-eval` exit loses the whole buffered output,
/// so both give-up arms print to `standard_error` and `halt(1)` (see the node's *Known Issues*).
fn oracle_state_expression(exit_on_close: bool) -> String {
    let code = |request: u64| (CONTROL_MAGIC + request).to_string();
    let rows = STATE_ROWS
        .iter()
        .map(|(request, _)| {
            let arg = if *request == 1 { "1,1,0" } else { "" };
            format!(
                "J(erlang:port_control(P, {}, <<{arg}>>)), ",
                code(u64::from(*request))
            )
        })
        .collect::<String>();
    let wait_for_connected = format!(
        "Wait = fun F(0) -> io:format(standard_error, \"the raw connect never completed~n\"), \
         erlang:halt(1); \
         F(N) -> case erlang:port_control(P, {}, <<>>) of \
                     [1,0,0,0,5] -> ok; \
                     _ -> timer:sleep(1), F(N-1) \
                 end \
         end, \
         Wait(2000), ",
        code(16),
    );
    let recv_and_wait = format!(
        "Rq = erlang:port_control(P, {}, <<0:64>>), \
         J(Rq), \
         <<1,Id:16>> = iolist_to_binary([Rq]), \
         receive {{inet_async,P,Id,_}} -> ok after 5000 -> \
             io:format(standard_error, \"no completion for the queued receive~n\"), erlang:halt(1) \
         end, ",
        code(42),
    );
    format!(
        "J = fun(R) -> io:format(\"~w|\", [R]) end, \
         {{ok,L}} = gen_tcp:listen(0,[binary,{{active,false}}]), \
         {{ok,LP}} = inet:port(L), \
         P = open_port({{spawn_driver,\"tcp_inet\"}}, [binary]), \
         J(erlang:port_control(P, {}, <<1,1,0>>)), \
         J(erlang:port_control(P, {}, <<0,0,3,232,4,LP:16>>)), \
         J(erlang:port_control(P, {}, <<26,0,0,0,{}>>)), \
         {{ok,C}} = gen_tcp:accept(L, 5000), \
         {wait_for_connected}gen_tcp:close(C), \
         {recv_and_wait}{rows}erlang:port_close(P)",
        code(1),
        code(3),
        code(7),
        u8::from(exit_on_close),
    )
}

/// The default `{exit_on_close,true}`: a passive socket that has read the peer's end-of-file runs
/// `tcp_desc_close` (`inet_drv.c:12824`-`:12827`), so `erl_inet_close` (`:5098`-`:5117`) closes the
/// descriptor (`:5078` clears `INET_STATE_CONNECTED`) and sets `INET_STATE_CLOSED` (`:5106`).
/// Every descriptor-state reply flips with it, `GETTYPE` does not (`inet.stype`/`inet.sfamily` are
/// cached at `open`, `:5214`-`:5216`), and `OPEN` is legal again because its `exbadseq` guard
/// only rejects a non-`Closed` state (`inet_ctl_open`, `:5130`).
#[test]
fn the_default_options_tear_the_descriptor_down_after_a_close() {
    let mut rig = connected_rig(HIDE_ECONNRESET);
    close_peer_write(&mut rig);
    drive_reactor(&mut rig);
    assert!(
        rig.port.poller.is_empty(),
        "the reactor's discovery released the dead socket's registration"
    );

    assert_eq!(
        rig.port.control(&op(16), &[]),
        vec![1, 0, 0, 0, 0],
        "GETSTATUS is INET_STATE_CLOSED (`inet_drv.c:5106`), not CONNECTED (5)"
    );
    assert_eq!(
        rig.port.control(&op(14), &[]),
        ctl_xerror("einval"),
        "GETFD has no descriptor left (`inet_drv.c:11160`'s `IS_OPEN` refusal)"
    );
    assert_eq!(
        rig.port.control(&op(4), &[]),
        ctl_xerror("enotconn"),
        "PEER refuses on a torn-down connection (`:11296`)"
    );
    assert_eq!(
        rig.port.control(&op(5), &[]),
        ctl_xerror("ebadf"),
        "NAME answers the `getsockname(2)` errno of a nil descriptor (`:11329`)"
    );
    assert_eq!(
        rig.port.control(&op(15), &[]),
        vec![1, 0, 0, 0, 1, 0, 0, 0, 1],
        "GETTYPE is unaffected: the family/type are cached at `open` (`:5214`-`:5216`)"
    );
    assert_eq!(
        rig.port.control(&op(1), &open_arg()),
        ctl_ok(),
        "`INET_REQ_OPEN` is legal again — its guard only rejects a non-Closed state (`:5130`)"
    );
    assert_eq!(
        rig.port.control(&op(16), &[]),
        vec![1, 0, 0, 0, 1],
        "the re-`open` created a fresh socket, so the state is INET_STATE_OPEN"
    );
}

/// The twin: with `{exit_on_close,false}` the C's read discovery takes `desc_close_read`
/// (`inet_drv.c:5083`-`:5098`), which only drops read readiness and **never touches
/// `desc->state`** — so the connected replies are correct and stay that way. Pinned so the
/// teardown can never be "fixed" into firing there too.
#[test]
fn exit_on_close_false_keeps_the_descriptor_and_the_connected_state() {
    let mut rig = connected_rig(HIDE_ECONNRESET);
    assert_eq!(
        rig.port.control(&op(7), &EXIT_ON_CLOSE_OFF),
        ctl_ok(),
        "the `{{exit_on_close,false}}` record is accepted"
    );
    let fd = rig.fd;
    close_peer_write(&mut rig);
    drive_reactor(&mut rig);

    assert_eq!(
        rig.port.control(&op(16), &[]),
        vec![1, 0, 0, 0, 5],
        "`desc_close_read` leaves INET_STATE_CONNECTED (`inet_drv.c:5083`-`:5098`)"
    );
    let mut descriptor = ctl_ok();
    descriptor.extend_from_slice(&fd.to_be_bytes());
    assert_eq!(
        rig.port.control(&op(14), &[]),
        descriptor,
        "the descriptor is still the driver's, so GETFD reports it"
    );
    assert_eq!(
        rig.port.control(&op(1), &open_arg()),
        ctl_xerror("exbadseq"),
        "the state is not Closed, so `inet_ctl_open` answers the sequence error (`:5130`)"
    );
    assert_eq!(
        rig.port.control(&op(16), &[]),
        vec![1, 0, 0, 0, 5],
        "and it is still connected afterwards"
    );
}

/// The teardown must not swallow the reply ladder: the C keeps `desc->tcp_add_flags`
/// (`TCP_ADDF_DELAYED_CLOSE_RECV`/`_ECONNRESET`) across `tcp_desc_close`, and the receive ladder
/// consults that flag **before** `INET_STATE_CONNECTED` (`inet_drv.c:12279`-`:12286`) — which is
/// exactly why `async_recv`/`verb_recv` read `PeerClose::recv_reply` first. Without that order
/// the armed `closed`/`econnreset` would become the bare `enotconn` as soon as the descriptor
/// reached `INET_STATE_CLOSED`.
#[test]
fn the_latch_keeps_its_one_shot_whatever_the_teardown_did() {
    // A clean end-of-file: `closed` first, with and without the teardown.
    for exit_on_close in [true, false] {
        let mut rig = connected_rig(HIDE_ECONNRESET);
        if !exit_on_close {
            assert_eq!(rig.port.control(&op(7), &EXIT_ON_CLOSE_OFF), ctl_ok());
        }
        close_peer_write(&mut rig);
        drive_reactor(&mut rig);
        if exit_on_close {
            // Torn down, so `INET_STATE_CLOSED` is set (`inet_drv.c:5106`) and both ladders'
            // flag-before-state order is load-bearing — the send's
            // (`tcp_inet_commandv`, `:12681`-`:12692`) and the receive's (`:12279`-`:12286`).
            // A state-first guard would answer the bare `enotconn` and swallow the one-shot.
            assert_eq!(
                rig.port.control("send", b"x"),
                ctl_xerror("closed"),
                "the send ladder's one-shot is read before the (now Closed) state"
            );
            assert_eq!(
                rig.port.control("send", b"x"),
                ctl_xerror("enotconn"),
                "and it is spent after one use"
            );
        }
        assert_eq!(
            rig.port.control(&op(42), &RECV_OPERAND),
            ctl_xerror("closed"),
            "{exit_on_close}: the armed `TCP_ADDF_DELAYED_CLOSE_RECV` one-shot survives the latch"
        );
        if exit_on_close {
            // `INET_STATE_CONNECTED` is gone, so the ladder's `else` is `enotconn`
            // (`inet_drv.c:12286`). (Without the teardown the C's socket is still open, so a
            // further receive is a *fresh* operation rather than a bare `enotconn` — not pinned
            // here, and unchanged by this teardown.)
            assert_eq!(
                rig.port.control(&op(42), &RECV_OPERAND),
                ctl_xerror("enotconn"),
                "{exit_on_close}: the one-shot is spent and the connection is gone"
            );
            assert_eq!(
                rig.port.control("recv", b""),
                ctl_xerror("enotconn"),
                "{exit_on_close}: the named verb reads the same ladder"
            );
        }
    }

    // A reset: `econnreset` needs `TCP_ADDF_DELAYED_ECONNRESET`, which `tcp_recv`'s
    // `ECONNRESET` arm (`:13185`-`:13196`) only keeps while `{show_econnreset,true}` is set —
    // and it too must survive the teardown.
    for (option, expected) in [(SHOW_ECONNRESET, "econnreset"), (HIDE_ECONNRESET, "closed")] {
        let mut rig = connected_rig(option);
        abort_peer(&mut rig);
        drive_reactor(&mut rig);
        assert_eq!(
            rig.port.control(&op(42), &RECV_OPERAND),
            ctl_xerror(expected),
            "{option:?}: the teardown does not lose the armed reset one-shot"
        );
        assert_eq!(
            rig.port.control(&op(42), &RECV_OPERAND),
            ctl_xerror("enotconn"),
            "{option:?}: the one-shot is spent"
        );
    }
}

/// The whole [`STATE_ROWS`] table against the system `erl`, for both `{exit_on_close, …}` values:
/// the raw reply bytes must be identical, not hard-coded from memory. A descriptor number and an
/// ephemeral port differ between two runtimes, so those two rows compare shape and address octets
/// only ([`Rule::Descriptor`]/[`Rule::PackedAddress`]); every other row is byte-exact.
///
/// Skips **loudly** when `erl` is absent (`erl_stdout`); the Rust half always runs.
#[test]
fn the_state_rows_agree_with_the_erl_oracle_for_both_exit_on_close_values() {
    for (label, exit_on_close, torn_down) in [
        ("the default {exit_on_close,true}", true, true),
        ("{exit_on_close,false}", false, false),
    ] {
        let mut rig = connected_rig(HIDE_ECONNRESET);
        if !exit_on_close {
            assert_eq!(
                rig.port.control(&op(7), &EXIT_ON_CLOSE_OFF),
                ctl_ok(),
                "{label}: the option is accepted"
            );
        }
        close_peer_write(&mut rig);
        drive_reactor(&mut rig);
        assert!(
            rig.port
                .registry
                .port_closed(rig.port.id)
                .expect("the port is still live"),
            "{label}: the reactor's discovery latched the peer close"
        );
        let rust = rust_state_rows(&mut rig);

        let Some(dump) = erl_stdout(&oracle_state_expression(exit_on_close)) else {
            continue; // the oracle is unavailable; the check skipped loudly above
        };
        let replies = oracle_replies(&dump);
        assert_eq!(
            replies.len(),
            ORACLE_PREAMBLE_ROWS + STATE_ROWS.len(),
            "{label}: the oracle emitted one reply per call: {dump}"
        );
        let oracle = &replies[ORACLE_PREAMBLE_ROWS..];
        for (index, (request, name)) in STATE_ROWS.iter().enumerate() {
            let rule = if torn_down {
                Rule::Exact
            } else {
                match *request {
                    14 => Rule::Descriptor,
                    4 | 5 => Rule::PackedAddress,
                    _ => Rule::Exact,
                }
            };
            assert_agrees(&format!("{label}: {name}"), rule, &oracle[index], &rust[index]);
        }
        eprintln!("oracle rows `{label}`: {}", dump.trim());
    }
}

// -----------------------------------------------------------------------
// Remaining work (reported, not silently skipped)
// -----------------------------------------------------------------------
//
// An `erl` cross-check of the rows above needs the oracle probe to force the C's **write**-path
// discovery of the reset (which is what arms `TCP_ADDF_DELAYED_CLOSE_RECV` +
// `TCP_ADDF_DELAYED_ECONNRESET`, `inet_drv.c:13678`/`:13683`). A single `-eval` expression cannot
// order that deterministically: `port_command` may complete before the RST is delivered, and the
// C then discovers the reset on the **read** path instead (`tcp_recv_error`, which arms nothing and
// answers the `{inet_async, …}` completion with `{error,econnreset}` — measured on OTP 29.1.1:
// `[1]`, `[1,0,1]`, `[0]"enotconn"` with the mailbox carrying `{error,econnreset}`). Doing it
// faithfully needs a probe that drives the port's descriptor with `poll(2)` and collects the
// `{inet_async, …}` messages, i.e. a small escript rather than `erl_stdout`'s one-liner. The
// measured oracle rows the ladder must match (raw `tcp_inet` port, OTP 29.1.1 / erts-17.1) are:
//   * `{show_econnreset,true}` + send-discovered RST: `[0]"econnreset"` then `[0]"enotconn"`;
//   * default option + send-discovered RST:        `[0]"closed"` then `[0]"enotconn"`;
//   * clean peer `shutdown(write)`, either option: `[0]"closed"` then `[0]"enotconn"`.
