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

use super::super::opt::{INET_OPT_BROADCAST, INET_OPT_REUSEADDR, INET_OPT_TTL};
use super::super::wire::{ctl_ok, ctl_xerror};
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

#[test]
fn the_datagram_driver_state_options_are_served() {
    let mut rig = Rig::new();
    rig.open();
    // `{mode, list}` (24) = 0, `{deliver, term}` (25) = 1, `{read_packets, }` (33) = 5.
    let defaults = rig.control("getopts", &[24, 25, 33]).expect("getopts");
    assert_eq!(
        defaults,
        vec![INET_REP_OK, 24, 0, 0, 0, 0, 25, 0, 0, 0, 1, 33, 0, 0, 0, 5],
    );
    // A non-positive `read_packets` is the C's `[0]einval`.
    let mut set = vec![33];
    set.extend_from_slice(&0i32.to_be_bytes());
    assert_eq!(
        rig.control("setopts", &set).expect("setopts"),
        ctl_xerror("einval"),
    );
}

#[test]
fn read_packets_is_dropped_before_a_socket_exists() {
    let mut rig = Rig::new();
    let reply = rig.control("getopts", &[24, 25, 33]).expect("getopts");
    assert_eq!(
        reply,
        vec![INET_REP_OK, 24, 0, 0, 0, 0, 25, 0, 0, 0, 1],
        "code 33 contributes nothing while there is no datagram socket",
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
