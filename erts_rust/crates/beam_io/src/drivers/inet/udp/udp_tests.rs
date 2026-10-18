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

//! The `udp_inet` driver's in-module suite: real loopback datagram round trips through
//! [`UdpInetDriver`], the control-reply classes, the option wire, and the `erl`-oracle
//! cross-check of the verb replies.
//!
//! The oracle cases drive a real `udp_inet` port with `port_control(Port, Magic + code, Arg)`
//! (the same technique [`super::super::opt`]'s and [`super::super::tests`]' own suites use: the
//! magic offset is `ERTS_INET_DRV_CONTROL_MAGIC_NUMBER`, `prim_inet.erl:2969`, and the request
//! numbers are the C's `INET_REQ_*`/`PACKET_REQ_RECV`) and compare the reply bytes to the
//! driver's. Every oracle case **skips loudly** when `erl` is not on `PATH`.

use super::super::opt::{
    INET_LOPT_DELIVER, INET_LOPT_MODE, INET_LOPT_UDP_READ_PACKETS, INET_OPT_BROADCAST,
    INET_OPT_REUSEADDR, INET_OPT_TTL,
};
use super::super::wire::{ctl_ok, ctl_xerror, split_caller_ref, ETF_VERSION_MAGIC};
use super::super::{INET_AF_ANY, INET_AF_INET, INET_AF_LOOPBACK, INET_TYPE_DGRAM, INET_REP_OK};
use super::{UdpInetDriver, UDP_DRIVER_NAME};
use crate::driver::{Driver, DriverContext, PortArms};
use crate::poller::Poller;
use crate::queue::ByteQueue;
use beam_common::{BeamError, Result};
use beam_process::PortId;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// The C's `INET_REQ_*` request numbers and the magic offset the Erlang side adds
/// (`inet_drv.c:805`-`:847`, `prim_inet.erl:2969`).
const CONTROL_MAGIC: u32 = 0x03f1_a300;
/// `INET_REQ_OPEN` (`inet_drv.c:806`).
const INET_REQ_OPEN: u32 = 1;
/// `INET_REQ_CLOSE` (`inet_drv.c:807`).
const INET_REQ_CLOSE: u32 = 2;
/// `INET_REQ_BIND` (`inet_drv.c:810`).
const INET_REQ_BIND: u32 = 6;
/// `INET_REQ_SETOPTS` (`inet_drv.c:811`).
const INET_REQ_SETOPTS: u32 = 7;
/// `INET_REQ_GETOPTS` (`inet_drv.c:812`).
const INET_REQ_GETOPTS: u32 = 8;

/// Wall-clock budget for the `erl` oracle before the affected check is skipped.
const ERL_TIMEOUT: Duration = Duration::from_secs(30);

/// The three-byte `open` argument: `[AF_INET, SOCK_DGRAM, default protocol]`.
fn open_arg() -> [u8; 3] {
    [INET_AF_INET, INET_TYPE_DGRAM, super::super::INET_PROTO_DEFAULT]
}

/// The packed `INET_AF_INET` address of `127.0.0.1:port`.
fn loopback_arg(port: u16) -> Vec<u8> {
    let [hi, lo] = port.to_be_bytes();
    vec![INET_AF_INET, hi, lo, 127, 0, 0, 1]
}

/// A driver plus the port state a `DriverContext` needs, so a verb can be driven directly
/// without a `PortRegistry` (the same shape `super::super::tests`' rig uses).
struct Rig {
    driver: UdpInetDriver,
    poller: Poller,
    input: ByteQueue,
    output: ByteQueue,
    arms: PortArms,
}

impl Rig {
    /// A started, socket-less driver.
    fn new() -> Rig {
        let mut rig = Rig {
            driver: UdpInetDriver::new(),
            poller: Poller::new().expect("a poller"),
            input: ByteQueue::new(),
            output: ByteQueue::new(),
            arms: PortArms::default(),
        };
        rig.start().expect("start");
        rig
    }

    /// [`Driver::start`]. A fresh context per call, as `crate::port::Port` builds one.
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

    /// One control verb.
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

    /// The bytes the driver has read into the port input queue, consumed.
    fn take_input(&mut self) -> Vec<u8> {
        let bytes = self.input.front().to_vec();
        self.input.consume(bytes.len());
        bytes
    }

    /// `open` with the datagram argument, returning the reply.
    fn open(&mut self) -> Vec<u8> {
        let arg = open_arg();
        self.control("open", &arg).expect("open")
    }

    /// `open` + `bind` to an ephemeral loopback port, returning the port the kernel chose.
    fn bind_ephemeral(&mut self) -> u16 {
        let opened = self.control("open", &open_arg()).expect("open");
        assert_eq!(opened, ctl_ok(), "open answers [1]");
        let bound = self.control("bind", &[INET_AF_ANY, 0, 0]).expect("bind");
        assert_eq!(bound[0], INET_REP_OK, "bind succeeds");
        u16::from_be_bytes([bound[1], bound[2]])
    }

    /// `send` `payload` to `127.0.0.1:port`, returning the reply.
    fn send(&mut self, port: u16, payload: &[u8]) -> Vec<u8> {
        let mut arg = loopback_arg(port);
        arg.extend_from_slice(payload);
        self.control("send", &arg).expect("send")
    }

    /// `recv`, returning the reply.
    fn recv(&mut self) -> Vec<u8> {
        self.control("recv", &[]).expect("recv")
    }

    /// Queues `bytes` on the port output queue, as `port_command/2` does.
    fn queue_output(&mut self, bytes: &[u8]) {
        self.output.push(bytes);
    }

    /// How many bytes are waiting in the port output queue.
    fn output_len(&self) -> usize {
        self.output.len()
    }

    /// [`Driver::output`] — the port-task write path, where a queued command is drained.
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
}

// ---------------------------------------------------------------------------
// A socket-less / unconnected port refuses a port command (`inet_drv.c:15051`)
// ---------------------------------------------------------------------------

/// A `port_command/2` on a port with **no socket** is refused, and the queued bytes survive: the
/// C's `packet_inet_command` guards the send with `!IS_OPEN` (`inet_drv.c:15051`), and its queue
/// form expects the `[address][data]` grammar — a bare command carries no address at all, so
/// nothing in the queue can name a peer.
///
/// The driver-side contract is a typed `BeamError::BadArg` with the bytes **never consumed**.
/// The oracle's own first command answers `true` *and kills the port* (`driver_failure_posix`),
/// which needs a `driver_failure` seam this crate's `Driver` trait does not have; see
/// `the_socket_less_command_sequence_matches_the_oracle`.
#[test]
fn a_socket_less_port_command_is_refused_and_keeps_its_bytes() {
    let mut rig = Rig::new();
    rig.queue_output(b"\x01\x02\x03");
    assert_eq!(rig.output_len(), 3, "the command is queued");
    assert!(
        matches!(rig.drain_output(), Err(BeamError::BadArg)),
        "a socket-less datagram command is refused"
    );
    assert_eq!(
        rig.output_len(),
        3,
        "the refused command's bytes are never consumed"
    );
}

/// The same refusal once a socket exists but has no default peer: a datagram needs a destination
/// and the queue cannot name one, so the command is refused rather than silently dropped.
#[test]
fn an_unconnected_datagram_command_is_refused_and_keeps_its_bytes() {
    let mut rig = Rig::new();
    rig.open();
    rig.queue_output(b"\x01\x02\x03");
    assert!(
        matches!(rig.drain_output(), Err(BeamError::BadArg)),
        "an unconnected datagram command is refused"
    );
    assert_eq!(rig.output_len(), 3, "the bytes are never consumed");
}

/// The oracle's side of the same situation, pinned as a **sequence**: on a fresh bare port the
/// first `port_command/2` answers `true` and the port is then dead (`port_info(P, name)` is
/// `undefined`, the C's `driver_failure_posix(EINVAL)`), after which every command raises
/// `badarg`.
///
/// The residual difference is deliberate and asserted, not glossed over: reproducing the port
/// kill needs a `driver_failure` seam this crate does not have, so the Rust side refuses the
/// **first** delivered round with a typed `BeamError::BadArg` and keeps the bytes. The oracle's
/// `true` is therefore **not** compared to our reply — only the sequence (first command accepted,
/// port afterwards unusable) is pinned.
///
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn the_socket_less_command_sequence_matches_the_oracle() {
    let expr = "process_flag(trap_exit, true), \
                P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
                First = (catch erlang:port_command(P, <<1>>)), \
                Dead = erlang:port_info(P, name), \
                Second = (catch erlang:port_command(P, <<1>>)), \
                io:format(\"~p|~p|~p\", [First, Dead, Second])";
    let Some(dump) = erl_stdout(expr) else {
        eprintln!("SKIP: no `erl` on PATH — skipping the socket-less command oracle check");
        return;
    };
    let dump = String::from_utf8_lossy(&dump);
    assert!(
        dump.starts_with("true|undefined|"),
        "the oracle accepts the first command and kills the port: {dump:?}"
    );
    assert!(
        dump.contains("badarg"),
        "the oracle refuses every command after the port died (measured: a caught, \
         exit-shaped badarg raise): {dump:?}"
    );

    // Our side: the very first delivered round is refused, and the bytes stay queued.
    let mut rig = Rig::new();
    rig.queue_output(b"\x01");
    assert!(
        matches!(rig.drain_output(), Err(BeamError::BadArg)),
        "the Rust side refuses the first round (the oracle answers `true` and kills the port — \
         a `driver_failure` seam this driver has no access to)"
    );
    assert_eq!(rig.output_len(), 1, "and the refused bytes stay queued");
}

// ---------------------------------------------------------------------------
// The control verbs over real loopback datagram sockets
// ---------------------------------------------------------------------------

#[test]
fn the_registered_name_is_udp_inet() {
    assert_eq!(UDP_DRIVER_NAME, "udp_inet");
    assert_eq!(UdpInetDriver::new().name(), "udp_inet");
}

#[test]
fn open_bind_and_close_round_trip() {
    let mut rig = Rig::new();
    // `bind_ephemeral` performs the `open` and asserts its `[1]` reply.
    let port = rig.bind_ephemeral();
    // A bound socket reports its own port; the ephemeral one must be non-zero.
    assert_ne!(port, 0, "the kernel assigned a port");
    assert_eq!(rig.control("close", &[]).expect("close"), ctl_ok());
    // A close is idempotent, as in the C.
    assert_eq!(rig.control("close", &[]).expect("close again"), ctl_ok());
    assert!(rig.driver.socket().is_none(), "the socket was released");
}

#[test]
fn open_rejects_a_second_socket() {
    let mut rig = Rig::new();
    rig.open();
    assert_eq!(
        rig.control("open", &open_arg()).expect("second open"),
        ctl_xerror("einval"),
    );
}

#[test]
fn open_mirrors_the_c_error_classes() {
    let mut rig = Rig::new();
    // An unserved family is `eafnosupport`.
    assert_eq!(
        rig.control("open", &[99, INET_TYPE_DGRAM, 0]).expect("open"),
        ctl_xerror("eafnosupport"),
    );
    // A non-datagram type is `einval` (this floor's `udp_inet` is a datagram driver).
    assert_eq!(
        rig.control("open", &[INET_AF_INET, 1, 0]).expect("open"),
        ctl_xerror("einval"),
    );
    // A protocol that is neither default nor UDP is `eprotonosupport`.
    assert_eq!(
        rig.control("open", &[INET_AF_INET, INET_TYPE_DGRAM, 6]).expect("open"),
        ctl_xerror("eprotonosupport"),
    );
    // The UDP protocol is accepted.
    assert_eq!(
        rig.control("open", &[INET_AF_INET, INET_TYPE_DGRAM, 2]).expect("open"),
        ctl_ok(),
    );
}

#[test]
fn a_malformed_open_argument_is_a_badarg() {
    let mut rig = Rig::new();
    assert!(matches!(
        rig.control("open", &[INET_AF_INET, INET_TYPE_DGRAM]),
        Err(BeamError::BadArg)
    ));
}

#[test]
fn a_malformed_address_is_a_badarg() {
    let mut rig = Rig::new();
    rig.open();
    assert!(matches!(
        rig.control("bind", &[99, 0, 0]),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        rig.control("send", &[99, 0, 0, b'x']),
        Err(BeamError::BadArg)
    ));
}

#[test]
fn bind_twice_reaches_the_kernel() {
    let mut rig = RustRig::bind();
    let reply = rig.control("bind", &[INET_AF_ANY, 0, 0]).expect("second bind");
    assert_eq!(reply[0], 0, "the kernel refuses a second bind");
    assert_eq!(reply, ctl_xerror("einval"));
}

/// Alias so the verbose binder reads short in tests.
struct RustRig;
impl RustRig {
    fn bind() -> Rig {
        let mut rig = Rig::new();
        rig.bind_ephemeral();
        rig
    }
}

#[test]
fn bind_without_a_socket_is_exbadport() {
    let mut rig = Rig::new();
    assert_eq!(
        rig.control("bind", &[INET_AF_ANY, 0, 0]).expect("bind"),
        ctl_xerror("exbadport"),
    );
}

#[test]
fn a_datagram_reaches_the_other_port_with_its_source() {
    // The receiver binds; the sender is unconnected and names it explicitly.
    let mut receiver = Rig::new();
    let port = receiver.bind_ephemeral();
    let mut sender = Rig::new();
    sender.open();
    assert_eq!(sender.send(port, b"hello"), ctl_ok(), "sendto succeeds");

    let reply = receiver.recv();
    // The reply carries the source address: `[1] ++ [AF_INET, hi, lo, 127, 0, 0, 1]`.
    assert_eq!(reply.len(), 8, "a datagram reply carries the source");
    assert_eq!(reply[0], INET_REP_OK);
    assert_eq!(reply[1], INET_AF_INET);
    assert_eq!(&reply[4..8], &[127, 0, 0, 1], "the sender is loopback");
    assert_ne!(u16::from_be_bytes([reply[2], reply[3]]), 0);
    assert_eq!(receiver.take_input(), b"hello", "the payload is delivered");
}

#[test]
fn recv_without_a_datagram_replies_the_bare_ok() {
    let mut receiver = Rig::new();
    receiver.bind_ephemeral();
    let reply = receiver.recv();
    assert_eq!(reply, ctl_ok(), "a drained socket answers just [1]");
    assert!(receiver.take_input().is_empty());
}

#[test]
fn a_connected_socket_sends_without_naming_the_peer() {
    let mut receiver = Rig::new();
    let port = receiver.bind_ephemeral();
    let mut sender = Rig::new();
    sender.open();
    // Connect to the loopback form the C's `INET_REQ_CONNECT` uses.
    let mut arg = vec![0, 0, 0, 0];
    arg.extend_from_slice(&[INET_AF_LOOPBACK, port.to_be_bytes()[0], port.to_be_bytes()[1]]);
    assert_eq!(sender.control("connect", &arg).expect("connect"), ctl_ok());
    assert!(sender.driver.state().is_connected());

    // The address is still parsed and consumed, but it is not used for routing.
    let mut send_arg = loopback_arg(1);
    send_arg.extend_from_slice(b"world");
    assert_eq!(sender.control("send", &send_arg).expect("send"), ctl_ok());

    let reply = receiver.recv();
    assert_eq!(reply.len(), 8);
    assert_eq!(receiver.take_input(), b"world");
}

#[test]
fn connect_without_a_socket_is_exbadport() {
    let mut rig = Rig::new();
    assert_eq!(
        rig.control("connect", &[INET_AF_LOOPBACK, 0, 0]).expect("connect"),
        ctl_xerror("exbadport"),
    );
}

#[test]
fn dissolve_connect_returns_the_socket_to_open() {
    let mut rig = Rig::new();
    rig.open();
    assert_eq!(
        rig.control("connect", &[INET_AF_LOOPBACK, 0, 0]).expect("connect"),
        ctl_ok(),
    );
    assert_eq!(rig.control("connect", &[]).expect("dissolve"), ctl_ok());
    assert!(!rig.driver.state().is_connected(), "the peer was dropped");
}

#[test]
fn send_without_a_socket_is_einval() {
    let mut rig = Rig::new();
    assert_eq!(rig.send(1, b"x"), ctl_xerror("einval"));
}

#[test]
fn recv_rejects_a_nonempty_argument() {
    let mut rig = Rig::new();
    rig.bind_ephemeral();
    assert!(matches!(rig.recv_arg(&[0, 0, 0, 1, 0, 0, 0, 1]), Err(BeamError::BadArg)));
}

impl Rig {
    /// `recv` with an explicit argument, for the argument-validation case.
    fn recv_arg(&mut self, arg: &[u8]) -> Result<Vec<u8>> {
        self.control("recv", arg)
    }
}

#[test]
fn an_unknown_verb_is_a_badarg() {
    let mut rig = Rig::new();
    assert!(matches!(rig.control("nonsense", &[]), Err(BeamError::BadArg)));
}

// ---------------------------------------------------------------------------
// The option wire
// ---------------------------------------------------------------------------

#[test]
fn the_datagram_syscall_options_round_trip() {
    let mut rig = Rig::new();
    rig.open();
    for (code, value) in [
        (INET_OPT_REUSEADDR, 1i32),
        (INET_OPT_BROADCAST, 1i32),
        (INET_OPT_TTL, 42i32),
    ] {
        let mut set = vec![code];
        set.extend_from_slice(&value.to_be_bytes());
        assert_eq!(rig.control("setopts", &set).expect("setopts"), ctl_ok());
        let read_back = rig.control("getopts", &[code]).expect("getopts");
        let mut expected = vec![INET_REP_OK, code];
        expected.extend_from_slice(&value.to_be_bytes());
        assert_eq!(read_back, expected, "code {code} reads back");
    }
}

#[test]
fn an_unserved_option_code_is_einval() {
    let mut rig = Rig::new();
    rig.open();
    assert_eq!(
        rig.control("getopts", &[200]).expect("getopts"),
        ctl_xerror("einval"),
    );
    let mut set = vec![200, 0, 0, 0, 1];
    set.push(0);
    assert_eq!(
        rig.control("setopts", &set).expect("setopts"),
        ctl_xerror("einval"),
    );
}

/// A five-byte `[opt, val_be32]` `setopts` record.
fn record(code: u8, value: i32) -> Vec<u8> {
    let mut record = vec![code];
    record.extend_from_slice(&value.to_be_bytes());
    record
}

/// `{mode, …}` (`24`) and `{deliver, …}` (`25`) are **ungated** — the C's `inet_set_opts`
/// (`inet_drv.c:6939`-`:6956`) and `inet_fill_opts` (`:9168`-`:9175`) carry no `stype` test — so
/// the datagram driver serves both exactly as `tcp_inet` does, in every socket state.
/// `{read_packets, N}` (`33`) is the datagram-gated one: it is served once `open` has cached
/// `SOCK_DGRAM`, ignored before that (still answering `[1]`), and the C's `ival <= 0` refusal
/// applies only inside the gate.
#[test]
fn the_datagram_driver_state_options_are_served() {
    let mut rig = Rig::new();
    // ---- no socket yet: `inet.stype` is -1, so 33 is gated out ----
    let reply = rig.control("getopts", &[24, 25, 33]).expect("getopts");
    assert_eq!(
        reply,
        vec![INET_REP_OK, 24, 0, 0, 0, 0, 25, 0, 0, 0, 1],
        "code 33 contributes nothing while there is no datagram socket",
    );
    assert_eq!(
        rig.control("setopts", &record(24, 1)).expect("setopts"),
        ctl_ok(),
        "a `mode` = binary store is accepted before a socket exists",
    );
    assert_eq!(
        rig.control("setopts", &record(25, 0)).expect("setopts"),
        ctl_ok(),
        "a `deliver` = port store is accepted before a socket exists",
    );
    assert_eq!(
        rig.control("setopts", &record(33, 7)).expect("setopts"),
        ctl_ok(),
        "a gated-out read_packets store still answers `[1]`",
    );
    assert_eq!(
        rig.control("setopts", &record(33, 0)).expect("setopts"),
        ctl_ok(),
        "the `ival <= 0` refusal is inside the datagram gate",
    );
    assert_eq!(
        rig.control("getopts", &[24, 25, 33]).expect("getopts"),
        vec![INET_REP_OK, 24, 0, 0, 0, 1, 25, 0, 0, 0, 0],
        "both ungated records read back, the gated one still contributes nothing",
    );

    // ---- an opened datagram socket: the gated code comes alive ----
    rig.open();
    let defaults = rig.control("getopts", &[24, 25, 33]).expect("getopts");
    assert_eq!(
        defaults,
        vec![INET_REP_OK, 24, 0, 0, 0, 1, 25, 0, 0, 0, 0, 33, 0, 0, 0, 5],
        "`mode`/`deliver` keep their stored values and `read_packets` reports its default",
    );
    assert_eq!(
        rig.control("setopts", &record(33, 7)).expect("setopts"),
        ctl_ok(),
    );
    assert_eq!(
        rig.control("getopts", &[33]).expect("getopts"),
        vec![INET_REP_OK, 33, 0, 0, 0, 7],
        "a positive read_packets stores and reads back",
    );
    // A non-positive `read_packets` is the C's `[0]einval` — inside the datagram gate.
    assert_eq!(
        rig.control("setopts", &record(33, 0)).expect("setopts"),
        ctl_xerror("einval"),
    );
    assert_eq!(
        rig.control("getopts", &[33]).expect("getopts"),
        vec![INET_REP_OK, 33, 0, 0, 0, 7],
        "a refused record leaves the stored value alone",
    );

    // ---- after `close`: the cached `SOCK_DGRAM` keeps the read-back alive ----
    assert_eq!(rig.control("close", &[]).expect("close"), ctl_ok());
    assert_eq!(
        rig.control("getopts", &[24, 25, 33]).expect("getopts"),
        vec![INET_REP_OK, 24, 0, 0, 0, 1, 25, 0, 0, 0, 0, 33, 0, 0, 0, 7],
        "a close never clears `desc->stype`, so code 33 is still reported",
    );
    assert_eq!(
        rig.control("setopts", &record(33, 0)).expect("setopts"),
        ctl_xerror("einval"),
        "the gate is still open after a close",
    );
}

// ---------------------------------------------------------------------------
// The `erl` oracle cross-check
// ---------------------------------------------------------------------------

/// Runs `expr` under the system `erl`, returning its **raw stdout bytes**, or `None` when `erl`
/// is absent or the run does not finish inside [`ERL_TIMEOUT`] (the caller then **skips loudly**).
///
/// The bytes are handed over untouched: the dump is the driver's own reply bytes, and a
/// `String::from_utf8_lossy` would replace every byte above `127` with `U+FFFD` — which the
/// ephemeral port a `bind` reply carries routinely are.
fn erl_stdout(expr: &str) -> Option<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    let script = format!("{expr}, halt(0).");
    thread::spawn(move || {
        let out = Command::new("erl")
            .args(["-noshell", "-eval", &script])
            .output();
        let _ = tx.send(out);
    });
    match rx.recv_timeout(ERL_TIMEOUT) {
        Ok(Ok(out)) => Some(out.stdout),
        Ok(Err(_)) | Err(_) => None,
    }
}

/// The `erl` expression driving an oracle `udp_inet` port: open the driver port, run `calls`
/// (each `(request code, argument bytes)`) through `port_control`, and print the raw reply bytes
/// joined by `|` so the caller can compare them byte for byte.
///
/// `standard_io` is switched to `latin1` first: with the default (`unicode`) encoding,
/// `io:format("~s", …)` would UTF-8-encode any byte above `127` — and a `bind` reply carries the
/// kernel-assigned ephemeral port, whose bytes routinely are — so the oracle's dump would no
/// longer be the driver's raw reply bytes.
fn oracle_expression(calls: &[(u32, Vec<u8>)]) -> String {
    let mut body = String::new();
    for (code, arg) in calls {
        let arg_list = arg
            .iter()
            .map(|b| b.to_string())
            .collect::<Vec<_>>()
            .join(",");
        body.push_str(&format!(
            "erlang:port_control(P, {code} + {CONTROL_MAGIC}, <<{arg_list}>>) ++ \"|\" ++ "
        ));
    }
    format!(
        "P = open_port({{spawn_driver, \"udp_inet\"}}, [binary]), \
         io:setopts(standard_io, [{{encoding, latin1}}]), \
         io:format(\"~s\", [{body}\"\"]), \
         erlang:port_close(P)"
    )
}

/// Splits the oracle's `A|B|C` raw-byte dump into the reply byte strings.
fn oracle_replies(dump: &[u8]) -> Vec<Vec<u8>> {
    dump.split(|b| *b == b'|')
        .filter(|part| !part.is_empty())
        .map(|part| part.to_vec())
        .collect()
}

/// The **compatibility gate** for the datagram control verbs: the same `open`/`bind`/`close`
/// sequence the Rust driver is given, driven against the oracle's `udp_inet` port through
/// `port_control`, compared byte for byte.
///
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn the_control_replies_agree_with_the_erl_oracle() {
    let calls = vec![
        (INET_REQ_OPEN, vec![INET_AF_INET, INET_TYPE_DGRAM, 0]),
        (INET_REQ_BIND, vec![INET_AF_ANY, 0, 0]),
        (INET_REQ_CLOSE, vec![]),
    ];
    let Some(dump) = erl_stdout(&oracle_expression(&calls)) else {
        eprintln!("SKIP: no `erl` on PATH — skipping the udp_inet control-reply oracle check");
        return;
    };
    let oracle = oracle_replies(&dump);
    assert_eq!(oracle.len(), 3, "the oracle answered every call: {dump:?}");

    // The Rust side: the same calls, byte for byte (the bind reply's ephemeral port is the one
    // value that legitimately differs, so only its shape is compared).
    let mut rig = Rig::new();
    let opened = rig.control("open", &open_arg()).expect("open");
    assert_eq!(opened, oracle[0], "open agrees with the oracle");
    let bound = rig.control("bind", &[INET_AF_ANY, 0, 0]).expect("bind");
    assert_eq!(bound.len(), oracle[1].len(), "bind reply length agrees");
    assert_eq!(bound[0], oracle[1][0], "bind result tag agrees");
    let closed = rig.control("close", &[]).expect("close");
    assert_eq!(closed, oracle[2], "close agrees with the oracle");
}

/// The Rust side of an oracle comparison driven the way the runtime drives it: a **real**
/// `PortRegistry` port on the numeric `erlang:port_control/3` route (the decimal
/// `code + ERTS_INET_DRV_CONTROL_MAGIC_NUMBER` string the `opcode.rs` bridge decodes).
fn port_replies(calls: &[(u32, Vec<u8>)]) -> Vec<Vec<u8>> {
    let mut poller = Poller::new().expect("a poll set");
    let registry = crate::driver::DriverRegistry::with_builtins();
    let driver = registry
        .create(UDP_DRIVER_NAME)
        .expect("the built-in udp_inet driver is registered");
    let mut ports = crate::port::PortRegistry::new();
    let id = ports.open(&mut poller, driver).expect("the port opens");
    let replies = calls
        .iter()
        .map(|(code, arg)| {
            ports
                .port_control(&mut poller, id, &(code + CONTROL_MAGIC).to_string(), arg)
                .unwrap_or_else(|err| panic!("request {code} failed: {err:?}"))
        })
        .collect();
    ports.close(&mut poller, id).expect("closes the port");
    replies
}

/// The datagram driver's `{mode, …}`/`{deliver, …}`/`{read_packets, N}` matrix against the live
/// oracle, over the three states a port can be in (no socket, an opened datagram socket, after a
/// `close`).
///
/// Codes `24`/`25` are ungated in the C (`inet_set_opts`, `inet_drv.c:6939`-`:6956`;
/// `inet_fill_opts`, `:9168`-`:9175`), so they answer identically in every state; code `33` is
/// gated on the **cached** `SOCK_DGRAM` (`:7138`-`:7146`, `:9258`-`:9270`), so it is ignored
/// before `open` (still answering `[1]`) and still reported after a `close`.
///
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn the_mode_deliver_and_read_packets_rows_agree_with_the_erl_oracle() {
    let three = || vec![INET_LOPT_MODE, INET_LOPT_DELIVER, INET_LOPT_UDP_READ_PACKETS];
    let calls = vec![
        // ---- no socket yet ----
        (INET_REQ_GETOPTS, three()),
        (INET_REQ_SETOPTS, record(INET_LOPT_MODE, 1)),
        (INET_REQ_SETOPTS, record(INET_LOPT_DELIVER, 0)),
        (INET_REQ_SETOPTS, record(INET_LOPT_UDP_READ_PACKETS, 7)),
        (INET_REQ_SETOPTS, record(INET_LOPT_UDP_READ_PACKETS, 0)),
        (INET_REQ_GETOPTS, three()),
        // ---- an opened datagram socket ----
        (
            INET_REQ_OPEN,
            vec![INET_AF_INET, INET_TYPE_DGRAM, super::super::INET_PROTO_DEFAULT],
        ),
        (INET_REQ_SETOPTS, record(INET_LOPT_UDP_READ_PACKETS, 9)),
        (INET_REQ_GETOPTS, three()),
        (INET_REQ_SETOPTS, record(INET_LOPT_UDP_READ_PACKETS, 0)),
        // ---- after a `close`: the cached stype survives ----
        (INET_REQ_CLOSE, vec![]),
        (INET_REQ_GETOPTS, three()),
        (INET_REQ_SETOPTS, record(INET_LOPT_UDP_READ_PACKETS, 0)),
    ];

    let rust = port_replies(&calls);
    assert_eq!(rust.len(), calls.len(), "one reply per call");
    assert_eq!(
        rust[0],
        vec![INET_REP_OK, 24, 0, 0, 0, 0, 25, 0, 0, 0, 1],
        "before `open` code 33 is gated out; 24/25 report their defaults",
    );
    assert_eq!(rust[1], ctl_ok(), "`{{mode, binary}}` before `open`");
    assert_eq!(rust[2], ctl_ok(), "`{{deliver, port}}` before `open`");
    assert_eq!(rust[3], ctl_ok(), "a gated-out `read_packets` store is `[1]`");
    assert_eq!(rust[4], ctl_ok(), "the `ival <= 0` refusal is inside the gate");
    assert_eq!(
        rust[5],
        vec![INET_REP_OK, 24, 0, 0, 0, 1, 25, 0, 0, 0, 0],
        "both ungated values read back, code 33 still contributes nothing",
    );
    assert_eq!(rust[6], ctl_ok(), "`open`");
    assert_eq!(rust[7], ctl_ok(), "a positive `read_packets` on a datagram socket");
    assert_eq!(
        rust[8],
        vec![
            INET_REP_OK, 24, 0, 0, 0, 1, 25, 0, 0, 0, 0, 33, 0, 0, 0, 9,
        ],
        "all three read back once the socket is a datagram",
    );
    assert_eq!(rust[9], ctl_xerror("einval"), "a non-positive `read_packets`");
    assert_eq!(rust[10], ctl_ok(), "`close`");
    assert_eq!(
        rust[11],
        vec![
            INET_REP_OK, 24, 0, 0, 0, 1, 25, 0, 0, 0, 0, 33, 0, 0, 0, 9,
        ],
        "a close never clears `desc->stype`, so code 33 is still reported",
    );
    assert_eq!(rust[12], ctl_xerror("einval"), "the gate is still open after a close");

    let Some(dump) = erl_stdout(&oracle_expression(&calls)) else {
        eprintln!("SKIP: no `erl` on PATH — skipping the udp_inet option-gate oracle check");
        return;
    };
    let oracle = oracle_replies(&dump);
    assert_eq!(oracle.len(), calls.len(), "the oracle answered every call: {dump:?}");
    for (index, ((code, arg), (rust_reply, oracle_reply))) in
        calls.iter().zip(rust.iter().zip(oracle.iter())).enumerate()
    {
        assert_eq!(
            rust_reply, oracle_reply,
            "reply {index} (request code {code}, argument {arg:?}) disagrees",
        );
    }
}

/// The oracle's `port_info(Port, name)` — the registered driver name, which is the one value
/// that must be `"udp_inet"` for `gen_udp` to recognise the port.
///
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn the_oracle_reports_the_registered_driver_name() {
    let expr = "P = open_port({spawn_driver, \"udp_inet\"}, [binary]), \
                io:format(\"~p\", [erlang:port_info(P, name)]), \
                erlang:port_close(P)";
    let Some(dump) = erl_stdout(expr) else {
        eprintln!("SKIP: no `erl` on PATH — skipping the udp_inet driver-name oracle check");
        return;
    };
    let dump = String::from_utf8_lossy(&dump);
    assert!(
        dump.contains("udp_inet"),
        "the oracle's udp_inet port reports its name: {dump:?}",
    );
}

// ---------------------------------------------------------------------------
// The `prim_inet` caller-reference prefix (`init_caller`, `inet_drv.c:1820-1890`)
// ---------------------------------------------------------------------------
/// The `prim_inet` caller-reference tag as it looks on the wire: `term_to_binary(Mref, [local])`
/// for `Mref = monitor(port, S)` (`prim_inet.erl:575`-`:620`) always starts with the ETF
/// `VERSION_MAGIC` (131) and the rest is opaque to the driver, so a hand-built tag of four-plus
/// bytes exercises the same strip path a real monitor reference does.
const CALLER_REF_TAG: &[u8] = &[131, 100, 0, 8, b'r', b'e', b'f', b'0'];

/// The `prim_inet` command wire form:
/// `[tag_len:be16][tag_bytes][address][anc_len:be32][anc_data][data]`
/// (`prim_inet:send/3` + `do_sendto/4`, `erts/preloaded/src/prim_inet.erl:575`-`:700`).
fn prefixed_command(port: u16, anc: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut arg = Vec::new();
    arg.extend_from_slice(&(CALLER_REF_TAG.len() as u16).to_be_bytes());
    arg.extend_from_slice(CALLER_REF_TAG);
    arg.extend_from_slice(&loopback_arg(port));
    arg.extend_from_slice(&(anc.len() as u32).to_be_bytes());
    arg.extend_from_slice(anc);
    arg.extend_from_slice(payload);
    arg
}

/// A real `std::net::UdpSocket` on loopback — an independent receiver, so the assertions below
/// are on the bytes that reach the wire rather than on this driver's own queue.
///
/// The read timeout is a **hang guard** (the shape the spawn suite uses), never a latency budget.
fn udp_receiver() -> std::net::UdpSocket {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("a loopback datagram receiver");
    socket
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("a read timeout");
    socket
}

/// Receives exactly one datagram from `socket`.
fn receive_one(socket: &std::net::UdpSocket) -> Vec<u8> {
    let mut buf = [0u8; 1024];
    let (len, _from) = socket.recv_from(&mut buf).expect("the receiver gets a datagram");
    buf[..len].to_vec()
}

/// The reason atom of a typed error, so the `unsupported`-vs-`badarg` distinction is asserted
/// rather than assumed.
fn reason_of(err: BeamError) -> String {
    err.into_raise()
        .expect("a typed raise, not a generic failure")
        .reason
        .as_atom()
        .expect("an atom reason")
        .to_string()
}

#[test]
fn the_caller_reference_discriminator_accepts_only_a_fitting_etf_tag() {
    // A well-formed prefix is stripped, exactly `2 + n` bytes.
    let mut arg = vec![0, 4, ETF_VERSION_MAGIC, 1, 2, 7];
    arg.extend_from_slice(&loopback_arg(9));
    assert_eq!(
        split_caller_ref(&arg),
        Some(&arg[6..]),
        "a well-formed prefix is stripped and the rest returned",
    );
    // `n == 0` carries no caller reference (`init_caller`'s `*size_p < 2` guard).
    assert_eq!(split_caller_ref(&[0, 0, ETF_VERSION_MAGIC, 1]), None);
    // A truncated prefix (`2 + n` overruns the argument) is not stripped.
    assert_eq!(split_caller_ref(&[0, 9, ETF_VERSION_MAGIC, 1, 2]), None);
    // The tag's first byte must be the ETF version byte.
    assert_eq!(split_caller_ref(&[0, 3, 100, 1, 2, 9, 9]), None);
    // A plain address-first payload is never stripped: an `INET_AF_INET`/`_ANY`/`_LOOPBACK`
    // lead byte with a port number whose low byte is not 131 is not a caller reference.
    for tag in [INET_AF_INET, INET_AF_ANY, INET_AF_LOOPBACK] {
        let mut plain = vec![tag, 0x04, 210];
        plain.extend_from_slice(&[127, 0, 0, 1, b'h', b'i']);
        assert_eq!(split_caller_ref(&plain), None, "tag {tag} is not a caller reference");
    }
}

#[test]
fn the_caller_reference_prefix_is_stripped_from_a_datagram_command() {
    let receiver = udp_receiver();
    let target = receiver.local_addr().expect("an address").port();
    let mut rig = Rig::new();
    rig.bind_ephemeral();
    let arg = prefixed_command(target, &[], b"prefixed");
    assert_eq!(
        rig.control("send", &arg).expect("send"),
        ctl_ok(),
        "a well-formed caller-reference command is accepted",
    );
    assert_eq!(
        receive_one(&receiver),
        b"prefixed",
        "the receiver sees exactly the payload — no prefix, ancillary word or address bytes",
    );
}

#[test]
fn a_caller_reference_command_with_ancillary_data_is_the_typed_unsupported() {
    let receiver = udp_receiver();
    let target = receiver.local_addr().expect("an address").port();
    let mut rig = Rig::new();
    rig.bind_ephemeral();
    // `anc_len != 0` is the C's `sendmsg(2)`-with-cmsgs arm (`inet_drv.c:15220`), which this
    // floor has no seam for: a typed raise, never a send with the ancillary bytes dropped (or
    // mistaken for payload).
    let arg = prefixed_command(target, &[1, 2, 3, 4], b"payload");
    let reason = reason_of(rig.control("send", &arg).expect_err("ancillary data is not served"));
    assert_eq!(
        reason,
        "unsupported: udp ancillary data (sendmsg cmsg) is not served by this driver floor",
    );
}

#[test]
fn a_caller_reference_command_with_a_truncated_ancillary_length_is_einval() {
    let mut rig = Rig::new();
    rig.bind_ephemeral();
    // The C's `if (len < 4) goto return_einval` (`inet_drv.c:15205`): two bytes where the
    // ancillary-length word needs four.
    let mut arg = Vec::new();
    arg.extend_from_slice(&(CALLER_REF_TAG.len() as u16).to_be_bytes());
    arg.extend_from_slice(CALLER_REF_TAG);
    arg.extend_from_slice(&loopback_arg(9));
    arg.extend_from_slice(&[0, 0]);
    assert_eq!(
        rig.control("send", &arg).expect("send"),
        ctl_xerror("einval"),
        "a truncated ancillary-length word is the C's einval reply",
    );
}

#[test]
fn a_caller_reference_length_that_does_not_fit_falls_back_to_the_plain_parse() {
    let mut rig = Rig::new();
    rig.bind_ephemeral();
    // `n = 16` with only four tag bytes present: not a caller reference at all, so the plain
    // grammar runs — and its lead byte is `0x00`, which is no address tag (1/3/4). Nothing is
    // sent: the command is a plain argument fault.
    let arg = vec![0, 16, ETF_VERSION_MAGIC, 1, 2, 3];
    assert!(
        matches!(rig.control("send", &arg), Err(BeamError::BadArg)),
        "a non-fitting prefix is not a caller reference and the plain parse refuses it",
    );
}

#[test]
fn the_plain_datagram_command_form_still_sends_unchanged() {
    let receiver = udp_receiver();
    let target = receiver.local_addr().expect("an address").port();
    let mut rig = Rig::new();
    rig.bind_ephemeral();
    let reply = rig.send(target, b"plain");
    assert_eq!(reply, ctl_ok(), "the plain [address][payload] form still answers [1]");
    assert_eq!(receive_one(&receiver), b"plain", "the payload is delivered unchanged");
}

// ---------------------------------------------------------------------------
// The plain form is authoritative (regression: the old caller-reference-first order)
// ---------------------------------------------------------------------------

/// A loopback datagram receiver bound to a port whose **low byte is `0x83`** (131, the ETF
/// `VERSION_MAGIC`) — the byte the old caller-reference-**first** discriminator mistook for a tag.
///
/// Port 131 itself needs privilege, so every `0xNN83` candidate is tried in turn; `None` means the
/// host refused them all and the check **skips loudly** rather than flaking.
fn udp_receiver_on_a_0x83_port() -> Option<(std::net::UdpSocket, u16)> {
    for high in std::iter::once(0u16).chain(1..=0xfe) {
        let port = (high << 8) | u16::from(ETF_VERSION_MAGIC);
        let Ok(socket) = std::net::UdpSocket::bind(("127.0.0.1", port)) else {
            continue;
        };
        // A hang guard, never a latency budget (the shape `udp_receiver` uses).
        socket
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("a read timeout");
        return Some((socket, port));
    }
    None
}

/// A plain `[address][payload]` command whose **target** port low byte is `0x83` used to be read
/// as a `prim_inet` caller reference: `split_caller_ref` takes `n = 0x0100 + target_port_hi` from
/// the address's tag/first port byte and then keys on `arg[2] = target_port_lo == 0x83`, so the
/// tail was re-parsed as a datagram operand. The plain form is now tried first, so the whole
/// payload must reach the receiver unchanged.
#[test]
fn a_plain_command_with_a_0x83_low_port_byte_is_not_read_as_a_caller_reference() {
    let Some((receiver, target)) = udp_receiver_on_a_0x83_port() else {
        eprintln!(
            "SKIP: no loopback UDP port with low byte 0x83 could be bound — skipping the \
             plain-form-wins regression check"
        );
        return;
    };
    let mut rig = Rig::new();
    rig.bind_ephemeral();

    // The argument's shape is exactly the one the old order misread: `n = 0x0100 + target_hi`
    // and `arg[2] = target_lo = 0x83` satisfy `split_caller_ref`'s three conditions, and a
    // 512-byte payload makes `2 + n` fit for every candidate port (`n <= 0x0100 + 0xfe`), so the
    // old rule really would have stripped it (and then refused the tail as `badarg`, sending
    // nothing). The payload byte at `2 + n` is `0xAB` — no address tag — which is what pinned the
    // mistake to a refusal rather than a send to the wrong endpoint.
    let payload = vec![0xABu8; 512];
    let mut arg = loopback_arg(target);
    assert_eq!(arg[0], INET_AF_INET, "the plain form leads with the address tag");
    assert_eq!(arg[2], ETF_VERSION_MAGIC, "the target port's low byte is 0x83");
    let n = usize::from(u16::from_be_bytes([arg[0], arg[1]]));
    arg.extend_from_slice(&payload);
    assert!(
        arg.len() >= 2 + n,
        "the argument is long enough for the old prefix rule to have fired (len {}, 2 + n {})",
        arg.len(),
        2 + n,
    );

    assert_eq!(
        rig.control("send", &arg).expect("the plain command is accepted"),
        ctl_ok(),
        "the plain form is authoritative: the command is sent, not stripped",
    );
    assert_eq!(
        receive_one(&receiver),
        payload,
        "the receiver sees the whole payload — the address was consumed and nothing was stripped",
    );
}
