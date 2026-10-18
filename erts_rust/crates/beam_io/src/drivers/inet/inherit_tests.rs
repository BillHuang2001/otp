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

//! The accepted-socket option inheritance suite — the C's `tcp_inet_copy`
//! (`inet_drv.c:11795`-`:11839`) as implemented by [`SocketOptions::inherited_from`] and
//! [`TcpInetDriver::from_accepted`].
//!
//! Two halves, because the mapping has two kinds of evidence:
//!
//! 1. **The pure unit half** (the first ten tests): **no** poller, no [`crate::PortRegistry`], no
//!    socket and no `erl`. The mapping is a pure function of the two option stores, so the whole
//!    positive half (the fifteen inherited fields) and the whole negative half (the fields the C
//!    resets) are decided there, each case built so it **fails without the copy**: the listener's
//!    inherited values are all non-default, and the child's non-inherited ones are non-default too
//!    (and different from the listener's), so neither half can pass by accident.
//! 2. **The real-loopback half** (the tests after it): a listener bound to an ephemeral
//!    `127.0.0.1` port, its option records installed through the **numeric** `setopts`(7) route,
//!    a real `std::net::TcpStream` peer, the driver's own `accept`(26) driving a real `accept(2)`
//!    and the resulting [`crate::InetAsyncResult::Accepted`] descriptor adopted through
//!    [`TcpInetDriver::from_accepted`] — then the accepted driver's `getopts`(8) reply is compared
//!    against the listener's, code for code. That is the only way to prove the copy happens on a
//!    genuinely accepted descriptor rather than in a store the test handed over.
//! 3. **An `erl` oracle row** that measures the C's own `tcp_inet_copy`: the system driver creates
//!    its accepted socket inside the C, so the expression drives both ports itself (no
//!    `prim_inet:accept_opts/3`, which is the *Erlang* half that re-applies `{active, …}`) and
//!    prints the `getopts` bytes of the accepted port and of the listener for comparison. It skips
//!    **loudly** when `erl` is absent or does not answer.

use beam_common::BeamError;

use super::op_queue::async_ok_reply;
use super::opcode_tests::{erl_stdout, loopback_arg, op, open_arg, oracle_replies, CONTROL_MAGIC};
use super::opt::{
    SocketOptions, INET_LOPT_ACTIVE, INET_LOPT_BUFFER, INET_LOPT_DEBUG, INET_LOPT_DELIVER,
    INET_LOPT_EXITONCLOSE, INET_LOPT_HEADER, INET_LOPT_MODE, INET_LOPT_MSGQ_HIWTRMRK,
    INET_LOPT_MSGQ_LOWTRMRK, INET_LOPT_PACKET, INET_LOPT_PACKET_SIZE, INET_LOPT_TCP_DELAY_SEND,
    INET_LOPT_TCP_HIWTRMRK, INET_LOPT_TCP_LOWTRMRK, INET_LOPT_TCP_READ_AHEAD,
    INET_LOPT_TCP_SEND_TIMEOUT, INET_LOPT_TCP_SEND_TIMEOUT_CLOSE, INET_LOPT_TCP_SHOW_ECONNRESET,
    INET_LOPT_UDP_READ_PACKETS,
};
use super::wire::ctl_ok;
use super::{TcpInetDriver, INET_REP_OK};
use crate::completion::{CompletionEvent, InetAsyncResult, PortCompletion};
use crate::driver::{Driver, DriverContext, PortArms};
use crate::poller::Poller;
use crate::port_failure::DriverFailure;
use crate::port_task::ActiveInput;
use crate::queue::ByteQueue;
use beam_process::PortId;
use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::TcpStream;
use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};

/// `{mode, binary}` (`INET_MODE_BINARY`, `inet_drv.c:779`).
const MODE_BINARY: i32 = 1;

/// `{deliver, port}` (`INET_DELIVER_PORT`, `inet_drv.c:783`).
const DELIVER_PORT: i32 = 0;

/// `{active, true}` (`INET_ACTIVE`, `inet_drv.c:787`).
const ACTIVE_TRUE: i32 = 1;

/// `{active, N}` (`INET_MULTI`, `inet_drv.c:789`).
const ACTIVE_MULTI: i32 = 3;

/// The listener's option store: every **inherited** field non-default, plus non-default values for
/// the fields the C does **not** copy (so the negative half has a value to leak if the mapping is
/// wrong).
fn listener_options() -> SocketOptions {
    SocketOptions {
        // --- the inherited set (`tcp_inet_copy`'s copy list, `:11809`-`:11838`) --------------
        packet: 2,          // {packet, 2}   — the C's `inet.htype`
        packet_size: 100,   // {packet_size, 100}
        header: 1,          // {header, 1}
        mode: MODE_BINARY,  // {mode, binary}
        deliver: DELIVER_PORT,
        exit_on_close: 0,   // {exit_on_close, false}
        buffer_size: 4096,  // {buffer, 4096}
        high: 4096,         // {high_watermark, 4096}
        low: 2048,          // {low_watermark, 2048}
        send_timeout: 500,  // {send_timeout, 500}
        send_timeout_close: 1,
        show_econnreset: true,  // TCP_ADDF_SHOW_ECONNRESET
        read_ahead: false,      // TCP_ADDF_NO_READ_AHEAD
        msgq_high: 1000,        // {high_msgq_watermark, 1000}
        msgq_low: 500,          // {low_msgq_watermark, 500}
        // --- the fields the C does NOT copy, deliberately pushed off their defaults ----------
        active: ACTIVE_MULTI,
        active_count: 7,
        debug: 1,
        netns: Some(b"/netns/listener".to_vec()),
        read_packets: 3,
        line_delim: b'|',
        buffer_set: false,
        delay_send: true,   // TCP_ADDF_DELAY_SEND — outside the C's mask
    }
}

/// The child store an accepted socket starts from: defaults with **its own** non-default values for
/// every field the C does *not* copy, each different from [`listener_options`]'s. A copy that
/// touched one of them would be visible here.
fn accepted_defaults() -> SocketOptions {
    SocketOptions {
        active: ACTIVE_TRUE,
        active_count: 2,
        debug: 2,
        netns: Some(b"/netns/accepted".to_vec()),
        read_packets: 9,
        line_delim: b';',
        buffer_set: true,
        delay_send: false,
        ..SocketOptions::default()
    }
}

/// The `{packet, …}`/`{packet_size, …}`/`{header, …}` framing group.
#[test]
fn framing_options_are_inherited() {
    let parent = listener_options();
    let child = accepted_defaults();
    let copy = child.inherited_from(&parent);
    assert_eq!(copy.packet, 2, "inet.htype (`:11812`)");
    assert_eq!(copy.packet_size, 100, "inet.psize (`:11813`)");
    assert_eq!(copy.header, 1, "inet.hsz (`:11815`)");
    // The child's own values were all defaults, so "inherited" has to mean "changed".
    assert_ne!(child.packet, copy.packet);
    assert_ne!(child.packet_size, copy.packet_size);
    assert_ne!(child.header, copy.header);
}

/// The `{mode, …}`/`{deliver, …}`/`{exit_on_close, …}` representation group.
#[test]
fn representation_options_are_inherited() {
    let parent = listener_options();
    let child = accepted_defaults();
    let copy = child.inherited_from(&parent);
    assert_eq!(copy.mode, MODE_BINARY, "inet.mode (`:11809`)");
    assert_eq!(copy.deliver, DELIVER_PORT, "inet.deliver (`:11811`)");
    assert_eq!(copy.exit_on_close, 0, "inet.exitf (`:11810`)");
    assert_ne!(child.mode, copy.mode);
    assert_ne!(child.deliver, copy.deliver);
    assert_ne!(child.exit_on_close, copy.exit_on_close);
}

/// The `{buffer, …}`/`{high_watermark, …}`/`{low_watermark, …}` window group.
#[test]
fn window_options_are_inherited() {
    let parent = listener_options();
    let child = accepted_defaults();
    let copy = child.inherited_from(&parent);
    assert_eq!(copy.buffer_size, 4096, "inet.bufsz (`:11816`)");
    assert_eq!(copy.high, 4096, "desc->high (`:11817`)");
    assert_eq!(copy.low, 2048, "desc->low (`:11818`)");
    assert_ne!(child.buffer_size, copy.buffer_size);
    assert_ne!(child.high, copy.high);
    assert_ne!(child.low, copy.low);
}

/// The `{send_timeout, …}`/`{send_timeout_close, …}` timeout group.
#[test]
fn timeout_options_are_inherited() {
    let parent = listener_options();
    let child = accepted_defaults();
    let copy = child.inherited_from(&parent);
    assert_eq!(copy.send_timeout, 500, "desc->send_timeout (`:11819`)");
    assert_eq!(
        copy.send_timeout_close, 1,
        "desc->send_timeout_close (`:11820`)"
    );
    assert_ne!(child.send_timeout, copy.send_timeout);
    assert_ne!(child.send_timeout_close, copy.send_timeout_close);
}

/// The `SHOW_ECONNRESET`/`NO_READ_AHEAD` flag group (the only two `tcp_add_flags` bits the C keeps).
#[test]
fn flag_options_are_inherited() {
    let parent = listener_options();
    let child = accepted_defaults();
    let copy = child.inherited_from(&parent);
    assert!(
        copy.show_econnreset,
        "tcp_add_flags & TCP_ADDF_SHOW_ECONNRESET (`:11821`)"
    );
    assert!(
        !copy.read_ahead,
        "tcp_add_flags & TCP_ADDF_NO_READ_AHEAD (`:11822`)"
    );
    assert_ne!(child.show_econnreset, copy.show_econnreset);
    assert_ne!(child.read_ahead, copy.read_ahead);
}

/// The busy-msgq limits, which the C reads off the parent port and writes to the child
/// (`erl_drv_busy_msgq_limits`, `:11835`-`:11839`).
#[test]
fn msgq_watermarks_are_inherited() {
    let parent = listener_options();
    let child = accepted_defaults();
    let copy = child.inherited_from(&parent);
    assert_eq!(copy.msgq_high, 1000, "the parent's busy-msgq high limit");
    assert_eq!(copy.msgq_low, 500, "the parent's busy-msgq low limit");
    assert_ne!(child.msgq_high, copy.msgq_high);
    assert_ne!(child.msgq_low, copy.msgq_low);
}

/// The negative half: every field the C resets (or never copies) keeps the **child's** value, never
/// the listener's.
#[test]
fn driver_only_options_are_never_inherited() {
    let parent = listener_options();
    let child = accepted_defaults();
    let copy = child.inherited_from(&parent);

    // `inet_start` resets `{active, …}` to `INET_PASSIVE` (`:10929`) and the copy list omits it, so
    // an accepted socket starts passive — `prim_inet:accept_opts/3` re-applies the mode.
    assert_eq!(copy.active, ACTIVE_TRUE, "the child's own `active`");
    assert_ne!(copy.active, parent.active);
    // ... and its `{active, N}` counter along with it (`:10930`).
    assert_eq!(copy.active_count, 2, "the child's own `active_count`");
    assert_ne!(copy.active_count, parent.active_count);
    // `inet_start` sets `DDBG_DEFAULT` (`:10983`).
    assert_eq!(copy.debug, 2, "the child's own `debug`");
    assert_ne!(copy.debug, parent.debug);
    // `inet_start` sets `NULL` (`:10982`).
    assert_eq!(
        copy.netns.as_deref(),
        Some(&b"/netns/accepted"[..]),
        "the child's own `netns`"
    );
    assert_ne!(copy.netns, parent.netns);
    // A `udp_descriptor` field (`:1536`) `tcp_inet_copy` never touches.
    assert_eq!(copy.read_packets, 9, "the child's own `read_packets`");
    assert_ne!(copy.read_packets, parent.read_packets);
    // `inet_start` sets `'\n'` (`:10934`); `desc->delimiter` is not in the copy list.
    assert_eq!(copy.line_delim, b';', "the child's own `line_delim`");
    assert_ne!(copy.line_delim, parent.line_delim);
    // `INET_FLG_BUFFER_SET` lives in `desc->flags`, zeroed at `:10981`.
    assert!(copy.buffer_set, "the child's own `buffer_set`");
    assert_ne!(copy.buffer_set, parent.buffer_set);
    // `TCP_ADDF_DELAY_SEND` is outside the mask (`:11821`-`:11823`) — `prim_inet:accept_opts/3`'s
    // half, not the driver's.
    assert!(!copy.delay_send, "the child's own `delay_send`");
    assert_ne!(copy.delay_send, parent.delay_send);
}

/// The whole mapping at once: the result is exactly `listener_options()` for the inherited fields
/// and `accepted_defaults()` for the rest — no field anywhere else.
///
/// `SocketOptions` derives `Debug` over all of its fields, so comparing the rendered stores is a
/// total check (and adding a field to the store without deciding its half breaks this suite).
#[test]
fn the_copy_is_exactly_the_c_mapping() {
    let parent = listener_options();
    let child = accepted_defaults();
    let copy = child.inherited_from(&parent);

    let expected = SocketOptions {
        // inherited — from the listener
        packet: parent.packet,
        packet_size: parent.packet_size,
        header: parent.header,
        mode: parent.mode,
        deliver: parent.deliver,
        exit_on_close: parent.exit_on_close,
        buffer_size: parent.buffer_size,
        high: parent.high,
        low: parent.low,
        send_timeout: parent.send_timeout,
        send_timeout_close: parent.send_timeout_close,
        show_econnreset: parent.show_econnreset,
        read_ahead: parent.read_ahead,
        msgq_high: parent.msgq_high,
        msgq_low: parent.msgq_low,
        // not inherited — the child's own
        active: child.active,
        active_count: child.active_count,
        debug: child.debug,
        netns: child.netns.clone(),
        read_packets: child.read_packets,
        line_delim: child.line_delim,
        buffer_set: child.buffer_set,
        delay_send: child.delay_send,
    };
    assert_eq!(format!("{copy:?}"), format!("{expected:?}"));
    // The two stores really do differ everywhere, so the check above is not an identity.
    assert_ne!(format!("{parent:?}"), format!("{child:?}"));
}

/// Two default stores copy to a default store: an inheritance with nothing to inherit is the
/// identity, so the accepted default cannot drift away from `inet_start`'s own initializers.
#[test]
fn a_default_listener_yields_the_defaults() {
    let parent = SocketOptions::default();
    let child = SocketOptions::default();
    let copy = child.inherited_from(&parent);
    assert_eq!(format!("{copy:?}"), format!("{parent:?}"));
}

/// [`TcpInetDriver::from_accepted`] adopts through [`TcpInetDriver::from_socket`] first, so an
/// unusable descriptor is that adoption's typed `BeamError::Io` — nothing is inherited and no
/// driver is produced (the caller keeps `fd` to close).
#[test]
fn from_accepted_propagates_the_adoption_failure() {
    let listener = TcpInetDriver::new();
    match TcpInetDriver::from_accepted(-1, &listener) {
        Ok(_) => panic!("fd -1 is not a socket: the adoption must fail"),
        Err(BeamError::Io(err)) => assert_eq!(err.raw_os_error(), Some(libc::EBADF)),
        Err(other) => panic!("expected the typed I/O error of the failed probe, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The real-loopback half: a real listener, a real `accept(2)`, the real hand-off
// ---------------------------------------------------------------------------

// The numeric control request codes this half drives, under the C's names (`inet_drv.c`):
// `OPEN`=1, `BIND`=6, `SETOPTS`=7, `GETOPTS`=8, `ACCEPT`=26, `LISTEN`=27 — the same numbers
// `accept_tests.rs`/`opcode_tests.rs` use, sent the way the Erlang side sends them (`op(code)`).
const OPEN: u32 = 1;
const BIND: u32 = 6;
const SETOPTS: u32 = 7;
const GETOPTS: u32 = 8;
const ACCEPT: u32 = 26;
const LISTEN: u32 = 27;

/// A 5 000 ms accept deadline: long enough that only a genuinely broken round can reach it, short
/// enough that a stuck case reports rather than hangs.
const ACCEPT_TIMEOUT: [u8; 4] = [0, 0, 0x13, 0x88];

/// The port number every bare driver here is driven under — the poller's registration key (and the
/// id a completion is stamped with). Nothing else is open on that poller.
const PORT: PortId = PortId::new(1);

/// The wall-clock cap on a loopback connect turning into an accept completion. A kernel-local
/// connect completes in microseconds; the cap only stops a genuinely broken round from hanging the
/// suite (the same polled-condition discipline `accept_tests.rs` uses).
const ACCEPT_WAIT: Duration = Duration::from_secs(10);

/// The driver plumbing a **bare** [`TcpInetDriver`] needs — its whole [`DriverContext`].
///
/// The real-loopback half cannot use the `opcode_tests::Rig`: that owns the driver inside a
/// [`crate::PortRegistry`], and [`TcpInetDriver::from_accepted`] needs `&TcpInetDriver` for the
/// listener whose options are being inherited. So the driver stays in the test's hand and the
/// context is built over these fields directly — the same shape
/// [`crate::PortRegistry`] builds internally (one poller, the two byte queues, the arming mask and
/// the four owner-side sinks).
struct Plumbing {
    poller: Poller,
    input: ByteQueue,
    output: ByteQueue,
    arms: PortArms,
    completions: Vec<PortCompletion>,
    active: VecDeque<ActiveInput>,
    failure: Option<DriverFailure>,
}

impl Plumbing {
    fn new() -> Plumbing {
        Plumbing {
            poller: Poller::new().expect("a poll set"),
            input: ByteQueue::new(),
            output: ByteQueue::new(),
            arms: PortArms::default(),
            completions: Vec::new(),
            active: VecDeque::new(),
            failure: None,
        }
    }

    /// A context over this plumbing with all four owner-side sinks installed, so a driver callback
    /// may deliver a completion/a read-an-active-input/latch a failure exactly as it can under a
    /// real port.
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
        DriverContext::new(PORT, poller, input, output, arms)
            .with_completions(completions)
            .with_active(active)
            .with_failure(failure)
    }

    /// [`Driver::start`] — the socket-less factory registers nothing, exactly as the C's
    /// `tcp_inet_start` leaves the socket to `INET_REQ_OPEN`.
    fn start(&mut self, driver: &mut TcpInetDriver) {
        let mut ctx = self.ctx();
        driver.start(&mut ctx).expect("the driver starts");
    }

    /// One **numeric** control call (`erlang:port_control/3`'s decimal opcode route).
    fn control(&mut self, driver: &mut TcpInetDriver, code: u32, arg: &[u8]) -> Vec<u8> {
        let mut ctx = self.ctx();
        driver
            .control(&mut ctx, &op(code), arg)
            .unwrap_or_else(|err| panic!("the numeric control call {code} ({arg:?}) failed: {err:?}"))
    }

    /// One readiness round: wait for the poller, then give the driver the `ready_input` the
    /// registry would have dispatched (the run half of `handle_events`).
    fn poll_ready(&mut self, driver: &mut TcpInetDriver) {
        let events = match self.poller.poll(Some(Duration::from_millis(100))) {
            Ok(events) => events,
            Err(BeamError::Io(err)) if err.kind() == ErrorKind::Interrupted => return,
            Err(err) => panic!("the poll failed: {err:?}"),
        };
        if events.is_empty() {
            return;
        }
        let mut ctx = self.ctx();
        driver
            .ready_input(&mut ctx)
            .expect("the driver's readiness callback runs");
    }

    /// Drains the completions the driver staged, without their port id (there is one port here).
    fn take_completions(&mut self) -> Vec<CompletionEvent> {
        std::mem::take(&mut self.completions)
            .into_iter()
            .map(|done| done.event)
            .collect()
    }
}

/// Opens, binds `127.0.0.1:0` (the kernel picks an ephemeral port) and listens through the
/// **numeric** route, returning the bound port.
fn listening(driver: &mut TcpInetDriver, plumbing: &mut Plumbing) -> u16 {
    assert_eq!(
        plumbing.control(driver, OPEN, &open_arg()),
        ctl_ok(),
        "`open` answers `[1]`"
    );
    let reply = plumbing.control(driver, BIND, &loopback_arg(0));
    assert_eq!(
        reply.len(),
        3,
        "`bind` answers `[1, port_hi, port_lo]`, got {reply:?}"
    );
    assert_eq!(reply[0], INET_REP_OK, "`bind`'s result tag, got {reply:?}");
    let bound = u16::from_be_bytes([reply[1], reply[2]]);
    assert_ne!(bound, 0, "the kernel assigned an ephemeral port");
    assert_eq!(
        plumbing.control(driver, LISTEN, &[0, 5]),
        ctl_ok(),
        "`listen` answers `[1]`"
    );
    bound
}

/// One `[opt, val_be32]` socket-option record — `inet_set_opts`'s five-byte wire shape.
///
/// [`super::opt::oracle`] carries an identical helper, but it is declared `pub(super)` there and
/// that module's `super` is `opt`, so it is reachable from `opt`'s own suites (`opt_tests`,
/// `opt_state_tests`, `accept_opts_tests`) and **not** from this sibling of `opt`. The record is a
/// three-line pure byte builder, so it is spelled here rather than widening a module this node may
/// not edit.
fn record(code: u8, value: i32) -> Vec<u8> {
    let mut record = vec![code];
    record.extend_from_slice(&value.to_be_bytes());
    record
}

/// The `setopts` payload of a `(code, value)` list — one `[opt, val_be32]` record per pair.
fn setopts_arg(records: &[(u8, i32)]) -> Vec<u8> {
    let mut arg = Vec::new();
    for (code, value) in records {
        arg.extend_from_slice(&record(*code, *value));
    }
    arg
}

/// The `getopts` reply a port holding exactly `records` must report: the result tag followed by one
/// `[opt, val_be32]` record per code, **in request order** (`inet_fill_opts`).
fn expected_getopts(records: &[(u8, i32)]) -> Vec<u8> {
    let mut reply = ctl_ok();
    for (code, value) in records {
        reply.extend_from_slice(&record(*code, *value));
    }
    reply
}

/// Drives readiness rounds until the queued accept `id` completes, returning the descriptor it
/// handed over — a **polled condition** under [`ACCEPT_WAIT`], never a sleep-then-assert.
fn drive_until_accepted(plumbing: &mut Plumbing, listener: &mut TcpInetDriver, id: u16) -> RawFd {
    let deadline = Instant::now() + ACCEPT_WAIT;
    loop {
        plumbing.poll_ready(listener);
        for event in plumbing.take_completions() {
            if let CompletionEvent::InetAsync {
                id: got,
                result: InetAsyncResult::Accepted { fd },
            } = event
            {
                assert_eq!(got, id, "the completion carries the accept's own id");
                return fd;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the accept never completed within {ACCEPT_WAIT:?}"
        );
    }
}

/// One **real** accept end to end: a listener on an ephemeral loopback port with `records`
/// installed on it through the numeric `setopts`(7) route, a real `std::net::TcpStream` peer, the
/// driver's own `accept`(26), and the `Accepted { fd }` hand-off adopted through
/// [`TcpInetDriver::from_accepted`].
struct Accepted {
    listener: TcpInetDriver,
    child: TcpInetDriver,
    plumbing: Plumbing,
    /// The far end, held so the connection is not closed under either driver.
    _peer: TcpStream,
}

impl Accepted {
    fn new(records: &[(u8, i32)]) -> Accepted {
        let mut plumbing = Plumbing::new();
        let mut listener = TcpInetDriver::new();
        plumbing.start(&mut listener);
        let bound = listening(&mut listener, &mut plumbing);
        assert_eq!(
            plumbing.control(&mut listener, SETOPTS, &setopts_arg(records)),
            ctl_ok(),
            "the listener accepts its whole option list (`setopts` answers `[1]`)"
        );
        assert_eq!(
            plumbing.control(&mut listener, ACCEPT, &ACCEPT_TIMEOUT),
            async_ok_reply(0),
            "the accept queues and answers its id"
        );
        let peer = TcpStream::connect(("127.0.0.1", bound)).expect("a real loopback peer connects");
        let fd = drive_until_accepted(&mut plumbing, &mut listener, 0);
        let child = TcpInetDriver::from_accepted(fd, &listener)
            .expect("the accepted descriptor is adopted with the listener's options");
        Accepted {
            listener,
            child,
            plumbing,
            _peer: peer,
        }
    }

    /// The **accepted** driver's `getopts`(8) reply for `codes`.
    fn child_getopts(&mut self, codes: &[u8]) -> Vec<u8> {
        let Self {
            child, plumbing, ..
        } = self;
        plumbing.control(child, GETOPTS, codes)
    }

    /// The **listener**'s `getopts`(8) reply for the same codes — the reference the accepted
    /// socket's reply has to equal, and the port `tcp_inet_copy` reads them from.
    fn listener_getopts(&mut self, codes: &[u8]) -> Vec<u8> {
        let Self {
            listener,
            plumbing,
            ..
        } = self;
        plumbing.control(listener, GETOPTS, codes)
    }
}

/// The listener records covering the **whole inherited set**, every value non-default (and, for the
/// watermarks, chosen so the C's cross-clamping never rewrites them: `4096/2048` for the stream
/// pair, `12288/6000` for the busy-msgq pair).
const INHERITED_RECORDS: &[(u8, i32)] = &[
    (INET_LOPT_PACKET, 2),                 // {packet, 2}
    (INET_LOPT_PACKET_SIZE, 100),          // {packet_size, 100}
    (INET_LOPT_HEADER, 1),                 // {header, 1}
    (INET_LOPT_MODE, MODE_BINARY),         // {mode, binary}
    (INET_LOPT_DELIVER, DELIVER_PORT),     // {deliver, port}
    (INET_LOPT_EXITONCLOSE, 0),            // {exit_on_close, false}
    (INET_LOPT_BUFFER, 4096),              // {buffer, 4096}
    (INET_LOPT_TCP_HIWTRMRK, 4096),        // {high_watermark, 4096}
    (INET_LOPT_TCP_LOWTRMRK, 2048),        // {low_watermark, 2048}
    (INET_LOPT_TCP_SEND_TIMEOUT, 500),     // {send_timeout, 500}
    (INET_LOPT_TCP_SEND_TIMEOUT_CLOSE, 1), // {send_timeout_close, true}
    (INET_LOPT_TCP_SHOW_ECONNRESET, 1),    // {show_econnreset, true}
    (INET_LOPT_TCP_READ_AHEAD, 0),         // {read_ahead, false}
    (INET_LOPT_MSGQ_HIWTRMRK, 12288),      // {high_msgq_watermark, 12288}
    (INET_LOPT_MSGQ_LOWTRMRK, 6000),       // {low_msgq_watermark, 6000}
];

/// The listener records of the fields `tcp_inet_copy` **never** copies, each pushed off its default
/// so a copy would be visible: `{active, true}` (`inet_start` re-sets it passive), `{debug, true}`
/// (`DDBG_DEFAULT`), `{delay_send, true}` (outside the copy mask, `inet_drv.c:11829`-`:11831`) and
/// `{read_packets, 9}` (a `udp_descriptor` field — and, on a stream socket, an option the C accepts
/// and drops, `:7138`-`:7146`).
const DRIVER_ONLY_RECORDS: &[(u8, i32)] = &[
    (INET_LOPT_ACTIVE, ACTIVE_TRUE),
    (INET_LOPT_TCP_DELAY_SEND, 1),
    (INET_LOPT_UDP_READ_PACKETS, 9),
    (INET_LOPT_DEBUG, 1),
];

/// Every record of both halves — what [`Accepted::new`] installs, so all five groups below read a
/// listener that really holds all nineteen options.
fn all_records() -> Vec<(u8, i32)> {
    INHERITED_RECORDS
        .iter()
        .chain(DRIVER_ONLY_RECORDS)
        .copied()
        .collect()
}

/// The five `getopts` code groups, one per field family (plus the driver-only one). Each stays a
/// handful of codes so every reply is a few bytes.
const FRAMING_CODES: &[u8] = &[INET_LOPT_PACKET, INET_LOPT_PACKET_SIZE, INET_LOPT_HEADER];
const REPRESENTATION_CODES: &[u8] = &[
    INET_LOPT_MODE,
    INET_LOPT_DELIVER,
    INET_LOPT_EXITONCLOSE,
    INET_LOPT_BUFFER,
];
const WINDOW_CODES: &[u8] = &[
    INET_LOPT_TCP_HIWTRMRK,
    INET_LOPT_TCP_LOWTRMRK,
    INET_LOPT_MSGQ_HIWTRMRK,
    INET_LOPT_MSGQ_LOWTRMRK,
];
const TIMEOUT_AND_FLAG_CODES: &[u8] = &[
    INET_LOPT_TCP_SEND_TIMEOUT,
    INET_LOPT_TCP_SEND_TIMEOUT_CLOSE,
    INET_LOPT_TCP_SHOW_ECONNRESET,
    INET_LOPT_TCP_READ_AHEAD,
];
const DRIVER_ONLY_CODES: &[u8] = &[
    INET_LOPT_ACTIVE,
    INET_LOPT_TCP_DELAY_SEND,
    INET_LOPT_UDP_READ_PACKETS,
    INET_LOPT_DEBUG,
];

/// Every group, in the order the oracle dump prints them (accepted row first, listener row second).
fn groups() -> [&'static [u8]; 5] {
    [
        FRAMING_CODES,
        REPRESENTATION_CODES,
        WINDOW_CODES,
        TIMEOUT_AND_FLAG_CODES,
        DRIVER_ONLY_CODES,
    ]
}

/// Asserts the **accepted** driver of a real accept reports exactly `expected`, and that the
/// listener it came from reports the identical bytes — the C's `tcp_inet_copy` as the wire shows it.
fn assert_inherited(codes: &[u8], expected: &[(u8, i32)]) {
    let mut accepted = Accepted::new(&all_records());
    let child = accepted.child_getopts(codes);
    let listener = accepted.listener_getopts(codes);
    eprintln!("codes {codes:?}: accepted {child:?}, listener {listener:?}");
    assert_eq!(
        child,
        expected_getopts(expected),
        "the accepted socket reports what `tcp_inet_copy` copies (`inet_drv.c:11809`-`:11844`)"
    );
    assert_eq!(
        listener, child,
        "the accepted port and its listener agree code for code"
    );
}

/// The `{packet, …}`/`{packet_size, …}`/`{header, …}` framing group on a **real** accepted socket:
/// `inet.htype`/`inet.psize`/`inet.hsz` (`inet_drv.c:11818`/`:11819`/`:11822`).
#[test]
fn framing_options_survive_a_real_accept() {
    assert_inherited(
        FRAMING_CODES,
        &[
            (INET_LOPT_PACKET, 2),
            (INET_LOPT_PACKET_SIZE, 100),
            (INET_LOPT_HEADER, 1),
        ],
    );
}

/// The `{mode, …}`/`{deliver, …}`/`{exit_on_close, …}` representation group plus `{buffer, …}` on a
/// **real** accepted socket (`:11815`/`:11817`/`:11816`/`:11823`).
#[test]
fn representation_options_survive_a_real_accept() {
    assert_inherited(
        REPRESENTATION_CODES,
        &[
            (INET_LOPT_MODE, MODE_BINARY),
            (INET_LOPT_DELIVER, DELIVER_PORT),
            (INET_LOPT_EXITONCLOSE, 0),
            (INET_LOPT_BUFFER, 4096),
        ],
    );
}

/// The four window values on a **real** accepted socket — the stream watermarks
/// (`:11824`/`:11825`) and the busy-msgq limits the C reads off the parent port and writes to the
/// child (`erl_drv_busy_msgq_limits`, `:11842`/`:11844`).
#[test]
fn window_options_survive_a_real_accept() {
    assert_inherited(
        WINDOW_CODES,
        &[
            (INET_LOPT_TCP_HIWTRMRK, 4096),
            (INET_LOPT_TCP_LOWTRMRK, 2048),
            (INET_LOPT_MSGQ_HIWTRMRK, 12288),
            (INET_LOPT_MSGQ_LOWTRMRK, 6000),
        ],
    );
}

/// The send-timeout pair (`:11826`/`:11827`) and the only two `tcp_add_flags` bits the C keeps
/// (`SHOW_ECONNRESET`/`NO_READ_AHEAD`, `:11829`-`:11831`) on a **real** accepted socket.
#[test]
fn timeout_and_flag_options_survive_a_real_accept() {
    assert_inherited(
        TIMEOUT_AND_FLAG_CODES,
        &[
            (INET_LOPT_TCP_SEND_TIMEOUT, 500),
            (INET_LOPT_TCP_SEND_TIMEOUT_CLOSE, 1),
            (INET_LOPT_TCP_SHOW_ECONNRESET, 1),
            (INET_LOPT_TCP_READ_AHEAD, 0),
        ],
    );
}

/// The negative half on a **real** accepted socket: a listener holding `{active, true}`,
/// `{debug, true}` and `{delay_send, true}` yields an accepted driver reporting the C's defaults
/// for all three, and `{read_packets, …}` is not reported at all (it is datagram-gated on both
/// sides, so a TCP port cannot observe it — the unit half pins the store).
///
/// `{delay_send, true}` in particular must **not** appear on the accepted driver: the C's copy mask
/// omits `TCP_ADDF_DELAY_SEND` (`inet_drv.c:11829`-`:11831`), so `gen_tcp:accept/1` shows it only
/// because the *Erlang* half re-applies it — `prim_inet:accept_opts/3`
/// (`erts/preloaded/src/prim_inet.erl:503`-`:517`), which is deliberately not duplicated here.
/// `{active, …}` is the same story (`inet_start` re-sets it passive, and that function re-applies
/// it too), and `{debug, …}` is simply outside the copy list.
#[test]
fn driver_only_options_are_reset_on_a_real_accepted_socket() {
    let mut accepted = Accepted::new(&all_records());
    let child = accepted.child_getopts(DRIVER_ONLY_CODES);
    let listener = accepted.listener_getopts(DRIVER_ONLY_CODES);
    eprintln!("driver-only codes: accepted {child:?}, listener {listener:?}");

    // The accepted socket: `{active, false}`, `{delay_send, false}`, `{debug, false}` — and no
    // record at all for `{read_packets, …}`, because the C's `getopts` arm is datagram-gated
    // (`:9268`-`:9280`) and so is this floor's.
    assert_eq!(
        child,
        expected_getopts(&[
            (INET_LOPT_ACTIVE, 0),
            (INET_LOPT_TCP_DELAY_SEND, 0),
            (INET_LOPT_DEBUG, 0),
        ]),
        "the accepted socket starts passive, without delay-send or debug"
    );
    // The listener really did hold the other values, so the assertion above is not vacuous.
    assert_eq!(
        listener,
        expected_getopts(&[
            (INET_LOPT_ACTIVE, ACTIVE_TRUE),
            (INET_LOPT_TCP_DELAY_SEND, 1),
            (INET_LOPT_DEBUG, 1),
        ]),
        "the listener holds every driver-only option"
    );
    assert_ne!(
        child, listener,
        "none of the driver-only options may cross `accept(2)`"
    );
}

/// `bytes` as the comma-separated decimals the oracle's binary literal and reply dump use.
fn decimals(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(u8::to_string)
        .collect::<Vec<String>>()
        .join(",")
}

/// The `erl` expression that measures the C's own `tcp_inet_copy` — the system driver creates its
/// accepted socket **inside the C**, so this drives both ports itself and never goes through
/// `prim_inet:accept_opts/3` (which would re-apply the non-inherited half and hide the answer):
///
/// * `P` — a real `tcp_inet` listener port (`open_port({spawn_driver,"tcp_inet"}, [binary])`);
/// * the same `setopts`(7) records this suite installs, `bind`(6) `127.0.0.1:0`, `listen`(27);
/// * a real `gen_tcp:connect/3` peer, then `accept`(26) → `[1, IdHi, IdLo]`;
/// * the `{inet_async, P, Id, {ok, Child}}` completion, where `Child` is the port the **C** made;
/// * `getopts`(8) for each code group on **both** `Child` and `P`, printed by `J` as a `~w` integer
///   list followed by `|` (a raw byte dump is not safe — an ephemeral port is two legal `|` bytes),
///   the accepted socket first.
///
/// A missing completion is `erlang:error(accept_timeout)`: the tool was present and the C was
/// driven, so that is a harness fault and must fail rather than skip. Only [`erl_stdout`]'s own
/// "no `erl` / no answer in time" case is a skip.
fn oracle_expression(groups: &[&[u8]]) -> String {
    let mut setopts = Vec::new();
    for (code, value) in all_records() {
        setopts.extend_from_slice(&record(code, value));
    }
    let mut fields = Vec::new();
    for codes in groups {
        fields.push(format!("Sh(G(Child, <<{}>>))", decimals(codes)));
        fields.push(format!("Sh(G(P, <<{}>>))", decimals(codes)));
    }
    format!(
        "M = {CONTROL_MAGIC}, \
         P = open_port({{spawn_driver, \"tcp_inet\"}}, [binary]), \
         F = fun(C, A) -> binary_to_list(iolist_to_binary(erlang:port_control(P, C + M, A))) end, \
         G = fun(S, Codes) -> binary_to_list(iolist_to_binary(erlang:port_control(S, 8 + M, Codes))) end, \
         Sh = fun(L) -> lists:flatten(lists:join(\",\", [integer_to_list(B) || B <- L])) end, \
         [1] = F(1, <<1,1,0>>), \
         [1] = F(7, <<{}>>), \
         [1, PH, PL] = F(6, <<1,0,0,127,0,0,1>>), \
         Bound = PH * 256 + PL, \
         true = Bound > 0, \
         [1] = F(27, <<0,5>>), \
         {{ok, _Peer}} = gen_tcp:connect({{127,0,0,1}}, Bound, [binary, {{active,false}}]), \
         [1, IH, IL] = F(26, <<0,0,19,136>>), \
         Id = IH * 256 + IL, \
         Child = receive {{inet_async, P, Id, {{ok, Sock}}}} -> Sock \
                 after 20000 -> erlang:error(accept_timeout) end, \
         Rows = [{}], \
         io:format(\"ORACLE|~s~n\", [lists:flatten(lists:join(\"|\", Rows))])",
        decimals(&setopts),
        fields.join(", ")
    )
}/// The oracle row: the system driver's accepted socket reports the listener's inherited options
/// and the **C's** defaults for the ones it does not copy — measured, not assumed.
///
/// Compares three ways per group: the oracle's accepted-socket reply against ours, the oracle's
/// listener reply against ours, and (for the inherited groups) the two oracle rows against each
/// other — which is the `tcp_inet_copy` statement itself. The last group is the negative half: the
/// C's own accepted socket reports `{active, 0}`, `{delay_send, 0}`, `{debug, 0}` and no
/// `{read_packets, …}` record at all, so the dump's two rows there **must** differ.
#[test]
fn the_oracle_measures_tcp_inet_copy_on_a_real_accepted_socket() {
    let groups = groups();
    let Some(stdout) = erl_stdout(&oracle_expression(&groups)) else {
        eprintln!("SKIP: no `erl` — the `tcp_inet_copy` oracle rows were not compared");
        return;
    };
    eprintln!("the `tcp_inet_copy` oracle dump: {stdout}");
    // The listener carries `{debug, true}` (that is how the negative group shows the C's default),
    // so the C's own driver prints `INET-DRV-DBG` lines on stdout while it works. The row line is
    // therefore **marker-prefixed** and printed in a single `io:format` after the accept, and the
    // rows are decimal fields joined by `|` (a raw byte dump could not be split — an ephemeral port
    // is two perfectly legal `|` bytes).
    let Some(rows) = stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("ORACLE|"))
    else {
        panic!("the oracle printed no `ORACLE|` row line: {stdout:?}");
    };
    let oracle = oracle_replies(rows);
    assert_eq!(
        oracle.len(),
        2 * groups.len(),
        "one accepted row and one listener row per group: {oracle:?}"
    );

    let mut accepted = Accepted::new(&all_records());
    for (index, codes) in groups.iter().enumerate() {
        let child = accepted.child_getopts(codes);
        let listener = accepted.listener_getopts(codes);
        let oracle_child = &oracle[2 * index];
        let oracle_listener = &oracle[2 * index + 1];
        eprintln!(
            "{codes:?}: oracle accepted {oracle_child:?}, listener {oracle_listener:?}; \
             rust accepted {child:?}, listener {listener:?}"
        );
        assert_eq!(
            &child, oracle_child,
            "codes {codes:?}: the accepted socket's `getopts` reply disagrees with the C's"
        );
        assert_eq!(
            &listener, oracle_listener,
            "codes {codes:?}: the listener's own `getopts` reply disagrees with the C's"
        );
        if index + 1 < groups.len() {
            assert_eq!(
                oracle_child, oracle_listener,
                "codes {codes:?}: the C's `tcp_inet_copy` makes the accepted port report its \
                 listener's value"
            );
        }
    }
    // The negative group: the C's accepted socket carries none of the listener's driver-only
    // values, while the C's listener carries all of them — the C's own proof that the copy list is
    // a subset, i.e. the same assertion as the Rust half's `assert_ne!` on the same two rows.
    let negative = groups.len() - 1;
    assert_ne!(
        oracle[2 * negative], oracle[2 * negative + 1],
        "the C's accepted socket must not report the listener's driver-only options: {oracle:?}"
    );
}
