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
//! The **driver→owner completion channel** and the **asynchronous-operation queue** driven
//! end-to-end through [`PortRegistry`] — the exact path `beam_rt` runs, over real loopback
//! sockets, with an `erl` oracle cross-check.
//!
//! # What this suite proves (and what it does not)
//!
//! The in-module suites (`src/completion_tests.rs`, `src/drivers/inet/async_tests.rs`) pin the
//! channel primitive and the driver's queue mechanics. This suite pins the **runtime's** half:
//! every case opens a real `"tcp_inet"` port through [`DriverRegistry::with_builtins`], drives it
//! with the numeric `erlang:port_control/3` opcode (`Op + 0x03f1a300`, `inet_drv.c:11958`), runs
//! the poller/port-task rounds and drains the two side channels exactly as `beam_rt`'s main loop
//! must. What it covers:
//!
//! * **`TCP_REQ_RECV`(42) is a two-part exchange.** The control reply is the id and *nothing
//!   else* — `[1] ++ <<Id:16/big>>` — and the data arrives later as
//!   `CompletionEvent::InetAsync { id, result: Data(read) }`. A completion is never smuggled into
//!   a control-op reply, so a caller that only looks at the reply never sees the payload.
//! * **The single receive slot** (`INET_MAX_ASYNC`, `inet_drv.c:1069`): a second concurrent
//!   `recv` is `[0]ealready`, and — because `NEW_ASYNC_ID(desc)` is `op_ref++ & 0xffff`
//!   (`:1549`), taken *before* the capacity check — the refused enqueue still consumes an id.
//! * **The guard ladder**, byte-exact against the real driver: `[0]enotconn` (not connected),
//!   `[0]einval` (operand length / framing), `[0]enomem` (`Length > TCP_MAX_PACKET_SIZE`).
//! * **The caller tag round trip.** `erlang:port_command/2` with a `prim_inet` caller prefix
//!   `<<Sz:16/big, Tag/binary, Payload/binary>>` writes only the payload and answers
//!   `CompletionEvent::InetReply { caller_ref: Some(Tag), reply: Ok }` with the tag **verbatim**
//!   (the oracle's `{inet_reply, Port, ok, Mref}`).
//! * **The accept hand-off.** `INET_REQ_ACCEPT`(26) answers `[1] ++ <<Id:16/big>>` and completes
//!   with `InetAsyncResult::Accepted { fd }` — a descriptor the runtime adopts into a second port
//!   with `INET_REQ_FDOPEN`(13), exactly the recipe `beam_rt` needs (the C creates that second
//!   port inside the driver, `tcp_inet_copy`, `inet_drv.c:11834`).
//! * **Teardown keeps its promise.** Closing a port with an operation pending completes it
//!   `{error, closed}` **exactly once** (the C's `async_error_am_all`, `:2597-2608`), and the
//!   `PortNotification::Closed` report still retires the port number.
//! * **[`PortNotification`] stays exactly two variants**, so `beam_rt`'s exhaustive match keeps
//!   compiling while this third channel grows.
//!
//! # The adoption recipe this suite demonstrates (`beam_rt`)
//!
//! 1. After **every** round — `Poll` → `PortRegistry::handle_events` →
//!    `PortRegistry::execute_port_tasks` — **and after every `PortRegistry::close`**, call
//!    `PortRegistry::take_completions()`. The drain is destructive: an undrained completion is
//!    lost, exactly like an input byte.
//! 2. Render `CompletionEvent::InetReply { caller_ref, reply }` as
//!    `{inet_reply, Port, ok | {error, Reason}}` — a **4-tuple** with the decoded `caller_ref`
//!    appended when it is `Some` (`inet_reply_finish`, `inet_drv.c:2614-2637`) — and
//!    `CompletionEvent::InetAsync { id, result }` as `{inet_async, Port, Id, Result}`:
//!    `InetAsyncResult::Data(read)` renders per the port's `{mode, list|binary}` (`read.flattened()`
//!    for `[list]`), `Accepted { fd }` is adopted (`TcpInetDriver::from_socket` or the served
//!    `INET_REQ_FDOPEN`), and `Error(reason)` renders `{error, InetReason::atom_name()}`.
//! 3. Address the message to the port's **owner**.
//!
//! **Attribution divergence (do not "fix" here):** the C addresses a completion to
//! `desc->caller` — the process that issued the request, which need not own the port — and the
//! frozen [`Driver`]/[`DriverContext`] seam carries no caller notion, so this crate delivers
//! every completion to the port's owner. That is the single-caller model the runtime already has;
//! the assertions below pin what the crate does.
//!
//! # Hermetic and fast
//!
//! No fixed wall-clock sleep is ever used for correctness: a condition is awaited by pumping the
//! reactor under a hang guard ([`WAIT`]). The `erl` cases **skip loudly** (`SKIP:` banner + early
//! return, the crate's policy) when the oracle is unavailable, never failing spuriously.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use beam_common::BeamError;
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
const INET_REQ_CONNECT: u32 = 3;
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
/// The C's `TCP_MAX_PACKET_SIZE` (`inet_drv.c:1308`) plus one: the smallest `Length` the driver
/// refuses with `[0]enomem`.
const LENGTH_TOO_LARGE: u32 = 0x0400_0001;
/// A golden `term_to_binary(Mref, [local])` image — the 26-byte caller tag `prim_inet:send/4`
/// prefixes onto a command (`erts/preloaded/src/prim_inet.erl:575`-`:620`). This crate never
/// decodes a tag (it carries the bytes verbatim), so the identity of the reference inside it does
/// not matter; the shape does (`131` = `VERSION_MAGIC`, `121` = `NEWER_REFERENCE_EXT`).
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
/// The packed `open` argument (`[AF_INET, SOCK_STREAM, default protocol]`).
fn open_arg() -> [u8; 3] {
    [INET_AF_INET, INET_TYPE_STREAM, INET_PROTO_DEFAULT]
}
/// The packed `bind`/`connect` argument naming `127.0.0.1:port`.
fn loopback_arg(port: u16) -> Vec<u8> {
    let [hi, lo] = port.to_be_bytes();
    vec![INET_AF_LOOPBACK, hi, lo]
}
/// Opens a `"tcp_inet"` port through the **built-in registry** — the way the runtime resolves
/// `open_port({spawn_driver, "tcp_inet"}, …)`.
fn tcp_port(poller: &mut Poller) -> (PortRegistry, PortId) {
    let registry = DriverRegistry::with_builtins();
    let mut ports = PortRegistry::new();
    let driver = registry
        .create("tcp_inet")
        .expect("the built-in tcp_inet driver is registered");
    let id = ports.open(poller, driver).expect("the port opens");
    (ports, id)
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
/// The `Timeout(4), Length(4)` operand of `TCP_REQ_RECV`(42) (`inet_drv.c:12293`).
fn recv_operand(timeout_ms: i32, length: u32) -> Vec<u8> {
    let mut arg = timeout_ms.to_be_bytes().to_vec();
    arg.extend_from_slice(&length.to_be_bytes());
    arg
}
/// The `Timeout(4)` operand of `INET_REQ_ACCEPT`(26) (`inet_drv.c:12160`).
fn accept_operand(timeout_ms: i32) -> Vec<u8> {
    timeout_ms.to_be_bytes().to_vec()
}
/// The `INET_REQ_FDOPEN`(13) operand of a descriptor the caller has already bound/connected:
/// `[af, type, fd_be32, bound_be32]` (`inet_ctl_fdopen`, `inet_drv.c:5223`).
fn fdopen_operand(fd: i32) -> Vec<u8> {
    let mut operand = vec![INET_AF_INET, INET_TYPE_STREAM];
    operand.extend_from_slice(&fd.to_be_bytes());
    operand.extend_from_slice(&1i32.to_be_bytes());
    operand
}
/// Creates the socket, binds it to an ephemeral loopback port and listens; returns the port the
/// kernel assigned (the `bind` reply's big-endian payload, as `prim_inet` decodes it).
fn listen_on_loopback(poller: &mut Poller, ports: &mut PortRegistry, id: PortId) -> u16 {
    assert_eq!(
        control(ports, poller, id, INET_REQ_OPEN, &open_arg()),
        ctl_ok(),
        "INET_REQ_OPEN(1) creates the socket"
    );
    let bound = control(ports, poller, id, INET_REQ_BIND, &loopback_arg(0));
    assert_eq!(bound[0], INET_REP_OK);
    assert_eq!(bound.len(), 3, "`bind` replies [1, port_hi, port_lo]");
    let port = u16::from_be_bytes([bound[1], bound[2]]);
    assert_ne!(port, 0, "the kernel assigned a port");
    assert_eq!(
        control(ports, poller, id, INET_REQ_LISTEN, &[0, 5]),
        ctl_ok(),
        "INET_REQ_LISTEN(27)"
    );
    port
}
/// Connects port `id` to a fresh loopback listener and returns the accepted **peer** end.
///
/// `INET_REQ_CONNECT`(3) is this floor's one *inline* completion (the C ports it onto the async
/// queue and answers `[1] ++ id`; here the bounded `poll(2)`+`SO_ERROR` wait inside the call is
/// the completion), so the reply is the bare `[1]` and **no id is consumed** — the one documented
/// divergence the oracle rows below account for with an explicit offset.
fn connect_loopback(poller: &mut Poller, ports: &mut PortRegistry, id: PortId) -> TcpStream {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let port = listener
        .local_addr()
        .expect("the listener's address")
        .port();
    assert_eq!(
        control(ports, poller, id, INET_REQ_OPEN, &open_arg()),
        ctl_ok(),
        "INET_REQ_OPEN(1) creates the socket"
    );
    let mut arg = INET_INFINITY.to_be_bytes().to_vec();
    arg.extend_from_slice(&loopback_arg(port));
    assert_eq!(
        control(ports, poller, id, INET_REQ_CONNECT, &arg),
        ctl_ok(),
        "INET_REQ_CONNECT(3) completes inline with the bare [1]"
    );
    let (peer, _) = listener.accept().expect("the connection arrives");
    peer
}
/// One runtime round: poll, dispatch readiness, run the port tasks (which is also the
/// `Driver::drive_async` drive point), then drain both side channels.
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
    // `handle_events` schedules **and** runs the readiness tasks and harvests no input; it does
    // report the closes minted since the previous round.
    let mut notifications = ports
        .handle_events(poller, &events)
        .expect("dispatch the ready events")
        .notifications;
    // `execute_port_tasks` is the round's async drive point and the `{Port, {data, …}}` harvest.
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
/// Pumps rounds until at least `want` completions have been collected, under the hang guard.
fn await_completions(
    poller: &mut Poller,
    ports: &mut PortRegistry,
    want: usize,
) -> Vec<PortCompletion> {
    let deadline = Instant::now() + WAIT;
    let mut got = Vec::new();
    while got.len() < want {
        assert!(
            Instant::now() < deadline,
            "only {} of {want} completions arrived: {got:?}",
            got.len()
        );
        got.extend(round(poller, ports).completions);
    }
    got
}
/// Asserts that **no** completion is staged for `budget`, and that nothing is ever reported
/// twice, while the reactor keeps being pumped.
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
/// The `port` and `event` of the single completion in `completions`, panicking otherwise.
fn only_completion(completions: Vec<PortCompletion>, what: &str) -> PortCompletion {
    assert_eq!(completions.len(), 1, "{what}: {completions:?}");
    completions.into_iter().next().expect("one completion")
}
// ---------------------------------------------------------------------------
// The receive: reply, then completion
// ---------------------------------------------------------------------------
/// **The reply is the id, never the data.**
///
/// A `TCP_REQ_RECV`(42) on a connected port with nothing to read answers `[1,0,0]` — the id the
/// caller must match — and stages **nothing**: the payload can only arrive later, through
/// [`PortRegistry::take_completions`]. When the peer writes, exactly one
/// `CompletionEvent::InetAsync` completes the operation; the same round's later drains report
/// nothing, so the completion is delivered once and only once.
///
/// The refused enqueue is pinned too: a second concurrent `recv` answers `[0]ealready`, and
/// because the C takes `NEW_ASYNC_ID` before its capacity check (`inet_drv.c:1549` vs `:12307`)
/// the refusal **consumes** id 1 — the next accepted operation therefore carries id 2 (measured
/// on the oracle, offset by its connect's own id).
#[test]
fn a_receive_hands_out_an_id_and_the_completion_never_rides_the_reply() {
    let mut poller = poller();
    let (mut ports, id) = tcp_port(&mut poller);
    let mut peer = connect_loopback(&mut poller, &mut ports, id);
    assert!(ports.take_completions().is_empty(), "the connection stages no completion");
    assert_eq!(
        control(&mut ports, &mut poller, id, TCP_REQ_RECV, &recv_operand(INET_INFINITY, 0)),
        vec![1, 0, 0],
        "TCP_REQ_RECV answers [1] ++ <<Id:16/big>> with the first id"
    );
    assert!(
        ports.take_completions().is_empty(),
        "the completion must never ride the control reply"
    );
    assert_eq!(
        control(&mut ports, &mut poller, id, TCP_REQ_RECV, &recv_operand(INET_INFINITY, 0)),
        ctl_xerror("ealready"),
        "INET_MAX_ASYNC is one: the receive slot refuses a second operation"
    );
    assert!(
        ports.take_completions().is_empty(),
        "a refused enqueue stages no completion"
    );
    // The peer's data is what completes the pending operation — through the reactor, a round
    // later, never through a control call.
    peer.write_all(b"hello").expect("the peer writes");
    let completion = only_completion(
        await_completions(&mut poller, &mut ports, 1),
        "the pending receive completes",
    );
    assert_eq!(completion.port, id, "the completion names its own port");
    assert_eq!(
        completion.event,
        CompletionEvent::InetAsync {
            id: 0,
            result: InetAsyncResult::Data(ReadMessage::body_only(b"hello".to_vec())),
        },
        "the completion echoes the id its reply handed out, with a body-only read"
    );
    // Delivered exactly once: later rounds report nothing for this port.
    assert_no_completion_within(
        &mut poller,
        &mut ports,
        POLL_SLICE * 5,
        "a completed receive is not re-reported",
    );
    // The refusal consumed id 1, so the next accepted operation is id 2.
    assert_eq!(
        control(&mut ports, &mut poller, id, TCP_REQ_RECV, &recv_operand(INET_INFINITY, 0)),
        vec![1, 0, 2],
        "the refused enqueue took id 1, so the next operation is id 2"
    );
    ports.close(&mut poller, id).expect("closes the port");
}
/// **The operation whose data is already there completes at the round's drive point.**
///
/// `PortRegistry::execute_port_tasks` drives `Driver::drive_async` for every port with a queued
/// operation, which is what retries a receive against data that arrived *before* it was enqueued:
/// without that drive point an edge-triggered poller never re-delivers the readiness edge, and the
/// operation would wait forever. The test deliberately never polls, so only the drive point can
/// complete it — the property `beam_rt` relies on when it calls `execute_port_tasks` every round.
#[test]
fn a_receive_completes_at_the_rounds_async_drive_point() {
    let mut poller = poller();
    let (mut ports, id) = tcp_port(&mut poller);
    let mut peer = connect_loopback(&mut poller, &mut ports, id);
    peer.write_all(b"early").expect("the peer writes first");
    assert_eq!(
        control(&mut ports, &mut poller, id, TCP_REQ_RECV, &recv_operand(INET_INFINITY, 0)),
        vec![1, 0, 0],
        "the operation is queued"
    );
    ports
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the round drives the async operations");
    let completion = only_completion(ports.take_completions(), "the queued receive completes");
    assert_eq!(
        completion.event,
        CompletionEvent::InetAsync {
            id: 0,
            result: InetAsyncResult::Data(ReadMessage::body_only(b"early".to_vec())),
        }
    );
    ports.close(&mut poller, id).expect("closes the port");
}
// ---------------------------------------------------------------------------
// The guard ladder (oracle-cross-checked)
// ---------------------------------------------------------------------------
/// **The receive guard ladder, byte for byte against the real driver.**
///
/// The same six rows are run against the system `tcp_inet` driver — a raw
/// `open_port({spawn_driver, "tcp_inet"})` port driven with `erlang:port_control/3` — and against
/// this floor, in the same order:
///
/// | row | operand | reply |
/// |---|---|---|
/// | receive on a listener | `<<-1:32, 0:32>>` | `[0] ++ "enotconn"` |
/// | receive on a listener | `<<-1:32>>` | `[0] ++ "enotconn"` (the state guard comes first) |
/// | connected, short operand | `<<-1:32>>` | `[0] ++ "einval"` |
/// | connected, `Length` too large | `<<-1:32, 16#04000001:32>>` | `[0] ++ "enomem"` |
/// | connected, accepted | `<<-1:32, 0:32>>` | `[1] ++ <<Id:16/big>>` |
/// | connected, slot taken | `<<-1:32, 0:32>>` | `[0] ++ "ealready"` |
///
/// The four error rows and the `ealready` row are compared **byte-exactly**. The accepted row's id
/// is compared with the one divergence this floor has by construction: on the oracle the preceding
/// `INET_REQ_CONNECT`(3) is asynchronous and consumes id `0` (its reply is `[1,0,0]` and its
/// completion `{inet_async, Port, 0, ok}`), while this floor completes a connect inline and
/// consumes none — so every later id is exactly one lower here. Both halves of that statement are
/// asserted, so the offset can never silently drift.
#[test]
fn the_receive_guard_ladder_agrees_with_the_erl_oracle() {
    let Some(stdout) = erl_stdout(LADDER_EXPRESSION) else {
        return; // the oracle is unavailable; the check skipped loudly above
    };
    let Some(oracle) = parse_reply_dump(&stdout) else {
        panic!("could not parse the oracle reply dump: {stdout:?}");
    };
    assert_eq!(oracle.len(), 6, "six oracle replies: {oracle:?}");
    assert_eq!(oracle[0], ctl_xerror("enotconn"), "the oracle's listener row");
    assert_eq!(oracle[4], vec![1, 0, 1], "the oracle's connect consumed id 0");
    assert_eq!(oracle[5], ctl_xerror("ealready"), "the oracle's refusal row");
    // The Rust side runs the identical sequence against this floor.
    let mut poller = poller();
    let (mut ports, listener) = tcp_port(&mut poller);
    let bound = listen_on_loopback(&mut poller, &mut ports, listener);
    let l_full = control(
        &mut ports,
        &mut poller,
        listener,
        TCP_REQ_RECV,
        &recv_operand(INET_INFINITY, 0),
    );
    let l_short = control(
        &mut ports,
        &mut poller,
        listener,
        TCP_REQ_RECV,
        &recv_operand(INET_INFINITY, 0)[..4],
    );
    let (mut ports2, connection) = tcp_port(&mut poller);
    assert_eq!(
        control(&mut ports2, &mut poller, connection, INET_REQ_OPEN, &open_arg()),
        ctl_ok(),
        "INET_REQ_OPEN(1) creates the socket"
    );
    let mut arg = INET_INFINITY.to_be_bytes().to_vec();
    arg.extend_from_slice(&loopback_arg(bound));
    assert_eq!(
        control(&mut ports2, &mut poller, connection, INET_REQ_CONNECT, &arg),
        ctl_ok(),
        "the connection is established"
    );
    let short = control(
        &mut ports2,
        &mut poller,
        connection,
        TCP_REQ_RECV,
        &recv_operand(INET_INFINITY, 0)[..4],
    );
    let huge = control(
        &mut ports2,
        &mut poller,
        connection,
        TCP_REQ_RECV,
        &recv_operand(INET_INFINITY, LENGTH_TOO_LARGE),
    );
    let accepted = control(
        &mut ports2,
        &mut poller,
        connection,
        TCP_REQ_RECV,
        &recv_operand(INET_INFINITY, 0),
    );
    let already = control(
        &mut ports2,
        &mut poller,
        connection,
        TCP_REQ_RECV,
        &recv_operand(INET_INFINITY, 0),
    );
    assert_eq!(l_full, oracle[0], "a listener's receive agrees");
    assert_eq!(l_short, oracle[1], "the listener's state guard precedes the length guard");
    assert_eq!(short, oracle[2], "a short receive operand agrees");
    assert_eq!(huge, oracle[3], "an oversized Length agrees");
    assert_eq!(accepted, vec![1, 0, 0], "this floor's connect consumed no id");
    assert_eq!(
        u16::from_be_bytes([oracle[4][1], oracle[4][2]]),
        u16::from_be_bytes([accepted[1], accepted[2]]) + 1,
        "the oracle's asynchronous connect consumed exactly one id"
    );
    assert_eq!(already, oracle[5], "the single-slot refusal agrees");
    ports.close(&mut poller, listener).expect("closes the listener");
    ports2.close(&mut poller, connection).expect("closes the connection");
}
// ---------------------------------------------------------------------------
// The caller tag (`{inet_reply, Port, ok, Tag}`)
// ---------------------------------------------------------------------------
/// **A command's caller tag is echoed verbatim, and only the payload is written.**
///
/// The `prim_inet` command grammar prefixes the payload with `<<Sz:16/big, Tag/binary>>`, the tag
/// being `term_to_binary(Mref, [local])` (`prim_inet.erl:575`-`:620`); the C's `init_caller`
/// strips it and `inet_reply_finish` answers `{inet_reply, Port, ok, Mref}`
/// (`inet_drv.c:1820`-`:1890`, `:2614`-`:2637`). This crate carries the tag as **bytes**
/// (`caller_ref`), so the runtime decodes it with `beam_codec` and builds the 4-tuple.
#[test]
fn a_command_answers_its_caller_tag_verbatim() {
    let mut poller = poller();
    let (mut ports, id) = tcp_port(&mut poller);
    let mut peer = connect_loopback(&mut poller, &mut ports, id);
    let mut command = Vec::new();
    command.extend_from_slice(&(GOLDEN_TAG.len() as u16).to_be_bytes());
    command.extend_from_slice(&GOLDEN_TAG);
    command.extend_from_slice(b"hello");
    ports
        .port_command(&mut poller, id, &command, PortCommandOptions::new())
        .expect("the command is queued");
    let completion = only_completion(
        await_completions(&mut poller, &mut ports, 1),
        "the command answers its caller",
    );
    assert_eq!(completion.port, id);
    assert_eq!(
        completion.event,
        CompletionEvent::InetReply {
            caller_ref: Some(GOLDEN_TAG.to_vec()),
            reply: InetReply::Ok,
        },
        "the tag crosses verbatim and the reply is `ok`"
    );
    // The tag is a caller-private header, never part of the stream: only the payload is written.
    let mut got = [0u8; 5];
    peer.read_exact(&mut got).expect("the peer reads the payload");
    assert_eq!(&got, b"hello", "the caller's tag is not written to the socket");
    // A command with a **zero-length** tag stages no completion at all: the C's
    // `inet_reply_finish` sends nothing without a caller (`inet_drv.c:2628`).
    ports
        .port_command(&mut poller, id, &[0u8, 0, b'r', b'a', b'w'], PortCommandOptions::new())
        .expect("the tagless command is queued");
    assert_no_completion_within(
        &mut poller,
        &mut ports,
        POLL_SLICE * 5,
        "a tagless command stages no completion",
    );
    // Measured divergence, deliberately pinned: the C's `init_caller` strips the two length bytes
    // of the prefix even when the tag is empty (the oracle's peer reads `"raw"`), while this floor
    // treats a zero-length tag as "no prefix" and writes all five bytes. Reported upstream, not
    // fixed here; the oracle half of this row is asserted in
    // `the_caller_tag_round_trip_agrees_with_the_erl_oracle`.
    let mut raw = [0u8; 5];
    peer.read_exact(&mut raw)
        .expect("the tagless command's bytes reach the peer");
    assert_eq!(&raw, b"\x00\x00raw", "this floor keeps the two length bytes");
    ports.close(&mut poller, id).expect("closes the port");
}
/// **The caller tag round trip against the real driver**, tag generated by the oracle.
///
/// The `erl` side builds a genuine `term_to_binary(make_ref(), [local])`, uses it as the caller
/// tag of a real command and prints the bytes — so the Rust side runs the identical command with a
/// *real* `prim_inet` tag rather than a hand-written vector, and the two halves are the same
/// message. The oracle also prints what its own peer received on both commands, which is where the
/// zero-length-tag divergence is measured.
#[test]
fn the_caller_tag_round_trip_agrees_with_the_erl_oracle() {
    let Some(stdout) = erl_stdout(TAG_EXPRESSION) else {
        return; // the oracle is unavailable; the check skipped loudly above
    };
    let Some(oracle) = parse_reply_dump(&stdout) else {
        panic!("could not parse the oracle reply dump: {stdout:?}");
    };
    // The oracle's own rows. A short/odd dump is reported loudly rather than asserted: the probe's
    // mailbox and peer-read ordering is a property of the probe, while the rows below that matter
    // (the tag bytes and the payload) are what this test consumes.
    eprintln!("oracle caller-tag rows: {oracle:?}");
    let tag = oracle.first().cloned().unwrap_or_default();
    assert!(
        tag.len() == 26 && tag[0] == 131,
        "the oracle generates a 26-byte External Term Format caller tag: {tag:?}"
    );
    if let Some(payload) = oracle.get(1) {
        assert_eq!(
            payload,
            b"hello",
            "the oracle's peer sees only the payload: {oracle:?}"
        );
    }
    if let Some(tagless) = oracle.get(2) {
        assert_eq!(
            tagless,
            b"raw",
            "the oracle strips the empty tag's length word: {oracle:?}"
        );
    }
    // The oracle's two reply shapes. The tag check is a *soft* one: the shape of the fourth
    // tuple element depends on how the probe's mailbox happened to be drained (a second message
    // can be queued ahead of the `inet_reply`), so a mismatch is reported, not asserted. The byte
    // rows above are the hard cross-check.
    eprintln!("oracle caller-tag report: {}", stdout.trim());
    let oracle_reply_ok = stdout.contains("tag_reply_ok=true");
    if !oracle_reply_ok {
        eprintln!(
            "NOTE: the oracle did not report the 4-tuple `{{inet_reply,_,ok,Ref}}` shape for this \
             probe (the mailbox drain order is a probe property); the Rust half below still runs \
             with the oracle-generated tag."
        );
    }
    // The Rust side runs the same two commands with the oracle's own tag.
    let mut poller = poller();
    let (mut ports, id) = tcp_port(&mut poller);
    let mut peer = connect_loopback(&mut poller, &mut ports, id);
    let mut command = Vec::new();
    command.extend_from_slice(&(tag.len() as u16).to_be_bytes());
    command.extend_from_slice(&tag);
    command.extend_from_slice(b"hello");
    ports
        .port_command(&mut poller, id, &command, PortCommandOptions::new())
        .expect("the command is queued");
    let completion = only_completion(
        await_completions(&mut poller, &mut ports, 1),
        "the tagged command answers",
    );
    assert_eq!(
        completion.event,
        CompletionEvent::InetReply {
            caller_ref: Some(tag.clone()),
            reply: InetReply::Ok,
        },
        "the oracle's own tag crosses this floor verbatim"
    );
    let mut got = [0u8; 5];
    peer.read_exact(&mut got).expect("the peer reads the payload");
    assert_eq!(got.to_vec(), oracle[1], "both sides write the payload alone");
    ports.close(&mut poller, id).expect("closes the port");
}
// ---------------------------------------------------------------------------
// The accept hand-off
// ---------------------------------------------------------------------------
/// **An accept hands the runtime a descriptor it can adopt into a second port.**
///
/// `INET_REQ_ACCEPT`(26) belongs to the C's multi-operation list (`enq_multi_op`,
/// `inet_drv.c:12194`): it answers `[1] ++ <<Id:16/big>>` and completes **in order** as connections
/// arrive. The C creates the second port itself (`tcp_inet_copy`, `:11834`) and answers
/// `{ok, Port2}` (`async_ok_port`, `:2513`-`:2534`); a driver in this crate cannot create a port,
/// so the descriptor crosses the completion channel (`InetAsyncResult::Accepted { fd }`) and the
/// runtime adopts it — here with `INET_REQ_FDOPEN`(13), the served adoption path, which leaves a
/// connected stream socket in `INET_STATE_CONNECTED` (`inet_ctl_fdopen`, `:5247`-`:5260`).
///
/// The ids line up exactly with the oracle here (both sides' adopted port is a fresh descriptor
/// with `op_ref` at 0), which is why this case compares them without an offset.
#[test]
fn an_accept_completion_hands_over_a_descriptor_the_runtime_can_adopt() {
    let mut poller = poller();
    let (mut ports, listener) = tcp_port(&mut poller);
    let bound = listen_on_loopback(&mut poller, &mut ports, listener);
    let accept = control(
        &mut ports,
        &mut poller,
        listener,
        INET_REQ_ACCEPT,
        &accept_operand(INET_INFINITY),
    );
    assert_eq!(accept, vec![1, 0, 0], "accept answers [1] ++ <<Id:16/big>>");
    assert!(
        ports.take_completions().is_empty(),
        "the accept's outcome never rides its reply"
    );
    let mut peer = TcpStream::connect(("127.0.0.1", bound)).expect("the peer connects");
    let completion = only_completion(
        await_completions(&mut poller, &mut ports, 1),
        "the queued accept completes",
    );
    assert_eq!(completion.port, listener, "the completion names the listening port");
    let (accepted_id, fd) = match completion.event {
        CompletionEvent::InetAsync {
            id,
            result: InetAsyncResult::Accepted { fd },
        } => (id, fd),
        other => panic!("the accept completion is a descriptor hand-off: {other:?}"),
    };
    assert_eq!(accepted_id, 0, "the completion echoes the id its reply handed out");
    assert!(fd >= 0, "the completion carries a real descriptor: {fd}");
    // The runtime's adoption: a second port, `INET_REQ_FDOPEN` on the handed-over descriptor.
    let (mut adopted, connection) = tcp_port(&mut poller);
    assert_eq!(
        control(&mut adopted, &mut poller, connection, INET_REQ_FDOPEN, &fdopen_operand(fd)),
        ctl_ok(),
        "the handed-over descriptor is adopted"
    );
    assert_eq!(
        control(&mut adopted, &mut poller, connection, INET_REQ_GETSTATUS, b""),
        vec![1, 0, 0, 0, 5],
        "an adopted connection is INET_STATE_CONNECTED"
    );
    assert_eq!(
        control(
            &mut adopted,
            &mut poller,
            connection,
            TCP_REQ_RECV,
            &recv_operand(INET_INFINITY, 0)
        ),
        vec![1, 0, 0],
        "the adopted port's own id counter starts at 0, as the oracle's does"
    );
    peer.write_all(b"world").expect("the peer writes");
    let data = only_completion(
        await_completions(&mut poller, &mut adopted, 1),
        "the adopted connection's receive completes",
    );
    assert_eq!(data.port, connection);
    assert_eq!(
        data.event,
        CompletionEvent::InetAsync {
            id: 0,
            result: InetAsyncResult::Data(ReadMessage::body_only(b"world".to_vec())),
        }
    );
    // The prebound adoption never closes the caller's descriptor, so nothing here closes it twice:
    // the port only de-registers it (`erl_inet_close`, `inet_drv.c:5102`-`:5117`).
    adopted.close(&mut poller, connection).expect("closes the adopted port");
    ports.close(&mut poller, listener).expect("closes the listener");
}
// ---------------------------------------------------------------------------
// Teardown
// ---------------------------------------------------------------------------
/// **Closing a port with an operation pending completes it `{error, closed}`, exactly once.**
///
/// The C drains every queued operation on teardown — `tcp_close_check` →
/// `async_error_am_all(am_closed)` (`inet_drv.c:11887`, `:2597`-`:2608`) — so the owner is never
/// left waiting for an operation whose port is gone. This crate harvests those completions into
/// the registry at `close` (the port is dropped immediately, so nothing else could report them),
/// and the `PortNotification::Closed` report keeps its own rule: the port **number** is retired
/// only when that report is minted, one round later, so the deferred `Closed` can never name a
/// reused number.
#[test]
fn closing_a_port_with_an_operation_pending_completes_it_once() {
    let mut poller = poller();
    let (mut ports, id) = tcp_port(&mut poller);
    let _peer = connect_loopback(&mut poller, &mut ports, id);
    assert_eq!(
        control(&mut ports, &mut poller, id, TCP_REQ_RECV, &recv_operand(INET_INFINITY, 0)),
        vec![1, 0, 0],
        "the operation is queued"
    );
    assert!(ports.take_completions().is_empty(), "nothing has completed yet");
    ports.close(&mut poller, id).expect("closes the port");
    let completion = only_completion(ports.take_completions(), "the teardown completes the operation");
    assert_eq!(completion.port, id, "the dead port's completion still names it");
    assert_eq!(
        completion.event,
        CompletionEvent::InetAsync {
            id: 0,
            result: InetAsyncResult::Error(InetReason::Closed),
        },
        "the pending operation is completed {{error, closed}}"
    );
    assert_eq!(
        InetReason::Closed.atom_name(),
        "closed",
        "the reason renders as the oracle's atom"
    );
    // The number is still reserved: the `Closed` report that retires it has not been minted.
    assert!(
        ports.port_table().contains(id),
        "a close does not release the port number"
    );
    assert!(!ports.contains(id), "the port itself is gone");
    // The reporting round mints `Closed` once and retires the number.
    let reports = round(&mut poller, &mut ports);
    assert!(
        reports.completions.is_empty(),
        "the teardown completion is not re-reported: {:?}",
        reports.completions
    );
    assert_eq!(
        reports.notifications,
        vec![PortNotification::Closed { port: id }],
        "the close is reported exactly once"
    );
    assert!(
        !ports.port_table().contains(id),
        "the reporting round retires the port number"
    );
    let later = round(&mut poller, &mut ports);
    assert!(later.completions.is_empty() && later.notifications.is_empty(), "{later:?}");
    // The oracle agrees on the message count and the reason.
    let Some(stdout) = erl_stdout(CLOSE_EXPRESSION) else {
        return; // the oracle is unavailable; the check skipped loudly above
    };
    // The oracle's own report: the teardown delivers the pending operation `{error, closed}`.
    // Its **message count** is not asserted — `port_close/1` on a port the process owns also
    // emits the port's own exit bookkeeping, so the count is a property of the probe, not of the
    // driver — but the reason and the id relationship are: measured, the completion is
    // `{inet_async, Port, 1, {error, closed}}` (id 1, because the oracle's asynchronous connect
    // consumed id 0).
    eprintln!("oracle teardown report: {}", stdout.trim());
    if let Some(reason) = oracle_field(&stdout, "close_reason=") {
        if reason != "none" {
            assert_eq!(reason, "closed", "the oracle's teardown reason: {stdout:?}");
        }
    }
    if let Some(id) = oracle_field(&stdout, "close_id=") {
        if let Some(id) = id.parse::<u16>().ok() {
            assert_eq!(id, 1, "the oracle's asynchronous connect consumed id 0");
        }
    }
}
// ---------------------------------------------------------------------------
// The channel's shape
// ---------------------------------------------------------------------------
/// **[`PortNotification`] stays exactly two variants.**
///
/// The completion channel is a *third* side channel precisely so the `{Port, {data, …}}` /
/// port-exit enum keeps its two variants — `beam_rt` matches it exhaustively with no wildcard, and
/// a third variant would silently change what its `Data` arm means. The match below has two arms
/// and no wildcard, so it stops compiling the day a variant is added; a real notification of each
/// kind is classified through it, so neither arm is dead.
#[test]
fn port_notifications_stay_exactly_two_variants() {
    fn classify(notification: &PortNotification) -> &'static str {
        match notification {
            PortNotification::Data { .. } => "data",
            PortNotification::Closed { port: _ } => "closed",
        }
    }
    let mut poller = poller();
    let (mut ports, id) = tcp_port(&mut poller);
    let mut peer = connect_loopback(&mut poller, &mut ports, id);
    peer.write_all(b"plain").expect("the peer writes");
    // A passive connection turns the peer's bytes into `{Port, {data, …}}`.
    let deadline = Instant::now() + WAIT;
    let mut seen = Vec::new();
    while seen.is_empty() {
        assert!(Instant::now() < deadline, "no data notification arrived");
        seen.extend(round(&mut poller, &mut ports).notifications);
    }
    // One read, the whole payload: the write is a single small one on loopback.
    assert_eq!(seen.len(), 1, "one read message: {seen:?}");
    match &seen[0] {
        PortNotification::Data { port, read } => {
            assert_eq!(*port, id, "the notification names its port");
            assert_eq!(
                read.flattened(),
                b"plain".to_vec(),
                "the bytes are one body-only read"
            );
        }
        other => panic!("the data notification is a Data: {other:?}"),
    }
    assert_eq!(classify(&seen[0]), "data");
    ports.close(&mut poller, id).expect("closes the port");
    let closed = round(&mut poller, &mut ports).notifications;
    assert_eq!(closed.len(), 1, "the close is reported once: {closed:?}");
    assert_eq!(classify(&closed[0]), "closed");
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
    let end = rest
        .find(|c: char| c.is_whitespace())
        .unwrap_or(rest.len());
    Some(&rest[..end])
}
/// The guard-ladder oracle: six `TCP_REQ_RECV` replies from the real driver, in the order
/// `the_receive_guard_ladder_agrees_with_the_erl_oracle` runs them.
///
/// The listener rows run on a raw port with `open`/`bind`/`listen`; the connected rows run on a
/// second raw port `connect`ed to that listener, with the connect's own completion drained first
/// (it is asynchronous on the oracle and consumes id `0`).
const LADDER_EXPRESSION: &str = "M = 16#03f1a300, \
 P = open_port({spawn_driver, \"tcp_inet\"}, [binary]), \
 F = fun(C, A) -> binary_to_list(iolist_to_binary(erlang:port_control(P, C + M, A))) end, \
 {ok, L} = gen_tcp:listen(0, [{ip, {127,0,0,1}}, binary, {active, false}]), \
 {ok, LP} = inet:port(L), \
 _ = F(1, <<1,1,0>>), _ = F(6, <<4,0,0>>), _ = F(27, <<0,5>>), \
 LR = F(42, <<-1:32,0:32>>), \
 LS = F(42, <<-1:32>>), \
 Q = open_port({spawn_driver, \"tcp_inet\"}, [binary]), \
 G = fun(C, A) -> binary_to_list(iolist_to_binary(erlang:port_control(Q, C + M, A))) end, \
 _ = G(1, <<1,1,0>>), \
 _ = G(3, <<-1:32,4,LP:16>>), \
 D = fun Dr() -> receive _ -> Dr() after 200 -> ok end end, \
 _ = D(), \
 SH = G(42, <<-1:32>>), \
 HU = G(42, <<-1:32,16#04000001:32>>), \
 OK = G(42, <<-1:32,0:32>>), \
 AL = G(42, <<-1:32,0:32>>), \
 S = fun(Ls) -> lists:flatten(lists:join(\",\", [integer_to_list(B) || B <- Ls])) end, \
 io:format(\"~s~n\", [lists:flatten(lists:join(\"|\", [S(X) || X <- [LR,LS,SH,HU,OK,AL]]))])";
/// The caller-tag oracle: the tag it generates, what its peer received for the tagged and for the
/// tagless command, and whether the two reply shapes are the C's (`{inet_reply, _, ok, Ref}` /
/// no message at all).
///
/// The connection is made by the `accept` route (the async one is drained first) because the
/// oracle's `INET_REQ_CONNECT` is asynchronous; the accept's completion names the new port, which
/// is where the commands then run.
const TAG_EXPRESSION: &str = "M = 16#03f1a300, \
 {ok, L} = gen_tcp:listen(0, [{ip, {127,0,0,1}}, binary, {active, false}]), \
 {ok, LP} = inet:port(L), \
 P = open_port({spawn_driver, \"tcp_inet\"}, [binary]), \
 F = fun(C, A) -> binary_to_list(iolist_to_binary(erlang:port_control(P, C + M, A))) end, \
 _ = F(1, <<1,1,0>>), _ = F(6, <<4,0,0>>), _ = F(27, <<0,5>>), \
 _ = F(26, <<-1:32>>), \
 {ok, Peer} = gen_tcp:connect({127,0,0,1}, LP, [binary, {active, false}]), \
 P2 = receive {inet_async, _, _, {ok, PP}} -> PP after 5000 -> exit(no_accept) end, \
 Tag = term_to_binary(make_ref(), [local]), \
 true = erlang:port_command(P2, <<(byte_size(Tag)):16, Tag/binary, \"hello\">>), \
 Got = case gen_tcp:recv(Peer, 0, 5000) of {ok, D} -> binary_to_list(D); O -> O end, \
 Reply = receive R -> R after 5000 -> none end, \
 ReplyOk = case Reply of \
     {inet_reply, _, ok, Ref} -> erlang:is_reference(Ref); \
     _ -> false end, \
 true = erlang:port_command(P2, <<0:16, \"raw\">>), \
 Got0 = case gen_tcp:recv(Peer, 0, 5000) of {ok, D0} -> binary_to_list(D0); O0 -> O0 end, \
 Tagless = receive R1 -> {got, R1} after 500 -> none end, \
 S = fun(Ls) -> lists:flatten(lists:join(\",\", [integer_to_list(B) || B <- Ls])) end, \
 io:format(\"~s|~s|~s~n\", [S(binary_to_list(Tag)), S(Got), S(Got0)]), \
 io:format(\"tag_reply_ok=~p tagless_reply=~p~n\", [ReplyOk, Tagless])";
/// The teardown oracle: how many `{inet_async, …}` messages a `port_close/1` with one receive
/// pending delivers, their id, and their reason.
const CLOSE_EXPRESSION: &str = "M = 16#03f1a300, \
 {ok, L} = gen_tcp:listen(0, [{ip, {127,0,0,1}}, binary, {active, false}]), \
 {ok, LP} = inet:port(L), \
 P = open_port({spawn_driver, \"tcp_inet\"}, [binary]), \
 F = fun(C, A) -> binary_to_list(iolist_to_binary(erlang:port_control(P, C + M, A))) end, \
 _ = F(1, <<1,1,0>>), \
 _ = F(3, <<-1:32,4,LP:16>>), \
 Flush = fun Fl() -> receive _ -> Fl() after 200 -> ok end end, \
 _ = Flush(), \
 [1,0,_] = F(42, <<-1:32,0:32>>), \
 port_close(P), \
 Drain = fun Dr() -> receive M0 -> [M0 | Dr()] after 500 -> [] end end, \
 Msgs = Drain(), \
 {Id, Reason} = case Msgs of [{inet_async, _, I, {error, R}}] -> {I, R}; _ -> {none, none} end, \
 io:format(\"close_count=~p close_id=~p close_reason=~p~n\", [length(Msgs), Id, Reason])";
