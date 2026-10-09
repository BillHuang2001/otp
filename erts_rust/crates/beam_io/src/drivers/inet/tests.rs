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

//! In-module tests for the [`tcp_inet` driver](super): the wire/reply vocabulary, the argument
//! decoding, the lifecycle and the verb round trips over **real loopback sockets**, and the
//! peer-close state machine the driver latches when its peer goes away.
//!
//! Split out of `mod.rs` only because that file had grown past the ~1000-line target together
//! with this suite; the tests are still the driver's own in-module suite (`super` is the
//! `inet` module).
use super::wire::InetAddr;
use super::{
    ctl_error, ctl_ok, ctl_xerror, errno_name, make_tcp_inet, parse_address, socket_fd, InetState,
    PeerClose, TcpInetDriver, DRIVER_NAME, INET_AF_ANY, INET_AF_INET, INET_AF_LOOPBACK,
    INET_OPT_KEEPALIVE, INET_OPT_RCVBUF, INET_OPT_SNDBUF, INET_PROTO_DEFAULT, INET_REP_OK,
    INET_TYPE_STREAM, TCP_OPT_NODELAY, TCP_SHUT_RD, TCP_SHUT_RDWR, TCP_SHUT_WR,
};
use crate::driver::{Driver, DriverContext, DriverFlags, PortArms};
use crate::poller::Poller;
use crate::port::{PortRegistry, PortState};
use crate::port_task::{PortTaskKind, Readiness};
use crate::queue::ByteQueue;
use beam_common::{BeamError, Result};
use beam_process::PortId;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::io::{AsRawFd, RawFd};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long a helper waits for the kernel or the peer before failing the test.
const WAIT: Duration = Duration::from_secs(5);

/// Wall-clock budget for the `erl` oracle before the affected cross-check is skipped.
const ERL_TIMEOUT: Duration = Duration::from_secs(30);

fn poller() -> Poller {
    Poller::new().expect("a poll set")
}

/// A loopback listener the test owns (the driver is given its own copy of the fd).
fn loopback_listener() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").expect("a loopback listener")
}

/// The packed `open` argument: `[AF_INET, SOCK_STREAM, default protocol]`.
fn open_arg() -> [u8; 3] {
    [INET_AF_INET, INET_TYPE_STREAM, INET_PROTO_DEFAULT]
}

/// The packed `bind`/`connect` argument naming `127.0.0.1:port`.
fn loopback_arg(port: u16) -> Vec<u8> {
    let [hi, lo] = port.to_be_bytes();
    vec![INET_AF_LOOPBACK, hi, lo]
}

/// A copy of `fd` (`dup(2)`), so the driver and the test each own a descriptor.
fn duplicate(fd: RawFd) -> RawFd {
    // SAFETY: `fd` is an open descriptor owned by the caller and `dup(2)` takes no pointer
    // arguments; it hands back a second descriptor for the same open file description.
    let dup = unsafe { libc::dup(fd) };
    assert!(dup >= 0, "dup(2) succeeded");
    dup
}

/// Opens a port over `driver`.
fn open_port(poller: &mut Poller, driver: Box<dyn Driver>) -> (PortRegistry, PortId) {
    let mut registry = PortRegistry::new();
    let id = registry.open(poller, driver).expect("the port opens");
    (registry, id)
}

/// Runs one control verb on port `id`, panicking on the Rust error kind.
fn control(
    registry: &mut PortRegistry,
    poller: &mut Poller,
    id: PortId,
    command: &str,
    arg: &[u8],
) -> Vec<u8> {
    registry
        .get_mut(id)
        .expect("the port is live")
        .control(poller, command, arg)
        .unwrap_or_else(|err| panic!("the `{command}` verb failed: {err:?}"))
}

/// The `open`/`bind`/`listen` sequence over a loopback ephemeral port; returns the port
/// the kernel assigned.
fn listen_on_loopback(poller: &mut Poller, registry: &mut PortRegistry, id: PortId) -> u16 {
    assert_eq!(control(registry, poller, id, "open", &open_arg()), ctl_ok());
    let bound = control(registry, poller, id, "bind", &loopback_arg(0));
    assert_eq!(bound[0], INET_REP_OK);
    assert_eq!(bound.len(), 3, "`bind` replies [1, port_hi, port_lo]");
    let port = u16::from_be_bytes([bound[1], bound[2]]);
    assert_ne!(port, 0, "the kernel assigned a port");
    assert_eq!(control(registry, poller, id, "listen", &[0, 5]), ctl_ok());
    port
}

/// Accepts the connection queued for the listening port, retrying until it arrives;
/// returns the descriptor the reply carried.
fn accept(poller: &mut Poller, registry: &mut PortRegistry, id: PortId) -> RawFd {
    let deadline = Instant::now() + WAIT;
    loop {
        let reply = control(registry, poller, id, "accept", b"");
        assert_eq!(reply[0], INET_REP_OK);
        if reply.len() == 5 {
            return i32::from_be_bytes([reply[1], reply[2], reply[3], reply[4]]);
        }
        assert_eq!(reply.len(), 1, "an `accept` reply is [1] or [1] ++ fd_be32");
        assert!(Instant::now() < deadline, "no connection was accepted");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Drives the reactor until port `id`'s input queue holds at least `want` bytes,
/// returning everything it read.
fn read_into_queue(
    poller: &mut Poller,
    registry: &mut PortRegistry,
    id: PortId,
    want: usize,
) -> Vec<u8> {
    let deadline = Instant::now() + WAIT;
    let mut got = Vec::new();
    while got.len() < want {
        assert!(Instant::now() < deadline, "no input arrived");
        let events = poller
            .poll(Some(Duration::from_millis(100)))
            .expect("poll");
        registry
            .handle_events(poller, &events)
            .expect("dispatch the ready events");
        got.extend(registry.get_mut(id).expect("live").drain_input());
    }
    got
}

// -----------------------------------------------------------------------
// Names, reply shapes and argument decoding
// -----------------------------------------------------------------------

#[test]
fn the_driver_registers_under_the_oracle_name() {
    assert_eq!(DRIVER_NAME, "tcp_inet");
    let driver = make_tcp_inet().expect("the factory builds a driver");
    assert_eq!(driver.name(), "tcp_inet");
    assert!(driver.flags() == DriverFlags::empty());
}

#[test]
fn the_reply_shape_helpers_match_the_c() {
    assert_eq!(ctl_ok(), vec![INET_REP_OK]);
    assert_eq!(ctl_xerror("exbadport"), b"\x00exbadport".to_vec());
    assert_eq!(
        ctl_error(std::io::Error::from_raw_os_error(libc::ENOTCONN)),
        b"\x00enotconn".to_vec()
    );
    assert_eq!(
        ctl_error(std::io::Error::from_raw_os_error(libc::ECONNREFUSED)),
        b"\x00econnrefused".to_vec()
    );
    assert_eq!(
        ctl_error(std::io::Error::from_raw_os_error(libc::EISCONN)),
        b"\x00eisconn".to_vec()
    );
    assert_eq!(
        ctl_error(std::io::Error::from_raw_os_error(libc::EINVAL)),
        b"\x00einval".to_vec()
    );
    // An unknown code keeps the C's `errno_<N>` fallback format.
    assert_eq!(ctl_xerror(&errno_name(-1)), b"\x00errno_-1".to_vec());
    assert_eq!(errno_name(0), "errno_0");
    assert_eq!(errno_name(libc::EAGAIN), "eagain");
    assert_eq!(errno_name(libc::EINPROGRESS), "einprogress");
}

#[test]
fn the_packed_address_decodes_the_three_served_forms() {
    assert_eq!(
        parse_address(&[INET_AF_INET, 0x1f, 0x90, 10, 0, 0, 7]).expect("an IPv4 address"),
        InetAddr {
            port: 8080,
            addr: [10, 0, 0, 7]
        }
    );
    assert_eq!(
        parse_address(&[INET_AF_ANY, 0, 5]).expect("INADDR_ANY"),
        InetAddr {
            port: 5,
            addr: [0, 0, 0, 0]
        }
    );
    assert_eq!(
        parse_address(&[INET_AF_LOOPBACK, 0xff, 0xff]).expect("INADDR_LOOPBACK"),
        InetAddr {
            port: 0xffff,
            addr: [127, 0, 0, 1]
        }
    );
}

#[test]
fn a_malformed_or_unsupported_address_is_bad_arg() {
    assert!(parse_address(&[]).is_err(), "an empty argument");
    assert!(
        parse_address(&[INET_AF_INET, 0, 1, 127]).is_err(),
        "truncated"
    );
    assert!(parse_address(&[INET_AF_ANY]).is_err(), "truncated");
    assert!(
        parse_address(&[2, 0, 1, 0, 0, 0, 0, 0, 0]).is_err(),
        "INET_AF_INET6 (out of scope)"
    );
    assert!(
        parse_address(&[5, 2, b'x']).is_err(),
        "INET_AF_LOCAL (out of scope)"
    );
    // The error is the crate's argument fault.
    assert!(matches!(parse_address(&[]), Err(BeamError::BadArg)));
}

#[test]
fn the_sockaddr_in_of_an_endpoint_is_network_byte_order() {
    let sa = InetAddr {
        port: 0x1234,
        addr: [192, 168, 1, 2],
    }
    .sockaddr_in();
    assert_eq!(sa.sin_family as i32, libc::AF_INET);
    assert_eq!(sa.sin_port, 0x1234u16.to_be());
    assert_eq!(sa.sin_addr.s_addr, u32::from_be_bytes([192, 168, 1, 2]).to_be());
}

// -----------------------------------------------------------------------
// Lifecycle
// -----------------------------------------------------------------------

#[test]
fn a_fresh_driver_has_no_socket() {
    let driver = TcpInetDriver::new();
    assert_eq!(driver.state(), InetState::Closed);
    assert_eq!(driver.socket(), None);
    assert!(!driver.eof());
    assert!(!driver.peer_closed(), "nothing has closed yet");
    assert_eq!(driver.connect_error(), None);
    assert_eq!(TcpInetDriver::default().state(), InetState::Closed);
}

/// The peer-close state machine: once the close has been observed the **first** `recv` and the
/// **first** `send` answer `closed`, every later one `enotconn` — the C's
/// `TCP_ADDF_DELAYED_CLOSE_RECV`/`TCP_ADDF_DELAYED_CLOSE_SEND` bookkeeping
/// (`inet_drv.c:12798`-`:12819`, `:13662`-`:13684`), which the `erl` oracle reproduces as
/// `R,R,S,S -> closed, enotconn, closed, enotconn` and `S,S,R,R,R -> ok, closed, closed,
/// enotconn, enotconn`.
#[test]
fn the_peer_close_answers_once_per_direction() {
    // While the connection is usable neither direction owes a reply.
    let mut close = PeerClose::default();
    assert_eq!(close.recv_reply(), None);
    assert_eq!(close.send_reply(), None);

    // The close is observed: each direction reports it exactly once, then `enotconn` forever.
    close.observe();
    assert_eq!(close.recv_reply(), Some("closed"));
    assert_eq!(close.send_reply(), Some("closed"));
    assert_eq!(close.recv_reply(), Some("enotconn"));
    assert_eq!(close.send_reply(), Some("enotconn"));
    assert_eq!(close.recv_reply(), Some("enotconn"));

    // The two directions are independent: a `send` that is what discovered the close does not
    // consume the receive direction's one-shot `closed` (the C's `tcp_send_error` ->
    // `tcp_recv_closed` path, which is how a caller that ignores its send errors still learns
    // about the close from its next receive).
    let mut close = PeerClose::default();
    close.observe();
    assert_eq!(close.send_reply(), Some("closed"));
    assert_eq!(close.recv_reply(), Some("closed"));
    assert_eq!(close.send_reply(), Some("enotconn"));
    assert_eq!(close.recv_reply(), Some("enotconn"));
}

/// Once the peer's end goes away the driver **latches** the close: the
/// [`TcpInetDriver::peer_closed`] accessor the runtime reads becomes true, the socket's poller
/// registration is released (a dead descriptor must not keep reporting readiness), and the
/// connection's [`InetState`] is deliberately **unchanged** — only the read half is gone, so
/// answering `closed` the way the oracle does stays the verb layer's job.
#[test]
fn a_peer_close_is_observable_through_the_accessor() {
    let listener = loopback_listener();
    let address = listener.local_addr().expect("an address");
    let client = TcpStream::connect(address).expect("connects");
    let (server, _peer) = listener.accept().expect("accepts");

    let mut poller = poller();
    let mut driver = TcpInetDriver::from_socket(duplicate(server.as_raw_fd())).expect("adopts");
    assert!(!driver.peer_closed());
    let mut input = ByteQueue::new();
    let mut output = ByteQueue::new();
    let mut arms = PortArms::default();
    let mut ctx = DriverContext::new(
        PortId::new(0),
        &mut poller,
        &mut input,
        &mut output,
        &mut arms,
    );
    driver.start(&mut ctx).expect("registers the socket");

    // The peer goes away; the read that observes the end-of-file is the one that answers
    // `closed` and latches the close.
    client.shutdown(Shutdown::Both).expect("shuts the peer down");
    let deadline = Instant::now() + WAIT;
    let reply = loop {
        let reply = driver.control(&mut ctx, "recv", b"").expect("the recv verb");
        if reply != ctl_ok() {
            break reply;
        }
        assert!(Instant::now() < deadline, "the close was never observed");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(reply, ctl_xerror("closed"));
    assert!(driver.peer_closed(), "the accessor the runtime reads");
    assert!(driver.eof());
    assert_eq!(
        driver.state(),
        InetState::Connected,
        "only the read half is gone, so the state is unchanged"
    );
    assert_eq!(
        driver.control(&mut ctx, "recv", b"").expect("the recv verb"),
        ctl_xerror("enotconn")
    );
    assert_eq!(
        driver.control(&mut ctx, "send", b"x").expect("the send verb"),
        ctl_xerror("closed"),
        "the send direction still owes its one-shot `closed`"
    );
    assert_eq!(
        driver.control(&mut ctx, "send", b"x").expect("the send verb"),
        ctl_xerror("enotconn")
    );
}
#[test]
fn from_socket_reads_the_state_from_the_kernel() {
    // A listening socket (`SO_ACCEPTCONN`).
    let listener = loopback_listener();
    let driver = TcpInetDriver::from_socket(duplicate(listener.as_raw_fd())).expect("adopts");
    assert_eq!(driver.state(), InetState::Listening);
    drop(driver); // closes only the duplicate, not the test's listener

    // A connected socket (has a peer).
    let address = listener.local_addr().expect("an address");
    let client = TcpStream::connect(address).expect("connects");
    let (server, _peer) = listener.accept().expect("accepts");
    let fd = duplicate(server.as_raw_fd());
    let driver = TcpInetDriver::from_socket(fd).expect("adopts");
    assert_eq!(driver.state(), InetState::Connected);
    assert_eq!(driver.socket(), Some(fd));
    drop(driver);
    let _ = client;

    // An opened but unconnected socket.
    let fd = socket_fd().expect("a fresh socket");
    let driver = TcpInetDriver::from_socket(fd).expect("adopts");
    assert_eq!(driver.state(), InetState::Open);

    // A descriptor that is not a socket is rejected, never dereferenced blindly.
    assert!(TcpInetDriver::from_socket(-1).is_err());
}

#[test]
fn commands_before_open_report_the_c_operational_errors() {
    let mut poller = poller();
    let (mut registry, id) = open_port(&mut poller, make_tcp_inet().expect("a driver"));

    // No socket yet: the state errors the C answers with a `ctl_xerror` reply.
    assert_eq!(
        control(&mut registry, &mut poller, id, "bind", &loopback_arg(0)),
        b"\x00exbadport".to_vec()
    );
    assert_eq!(
        control(&mut registry, &mut poller, id, "listen", &[0, 5]),
        b"\x00exbadport".to_vec()
    );
    assert_eq!(
        control(&mut registry, &mut poller, id, "connect", &loopback_arg(1)),
        b"\x00exbadport".to_vec()
    );
    assert_eq!(
        control(&mut registry, &mut poller, id, "recv", b""),
        b"\x00enotconn".to_vec()
    );
    assert_eq!(
        control(&mut registry, &mut poller, id, "send", b"x"),
        b"\x00enotconn".to_vec()
    );
    assert_eq!(
        control(&mut registry, &mut poller, id, "shutdown", &[0]),
        b"\x00enotconn".to_vec()
    );
    assert_eq!(
        control(&mut registry, &mut poller, id, "accept", b""),
        b"\x00einval".to_vec(),
        "`accept` on a non-listener"
    );
    // `close` is unconditional in the C (`ctl_reply_ok`, `inet_drv.c:12259`).
    assert_eq!(control(&mut registry, &mut poller, id, "close", b""), ctl_ok());
    registry.close(&mut poller, id).expect("closes");
}

#[test]
fn unknown_verbs_and_malformed_arguments_are_bad_arg() {
    let mut poller = poller();
    let (mut registry, id) = open_port(&mut poller, make_tcp_inet().expect("a driver"));
    assert_eq!(registry.get(id).expect("live").driver_name(), "tcp_inet");
    assert_eq!(registry.get(id).expect("live").state(), PortState::Open);

    let mut verb = |command: &str, arg: &[u8]| {
        registry
            .get_mut(id)
            .expect("live")
            .control(&mut poller, command, arg)
    };
    assert!(matches!(verb("frobnicate", b""), Err(BeamError::BadArg)));
    // `open` needs its three-byte record ...
    assert!(matches!(
        verb("open", &[INET_AF_INET, 1]),
        Err(BeamError::BadArg)
    ));
    // ... whose second byte must be SOCK_STREAM ...
    assert!(matches!(
        verb("open", &[INET_AF_INET, 2, INET_PROTO_DEFAULT]),
        Err(BeamError::BadArg)
    ));
    // ... and whose protocol byte must be the default or TCP.
    assert!(matches!(
        verb("open", &[INET_AF_INET, INET_TYPE_STREAM, 9]),
        Err(BeamError::BadArg)
    ));
    // None of those failures created a socket.
    assert_eq!(verb("recv", b"").expect("a reply"), b"\x00enotconn".to_vec());

    // A valid `open` creates the socket ...
    assert_eq!(verb("open", &open_arg()).expect("opens"), ctl_ok());
    // ... after which the argument faults of the remaining verbs surface as `BadArg`
    // (the state guards that answer `exbadport`/`enotconn` come first, as in the C).
    assert!(matches!(verb("listen", &[9]), Err(BeamError::BadArg)));
    assert!(matches!(verb("bind", &[]), Err(BeamError::BadArg)));
    assert!(matches!(verb("connect", b""), Err(BeamError::BadArg)));
    // A second `open` is the operational `einval` (the C would leak the first socket
    // here, so this floor refuses instead).
    assert_eq!(
        verb("open", &open_arg()).expect("a reply"),
        b"\x00einval".to_vec()
    );
    registry.close(&mut poller, id).expect("closes");
}

// -----------------------------------------------------------------------
// The verb round trips (real loopback sockets)
// -----------------------------------------------------------------------

#[test]
fn the_open_bind_listen_accept_send_and_recv_verbs_round_trip() {
    let mut poller = poller();
    let (mut registry, id) = open_port(&mut poller, make_tcp_inet().expect("a driver"));
    let port = listen_on_loopback(&mut poller, &mut registry, id);

    let mut client = TcpStream::connect(("127.0.0.1", port)).expect("the client connects");
    let accepted_fd = accept(&mut poller, &mut registry, id);
    assert!(accepted_fd >= 0, "the reply carried a real descriptor");

    // Peer -> port: the reactor reads the bytes into the port's input queue.
    client.write_all(b"ping").expect("the client writes");
    assert_eq!(
        read_into_queue(&mut poller, &mut registry, id, 4),
        b"ping",
        "the connection's bytes reach the port input queue"
    );

    // Port -> peer: the `send` verb drains the socket.
    assert_eq!(
        control(&mut registry, &mut poller, id, "send", b"pong"),
        ctl_ok()
    );
    assert_eq!(registry.get(id).expect("live").output_len(), 0);
    let mut reply = [0u8; 4];
    client.read_exact(&mut reply).expect("the client reads");
    assert_eq!(&reply, b"pong");

    // `shutdown` then `close`, and a second `close` is idempotent.
    assert_eq!(
        control(&mut registry, &mut poller, id, "shutdown", &[TCP_SHUT_WR]),
        ctl_ok()
    );
    assert_eq!(control(&mut registry, &mut poller, id, "close", b""), ctl_ok());
    assert_eq!(control(&mut registry, &mut poller, id, "close", b""), ctl_ok());
    registry.close(&mut poller, id).expect("closes the port");
}

#[test]
fn an_accept_with_nothing_pending_answers_ok() {
    let listener = loopback_listener();
    let mut poller = poller();
    let (mut registry, id) = open_port(
        &mut poller,
        Box::new(TcpInetDriver::from_socket(duplicate(listener.as_raw_fd())).expect("adopts")),
    );
    assert_eq!(
        control(&mut registry, &mut poller, id, "accept", b""),
        ctl_ok(),
        "EAGAIN is `ok` with no descriptor (retry when the port reports readability)"
    );
    registry.close(&mut poller, id).expect("closes");
}

#[test]
fn the_recv_and_send_verbs_work_on_an_adopted_connected_socket() {
    let listener = loopback_listener();
    let address = listener.local_addr().expect("an address");
    let mut client = TcpStream::connect(address).expect("connects");
    let (server, _peer) = listener.accept().expect("accepts");

    let mut poller = poller();
    let (mut registry, id) = open_port(
        &mut poller,
        Box::new(TcpInetDriver::from_socket(duplicate(server.as_raw_fd())).expect("adopts")),
    );

    // `recv` drives the read path directly (no poller needed): retry until the bytes the
    // client sent have arrived.
    client.write_all(b"hello").expect("the client writes");
    let deadline = Instant::now() + WAIT;
    let mut got = Vec::new();
    while got.len() < 5 {
        assert!(Instant::now() < deadline, "`recv` read nothing");
        assert_eq!(control(&mut registry, &mut poller, id, "recv", b""), ctl_ok());
        got.extend(registry.get_mut(id).expect("live").drain_input());
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(got, b"hello");

    // ... and `send` writes back to the peer.
    assert_eq!(
        control(&mut registry, &mut poller, id, "send", b"world"),
        ctl_ok()
    );
    let mut reply = [0u8; 5];
    client.read_exact(&mut reply).expect("the client reads");
    assert_eq!(&reply, b"world");

    // `recv` with an argument is a `BadArg`: the C's timeout/length are not honored, so
    // accepting them would silently ignore them.
    assert!(matches!(
        registry
            .get_mut(id)
            .expect("live")
            .control(&mut poller, "recv", &[0, 0, 0, 0]),
        Err(BeamError::BadArg)
    ));
    registry.close(&mut poller, id).expect("closes");
}

// -----------------------------------------------------------------------
// Flow control: readiness arming and short writes
// -----------------------------------------------------------------------

#[test]
fn a_write_ready_event_is_ignored_until_write_readiness_is_armed() {
    let mut poller = poller();
    let (mut registry, id) = open_port(&mut poller, make_tcp_inet().expect("a driver"));
    listen_on_loopback(&mut poller, &mut registry, id);

    // A listening socket arms only readability, so a write-ready event schedules nothing
    // (`PortArms`), while a read-ready one does.
    assert!(registry.schedule_readiness(
        id.number(),
        Readiness {
            writable: true,
            ..Readiness::default()
        }
    ));
    assert!(!registry
        .get(id)
        .expect("live")
        .has_pending_task(PortTaskKind::Output));
    assert!(registry.schedule_readiness(
        id.number(),
        Readiness {
            readable: true,
            ..Readiness::default()
        }
    ));
    assert!(registry
        .get(id)
        .expect("live")
        .has_pending_task(PortTaskKind::Input));
    registry.close(&mut poller, id).expect("closes");
}

#[test]
fn a_stalled_write_arms_write_readiness_and_defers_a_shutdown() {
    let mut poller = poller();
    let (mut registry, id) = open_port(&mut poller, make_tcp_inet().expect("a driver"));
    let port = listen_on_loopback(&mut poller, &mut registry, id);
    // The peer connects but never reads, so the kernel's buffers fill and stay full.
    let _client = TcpStream::connect(("127.0.0.1", port)).expect("connects");
    accept(&mut poller, &mut registry, id);

    // A small write is taken by the kernel straight away.
    assert_eq!(
        control(&mut registry, &mut poller, id, "send", b"tiny"),
        ctl_ok()
    );
    assert_eq!(registry.get(id).expect("live").output_len(), 0);

    // Keep offering bytes until the kernel stops taking them: the remainder stays in the
    // port output queue and write readiness is armed.
    let chunk = vec![b'x'; 256 * 1024];
    let deadline = Instant::now() + WAIT;
    while registry.get(id).expect("live").output_len() == 0 {
        assert!(Instant::now() < deadline, "the kernel accepted every byte");
        assert_eq!(
            control(&mut registry, &mut poller, id, "send", &chunk),
            ctl_ok()
        );
    }

    // The evidence that write readiness is armed: a write-ready event now schedules an
    // OUTPUT task (which the port-task round would run when the kernel drains).
    assert!(registry.schedule_readiness(
        id.number(),
        Readiness {
            writable: true,
            ..Readiness::default()
        }
    ));
    assert!(registry
        .get(id)
        .expect("live")
        .has_pending_task(PortTaskKind::Output));

    // A `SHUT_WR`/`SHUT_RDWR` with output still queued is deferred, never dropped.
    assert_eq!(
        control(&mut registry, &mut poller, id, "shutdown", &[TCP_SHUT_RDWR]),
        ctl_ok()
    );
    assert_eq!(
        control(&mut registry, &mut poller, id, "shutdown", &[TCP_SHUT_WR]),
        ctl_ok()
    );
    assert!(registry.get(id).expect("live").output_len() > 0);
    registry.close(&mut poller, id).expect("closes");
}

// -----------------------------------------------------------------------
// The socket options (`setopts` / `getopts`)
// -----------------------------------------------------------------------

/// One `[opt, val_be32]` record — the C's `while (len >= 5)` unit
/// (`inet_set_opts`, `inet_drv.c:6908`).
fn opt_record(code: u8, value: u32) -> Vec<u8> {
    let mut record = vec![code];
    record.extend_from_slice(&value.to_be_bytes());
    record
}

/// The reply `getopts` answers for a single served code: the C's `[1]` followed by one
/// `[opt, val_be32]` record.
fn opt_reply(code: u8, value: u32) -> Vec<u8> {
    let mut reply = ctl_ok();
    reply.extend_from_slice(&opt_record(code, value));
    reply
}

/// Decodes a `getopts` reply into its `(code, value)` records, asserting the `[1]` tag and the
/// five-byte record framing.
fn parse_opt_reply(reply: &[u8]) -> Vec<(u8, u32)> {
    assert_eq!(
        reply.first(),
        Some(&INET_REP_OK),
        "a `getopts` reply starts with the ok tag: {reply:?}"
    );
    let records = &reply[1..];
    assert_eq!(records.len() % 5, 0, "records are five bytes each: {reply:?}");
    records
        .chunks_exact(5)
        .map(|r| (r[0], u32::from_be_bytes([r[1], r[2], r[3], r[4]])))
        .collect()
}

/// **The four served options round-trip**, and the read-back is the **kernel's** value, not the
/// requested one — the oracle's `setopts 4096 -> getopts 8192` doubling for the two buffers.
///
/// The expected bytes are the ones the system `erl` reports for the identical sequence
/// (`open`/`bind`/`listen`, then the records below): `setopts nodelay=1`/`getopts nodelay` is
/// `[1]`/`[1,10,0,0,0,1]`, `keepalive` likewise `[1,1,0,0,0,1]`, and `recbuf`/`sndbuf` are
/// `[1,7,0,0,32,0]`/`[1,6,0,0,32,0]` — i.e. 8192 for a 4096 request.
#[test]
fn the_served_socket_options_round_trip_like_the_oracle() {
    let mut poller = poller();
    let (mut registry, id) = open_port(&mut poller, make_tcp_inet().expect("a driver"));
    listen_on_loopback(&mut poller, &mut registry, id);

    // The defaults a fresh AF_INET stream socket reports for the two boolean flags.
    assert_eq!(
        control(&mut registry, &mut poller, id, "getopts", &[TCP_OPT_NODELAY]),
        opt_reply(TCP_OPT_NODELAY, 0)
    );
    assert_eq!(
        control(&mut registry, &mut poller, id, "getopts", &[INET_OPT_KEEPALIVE]),
        opt_reply(INET_OPT_KEEPALIVE, 0)
    );

    // The two boolean flags: set, then read back as 1.
    for code in [TCP_OPT_NODELAY, INET_OPT_KEEPALIVE] {
        assert_eq!(
            control(&mut registry, &mut poller, id, "setopts", &opt_record(code, 1)),
            ctl_ok(),
            "`setopts` answers the C's ctl_reply_ok whatever the option"
        );
        assert_eq!(
            control(&mut registry, &mut poller, id, "getopts", &[code]),
            opt_reply(code, 1),
            "the option reads back as the kernel stored it"
        );
    }

    // The two buffers: the kernel reports **twice** the requested size (Linux's
    // `SO_RCVBUF`/`SO_SNDBUF` doubling), exactly as the oracle measured, because the driver
    // reports the kernel's own `getsockopt(2)` value.
    for code in [INET_OPT_RCVBUF, INET_OPT_SNDBUF] {
        let requested = 4096u32;
        assert_eq!(
            control(
                &mut registry,
                &mut poller,
                id,
                "setopts",
                &opt_record(code, requested)
            ),
            ctl_ok()
        );
        let reply = control(&mut registry, &mut poller, id, "getopts", &[code]);
        assert_eq!(reply.len(), 6, "one record: {reply:?}");
        assert_eq!(reply[1], code);
        let value = u32::from_be_bytes([reply[2], reply[3], reply[4], reply[5]]);
        assert_eq!(
            value,
            requested * 2,
            "the driver reports the kernel's own value (Linux doubles a buffer request), \
             never the requested one"
        );
    }
    registry.close(&mut poller, id).expect("closes");
}

/// `getopts` answers **one record per request code, in request order**, each value read live
/// from the socket — the C's `*ptr++ = opt; put_int32(ival, ptr);` loop (`inet_drv.c:9141`-
/// `:9746`), which the oracle shows as `getopts <<1,10,6,7>>` answering the records in exactly
/// that order.
#[test]
fn getopts_reads_the_records_in_request_order() {
    let mut poller = poller();
    let (mut registry, id) = open_port(&mut poller, make_tcp_inet().expect("a driver"));
    listen_on_loopback(&mut poller, &mut registry, id);

    let order = [
        INET_OPT_KEEPALIVE,
        TCP_OPT_NODELAY,
        INET_OPT_SNDBUF,
        INET_OPT_RCVBUF,
    ];
    let reply = control(&mut registry, &mut poller, id, "getopts", &order);
    let codes: Vec<u8> = parse_opt_reply(&reply)
        .into_iter()
        .map(|(code, _)| code)
        .collect();
    assert_eq!(codes, order, "the records follow the request order");

    // A `setopts` of two records is reflected in the next read-back, in FIFO order.
    let mut set = opt_record(TCP_OPT_NODELAY, 1);
    set.extend_from_slice(&opt_record(INET_OPT_RCVBUF, 4096));
    assert_eq!(
        control(&mut registry, &mut poller, id, "setopts", &set),
        ctl_ok()
    );

    let records = parse_opt_reply(&control(
        &mut registry,
        &mut poller,
        id,
        "getopts",
        &[TCP_OPT_NODELAY, INET_OPT_RCVBUF],
    ));
    assert_eq!(records[0], (TCP_OPT_NODELAY, 1));
    assert_eq!(
        records[1],
        (INET_OPT_RCVBUF, 8192),
        "the kernel's doubled value, read live"
    );
    registry.close(&mut poller, id).expect("closes");
}

/// A code this floor does not serve is the C's `default: return -1` becoming the **whole**
/// `ctl_error(EINVAL)` reply (`inet_drv.c:7914`/`:9719` -> `:11062`/`:11128`), never a partial
/// one: the oracle's `getopts <<1,200>>` is exactly `[0] ++ "einval"`.
#[test]
fn an_unserved_option_code_is_the_whole_einval_reply() {
    let mut poller = poller();
    let (mut registry, id) = open_port(&mut poller, make_tcp_inet().expect("a driver"));
    listen_on_loopback(&mut poller, &mut registry, id);

    assert_eq!(
        control(&mut registry, &mut poller, id, "getopts", &[200]),
        ctl_xerror("einval")
    );
    assert_eq!(
        control(&mut registry, &mut poller, id, "setopts", &opt_record(200, 1)),
        ctl_xerror("einval")
    );
    // A **mixed** request answers the whole error, so `getopts` never returns a short reply
    // that a caller could mistake for a partial success.
    assert_eq!(
        control(&mut registry, &mut poller, id, "getopts", &[INET_OPT_KEEPALIVE, 200]),
        ctl_xerror("einval")
    );
    let mut mixed = opt_record(INET_OPT_KEEPALIVE, 1);
    mixed.extend_from_slice(&opt_record(200, 1));
    assert_eq!(
        control(&mut registry, &mut poller, id, "setopts", &mixed),
        ctl_xerror("einval")
    );
    registry.close(&mut poller, id).expect("closes");
}

/// The payload framing: `setopts`'s `while (len >= 5)` ignores a trailing partial record
/// (`inet_set_opts`, `inet_drv.c:6908`) and `inet_fill_opts`'s `while(len--)` makes an empty
/// `getopts` payload a bare `[1]` — the oracle's `setopts <<7,0,0>>` = `[1]`, `getopts <<>>` =
/// `[1]`, `setopts <<>>` = `[1]`.
#[test]
fn a_partial_or_empty_option_payload_is_ignored_like_the_c() {
    let mut poller = poller();
    let (mut registry, id) = open_port(&mut poller, make_tcp_inet().expect("a driver"));
    listen_on_loopback(&mut poller, &mut registry, id);

    // A partial trailing record is ignored ...
    assert_eq!(
        control(&mut registry, &mut poller, id, "setopts", &[INET_OPT_RCVBUF, 0, 0]),
        ctl_ok()
    );
    // ... while a complete record before it is still applied.
    let mut arg = opt_record(TCP_OPT_NODELAY, 1);
    arg.extend_from_slice(&[INET_OPT_RCVBUF, 0]);
    assert_eq!(
        control(&mut registry, &mut poller, id, "setopts", &arg),
        ctl_ok()
    );
    assert_eq!(
        control(&mut registry, &mut poller, id, "getopts", &[TCP_OPT_NODELAY]),
        opt_reply(TCP_OPT_NODELAY, 1),
        "the complete record was applied"
    );

    // An empty payload is the C's bare `[1]` for either verb.
    assert_eq!(
        control(&mut registry, &mut poller, id, "getopts", &[]),
        ctl_ok()
    );
    assert_eq!(
        control(&mut registry, &mut poller, id, "setopts", &[]),
        ctl_ok()
    );
    registry.close(&mut poller, id).expect("closes");
}

/// Neither verb has the C's `IS_OPEN` guard (`inet_drv.c:11046`/`:11111`), so a port with **no
/// socket at all** still answers: `setopts` is `[1]` (the `setsockopt` on the invalid descriptor
/// fails and is not propagated), `getopts` is a bare `[1]` (the `getsockopt` fails and the C's
/// `TRUNCATE_TO(0, ptr)` drops the record, `:9739`) — and an unserved code is still `einval`,
/// because the option switch precedes the syscall.
#[test]
fn the_option_verbs_on_a_socket_less_port_match_the_oracle() {
    let mut poller = poller();
    let (mut registry, id) = open_port(&mut poller, make_tcp_inet().expect("a driver"));

    assert_eq!(
        control(
            &mut registry,
            &mut poller,
            id,
            "setopts",
            &opt_record(INET_OPT_RCVBUF, 4096)
        ),
        ctl_ok()
    );
    assert_eq!(
        control(&mut registry, &mut poller, id, "getopts", &[INET_OPT_RCVBUF]),
        ctl_ok(),
        "no socket: the record is dropped, so the reply is the bare [1]"
    );
    assert_eq!(
        control(&mut registry, &mut poller, id, "getopts", &[200]),
        ctl_xerror("einval")
    );
    assert_eq!(
        control(&mut registry, &mut poller, id, "setopts", &opt_record(200, 1)),
        ctl_xerror("einval")
    );

    // After a bare `open` (no `bind`/`listen`) the socket exists, so the read-back is live:
    // the oracle reports `[1,1,0,0,0,0]` for `keepalive` on such a socket.
    assert_eq!(
        control(&mut registry, &mut poller, id, "open", &open_arg()),
        ctl_ok()
    );
    assert_eq!(
        control(&mut registry, &mut poller, id, "getopts", &[INET_OPT_KEEPALIVE]),
        opt_reply(INET_OPT_KEEPALIVE, 0)
    );
    registry.close(&mut poller, id).expect("closes");
}

// -----------------------------------------------------------------------
// Half-close, deferred shutdown and the latched peer close
// -----------------------------------------------------------------------

/// A [`TcpInetDriver`] driven **directly** through a [`DriverContext`] rather than through a
/// [`PortRegistry`], because the half-close tests need the driver's own fields
/// (`pending_shutdown`, the registered `interest`, `peer_closed`) and the port's queues — none of
/// which a [`crate::port::Port`] exposes.
///
/// The context is rebuilt for every call: it borrows the rig's fields, and a driver never holds
/// one across calls anyway (`crate::port::Port` builds one per driver interaction).
struct Rig {
    driver: TcpInetDriver,
    poller: Poller,
    input: ByteQueue,
    output: ByteQueue,
    arms: PortArms,
}

impl Rig {
    /// Wraps `driver` in a fresh poll set and a pair of empty port queues.
    fn new(driver: TcpInetDriver) -> Rig {
        Rig {
            driver,
            poller: poller(),
            input: ByteQueue::new(),
            output: ByteQueue::new(),
            arms: PortArms::default(),
        }
    }

    /// Adopts one end of a **connected loopback pair**, starts it, and returns the rig together
    /// with the peer [`TcpStream`] the test drives.
    fn connected() -> (Rig, TcpStream) {
        let listener = loopback_listener();
        let address = listener.local_addr().expect("an address");
        let client = TcpStream::connect(address).expect("the peer connects");
        let (server, _peer) = listener.accept().expect("the driver's end is accepted");
        let driver = TcpInetDriver::from_socket(duplicate(server.as_raw_fd())).expect("adopts");
        let mut rig = Rig::new(driver);
        rig.start().expect("the socket registers");
        (rig, client)
    }

    /// [`Driver::start`]: registers the socket with the poll set.
    ///
    /// The [`DriverContext`] is built **inline** by each method below rather than returned from a
    /// `&mut self` helper: such a helper would borrow the whole rig, so the driver could not be
    /// borrowed at the same time (`crate::port::Port` builds one context per driver interaction
    /// for the same reason).
    fn start(&mut self) -> Result<()> {
        let mut ctx = DriverContext::new(
            PortId::new(0),
            &mut self.poller,
            &mut self.input,
            &mut self.output,
            &mut self.arms,
        );
        self.driver.start(&mut ctx)
    }

    /// One control verb on the driver.
    fn control(&mut self, command: &str, arg: &[u8]) -> Result<Vec<u8>> {
        let mut ctx = DriverContext::new(
            PortId::new(0),
            &mut self.poller,
            &mut self.input,
            &mut self.output,
            &mut self.arms,
        );
        self.driver.control(&mut ctx, command, arg)
    }

    /// [`Driver::output`]: drains the port output queue to the socket.
    fn drain_output(&mut self) -> Result<usize> {
        let mut ctx = DriverContext::new(
            PortId::new(0),
            &mut self.poller,
            &mut self.input,
            &mut self.output,
            &mut self.arms,
        );
        self.driver.output(&mut ctx)
    }

    /// The bytes the driver has read into the port input queue, consumed.
    fn take_input(&mut self) -> Vec<u8> {
        let bytes = self.input.front().to_vec();
        self.input.consume(bytes.len());
        bytes
    }

    /// How many bytes are waiting in the port input queue.
    fn input_len(&self) -> usize {
        self.input.len()
    }

    /// How many bytes are waiting in the port output queue.
    fn output_len(&self) -> usize {
        self.output.len()
    }
}

/// Offers 256 KiB chunks through the `send` verb until the kernel stops accepting them, so the
/// remainder stays in the port output queue — the precondition of the C's
/// `TCP_ADDF_PENDING_SHUT_WR`/`TCP_ADDF_PENDING_SHUT_RDWR` (`inet_drv.c:12362`), and the same
/// state a short `write(2)` leaves behind. Returns the number of queued bytes.
fn fill_output_queue(rig: &mut Rig) -> usize {
    let chunk = vec![b'x'; 256 * 1024];
    let deadline = Instant::now() + WAIT;
    while rig.output_len() == 0 {
        assert!(Instant::now() < deadline, "the kernel accepted every byte");
        assert_eq!(
            rig.control("send", &chunk).expect("the send verb"),
            ctl_ok()
        );
    }
    rig.output_len()
}

/// **A write-side `shutdown` leaves the connection readable**: the peer's later bytes are still
/// read into the port input queue.
///
/// This is the `gen_tcp:shutdown(S, write)` -> `ok`, `gen_tcp:recv(S, 5)` -> `{ok, <<"hello">>}`
/// pair the oracle cross-check below pins on the real `inet` stack.
#[test]
fn a_half_close_leaves_the_connection_readable() {
    let (mut rig, mut client) = Rig::connected();

    // Nothing is queued, so `SHUT_WR` takes the C's immediate path: the reply is `[1]` and
    // nothing is deferred.
    assert_eq!(
        rig.control("shutdown", &[TCP_SHUT_WR]).expect("the verb"),
        ctl_ok()
    );
    assert_eq!(
        rig.driver.pending_shutdown, None,
        "nothing was queued, so nothing was deferred"
    );
    assert!(
        rig.driver.interest.is_readable(),
        "the read direction stays armed"
    );

    // The peer observes the half-close as end-of-file ...
    client
        .set_read_timeout(Some(WAIT))
        .expect("a read timeout");
    let mut eof = [0u8; 1];
    assert_eq!(
        client.read(&mut eof).expect("the peer reads its end-of-file"),
        0,
        "a real `shutdown(2)` on the write side reaches the peer as EOF"
    );

    // ... and the connection is still READABLE: the bytes the peer sends afterwards arrive.
    client.write_all(b"hello").expect("the peer writes");
    let deadline = Instant::now() + WAIT;
    while rig.input_len() < 5 {
        assert!(Instant::now() < deadline, "the half-closed driver read nothing");
        assert_eq!(rig.control("recv", b"").expect("the recv verb"), ctl_ok());
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(rig.take_input(), b"hello");
}

/// **A `SHUT_WR` with output still queued is deferred, never dropped** — and it is applied as
/// soon as the queue drains (the C's `TCP_ADDF_PENDING_SHUT_WR`, applied from the write path).
#[test]
fn a_deferred_shutdown_is_applied_when_the_output_queue_drains() {
    let (mut rig, mut client) = Rig::connected();
    let queued = fill_output_queue(&mut rig);
    assert!(queued > 0, "the output queue is stalled");

    // The request is remembered, not dropped, and the reply is still the C's `[1]`.
    assert_eq!(
        rig.control("shutdown", &[TCP_SHUT_WR]).expect("the verb"),
        ctl_ok()
    );
    assert_eq!(
        rig.driver.pending_shutdown,
        Some(TCP_SHUT_WR),
        "deferred until the queue drains"
    );
    assert_eq!(rig.output_len(), queued, "no queued byte was dropped");
    assert!(
        rig.driver.interest.is_writable(),
        "the stalled write keeps write readiness armed"
    );

    // The peer reads everything, so the queue drains; *that* is when the deferred `shutdown(2)`
    // is applied and write readiness is dropped again.
    let (done_tx, done_rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 64 * 1024];
        let mut total = 0usize;
        let outcome = loop {
            match client.read(&mut buf) {
                Ok(0) => break Ok(total),
                Ok(n) => total += n,
                Err(err) => break Err(err),
            }
        };
        let _ = done_tx.send(outcome);
    });

    let deadline = Instant::now() + WAIT;
    while rig.output_len() > 0 {
        assert!(Instant::now() < deadline, "the peer never drained the queue");
        rig.drain_output().expect("the output callback");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        rig.driver.pending_shutdown, None,
        "the deferred shutdown was applied on drain"
    );
    assert!(rig.driver.interest.is_readable(), "read readiness is kept");
    assert!(
        !rig.driver.interest.is_writable(),
        "write readiness is dropped once the queue drains"
    );

    // The peer read every byte and then hit end-of-file, which only the real `shutdown(2)`
    // produces.
    let total = match done_rx.recv_timeout(WAIT) {
        Ok(Ok(total)) => total,
        Ok(Err(err)) => panic!("the peer read failed: {err}"),
        Err(err) => panic!("the peer never reached end-of-file: {err}"),
    };
    assert!(
        total >= queued,
        "the peer read at least the queued {queued} bytes, not {total}"
    );
    reader.join().expect("the reader thread");
}

/// **`SHUT_RD` is never deferred**: the read direction has nothing to do with the output queue,
/// so it is applied at once even while bytes are still queued, leaving them untouched.
///
/// The read half really is gone afterwards — a `recv` now finds end-of-file (not `EAGAIN`) and
/// latches the peer's close, which is how the oracle reports `gen_tcp:shutdown(S, read)` followed
/// by `gen_tcp:recv(S, 0)` as `ok` then `{error, closed}`.
#[test]
fn a_read_side_shutdown_is_never_deferred() {
    let (mut rig, _client) = Rig::connected();
    let queued = fill_output_queue(&mut rig);
    assert!(queued > 0, "the output queue is stalled");

    assert_eq!(
        rig.control("shutdown", &[TCP_SHUT_RD]).expect("the verb"),
        ctl_ok()
    );
    assert_eq!(
        rig.driver.pending_shutdown, None,
        "`SHUT_RD` is never deferred"
    );
    assert_eq!(rig.output_len(), queued, "the queued output is untouched");

    assert_eq!(
        rig.control("recv", b"").expect("the recv verb"),
        ctl_xerror("closed"),
        "the read side is shut immediately, so the read finds end-of-file"
    );
}

/// **The latched peer close answers `closed` once per direction, then `enotconn` forever** — and
/// the close **drops** the output that can never reach the peer (the C's `tcp_clear_output`,
/// `inet_drv.c:12786`).
#[test]
fn latching_the_peer_close_drops_the_queued_output_and_answers_closed_once() {
    let (mut rig, client) = Rig::connected();
    let queued = fill_output_queue(&mut rig);
    assert!(queued > 0, "the output queue is stalled");

    // The peer half-closes: the driver's read finds end-of-file, which is what latches the
    // close. (`Shutdown::Write` keeps the peer reachable, so the driver must see a clean EOF
    // rather than a reset.)
    client
        .shutdown(Shutdown::Write)
        .expect("the peer ends its write side");

    let deadline = Instant::now() + WAIT;
    let reply = loop {
        let reply = rig.control("recv", b"").expect("the recv verb");
        if reply != ctl_ok() {
            break reply;
        }
        assert!(Instant::now() < deadline, "the close was never observed");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(
        reply,
        ctl_xerror("closed"),
        "the read that found the end-of-file answers `closed` itself"
    );
    assert!(rig.driver.peer_closed(), "the accessor the runtime reads");
    assert_eq!(rig.driver.pending_shutdown, None);
    assert_eq!(
        rig.output_len(),
        0,
        "the {queued} queued bytes can never reach the peer, so they are dropped"
    );

    // Each direction owes its one-shot `closed` and then `enotconn`.
    assert_eq!(
        rig.control("recv", b"").expect("the recv verb"),
        ctl_xerror("enotconn")
    );
    assert_eq!(
        rig.control("send", b"x").expect("the send verb"),
        ctl_xerror("closed")
    );
    assert_eq!(
        rig.control("send", b"x").expect("the send verb"),
        ctl_xerror("enotconn")
    );
}

/// A `shutdown` on a port that is not connected is the C's operational error, for **every**
/// direction — with no socket at all, and with a socket that simply has no peer.
#[test]
fn a_shutdown_on_a_not_connected_port_is_enotconn() {
    let mut rig = Rig::new(TcpInetDriver::new());
    rig.start().expect("a socket-less driver registers nothing");

    for how in [TCP_SHUT_RD, TCP_SHUT_WR, TCP_SHUT_RDWR] {
        assert_eq!(
            rig.control("shutdown", &[how]).expect("the verb"),
            ctl_xerror("enotconn"),
            "`shutdown` with `how` = {how} and no socket"
        );
    }

    // A bare `open` leaves a socket in `Open`, which is still not connected.
    assert_eq!(
        rig.control("open", &open_arg()).expect("the open verb"),
        ctl_ok()
    );
    assert_eq!(rig.driver.state(), InetState::Open);
    for how in [TCP_SHUT_RD, TCP_SHUT_WR, TCP_SHUT_RDWR] {
        assert_eq!(
            rig.control("shutdown", &[how]).expect("the verb"),
            ctl_xerror("enotconn"),
            "`shutdown` with `how` = {how} on an unconnected socket"
        );
    }
}

// -----------------------------------------------------------------------
// The oracle cross-check (skips loudly when `erl` is absent)
// -----------------------------------------------------------------------

/// Runs `<expr>, halt(0).` under the system `erl` and returns its stdout, or `None` — after
/// printing a clear skip message — when `erl` cannot be spawned or does not answer within
/// [`ERL_TIMEOUT`]. A spawned `erl` that exits non-zero is a bug in the expression, so it fails
/// the test.
///
/// A local copy of the helper the `self::opt` suite and `tests/inet.rs` each keep: a
/// `#[cfg(test)]` module's private helpers are not reachable across those boundaries, and the
/// crate's convention is to skip loudly rather than fail when the oracle is unavailable.
fn erl_stdout(expr: &str) -> Option<String> {
    let code = format!("{expr}, halt(0).");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(
            Command::new("erl")
                .arg("-noshell")
                .arg("-eval")
                .arg(&code)
                .output(),
        );
    });

    match rx.recv_timeout(ERL_TIMEOUT) {
        Err(_) => {
            eprintln!("skipping oracle check: `erl` did not answer within {ERL_TIMEOUT:?}");
            None
        }
        Ok(Err(err)) => {
            eprintln!("skipping oracle check: could not spawn `erl` ({err}); is Erlang on PATH?");
            None
        }
        Ok(Ok(out)) => {
            assert!(
                out.status.success(),
                "the oracle expression failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            Some(String::from_utf8_lossy(&out.stdout).into_owned())
        }
    }
}

/// The Erlang side of the half-close cross-check: a real `gen_tcp` pair over loopback, the
/// server's write side shut down, a `gen_tcp:recv` that **still** returns the peer's bytes, the
/// peer's end-of-file, and then the peer close with the two reasons after it — printed as
/// `Shut|R1|E|R2|R3`.
///
/// `gen_tcp` is used rather than a raw `open_port({spawn_driver, "tcp_inet"})` because the C's
/// `INET_REQ_ACCEPT` makes a *second* port for the accepted socket (`inet_drv.c:12236`), which the
/// Erlang side hides; the observable `{ok, Bytes}`/`{error, closed}`/`{error, enotconn}` results
/// are exactly what this floor's `[1]`/`[0] ++ name` replies encode.
fn oracle_half_close_expression() -> String {
    "{ok, L} = gen_tcp:listen(0, [{ip, {127,0,0,1}}, binary, {active, false}]), \
     {ok, P} = inet:port(L), \
     {ok, C} = gen_tcp:connect({127,0,0,1}, P, [binary, {active, false}]), \
     {ok, S} = gen_tcp:accept(L), \
     Shut = gen_tcp:shutdown(S, write), \
     ok = gen_tcp:send(C, <<\"hello\">>), \
     R1 = gen_tcp:recv(S, 5, 5000), \
     E = gen_tcp:recv(C, 0, 5000), \
     R2 = gen_tcp:recv(S, 0, 5000), \
     R3 = gen_tcp:recv(S, 0, 5000), \
     io:format(\"~p|~p|~p|~p|~p~n\", [Shut, R1, E, R2, R3])"
        .to_string()
}

/// The **compatibility gate** for the half-close, the deferred shutdown and the latched peer
/// close: the same sequence run against the real `inet` stack must produce the results this
/// driver's replies encode.
///
/// The oracle pins that a write-side `shutdown` leaves the socket readable (`{ok, <<"hello">>}`),
/// that the peer sees it as end-of-file (`{error, closed}`), and that the peer close is reported
/// as `closed` once and `enotconn` after that.
#[test]
fn the_half_close_and_peer_close_semantics_agree_with_the_erl_oracle() {
    let Some(stdout) = erl_stdout(&oracle_half_close_expression()) else {
        return; // the oracle is unavailable; the check skipped loudly above
    };
    let Some(line) = stdout.lines().find(|line| line.contains('|')) else {
        panic!("could not parse the oracle reply dump: {stdout:?}");
    };
    let oracle: Vec<&str> = line.trim().split('|').collect();
    eprintln!("oracle half-close sequence: {oracle:?}");
    assert_eq!(
        oracle,
        vec![
            "ok",
            "{ok,<<\"hello\">>}",
            "{error,closed}",
            "{error,closed}",
            "{error,enotconn}",
        ],
        "the real inet stack's own sequence"
    );

    // The driver runs the identical sequence, step for step.
    let (mut rig, mut client) = Rig::connected();
    client
        .set_read_timeout(Some(WAIT))
        .expect("a read timeout");

    // `Shut` — `gen_tcp:shutdown(S, write)` -> `ok` is this floor's `[1]`.
    assert_eq!(
        rig.control("shutdown", &[TCP_SHUT_WR]).expect("the verb"),
        ctl_ok(),
        "the oracle's `ok`"
    );

    // `R1` — the socket is still readable, so the peer's bytes arrive: the oracle's
    // `{ok, <<"hello">>}`.
    client.write_all(b"hello").expect("the peer writes");
    let deadline = Instant::now() + WAIT;
    while rig.input_len() < 5 {
        assert!(Instant::now() < deadline, "the half-closed driver read nothing");
        assert_eq!(rig.control("recv", b"").expect("the recv verb"), ctl_ok());
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        rig.take_input(),
        b"hello",
        "the oracle's `{{ok, <<\"hello\">>}}`"
    );

    // `E` — the peer observes the half-close as end-of-file: the oracle's `{error, closed}`.
    // (`gen_tcp` closes its socket when that `recv` reports `closed`, so the peer's end of the
    // connection is what the driver must observe next.)
    let mut eof = [0u8; 1];
    assert_eq!(
        client.read(&mut eof).expect("the peer reads its end-of-file"),
        0,
        "the oracle's `{{error,closed}}` on the peer's next read"
    );

    // `R2`/`R3` — the peer is gone: `closed` once, then `enotconn`.
    client
        .shutdown(Shutdown::Write)
        .expect("the peer's end of the connection goes away");
    let deadline = Instant::now() + WAIT;
    let reply = loop {
        let reply = rig.control("recv", b"").expect("the recv verb");
        if reply != ctl_ok() {
            break reply;
        }
        assert!(Instant::now() < deadline, "the close was never observed");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(reply, ctl_xerror("closed"), "the oracle's `{{error,closed}}`");
    assert_eq!(
        rig.control("recv", b"").expect("the recv verb"),
        ctl_xerror("enotconn"),
        "the oracle's `{{error,enotconn}}`"
    );
}
