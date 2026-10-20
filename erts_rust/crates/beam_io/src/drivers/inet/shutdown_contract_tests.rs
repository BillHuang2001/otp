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

//! The **Erlang-level `gen_tcp:shutdown/2` contract**: what the connection does *after* each of the
//! three directions is shut down, and how that differs from the driver-level guard ladder.
//!
//! The sibling [`tcp_shutdown_tests`](super::tcp_shutdown_tests) pins the `TCP_REQ_SHUTDOWN`(44)
//! **arm itself** — the guard order, the wrong-length reply and the output-queue deferral. This
//! suite pins the observable *aftermath*, which is what `gen_tcp`'s users actually depend on:
//! which later operations still work, which are refused, and with which reason.
//!
//! `gen_tcp:shutdown/2` is `prim_inet:shutdown/2` (`erts/preloaded/src/prim_inet.erl:156`-`:167`),
//! which maps the three atoms onto the `how` byte and then drives
//! `ctl_cmd(S, ?TCP_REQ_SHUTDOWN, [How])` (`:2969`-`:2985`):
//!
//! | `gen_tcp:shutdown/2` | `how` | our constant |
//! |---|---|---|
//! | `read` | `0` | [`TCP_SHUT_RD`] |
//! | `write` | `1` | [`TCP_SHUT_WR`] |
//! | `read_write` | `2` | [`TCP_SHUT_RDWR`] |
//!
//! `shutdown_1/2` maps the bare `[1]` reply to `ok` and a `[0] ++ Name` reply to `{error, Name}`
//! (`:160`-`:163`), so every row below is a *reply*, never a raise.
//!
//! # The C's aftermath rules
//!
//! * `shutdown(2)` on a connected socket changes **no** `inet` state: the descriptor stays
//!   `INET_STATE_CONNECTED` while the socket itself degrades. So a `read` shutdown leaves sending
//!   completely intact (`tcp_sendv` never consults the read side) and a `write` shutdown leaves
//!   reading intact — a later `TCP_REQ_RECV` completes normally from the peer's bytes.
//! * After a **write** shutdown the first later write gets `EPIPE` from the kernel, and
//!   `tcp_send_error` (`inet_drv.c:13717`) **rewrites** it to `ECONNRESET` — the direct success arm
//!   of `TCP_REQ_SHUTDOWN` (`:12378`) latches no `TCP_ADDF_SHUTDOWN_WR_DONE`.
//!   `tcp_send_or_shutdown_error` (`:13613`-`:13688`) then answers the caller `econnreset` when
//!   `{show_econnreset,true}` is in force (`:13615`, `inet_reply_error`) and `closed` otherwise
//!   (`inet_reply_error_am(am_closed)`, `:13667`), and ends the descriptor (`tcp_desc_close`,
//!   `:13670`). That teardown (`:13668`, `tcp_desc_close` → `erl_inet_close`) drops the connection
//!   out of `INET_STATE_CONNECTED`, so every **later** operation is the bare `enotconn`
//!   (`tcp_inet_commandv`, `:12694`).
//! * `TCP_ADDF_SHUTDOWN_WR_DONE` is set in exactly **two** places, neither of them the direct success
//!   arm (`:12378`, `return ctl_reply_ok(...)`, which latches nothing): the **failure** arm of
//!   `TCP_REQ_SHUTDOWN`, before its error reply (`:12372`), and the **deferred** drain
//!   `tcp_shutdown_async` (`:13973`). It is consumed at `:13717`, so its whole point is to keep a
//!   *pending* write shutdown from being reported as the peer's reset: a **successful direct**
//!   `shutdown(write)` leaves the flag clear and the later `EPIPE` really is `ECONNRESET` under
//!   `{show_econnreset,true}`, while the **deferred** leg keeps the `EPIPE` and answers `closed`
//!   whatever the option says.
//! * A repeated `shutdown` is still `[1]`: the descriptor is still connected and `shutdown(2)` on an
//!   already-shut direction succeeds. `enotconn` appears only once the teardown above has run.
//! * A **failed** `shutdown(2)` still latches `TCP_ADDF_SHUTDOWN_WR_DONE` for a write-side `how`
//!   (`:12370`-`:12374`, the latch runs *before* the error reply), which is why the flag is a
//!   "a write shutdown was attempted" mark rather than "it worked".
//! * `tcp_inet_commandv` (`:12695`) routes a command queued on a socket with a *deferred*
//!   shutdown ([`TcpInetDriver::pending_shutdown`](super::TcpInetDriver), the C's
//!   `TCP_ADDF_PENDING_SHUT_WR`/`_RDWR`, `:12356`-`:12368`) straight to
//!   `tcp_shutdown_error(desc, EPIPE)`, which always answers `closed`: the `EPIPE` it hands over is
//!   never a peer reset, so `{show_econnreset, …}` cannot change that reply.
//!
//! # Measured on OTP 29.1.1 / erts-17.1
//!
//! The 34-row table at the bottom of this file is byte-compared against the system `escript`: the
//! 24 baseline rows covering the three `how` values, plus two `{show_econnreset,true}` groups. Those
//! two groups are what separates a **direct** write shutdown (the later write is the reset) from a
//! **deferred** one (the later write is `closed`, because there the latch really is set) — the
//! contrast the deferred case
//! [`a_deferred_write_shutdown_keeps_the_epipe_so_the_reply_is_closed`] pins in-process.

use std::io::{Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::op_queue::async_tests::{await_events, linked, recv_operand};
use super::opcode_tests::{connection, escript_stdout, op, listening_port, Rig};
use super::opt::{INET_INFINITY, INET_OPT_SNDBUF};
use super::wire::{ctl_ok, INET_REP_OK};
use super::{TCP_SHUT_RD, TCP_SHUT_RDWR, TCP_SHUT_WR};
use crate::completion::InetAsyncResult;

/// `TCP_REQ_SHUTDOWN` (`inet_drv.c:1107`).
const SHUTDOWN: u32 = 44;
/// `TCP_REQ_RECV` (`inet_drv.c:110`).
const TCP_REQ_RECV: u32 = 42;
/// `INET_REQ_SETOPTS` (`inet_drv.c:597`) — the option-record walk.
const INET_REQ_SETOPTS: u32 = 7;
/// `INET_REQ_GETSTATUS` (`inet_drv.c:612`) — the descriptor-state read-back.
const INET_REQ_GETSTATUS: u32 = 16;
/// `INET_STATE_CONNECTED` (`inet_drv.c:668`).
const INET_STATE_CONNECTED: u32 = 5;
/// `INET_STATE_CLOSED` (`inet_drv.c:664`).
const INET_STATE_CLOSED: u32 = 0;
/// `INET_STATE_LISTENING` (`inet_drv.c:666`).
const INET_STATE_LISTENING: u32 = 9;
/// `INET_LOPT_TCP_SHOW_ECONNRESET` (`inet_drv.c:706`) — the five-byte `[code, value_be32]` record.
const SHOW_ECONNRESET: u8 = 39;

/// A byte the `how` field does not define — it reaches `shutdown(2)`, which answers `EINVAL`, so
/// the C's failure arm (`inet_drv.c:12370`-`:12374`) is the one that runs.
const UNKNOWN_HOW: u8 = 3;

/// How long the far end's read may take before the test fails instead of hanging.
const READ_GUARD: Duration = Duration::from_secs(30);

/// The bound on the deferred-shutdown drain below. The wait ends the instant the port's output queue
/// empties, so this only stops a genuinely broken driver from hanging the suite — it is never a
/// latency budget tuned for an idle host.
const HANG_GUARD: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------
// Rendering (the `prim_inet` vocabulary both sides speak)
// ---------------------------------------------------------------------------

/// `prim_inet:shutdown_1/2`'s rendering of a `TCP_REQ_SHUTDOWN` reply: `[1]` ⇒ `ok`, `[0] ++ Name`
/// ⇒ `{error,Name}` (`erts/preloaded/src/prim_inet.erl:160`-`:163`). `prim_inet:ctl_cmd` builds the
/// same pair for every other control call (`:2978`-`:2983`), which is why one renderer covers both.
fn render_result(reply: &[u8]) -> String {
    match reply.split_first() {
        Some((&INET_REP_OK, rest)) if rest.is_empty() => "ok".to_owned(),
        // `ctl_cmd` answers `{ok, Tail}` when the reply carries a payload, and `~w` of a byte tail
        // is the integer list this renders.
        Some((&INET_REP_OK, rest)) => render_bytes(rest),
        Some((&super::wire::INET_REP_ERROR, name)) => {
            format!("{{error,{}}}", String::from_utf8_lossy(name))
        }
        Some((tag, _)) => panic!("`{tag}` is not a result tag: {reply:?}"),
        None => panic!("a control reply is never empty"),
    }
}

/// `~w` of a raw reply list, e.g. `[1,0,0,0,5]` — the `GETSTATUS` rows' shape, and the spelling the
/// listener group's last row uses for a `[0] ++ Name` reply (`[0,101,110,…]`, the raw bytes rather
/// than the `{error,Name}` form [`render_result`] builds from the same reply).
fn render_bytes(reply: &[u8]) -> String {
    let inner = reply
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!("[{inner}]")
}

/// `~w` of the `{ok, Data}` shape `prim_inet` returns for an asynchronous receive in `{mode,list}`
/// (the C's default, `INET_MODE_LIST`), with the bytes as a flat int list.
fn render_data(bytes: &[u8]) -> String {
    let inner = bytes.iter().map(u8::to_string).collect::<Vec<_>>().join(",");
    format!("{{ok,[{inner}]}}")
}

// ---------------------------------------------------------------------------
// Driving the port
// ---------------------------------------------------------------------------

/// One `shutdown` call, asserted to be a **reply** (the C's `ctl_error`/`ctl_reply_ok`), never an
/// argument raise.
fn shutdown(port: &mut Rig, how: u8) -> Vec<u8> {
    match port.try_control(&op(SHUTDOWN), &[how]) {
        Ok(reply) => reply,
        Err(err) => panic!("`shutdown`/{how} raised {err:?} instead of answering a reply"),
    }
}

/// The named `send` verb's reply — the C's `TCP_REQ_SEND` body, which is `tcp_sendv`'s caller.
fn send(port: &mut Rig, payload: &[u8]) -> Vec<u8> {
    match port.try_control("send", payload) {
        Ok(reply) => reply,
        Err(err) => panic!("`send` raised {err:?} instead of answering a reply"),
    }
}

/// The `INET_REQ_GETSTATUS` state code (`5` connected, `0` closed, `9` listening).
fn status(port: &mut Rig) -> Vec<u8> {
    let reply = port.control(&op(INET_REQ_GETSTATUS), &[]);
    assert_eq!(reply.len(), 5, "`[1] ++ be32(state)`: {reply:?}");
    reply
}

/// Arms a passive receive and waits for its completion, returning the delivered bytes — asserted to
/// be exactly the `want` the peer wrote, so a short or duplicated read cannot pass silently.
fn receive(port: &mut Rig, want: usize) -> Vec<u8> {
    let armed = port.control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 0));
    assert_eq!(armed[0], INET_REP_OK, "the arming reply is `[1] ++ id`");
    let events = await_events(port, 1);
    match &events[0].1 {
        InetAsyncResult::Data(read) => {
            let delivered = read.flattened();
            assert_eq!(
                delivered.len(),
                want,
                "the receive delivered {delivered:?} instead of the {want} bytes the peer wrote"
            );
            delivered
        }
        other => panic!("the receive produced {other:?} instead of data"),
    }
}

/// The port's current output-queue depth (the C's `driver_sizeq`) — what decides whether a write
/// shutdown is applied at once or deferred (`inet_drv.c:12356`-`:12368`).
fn output_len(port: &mut Rig) -> usize {
    port.registry.get(port.id).expect("a live port").output_len()
}

/// One reactor round: poll for readiness (write readiness re-drives a stalled write through
/// `ready_output`) and run the port tasks it scheduled. This is also what lets a deferred
/// `shutdown(2)` run — [`super::TcpInetDriver::verb_shutdown`]'s pending arm is drained from
/// `write_pending` the moment the queue empties.
fn poll_round(port: &mut Rig) {
    let events = match port.poller.poll(Some(Duration::from_millis(20))) {
        Ok(events) => events,
        Err(err) => panic!("the poll set failed: {err:?}"),
    };
    match port.registry.handle_events(&mut port.poller, &events) {
        Ok(_outcome) => {}
        Err(err) => panic!("the reactor round failed: {err:?}"),
    }
}

// ---------------------------------------------------------------------------
// Row 1 — a read shutdown leaves the send side intact
// ---------------------------------------------------------------------------

/// **`shutdown(read)` is `ok`, is idempotent, and does not touch the write side.** The reply is the
/// bare `[1]`; a second `shutdown(read)` is still `[1]` (the descriptor is still
/// `INET_STATE_CONNECTED` and `shutdown(2)` on an already-shut direction succeeds); and a send
/// afterwards is `ok` with the peer really receiving the payload — the C's read-side degradation
/// never reaches `tcp_sendv`.
///
/// Oracle: `[1,0,0,0,5] ok ok ok {ok,[97,97]} [1,0,0,0,5]` (state, three replies, the peer's bytes,
/// the unchanged state).
#[test]
fn a_read_shutdown_leaves_sending_intact() {
    let (mut port, mut peer) = linked();
    assert_eq!(
        status(&mut port),
        [INET_REP_OK, 0, 0, 0, INET_STATE_CONNECTED as u8],
        "the rows start on a connected socket"
    );

    assert_eq!(render_result(&shutdown(&mut port, TCP_SHUT_RD)), "ok");
    assert_eq!(
        render_result(&shutdown(&mut port, TCP_SHUT_RD)),
        "ok",
        "a repeated read shutdown is still `[1]`: `shutdown(2)` on a shut direction succeeds"
    );
    assert_eq!(
        render_result(&send(&mut port, b"aa")),
        "ok",
        "`tcp_sendv` never consults the read side, so sending still works"
    );

    let mut received = [0u8; 2];
    peer.set_read_timeout(Some(READ_GUARD)).expect("a guard");
    peer.read_exact(&mut received).expect("the peer reads");
    assert_eq!(&received, b"aa", "the payload really reached the peer");
    assert_eq!(
        status(&mut port),
        [INET_REP_OK, 0, 0, 0, INET_STATE_CONNECTED as u8],
        "a read shutdown changes no `inet` state"
    );
}

// ---------------------------------------------------------------------------
// Row 2 — a write shutdown ends the connection on the first later write
// ---------------------------------------------------------------------------

/// **`shutdown(write)` is `ok` and repeatable; the first later write is `{error,closed}` and every
/// write after it is `{error,enotconn}`.**
///
/// The kernel answers that write with `EPIPE`, and `tcp_send_error` (`inet_drv.c:13717`)
/// **rewrites** it to `ECONNRESET`: the direct success arm of the shutdown (`:12378`) latches no
/// `TCP_ADDF_SHUTDOWN_WR_DONE`. Without `{show_econnreset,true}` that reset is still rendered
/// `closed` (`inet_reply_error_am(am_closed)`, `:13667`), and `tcp_send_or_shutdown_error`
/// (`:13613`-`:13688`) runs `tcp_desc_close` (`:13670`) — which is also why `GETSTATUS` drops to
/// `INET_STATE_CLOSED`. The reset's own spelling is
/// [`a_write_shutdown_under_show_econnreset_reports_the_reset`]'s.
#[test]
fn a_write_shutdown_refuses_the_next_send_and_then_reports_enotconn() {
    let mut conn = connection();
    assert_eq!(
        render_result(&shutdown(&mut conn.port, TCP_SHUT_WR)),
        "ok",
        "a write shutdown on a connected socket is `[1]`"
    );
    assert_eq!(
        render_result(&shutdown(&mut conn.port, TCP_SHUT_WR)),
        "ok",
        "a repeated write shutdown is `[1]`, not `{{error,enotconn}}`: the descriptor is still \
         connected"
    );
    assert_eq!(
        render_result(&send(&mut conn.port, b"bb")),
        "{error,closed}",
        "the write after a write shutdown answers the C's `inet_reply_error_am(am_closed)`, which \
         `prim_inet:ctl_cmd` renders `{{error,closed}}`"
    );
    assert_eq!(
        render_result(&send(&mut conn.port, b"cc")),
        "{error,enotconn}",
        "the first failure ended the connection (`tcp_desc_close`), so the next write is `enotconn`"
    );
    assert_eq!(
        status(&mut conn.port),
        [INET_REP_OK, 0, 0, 0, INET_STATE_CLOSED as u8],
        "`tcp_desc_close` → `erl_inet_close` leaves `INET_STATE_CLOSED`"
    );
}

// ---------------------------------------------------------------------------
// Row 3 — a read_write shutdown closes both directions at once
// ---------------------------------------------------------------------------

/// **`shutdown(read_write)` is `ok` and repeatable; both a send and a receive afterwards are
/// `{error,closed}`.**
///
/// Both directions really are gone, so the write fails immediately (`EPIPE`) and the receive is
/// answered from the armed receive one-shot that failure latches
/// (`tcp_send_or_shutdown_error`'s `TCP_ADDF_DELAYED_CLOSE_RECV`, `inet_drv.c:13678`) rather than
/// from the bare `enotconn` a spent ladder would give.
#[test]
fn a_read_write_shutdown_closes_both_directions() {
    let mut conn = connection();
    assert_eq!(
        render_result(&shutdown(&mut conn.port, TCP_SHUT_RDWR)),
        "ok",
        "`shutdown(2)` with `SHUT_RDWR` on a connected socket is `[1]`"
    );
    assert_eq!(
        render_result(&shutdown(&mut conn.port, TCP_SHUT_RDWR)),
        "ok",
        "and repeating it is still `[1]`"
    );
    assert_eq!(
        render_result(&send(&mut conn.port, b"ff")),
        "{error,closed}",
        "the write after a `SHUT_RDWR` answers `closed`, not `{{error,enotconn}}`"
    );
    assert_eq!(
        render_result(&conn.port.control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 0))),
        "{error,closed}",
        "the receive one-shot the write failure armed answers `closed`, not `enotconn`"
    );
    assert_eq!(
        status(&mut conn.port),
        [INET_REP_OK, 0, 0, 0, INET_STATE_CLOSED as u8],
    );
}

// ---------------------------------------------------------------------------
// Row 4 — the read side still works after a write shutdown
// ---------------------------------------------------------------------------

/// **A write shutdown does not stop reading.** With the write side shut, a passive receive armed on
/// the socket still delivers the peer's later payload — the C's `TCP_REQ_RECV` arm has no
/// write-side guard, and the socket's read half is untouched by `SHUT_WR`. The descriptor stays
/// `INET_STATE_CONNECTED` (a write shutdown changes no `inet` state).
#[test]
fn a_write_shutdown_keeps_the_read_side_working() {
    let (mut port, mut peer) = linked();
    assert_eq!(render_result(&shutdown(&mut port, TCP_SHUT_WR)), "ok");

    peer.write_all(b"hello").expect("the peer writes");
    peer.flush().expect("the peer flushes");

    assert_eq!(render_data(&receive(&mut port, 5)), "{ok,[104,101,108,108,111]}");
    assert_eq!(
        status(&mut port),
        [INET_REP_OK, 0, 0, 0, INET_STATE_CONNECTED as u8],
        "nothing about the write shutdown touches `inet` state"
    );
}

// ---------------------------------------------------------------------------
// Row 5 — a listener answers enotconn for every `how`
// ---------------------------------------------------------------------------

/// **A bound-and-listening port answers `[0]enotconn` for every `how`.** The C's `!IS_CONNECTED`
/// guard (`inet_drv.c:12347`) is the arm's first statement, so the `how` byte is never even read;
/// `GETSTATUS` stays `INET_STATE_LISTENING` (`9`).
#[test]
fn a_listener_answers_enotconn_for_every_how() {
    let (mut listener, _port) = listening_port();
    for how in [TCP_SHUT_RD, TCP_SHUT_WR, TCP_SHUT_RDWR, UNKNOWN_HOW] {
        assert_eq!(
            render_result(&shutdown(&mut listener, how)),
            "{error,enotconn}",
            "`!IS_CONNECTED ⇒ ctl_error(ENOTCONN)` for `how` = {how}"
        );
    }
    assert_eq!(
        status(&mut listener),
        [INET_REP_OK, 0, 0, 0, INET_STATE_LISTENING as u8],
        "the refused shutdowns leave the listener listening"
    );
}

// ---------------------------------------------------------------------------
// Row 6 — the `how` byte mapping is `prim_inet`'s
// ---------------------------------------------------------------------------

/// **`prim_inet:shutdown/2`'s three atoms map onto `0`/`1`/`2`**, the C's
/// `TCP_SHUT_RD`/`TCP_SHUT_WR`/`TCP_SHUT_RDWR` (`erts/preloaded/src/prim_inet.erl:156`-`:167`), and
/// each of those three bytes is accepted on a connected socket.
#[test]
fn the_how_byte_mapping_is_the_prim_inet_one() {
    assert_eq!(
        (TCP_SHUT_RD, TCP_SHUT_WR, TCP_SHUT_RDWR),
        (0, 1, 2),
        "`read`/`write`/`read_write` are `0`/`1`/`2` in `prim_inet:shutdown/2`"
    );
    for how in [TCP_SHUT_RD, TCP_SHUT_WR, TCP_SHUT_RDWR] {
        let mut conn = connection();
        assert_eq!(
            render_result(&shutdown(&mut conn.port, how)),
            "ok",
            "the mapped byte {how} is accepted on a connected socket"
        );
    }
}

// ---------------------------------------------------------------------------
// Row 7 — a failed write shutdown still counts as attempted
// ---------------------------------------------------------------------------

/// **A write-side `shutdown(2)` the kernel refuses leaves the connection usable.** An undefined
/// `how` byte falls through the deferral gate to `shutdown(2)`, whose `EINVAL` the C answers as
/// `[0]einval` (`inet_drv.c:12375`) — and the socket itself is untouched, so a later send is `ok`
/// and really reaches the peer.
///
/// The same arm latches `TCP_ADDF_SHUTDOWN_WR_DONE` **before** the error reply
/// (`inet_drv.c:12370`-`:12374`), so the flag means "a write shutdown was attempted" rather than
/// "it worked". Its observable is the **deferred** leg, not this one: a queued write shutdown latches
/// the flag on success (`tcp_shutdown_async`, `:13973`), so the later `EPIPE` keeps its own value and
/// answers `closed` even under `{show_econnreset,true}` — see
/// [`a_deferred_write_shutdown_keeps_the_epipe_so_the_reply_is_closed`].
#[test]
fn a_failed_write_shutdown_leaves_the_connection_usable() {
    let (mut port, mut peer) = linked();
    assert_eq!(
        render_result(&shutdown(&mut port, UNKNOWN_HOW)),
        "{error,einval}",
        "`shutdown(2)`'s `EINVAL`, via `ctl_error(sock_errno())`"
    );
    assert_eq!(
        render_result(&send(&mut port, b"zz")),
        "ok",
        "the refused shutdown changed nothing: the socket can still write"
    );
    let mut received = [0u8; 2];
    peer.set_read_timeout(Some(READ_GUARD)).expect("a guard");
    peer.read_exact(&mut received).expect("the peer reads");
    assert_eq!(&received, b"zz", "and the payload really reached the peer");
}

/// **A successful `shutdown(write)` under `{show_econnreset,true}` reports the later write as the
/// reset.** The direct success arm latches **no** `TCP_ADDF_SHUTDOWN_WR_DONE` (`inet_drv.c:12378`),
/// so `tcp_send_error` (`:13717`) rewrites the kernel's `EPIPE` to `ECONNRESET` and
/// `tcp_send_or_shutdown_error`'s `err == ECONNRESET && TCP_ADDF_SHOW_ECONNRESET` test (`:13615`)
/// takes the `inet_reply_error` arm — `{error,econnreset}`. The teardown then arms
/// `TCP_ADDF_DELAYED_CLOSE_RECV` (`:13678`) and, because the observation was a reset,
/// `TCP_ADDF_DELAYED_ECONNRESET` (`:13683`), so the receive after it answers the reset too — while
/// the send one-shot is already spent, which is the `enotconn`.
///
/// Oracle (a real loopback pair): `ok {error,econnreset} {error,enotconn} {error,econnreset}
/// [1,0,0,0,0]`.
#[test]
fn a_write_shutdown_under_show_econnreset_reports_the_reset() {
    let mut conn = connection();
    assert_eq!(
        conn.port
            .control(&op(INET_REQ_SETOPTS), &show_econnreset_record(true)),
        ctl_ok(),
        "`{{show_econnreset,true}}` is accepted — it only changes how a reset is reported"
    );
    assert_eq!(
        render_result(&shutdown(&mut conn.port, TCP_SHUT_WR)),
        "ok",
        "the write shutdown itself succeeds"
    );
    assert_eq!(
        render_result(&send(&mut conn.port, b"dd")),
        "{error,econnreset}",
        "the direct success arm latched nothing, so the `EPIPE` is rewritten to `ECONNRESET`"
    );
    assert_eq!(
        render_result(&send(&mut conn.port, b"ee")),
        "{error,enotconn}",
        "the reset already ended the descriptor and spent the send one-shot"
    );
    assert_eq!(
        render_result(&conn.port.control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 0))),
        "{error,econnreset}",
        "`TCP_ADDF_DELAYED_ECONNRESET` (`:13683`) arms the receive one-shot with the reset"
    );
    assert_eq!(
        status(&mut conn.port),
        [INET_REP_OK, 0, 0, 0, INET_STATE_CLOSED as u8],
        "and `tcp_desc_close` left `INET_STATE_CLOSED`"
    );
}

/// **The `read_write` twin of the row above.** Both directions are gone at once, so the send is the
/// reset and the receive that follows it is too; `enotconn` appears only once the send one-shot is
/// spent, i.e. on the *second* send.
///
/// Oracle (a real loopback pair): `ok {error,econnreset} {error,econnreset} {error,enotconn}
/// [1,0,0,0,0]`.
#[test]
fn a_read_write_shutdown_under_show_econnreset_reports_the_reset() {
    let mut conn = connection();
    assert_eq!(
        conn.port
            .control(&op(INET_REQ_SETOPTS), &show_econnreset_record(true)),
        ctl_ok(),
        "`{{show_econnreset,true}}` is accepted"
    );
    assert_eq!(
        render_result(&shutdown(&mut conn.port, TCP_SHUT_RDWR)),
        "ok",
        "`SHUT_RDWR` on a connected socket is `[1]`"
    );
    assert_eq!(
        render_result(&send(&mut conn.port, b"gg")),
        "{error,econnreset}",
        "the write half's `EPIPE` is rewritten to the reset, exactly as the `write` row's is"
    );
    assert_eq!(
        render_result(&conn.port.control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 0))),
        "{error,econnreset}",
        "the receive one-shot carries the same reset"
    );
    assert_eq!(
        render_result(&send(&mut conn.port, b"hh")),
        "{error,enotconn}",
        "with both one-shots spent, the next write is the bare `enotconn`"
    );
    assert_eq!(
        status(&mut conn.port),
        [INET_REP_OK, 0, 0, 0, INET_STATE_CLOSED as u8],
    );
}

/// **The `TCP_ADDF_SHUTDOWN_WR_DONE` latch's real observable: a *deferred* write shutdown answers
/// `closed`, not `econnreset`, even under `{show_econnreset,true}`.**
///
/// A shutdown issued while the port's output queue is non-empty is not applied at once: the C stores
/// `TCP_ADDF_PENDING_SHUT_WR` and replies `ok` (`inet_drv.c:12356`-`:12368`), then
/// `tcp_shutdown_async` runs the real `shutdown(2)` once the queue drains and latches the flag
/// (`:13973`). So the later write's `EPIPE` keeps its own value — `tcp_send_error`'s rewrite
/// (`:13717`) is skipped — and `tcp_send_or_shutdown_error`'s `err == ECONNRESET && …` test
/// (`:13615`) fails, leaving the `am_closed` arm (`:13667`).
///
/// This is the direct contrast with
/// [`a_write_shutdown_under_show_econnreset_reports_the_reset`]: same option, same call, opposite
/// reply, decided only by whether the shutdown was deferred.
///
/// Oracle (a real loopback pair, `{sndbuf,4096}` and a 2 MiB send the peer had not drained):
/// `ok ok {error,closed} {error,enotconn} {error,closed} [1,0,0,0,0]` (shutdown, the stalled send,
/// send1, send2, recv, state).
#[test]
fn a_deferred_write_shutdown_keeps_the_epipe_so_the_reply_is_closed() {
    // 16 MiB can never fit in the kernel's send buffer plus the peer's receive window while nobody
    // reads, so the first `send(2)` is guaranteed to leave a strict remainder queued — the peer's
    // reader is started only *after* the stall, so the queue cannot drain under the assertion.
    const PAYLOAD: usize = 16 * 1024 * 1024;

    let (mut port, peer) = linked();
    assert_eq!(
        port.control(&op(INET_REQ_SETOPTS), &sndbuf_record(4096)),
        ctl_ok(),
        "`{{sndbuf,4096}}` narrows the kernel's send buffer"
    );
    assert_eq!(
        port.control(&op(INET_REQ_SETOPTS), &show_econnreset_record(true)),
        ctl_ok(),
        "`{{show_econnreset,true}}` is in force for the whole row"
    );

    let payload = vec![b'a'; PAYLOAD];
    assert_eq!(
        render_result(&send(&mut port, &payload)),
        "ok",
        "the named send answers `[1]` even though its remainder is still queued"
    );
    let queued = output_len(&mut port);
    assert!(
        queued > 0,
        "the kernel could not take the whole payload; a strict remainder must be queued"
    );
    assert!(
        queued < PAYLOAD,
        "a strict remainder of the payload, not the whole of it"
    );

    // The shutdown is **deferred**: the queue is not empty, so nothing is applied yet.
    assert_eq!(
        render_result(&shutdown(&mut port, TCP_SHUT_WR)),
        "ok",
        "a write shutdown with output queued replies `[1]` and is applied later"
    );
    assert!(
        output_len(&mut port) > 0,
        "the deferred shutdown consumed nothing"
    );

    // Now let the peer drain, so the reactor can re-drive the write to completion.
    let mut reader = peer.try_clone().expect("a second descriptor");
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let mut got = vec![0u8; PAYLOAD];
        let read = reader.read_exact(&mut got);
        let _ = tx.send(read);
    });

    // Pump the reactor until the queue drains — the point `write_pending` applies the deferred
    // `shutdown(2)` and latches `TCP_ADDF_SHUTDOWN_WR_DONE`.
    let started = Instant::now();
    while output_len(&mut port) > 0 {
        poll_round(&mut port);
        assert!(
            started.elapsed() < HANG_GUARD,
            "the deferred write never drained ({} byte(s) still queued)",
            output_len(&mut port)
        );
    }
    let read = rx.recv_timeout(HANG_GUARD).expect("the peer read it all");
    read.expect("the peer really received every byte");
    handle.join().expect("the reader thread");

    assert_eq!(
        render_result(&send(&mut port, b"ii")),
        "{error,closed}",
        "the deferred drain latched `TCP_ADDF_SHUTDOWN_WR_DONE`, so the `EPIPE` is NOT rewritten \
         and `{{show_econnreset,true}}` cannot turn this into `econnreset`"
    );
    assert_eq!(
        render_result(&send(&mut port, b"jj")),
        "{error,enotconn}",
        "the first failure still ended the descriptor"
    );
    assert_eq!(
        render_result(&port.control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 0))),
        "{error,closed}",
        "the write observation was `closed`, so the receive one-shot carries `closed` too"
    );
    assert_eq!(
        status(&mut port),
        [INET_REP_OK, 0, 0, 0, INET_STATE_CLOSED as u8],
    );
}

// ---------------------------------------------------------------------------
// The oracle cross-check
// ---------------------------------------------------------------------------

/// The `gen_tcp`/`prim_inet` probe the compared rows come from: five connections driven through the
/// **Erlang** API (`gen_tcp:shutdown/2`, `gen_tcp:send/2`, `gen_tcp:recv/2,3`) with the raw
/// `INET_REQ_GETSTATUS` read-back, printed as `~w` per row.
///
/// Every leg builds its own loopback pair, so no row can be perturbed by an earlier one.
const ORACLE_PROBE: &str = r#"#!/usr/bin/env escript
main(_) ->
    J = fun(R) -> io:format("~w|", [R]) end,
    M = 16#03f1a300,
    ST = 16 + M,
    Rcv = fun(P,N,T) -> case gen_tcp:recv(P,N,T) of
                            {ok,Da} -> {ok,binary_to_list(Da)};
                            Ea -> Ea
                        end
          end,
    Pair = fun() ->
                   {ok,L} = gen_tcp:listen(0,[binary,{active,false},{reuseaddr,true},
                                               {ip,{127,0,0,1}}]),
                   {ok,{_,P0}} = inet:sockname(L),
                   {ok,A} = gen_tcp:connect({127,0,0,1},P0,[binary,{active,false}],5000),
                   {ok,B} = gen_tcp:accept(L,5000),
                   gen_tcp:close(L),
                   {A,B}
           end,
    {A1,B1} = Pair(),
    J(erlang:port_control(A1,ST,<<>>)),
    J(gen_tcp:shutdown(A1,read)),
    J(gen_tcp:shutdown(A1,read)),
    J(gen_tcp:send(A1,<<"aa">>)),
    J(Rcv(B1,2,2000)),
    J(erlang:port_control(A1,ST,<<>>)),
    {A2,B2} = Pair(),
    J(gen_tcp:shutdown(A2,write)),
    J(gen_tcp:shutdown(A2,write)),
    J(gen_tcp:send(A2,<<"bb">>)),
    J(gen_tcp:send(A2,<<"cc">>)),
    J(erlang:port_control(A2,ST,<<>>)),
    {A3,B3} = Pair(),
    J(gen_tcp:shutdown(A3,read_write)),
    J(gen_tcp:shutdown(A3,read_write)),
    J(gen_tcp:send(A3,<<"ff">>)),
    J(gen_tcp:recv(A3,0,1000)),
    J(erlang:port_control(A3,ST,<<>>)),
    {A4,B4} = Pair(),
    J(gen_tcp:shutdown(A4,write)),
    ok = gen_tcp:send(B4,<<"hello">>),
    J(Rcv(A4,5,2000)),
    J(erlang:port_control(A4,ST,<<>>)),
    {ok,L5} = gen_tcp:listen(0,[binary,{active,false},{ip,{127,0,0,1}}]),
    J(gen_tcp:shutdown(L5,read)),
    J(gen_tcp:shutdown(L5,write)),
    J(gen_tcp:shutdown(L5,read_write)),
    J(erlang:port_control(L5,ST,<<>>)),
    J(erlang:port_control(L5,44+M,<<0>>)),
    {A6,B6} = Pair(),
    ok = inet:setopts(A6,[{show_econnreset,true}]),
    J(gen_tcp:shutdown(A6,write)),
    J(gen_tcp:send(A6,<<"dd">>)),
    J(gen_tcp:send(A6,<<"ee">>)),
    J(gen_tcp:recv(A6,0,1000)),
    J(erlang:port_control(A6,ST,<<>>)),
    {A7,B7} = Pair(),
    ok = inet:setopts(A7,[{show_econnreset,true}]),
    J(gen_tcp:shutdown(A7,read_write)),
    J(gen_tcp:send(A7,<<"gg">>)),
    J(gen_tcp:recv(A7,0,1000)),
    J(gen_tcp:send(A7,<<"hh">>)),
    J(erlang:port_control(A7,ST,<<>>)),
    io:format("~n"),
    gen_tcp:close(A1), gen_tcp:close(B1), gen_tcp:close(A2), gen_tcp:close(B2),
    gen_tcp:close(A3), gen_tcp:close(B3), gen_tcp:close(A4), gen_tcp:close(B4),
    gen_tcp:close(A6), gen_tcp:close(B6), gen_tcp:close(A7), gen_tcp:close(B7),
    gen_tcp:close(L5),
    ok.
"#;

/// **The whole `gen_tcp:shutdown/2` contract, row for row against the system `erl`.**
///
/// Thirty-four rows, driven identically on both sides: the connected state, the read shutdown and
/// its repeat, the send that still works and the bytes the peer receives, the unchanged state, the
/// write shutdown and its repeat, the `closed` then `enotconn` ladder, the closed state, the
/// read_write shutdown with both later operations refused, the write shutdown whose read side still
/// delivers the peer's payload, a listener's three `enotconn` replies with its state and the
/// byte-identical raw reply for an out-of-range `how`, and the two `{show_econnreset,true}` groups —
/// a direct `write` shutdown (the later write and the receive are both the reset) and a direct
/// `read_write` one (the same, with the reset receive ahead of the second send's `enotconn`).
///
/// **Skips loudly** when `escript` is not on `PATH` or does not answer within its budget.
#[test]
fn the_shutdown_contract_agrees_with_the_erl_oracle() {
    let mut rust: Vec<String> = Vec::new();

    // ---- rows 1-6: a read shutdown leaves sending intact --------------------
    let (mut one, mut peer_one) = linked();
    rust.push(render_bytes(&status(&mut one)));
    rust.push(render_result(&shutdown(&mut one, TCP_SHUT_RD)));
    rust.push(render_result(&shutdown(&mut one, TCP_SHUT_RD)));
    rust.push(render_result(&send(&mut one, b"aa")));
    let mut received = [0u8; 2];
    peer_one.set_read_timeout(Some(READ_GUARD)).expect("a guard");
    peer_one.read_exact(&mut received).expect("the peer reads");
    rust.push(render_data(&received));
    rust.push(render_bytes(&status(&mut one)));

    // ---- rows 7-11: a write shutdown, then the send ladder -------------------
    let mut two = connection();
    rust.push(render_result(&shutdown(&mut two.port, TCP_SHUT_WR)));
    rust.push(render_result(&shutdown(&mut two.port, TCP_SHUT_WR)));
    rust.push(render_result(&send(&mut two.port, b"bb")));
    rust.push(render_result(&send(&mut two.port, b"cc")));
    rust.push(render_bytes(&status(&mut two.port)));

    // ---- rows 12-16: a read_write shutdown closes both directions -----------
    let mut three = connection();
    rust.push(render_result(&shutdown(&mut three.port, TCP_SHUT_RDWR)));
    rust.push(render_result(&shutdown(&mut three.port, TCP_SHUT_RDWR)));
    rust.push(render_result(&send(&mut three.port, b"ff")));
    rust.push(render_result(&three.port.control(
        &op(TCP_REQ_RECV),
        &recv_operand(INET_INFINITY, 0),
    )));
    rust.push(render_bytes(&status(&mut three.port)));

    // ---- rows 17-19: the read side survives a write shutdown ----------------
    // The peer's own `gen_tcp:send` is a *setup* step on the oracle side (asserted `ok` there), not
    // a row: it observes the peer, not the port under test.
    let (mut four, mut peer_four) = linked();
    rust.push(render_result(&shutdown(&mut four, TCP_SHUT_WR)));
    peer_four.write_all(b"hello").expect("the peer writes");
    peer_four.flush().expect("the peer flushes");
    rust.push(render_data(&receive(&mut four, 5)));
    rust.push(render_bytes(&status(&mut four)));

    // ---- rows 20-24: a listener answers enotconn for every how --------------
    let (mut five, _port) = listening_port();
    for how in [TCP_SHUT_RD, TCP_SHUT_WR, TCP_SHUT_RDWR] {
        rust.push(render_result(&shutdown(&mut five, how)));
    }
    rust.push(render_bytes(&status(&mut five)));
    rust.push(render_bytes(&shutdown(&mut five, TCP_SHUT_RD)));

    // ---- rows 25-29: a direct write shutdown under `{show_econnreset,true}` ----
    let mut six = connection();
    assert_eq!(
        six.port
            .control(&op(INET_REQ_SETOPTS), &show_econnreset_record(true)),
        ctl_ok()
    );
    rust.push(render_result(&shutdown(&mut six.port, TCP_SHUT_WR)));
    rust.push(render_result(&send(&mut six.port, b"dd")));
    rust.push(render_result(&send(&mut six.port, b"ee")));
    rust.push(render_result(&six.port.control(
        &op(TCP_REQ_RECV),
        &recv_operand(INET_INFINITY, 0),
    )));
    rust.push(render_bytes(&status(&mut six.port)));

    // ---- rows 30-34: a direct read_write shutdown under the same option ------
    let mut seven = connection();
    assert_eq!(
        seven
            .port
            .control(&op(INET_REQ_SETOPTS), &show_econnreset_record(true)),
        ctl_ok()
    );
    rust.push(render_result(&shutdown(&mut seven.port, TCP_SHUT_RDWR)));
    rust.push(render_result(&send(&mut seven.port, b"gg")));
    rust.push(render_result(&seven.port.control(
        &op(TCP_REQ_RECV),
        &recv_operand(INET_INFINITY, 0),
    )));
    rust.push(render_result(&send(&mut seven.port, b"hh")));
    rust.push(render_bytes(&status(&mut seven.port)));

    // The rows the C's arms and the OTP 29.1.1 measurement give (the `~w` spellings the probe
    // prints).
    let expected: Vec<String> = vec![
        "[1,0,0,0,5]".to_owned(),              // connected
        "ok".to_owned(),                       // shutdown(read)
        "ok".to_owned(),                       // …again
        "ok".to_owned(),                       // the send still works
        "{ok,[97,97]}".to_owned(),             // and the peer receives it
        "[1,0,0,0,5]".to_owned(),              // a read shutdown changes no state
        "ok".to_owned(),                       // shutdown(write)
        "ok".to_owned(),                       // …again, NOT enotconn
        "{error,closed}".to_owned(),           // the first later write
        "{error,enotconn}".to_owned(),         // every write after the teardown
        "[1,0,0,0,0]".to_owned(),              // …which closed the descriptor
        "ok".to_owned(),                       // shutdown(read_write)
        "ok".to_owned(),                       // …again
        "{error,closed}".to_owned(),           // the send
        "{error,closed}".to_owned(),           // the receive
        "[1,0,0,0,0]".to_owned(),              // closed
        "ok".to_owned(),                       // shutdown(write)
        "{ok,[104,101,108,108,111]}".to_owned(), // the read side still works
        "[1,0,0,0,5]".to_owned(),              // …and the descriptor is still connected
        "{error,enotconn}".to_owned(),         // listener, read
        "{error,enotconn}".to_owned(),         // listener, write
        "{error,enotconn}".to_owned(),         // listener, read_write
        "[1,0,0,0,9]".to_owned(),              // still listening
        "[0,101,110,111,116,99,111,110,110]".to_owned(), // `[0]"enotconn"`, the raw reply
        "ok".to_owned(),                       // shutdown(write) under `{show_econnreset,true}`
        "{error,econnreset}".to_owned(),       // the direct arm latched nothing ⇒ the EPIPE is a reset
        "{error,enotconn}".to_owned(),         // the reset ended the descriptor, spending the one-shot
        "{error,econnreset}".to_owned(),       // the receive one-shot the reset armed
        "[1,0,0,0,0]".to_owned(),              // closed
        "ok".to_owned(),                       // shutdown(read_write), same option
        "{error,econnreset}".to_owned(),       // the send
        "{error,econnreset}".to_owned(),       // the receive
        "{error,enotconn}".to_owned(),         // …and the second send, both one-shots spent
        "[1,0,0,0,0]".to_owned(),              // closed
    ];
    // Both sides push the same 34 rows in the same order, so this is a straight comparison: the
    // listener group's last row is the *raw* `[0] ++ Name` reply (`[0,101,110,…]`) and the three
    // before it are the same reply rendered the way `prim_inet` spells it (`{error,enotconn}`).
    assert_eq!(rust, expected, "this floor's `gen_tcp:shutdown/2` rows");
    assert_eq!(
        rust.len(),
        34,
        "the compared table is the 34 rows the probe prints"
    );

    let Some(dump) = escript_stdout("shutdown_contract", ORACLE_PROBE, &[]) else {
        eprintln!("SKIP: no `escript` on PATH — skipping the shutdown-contract oracle cross-check");
        return;
    };
    let oracle: Vec<String> = dump
        .split('|')
        .map(str::trim)
        .filter(|row| !row.is_empty())
        .map(str::to_owned)
        .collect();
    assert_eq!(
        oracle.len(),
        expected.len(),
        "the oracle answered every row: {dump:?}"
    );
    for (index, (oracle_row, expected_row)) in oracle.iter().zip(&expected).enumerate() {
        assert_eq!(
            oracle_row, expected_row,
            "shutdown-contract row #{index} disagrees (dump: {dump:?})"
        );
    }
}

/// The option record that turns `{show_econnreset, …}` on — the five-byte `[code, value_be32]`
/// shape the C's `inet_set_opts` walk expects (`inet_drv.c:6908`).
fn show_econnreset_record(on: bool) -> [u8; 5] {
    let [a, b, c, d] = (on as i32).to_be_bytes();
    [SHOW_ECONNRESET, a, b, c, d]
}

/// The `{sndbuf, Bytes}` record — the same five-byte shape on the `SO_SNDBUF` code
/// ([`INET_OPT_SNDBUF`], `inet_drv.c:880`).
fn sndbuf_record(bytes: u32) -> [u8; 5] {
    let [a, b, c, d] = bytes.to_be_bytes();
    [INET_OPT_SNDBUF, a, b, c, d]
}

/// The suite's own smoke assertion that the option record is the five-byte shape the C's
/// `inet_set_opts` walk expects (`inet_drv.c:6908`).
#[test]
fn the_show_econnreset_record_is_a_five_byte_option_record() {
    assert_eq!(show_econnreset_record(true), [39, 0, 0, 0, 1]);
    assert_eq!(show_econnreset_record(false), [39, 0, 0, 0, 0]);
    let mut conn = connection();
    assert_eq!(
        conn.port
            .control(&op(INET_REQ_SETOPTS), &show_econnreset_record(true)),
        ctl_ok(),
        "`INET_REQ_SETOPTS` accepts the record"
    );
    assert_eq!(
        conn.port
            .control(&op(INET_REQ_SETOPTS), &show_econnreset_record(false)),
        ctl_ok(),
    );
}
