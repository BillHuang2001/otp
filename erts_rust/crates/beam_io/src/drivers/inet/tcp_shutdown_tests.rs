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
//! The `TCP_REQ_SHUTDOWN`(44) guard ladder and its bad-operand reply.
//!
//! `tcp_inet_ctl`'s shutdown arm (`inet_drv.c:12343`-`:12370`) tests the **connection** first
//! (`!IS_CONNECTED` ⇒ `ctl_error(ENOTCONN)`) and only then the operand length
//! (`len != 1` ⇒ `ctl_error(EINVAL)`), so both faults are `[0]`-tagged **replies** — never a
//! `badarg` raise — and the order decides which one an unconnected port with a bad operand sees.
//! The rows here are cross-checked byte-for-byte against the system `erl`.
//!
//! The arm in full, as the C spells it (`:12349`-`:12370`):
//!
//! ```c
//! if (!IS_CONNECTED(INETP(desc)))      return ctl_error(ENOTCONN, rbuf, rsize);
//! if (len != 1)                        return ctl_error(EINVAL, rbuf, rsize);
//! how = buf[0];
//! if (how != TCP_SHUT_RD && driver_sizeq(desc->inet.port) > 0) {
//!     if (how == TCP_SHUT_WR)          desc->tcp_add_flags |= TCP_ADDF_PENDING_SHUT_WR;
//!     else if (how == TCP_SHUT_RDWR)   desc->tcp_add_flags |= TCP_ADDF_PENDING_SHUT_RDWR;
//!     return ctl_reply_ok(rbuf, rsize);          /* deferred */
//! }
//! if (IS_SOCKET_ERROR(sock_shutdown(INETP(desc)->s, how))) {
//!     if (how != TCP_SHUT_RD)          desc->tcp_add_flags |= TCP_ADDF_SHUTDOWN_WR_DONE;
//!     return ctl_error(sock_errno(), rbuf, rsize);
//! } else {
//!     return ctl_reply_ok(rbuf, rsize);
//! }
//! ```
//!
//! Three consequences are pinned below: the guard **order** (so an unconnected port answers
//! `[0]enotconn` for *any* operand, however malformed), the `[0]einval` **reply** for a
//! wrong-length operand on a **connected** one (never a raise), and the deferral gate — only the
//! two write values defer, so a `SHUT_RD` is applied at once and an unknown `how` byte falls
//! through to `shutdown(2)`, whose `EINVAL` is answered as `[0]einval`.
//!
//! Rows measured on OTP 29.1.1 / erts-17.1 against a raw `{spawn_driver,"tcp_inet"}` port (the
//! control values include the `0x03f1a300` magic number):
//!
//! | state | operand | reply |
//! |---|---|---|
//! | socket-less | `<<>>` / `<<1,2>>` / `<<1>>` | `[0]"enotconn"` |
//! | open, no peer | `<<>>` / `<<1,2>>` / `<<1>>` | `[0]"enotconn"` |
//! | connected | `<<>>` / `<<1,2>>` | `[0]"einval"` |
//! | connected | `<<3>>` (unknown `how`) | `[0]"einval"` (from `shutdown(2)`) |
//! | connected | `<<1>>` / `<<2>>` / `<<0>>` | `[1]` |
use super::opcode_tests::{
    bare_port, connection, erl_stdout, op, opened_port, oracle_replies, Rig, CONTROL_MAGIC,
};
use super::wire::{ctl_ok, ctl_xerror};
use super::{TcpInetDriver, TCP_SHUT_RD, TCP_SHUT_RDWR, TCP_SHUT_WR};
use crate::port::PortCommandOptions;
use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::IntoRawFd;
use std::time::Duration;

/// `TCP_REQ_SHUTDOWN` (`inet_drv.c:1107`).
const SHUTDOWN: u32 = 44;
/// `INET_REQ_OPEN` (`inet_drv.c:578`) — the request that gives a socket-less port a socket.
const OPEN: u32 = 1;
/// `INET_REQ_CONNECT` (`inet_drv.c:601`) — `Timeout(4) ++ address`, driven at infinity below.
const CONNECT: u32 = 3;
/// `INET_REQ_GETSTATUS` (`inet_drv.c:612`).
const GETSTATUS: u32 = 16;
/// `INET_STATE_CONNECTED` (`inet_drv.c:668`) — the code `GETSTATUS` reports for a live connection,
/// so the matrix can prove both sides were really connected when the shutdown rows ran.
const CONNECTED: u32 = 5;
/// `INET_INFINITY` (`-1` as the C's `unsigned timeout`): a `0` connect budget expires before the
/// handshake completes (measured: `{error,timeout}`), and this suite has no clock to race.
const INFINITY_TIMEOUT: [u8; 4] = [0xff, 0xff, 0xff, 0xff];

/// How long the peer read below may take before the test fails instead of hanging (the read
/// returns at the deferred `shutdown(2)`'s EOF, so only a broken driver reaches this).
const READ_GUARD: Duration = Duration::from_secs(30);

/// One `TCP_REQ_SHUTDOWN` call, asserting it was a **reply**: the C's `ctl_error` for both faults
/// this suite is about, never an argument raise.
fn shutdown_reply(rig: &mut Rig, arg: &[u8]) -> Vec<u8> {
    match rig.try_control(&op(SHUTDOWN), arg) {
        Ok(reply) => reply,
        Err(err) => panic!("`TCP_REQ_SHUTDOWN` {arg:?} raised {err:?} instead of answering a reply"),
    }
}

/// A connected `tcp_inet` port **plus the peer stream**, which
/// [`connection`](super::opcode_tests::connection) keeps to itself:
/// [`a_write_shutdown_defers_until_the_output_queue_drains`] has to read what the peer actually
/// receives, because the C's `TCP_ADDF_PENDING_SHUT_WR` rule is only observable as *the queued
/// payload arriving before the end-of-file*.
fn connected_with_peer() -> (Rig, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let peer = TcpStream::connect(listener.local_addr().expect("an address"))
        .expect("the far end connects");
    let (server, _accepted) = listener.accept().expect("the port's end is accepted");
    let fd = server
        .try_clone()
        .expect("a second descriptor for the same socket")
        .into_raw_fd();
    let driver = TcpInetDriver::from_socket(fd).expect("the driver adopts the socket");
    assert!(driver.state().is_connected(), "the adopted socket has a peer");
    peer.set_read_timeout(Some(READ_GUARD))
        .expect("the peer takes a read guard");
    (Rig::new(Box::new(driver)), peer)
}

/// A well-formed `erlang:port_command/2` payload of the C's `init_caller` shape
/// (`inet_drv.c:1820`-`:1890`): `[tag_len:be16] ++ tag ++ payload`, here with a **zero-length**
/// tag — which the C consumes, leaves the payload to be written and answers nothing for.
fn queued_command(payload: &[u8]) -> Vec<u8> {
    let mut command = vec![0u8, 0u8];
    command.extend_from_slice(payload);
    command
}

/// Queues `bytes` as a `port_command/2` **without running a port-task round**, so the port's
/// output queue is still non-empty when the next control call runs — the state the C's
/// `driver_sizeq(desc->inet.port) > 0` deferral test reads.
fn queue_only(rig: &mut Rig, bytes: &[u8]) {
    rig.registry
        .port_command(
            &mut rig.poller,
            rig.id,
            bytes,
            PortCommandOptions::EMPTY,
        )
        .expect("the port accepts the command");
    assert!(
        rig.registry
            .get(rig.id)
            .expect("a live port")
            .output_len()
            > 0,
        "the command is queued and not yet written"
    );
}

// -----------------------------------------------------------------------
// Row 1 — the connection guard precedes the operand guard
// -----------------------------------------------------------------------

/// **`shutdown` on a port with no connection answers `[0]enotconn` for every operand** — the C's
/// `!IS_CONNECTED` guard is the arm's **first** statement (`inet_drv.c:12349`), so it wins over the
/// `len != 1` check (`:12352`) and over the `how` byte's value. A wrong-length operand therefore
/// proves the order: were the length checked first, an empty operand would answer `[0]einval`.
///
/// Both states before a connection are covered: a **socket-less** port (the state a bare
/// `open_port({spawn_driver,"tcp_inet"}, …)` starts in, and what `INET_REQ_SHUTDOWN` reaches first)
/// and an **`open`ed but peerless** socket (`INET_STATE_OPEN`).
#[test]
fn a_shutdown_without_a_connection_is_enotconn_whatever_the_operand() {
    let mut bare = bare_port();
    for arg in [vec![], vec![1, 2], vec![TCP_SHUT_WR]] {
        assert_eq!(
            shutdown_reply(&mut bare, &arg),
            ctl_xerror("enotconn"),
            "the C's `!IS_CONNECTED` guard precedes `len != 1`: a socket-less port with {arg:?}"
        );
    }

    let mut opened = opened_port();
    for arg in [vec![], vec![1, 2], vec![TCP_SHUT_WR]] {
        assert_eq!(
            shutdown_reply(&mut opened, &arg),
            ctl_xerror("enotconn"),
            "an opened but peerless socket is still `!IS_CONNECTED`: operand {arg:?}"
        );
    }
}

// -----------------------------------------------------------------------
// Rows 2 and 3 — the operand guards on a connected socket
// -----------------------------------------------------------------------

/// **A wrong-length `shutdown` operand on a connected port is the `[0]einval` reply**, the C's
/// `ctl_error(EINVAL)` (`inet_drv.c:12352`) — never an argument raise. An empty operand and a
/// two-byte one both take that arm; the connection guard above them is already satisfied.
#[test]
fn a_wrong_length_shutdown_on_a_connected_port_is_the_einval_reply() {
    let mut conn = connection();
    assert_eq!(
        conn.port.control(&op(GETSTATUS), &[]),
        {
            let mut reply = ctl_ok();
            reply.extend_from_slice(&CONNECTED.to_be_bytes());
            reply
        },
        "the guard `enotconn` would have answered is satisfied: the socket is connected"
    );
    for arg in [vec![], vec![1, 2]] {
        assert_eq!(
            shutdown_reply(&mut conn.port, &arg),
            ctl_xerror("einval"),
            "`if (len != 1) return ctl_error(EINVAL, rbuf, rsize)`: operand {arg:?}"
        );
    }
    assert_eq!(
        conn.port.control(&op(GETSTATUS), &[])[4],
        CONNECTED as u8,
        "a refused operand leaves the connection alone"
    );
}

/// **A valid one-byte `how` on a connected port answers `[1]`** for all three directions
/// (`shutdown(2)` succeeds on an established socket), and a byte the kernel does not know falls
/// through to that same `shutdown(2)`, whose `EINVAL` the C answers as `[0]einval`
/// (`:12366`-`:12370`) — a reply, and no deferral (see the deferral gate below).
#[test]
fn a_valid_shutdown_operand_on_a_connected_port_is_ok() {
    let mut conn = connection();
    for how in [TCP_SHUT_RD, TCP_SHUT_WR, TCP_SHUT_RDWR] {
        assert_eq!(
            shutdown_reply(&mut conn.port, &[how]),
            ctl_ok(),
            "`how` = {how} on a connected socket with an empty output queue"
        );
    }
    assert_eq!(
        shutdown_reply(&mut conn.port, &[3]),
        ctl_xerror("einval"),
        "`shutdown(2)`'s `EINVAL` for an unknown `how` byte, via `ctl_error(sock_errno())`"
    );
}

// -----------------------------------------------------------------------
// Row 4 — the deferral gate (`TCP_ADDF_PENDING_SHUT_*`)
// -----------------------------------------------------------------------

/// **A write-side `shutdown` with output still queued is deferred**, and the deferral is visible
/// exactly where the C makes it: the queued bytes are still queued (nothing is dropped), and once
/// the port-task round writes them the `shutdown(2)` runs — so the peer reads the **whole
/// payload and then** its end-of-file. Had the arm applied the shutdown at once, the socket's
/// write side would already be gone when the round ran, the payload would never arrive and the
/// peer would see the end-of-file alone.
///
/// The C's condition (`inet_drv.c:12356`-`:12363`):
/// `how != TCP_SHUT_RD && driver_sizeq(desc->inet.port) > 0`, with `PENDING_SHUT_WR`/`_RDWR`
/// latched for `TCP_SHUT_WR`/`TCP_SHUT_RDWR` only.
#[test]
fn a_write_shutdown_defers_until_the_output_queue_drains() {
    let (mut rig, mut peer) = connected_with_peer();
    queue_only(&mut rig, &queued_command(b"deferred-payload"));

    assert_eq!(
        shutdown_reply(&mut rig, &[TCP_SHUT_WR]),
        ctl_ok(),
        "the C's deferred arm answers `ctl_reply_ok` (`:12362`), not a refusal"
    );
    assert!(
        rig.registry
            .get(rig.id)
            .expect("a live port")
            .output_len()
            > 0,
        "a deferred shutdown drops nothing: the command is still queued for the write path"
    );

    rig.round().expect("the round writes the queued command");

    let mut received = Vec::new();
    peer.read_to_end(&mut received)
        .expect("the peer reads to the deferred shutdown's EOF");
    assert_eq!(
        received, b"deferred-payload",
        "the queued bytes reach the peer, and only then does the deferred `shutdown(2)` close the \
         write side"
    );
}

/// **A read-side `shutdown` is never deferred**: the C's gate opens with `how != TCP_SHUT_RD`
/// (`inet_drv.c:12356`), so a `SHUT_RD` runs `shutdown(2)` at once even with output queued and
/// latches **no** pending write shutdown.
///
/// That is observable two ways: the reply is the immediate arm's `[1]`, and — the discriminating
/// one — a following `send` is **not** the `TCP_ADDF_PENDING_SHUT_WR` refusal: the C routes a send
/// behind a pending write shutdown to `tcp_shutdown_error(desc, EPIPE)` and answers `{error,
/// closed}` (`:12695`, `:13654`-`:13670`), while here it writes normally and answers `[1]`.
#[test]
fn a_read_shutdown_is_never_deferred() {
    let mut rig = connection();
    queue_only(&mut rig.port, &queued_command(b"x"));

    assert_eq!(
        shutdown_reply(&mut rig.port, &[TCP_SHUT_RD]),
        ctl_ok(),
        "`how == TCP_SHUT_RD` skips the deferral and `shutdown(2)` succeeds"
    );
    assert_eq!(
        rig.port.control("send", b"y"),
        ctl_ok(),
        "no `TCP_ADDF_PENDING_SHUT_WR` was latched, so the send is not the EPIPE refusal"
    );
}

/// **An unknown `how` byte defers nothing either**: the C's latch is gated on `how` being
/// `TCP_SHUT_WR` or `TCP_SHUT_RDWR` (`inet_drv.c:12357`-`:12361`), so any other byte with output
/// still queued falls through to `shutdown(2)`, which answers `EINVAL` — the `[0]einval` reply,
/// **not** the deferred `[1]`.
#[test]
fn an_unknown_how_defers_nothing_and_takes_the_shutdown_errno() {
    let mut rig = connection();
    queue_only(&mut rig.port, &queued_command(b"x"));

    assert_eq!(
        shutdown_reply(&mut rig.port, &[3]),
        ctl_xerror("einval"),
        "a non-write `how` never takes the deferred `ctl_reply_ok` arm"
    );
}

// -----------------------------------------------------------------------
// The oracle cross-check
// -----------------------------------------------------------------------

/// The whole guard ladder against the real `tcp_inet` driver port, byte for byte: the same three
/// port states (socket-less, opened, connected) and the same operands as the rows above, in one
/// `erl` run, compared against the reply list those rows already pin.
///
/// The oracle needs a **real** connection, and its raw `INET_REQ_CONNECT`(3) is asynchronous with
/// an `INET_INFINITY` budget (`0xffffffff`; a `0` budget answers `{error,timeout}` before the
/// handshake completes), so the probe listens, connects, accepts it and reads `GETSTATUS` back —
/// that status row is part of the compared list, so a probe that silently ran against an
/// *unconnected* port cannot pass.
///
/// Two reply-independent details are what make that connection real:
///
/// * the connect operand's address is `Tag(1) ++ Port(2)` — the C parses it with
///   `inet_set_faddress` (`inet_drv.c:4795`), whose first byte is an `INET_AF_*` tag
///   (`INET_AF_LOOPBACK` = `4`, `:760`), **not** an address octet; a tag-less port-then-address
///   payload is refused (`[0]"eafnosupport"`) and the `accept` then blocks forever;
/// * the connect completes asynchronously (`tcp_inet_ready_output` sets `INET_STATE_CONNECTED`
///   before `async_ok`, `inet_drv.c:14284`/`:14287`), so the probe polls `GETSTATUS` until it
///   reports [`CONNECTED`] instead of sleeping on it, bounded at 2000 polls (≈2 s) and loud when it
///   runs out — an abnormal `-eval` exit loses the whole output (`opcode_tests::erl_stdout`'s
///   note), so the give-up arm prints to `standard_error` and `halt(1)`.
///
/// **Skips loudly** when `erl` is not on `PATH` or does not answer within its budget.
#[test]
fn the_shutdown_replies_agree_with_the_erl_oracle() {
    let shutdown = CONTROL_MAGIC + u64::from(SHUTDOWN);
    let open = CONTROL_MAGIC + u64::from(OPEN);
    let connect = CONTROL_MAGIC + u64::from(CONNECT);
    let status = CONTROL_MAGIC + u64::from(GETSTATUS);
    let expr = format!(
        "J = fun(R) -> io:format(\"~w|\", [R]) end, \
         B = open_port({{spawn_driver,\"tcp_inet\"}},[binary]), \
         J(erlang:port_control(B,{shutdown},<<>>)), \
         J(erlang:port_control(B,{shutdown},<<1,2>>)), \
         J(erlang:port_control(B,{shutdown},<<1>>)), \
         O = open_port({{spawn_driver,\"tcp_inet\"}},[binary]), \
         [1] = erlang:port_control(O,{open},<<1,1,0>>), \
         J(erlang:port_control(O,{shutdown},<<>>)), \
         J(erlang:port_control(O,{shutdown},<<1,2>>)), \
         J(erlang:port_control(O,{shutdown},<<1>>)), \
         {{ok,L}} = gen_tcp:listen(0,[binary,{{active,false}},{{reuseaddr,true}},\
         {{ip,{{127,0,0,1}}}}]), \
         {{ok,{{_,P0}}}} = inet:sockname(L), \
         C = open_port({{spawn_driver,\"tcp_inet\"}},[binary]), \
         [1] = erlang:port_control(C,{open},<<1,1,0>>), \
         erlang:port_control(C,{connect},<<{INFINITY},4,(P0 bsr 8),(P0 band 255)>>), \
         Wait = fun F(0) -> io:format(standard_error, \"the raw connect never completed~n\"), \
         erlang:halt(1); \
         F(N) -> case erlang:port_control(C,{status},<<>>) of \
                     [1,0,0,0,{CONNECTED}] -> ok; \
                     _ -> timer:sleep(1), F(N-1) \
                 end \
         end, \
         Wait(2000), \
         {{ok,_A}} = gen_tcp:accept(L, 5000), \
         J(erlang:port_control(C,{status},<<>>)), \
         J(erlang:port_control(C,{shutdown},<<>>)), \
         J(erlang:port_control(C,{shutdown},<<1,2>>)), \
         J(erlang:port_control(C,{shutdown},<<3>>)), \
         J(erlang:port_control(C,{shutdown},<<0>>)), \
         J(erlang:port_control(C,{shutdown},<<1>>)), \
         J(erlang:port_control(C,{shutdown},<<2>>)), \
         gen_tcp:close(L)",
        INFINITY = INFINITY_TIMEOUT
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(",")
    );

    // The reply every row must produce, derived from the C arm quoted at the top of this file and
    // measured on OTP 29.1.1. `open`/`connect` are preconditions rather than compared rows: the
    // oracle's raw `INET_REQ_CONNECT` is asynchronous and answers `[1] ++ <<Id:16>>` where this
    // floor's named `connect` completes in place, so the `GETSTATUS` row is what proves both sides
    // were really connected when the shutdown rows ran.
    let mut connected_status = ctl_ok();
    connected_status.extend_from_slice(&CONNECTED.to_be_bytes());
    let expected = vec![
        ctl_xerror("enotconn"), // bare port, `<<>>`
        ctl_xerror("enotconn"), // bare port, `<<1,2>>` — the connection guard wins
        ctl_xerror("enotconn"), // bare port, `<<1>>`
        ctl_xerror("enotconn"), // opened, `<<>>`
        ctl_xerror("enotconn"), // opened, `<<1,2>>`
        ctl_xerror("enotconn"), // opened, `<<1>>`
        connected_status,       // GETSTATUS ⇒ `[1,0,0,0,5]`: really connected
        ctl_xerror("einval"),   // connected, `<<>>`
        ctl_xerror("einval"),   // connected, `<<1,2>>`
        ctl_xerror("einval"),   // connected, `<<3>>` (unknown how → `shutdown(2)` EINVAL)
        ctl_ok(),               // connected, `<<TCP_SHUT_RD>>`
        ctl_ok(),               // connected, `<<TCP_SHUT_WR>>`
        ctl_ok(),               // connected, `<<TCP_SHUT_RDWR>>`
    ];

    // The same rows on this crate's ports.
    let mut replies = Vec::new();
    let mut bare = bare_port();
    for arg in [vec![], vec![1, 2], vec![TCP_SHUT_WR]] {
        replies.push(shutdown_reply(&mut bare, &arg));
    }
    let mut opened = opened_port();
    for arg in [vec![], vec![1, 2], vec![TCP_SHUT_WR]] {
        replies.push(shutdown_reply(&mut opened, &arg));
    }
    let mut conn = connection();
    replies.push(conn.port.control(&op(GETSTATUS), &[]));
    for arg in [
        vec![],
        vec![1, 2],
        vec![3],
        vec![TCP_SHUT_RD],
        vec![TCP_SHUT_WR],
        vec![TCP_SHUT_RDWR],
    ] {
        replies.push(shutdown_reply(&mut conn.port, &arg));
    }
    assert_eq!(replies, expected, "this floor's rows");

    let Some(dump) = erl_stdout(&expr) else {
        eprintln!("SKIP: no `erl` on PATH — skipping the shutdown guard-ladder oracle cross-check");
        return;
    };
    let oracle = oracle_replies(&dump);
    assert_eq!(
        oracle.len(),
        expected.len(),
        "the oracle answered every row: {dump:?}"
    );
    for (index, (oracle_row, expected_row)) in oracle.iter().zip(&expected).enumerate() {
        assert_eq!(
            oracle_row, expected_row,
            "oracle row #{index} disagrees (dump: {dump:?})"
        );
    }
}
