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

//! The **active-mode read error** — `{tcp_error, S, Reason}` — over **real loopback sockets**.
//!
//! The C's active arm of `tcp_recv_error` (`inet_drv.c:12872`-`:12882`) runs
//! `tcp_error_message(desc, err)` and **then** `tcp_closed_message(desc)`: the error is delivered
//! strictly **before** the close, and the close itself is the `port_closed()` latch rather than a
//! second event on this channel (a datagram port has no close message at all).
//!
//! Every wait here is a **condition** under a hang guard — the peer's `RST` is awaited by pumping
//! the reactor until the delivery appears — never a wall-clock budget tuned for an idle host.

use super::opcode_tests::{loopback_arg, op, opened_port, Rig};
use super::wire::ctl_ok;
use crate::completion::InetReason;
use crate::port_task::{ActiveInput, ActiveInputEvent};
use beam_common::BeamError;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::io::AsRawFd;
use std::time::{Duration, Instant};

/// `INET_REQ_CONNECT` (`inet_drv.c:11993`).
const INET_REQ_CONNECT: u32 = 3;
/// `INET_REQ_SETOPTS` (`:11127`).
const INET_REQ_SETOPTS: u32 = 7;
/// `INET_LOPT_ACTIVE` (`:897`) with `INET_ACTIVE` (`:780`).
const ACTIVE_TRUE: [u8; 5] = [22, 0, 0, 0, 1];
/// `INET_LOPT_TCP_SHOW_ECONNRESET` (`:916`) — the option that turns a read reset into the C's
/// `tcp_recv_error` route instead of `tcp_recv_closed` (`:13185`-`:13196`).
const SHOW_ECONNRESET: [u8; 5] = [39, 0, 0, 0, 1];
/// `INET_LOPT_TCP_SHOW_ECONNRESET` with the value `0` — the default, spelled out.
const HIDE_ECONNRESET: [u8; 5] = [39, 0, 0, 0, 0];

/// How long a condition may stay unsatisfied before the case fails (a liveness bound only).
const HANG_GUARD: Duration = Duration::from_secs(20);
/// One reactor round's poll slice.
const POLL_SLICE: Duration = Duration::from_millis(20);

/// A `tcp_inet` port **connected out** to a loopback listener, armed `{active, …}` with the given
/// `{show_econnreset, …}` record, plus the far end (which the test aborts or half-closes).
///
/// The port makes the connection itself (`INET_REQ_CONNECT`), which is the same shape
/// `peer_close_tests::connected_rig` uses and the one the oracle probe drives.
fn connected(option: [u8; 5]) -> (Rig, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let address = listener.local_addr().expect("an address");
    let mut port = opened_port();
    let mut connect = vec![0u8, 0, 3, 232]; // a 1000 ms connect budget
    connect.extend_from_slice(&loopback_arg(address.port()));
    assert_eq!(
        port.control(&op(INET_REQ_CONNECT), &connect),
        ctl_ok(),
        "the port connects to the loopback listener"
    );
    let (peer, _) = listener.accept().expect("the peer side is accepted");
    let mut records = option.to_vec();
    records.extend_from_slice(&ACTIVE_TRUE);
    assert_eq!(
        port.control(&op(INET_REQ_SETOPTS), &records),
        ctl_ok(),
        "`{{show_econnreset,_}}` and `{{active,true}}` are accepted"
    );
    (port, peer)
}

/// Aborts the connection: `SO_LINGER` with a zero timeout makes `close(2)` send an **RST** instead
/// of an orderly FIN.
fn abort(peer: TcpStream) {
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
}

/// One reactor round — poll, dispatch the readiness (`handle_events` schedules *and* runs the port
/// tasks) — returning every `{active, …}` delivery it produced.
fn round(port: &mut Rig) -> Vec<ActiveInput> {
    let events = match port.poller.poll(Some(POLL_SLICE)) {
        Ok(events) => events,
        Err(BeamError::Io(err)) if err.kind() == std::io::ErrorKind::Interrupted => Vec::new(),
        Err(err) => panic!("poll failed: {err:?}"),
    };
    port.registry
        .handle_events(&mut port.poller, &events)
        .expect("the ready events are dispatched");
    port.registry.take_active_inputs()
}

/// Pumps rounds until at least `want` deliveries have been drained, under [`HANG_GUARD`].
fn until(port: &mut Rig, want: usize) -> Vec<ActiveInput> {
    let deadline = Instant::now() + HANG_GUARD;
    let mut got = Vec::new();
    while got.len() < want {
        got.extend(round(port));
        if got.len() >= want {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "only {} of {want} deliveries arrived: {got:?}",
            got.len()
        );
    }
    got
}

/// Pumps rounds for a bounded budget, asserting nothing is delivered.
fn quiet(port: &mut Rig) -> Vec<ActiveInput> {
    let deadline = Instant::now() + Duration::from_millis(200);
    let mut got = Vec::new();
    loop {
        got.extend(round(port));
        if Instant::now() >= deadline {
            return got;
        }
    }
}

/// The `Error` reasons of a drain, rendered by their atom name.
fn errors(inputs: &[ActiveInput]) -> Vec<String> {
    inputs
        .iter()
        .filter_map(|input| match &input.event {
            ActiveInputEvent::Error(reason) => Some(reason.atom_name()),
            ActiveInputEvent::Data(_) | ActiveInputEvent::Passive => None,
        })
        .collect()
}

/// `{show_econnreset,true}` + `{active,true}` + a peer that aborts: the read reset is the C's
/// `tcp_recv_error` **active** arm (`inet_drv.c:12872`-`:12882`), so exactly **one**
/// `{tcp_error, S, econnreset}` is delivered and the close that follows it is the `port_closed()`
/// latch — no second event on this channel.
///
/// A regression that dropped the delivery (or reported the reset as a plain close, which is what
/// `read_bytes` does without the option) fails on the drained list; a regression that delivered
/// the close as a second event fails on the count.
#[test]
fn an_active_read_error_delivers_the_error_before_the_close() {
    let (mut port, peer) = connected(SHOW_ECONNRESET);
    abort(peer);

    let delivered = until(&mut port, 1);
    assert_eq!(
        errors(&delivered),
        vec!["econnreset".to_string()],
        "exactly one `{{tcp_error,_econnreset}}`: {delivered:?}"
    );
    assert!(
        delivered.iter().all(|input| input.port == port.id),
        "the delivery is stamped with the port's own id"
    );
    assert!(
        matches!(
            &delivered[0].event,
            ActiveInputEvent::Error(InetReason::Errno(errno)) if *errno == libc::ECONNRESET
        ),
        "the reason is the failing errno, not a synthesised atom: {delivered:?}"
    );
    assert!(
        port.registry.port_closed(port.id).expect("a live port"),
        "the close the error precedes is the `port_closed()` latch"
    );
    assert!(
        quiet(&mut port).is_empty(),
        "the close is never a second event on the active channel"
    );
}

/// The same abort with the **default** `{show_econnreset,false}`: `tcp_recv`'s `ECONNRESET` arm
/// takes `tcp_recv_closed` (`inet_drv.c:13185`-`:13196`), which sends **no** error message — only
/// the close is observed.
#[test]
fn a_read_reset_without_show_econnreset_delivers_no_error() {
    let (mut port, peer) = connected(HIDE_ECONNRESET);
    abort(peer);

    // Wait for the close the driver latches, then assert the channel carried no error.
    let deadline = Instant::now() + HANG_GUARD;
    while !port.registry.port_closed(port.id).expect("a live port") {
        assert!(
            Instant::now() < deadline,
            "the peer's reset was never observed on the port's descriptor"
        );
        round(&mut port);
    }
    assert!(
        quiet(&mut port).is_empty(),
        "without the option the reset is a plain close and reports nothing on this channel"
    );
}

/// A **clean** peer half-close (FIN) is `tcp_recv_closed`'s `n == 0` arm (`inet_drv.c:13206`)
/// whatever the option says: no `{tcp_error, …}` at all, only the close observation.
#[test]
fn a_clean_peer_shutdown_delivers_no_error_event() {
    let (mut port, peer) = connected(SHOW_ECONNRESET);
    peer.shutdown(Shutdown::Write).expect("the peer half-closes");

    let deadline = Instant::now() + HANG_GUARD;
    while !port.registry.port_closed(port.id).expect("a live port") {
        assert!(
            Instant::now() < deadline,
            "the end-of-file was never observed on the port's descriptor"
        );
        round(&mut port);
    }
    assert!(
        quiet(&mut port).is_empty(),
        "a clean end-of-file is never a read error, even under {{show_econnreset,true}}"
    );
}
