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
//! The **control-time read attempt** of `TCP_REQ_RECV`(42) — the C's
//! `tcp_recv(desc, n)` made from inside the control arm (`inet_drv.c:12310`).
//!
//! The poller is edge-triggered (`EPOLLET`), so a payload that reaches the socket **before** the
//! read side is armed produces no readiness event: a receive that only armed and returned would
//! park for ever. The C solves this by reading first — `if (INET_IGNORED(...) || tcp_recv(desc, n)
//! == 0) { ... }` (`:12310`), arming `sock_select` (`:12315`) and `add_multi_timer` (`:12313`)
//! **only** on would-block, and answering a `Timeout == 0` operation that would block with
//! `async_error_am(am_timeout)` (`:12311`) straight away.
//!
//! So every case here pre-buffers its bytes *and proves they are in the socket's receive queue*
//! (`MSG_PEEK` on a duplicate of the driver's descriptor, which consumes nothing) before the
//! control call, then asserts the completion existed **before any round ran**.
use super::super::opcode_tests::{erl_stdout, op, Rig};
use super::super::opt::{INET_INFINITY, INET_LOPT_PACKET};
use super::super::packet::PacketType;
use super::super::stat_tests::{stat32, stat64};
use super::super::TcpInetDriver;
use super::async_tests::{async_events, await_events, recv_operand, set_option};
use crate::completion::InetAsyncResult;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::{IntoRawFd, RawFd};
use std::time::{Duration, Instant};

/// `TCP_REQ_RECV` (`inet_drv.c:12268`): the asynchronous receive.
const TCP_REQ_RECV: u32 = 42;
/// `INET_STAT_RECV_CNT` (30-bit counter): one per delivered read.
const RECV_CNT: u8 = 1;
/// `INET_STAT_RECV_OCT` (64-bit counter): the octets a delivered read carried.
const RECV_OCT: u8 = 9;
/// The hang guard of every condition wait: never a wall-clock budget for an idle host.
const HANG_GUARD: Duration = Duration::from_secs(5);
/// One condition-wait iteration.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// A connected `tcp_inet` port plus its far end and a **peek** descriptor.
///
/// `probe` is a second descriptor for the driver's own socket, used only to prove that bytes
/// really are sitting in the receive queue (`MSG_PEEK` consumes nothing, so the driver's later
/// read still sees them) — that is what makes these cases deterministic instead of racing the
/// kernel's loopback delivery.
struct Buffered {
    port: Rig,
    peer: TcpStream,
    probe: RawFd,
}

impl Buffered {
    /// The connect-side shape: the driver adopts an accepted socket, exactly as the runtime's
    /// `gen_tcp:connect/3` route does.
    fn connected() -> Buffered {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
        let peer = TcpStream::connect(listener.local_addr().expect("an address"))
            .expect("the far end connects");
        let (server, _addr) = listener.accept().expect("the port's end is accepted");
        let probe = server
            .try_clone()
            .expect("a peek descriptor for the same socket")
            .into_raw_fd();
        let fd = server
            .try_clone()
            .expect("a descriptor for the driver")
            .into_raw_fd();
        let driver = TcpInetDriver::from_socket(fd).expect("the driver adopts the socket");
        assert!(driver.state().is_connected(), "the adopted socket has a peer");
        Buffered {
            port: Rig::new(Box::new(driver)),
            peer,
            probe,
        }
    }

    /// The accepted shape: `TcpInetDriver::from_accepted(fd, listener)` — the runtime's
    /// `tcp_inet_copy` route — so the accepted socket is a real `TcpInetDriver`, not a bare
    /// `from_socket` adoption.
    fn accepted() -> Buffered {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
        let peer = TcpStream::connect(listener.local_addr().expect("an address"))
            .expect("the far end connects");
        let (server, _addr) = listener.accept().expect("the port's end is accepted");
        let probe = server
            .try_clone()
            .expect("a peek descriptor for the same socket")
            .into_raw_fd();
        let fd = server
            .try_clone()
            .expect("a descriptor for the driver")
            .into_raw_fd();
        let listener_fd = listener
            .try_clone()
            .expect("a descriptor for the listener driver")
            .into_raw_fd();
        let listener_driver = TcpInetDriver::from_socket(listener_fd).expect("a listener driver");
        let driver = TcpInetDriver::from_accepted(fd, &listener_driver)
            .expect("the accepted socket inherits the listener's options");
        assert!(driver.state().is_connected(), "the accepted socket has a peer");
        Buffered {
            port: Rig::new(Box::new(driver)),
            peer,
            probe,
        }
    }

    /// Waits until at least `want` bytes are visible in the driver's receive queue.
    fn wait_buffered(&self, want: usize) {
        let guard = Instant::now() + HANG_GUARD;
        let mut buf = [0u8; 4096];
        loop {
            // SAFETY: `MSG_PEEK | MSG_DONTWAIT` copies the queue's head into a buffer this
            // function owns and removes nothing, so the driver's own read still sees the bytes.
            let n = unsafe {
                libc::recv(
                    self.probe,
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    libc::MSG_PEEK | libc::MSG_DONTWAIT,
                )
            };
            assert!(
                n >= 0,
                "the peek must not fail: {}",
                std::io::Error::last_os_error()
            );
            if n as usize >= want {
                return;
            }
            assert!(
                Instant::now() < guard,
                "only {n} of {want} bytes were in the receive queue within {HANG_GUARD:?}"
            );
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Sends `bytes` from the far end.
    fn send(&mut self, bytes: &[u8]) {
        self.peer.write_all(bytes).expect("the peer writes");
        self.peer.flush().expect("the peer flushes");
    }
}

impl Drop for Buffered {
    fn drop(&mut self) {
        // SAFETY: `probe` is a duplicate this fixture opened itself; nothing else closes it, so
        // it is closed exactly once here.
        unsafe { libc::close(self.probe) };
    }
}

/// Renders one completion the way the `erl` oracle prints it (`~w` of `gen_tcp:recv/3`).
fn render(result: &InetAsyncResult) -> String {
    match result {
        InetAsyncResult::Data(read) => {
            let body = read.body.as_deref().unwrap_or(&[]);
            let octets: Vec<String> = body.iter().map(u8::to_string).collect();
            format!("{{ok,<<{}>>}}", octets.join(","))
        }
        InetAsyncResult::Error(crate::completion::InetReason::Timeout) => {
            "{error,timeout}".to_string()
        }
        other => panic!("unexpected completion: {other:?}"),
    }
}

/// Asserts that `events` is exactly one `Data` completion carrying `body` under `id`.
fn assert_data(events: &[(u16, InetAsyncResult)], id: u16, body: &[u8]) {
    assert_eq!(events.len(), 1, "exactly one completion: {events:?}");
    assert_eq!(events[0].0, id, "the operation's own id");
    match &events[0].1 {
        InetAsyncResult::Data(read) => {
            assert!(read.header.is_empty(), "a raw read carries no header");
            assert_eq!(
                read.body.as_deref(),
                Some(body),
                "the payload the control call read (never truncated)"
            );
        }
        other => panic!("expected a data completion, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The three arms of the control call
// ---------------------------------------------------------------------------

/// (a) A payload buffered **before** the read arms is delivered by the control call itself, with
/// no deadline and no queued operation left behind.
#[test]
fn a_buffered_payload_is_delivered_by_the_recv_control_call() {
    let mut fixture = Buffered::connected();
    fixture.send(b"hi");
    fixture.wait_buffered(2);

    let reply = fixture
        .port
        .control(&op(TCP_REQ_RECV), &recv_operand(0, 2));
    assert_eq!(
        reply,
        vec![1, 0, 0],
        "the control call still answers [1] ++ <<Id:16/big>>"
    );
    assert_data(&async_events(&fixture.port.completions()), 0, b"hi");
    assert!(
        fixture.port.next_timeout().is_none(),
        "a completed read leaves no operation and therefore no deadline behind"
    );

    fixture.port.round().expect("a port-task round");
    assert!(
        fixture.port.completions().is_empty(),
        "the completion is reported exactly once"
    );
}

/// The same completion reaches the runtime through the registry seam it actually uses
/// (`PortRegistry::port_control_reply` → `take_completions`), not only through a direct
/// `Driver::control` call.
#[test]
fn the_registry_hands_up_the_completion_the_control_call_staged() {
    let mut fixture = Buffered::connected();
    fixture.send(b"hi");
    fixture.wait_buffered(2);

    let reply = fixture
        .port
        .registry
        .port_control_reply(
            &mut fixture.port.poller,
            fixture.port.id,
            &op(TCP_REQ_RECV),
            &recv_operand(0, 2),
        )
        .expect("the control request succeeds");
    assert_eq!(reply.bytes, vec![1, 0, 0], "the id-only synchronous answer");

    let completions = fixture.port.registry.take_completions();
    assert_eq!(completions.len(), 1, "the control call staged it: {completions:?}");
    assert_eq!(
        completions[0].port, fixture.port.id,
        "stamped with the driving port"
    );
    assert_data(&async_events(&completions), 0, b"hi");
}

/// (b) An empty socket with `Timeout == 0` answers `{error, timeout}` from inside the control
/// call (`async_error_am(am_timeout)`, `inet_drv.c:12311`) — it neither arms readiness nor leaves
/// a deadline — and a later read still works.
#[test]
fn an_empty_socket_with_a_zero_deadline_answers_timeout_from_the_control_call() {
    let mut fixture = Buffered::connected();

    let reply = fixture
        .port
        .control(&op(TCP_REQ_RECV), &recv_operand(0, 2));
    assert_eq!(reply, vec![1, 0, 0], "the id is answered, the op is not left queued");
    assert_eq!(
        async_events(&fixture.port.completions()),
        vec![(0, InetAsyncResult::Error(crate::completion::InetReason::Timeout))],
        "the would-block arm answers the operation immediately"
    );
    assert!(
        fixture.port.next_timeout().is_none(),
        "no operation is left queued, so no deadline reaches the port's TIMEOUT task"
    );
    fixture.port.round().expect("a port-task round");
    assert!(
        fixture.port.completions().is_empty(),
        "a timeout is reported exactly once"
    );

    // A following read is armed and really completes when the peer writes.
    let reply = fixture
        .port
        .control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 2));
    assert_eq!(reply, vec![1, 0, 1], "the second operation's id");
    fixture.send(b"hi");
    assert_data(&await_events(&mut fixture.port, 1), 1, b"hi");
}

/// (c) The same rule holds on an **accepted** socket built through
/// `TcpInetDriver::from_accepted(fd, listener)` — the runtime's `tcp_inet_copy` route.
#[test]
fn an_accepted_socket_delivers_a_buffered_payload_from_the_control_call() {
    let mut fixture = Buffered::accepted();
    fixture.send(b"hi");
    fixture.wait_buffered(2);

    let reply = fixture
        .port
        .control(&op(TCP_REQ_RECV), &recv_operand(0, 2));
    assert_eq!(reply, vec![1, 0, 0], "the id-only synchronous answer");
    assert_data(&async_events(&fixture.port.completions()), 0, b"hi");
    assert!(
        fixture.port.next_timeout().is_none(),
        "an accepted socket arms nothing after a completed read"
    );
}

/// (d) A **complete** `{packet, 4}` frame is delivered by the control call.
#[test]
fn a_complete_packet_frame_is_delivered_from_the_control_call() {
    let mut fixture = Buffered::connected();
    set_option(
        &mut fixture.port,
        INET_LOPT_PACKET,
        i32::from(PacketType::Len4.code()),
    );

    let mut framed = vec![0, 0, 0, 5];
    framed.extend_from_slice(b"hello");
    fixture.send(&framed);
    fixture.wait_buffered(framed.len());

    let reply = fixture
        .port
        .control(&op(TCP_REQ_RECV), &recv_operand(0, 0));
    assert_eq!(reply, vec![1, 0, 0], "the id-only synchronous answer");
    assert_data(&async_events(&fixture.port.completions()), 0, b"hello");
    assert!(
        fixture.port.next_timeout().is_none(),
        "a completed frame leaves no deadline"
    );
}

/// (d) A **partial** frame never delivers: the bytes buffer in `pending_input`, a `Timeout == 0`
/// read answers `{error, timeout}` (never a truncated binary), and the frame completes only when
/// the rest arrives.
#[test]
fn a_partial_packet_frame_is_never_delivered_truncated() {
    let mut fixture = Buffered::connected();
    set_option(
        &mut fixture.port,
        INET_LOPT_PACKET,
        i32::from(PacketType::Len4.code()),
    );

    let mut partial = vec![0, 0, 0, 5];
    partial.extend_from_slice(b"he");
    fixture.send(&partial);
    fixture.wait_buffered(partial.len());

    let reply = fixture
        .port
        .control(&op(TCP_REQ_RECV), &recv_operand(0, 0));
    assert_eq!(reply, vec![1, 0, 0], "the id-only synchronous answer");
    assert_eq!(
        async_events(&fixture.port.completions()),
        vec![(0, InetAsyncResult::Error(crate::completion::InetReason::Timeout))],
        "a partial frame buffers instead of delivering a truncated body"
    );

    // The buffered bytes are re-read into `pending_input`, and the operation parks correctly.
    let reply = fixture
        .port
        .control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 0));
    assert_eq!(reply, vec![1, 0, 1], "the second operation's id");
    assert!(
        fixture.port.completions().is_empty(),
        "a partial frame does not complete the parked read"
    );

    fixture.send(b"llo");
    assert_data(&await_events(&mut fixture.port, 1), 1, b"hello");
}

/// (e) A pre-buffered payload with a **finite** deadline completes from the control call — no
/// 3 s wait, no deadline armed.
#[test]
fn a_finite_deadline_answers_a_buffered_payload_from_the_control_call() {
    let mut fixture = Buffered::connected();
    fixture.send(b"hi");
    fixture.wait_buffered(2);

    let reply = fixture
        .port
        .control(&op(TCP_REQ_RECV), &recv_operand(3000, 2));
    assert_eq!(reply, vec![1, 0, 0], "the id-only synchronous answer");
    assert_data(&async_events(&fixture.port.completions()), 0, b"hi");
    assert!(
        fixture.port.next_timeout().is_none(),
        "the timer is armed only on would-block, so a satisfied read declares none"
    );
}

/// The counters charge a synchronously-read payload exactly once.
#[test]
fn a_synchronous_read_is_counted_once() {
    let mut fixture = Buffered::connected();
    fixture.send(b"hi");
    fixture.wait_buffered(2);

    fixture
        .port
        .control(&op(TCP_REQ_RECV), &recv_operand(0, 2));
    assert_eq!(stat32(&mut fixture.port, RECV_CNT), 1, "one delivered read");
    assert_eq!(stat64(&mut fixture.port, RECV_OCT), 2, "two octets, once");

    fixture.port.round().expect("a port-task round");
    assert_eq!(stat32(&mut fixture.port, RECV_CNT), 1, "never counted twice");
    assert_eq!(stat64(&mut fixture.port, RECV_OCT), 2, "never counted twice");
}

// ---------------------------------------------------------------------------
// The oracle row
// ---------------------------------------------------------------------------

/// The two-step row `gen_tcp:recv(C, 2, 0)` runs on the oracle: `{error,timeout}` on an empty
/// socket, then `{ok,<<"hi">>}` for a payload the peer sent before the second call.
const ORACLE_PROBE: &str = r#"
    {ok,L} = gen_tcp:listen(0,[{ip,{127,0,0,1}},binary,{active,false}]),
    {ok,P} = inet:port(L),
    {ok,C} = gen_tcp:connect({127,0,0,1},P,[binary,{active,false}]),
    {ok,A} = gen_tcp:accept(L),
    R0 = gen_tcp:recv(C,2,0),
    ok = gen_tcp:send(A,<<"hi">>),
    timer:sleep(100),
    R1 = gen_tcp:recv(C,2,0),
    io:format("~w ~w",[R0,R1]),
    gen_tcp:close(C), gen_tcp:close(A), gen_tcp:close(L)
"#;

/// The control-time read agrees with the real driver, byte for byte.
#[test]
fn the_control_time_receive_agrees_with_the_erl_oracle() {
    let Some(oracle) = erl_stdout(ORACLE_PROBE) else {
        return;
    };
    let oracle = oracle.trim();
    assert_eq!(
        oracle, "{error,timeout} {ok,<<104,105>>}",
        "the oracle's own row"
    );

    let mut fixture = Buffered::connected();
    let reply = fixture
        .port
        .control(&op(TCP_REQ_RECV), &recv_operand(0, 2));
    assert_eq!(reply, vec![1, 0, 0]);
    let first = async_events(&fixture.port.completions());

    fixture.send(b"hi");
    fixture.wait_buffered(2);
    let reply = fixture
        .port
        .control(&op(TCP_REQ_RECV), &recv_operand(0, 2));
    assert_eq!(reply, vec![1, 0, 1]);
    let second = async_events(&fixture.port.completions());

    let ours = format!("{} {}", render(&first[0].1), render(&second[0].1));
    assert_eq!(ours, oracle, "the control-time read against the oracle");
}
