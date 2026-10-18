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
//! The **TCP accept chain at the driver boundary** — the acceptance gate for contract A.4's
//! `gen_tcp:accept/2` data path, driven end-to-end through a real [`PortRegistry`] port and real
//! [`Poller`] rounds, with a **foreign peer** (`std::net::TcpStream` in the test process).
//!
//! # What this suite proves
//!
//! The in-module suite (`src/drivers/inet/accept_tests.rs`) pins the driver's own accept state
//! machine with a `TcpInetDriver` in hand. Here the driver is only reachable through the port the
//! runtime owns, so what is pinned is exactly what `beam_rt` observes (each test's own rustdoc
//! carries its full rationale):
//!
//! * **The whole chain, with a peer that is not a port** — reply-is-only-the-id, one `Accepted
//!   { fd }` completion, *both* of the runtime's adoption routes (`from_socket` + `open`, and the
//!   served `INET_REQ_FDOPEN`), then a real two-way byte path on the adopted connection.
//! * **The listener keeps accepting** — its own ids, *distinct* descriptors, an unchanged
//!   [`PortId`] that is still `INET_STATE_LISTENING`, and readiness that follows the accept list.
//! * **The accept deadline** — one 50 ms accept completes `{error, timeout}` exactly once and the
//!   port stays a live listener.
//! * **Two queued accepts** — ids in enqueue order (an accept never answers `ealready`) and the C's
//!   descriptor-state bits `9` → `41` → `297` → `9`.
//! * **The refusals and the teardown** — `[0]einval` / `[0]exbadport`, and a pending accept drained
//!   `{error, closed}` exactly once when its listener closes.
//! * **The channel stayed frozen** — wildcard-free `match`es over [`PortNotification`] and
//!   [`CompletionEvent`], so a new variant breaks the build instead of silently re-meaning an arm.
//!
//! # The discipline (the recipe `beam_rt`'s main loop runs)
//!
//! 1. A round is `Poller::poll` → [`PortRegistry::handle_events`] (which schedules **and** runs the
//!    readiness tasks) → [`PortRegistry::execute_port_tasks`] (the `Driver::drive_async` point and
//!    the `TIMEOUT` task's run point).
//! 2. **After every round** (and after every `close`) drain [`PortRegistry::take_completions`]: the
//!    drain is destructive, so an undrained completion is lost, exactly like an input byte.
//!
//! # Hermetic and fast
//!
//! Loopback only, no fixed port (the kernel assigns one), and no wall-clock sleep used as a
//! correctness device: every condition is awaited by pumping the reactor under one hang guard
//! ([`WAIT`]). The `erl` oracle cases **skip loudly** (`SKIP:` banner + early return, the crate's
//! policy) when the oracle is unavailable or exceeds [`ERL_TIMEOUT`]; every row they assert was
//! measured on the system driver first (the constant expressions at the bottom of this file carry
//! the probe that produced them).
//!
//! ```text
//! measured (OTP 29.1.1 / erts-17.1, this host)
//! bare tcp_inet accept   [0] ++ "einval"
//! bare udp_inet accept   [0] ++ "exbadport"
//! open udp_inet accept   [0] ++ "exbadport"
//! listener getstatus     [1,0,0,0,9] → [1,0,0,0,41] → [1,0,0,1,41]
//! accept replies         [1,0,0] then [1,0,1]
//! single 50 ms accept    reply [1,0,0], completion {0,{error,timeout}}
//! gen_tcp:accept(L, 50)  {error,timeout}
//! gen_tcp:accept(closed) {error,closed}
//! gen_tcp:accept(conn)   {error,einval}
//! ```
//!
//! # Why one file
//!
//! The six cases share one harness (the port plumbing, the acceptance helpers, the two adoption
//! routes) and one oracle probe set; splitting them would duplicate that vocabulary across test
//! binaries. This is the crate's documented "comprehensive test module" exception to the
//! ~1000-line target, like `tests/spawn.rs` and `tests/abi_driver.rs`.
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use beam_common::BeamError;
use beam_io::drivers::inet::{
    INET_AF_INET, INET_AF_LOOPBACK, INET_PROTO_DEFAULT, INET_REP_ERROR, INET_REP_OK,
    INET_TYPE_DGRAM, INET_TYPE_STREAM,
};
use beam_io::prelude::*;
use beam_process::PortId;
/// Wall-clock budget for one `erl` oracle before the affected check is skipped.
const ERL_TIMEOUT: Duration = Duration::from_secs(30);
/// The upper bound a condition wait may take before the test fails — a hang guard, never a
/// wall-clock budget tuned for an idle host.
const WAIT: Duration = Duration::from_secs(10);
/// How long one round blocks in the reactor when nothing is ready.
const POLL_SLICE: Duration = Duration::from_millis(20);
/// The C request numbers this suite drives (`inet_drv.c:805`-`:847`).
const INET_REQ_OPEN: u32 = 1;
const INET_REQ_NAME: u32 = 5;
const INET_REQ_BIND: u32 = 6;
const INET_REQ_FDOPEN: u32 = 13;
const INET_REQ_GETSTATUS: u32 = 16;
const INET_REQ_ACCEPT: u32 = 26;
const INET_REQ_LISTEN: u32 = 27;
const TCP_REQ_RECV: u32 = 42;
/// `ERTS_INET_DRV_CONTROL_MAGIC_NUMBER` (`erts/emulator/beam/sys.h:1466`): what
/// `erlang:port_control/3` adds to every opcode, and what the drivers subtract again.
const CONTROL_MAGIC: u32 = 0x03f1_a300;
/// The C's `INET_INFINITY` (`inet_drv.c:1043`), the 4-byte timeout operand meaning "no deadline".
const INET_INFINITY: i32 = -1;
/// A listener with no accept queued: `INET_STATE_LISTENING` = `OPEN | INET_F_LISTEN` = `1 | 8`.
/// Measured on the oracle as `[1,0,0,0,9]`.
const LISTENING_STATUS: u32 = 9;
/// One queued accept: `9 | INET_F_ACC` (`0x20`) = `0x29`. Measured on the oracle as `41`.
const ONE_ACCEPT_STATUS: u32 = 41;
/// Two or more queued accepts: `9 | INET_F_ACC | INET_F_MULTI_CLIENT` (`0x100`) = `0x129`.
/// Measured on the oracle as `297`.
const MULTI_ACCEPT_STATUS: u32 = 297;
/// `INET_STATE_CONNECTED` = `OPEN | INET_F_ACTIVE` = `1 | 4`. Measured on the oracle as
/// `[1,0,0,0,5]` for an adopted (connected) descriptor.
const CONNECTED_STATUS: u32 = 5;
/// A golden `term_to_binary(Mref, [local])` image — the 26-byte caller tag `prim_inet:send/4`
/// prefixes onto a command (`erts/preloaded/src/prim_inet.erl:575`-`:620`). This crate carries a
/// tag verbatim and never decodes it, so only the shape matters (`131` = `VERSION_MAGIC`,
/// `121` = `NEWER_REFERENCE_EXT`); `tests/inet_async.rs` proves an oracle-generated tag crosses
/// the same way, which is why no second oracle probe for it is run here.
const GOLDEN_TAG: [u8; 26] = [
    131, 121, 254, 153, 216, 200, 90, 0, 3, 106, 0, 0, 0, 0, 0, 0, 11, 218, 194, 188, 0, 11, 244,
    41, 135, 150,
];
// ---------------------------------------------------------------------------
// The runtime's port plumbing
// ---------------------------------------------------------------------------
fn poller() -> Poller {
    Poller::new().expect("a poll set")
}
/// The packed `INET_REQ_OPEN` argument for a stream socket: `[AF_INET, SOCK_STREAM, default]`.
fn open_arg() -> [u8; 3] {
    [INET_AF_INET, INET_TYPE_STREAM, INET_PROTO_DEFAULT]
}
/// The packed `INET_REQ_OPEN` argument for a datagram socket.
fn dgram_open_arg() -> [u8; 3] {
    [INET_AF_INET, INET_TYPE_DGRAM, INET_PROTO_DEFAULT]
}
/// The packed `bind` argument naming `127.0.0.1:port` (the C's three-byte form; a zero port asks
/// the kernel for an ephemeral one).
fn loopback_arg(port: u16) -> Vec<u8> {
    let [hi, lo] = port.to_be_bytes();
    vec![INET_AF_LOOPBACK, hi, lo]
}
/// Opens a `"tcp_inet"` port **through the built-in registry** — the way the runtime resolves
/// `open_port({spawn_driver, "tcp_inet"}, …)`.
///
/// Every port of a test lives in the **same** [`PortRegistry`]: port numbers are that registry's
/// single id space and the poller keys are those numbers, so two registries could hand out the same
/// key for two live ports.
fn open_tcp(ports: &mut PortRegistry, poller: &mut Poller) -> PortId {
    let registry = DriverRegistry::with_builtins();
    let driver = registry
        .create("tcp_inet")
        .expect("the built-in tcp_inet driver is registered");
    ports.open(poller, driver).expect("the port opens")
}
/// Opens a `"udp_inet"` port through the built-in registry.
fn open_udp(ports: &mut PortRegistry, poller: &mut Poller) -> PortId {
    let registry = DriverRegistry::with_builtins();
    let driver = registry
        .create("udp_inet")
        .expect("the built-in udp_inet driver is registered");
    ports.open(poller, driver).expect("the port opens")
}
/// One control op through the **numeric** opcode route — what `erlang:port_control/3` sends.
fn control(
    ports: &mut PortRegistry,
    poller: &mut Poller,
    id: PortId,
    code: u32,
    arg: &[u8],
) -> Vec<u8> {
    let op = (u64::from(code) + u64::from(CONTROL_MAGIC)).to_string();
    ports
        .port_control(poller, id, &op, arg)
        .unwrap_or_else(|err| panic!("the {code} control op failed: {err:?}"))
}
/// The C's `ctl_ok()` reply: `[INET_REP_OK]`.
fn ctl_ok() -> Vec<u8> {
    vec![INET_REP_OK]
}
/// The C's `ctl_xerror(name)` reply: the `[INET_REP_ERROR]` tag followed by the lowercase name
/// (`inet_drv.c:2307`).
fn ctl_xerror(name: &str) -> Vec<u8> {
    let mut reply = vec![INET_REP_ERROR];
    reply.extend_from_slice(name.as_bytes());
    reply
}
/// The `Timeout(4)` operand of `INET_REQ_ACCEPT`(26) (`inet_drv.c:12160`).
fn accept_operand(timeout_ms: i32) -> Vec<u8> {
    timeout_ms.to_be_bytes().to_vec()
}
/// The `Timeout(4), Length(4)` operand of `TCP_REQ_RECV`(42) (`inet_drv.c:12293`).
fn recv_operand(timeout_ms: i32, length: u32) -> Vec<u8> {
    let mut arg = timeout_ms.to_be_bytes().to_vec();
    arg.extend_from_slice(&length.to_be_bytes());
    arg
}
/// The `INET_REQ_FDOPEN`(13) operand: `[af, type, fd_be32, bound_be32]` (`inet_ctl_fdopen`,
/// `inet_drv.c:5223`).
fn fdopen_operand(fd: i32) -> Vec<u8> {
    let mut operand = vec![INET_AF_INET, INET_TYPE_STREAM];
    operand.extend_from_slice(&fd.to_be_bytes());
    operand.extend_from_slice(&1i32.to_be_bytes());
    operand
}
/// The descriptor-state bits `INET_REQ_GETSTATUS`(16) reports.
fn status_of(ports: &mut PortRegistry, poller: &mut Poller, id: PortId) -> u32 {
    let reply = control(ports, poller, id, INET_REQ_GETSTATUS, b"");
    let [tag, a, b, c, d] = reply[..] else {
        panic!("a getstatus reply is `[1] ++ be32(state)`, got {reply:?}");
    };
    assert_eq!(tag, INET_REP_OK, "the getstatus result tag, got {reply:?}");
    u32::from_be_bytes([a, b, c, d])
}
/// Creates the socket, binds it to an ephemeral loopback port, **reports that endpoint through
/// `INET_REQ_NAME`**, listens, and returns the port the kernel assigned.
///
/// `NAME` is the runtime's way of learning the address it is listening on; asserting it here is
/// what lets the foreign peer connect to a port nobody had to hard-code.
fn listen_on_loopback(poller: &mut Poller, ports: &mut PortRegistry, id: PortId) -> u16 {
    assert_eq!(
        control(ports, poller, id, INET_REQ_OPEN, &open_arg()),
        ctl_ok(),
        "INET_REQ_OPEN(1) creates the socket"
    );
    let bound = control(ports, poller, id, INET_REQ_BIND, &loopback_arg(0));
    assert_eq!(bound.len(), 3, "`bind` replies `[1, port_hi, port_lo]`, got {bound:?}");
    assert_eq!(bound[0], INET_REP_OK, "`bind` succeeds, got {bound:?}");
    let port = u16::from_be_bytes([bound[1], bound[2]]);
    assert_ne!(port, 0, "the kernel assigned an ephemeral port");
    let name = control(ports, poller, id, INET_REQ_NAME, b"");
    assert_eq!(
        name,
        vec![INET_REP_OK, INET_AF_INET, bound[1], bound[2], 127, 0, 0, 1],
        "INET_REQ_NAME(5) reports the endpoint the socket is bound to"
    );
    assert_eq!(
        control(ports, poller, id, INET_REQ_LISTEN, &[0, 5]),
        ctl_ok(),
        "INET_REQ_LISTEN(27)"
    );
    port
}
/// One runtime round: poll, dispatch readiness (which also runs the readiness tasks), run the port
/// tasks — the round's `drive_async` and `TIMEOUT` run point — then drain the completion channel.
#[derive(Debug)]
struct Round {
    completions: Vec<PortCompletion>,
    notifications: Vec<PortNotification>,
}
fn round(poller: &mut Poller, ports: &mut PortRegistry) -> Round {
    let events = match poller.poll(Some(POLL_SLICE)) {
        Ok(events) => events,
        Err(BeamError::Io(err)) if err.kind() == std::io::ErrorKind::Interrupted => Vec::new(),
        Err(err) => panic!("poll failed: {err:?}"),
    };
    let mut notifications = ports
        .handle_events(poller, &events)
        .expect("dispatch the ready events")
        .notifications;
    notifications.extend(
        ports
            .execute_port_tasks(poller, DEFAULT_READINESS_RED_BUDGET)
            .expect("run the port tasks")
            .notifications,
    );
    Round {
        completions: ports.take_completions(),
        notifications,
    }
}
/// Pumps rounds until **each** port in `wanted` has produced a completion, returning every
/// completion the rounds reported (so an early completion for another port is never lost).
fn await_completions(
    poller: &mut Poller,
    ports: &mut PortRegistry,
    wanted: &[PortId],
    what: &str,
) -> Vec<PortCompletion> {
    let deadline = Instant::now() + WAIT;
    let mut got: Vec<PortCompletion> = Vec::new();
    while wanted
        .iter()
        .any(|port| !got.iter().any(|done| done.port == *port))
    {
        assert!(Instant::now() < deadline, "{what}: only {got:?} arrived");
        got.extend(round(poller, ports).completions);
    }
    got
}
/// The single completion of `completions`, panicking otherwise.
fn only_completion(completions: Vec<PortCompletion>, what: &str) -> PortCompletion {
    assert_eq!(completions.len(), 1, "{what}: {completions:?}");
    completions.into_iter().next().expect("one completion")
}
/// The `(id, fd)` of an accept completion, panicking on any other shape.
fn accepted(event: CompletionEvent, what: &str) -> (u16, i32) {
    match event {
        CompletionEvent::InetAsync {
            id,
            result: InetAsyncResult::Accepted { fd },
        } => (id, fd),
        other => panic!("{what}: the accept completion is a descriptor hand-off, got {other:?}"),
    }
}
/// The [`ReadMessage`] of a data completion, panicking on any other shape.
fn data_of(event: CompletionEvent, what: &str) -> ReadMessage {
    match event {
        CompletionEvent::InetAsync {
            result: InetAsyncResult::Data(read),
            ..
        } => read,
        other => panic!("{what}: expected a data completion, got {other:?}"),
    }
}
/// Asserts that **no** completion is staged for `budget`, while the reactor keeps being pumped.
fn assert_no_completion_within(
    poller: &mut Poller,
    ports: &mut PortRegistry,
    budget: Duration,
    what: &str,
) {
    let deadline = Instant::now() + budget;
    loop {
        let round = round(poller, ports);
        assert!(round.completions.is_empty(), "{what}: {:?}", round.completions);
        if Instant::now() >= deadline {
            return;
        }
    }
}
/// Adopts an accepted descriptor the way `beam_rt` may: `TcpInetDriver::from_socket` + a second
/// port in the same registry. The driver **owns** the number from here on (its `stop`/`Drop`
/// closes it), which is why the caller never closes it too.
fn adopt_with_from_socket(ports: &mut PortRegistry, poller: &mut Poller, fd: i32) -> PortId {
    let driver = TcpInetDriver::from_socket(fd).expect("the handed-over descriptor is adopted");
    assert_eq!(driver.socket(), Some(fd), "the adopted driver owns the descriptor");
    ports
        .open(poller, Box::new(driver))
        .expect("the second port opens")
}
/// Adopts an accepted descriptor the way `beam_rt` may instead: the **served** `INET_REQ_FDOPEN`
/// (13) on a fresh, socket-less `"tcp_inet"` port.
///
/// The adoption is `prebound`, so the driver never closes the caller's number — the caller closes
/// it once the port is gone.
fn adopt_with_fdopen(ports: &mut PortRegistry, poller: &mut Poller, fd: i32) -> PortId {
    let id = open_tcp(ports, poller);
    assert_eq!(
        control(ports, poller, id, INET_REQ_FDOPEN, &fdopen_operand(fd)),
        ctl_ok(),
        "INET_REQ_FDOPEN(13) adopts the handed-over descriptor"
    );
    assert_eq!(
        status_of(ports, poller, id),
        CONNECTED_STATUS,
        "an adopted connection is INET_STATE_CONNECTED"
    );
    id
}
// ---------------------------------------------------------------------------
// The chain: accept, adopt, read, write — against a foreign peer
// ---------------------------------------------------------------------------
/// **The whole accept chain, with a peer that is not a port**: reply-is-only-the-id, one
/// `Accepted { fd }` completion, both of the runtime's adoption routes, and then a real two-way
/// data path on the adopted connection.
///
/// Everything a `gen_tcp:accept/2` caller depends on is here: the id the accept answered is the id
/// the completion echoes, the descriptor is a *connected* stream socket (`GETSTATUS` `5`), the
/// bytes the peer wrote arrive as **one** [`ReadMessage`], and a command written through
/// [`PortRegistry::port_command`] reaches the peer with its `prim_inet` caller prefix consumed and
/// answered by an `InetReply { Ok }` completion.
#[test]
fn the_whole_accept_chain_runs_with_a_foreign_peer() {
    let mut poller = poller();
    let mut ports = PortRegistry::new();
    let listener = open_tcp(&mut ports, &mut poller);
    let bound = listen_on_loopback(&mut poller, &mut ports, listener);
    assert_eq!(
        status_of(&mut ports, &mut poller, listener),
        LISTENING_STATUS,
        "`listen` leaves a bare listener (`INET_STATE_LISTENING`)"
    );

    // The accept's reply is the id and nothing else: the outcome needs a round.
    assert_eq!(
        control(
            &mut ports,
            &mut poller,
            listener,
            INET_REQ_ACCEPT,
            &accept_operand(INET_INFINITY)
        ),
        vec![1, 0, 0],
        "INET_REQ_ACCEPT(26) answers `[1] ++ <<Id:16/big>>`"
    );
    assert!(
        ports.take_completions().is_empty(),
        "the accept's outcome never rides its control reply"
    );

    // The **foreign peer**: an ordinary `std::net::TcpStream` in this test's process.
    let mut peer = TcpStream::connect(("127.0.0.1", bound)).expect("the foreign peer connects");
    let done = only_completion(
        await_completions(&mut poller, &mut ports, &[listener], "the queued accept completes"),
        "the queued accept completes",
    );
    assert_eq!(done.port, listener, "the completion names the listening port");
    let (id, fd) = accepted(done.event, "the queued accept completes");
    assert_eq!(id, 0, "the completion echoes the id its reply handed out");
    assert!(fd >= 0, "the completion carries a real descriptor: {fd}");

    // Adoption route 1 of 2: `TcpInetDriver::from_socket` + `PortRegistry::open`.
    let connection = adopt_with_from_socket(&mut ports, &mut poller, fd);
    assert_eq!(
        status_of(&mut ports, &mut poller, connection),
        CONNECTED_STATUS,
        "the adopted descriptor is a connected stream socket"
    );

    // Read: queue the operation first, so the peer's bytes complete it (rather than being taken by
    // the passive path into the port's input queue).
    assert_eq!(
        control(
            &mut ports,
            &mut poller,
            connection,
            TCP_REQ_RECV,
            &recv_operand(INET_INFINITY, 0)
        ),
        vec![1, 0, 0],
        "the connection's own id counter starts at 0"
    );
    peer.write_all(b"hello from the peer").expect("the peer writes");
    let read = data_of(
        only_completion(
            await_completions(
                &mut poller,
                &mut ports,
                &[connection],
                "the connection's receive completes",
            ),
            "the connection's receive completes",
        )
        .event,
        "the connection's receive completes",
    );
    assert_eq!(
        read,
        ReadMessage::body_only(b"hello from the peer".to_vec()),
        "one header-less read message carrying exactly the peer's bytes"
    );

    // Write: a `prim_inet` command is `[n:be16][tag][payload]`; only the payload reaches the peer,
    // and the tag comes back verbatim on the reply completion.
    let mut command = Vec::new();
    command.extend_from_slice(&(GOLDEN_TAG.len() as u16).to_be_bytes());
    command.extend_from_slice(&GOLDEN_TAG);
    command.extend_from_slice(b"hello back");
    ports
        .port_command(&mut poller, connection, &command, PortCommandOptions::new())
        .expect("the command is queued");
    let reply = only_completion(
        await_completions(&mut poller, &mut ports, &[connection], "the command answers"),
        "the command answers",
    );
    assert_eq!(reply.port, connection, "the reply names the writing port");
    assert_eq!(
        reply.event,
        CompletionEvent::InetReply {
            caller_ref: Some(GOLDEN_TAG.to_vec()),
            reply: InetReply::Ok,
        },
        "the caller tag crosses verbatim and the reply is `ok`"
    );
    let mut got = [0u8; 10];
    peer.read_exact(&mut got).expect("the foreign peer reads the payload");
    assert_eq!(&got, b"hello back", "the caller's tag is not written to the socket");

    // Adoption route 2 of 2: the **served** `INET_REQ_FDOPEN`(13) on a fresh port, since the
    // runtime may equally well take that route with the next descriptor it is handed.
    ports.close(&mut poller, connection).expect("closes the first connection");
    let mut peer2 = TcpStream::connect(("127.0.0.1", bound)).expect("a second foreign peer");
    assert_eq!(
        control(
            &mut ports,
            &mut poller,
            listener,
            INET_REQ_ACCEPT,
            &accept_operand(INET_INFINITY)
        ),
        vec![1, 0, 1],
        "the listener's accept id counter keeps advancing across operations"
    );
    let done = only_completion(
        await_completions(
            &mut poller,
            &mut ports,
            &[listener],
            "the second accept completes",
        ),
        "the second accept completes",
    );
    let (id2, fd2) = accepted(done.event, "the second accept completes");
    assert_eq!(id2, 1, "the second accept's completion echoes its own id");
    assert_ne!(fd2, fd, "two connections are two descriptors");
    let adopted = adopt_with_fdopen(&mut ports, &mut poller, fd2);
    assert_eq!(
        control(
            &mut ports,
            &mut poller,
            adopted,
            TCP_REQ_RECV,
            &recv_operand(INET_INFINITY, 0)
        ),
        vec![1, 0, 0],
        "the adopted port's own id counter starts at 0"
    );
    peer2.write_all(b"route two").expect("the second peer writes");
    let read = data_of(
        only_completion(
            await_completions(&mut poller, &mut ports, &[adopted], "the adopted receive completes"),
            "the adopted receive completes",
        )
        .event,
        "the adopted receive completes",
    );
    assert_eq!(
        read.flattened(),
        b"route two".to_vec(),
        "both adoption routes reach the same data path"
    );

    ports.close(&mut poller, adopted).expect("closes the adopted port");
    // A `prebound` adoption never closes the caller's number: the port only de-registers it
    // (`erl_inet_close`, `inet_drv.c:5102`-`:5117`), so the consumer owns the last close.
    close_fd(fd2).expect("the prebound descriptor is still open for its owner");
    ports.close(&mut poller, listener).expect("closes the listener");
}
// ---------------------------------------------------------------------------
// The listener keeps accepting
// ---------------------------------------------------------------------------
/// **The listener survives and keeps accepting**, and its readiness follows the accept list: a
/// connection that arrives with **no** accept queued produces no completion of its own, yet the
/// next enqueue re-arms the listener and that same pending connection is accepted by a plain
/// `poll` + `handle_events` round.
///
/// This is the C's `sock_select(FD_ACCEPT, 1)/0` discipline (`FD_ACCEPT` *is* `ERL_DRV_READ`,
/// `inet_drv.c:631`) observed from outside the driver: no accessor is read, only the round's
/// outcome, because `Port::read_armed` is crate-private.
#[test]
fn the_listener_keeps_accepting_and_reports_a_drained_list() {
    let mut poller = poller();
    let mut ports = PortRegistry::new();
    let listener = open_tcp(&mut ports, &mut poller);
    let bound = listen_on_loopback(&mut poller, &mut ports, listener);

    // A peer that connects while **nothing** is queued is not an accept: the listener's readiness
    // is not an accept operation, so no completion may be staged.
    let mut peer_a = TcpStream::connect(("127.0.0.1", bound)).expect("the first foreign peer");
    assert_no_completion_within(
        &mut poller,
        &mut ports,
        POLL_SLICE * 5,
        "a connection with no queued accept stages nothing",
    );

    // Queue the accept: the enqueue re-arms accept readiness, which re-reports the connection that
    // is already in the kernel's backlog — no explicit drive is needed, a readiness round suffices.
    assert_eq!(
        control(
            &mut ports,
            &mut poller,
            listener,
            INET_REQ_ACCEPT,
            &accept_operand(INET_INFINITY)
        ),
        vec![1, 0, 0],
        "the queued accept answers id 0"
    );
    let done = only_completion(
        await_completions(&mut poller, &mut ports, &[listener], "the pending connection is accepted"),
        "the pending connection is accepted",
    );
    let (id_a, fd_a) = accepted(done.event, "the pending connection is accepted");
    assert_eq!(id_a, 0, "the completion echoes the accept's id");
    let first = adopt_with_from_socket(&mut ports, &mut poller, fd_a);
    assert_eq!(
        status_of(&mut ports, &mut poller, listener),
        LISTENING_STATUS,
        "a drained accept list is a bare listener again"
    );

    // A second peer gets its own id and its own descriptor, and the listening port is unchanged.
    let mut peer_b = TcpStream::connect(("127.0.0.1", bound)).expect("the second foreign peer");
    assert_eq!(
        control(
            &mut ports,
            &mut poller,
            listener,
            INET_REQ_ACCEPT,
            &accept_operand(INET_INFINITY)
        ),
        vec![1, 0, 1],
        "the second accept answers the next id"
    );
    let done = only_completion(
        await_completions(&mut poller, &mut ports, &[listener], "the second connection is accepted"),
        "the second connection is accepted",
    );
    assert_eq!(done.port, listener, "still the same listening port");
    let (id_b, fd_b) = accepted(done.event, "the second connection is accepted");
    assert_eq!(id_b, 1, "the second completion echoes its own id");
    assert_ne!(fd_b, fd_a, "each connection is its own descriptor");
    let second = adopt_with_from_socket(&mut ports, &mut poller, fd_b);

    assert!(ports.contains(listener), "the listener is still a live port");
    assert_eq!(
        status_of(&mut ports, &mut poller, listener),
        LISTENING_STATUS,
        "the listener is still a listener after accepting twice"
    );

    // Both accepted connections are usable, and each keeps its own byte stream.
    for port in [first, second] {
        assert_eq!(
            control(
                &mut ports,
                &mut poller,
                port,
                TCP_REQ_RECV,
                &recv_operand(INET_INFINITY, 0)
            ),
            vec![1, 0, 0],
            "each adopted connection queues its own receive"
        );
    }
    peer_a.write_all(b"alpha").expect("the first peer writes");
    peer_b.write_all(b"beta").expect("the second peer writes");
    let completions =
        await_completions(&mut poller, &mut ports, &[first, second], "both receives complete");
    let mut alpha = None;
    let mut beta = None;
    for completion in completions {
        if completion.port == first {
            alpha = Some(data_of(completion.event, "the first connection's data"));
        } else if completion.port == second {
            beta = Some(data_of(completion.event, "the second connection's data"));
        } else {
            panic!("an unexpected completion: {completion:?}");
        }
    }
    assert_eq!(
        alpha.expect("the first connection's read").flattened(),
        b"alpha".to_vec(),
        "the first connection carries only its own peer's bytes"
    );
    assert_eq!(
        beta.expect("the second connection's read").flattened(),
        b"beta".to_vec(),
        "the second connection carries only its own peer's bytes"
    );

    for port in [first, second, listener] {
        ports.close(&mut poller, port).expect("closes the port");
    }
}
// ---------------------------------------------------------------------------
// The accept deadline
// ---------------------------------------------------------------------------
/// **A due accept deadline completes exactly once `{error, timeout}`, through the registry.**
///
/// The reply is only the id; the outcome is a completion staged by the port's `TIMEOUT` task (armed
/// from `Driver::next_timeout`, run by [`PortRegistry::execute_port_tasks`] once the deadline has
/// passed) — the C's `tcp_inet_multi_timeout` answering `async_error(am_timeout)`
/// (`inet_drv.c:12600`-`:12616`). The port stays a live listener and its accept list drains.
///
/// The `erl` half of this case measured the oracle row this suite reproduces: reply `[1,0,0]`, then
/// `{inet_async, Port, 0, {error, timeout}}` — one accept queued, so no id accounting to unwind.
#[test]
fn an_accept_deadline_completes_exactly_once_through_the_registry() {
    /// A ~50 ms accept deadline: long enough that the reply is always answered first, short enough
    /// that a handful of 20 ms poll slices reach it.
    const DEADLINE_MS: i32 = 50;

    let mut poller = poller();
    let mut ports = PortRegistry::new();
    let listener = open_tcp(&mut ports, &mut poller);
    listen_on_loopback(&mut poller, &mut ports, listener);

    assert_eq!(
        control(
            &mut ports,
            &mut poller,
            listener,
            INET_REQ_ACCEPT,
            &accept_operand(DEADLINE_MS)
        ),
        vec![1, 0, 0],
        "a timed accept answers its id and nothing else"
    );
    assert!(
        ports.take_completions().is_empty(),
        "the deadline's outcome never rides the control reply"
    );

    let done = only_completion(
        await_completions(&mut poller, &mut ports, &[listener], "the accept deadline is reported"),
        "the accept deadline is reported",
    );
    assert_eq!(done.port, listener, "the deadline completion names its own port");
    assert_eq!(
        done.event,
        CompletionEvent::InetAsync {
            id: 0,
            result: InetAsyncResult::Error(InetReason::Timeout),
        },
        "the deadline completes the accept {{error, timeout}} with its own id"
    );
    assert_eq!(
        InetReason::Timeout.atom_name(),
        "timeout",
        "the reason renders as the oracle's atom"
    );

    // Exactly once: further rounds report nothing for this port.
    assert_no_completion_within(
        &mut poller,
        &mut ports,
        POLL_SLICE * 5,
        "a completed deadline is not re-reported",
    );
    assert!(ports.contains(listener), "the port is still a live listener");
    assert_eq!(
        status_of(&mut ports, &mut poller, listener),
        LISTENING_STATUS,
        "the drained accept list is a bare listener again"
    );
    assert!(ports.contains(listener), "still live after the status probe");

    // The oracle row: the same one-accept sequence on the system driver.
    let Some(stdout) = erl_stdout(TIMEOUT_EXPRESSION) else {
        return; // the oracle is unavailable; the check skipped loudly above
    };
    eprintln!("oracle accept-deadline row: {}", stdout.trim());
    let reply = oracle_field(&stdout, "timeout_reply=").unwrap_or_else(|| {
        panic!("the oracle reported no accept reply: {stdout:?}");
    });
    let reply: Vec<u8> = reply
        .split(',')
        .map(|byte| byte.trim().parse::<u8>().expect("an oracle reply byte"))
        .collect();
    let id = oracle_field(&stdout, "timeout_id=").expect("the oracle's completion id");
    let reason = oracle_field(&stdout, "timeout_reason=").expect("the oracle's completion reason");
    assert_eq!(reply, vec![1, 0, 0], "the oracle's timed accept answers `[1,0,0]`");
    assert_eq!(id, "0", "the oracle's completion echoes the id its reply handed out");
    assert_eq!(reason, "timeout", "the oracle's deadline reason is `timeout`");

    ports.close(&mut poller, listener).expect("closes the listener");
}
// ---------------------------------------------------------------------------
// Two queued accepts: ids in enqueue order, and the descriptor-state bits
// ---------------------------------------------------------------------------
/// **Two queued accepts answer their own ids in enqueue order and report the C's state bits**, and
/// both connections are really accepted — one peer per accept.
///
/// `INET_REQ_ACCEPT` joins the C's multi-operation list (`enq_multi_op`, `inet_drv.c:12194`), so a
/// repeat never answers `ealready`; `INET_REQ_GETSTATUS`(16) reports `INET_F_ACC` while one is
/// queued and adds `INET_F_MULTI_CLIENT` from two on (measured on the oracle: `9` → `41` → `297`,
/// and back to `9` once the list drains).
#[test]
fn two_queued_accepts_answer_their_ids_in_order_and_report_the_status_bits() {
    let mut poller = poller();
    let mut ports = PortRegistry::new();
    let listener = open_tcp(&mut ports, &mut poller);
    let bound = listen_on_loopback(&mut poller, &mut ports, listener);

    assert_eq!(
        status_of(&mut ports, &mut poller, listener),
        LISTENING_STATUS,
        "a bare listener reports INET_STATE_LISTENING"
    );
    for (index, expected) in [(0u16, ONE_ACCEPT_STATUS), (1u16, MULTI_ACCEPT_STATUS)] {
        assert_eq!(
            control(
                &mut ports,
                &mut poller,
                listener,
                INET_REQ_ACCEPT,
                &accept_operand(INET_INFINITY)
            ),
            vec![1, (index >> 8) as u8, index as u8],
            "accept #{index} answers its own id — an accept never answers `ealready`"
        );
        assert_eq!(
            status_of(&mut ports, &mut poller, listener),
            expected,
            "the accept list of {} operation(s) is reported in the state bits",
            index + 1
        );
    }

    // One peer per queued accept, in enqueue order.
    let mut peer_a = TcpStream::connect(("127.0.0.1", bound)).expect("the first foreign peer");
    let done = only_completion(
        await_completions(&mut poller, &mut ports, &[listener], "the first accept completes"),
        "the first accept completes",
    );
    let (id_a, fd_a) = accepted(done.event, "the first accept completes");
    assert_eq!(id_a, 0, "the first completion takes the first id");
    let mut peer_b = TcpStream::connect(("127.0.0.1", bound)).expect("the second foreign peer");
    let done = only_completion(
        await_completions(&mut poller, &mut ports, &[listener], "the second accept completes"),
        "the second accept completes",
    );
    let (id_b, fd_b) = accepted(done.event, "the second accept completes");
    assert_eq!(id_b, 1, "the second completion takes the second id");
    assert_ne!(fd_a, fd_b, "each accept hands over its own descriptor");
    assert_eq!(
        status_of(&mut ports, &mut poller, listener),
        LISTENING_STATUS,
        "the drained list reports INET_STATE_LISTENING again"
    );

    // Both descriptors are usable: each carries its own peer's bytes.
    let first = adopt_with_from_socket(&mut ports, &mut poller, fd_a);
    let second = adopt_with_fdopen(&mut ports, &mut poller, fd_b);
    for port in [first, second] {
        assert_eq!(
            control(
                &mut ports,
                &mut poller,
                port,
                TCP_REQ_RECV,
                &recv_operand(INET_INFINITY, 0)
            ),
            vec![1, 0, 0],
            "each accepted connection queues its own receive"
        );
    }
    peer_a.write_all(b"one").expect("the first peer writes");
    peer_b.write_all(b"two").expect("the second peer writes");
    let completions =
        await_completions(&mut poller, &mut ports, &[first, second], "both accepts' data arrives");
    let mut one = None;
    let mut two = None;
    for completion in completions {
        if completion.port == first {
            one = Some(data_of(completion.event, "the first accept's data"));
        } else if completion.port == second {
            two = Some(data_of(completion.event, "the second accept's data"));
        } else {
            panic!("an unexpected completion: {completion:?}");
        }
    }
    assert_eq!(one.expect("the first read").flattened(), b"one".to_vec());
    assert_eq!(two.expect("the second read").flattened(), b"two".to_vec());

    // The whole row set, byte for byte against the system driver.
    let Some(stdout) = erl_stdout(DRIVER_ROWS_EXPRESSION) else {
        return; // the oracle is unavailable; the check skipped loudly above
    };
    eprintln!("oracle accept rows: {}", stdout.trim());
    let Some(oracle) = parse_reply_dump(&stdout) else {
        panic!("could not parse the oracle reply dump: {stdout:?}");
    };
    assert_eq!(oracle.len(), DRIVER_ROW_LABELS.len(), "one oracle reply per row: {oracle:?}");
    // The rows this case owns (the refusal rows belong to the sibling case).
    assert_eq!(
        oracle[3],
        status_row(LISTENING_STATUS),
        "the oracle's bare-listener row: {oracle:?}"
    );
    assert_eq!(oracle[4], vec![1, 0, 0], "the oracle's first accept row: {oracle:?}");
    assert_eq!(
        oracle[5],
        status_row(ONE_ACCEPT_STATUS),
        "the oracle's one-accept status row: {oracle:?}"
    );
    assert_eq!(oracle[6], vec![1, 0, 1], "the oracle's second accept row: {oracle:?}");
    assert_eq!(
        oracle[7],
        status_row(MULTI_ACCEPT_STATUS),
        "the oracle's multi-accept status row: {oracle:?}"
    );

    ports.close(&mut poller, second).expect("closes the second connection");
    ports.close(&mut poller, first).expect("closes the first connection");
    ports.close(&mut poller, listener).expect("closes the listener");
    // The `INET_REQ_FDOPEN` adoption was `prebound`: its descriptor was never closed for us.
    close_fd(fd_b).expect("the prebound descriptor is still open for its owner");
}
// ---------------------------------------------------------------------------
// Refusals and the teardown drain
// ---------------------------------------------------------------------------
/// **A port that cannot accept is refused, and a listener that closes with an accept pending drains
/// it `{error, closed}` exactly once.**
///
/// The `tcp_inet` guard is the C's combined "not listening **or** bad operand"
/// (`inet_drv.c:12160`), so it answers `[0]einval` in every non-listening state and never
/// `exbadport`; `INET_REQ_ACCEPT` is a case of `tcp_inet_ctl` only, so a real `udp_inet` port
/// answers `[0]exbadport` in every state. Both rows are byte-compared with the system driver below.
///
/// The teardown is the C's `async_error_am_all(am_closed)` (`:2597`-`:2608`) as the runtime sees it:
/// the completion is harvested into the registry at `close`, and the drain is exactly once.
#[test]
fn a_bare_or_closed_port_is_refused_and_a_pending_accept_drains_closed() {
    let mut poller = poller();
    let mut ports = PortRegistry::new();

    // A never-opened `tcp_inet` port, then a real `udp_inet` port before and after its `open`.
    let stream = open_tcp(&mut ports, &mut poller);
    let datagram = open_udp(&mut ports, &mut poller);
    let refusals = [
        (
            stream,
            "a bare tcp_inet port's accept answers `einval`",
            ctl_xerror("einval"),
        ),
        (
            datagram,
            "a bare udp_inet port's accept answers `exbadport`",
            ctl_xerror("exbadport"),
        ),
    ];
    for (port, what, expected) in refusals {
        assert_eq!(
            control(
                &mut ports,
                &mut poller,
                port,
                INET_REQ_ACCEPT,
                &accept_operand(INET_INFINITY)
            ),
            expected,
            "{what}"
        );
    }
    assert_eq!(
        control(&mut ports, &mut poller, datagram, INET_REQ_OPEN, &dgram_open_arg()),
        ctl_ok(),
        "the datagram socket opens"
    );
    assert_eq!(
        control(
            &mut ports,
            &mut poller,
            datagram,
            INET_REQ_ACCEPT,
            &accept_operand(INET_INFINITY)
        ),
        ctl_xerror("exbadport"),
        "an open udp_inet port's accept answers `exbadport` (its switch has no accept arm)"
    );
    assert!(
        ports.take_completions().is_empty(),
        "a refused accept stages no completion"
    );

    // A listener closed with an accept pending: the teardown drains it `{error, closed}`, once.
    let listener = open_tcp(&mut ports, &mut poller);
    listen_on_loopback(&mut poller, &mut ports, listener);
    assert_eq!(
        control(
            &mut ports,
            &mut poller,
            listener,
            INET_REQ_ACCEPT,
            &accept_operand(INET_INFINITY)
        ),
        vec![1, 0, 0],
        "the accept is queued"
    );
    assert!(ports.take_completions().is_empty(), "nothing has completed yet");
    ports.close(&mut poller, listener).expect("closes the listener");
    let drained = only_completion(
        ports.take_completions(),
        "the teardown drains the pending accept",
    );
    assert_eq!(drained.port, listener, "the dead port's completion still names it");
    assert_eq!(
        drained.event,
        CompletionEvent::InetAsync {
            id: 0,
            result: InetAsyncResult::Error(InetReason::Closed),
        },
        "the pending accept is completed {{error, closed}}"
    );
    assert_eq!(
        InetReason::Closed.atom_name(),
        "closed",
        "the reason renders as the oracle's atom"
    );
    assert!(
        ports.take_completions().is_empty(),
        "the teardown drain is exactly once"
    );
    let after = round(&mut poller, &mut ports);
    assert!(after.completions.is_empty(), "a round after the drain reports nothing: {after:?}");
    assert!(!ports.contains(listener), "the closed port left the registry");

    // The oracle rows this case owns: the two refusal shapes and the listener's status rows.
    let Some(stdout) = erl_stdout(DRIVER_ROWS_EXPRESSION) else {
        return; // the oracle is unavailable; the check skipped loudly above
    };
    eprintln!("oracle refusal rows: {}", stdout.trim());
    let Some(oracle) = parse_reply_dump(&stdout) else {
        panic!("could not parse the oracle reply dump: {stdout:?}");
    };
    assert_eq!(oracle.len(), DRIVER_ROW_LABELS.len(), "one oracle reply per row: {oracle:?}");
    assert_eq!(oracle[0], ctl_xerror("einval"), "the oracle's bare tcp_inet row");
    assert_eq!(oracle[1], ctl_xerror("exbadport"), "the oracle's bare udp_inet row");
    assert_eq!(oracle[2], ctl_xerror("exbadport"), "the oracle's open udp_inet row");

    // The contract-level shape those rows translate into for a `gen_tcp:accept/2` caller, measured
    // on the system runtime (it is `beam_rt`'s rendering, so it is pinned here, not reproduced).
    let Some(stdout) = erl_stdout(GEN_TCP_EXPRESSION) else {
        return;
    };
    eprintln!("oracle gen_tcp:accept rows: {}", stdout.trim());
    for (key, expected) in [
        ("gen_accept_timeout=", "timeout"),
        ("gen_accept_closed=", "closed"),
        ("gen_accept_connected=", "einval"),
    ] {
        let value = oracle_field(&stdout, key)
            .unwrap_or_else(|| panic!("the oracle reported no `{key}` row: {stdout:?}"));
        assert_eq!(
            value, expected,
            "the oracle answers `gen_tcp:accept` with `{{error,{expected}}}`"
        );
    }
}
// ---------------------------------------------------------------------------
// The channel's shape
// ---------------------------------------------------------------------------
/// **The two side channels stayed frozen**: [`PortNotification`] has exactly `Data`/`Closed` and
/// [`CompletionEvent`] exactly `InetReply`/`InetAsync`.
///
/// Both matches below have no wildcard, so a new variant stops the suite compiling. That is the
/// invariant that lets `beam_rt` match both enums exhaustively: the accept chain rides the
/// completion channel ([`CompletionEvent`]), and the `{Port, {data, …}}`/port-exit enum keeps its
/// two arms — the active-mode delivery channel another agent owns must not need a third variant
/// here. A real notification of each kind is classified through the match, so neither arm is dead.
#[test]
fn the_completion_channel_stayed_frozen() {
    fn classify_notification(notification: &PortNotification) -> &'static str {
        match notification {
            PortNotification::Data { .. } => "data",
            PortNotification::Closed { port: _ } => "closed",
        }
    }
    fn classify_completion(event: &CompletionEvent) -> &'static str {
        match event {
            CompletionEvent::InetReply { .. } => "inet_reply",
            CompletionEvent::InetAsync { .. } => "inet_async",
        }
    }

    // A real `Closed` notification: a port that has just closed is reported once.
    let mut poller = poller();
    let mut ports = PortRegistry::new();
    let listener = open_tcp(&mut ports, &mut poller);
    let bound = listen_on_loopback(&mut poller, &mut ports, listener);
    let mut peer = TcpStream::connect(("127.0.0.1", bound)).expect("a foreign peer connects");
    assert_eq!(
        control(
            &mut ports,
            &mut poller,
            listener,
            INET_REQ_ACCEPT,
            &accept_operand(INET_INFINITY)
        ),
        vec![1, 0, 0],
        "the accept is queued"
    );
    let done = only_completion(
        await_completions(&mut poller, &mut ports, &[listener], "the accept completes"),
        "the accept completes",
    );
    assert_eq!(classify_completion(&done.event), "inet_async", "a real accept completion");
    let (_, fd) = accepted(done.event, "the accept completes");
    let connection = adopt_with_from_socket(&mut ports, &mut poller, fd);
    // A real `Data` notification: bytes with no pending receive take the passive path into the
    // port's input queue (`{Port, {data, …}}`), never the completion channel.
    peer.write_all(b"plain").expect("the peer writes");
    let deadline = Instant::now() + WAIT;
    let mut seen = Vec::new();
    while seen.is_empty() {
        assert!(Instant::now() < deadline, "no data notification arrived");
        seen.extend(round(&mut poller, &mut ports).notifications);
    }
    assert_eq!(seen.len(), 1, "one read message: {seen:?}");
    match &seen[0] {
        PortNotification::Data { port, read } => {
            assert_eq!(*port, connection, "the notification names the connection");
            assert_eq!(read.flattened(), b"plain".to_vec(), "the peer's bytes");
        }
        other => panic!("the passive path reports a Data: {other:?}"),
    }
    assert_eq!(classify_notification(&seen[0]), "data");
    ports.close(&mut poller, connection).expect("closes the connection");
    let closed = round(&mut poller, &mut ports).notifications;
    assert_eq!(closed.len(), 1, "the close is reported once: {closed:?}");
    assert_eq!(classify_notification(&closed[0]), "closed");
    // The other completion arm has no meaning without a command; it is classified as a value.
    assert_eq!(
        classify_completion(&CompletionEvent::InetReply {
            caller_ref: None,
            reply: InetReply::Ok,
        }),
        "inet_reply"
    );
    ports.close(&mut poller, listener).expect("closes the listener");
}
// ---------------------------------------------------------------------------
// The `erl` oracle
// ---------------------------------------------------------------------------
/// Runs one Erlang expression on the system ERTS, returning its stdout.
///
/// An unavailable oracle — `erl` not on `PATH`, or an answer slower than [`ERL_TIMEOUT`] — is a
/// **loud skip**, never a failure; a *failing* expression is a real failure (the case is wrong).
fn erl_stdout(expr: &str) -> Option<String> {
    let code = format!("{expr}, halt(0).");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let out = Command::new("erl")
            .arg("-noshell")
            .arg("-eval")
            .arg(&code)
            .output();
        let _ = tx.send(out);
    });
    match rx.recv_timeout(ERL_TIMEOUT) {
        Err(_) => {
            eprintln!("SKIP: the oracle check skipped: `erl` did not answer within {ERL_TIMEOUT:?}");
            None
        }
        Ok(Err(err)) => {
            eprintln!("SKIP: the oracle check skipped: could not spawn `erl` ({err})");
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
/// Parses the oracle's `a,b,c|d,e|…` reply dump into one byte vector per reply.
fn parse_reply_dump(stdout: &str) -> Option<Vec<Vec<u8>>> {
    let line = stdout.lines().find(|line| line.contains('|'))?;
    let mut replies = Vec::new();
    for field in line.trim().split('|') {
        let mut bytes = Vec::new();
        for part in field.split(',') {
            if part.trim().is_empty() {
                // An empty reply is a legal answer.
                continue;
            }
            bytes.push(part.trim().parse::<u8>().ok()?);
        }
        replies.push(bytes);
    }
    Some(replies)
}
/// The value of a `key=value` field on an oracle output line.
fn oracle_field<'a>(stdout: &'a str, key: &str) -> Option<&'a str> {
    let start = stdout.find(key)? + key.len();
    let rest = &stdout[start..];
    let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
    Some(&rest[..end])
}
/// The `INET_REQ_GETSTATUS` reply for `state`: `[1] ++ be32(state)`.
fn status_row(state: u32) -> Vec<u8> {
    let mut reply = ctl_ok();
    reply.extend_from_slice(&state.to_be_bytes());
    reply
}
/// The labels of [`DRIVER_ROWS_EXPRESSION`]'s rows, in order.
const DRIVER_ROW_LABELS: [&str; 8] = [
    "bare tcp_inet accept",
    "bare udp_inet accept",
    "open udp_inet accept",
    "getstatus, no accept queued",
    "accept #1",
    "getstatus, one accept queued",
    "accept #2",
    "getstatus, two accepts queued",
];
/// The driver rows this suite compares byte-for-byte, one `|`-separated dump per run.
///
/// Rows 0-2 are the refusal shapes (a bare `tcp_inet` port, and a `udp_inet` port before and after
/// its `open`); rows 3-7 are one listener's `getstatus`/`accept`/`getstatus`/`accept`/`getstatus`
/// sequence, so the ids (`0`, `1`) and the state bits (`9`, `41`, `297`) come from the driver
/// itself. Measured (this host): `0,101,105,110,118,97,108` = `[0]"einval"`,
/// `0,101,120,98,97,100,112,111,114,116` = `[0]"exbadport"`, then
/// `1,0,0,0,9 | 1,0,0 | 1,0,0,0,41 | 1,0,1 | 1,0,0,1,41`.
const DRIVER_ROWS_EXPRESSION: &str = "M = 16#03f1a300, \
 P = open_port({spawn_driver, \"tcp_inet\"}, [binary]), \
 F = fun(C, A) -> binary_to_list(iolist_to_binary(erlang:port_control(P, C + M, A))) end, \
 BARE = F(26, <<-1:32>>), \
 Q = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
 G = fun(C, A) -> binary_to_list(iolist_to_binary(erlang:port_control(Q, C + M, A))) end, \
 UBARE = G(26, <<-1:32>>), \
 _ = G(1, <<1,2,0>>), \
 UOPEN = G(26, <<-1:32>>), \
 _ = F(1, <<1,1,0>>), \
 _ = F(6, <<4,0,0>>), \
 _ = F(27, <<0,5>>), \
 ST0 = F(16, <<>>), \
 A1 = F(26, <<-1:32>>), \
 ST1 = F(16, <<>>), \
 A2 = F(26, <<-1:32>>), \
 ST2 = F(16, <<>>), \
 S = fun(Ls) -> lists:flatten(lists:join(\",\", [integer_to_list(B) || B <- Ls])) end, \
 io:format(\"~s~n\", [lists:flatten(lists:join(\"|\", [S(X) || X <- [BARE,UBARE,UOPEN,ST0,A1,ST1,A2,ST2]]))])";
/// The accept-deadline row: one listener, **one** 50 ms accept, its reply and the completion it
/// owes. Measured (this host): `timeout_reply=1,0,0 timeout_id=0 timeout_reason=timeout` — the
/// reply's id is the completion's id, and the reason is the `{error, timeout}` reason atom of the
/// `{inet_async, Port, 0, {error, timeout}}` message. (A probe with *several* queued accepts is
/// deliberately not used here: the oracle's multi-timer errors out the **head** of a caller's
/// accept queue — `remove_multi_op` matches the caller, `inet_drv.c:12606` — so its id would not be
/// the id of the operation whose deadline passed.)
const TIMEOUT_EXPRESSION: &str = "M = 16#03f1a300, \
 P = open_port({spawn_driver, \"tcp_inet\"}, [binary]), \
 F = fun(C, A) -> binary_to_list(iolist_to_binary(erlang:port_control(P, C + M, A))) end, \
 _ = F(1, <<1,1,0>>), \
 _ = F(6, <<4,0,0>>), \
 _ = F(27, <<0,5>>), \
 REPLY = F(26, <<50:32>>), \
 DRAIN = fun Dr() -> receive {inet_async, _, I, R} -> {I, R}; _ -> Dr() after 3000 -> none end end, \
 {Id, Reason} = case DRAIN() of {I0, {error, R0}} -> {I0, R0}; Other -> Other end, \
 S = fun(Ls) -> lists:flatten(lists:join(\",\", [integer_to_list(B) || B <- Ls])) end, \
 io:format(\"timeout_reply=~s timeout_id=~p timeout_reason=~p~n\", [S(REPLY), Id, Reason])";
/// The contract-level `gen_tcp:accept/2` shapes the rows above translate into, measured on the
/// system runtime: an expired accept, an accept on a **closed** listener, and an accept on a
/// **connected** (non-listening) socket.
///
/// Rendering those rows is `beam_rt`'s job, so this probe pins the oracle's answers rather than
/// comparing them with anything here — it is the recorded target of the driver rows this suite
/// pins (`[0]einval` ⇔ `{error,einval}`, the timeout completion ⇔ `{error,timeout}`).
const GEN_TCP_EXPRESSION: &str = "M = 16#03f1a300, \
 P = open_port({spawn_driver, \"tcp_inet\"}, [binary]), \
 F = fun(C, A) -> binary_to_list(iolist_to_binary(erlang:port_control(P, C + M, A))) end, \
 _ = F(1, <<1,1,0>>), \
 BP = case F(6, <<4,0,0>>) of [1, BH, BL] -> BH * 256 + BL; _ -> exit(no_bind) end, \
 _ = F(27, <<0,5>>), \
 {ok, L} = gen_tcp:listen(0, [{ip,{127,0,0,1}}, binary, {active,false}]), \
 {error, Timeout} = gen_tcp:accept(L, 50), \
 ok = gen_tcp:close(L), \
 {error, Closed} = gen_tcp:accept(L), \
 {ok, C} = gen_tcp:connect({127,0,0,1}, BP, [binary, {active,false}]), \
 {error, Connected} = gen_tcp:accept(C), \
 io:format(\"gen_accept_timeout=~p gen_accept_closed=~p gen_accept_connected=~p~n\", \
           [Timeout, Closed, Connected])";
