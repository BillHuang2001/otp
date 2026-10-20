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
//! The `tcp_inet` **close path**: the `INET_REQ_CLOSE`(2) verb and the driver teardown, both of
//! which are the C's `tcp_desc_close` (`inet_drv.c:11932`-`:11939`) — `tcp_clear_input`
//! (`:11706`), `tcp_clear_output` (`:11718`) and `erl_inet_close` (`:5102`-`:5117`).
//!
//! Two properties are pinned here, both over **real loopback sockets**:
//!
//! * **`tcp_clear_output` runs on every close**, and it has no guard at all: it is
//!   `driver_deq(ix, driver_sizeq(ix))`, so whatever `port_command`/`send` could not push into the
//!   socket is discarded. Without it a closed port keeps reporting the stale remainder through
//!   `GETSTAT`'s `send_pend` (the C's `driver_sizeq`) — a well-formed wrong value, not a typed gap.
//! * **`tcp_clear_input` runs before the prebound ownership test**, so a partially buffered
//!   `{packet, N}` frame is dropped by a close even when the descriptor is the caller's
//!   (`INET_REQ_FDOPEN`); the *descriptor* half of `erl_inet_close` is what a prebound close skips
//!   — no `close(2)`, no state transition, no bookkeeping reset — which is why a prebound port's
//!   `GETSTATUS`/`GETFD`/`NAME`/`GETTYPE` answers are unchanged afterwards.
//!
//! The oracle rows are measured on OTP 29.1.1 / erts-17.1 and quoted at the assertion that uses
//! them; the `erl` cross-check skips **loudly** when the oracle is absent.
use super::opcode_tests::{connection, erl_stdout, op, Rig, CONTROL_MAGIC};
use super::wire::{ctl_ok, ctl_xerror};
use super::{InetState, TcpInetDriver, INET_AF_INET, INET_TYPE_STREAM};
use crate::poller::Poller;
use crate::port::PortCommandOptions;
use crate::registry::PortRegistry;
use beam_process::PortId;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::{AsRawFd, IntoRawFd, RawFd};
/// The C request numbers this suite drives (`inet_drv.c`'s `INET_REQ_*` enum).
const CLOSE: u32 = 2; // INET_REQ_CLOSE
const GETSTATUS: u32 = 16; // INET_REQ_GETSTATUS
/// `INET_STAT_SEND_PEND` (`inet_drv.c:968`) — the port's output-queue depth, i.e. the C's
/// `driver_sizeq` / this floor's `ctx.output().len()`.
const SEND_PEND: u8 = 8;
/// The `{packet, N}` option code (`INET_OPT_PACKET`, `inet_drv.c:637`).
const INET_OPT_PACKET: u8 = 23;
/// One `INET_REQ_GETSTAT`(11) call for `code`, as its 32-bit value — the wire shape
/// [`super::wire::getstat_reply`] builds (`[1, op, 4 bytes]`).
fn stat32(rig: &mut Rig, code: u8) -> u32 {
    let reply = rig.control(&op(11), &[code]);
    assert_eq!(reply.len(), 6, "a 32-bit counter record is [tag, op, 4 bytes]");
    assert_eq!(reply[0], 1, "the leading success tag");
    assert_eq!(reply[1], code, "the requested op byte, echoed");
    u32::from_be_bytes([reply[2], reply[3], reply[4], reply[5]])
}
/// The `send_pend` counter alone — the port's output-queue depth.
fn send_pend(rig: &mut Rig) -> u32 {
    stat32(rig, SEND_PEND)
}
/// `INET_REQ_GETSTATUS`(16)'s reply for one lifecycle state: `[1] ++ state_be32`
/// (`inet_drv.c:11128`), the code being [`InetState::state_code`].
fn getstatus_reply(state: InetState) -> Vec<u8> {
    let mut reply = ctl_ok();
    reply.extend_from_slice(&state.state_code().to_be_bytes());
    reply
}
/// `INET_REQ_GETTYPE`(15)'s reply: `[1] ++ af_be32 ++ stype_be32`.
fn gettype_reply(stype: u8) -> Vec<u8> {
    let mut reply = ctl_ok();
    reply.extend_from_slice(&u32::from(INET_AF_INET).to_be_bytes());
    reply.extend_from_slice(&u32::from(stype).to_be_bytes());
    reply
}
/// The six-byte `INET_REQ_FDOPEN` operand `[af, type, fd_be32]` — the form whose `bound` word is
/// implicitly 1 (`if (len == 6) bound = 1`, `inet_drv.c:5264`), so the C's `sock_name(2)`
/// pre-check runs.
fn fdopen_six(type_byte: u8, fd: RawFd) -> Vec<u8> {
    let mut operand = vec![INET_AF_INET, type_byte];
    operand.extend_from_slice(&fd.to_be_bytes());
    operand
}
/// The `{packet, 2}` option record (`[opt, val_be32]`).
fn packet_2_record() -> [u8; 5] {
    [INET_OPT_PACKET, 0, 0, 0, 2]
}
/// A well-formed `erlang:port_command/2` payload of `len` bytes: the C's `init_caller` prefix
/// `[n:be16] ++ tag`, here with a **zero-length** tag (`[0,0]`), which the C consumes and leaves
/// the payload untouched (`inet_drv.c:1820`-`:1890`). Anything else would read as a huge tag and
/// latch `driver_failure_posix(EINVAL)`.
fn tagged_command(len: usize) -> Vec<u8> {
    let mut command = vec![0u8, 0u8];
    command.extend(std::iter::repeat(0u8).take(len));
    command
}
/// A `tcp_inet` port (in the registry, so driven through the runtime's `port_control` path) whose
/// socket is **connected** to a peer that never reads, plus a command big enough that the kernel
/// cannot absorb it — `stat_tests`' measurement of the same state (`send_pend` = the strict
/// remainder of a 16 MiB command; the kernel's send buffer and the peer's receive window together
/// are far smaller, so no timing is involved).
fn stalled_output(rig: &mut Rig) {
    rig.registry
        .port_command(
            &mut rig.poller,
            rig.id,
            &tagged_command(16 * 1024 * 1024),
            PortCommandOptions::EMPTY,
        )
        .expect("the port accepts the command");
    rig.round().expect("the round writes what the socket takes");
    assert!(
        send_pend(rig) > 0,
        "the peer never reads, so a strict remainder is still queued"
    );
}
/// `INET_REQ_CLOSE`(2) — the numeric route a live `beam_rt` forwards for the C's close request,
/// i.e. `tcp_close_check` + `tcp_desc_close` (`inet_drv.c:12258`-`:12266`).
fn close_verb(rig: &mut Rig) -> Vec<u8> {
    rig.control(&op(CLOSE), &[])
}
/// A bare `tcp_inet` port plus a connected caller-owned descriptor adopted through
/// `INET_REQ_FDOPEN`, the peer end and the `TcpStream` that owns the descriptor.
///
/// The descriptor is **borrowed** (`as_raw_fd`), exactly as the C's `prebound` flag documents: the
/// driver never closes it, so the owner here does. The sockets are returned so they outlive the
/// port.
fn prebound_connection() -> (Rig, TcpStream, TcpStream, RawFd) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let peer = TcpStream::connect(listener.local_addr().expect("an address"))
        .expect("the far end connects");
    let (server, _peer_addr) = listener.accept().expect("the port's end is accepted");
    let fd = server.as_raw_fd();
    let mut rig = Rig::new(Box::new(TcpInetDriver::new()));
    assert_eq!(
        rig.control(&op(13), &fdopen_six(INET_TYPE_STREAM, fd)),
        ctl_ok(),
        "INET_REQ_FDOPEN adopts the caller's descriptor as prebound"
    );
    assert_eq!(
        rig.control(&op(GETSTATUS), &[]),
        getstatus_reply(InetState::Connected),
        "the C's `sock_peer` probe promotes a connected stream adoption"
    );
    (rig, peer, server, fd)
}
/// Splits an oracle dump of `<value>|` rows into its trimmed fields.
fn oracle_rows(expr: &str) -> Option<Vec<String>> {
    let dump = erl_stdout(expr)?;
    Some(
        dump.split('|')
            .map(str::trim)
            .filter(|row| !row.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}
// -----------------------------------------------------------------------
// tcp_clear_output on the close verb
// -----------------------------------------------------------------------
/// **`INET_REQ_CLOSE` empties the port's output queue** — `tcp_desc_close`'s
/// `tcp_clear_output` (`inet_drv.c:11718`-`:11724`) is an unguarded
/// `driver_deq(ix, driver_sizeq(ix))`, so bytes that could not be written are discarded instead of
/// surviving the close and being reported by `send_pend` (the C's `driver_sizeq`,
/// `inet_fill_stat` `:10766`) — the well-formed wrong value this defect produced.
///
/// The port itself stays **live and idle** (the verb closes the *socket*), so the counter is
/// still observable afterwards; an owned descriptor also moves to `INET_STATE_CLOSED`, which is
/// the oracle's measured `GETSTATUS` `[1,0,0,0,0]` after a close.
#[test]
fn the_close_verb_empties_the_queued_output() {
    let mut conn = connection();
    stalled_output(&mut conn.port);
    let queued = send_pend(&mut conn.port) as usize;
    assert_eq!(
        conn.port
            .registry
            .get(conn.port.id)
            .expect("a live port")
            .output_len(),
        queued,
        "`send_pend` is the port's output-queue depth"
    );
    assert_eq!(close_verb(&mut conn.port), ctl_ok(), "INET_REQ_CLOSE");
    assert_eq!(
        send_pend(&mut conn.port),
        0,
        "tcp_clear_output dropped the {queued} bytes that can never be written"
    );
    assert_eq!(
        conn.port
            .registry
            .get(conn.port.id)
            .expect("the port stays live")
            .output_len(),
        0,
        "and the queue itself is empty"
    );
    assert!(conn.port.contains(), "closing the socket leaves the port");
    assert_eq!(
        conn.port.control(&op(GETSTATUS), &[]),
        getstatus_reply(InetState::Closed),
        "an owned descriptor moves to INET_STATE_CLOSED"
    );
    assert_eq!(
        conn.port.control(&op(14), &[]),
        ctl_xerror("einval"),
        "GETFD(14) on the now socket-less port"
    );
}
/// **A teardown does not discard what can still be written.** The C's `tcp_inet_flush`
/// (`inet_drv.c:12708`-`:12731`) keeps the queue unless write readiness is unarmed
/// (`discard_output = !(desc->inet.event_mask & FD_WRITE)`) or `{linger,{true,0}}` is set, and
/// `flush_port` (`erts/emulator/beam/io.c:3659`-`:3700`) only reaches `terminate_port` (`:3704`)
/// once `is_port_ioq_empty(p)` (`:3697`) — otherwise the port sits in the C's Limbo
/// (`ERTS_PORT_SFLG_CLOSING`, `:3946`-`:3956`) until the queue drains.
///
/// So a 16 MiB write to a peer that never reads leaves the port **parked in
/// [`PortState::Closing`]** after `close`, with the unwritable remainder still queued and `stop`
/// not yet run; only the stall bound (`CLOSING_DRAIN_STALL_LIMIT`) or a drain that completes ends
/// it, and it is that later round which reports `Closed`.
///
/// Measured on the oracle (OTP 29.1.1 / erts-17.1): `port_close/1` returns `true` with 369 036
/// bytes still queued and the peer later receives 500 000/500 000.
#[test]
fn a_teardown_keeps_the_queued_output_that_write_readiness_can_still_send() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let _peer = TcpStream::connect(listener.local_addr().expect("an address"))
        .expect("the far end connects");
    let (server, _peer_addr) = listener.accept().expect("the port's end is accepted");
    // The driver **owns** the descriptor it adopts, so it gets a duplicate: `server` keeps its own.
    let fd = server
        .try_clone()
        .expect("a second descriptor for the same socket")
        .into_raw_fd();
    let driver = TcpInetDriver::from_socket(fd).expect("the driver adopts the socket");
    assert_eq!(driver.state(), InetState::Connected);
    let mut poller = Poller::new().expect("a poll set");
    let mut ports = PortRegistry::new();
    let id = ports
        .open(&mut poller, Box::new(driver))
        .expect("the port opens");
    ports
        .output(&mut poller, id, &tagged_command(16 * 1024 * 1024))
        .expect("the write");
    let queued = ports
        .get(id)
        .expect("the port is live")
        .output_len();
    assert!(queued > 0, "the peer never reads, so a remainder is queued");
    assert!(
        ports
            .get(id)
            .expect("the port is live")
            .take_active_spends()
            .is_empty(),
        "a plain write spends no {{active,_}} grant"
    );
    ports.close(id).expect("the close runs (and answers its `true`)");
    // The queue is kept: `tcp_inet_flush` saw write readiness armed (the stalled
    // `write_pending` left `Interest::BOTH`), so it did not `tcp_clear_output`.
    assert!(
        ports.contains(id),
        "a parked closing port stays in the live map — the C's Limbo"
    );
    assert!(
        ports.is_closing(id).expect("the port is live"),
        "the port is parked in PortState::Closing, not closed"
    );
    let port = ports.get(id).expect("the port is live");
    assert_eq!(
        port.output_len(),
        queued,
        "the {queued} bytes write readiness can still send are retained, not discarded"
    );
    assert!(!port.is_closed(), "`stop` has not run yet");
    // A second `close` is the C's idempotent re-entry (`io.c` returns at once for a CLOSING port).
    ports
        .close(id)
        .expect("a second close is an idempotent Ok(())");
    // Only once the remainder is written (or the stall bound drops it) does the port leave the
    // live map and report `Closed`; the peer here never reads, so that is the bound.
    let mut closed = false;
    for _ in 0..(crate::port::CLOSING_DRAIN_STALL_LIMIT + 4) {
        let round = ports
            .execute_port_tasks(&mut poller, crate::port::DEFAULT_READINESS_RED_BUDGET)
            .expect("a port-task round");
        if round.notifications.iter().any(|n| n.port == id) {
            closed = true;
            break;
        }
    }
    assert!(
        closed,
        "the stall bound ends the parked close and reports Closed exactly once"
    );
    assert!(!ports.contains(id), "the number retires with that report");
}
// -----------------------------------------------------------------------
// tcp_clear_input on a prebound close
// -----------------------------------------------------------------------
/// **A `{packet, N}` frame buffered before the close is dropped by it**, and a prebound close is
/// no exception: `tcp_clear_input` (`inet_drv.c:11706`) is the *first* thing `tcp_desc_close`
/// does, in front of the ownership test that only gates `erl_inet_close`.
///
/// The control half of this test proves the buffering is real — the same sequence without a close
/// delivers the frame — and the close half proves the buffer is gone: after the close the two
/// missing bytes are a *partial frame of their own*, so nothing is delivered. (Before the fix the
/// retained prefix completed the frame and the body arrived, which is the defect.)
#[test]
fn a_close_drops_a_partially_buffered_packet_frame() {
    // Control: no close, so the retained prefix completes the frame.
    let (mut rig, mut peer, _server, _fd) = prebound_connection();
    assert_eq!(
        rig.control(&op(7), &packet_2_record()),
        ctl_ok(),
        "the {{packet,2}} option"
    );
    peer.write_all(&[0, 5, b'a', b'b', b'c'])
        .expect("the peer writes a header plus three of the five body bytes");
    assert_eq!(
        rig.control("recv", b""),
        ctl_ok(),
        "no whole frame yet: the C buffers it in `desc->i_buf`"
    );
    assert_eq!(rig.input_len(), 0, "nothing was delivered");
    peer.write_all(b"de").expect("the peer completes the body");
    assert_eq!(rig.control("recv", b""), ctl_ok(), "the frame is whole now");
    assert_eq!(
        rig.take_input(5),
        b"abcde",
        "the retained prefix and the new bytes are one frame"
    );
    // The close: the retained prefix is `tcp_clear_input`'s first casualty.
    let (mut rig, mut peer, _server, _fd) = prebound_connection();
    assert_eq!(rig.control(&op(7), &packet_2_record()), ctl_ok());
    peer.write_all(&[0, 5, b'a', b'b', b'c'])
        .expect("the peer writes the same partial frame");
    assert_eq!(rig.control("recv", b""), ctl_ok(), "buffered, not delivered");
    assert_eq!(rig.input_len(), 0);
    assert_eq!(close_verb(&mut rig), ctl_ok(), "INET_REQ_CLOSE");
    peer.write_all(b"de").expect("the peer completes the body");
    assert_eq!(
        rig.control("recv", b""),
        ctl_ok(),
        "the socket is the caller's, so the port still reads it"
    );
    assert_eq!(
        rig.input_len(),
        0,
        "tcp_clear_input dropped the buffered prefix: `de` is a partial frame of its own, not the \
         tail of `abc`"
    );
}
/// **A prebound close keeps the descriptor, the state and the cached type** — `erl_inet_close`
/// (`inet_drv.c:5102`-`:5117`) only drops the select registration for a `prebound` descriptor
/// (`sock_select(…, 0)` + `event_mask = 0`); it issues no `close(2)` and makes no state
/// transition, because the `desc->state = INET_STATE_CLOSED` assignment lives in the
/// `prebound == 0` branch.
///
/// Measured on OTP 29.1.1 / erts-17.1 through `INET_REQ_FDOPEN`(13) of a connected socket: before
/// the close `GETSTATUS` `[1,0,0,0,5]`, `GETFD` `[1] ++ fd_be32`, a packed `NAME`, `GETTYPE`
/// `[1,0,0,0,1,0,0,0,1]`; after `INET_REQ_CLOSE` (reply `[1]`) **all four are byte-identical**,
/// where an owned descriptor answers `GETSTATUS` `[1,0,0,0,0]` and `GETFD` `[0]einval`.
///
/// What the close *does* discard is the queued output, because `tcp_clear_output` runs before the
/// ownership test — pinned here as well, since the oracle row above cannot see it.
#[test]
fn a_prebound_close_keeps_the_descriptor_and_the_state() {
    let (mut rig, _peer, _server, fd) = prebound_connection();
    let status = rig.control(&op(GETSTATUS), &[]);
    let getfd = rig.control(&op(14), &[]);
    let name = rig.control(&op(5), &[]);
    let gettype = rig.control(&op(15), &[]);
    assert_eq!(status, getstatus_reply(InetState::Connected));
    assert_eq!(getfd, {
        let mut reply = ctl_ok();
        reply.extend_from_slice(&fd.to_be_bytes());
        reply
    });
    assert_eq!(gettype, gettype_reply(INET_TYPE_STREAM));
    assert_eq!(name.first().copied(), Some(1), "NAME answers a packed address");
    assert_eq!(name[1], INET_AF_INET, "of the IPv4 family");
    stalled_output(&mut rig);
    assert_eq!(close_verb(&mut rig), ctl_ok(), "INET_REQ_CLOSE");
    assert_eq!(
        rig.control(&op(GETSTATUS), &[]),
        status,
        "a prebound close makes no state transition"
    );
    assert_eq!(
        rig.control(&op(14), &[]),
        getfd,
        "and never takes the caller's descriptor away"
    );
    assert_eq!(
        rig.control(&op(5), &[]),
        name,
        "so NAME still asks the kernel about the same socket"
    );
    assert_eq!(
        rig.control(&op(15), &[]),
        gettype,
        "the cached family/type survive, as they do on the owned path"
    );
    assert_eq!(
        send_pend(&mut rig),
        0,
        "`tcp_clear_output` runs in front of the ownership test, so the remainder is gone anyway"
    );
}
// -----------------------------------------------------------------------
// The oracle cross-check
// -----------------------------------------------------------------------
/// The oracle half of [`the_close_verb_empties_the_queued_output`]: the same close on the system
/// `tcp_inet` driver, read back at the `port_info` level.
///
/// The oracle needs `[nosuspend]` `port_command`s to leave a remainder (a plain `gen_tcp:send`
/// blocks until the queue drains), and it reports the depth with `erlang:port_info/2` rather than
/// `inet:getstat/2`, which the `prim_inet` layer refuses while the port is busy. Both sides must
/// answer the same invariants: a **non-zero** queue before the close, the C's `[1]` reply from
/// `INET_REQ_CLOSE`, and **zero** afterwards.
///
/// **Skips loudly** when `erl` is not on `PATH` or does not answer within [`ERL_TIMEOUT`].
#[test]
fn the_close_verb_output_row_agrees_with_the_erl_oracle() {
    let close_op = CONTROL_MAGIC + u64::from(CLOSE);
    let expr = format!(
        "J = fun(R) -> io:format(\"~w|\", [R]) end, \
         {{ok,L}} = gen_tcp:listen(0,[binary,{{active,false}},{{recbuf,512}},\
         {{ip,{{127,0,0,1}}}}]), \
         {{ok,P0}} = inet:port(L), \
         {{ok,C}} = gen_tcp:connect({{127,0,0,1}},P0,[binary,{{active,false}}]), \
         {{ok,S}} = gen_tcp:accept(L), \
         Fill = fun F(N) -> case erlang:port_command(S, binary:copy(<<0>>, 65536), [nosuspend]) \
         of true -> F(N+1); false -> N end end, \
         N = Fill(0), \
         J(N), \
         J(element(2, erlang:port_info(S, queue_size))), \
         J(erlang:port_control(S, {close_op}, <<>>)), \
         J(element(2, erlang:port_info(S, queue_size))), \
         gen_tcp:close(C), gen_tcp:close(L)"
    );
    let Some(rows) = oracle_rows(&expr) else {
        return;
    };
    assert_eq!(rows.len(), 4, "four oracle rows: {rows:?}");
    let accepted: u64 = rows[0].parse().expect("the accepted-command count");
    let before: u64 = rows[1].parse().expect("the queue depth before the close");
    let after: u64 = rows[3].parse().expect("the queue depth after the close");
    assert!(accepted > 0, "the oracle queued something: {rows:?}");
    assert!(before > 0, "so its output queue was non-empty: {rows:?}");
    assert_eq!(rows[2], "[1]", "INET_REQ_CLOSE's reply");
    assert_eq!(after, 0, "and tcp_clear_output emptied the queue: {rows:?}");
    // The same invariants on this floor, whose `send_pend` is its own output-queue depth.
    let mut conn = connection();
    stalled_output(&mut conn.port);
    assert!(send_pend(&mut conn.port) > 0, "the same non-empty queue");
    assert_eq!(close_verb(&mut conn.port), ctl_ok(), "the same [1] reply");
    assert_eq!(send_pend(&mut conn.port), 0, "and the same zero afterwards");
}
