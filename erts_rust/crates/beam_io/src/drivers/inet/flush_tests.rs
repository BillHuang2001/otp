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
//! `tcp_inet`'s `flush` override — the C's `tcp_inet_flush` (`inet_drv.c:12708`-`:12731`).
//!
//! A close reaches the driver's `flush` before its `stop` (the C `flush_port`,
//! `erts/emulator/beam/io.c:3659`-`:3700`, called from `erts_deliver_port_exit`'s CLOSING arm,
//! `:3946`-`:3956`), and this driver's answer decides whether the port may still drain its queue or
//! has to drop it:
//!
//! ```text
//! discard_output = !(desc->inet.event_mask & FD_WRITE) || (tcp_add_flags & TCP_ADDF_LINGER_ZERO)
//! ```
//!
//! so the queue is cleared (`tcp_clear_output`, `:11718`) only when write readiness is unarmed — no
//! peer can ever take another byte — or `{linger, {true, 0}}` is in effect, whose zero-linger close
//! aborts the connection with an `RST`. Otherwise the bytes stay queued and the port parks in
//! `PortState::Closing` until they are written.
//!
//! The rows below pin the two discard arms and the one keep arm, plus the two facts that make them
//! non-vacuous: the flag **crosses `accept(2)`** (the C's `tcp_inet_copy` mask at `:11829`-`:11831`),
//! and a latched flag is recorded **before** the `setsockopt` — so a descriptor whose syscall cannot
//! run latches it too (`:7363`-`:7388`, `propagate = 0`). `stop` is deliberately the other half: the
//! C's `tcp_inet_stop` (`:11895`-`:11924`) never touches the `ioq`, and only the close **verb**'s
//! `tcp_desc_close` (`:11932`-`:11939`) does.
//!
//! Neighbouring coverage (not repeated here): the parked-close row itself
//! (`close_tests.rs::a_teardown_keeps_the_queued_output_that_write_readiness_can_still_send`), the
//! port layer's flush → drain → `stop` ordering (`src/closing_tests.rs`), `fd`'s latch-only
//! `fd_flush` (`src/drivers/fd_close_tests.rs`, the C `sys/unix/sys_drivers.c:1029`-`:1034`) and
//! `ram_file_drv`'s `flush = NULL` slot (`src/drivers/ram_file_close_tests.rs`,
//! `drivers/common/ram_file_drv.c:120`).
use super::opcode_tests::{op, Rig, CONTROL_MAGIC};
use super::wire::ctl_ok;
use super::{TcpInetDriver, INET_AF_INET, INET_TYPE_STREAM};
use crate::driver::{Driver, DriverContext, PortArms};
use crate::poller::Poller;
use crate::port::PortCommandOptions;
use crate::port_task::PortNotification;
use crate::queue::ByteQueue;
use beam_common::Result;
use beam_process::PortId;
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::{IntoRawFd, RawFd};
/// `INET_REQ_SETOPTS` (`inet_drv.c`'s `INET_REQ_*` enum).
const SETOPTS: u32 = 7;
/// `INET_REQ_CLOSE` (`inet_drv.c`'s `INET_REQ_*` enum) — the close **verb**.
const CLOSE: u32 = 2;
/// The `linger` option code (`INET_OPT_LINGER`, `inet_drv.c:877`) — `SO_LINGER`.
const INET_OPT_LINGER: u8 = 3;
/// One `{linger, {OnOff, Linger}}` `setopts` record: the C reads `l_onoff` as the record's value
/// word and `l_linger` from the four bytes that follow, so the record is **nine** bytes
/// (`inet_drv.c:7363`-`:7388`) — not the five-byte `[opt, val_be32]` shape.
fn linger_record(on_off: i32, linger: i32) -> Vec<u8> {
    let mut record = vec![INET_OPT_LINGER];
    record.extend_from_slice(&on_off.to_be_bytes());
    record.extend_from_slice(&linger.to_be_bytes());
    record
}
/// A well-formed `erlang:port_command/2` payload of `len` bytes: the C's `init_caller` prefix
/// `[n:be16] ++ tag`, here with a **zero-length** tag (`[0,0]`), which the C consumes and leaves
/// the payload untouched (`inet_drv.c:1820`-`:1890`).
fn tagged_command(len: usize) -> Vec<u8> {
    let mut command = vec![0u8, 0u8];
    command.extend(std::iter::repeat(0u8).take(len));
    command
}
/// The driver plumbing a **bare** [`TcpInetDriver`] needs — its whole [`DriverContext`], the same
/// shape [`crate::PortRegistry`] builds internally (one poller, the two byte queues, the arming mask
/// and the four owner-side sinks). The same rig as `inherit_tests.rs`'s `Plumbing`, which is private
/// to that sibling.
struct Sink {
    poller: Poller,
    input: ByteQueue,
    output: ByteQueue,
    arms: PortArms,
    completions: Vec<crate::completion::PortCompletion>,
    active: std::collections::VecDeque<crate::port_task::ActiveInput>,
    failure: Option<crate::port_failure::DriverFailure>,
}

impl Sink {
    fn new() -> Sink {
        Sink {
            poller: Poller::new().expect("a poll set"),
            input: ByteQueue::new(),
            output: ByteQueue::new(),
            arms: PortArms::default(),
            completions: Vec::new(),
            active: std::collections::VecDeque::new(),
            failure: None,
        }
    }

    fn ctx(&mut self) -> DriverContext<'_> {
        let Self {
            poller,
            input,
            output,
            arms,
            completions,
            active,
            failure,
        } = self;
        DriverContext::new(PortId::new(1), poller, input, output, arms)
            .with_completions(completions)
            .with_active(active)
            .with_failure(failure)
    }

    /// [`Driver::start`] — a socket-less factory registers nothing (`tcp_inet_start` leaves the
    /// socket to `INET_REQ_OPEN`), a socket-bearing one arms `READABLE`.
    fn start(&mut self, driver: &mut TcpInetDriver) {
        Driver::start(driver, &mut self.ctx()).expect("the driver starts");
    }

    /// [`Driver::flush`] — the C's `tcp_inet_flush`.
    fn flush(&mut self, driver: &mut TcpInetDriver) -> Result<()> {
        driver.flush(&mut self.ctx())
    }

    /// [`Driver::stop`] — the C's `tcp_inet_stop`.
    fn stop(&mut self, driver: &mut TcpInetDriver) -> Result<()> {
        driver.stop(&mut self.ctx())
    }

    /// One **numeric** control call (`erlang:port_control/3`'s decimal opcode route).
    fn control(&mut self, driver: &mut TcpInetDriver, code: u32, arg: &[u8]) -> Vec<u8> {
        driver
            .control(&mut self.ctx(), &op(code), arg)
            .unwrap_or_else(|err| panic!("the numeric control call {code} ({arg:?}) failed: {err:?}"))
    }

    /// How many bytes sit in the port's output queue (the C's `driver_sizeq`).
    fn queued(&mut self) -> usize {
        self.ctx().output().len()
    }

    /// Queues `bytes` straight into the port's output queue without touching the driver — the state
    /// a queued `erlang:port_command/2` leaves behind before any round drives it.
    fn queue(&mut self, bytes: &[u8]) {
        self.ctx().output().push(bytes);
    }
}

/// A connected loopback pair: the driver's descriptor (a duplicate, so the driver owns it), the
/// port's own end and the far end, both held for the test's lifetime.
struct Loopback {
    fd: RawFd,
    _server: TcpStream,
    _peer: TcpStream,
}

fn loopback() -> Loopback {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let peer = TcpStream::connect(listener.local_addr().expect("an address"))
        .expect("the far end connects");
    let (server, _accepted) = listener.accept().expect("the port's end is accepted");
    let fd = server
        .try_clone()
        .expect("a second descriptor for the same socket")
        .into_raw_fd();
    Loopback {
        fd,
        _server: server,
        _peer: peer,
    }
}

/// A started, connected [`TcpInetDriver`] whose output queue holds an unwritable remainder: a
/// 16 MiB command against a peer that never reads stalls in `write_pending`'s `EAGAIN` arm, which
/// arms `Interest::BOTH` — the C's `sock_select(…, FD_WRITE, 1)` after a short write, i.e.
/// `event_mask & FD_WRITE` set.
fn write_armed_with_a_remainder(sink: &mut Sink) -> (TcpInetDriver, Loopback) {
    let pair = loopback();
    let mut driver = TcpInetDriver::from_socket(pair.fd).expect("the driver adopts the socket");
    sink.start(&mut driver);
    sink.queue(&tagged_command(16 * 1024 * 1024));
    Driver::output(&mut driver, &mut sink.ctx()).expect("the write is driven");
    assert!(
        sink.queued() > 0,
        "the peer never reads, so a strict remainder is still queued"
    );
    (driver, pair)
}

// -----------------------------------------------------------------------
// `!(event_mask & FD_WRITE)` — the unarmed-write arm
// -----------------------------------------------------------------------
/// **A port with no armed write interest discards its queue.** `event_mask & FD_WRITE` is set only
/// while a write can still make progress (the C arms it after a short write's
/// `INET_ECONN_BLOCK`, and `erl_inet_close` **zeroes the mask**, `inet_drv.c:5101`-`:5117`), so the
/// flush's first term is a plain "cannot ever write this again ⇒ drop it" test
/// (`discard_output = !(desc->inet.event_mask & FD_WRITE)`, `:12710`).
///
/// Both shapes of "unarmed" are pinned: a socket-less driver (no registration at all, `token` is
/// `None`) and a driver whose registration exists but is `READABLE`-only
/// ([`TcpInetDriver::start`]'s arm for a socket-bearing port). Neither may leave the port holding
/// bytes it can never write out.
#[test]
fn an_unarmed_write_interest_discards_the_queued_output() {
    // (1) No registration at all: the `"tcp_inet"` factory's socket-less port.
    let mut sink = Sink::new();
    let mut bare = TcpInetDriver::new();
    sink.start(&mut bare);
    sink.queue(&tagged_command(4096));
    assert_eq!(sink.queued(), 4098, "the queue really holds the command");
    sink.flush(&mut bare).expect("the C entry returns void");
    assert_eq!(
        sink.queued(),
        0,
        "`!(event_mask & FD_WRITE)` ⇒ `tcp_clear_output` (`:11718`-`:11724`)"
    );
    // (2) A live registration whose mask lacks FD_WRITE: `start` arms `READABLE` only.
    let pair = loopback();
    let mut read_only = TcpInetDriver::from_socket(pair.fd).expect("the driver adopts the socket");
    sink.start(&mut read_only);
    sink.queue(&tagged_command(4096));
    assert_eq!(sink.queued(), 4098);
    sink.flush(&mut read_only).expect("the C entry returns void");
    assert_eq!(
        sink.queued(),
        0,
        "a registered-but-not-writable port is unarmed too — the discard is `:12710`, not `token.is_none()`"
    );
}

// -----------------------------------------------------------------------
// `TCP_ADDF_LINGER_ZERO` — the second discard arm
// -----------------------------------------------------------------------
/// **`{linger, {true, 0}}` discards a queue write readiness could still send**, and a socket
/// without it keeps the same queue. This is the arm that makes the two halves of the C's formula
/// distinguishable: both drivers here are write-armed with a real remainder (see
/// [`write_armed_with_a_remainder`]), so only `TCP_ADDF_LINGER_ZERO` (`inet_drv.c:864`, latched at
/// `:7382`-`:7386`) can explain a discard.
///
/// The flag is installed through the **numeric `setopts`(7) route** a live runtime uses, with the
/// C's nine-byte `{linger, {OnOff, Linger}}` record, and the record answers `[1]` — `linger` is one
/// of the `propagate = 0` codes (`:7363`-`:7388`, `apply`'s non-propagating arm).
#[test]
fn the_linger_zero_flag_decides_the_discard_of_a_write_armed_queue() {
    // Control: write armed, no `{linger,{true,0}}` ⇒ the queue is KEPT (`flush` is a no-op).
    let mut sink = Sink::new();
    let (mut kept, _pair) = write_armed_with_a_remainder(&mut sink);
    let queued = sink.queued();
    sink.flush(&mut kept).expect("the C entry returns void");
    assert_eq!(
        sink.queued(),
        queued,
        "write readiness is armed ⇒ `tcp_inet_flush` keeps every byte for the Closing drain"
    );
    // The same driver, now carrying `{linger,{true,0}}`: the very same queue is discarded.
    assert_eq!(
        sink.control(&mut kept, SETOPTS, &linger_record(1, 0)),
        ctl_ok(),
        "INET_REQ_SETOPTS accepts the nine-byte linger record"
    );
    sink.flush(&mut kept).expect("the C entry returns void");
    assert_eq!(
        sink.queued(),
        0,
        "`discard_output |= tcp_add_flags & TCP_ADDF_LINGER_ZERO` ⇒ `tcp_clear_output`"
    );
    // And `{linger, {true, N>0}}` is NOT zero-linger: the flag's own `linger == 0` test.
    let mut sink = Sink::new();
    let (mut kept, _pair) = write_armed_with_a_remainder(&mut sink);
    assert_eq!(
        sink.control(&mut kept, SETOPTS, &linger_record(1, 5)),
        ctl_ok(),
        "a non-zero `l_linger` is accepted too"
    );
    let queued = sink.queued();
    sink.flush(&mut kept).expect("the C entry returns void");
    assert_eq!(
        sink.queued(),
        queued,
        "`value != 0 && linger == 0` — a five-second linger leaves the flag clear"
    );
}

/// **The flag is latched before — and independently of — the `setsockopt`.** The C records
/// `TCP_ADDF_LINGER_ZERO` in `inet_set_opts`' `INET_OPT_LINGER` arm ahead of the syscall
/// (`inet_drv.c:7382`-`:7386`), and `linger` is not one of the `propagate = 1` codes, so a
/// descriptor whose `setsockopt` cannot run (a socket-less port — the C's invalid descriptor) still
/// answers the record `[1]` **and** still latches the flag. So the record's reply alone cannot
/// distinguish "applied" from "latched", which is why the discard below is the observable half.
#[test]
fn the_linger_zero_flag_is_latched_before_the_setsockopt() {
    let mut sink = Sink::new();
    let mut bare = TcpInetDriver::new();
    sink.start(&mut bare);
    assert_eq!(
        sink.control(&mut bare, SETOPTS, &linger_record(1, 0)),
        ctl_ok(),
        "a non-propagating code answers `[1]` even with no socket to set it on"
    );
    sink.queue(&tagged_command(1024));
    sink.flush(&mut bare).expect("the C entry returns void");
    assert_eq!(
        sink.queued(),
        0,
        "the flag the record latched is what the flush reads"
    );
}

// -----------------------------------------------------------------------
// The flag crosses `accept(2)` — the `tcp_inet_copy` mask
// -----------------------------------------------------------------------
/// **`TCP_ADDF_LINGER_ZERO` is one of the fields `tcp_inet_copy` inherits**, so a listener's
/// `{linger, {true, 0}}` reaches the accepted socket and decides *its* close — while a listener
/// without it leaves the accepted socket keeping (and parking on) the same queue.
///
/// The route is the runtime's: a listener port whose `setopts`(7) record installed the flag, the
/// [`crate::PortRegistry::driver_as`] downcast, [`TcpInetDriver::from_accepted`] (the C's
/// `tcp_inet_copy`, `inet_drv.c:11795`-`:11845`, the `tcp_add_flags` mask at `:11829`-`:11831`),
/// and the accepted connection opened as a second port. Both halves are asserted, so the
/// inheritance is not vacuous: with the flag the close discards and **finishes inside the call**
/// (`stop` ran, the port is gone, one `Closed` report); without it the close **parks** with the
/// queue intact (the C's Limbo, `erts/emulator/beam/io.rs`'s `ERL_PORT_SFLG_CLOSING` twin —
/// `io.c:3946`-`:3956`).
#[test]
fn the_linger_zero_flag_crosses_accept_and_decides_the_close() {
    // (1) The listener carries the flag: the accepted socket's close discards.
    let (mut rig, child, _peer) = accepted_over(true);
    let queued = rig
        .registry
        .get(child)
        .expect("the accepted port is live")
        .output_len();
    assert!(queued > 0, "the accepted socket carries a remainder");
    rig.registry
        .close(&mut rig.poller, child)
        .expect("the close answers its `true`");
    assert!(
        !rig.registry.contains(child),
        "the discarded queue let the close finish inside the call — `stop` ran"
    );
    assert!(
        rig.registry.is_closing(child).is_err(),
        "a finished port is gone from the live map, so `is_closing` is the registry's badarg"
    );
    let outcome = rig
        .registry
        .handle_events(&mut rig.poller, &[])
        .expect("the reporting round");
    assert_eq!(
        outcome.notifications,
        vec![PortNotification::Closed { port: child }],
        "the Closed report is owed by the very next round — no drain loop is needed"
    );
    // (2) Control: the same listener, no flag ⇒ the close parks with the queue intact.
    let (mut rig, child, _peer) = accepted_over(false);
    let queued = rig
        .registry
        .get(child)
        .expect("the accepted port is live")
        .output_len();
    assert!(queued > 0, "the accepted socket carries a remainder");
    rig.registry
        .close(&mut rig.poller, child)
        .expect("the close answers its `true`; the drain is asynchronous");
    assert!(
        rig.registry.contains(child),
        "a parked closing port stays in the live map — the C's Limbo"
    );
    assert!(
        rig.registry.is_closing(child).expect("the parked port is live"),
        "write readiness can still send, so `tcp_inet_flush` kept the bytes"
    );
    assert_eq!(
        rig.registry
            .get(child)
            .expect("the parked port is live")
            .output_len(),
        queued,
        "none of the {queued} bytes was discarded"
    );
    assert!(
        !rig.registry
            .get(child)
            .expect("the parked port is live")
            .is_closed(),
        "and `stop` has not run"
    );
    assert!(
        rig.registry
            .get_mut(child)
            .expect("the parked port is live")
            .take_active_spends()
            .is_empty(),
        "a plain write spends no {{active,_}} grant"
    );
}

/// A registry listener whose `setopts`(7) record either installs `{linger,{true,0}}` or does not,
/// an accepted connection adopted through [`TcpInetDriver::from_accepted`] and opened as a second
/// port, with a 16 MiB command already stalled in that port's output queue.
///
/// The flag is installed on a **socket-less** listener: [`TcpInetDriver::from_accepted`] copies the
/// listener's option store (`inherit_tests.rs`'s rig drives a real `accept`(26) for the same copy),
/// and the listener's own socket plays no part in the store.
fn accepted_over(linger_zero: bool) -> (Rig, PortId, TcpStream) {
    let mut rig = Rig::new(Box::new(TcpInetDriver::new()));
    if linger_zero {
        assert_eq!(
            rig.control(&op(SETOPTS), &linger_record(1, 0)),
            ctl_ok(),
            "the listener's own `setopts`(7) record"
        );
    }
    let pair = loopback();
    let child = {
        let listener = rig
            .registry
            .driver_as::<TcpInetDriver>(rig.id)
            .expect("the listener port's own tcp_inet driver");
        TcpInetDriver::from_accepted(pair.fd, listener).expect("the accepted descriptor is adopted")
    };
    assert_eq!(
        child.state(),
        super::InetState::Connected,
        "the adopted descriptor is a connected stream"
    );
    let child = rig
        .registry
        .open(&mut rig.poller, Box::new(child))
        .expect("the accepted connection opens as a second port");
    assert_ne!(child, rig.id, "the accepted connection is a port of its own");
    rig.registry
        .output(&mut rig.poller, child, &tagged_command(16 * 1024 * 1024))
        .expect("the write");
    rig.round().expect("the round writes what the socket takes");
    // The far end is handed back: dropping it would close the connection under the port and turn
    // the stalled write into a reset, which would let the teardown discard the queue instead.
    (rig, child, pair._peer)
}

// -----------------------------------------------------------------------
// `stop` never touches the queue; the close verb does
// -----------------------------------------------------------------------
/// **The teardown is not the discard.** The C's `tcp_inet_stop` (`inet_drv.c:11895`-`:11924`) is
/// `tcp_close_check` + `tcp_clear_input` + `inet_stop`/`erl_inet_close` and never touches the
/// `ioq` — the queued output is the flush's and the port layer's business (`flush_port` reaches
/// `terminate_port` only once `is_port_ioq_empty(p)`, `io.c:3696`-`:3699`) — whereas the close
/// **verb** runs `tcp_desc_close` (`:11932`-`:11939`), whose unconditional `tcp_clear_output`
/// (`:11718`) is pinned by `close_tests.rs::the_close_verb_empties_the_queued_output`.
///
/// So the same write-armed residue survives `stop` byte for byte and is emptied by
/// `INET_REQ_CLOSE`(2).
#[test]
fn the_teardown_never_clears_the_queue_but_the_close_verb_does() {
    let mut sink = Sink::new();
    let (mut stopped, _pair) = write_armed_with_a_remainder(&mut sink);
    let queued = sink.queued();
    sink.stop(&mut stopped).expect("the teardown runs");
    assert_eq!(
        sink.queued(),
        queued,
        "`tcp_inet_stop` never touches the ioq — all {queued} bytes are still there"
    );
    let mut sink = Sink::new();
    let (mut closed, _pair) = write_armed_with_a_remainder(&mut sink);
    assert_eq!(
        sink.control(&mut closed, CLOSE, &[]),
        ctl_ok(),
        "INET_REQ_CLOSE(2) — the `tcp_desc_close` route"
    );
    assert_eq!(
        sink.queued(),
        0,
        "`tcp_desc_close`'s unguarded `tcp_clear_output` emptied the queue"
    );
}

// -----------------------------------------------------------------------
// `udp_inet` has no flush slot
// -----------------------------------------------------------------------
/// **The datagram driver declares no `flush`**, so its queued output is never silently discarded by
/// the flush step — the port layer's own `Closing` drain decides, and a command the driver refuses
/// stays queued for it. The C's `udp_inet` entry leaves the slot `NULL` (`inet_drv.c:1406`-`:1443`,
/// where `tcp_inet` fills it at `:1368`), which is the `drv_ptr->flush != NULL` test in `flush_port`
/// (`io.c:3664`) answering "no"; `ram_file_drv` is the same (`ram_file_drv.c:120`,
/// `ram_file_close_tests.rs`).
///
/// The Rust driver inherits [`Driver::flush`]'s defaulted no-op, pinned here on a socket-less port
/// whose queue a command really occupies: were `udp_inet` to clear it, a `gen_udp:send/4` queued at
/// close time would vanish instead of being answered.
#[test]
fn udp_inet_has_no_flush_so_the_queued_output_survives() {
    let mut sink = Sink::new();
    let mut udp = super::udp::UdpInetDriver::new();
    Driver::start(&mut udp, &mut sink.ctx()).expect("the datagram driver starts socket-less");
    sink.queue(&tagged_command(1024));
    assert_eq!(sink.queued(), 1026);
    udp.flush(&mut sink.ctx())
        .expect("the defaulted flush answers Ok(())");
    assert_eq!(
        sink.queued(),
        1026,
        "the C's `flush == NULL` ⇒ nothing is discarded here; the port's Closing drain decides"
    );
    // The registry-level half: a `tcp_inet` port's own queue cannot survive the *same* close shape,
    // which is what makes the datagram answer specific rather than a property of the port layer.
    let mut rig = Rig::new(Box::new(TcpInetDriver::new()));
    rig.registry
        .port_command(
            &mut rig.poller,
            rig.id,
            &tagged_command(1024),
            PortCommandOptions::EMPTY,
        )
        .expect("the command is queued");
    assert_eq!(
        rig.registry
            .get(rig.id)
            .expect("a live port")
            .output_len(),
        1026,
        "the command sits in the output queue before the close"
    );
    rig.close();
    assert!(
        !rig.contains(),
        "a socket-less `tcp_inet` port discards (`!write_armed`) and finishes inside the close"
    );
    let close_magic = CONTROL_MAGIC;
    assert_eq!(
        close_magic + u64::from(CLOSE),
        CONTROL_MAGIC + 2,
        "the numeric close opcode is the magic plus INET_REQ_CLOSE"
    );
    assert_eq!(
        INET_AF_INET, 1,
        "the packed-address family tag this suite's operands use"
    );
    assert_eq!(INET_TYPE_STREAM, 1, "and the stream type tag");
}
