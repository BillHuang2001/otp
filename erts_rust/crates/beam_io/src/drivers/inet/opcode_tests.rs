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

//! The `tcp_inet` driver's **numeric control-opcode** suite, driven through the runtime's own
//! `erlang:port_control/3` path ([`crate::PortRegistry::port_control`]) rather than by calling
//! [`Driver::control`] directly: the command a live `beam_rt` forwards is the decimal string of
//! `opcode + `[`CONTROL_MAGIC`]`, so that is exactly what these tests send.
//!
//! The suite is split in two halves:
//!
//! * the **numeric-dispatch** tests, which pin the reply every opcode the bridge serves actually
//!   produces (and the typed gap for the ones it refuses), plus the invariant that the named and
//!   the numeric route cannot drift apart;
//! * the **`erl`-oracle matrix**, which runs the *same* `(opcode, operand)` list against the real
//!   system `tcp_inet` driver port and compares the two reply streams — byte for byte where the
//!   reply is deterministic, shape-wise where it carries an ephemeral port or a descriptor.
//!
//! The datagram half of the same bridge lives in the sibling
//! [`opcode_tests_udp`](super::opcode_tests_udp) suite; the helpers both halves share are defined
//! here once (a `#[cfg(test)]` module's private items are not reachable across the integration-test
//! boundary, but they are reachable from a sibling in-module suite — so unlike the crate's
//! `erl`-oracle helpers there is no reason to copy them).
use super::wire::{ctl_ok, ctl_xerror};
use super::{
    TcpInetDriver, INET_AF_INET, INET_AF_LOOPBACK, INET_OPT_KEEPALIVE, INET_PROTO_DEFAULT,
    INET_REP_OK, INET_TYPE_STREAM,
};
use crate::driver::Driver;
use crate::poller::Poller;
use crate::port::PortRegistry;
use beam_common::{BeamError, Result};
use beam_process::PortId;
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::{IntoRawFd, RawFd};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// `ERTS_INET_DRV_CONTROL_MAGIC_NUMBER` (`erts/emulator/beam/sys.h:1466`) — the offset the
/// Erlang side adds to every control opcode and both `*_inet_ctl` entries subtract again.
pub(super) const CONTROL_MAGIC: u64 = 0x03f1_a300;

/// Wall-clock budget for the `erl` oracle before the affected cross-check is skipped.
pub(super) const ERL_TIMEOUT: Duration = Duration::from_secs(30);

/// The decimal command string a live runtime forwards for the C request `code`: the integer
/// `code + `[`CONTROL_MAGIC`]`, rendered exactly as `beam_rt` renders it.
pub(super) fn op(code: u32) -> String {
    (u64::from(code) + CONTROL_MAGIC).to_string()
}

/// The packed `open` argument of the stream driver: `[AF_INET, SOCK_STREAM, default protocol]`.
pub(super) fn open_arg() -> [u8; 3] {
    [INET_AF_INET, INET_TYPE_STREAM, INET_PROTO_DEFAULT]
}

/// The packed `bind`/`connect` argument naming `127.0.0.1:port` (the C's `INET_AF_LOOPBACK`
/// short form: family tag + the two port bytes).
pub(super) fn loopback_arg(port: u16) -> Vec<u8> {
    let [hi, lo] = port.to_be_bytes();
    vec![INET_AF_LOOPBACK, hi, lo]
}

/// A live in-crate port, driven through the runtime's `port_control` path.
///
/// Every method takes `&mut self` because the registry node needs both the poller and the
/// registry mutably — exactly the pair `beam_rt` holds.
pub(super) struct Rig {
    registry: PortRegistry,
    poller: Poller,
    id: PortId,
}

impl Rig {
    /// Opens a port over `driver` — the same shape [`crate::PortRegistry::open`] gives `beam_rt`.
    pub(super) fn new(driver: Box<dyn Driver>) -> Rig {
        let mut poller = Poller::new().expect("a poll set");
        let mut registry = PortRegistry::new();
        let id = registry.open(&mut poller, driver).expect("the port opens");
        Rig {
            registry,
            poller,
            id,
        }
    }

    /// One control call, panicking on a typed error.
    pub(super) fn control(&mut self, command: &str, arg: &[u8]) -> Vec<u8> {
        match self.try_control(command, arg) {
            Ok(reply) => reply,
            Err(err) => panic!("the `{command}` control call failed: {err:?}"),
        }
    }

    /// One control call, returning the typed error untouched.
    pub(super) fn try_control(&mut self, command: &str, arg: &[u8]) -> Result<Vec<u8>> {
        self.registry
            .port_control(&mut self.poller, self.id, command, arg)
    }

    /// Every control call in order, panicking on the first typed error — the matrix driver.
    pub(super) fn run(&mut self, calls: &[Call]) -> Vec<Vec<u8>> {
        calls
            .iter()
            .map(|call| {
                let command = call.value.to_string();
                match self.try_control(&command, &call.arg) {
                    Ok(reply) => reply,
                    Err(err) => panic!(
                        "the numeric control call {} ({:?}) failed: {err:?}",
                        call.value, call.arg
                    ),
                }
            })
            .collect()
    }

    /// Drains up to `len` bytes out of the port's **input queue** — what a driver pushed through
    /// [`crate::DriverContext::input`], i.e. the bytes the runtime would deliver as
    /// `{Port, {data, …}}`.
    pub(super) fn take_input(&mut self, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        let port = self.registry.get_mut(self.id).expect("a live port");
        let read = port.read_input(&mut buf);
        buf.truncate(read);
        buf
    }

    /// How many bytes currently sit in the port's input queue.
    pub(super) fn input_len(&mut self) -> usize {
        self.registry
            .get(self.id)
            .expect("a live port")
            .input_len()
    }

    /// Queues `bytes` as a port command and runs one port-task round — the pair the runtime's
    /// `erlang:port_command/2` drives ([`crate::PortRegistry::port_command`] then
    /// [`crate::PortRegistry::execute_port_tasks`]).
    ///
    /// Returns the round's outcome. A driver that **refuses** the write reports it as an error,
    /// and the command's bytes stay in the port's output queue — the state an
    /// `INET_REQ_SUBSCRIBE` reply reports the depth of.
    pub(super) fn command(&mut self, bytes: &[u8]) -> Result<()> {
        self.registry.port_command(
            &mut self.poller,
            self.id,
            bytes,
            crate::port::PortCommandOptions::EMPTY,
        )?;
        self.registry
            .execute_port_tasks(&mut self.poller, crate::port::DEFAULT_READINESS_RED_BUDGET)
            .map(|_round| ())
    }
}
/// How a matrix reply is compared, chosen by whether it can be deterministic in the first place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Rule {
    /// Byte-for-byte identical on both sides.
    Exact,
    /// `[1] ++ port_hi ++ port_lo`: the kernel assigns the ephemeral port, so only the shape
    /// (tag, length, a non-zero port) can be compared.
    BoundPort,
    /// `[1,1,port_hi,port_lo,a,b,c,d]`: a packed local/peer address. Two independent sockets
    /// bind two different ephemeral ports, so the shape and the address octets are compared and
    /// the port may not be zero.
    PackedAddress,
    /// `[1] ++ fd_be32`: the two runtimes own different descriptor numbers.
    Descriptor,
}

/// One `port_control` call in an oracle matrix: the integer the Erlang side is given (the
/// code plus [`CONTROL_MAGIC`], or the raw value of a wrapped one), its operand and the
/// comparison rule.
pub(super) struct Call {
    /// The integer `erlang:port_control(Port, Value, Arg)` receives.
    pub(super) value: u64,
    /// The request payload (the C's `buf`/`len`).
    pub(super) arg: Vec<u8>,
    /// How the two sides' replies are compared.
    pub(super) rule: Rule,
}

impl Call {
    /// The C request `code`, sent the way the Erlang side sends it (`code + `[`CONTROL_MAGIC`]`).
    pub(super) fn req(code: u32, arg: impl Into<Vec<u8>>, rule: Rule) -> Call {
        Call {
            value: u64::from(code) + CONTROL_MAGIC,
            arg: arg.into(),
            rule,
        }
    }

    /// A **raw** integer: an opcode below the magic number (which the C's unsigned subtraction
    /// wraps into a value no `case` matches) or a retired request number.
    pub(super) fn raw(value: u64, arg: impl Into<Vec<u8>>, rule: Rule) -> Call {
        Call {
            value,
            arg: arg.into(),
            rule,
        }
    }
}

/// Asserts that the oracle's reply and this floor's reply agree under `rule`.
///
/// The panic message always carries both byte lists, so a divergence is readable without a
/// re-run.
pub(super) fn assert_agrees(label: &str, rule: Rule, oracle: &[u8], rust: &[u8]) {
    match rule {
        Rule::Exact => assert_eq!(rust, oracle, "{label}: replies differ"),
        Rule::BoundPort => {
            assert_eq!(oracle.len(), 3, "{label}: the oracle's bind reply is [1,hi,lo]");
            assert_eq!(rust.len(), 3, "{label}: [1,hi,lo]");
            assert_eq!(rust[0], oracle[0], "{label}: result tag");
            assert_eq!(rust[0], INET_REP_OK, "{label}: result tag");
            assert_ne!(
                u16::from_be_bytes([oracle[1], oracle[2]]),
                0,
                "{label}: the oracle's socket has a port"
            );
            assert_ne!(
                u16::from_be_bytes([rust[1], rust[2]]),
                0,
                "{label}: the kernel assigned a port"
            );
        }
        Rule::PackedAddress => {
            assert_eq!(
                rust.len(),
                8,
                "{label}: [1, family, port_hi, port_lo, a, b, c, d]"
            );
            assert_eq!(oracle.len(), 8, "{label}: the oracle's packed address");
            assert_eq!(rust[0], oracle[0], "{label}: result tag");
            assert_eq!(rust[1], oracle[1], "{label}: address family");
            assert_ne!(
                u16::from_be_bytes([rust[2], rust[3]]),
                0,
                "{label}: a bound socket has a non-zero port"
            );
            assert_eq!(&rust[4..8], &oracle[4..8], "{label}: address octets");
        }
        Rule::Descriptor => {
            assert_eq!(rust.len(), 5, "{label}: [1] ++ fd_be32");
            assert_eq!(oracle.len(), 5, "{label}: the oracle's GETFD reply");
            assert_eq!(rust[0], INET_REP_OK, "{label}: result tag");
            assert_ne!(
                i32::from_be_bytes([rust[1], rust[2], rust[3], rust[4]]),
                0,
                "{label}: a descriptor number"
            );
        }
    }
}

/// Runs `<expr>, halt(0).` under the system `erl` and returns its stdout, or `None` — after
/// printing a clear skip message — when `erl` cannot be spawned or does not answer within
/// [`ERL_TIMEOUT`]. A spawned `erl` that exits non-zero is a bug in the expression, so it fails
/// the test.
pub(super) fn erl_stdout(expr: &str) -> Option<String> {
    let code = format!("{expr}, halt(0).");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(Command::new("erl").args(["-noshell", "-eval", &code]).output());
    });

    match rx.recv_timeout(ERL_TIMEOUT) {
        Err(_) => {
            eprintln!("SKIP: `erl` did not answer within {ERL_TIMEOUT:?}");
            None
        }
        Ok(Err(err)) => {
            eprintln!("SKIP: no `erl` on PATH — could not spawn the oracle ({err})");
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

/// The oracle expression driving a `{spawn_driver, Driver}` port through `calls`, printing each
/// reply as `~w` (a list of integers) followed by `|`.
///
/// The replies are printed as integer lists rather than as raw bytes on purpose: a `bind`/`name`
/// reply carries the raw two bytes of an ephemeral port, and `|` is a perfectly legal byte in
/// one — a raw byte dump split on `|` would mis-split roughly one run in a hundred.
pub(super) fn oracle_expression(driver: &str, calls: &[Call]) -> String {
    let mut body = String::new();
    for call in calls {
        let arg = call
            .arg
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(",");
        body.push_str(&format!(
            "J(erlang:port_control(P, {}, <<{arg}>>)), ",
            call.value
        ));
    }
    format!(
        "J = fun(R) -> io:format(\"~w|\", [R]) end, \
         P = open_port({{spawn_driver, \"{driver}\"}}, [binary]), \
         {body}erlang:port_close(P)"
    )
}

/// Parses the oracle's `[1]|[1,0,0]|` dump back into the reply byte lists.
pub(super) fn oracle_replies(dump: &str) -> Vec<Vec<u8>> {
    dump.split('|')
        .filter(|part| !part.trim().is_empty())
        .map(|part| {
            let inner = part.trim().trim_start_matches('[').trim_end_matches(']');
            if inner.is_empty() {
                Vec::new()
            } else {
                inner
                    .split(',')
                    .map(|number| number.trim().parse::<u8>().expect("a reply byte"))
                    .collect()
            }
        })
        .collect()
}

/// What one control call **observably** did: the reply it answered, or the `badarg` raise the C's
/// `return -1` becomes.
///
/// [`oracle_expression`] assumes every call answers a reply, so a raising call aborts the whole
/// expression; the guarded form below is what a probe of a C refusal needs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Outcome {
    /// The call answered these bytes.
    Reply(Vec<u8>),
    /// The call raised `error:badarg` (the C's `return -1` from a control arm, which the
    /// `inet_ctl` caller turns into an argument error).
    BadArg,
}

/// The oracle expression for [`Outcome`]: the same `{spawn_driver, Driver}` port and call list as
/// [`oracle_expression`], but every call runs inside a `try` and prints `R <reply>|` on success or
/// `B|` when it raised `error:badarg`. Any **other** class/reason is not caught, so an unexpected
/// raise still fails the probe loudly instead of being read as a refusal.
pub(super) fn oracle_expression_catching(driver: &str, calls: &[Call]) -> String {
    let mut body = String::new();
    for call in calls {
        let arg = call
            .arg
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(",");
        body.push_str(&format!(
            "J(fun() -> erlang:port_control(P, {}, <<{arg}>>) end), ",
            call.value
        ));
    }
    format!(
        "J = fun(F) -> try F() of R -> io:format(\"R ~w|\", [R]) \
         catch error:badarg -> io:format(\"B|\") end end, \
         P = open_port({{spawn_driver, \"{driver}\"}}, [binary]), \
         {body}erlang:port_close(P)"
    )
}

/// Parses a [`oracle_expression_catching`] dump back into one [`Outcome`] per call.
pub(super) fn oracle_outcomes(dump: &str) -> Vec<Outcome> {
    dump.split('|')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| match part.strip_prefix("R ") {
            Some(reply) => {
                let inner = reply.trim_start_matches('[').trim_end_matches(']');
                if inner.is_empty() {
                    Outcome::Reply(Vec::new())
                } else {
                    Outcome::Reply(
                        inner
                            .split(',')
                            .map(|number| number.trim().parse::<u8>().expect("a reply byte"))
                            .collect(),
                    )
                }
            }
            None => {
                assert_eq!(part, "B", "an unparsable guarded-oracle outcome: {part:?}");
                Outcome::BadArg
            }
        })
        .collect()
}

/// This floor's [`Outcome`] for one call, driven through the runtime's own `port_control` path.
pub(super) fn rust_outcome(port: &mut Rig, call: &Call) -> Outcome {
    match port.try_control(&call.value.to_string(), &call.arg) {
        Ok(reply) => Outcome::Reply(reply),
        Err(BeamError::BadArg) => Outcome::BadArg,
        Err(err) => panic!("an unexpected typed error for {}: {err:?}", call.value),
    }
}

/// Runs `calls` on both sides — a guarded `erl` probe and this crate's port — and asserts the
/// outcomes agree one by one. `open_port` builds the bare in-crate port for the same driver, so
/// each suite supplies its own.
///
/// **Skips loudly** when `erl` is not on `PATH`.
pub(super) fn assert_outcomes_agree_with_the_oracle(
    driver: &str,
    calls: &[Call],
    open_port: impl FnOnce() -> Rig,
) {
    let Some(dump) = erl_stdout(&oracle_expression_catching(driver, calls)) else {
        eprintln!("SKIP: no `erl` on PATH — skipping the {driver} guarded-probe cross-check");
        return;
    };
    let oracle = oracle_outcomes(&dump);
    assert_eq!(
        oracle.len(),
        calls.len(),
        "the oracle answered every call: {dump:?}"
    );

    let mut port = open_port();
    for (index, call) in calls.iter().enumerate() {
        let rust = rust_outcome(&mut port, call);
        assert_eq!(
            rust, oracle[index],
            "call #{index} (`{}` of {:?}) disagrees",
            call.value, call.arg
        );
    }
}/// A **bare** (socket-less) `tcp_inet` port — the state the driver factory starts in.
pub(super) fn bare_port() -> Rig {
    Rig::new(Box::new(TcpInetDriver::new()))
}

/// An `open`ed `tcp_inet` port (a socket exists, nothing is bound).
pub(super) fn opened_port() -> Rig {
    let mut port = bare_port();
    assert_eq!(
        port.control(&op(1), &open_arg()),
        ctl_ok(),
        "INET_REQ_OPEN(1) creates the socket"
    );
    port
}

/// A listening `tcp_inet` port over an ephemeral loopback port; returns the port it bound.
pub(super) fn listening_port() -> (Rig, u16) {
    let mut port = opened_port();
    let bound = port.control(&op(6), &loopback_arg(0));
    assert_eq!(bound[0], INET_REP_OK);
    assert_eq!(bound.len(), 3, "`bind` replies [1, port_hi, port_lo]");
    let bound_port = u16::from_be_bytes([bound[1], bound[2]]);
    assert_ne!(bound_port, 0, "the kernel assigned a port");
    assert_eq!(port.control(&op(27), &[0, 5]), ctl_ok(), "INET_REQ_LISTEN");
    (port, bound_port)
}

/// A `tcp_inet` port whose socket is **already connected** — the state `PEER`/`GETSTATUS`/`GETFD`
/// and the connected `TCP_REQ_RECV` path need.
///
/// The driver adopts a duplicate of the accepted socket (`try_clone().into_raw_fd()`, so the
/// driver owns a descriptor number the test knows and no `unsafe` is needed to make one); the far
/// end is held for the test's lifetime so the connection stays up.
pub(super) struct Connection {
    pub(super) port: Rig,
    /// The descriptor the port's driver owns — its `INET_REQ_GETFD` value.
    pub(super) fd: RawFd,
    /// The far end, held so the connection is not closed under the port.
    _peer: TcpStream,
}

pub(super) fn connection() -> Connection {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let peer = TcpStream::connect(listener.local_addr().expect("an address"))
        .expect("the far end connects");
    let (server, _accepted) = listener.accept().expect("the port's end is accepted");
    let fd = server
        .try_clone()
        .expect("a second descriptor for the same socket")
        .into_raw_fd();
    let driver = TcpInetDriver::from_socket(fd).expect("the driver adopts the socket");
    assert!(driver.state().is_connected(), "the adopted socket has a peer");
    Connection {
        port: Rig::new(Box::new(driver)),
        fd,
        _peer: peer,
    }
}

/// The typed "not served" reason of a numeric opcode, as the crate spells it.
pub(super) fn not_served_reason(name: &str, switch: &str) -> String {
    format!("unsupported: control opcode `{name}` ({switch}) is not served by this driver floor")
}

/// The reason atom of a typed error, for the exact-wording assertions below.
pub(super) fn reason_of(err: BeamError) -> String {
    err.into_raise()
        .expect("a typed raise, not a generic failure")
        .reason
        .as_atom()
        .expect("an atom reason")
        .to_string()
}

// -----------------------------------------------------------------------
// The numeric lifecycle verbs
// -----------------------------------------------------------------------

/// `INET_REQ_OPEN`(1) → `INET_REQ_BIND`(6) → `INET_REQ_LISTEN`(27) over the numeric route
/// reaches the very same verbs the named route does, and the driver moves through the C's
/// states with it.
#[test]
fn the_numeric_open_bind_listen_sequence_reaches_the_same_verbs() {
    let mut port = bare_port();
    // `INET_REQ_GETSTATUS`(16) on a bare port is the C's `INET_STATE_CLOSED` (0).
    assert_eq!(
        port.control(&op(16), &[]),
        vec![INET_REP_OK, 0, 0, 0, 0],
        "a factory port has no socket"
    );

    assert_eq!(port.control(&op(1), &open_arg()), ctl_ok(), "INET_REQ_OPEN");
    assert_eq!(
        port.control(&op(16), &[]),
        vec![INET_REP_OK, 0, 0, 0, 1],
        "`INET_STATE_OPEN` after `open`"
    );

    let bound = port.control(&op(6), &loopback_arg(0));
    assert_eq!(bound[0], INET_REP_OK, "INET_REQ_BIND succeeds");
    assert_eq!(bound.len(), 3, "the reply is [1, port_hi, port_lo]");
    let bound_port = u16::from_be_bytes([bound[1], bound[2]]);
    assert_ne!(bound_port, 0, "the kernel chose a port for us");

    assert_eq!(port.control(&op(27), &[0, 5]), ctl_ok(), "INET_REQ_LISTEN");
    assert_eq!(
        port.control(&op(16), &[]),
        vec![INET_REP_OK, 0, 0, 0, 9],
        "`INET_STATE_OPEN | INET_F_LISTEN` after `listen`"
    );
}

/// `INET_REQ_SETOPTS`(7)/`INET_REQ_GETOPTS`(8) are the same verbs the named route reaches: the
/// five-byte record sets the option and the one-byte code reads that same option back.
#[test]
fn numeric_setopts_and_getopts_round_trip_the_keepalive_option() {
    let mut port = opened_port();
    let mut record = vec![INET_OPT_KEEPALIVE];
    record.extend_from_slice(&1i32.to_be_bytes());
    assert_eq!(port.control(&op(7), &record), ctl_ok(), "INET_REQ_SETOPTS");

    let mut expected = ctl_ok();
    expected.push(INET_OPT_KEEPALIVE);
    expected.extend_from_slice(&1i32.to_be_bytes());
    assert_eq!(
        port.control(&op(8), &[INET_OPT_KEEPALIVE]),
        expected,
        "INET_REQ_GETOPTS reads the option back as [1, opt, val_be32]"
    );

    // A truncated `{active, N}` record (the multi form `{active,3}` without its trailing
    // 16-bit counter) is the driver's argument fault, on this route too.
    assert!(
        matches!(
            port.try_control(&op(7), &[22, 0, 0, 0, 3]),
            Err(BeamError::BadArg)
        ),
        "`{{active,3}}` carries a 16-bit counter the five-byte record cannot hold"
    );
}

/// `INET_REQ_GETTYPE`(15) reports the C's `{Family, Type}` pair with a socket and refuses with
/// `einval` without one.
#[test]
fn numeric_gettype_reports_a_stream_socket_and_refuses_without_one() {
    let mut bare = bare_port();
    assert_eq!(
        bare.control(&op(15), &[]),
        ctl_xerror("einval"),
        "the C's `if (!IS_OPEN) return ctl_error(EINVAL)`"
    );

    let mut port = opened_port();
    assert_eq!(
        port.control(&op(15), &[]),
        vec![INET_REP_OK, 0, 0, 0, INET_AF_INET, 0, 0, 0, INET_TYPE_STREAM],
        "AF_INET + SOCK_STREAM, big-endian"
    );
}

/// `INET_REQ_GETSTATUS`(16) is the C's `INET_STATE_*` bit set: `0` closed, `1` open, `9`
/// listening and `5` connected.
#[test]
fn numeric_getstatus_reports_the_c_state_bits() {
    let mut bare = bare_port();
    assert_eq!(bare.control(&op(16), &[]), vec![INET_REP_OK, 0, 0, 0, 0]);

    let (mut listening, _port) = listening_port();
    assert_eq!(
        listening.control(&op(16), &[]),
        vec![INET_REP_OK, 0, 0, 0, 9]
    );

    let mut connected = connection();
    assert_eq!(
        connected.port.control(&op(16), &[]),
        vec![INET_REP_OK, 0, 0, 0, 5],
        "`INET_STATE_OPEN | INET_F_ACTIVE`"
    );
}

/// `INET_REQ_GETSTAT`(11) answers the bare success tag for an **empty** request — nothing
/// requested, nothing appended — and the C's `badarg` for an out-of-range `INET_STAT_*` byte:
/// `inet_fill_stat`'s `default: return -1` (`inet_drv.c:10786`) makes the driver's `control`
/// callback return negative, which `erlang:port_control/3` raises as the bare atom (measured),
/// discarding the whole reply — so a request that starts with valid ops and then hits a bad one
/// is a plain `badarg`, never a partial reply.
///
/// The record rendering itself (the counter blocks, the 32-/64-bit widths and the per-driver
/// accounting) is pinned by the sibling [`stat_tests`](super::stat_tests) suite.
#[test]
fn numeric_getstat_is_ok_when_empty_and_badarg_for_an_unknown_counter() {
    let mut port = opened_port();
    assert_eq!(port.control(&op(11), &[]), ctl_ok(), "INET_REQ_GETSTAT");

    assert!(
        matches!(port.try_control(&op(11), &[99]), Err(BeamError::BadArg)),
        "an unknown counter op is the C's `-1`"
    );
    assert!(
        matches!(
            port.try_control(&op(11), &[1, 99]),
            Err(BeamError::BadArg)
        ),
        "the whole reply is discarded, not just the bad record"
    );
}

/// `INET_REQ_GETFD`(14) reports the driver's own descriptor, big-endian — and refuses with
/// `einval` while there is no socket.
#[test]
fn numeric_getfd_reports_the_drivers_descriptor() {
    let mut bare = bare_port();
    assert_eq!(bare.control(&op(14), &[]), ctl_xerror("einval"));

    let mut connected = connection();
    let mut expected = ctl_ok();
    expected.extend_from_slice(&connected.fd.to_be_bytes());
    assert_eq!(
        connected.port.control(&op(14), &[]),
        expected,
        "the descriptor the driver adopted"
    );
}

/// `INET_REQ_PEER`(4) is guarded on the **connection**, not on the socket: `enotconn` until the
/// socket really has a peer, then the packed peer address.
#[test]
fn numeric_peer_is_enotconn_until_the_socket_is_connected() {
    let mut bare = bare_port();
    assert_eq!(bare.control(&op(4), &[]), ctl_xerror("enotconn"));

    let (mut listening, _port) = listening_port();
    assert_eq!(
        listening.control(&op(4), &[]),
        ctl_xerror("enotconn"),
        "the C's `if (!(desc->state & INET_F_ACTIVE)) return ctl_error(ENOTCONN)`"
    );

    let mut connected = connection();
    let reply = connected.port.control(&op(4), &[]);
    assert_eq!(reply.len(), 8, "the reply is [1, family, hi, lo, a, b, c, d]");
    assert_eq!(reply[0], INET_REP_OK, "a connected socket has a peer");
    assert_eq!(reply[1], INET_AF_INET, "the packed address family");
    assert_ne!(
        u16::from_be_bytes([reply[2], reply[3]]),
        0,
        "the peer's ephemeral port"
    );
    assert_eq!(&reply[4..8], &[127, 0, 0, 1], "the loopback peer address");
}

/// `TCP_REQ_RECV`(42) carries the C's `Timeout(4), Length(4)` payload: the **connection** guard
/// comes first (so a wrong-length operand on a port with no peer is the C's `enotconn`), and only a
/// connected socket reaches the `len != 8 ⇒ einval` check.
///
/// Divergence from the C, recorded here rather than asserted away: on a connected socket the C
/// enqueues an asynchronous receive and answers `[1] ++ async_id_be16` (measured `[1,0,1]`), while
/// this floor's `recv` is a synchronous read-now verb and answers the bare `[1]` (see the
/// async-op-queue entry in the crate `CONTEXT.md`).
#[test]
fn numeric_tcp_recv_validates_the_eight_byte_operand() {
    let mut connected = connection();
    assert_eq!(
        connected.port.control(&op(42), &[]),
        ctl_xerror("einval"),
        "a wrong-length operand on a connected socket is the C's EINVAL"
    );
    assert_eq!(
        connected.port.control(&op(42), &[0, 0, 0, 0, 0, 0, 0, 0]),
        ctl_ok(),
        "the synchronous floor's bare `[1]` (the C would answer [1] ++ async_id_be16)"
    );

    let mut bare = bare_port();
    assert_eq!(
        bare.control(&op(42), &[0, 0, 0, 0, 0, 0, 0, 0]),
        ctl_xerror("enotconn"),
        "nothing to receive from without a connection"
    );
    assert_eq!(
        bare.control(&op(42), &[1, 2]),
        ctl_xerror("enotconn"),
        "the connection guard precedes the operand-length guard"
    );

    let (mut listening, _port) = listening_port();
    assert_eq!(
        listening.control(&op(42), &[0, 0, 0, 0, 0, 0, 0, 0]),
        ctl_xerror("enotconn"),
        "and a listener is not a connection either"
    );
}

/// `INET_REQ_ACCEPT`(26) requires the C's four-byte `Timeout` operand.
///
/// Divergence from the C: with a four-byte operand the C enqueues an asynchronous accept and
/// answers the three-byte op id `[1] ++ async_id_be16` (measured `[1,0,0]`); this floor's accept
/// is synchronous, so it answers the bare `[1]` when nothing is pending. Only the malformed
/// operand is compared against the oracle (below).
#[test]
fn numeric_accept_requires_the_four_byte_timeout() {
    let (mut listening, _port) = listening_port();
    assert_eq!(
        listening.control(&op(26), &[]),
        ctl_xerror("einval"),
        "the C's `if (len != 4) ctl_error(EINVAL)`, measured on a listener"
    );
    assert_eq!(
        listening.control(&op(26), &[0, 0, 0, 0]),
        ctl_ok(),
        "a synchronous accept with nothing pending answers the bare [1]"
    );

    // With no listener at all the verb's own state check refuses the call.
    let mut opened = opened_port();
    assert_eq!(
        opened.control(&op(26), &[0, 0, 0, 0]),
        ctl_xerror("einval"),
        "there is nothing to accept from a socket that is not listening"
    );
}

/// `INET_REQ_CONNECT`(3) carries the C's `Timeout(4), Port(2), Address(N)` payload: anything
/// shorter than the six-byte prefix is the C's `ctl_error(EINVAL)`, and a real loopback connect
/// answers the bare `[1]`.
#[test]
fn numeric_connect_validates_the_six_byte_prefix() {
    let mut port = opened_port();
    assert_eq!(
        port.control(&op(3), &[]),
        ctl_xerror("einval"),
        "the C's `if (len < 6) return ctl_error(EINVAL)`"
    );

    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let target = listener.local_addr().expect("an address").port();
    let mut arg = vec![0, 0, 0, 0, 0, 0]; // Timeout(4) ++ Port(2), unused by this floor.
    arg.extend_from_slice(&loopback_arg(target));
    assert_eq!(
        port.control(&op(3), &arg),
        ctl_ok(),
        "a loopback connect is accepted (EINPROGRESS or an immediate connect)"
    );
}

/// `INET_REQ_CLOSE`(2) releases the socket: the driver is back to the closed state, the descriptor
/// is gone, and every descriptor-dependent reply says so — except `INET_REQ_GETTYPE`, which reports
/// the **cached** `{Family, Type}` pair the port was opened with, because the C's arm has no
/// `IS_OPEN` guard and reads `desc->sfamily`/`desc->stype`, neither of which a close clears
/// (`inet_drv.c:11165`-`:11203`, `:10925`).
#[test]
fn numeric_close_returns_the_port_to_the_closed_state() {
    let (mut port, _bound) = listening_port();
    assert_eq!(port.control(&op(2), &[]), ctl_ok(), "INET_REQ_CLOSE");
    assert_eq!(
        port.control(&op(16), &[]),
        vec![INET_REP_OK, 0, 0, 0, 0],
        "the state bits are back to INET_STATE_CLOSED"
    );
    assert_eq!(port.control(&op(14), &[]), ctl_xerror("einval"), "no fd");
    assert_eq!(
        port.control(&op(15), &[]),
        vec![INET_REP_OK, 0, 0, 0, INET_AF_INET, 0, 0, 0, INET_TYPE_STREAM],
        "the cached AF_INET + SOCK_STREAM pair outlives the descriptor"
    );
    assert_eq!(
        port.control(&op(5), &[]),
        ctl_xerror("ebadf"),
        "the C's `getsockname(2)` on a closed descriptor"
    );
    assert_eq!(
        port.control(&op(2), &[]),
        ctl_ok(),
        "a second close is still the C's `ok`"
    );
}

// -----------------------------------------------------------------------
// The scope rules: a case of the *other* switch, a wrapped number, an unserved case
// -----------------------------------------------------------------------

/// `PACKET_REQ_RECV`(60) is a case of `packet_inet_ctl` only, so on a `tcp_inet` port neither
/// switch owns it and the C's `inet_ctl` default arm answers `[0]exbadport`.
#[test]
fn a_datagram_opcode_on_the_tcp_port_is_exbadport() {
    let (mut port, _bound) = listening_port();
    assert_eq!(
        port.control(&op(60), &[]),
        ctl_xerror("exbadport"),
        "INET_REQ_PACKET_REQ_RECV is the datagram driver's own case"
    );
    assert_eq!(
        port.control(&op(60), &[0, 0, 0, 0, 0, 0, 0, 0]),
        ctl_xerror("exbadport"),
        "the operand makes no difference: no switch owns the number"
    );
}

/// A command below the magic number wraps in the C's `unsigned int` subtraction and matches no
/// case — so `1` is **not** `open` — and a retired/unassigned number above the offset is just as
/// unserved. Both answer the C's `[0]exbadport`, never a silent success or a `badarg`.
#[test]
fn a_wrapped_or_unassigned_numeric_command_is_exbadport() {
    let mut port = opened_port();
    for command in ["0", "1", "99", "-1", &(9u64 + CONTROL_MAGIC).to_string()] {
        assert_eq!(
            port.control(command, &open_arg()),
            ctl_xerror("exbadport"),
            "`{command}` matches no switch's case"
        );
    }
}

/// The opcodes the C's switches **do** serve but this floor does not implement are a typed gap: a
/// raise whose reason names the request and the switch that owns it, never a fabricated reply.
///
/// Each row below is the *one* state in which the C succeeds, i.e. the refusal-only arms
/// (`fdopen`, `ignorefd`, `tcp_sendfile`) have already answered their exact C refusal for every
/// other operand/state and only their success path is missing.
#[test]
fn an_unserved_numeric_opcode_is_a_typed_gap() {
    // The `fdopen` form that reaches the C's **blind** adoption: a ten-byte operand whose
    // `bound` word is zero makes `inet_ctl_fdopen` take the descriptor over with no syscall at
    // all (`:5264`-`:5266`), which is the one state the refusal ladder cannot answer.
    let mut port = opened_port();
    let err = port
        .try_control(
            &op(13),
            &[INET_AF_INET, INET_TYPE_STREAM, 0, 0, 0, 0, 0, 0, 0, 0],
        )
        .expect_err("FDOPEN adopts a caller's descriptor, which the Driver seam cannot express");
    assert_eq!(reason_of(err), not_served_reason("fdopen", "inet_ctl"));
    // The name-service lookups are served, not gapped: an empty name/protocol pair is a lookup
    // the services database does not answer, i.e. the C's `ctl_error(EINVAL)`.
    assert_eq!(
        port.control(&op(17), &[0, 0]),
        ctl_xerror("einval"),
        "GETSERVBYNAME of an empty name"
    );
    assert_eq!(
        port.control(&op(18), &[0, 0, 0]),
        ctl_xerror("einval"),
        "GETSERVBYPORT of port 0"
    );

    // `ignorefd(28)` on a **connected** stream port: its reply means "suppress read *and* write
    // readiness", which a frozen `Driver` registration cannot express.
    let mut connected = connection();
    for suppress in [[1u8], [0u8]] {
        let err = connected
            .port
            .try_control(&op(28), &suppress)
            .expect_err("IGNOREFD would have to silence this driver's poller registration");
        assert_eq!(reason_of(err), not_served_reason("ignorefd", "inet_ctl"));
    }

    // `tcp_sendfile(45)` with its full 20-byte operand on a connected port: the length and the
    // connection are both checked first, so this is the one state left.
    let mut sendfile = vec![0u8; 20];
    sendfile[0..4].copy_from_slice(&0i32.to_be_bytes());
    let err = connected
        .port
        .try_control(&op(45), &sendfile)
        .expect_err("SENDFILE has no sendfile(2) path here");
    assert_eq!(reason_of(err), not_served_reason("tcp_sendfile", "tcp_inet_ctl"));
}

// -----------------------------------------------------------------------
// The two routes cannot drift
// -----------------------------------------------------------------------

/// The named and the numeric route reach the same verbs, so their replies must be identical —
/// the one exception being `close`, whose name the registry reserves
/// ([`crate::PortControl::CLOSE`]) and whose numeric form is the driver's own
/// `INET_REQ_CLOSE` verb.
#[test]
fn the_named_and_the_numeric_route_reply_identically() {
    let mut named = bare_port();
    let mut numeric = bare_port();

    assert_eq!(
        named.control("open", &open_arg()),
        numeric.control(&op(1), &open_arg()),
        "open vs INET_REQ_OPEN"
    );

    // `bind` to an ephemeral port: two sockets, so the ports differ; the shape must not.
    let named_bind = named.control("bind", &loopback_arg(0));
    let numeric_bind = numeric.control(&op(6), &loopback_arg(0));
    assert_eq!(named_bind.len(), numeric_bind.len(), "bind reply length");
    assert_eq!(named_bind[0], numeric_bind[0], "bind result tag");

    assert_eq!(
        named.control("listen", &[0, 5]),
        numeric.control(&op(27), &[0, 5]),
        "listen vs INET_REQ_LISTEN"
    );

    let mut record = vec![INET_OPT_KEEPALIVE];
    record.extend_from_slice(&1i32.to_be_bytes());
    assert_eq!(
        named.control("setopts", &record),
        numeric.control(&op(7), &record),
        "setopts vs INET_REQ_SETOPTS"
    );
    assert_eq!(
        named.control("getopts", &[INET_OPT_KEEPALIVE]),
        numeric.control(&op(8), &[INET_OPT_KEEPALIVE]),
        "getopts vs INET_REQ_GETOPTS"
    );
    // The named `accept` takes no operand; the numeric one carries the C's four-byte timeout.
    // Both reach the same synchronous accept with nothing pending.
    assert_eq!(
        named.control("accept", &[]),
        numeric.control(&op(26), &[0, 0, 0, 0]),
        "accept vs INET_REQ_ACCEPT"
    );
    assert_eq!(
        named.control("open", &open_arg()),
        ctl_xerror("einval"),
        "a second `open` on a live socket is refused on both routes"
    );
    assert_eq!(
        numeric.control(&op(1), &open_arg()),
        ctl_xerror("einval"),
        "and the numeric route agrees"
    );

    // A command that is not a decimal integer still takes the named path, where an unknown verb
    // is the driver's own `badarg` — the numeric bridge must never swallow it.
    assert!(matches!(
        numeric.try_control("frobnicate", &[]),
        Err(BeamError::BadArg)
    ));
    // The registry reserves the named [`crate::PortControl::CLOSE`]: that op closes the port and
    // answers no bytes at all, so `close` is the one verb whose two routes deliberately differ —
    // the numeric form reaches the driver's own `INET_REQ_CLOSE` verb and answers its `[1]`.
    assert_eq!(
        named.control("close", &[]),
        Vec::<u8>::new(),
        "the reserved `close` answers no bytes"
    );
    assert!(
        matches!(
            named.try_control("getopts", &[INET_OPT_KEEPALIVE]),
            Err(BeamError::BadArg)
        ),
        "and the port is gone afterwards"
    );
    assert_eq!(
        numeric.control(&op(2), &[]),
        ctl_ok(),
        "the numeric form is the driver's own close verb"
    );
}

// -----------------------------------------------------------------------
// The `erl` oracle matrix (skips loudly when `erl` is absent)
// -----------------------------------------------------------------------

// -----------------------------------------------------------------------
// `INET_REQ_SUBSCRIBE`(24)
// -----------------------------------------------------------------------

/// `INET_REQ_SUBSCRIBE`(24, `inet_subscribe` `:10819`) — the shared `inet_ctl` arm the kernel's
/// `inet_config:set_hostname/0` reaches through `prim_inet:close/1`'s
/// `subscribe(S, [subs_empty_out_q])`.
///
/// The reply is `[1] ++ (op ++ be32(outq byte count))*`; an operand byte the C's `switch` has no
/// case for is its `return -1`, i.e. the oracle's `badarg`, discarding the whole reply (so a valid
/// byte **before** the bad one does not survive). The counts here are `0` because a control call
/// runs between port-task rounds, when nothing is queued.
#[test]
fn subscribe_answers_the_c_outq_record_or_badarg() {
    let mut port = bare_port();
    assert_eq!(port.control(&op(24), &[]), vec![INET_REP_OK], "an empty operand");
    assert_eq!(
        port.control(&op(24), &[1]),
        vec![INET_REP_OK, 1, 0, 0, 0, 0],
        "one INET_SUBS_EMPTY_OUT_Q record"
    );
    assert_eq!(
        port.control(&op(24), &[1, 1]),
        vec![INET_REP_OK, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0],
        "one record per operand byte, in order"
    );
    assert_eq!(
        port.control(&op(24), &[1, 1, 1]).len(),
        1 + 3 * 5,
        "the reply grows by exactly five bytes per accepted op"
    );
    // A byte that is not `INET_SUBS_EMPTY_OUT_Q`: the C's `default: return -1`.
    for arg in [vec![2u8], vec![0], vec![255], vec![1, 2], vec![1, 1, 0]] {
        assert!(
            matches!(port.try_control(&op(24), &arg), Err(BeamError::BadArg)),
            "operand {arg:?} must be the oracle's badarg"
        );
    }
}

/// The compatibility gate for `subscribe`: the same calls against the real system `tcp_inet`
/// driver — including both `badarg` shapes, which the guarded probe catches instead of letting
/// them abort the expression.
///
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn the_subscribe_outcomes_agree_with_the_erl_oracle() {
    let calls = vec![
        Call::req(24, vec![], Rule::Exact),
        Call::req(24, vec![1], Rule::Exact),
        Call::req(24, vec![1, 1], Rule::Exact),
        Call::req(24, vec![1, 1, 1], Rule::Exact),
        Call::req(24, vec![2], Rule::Exact),
        Call::req(24, vec![1, 2], Rule::Exact),
        Call::req(24, vec![0], Rule::Exact),
    ];
    assert_outcomes_agree_with_the_oracle("tcp_inet", &calls, bare_port);
}

/// The `(opcode, operand)` list the oracle matrix drives, in one port's lifetime order: the bare/// (socket-less) queries and refusals, then `open` (plus the fake-address pair and its `NAME`
/// read-back), then the socket lifecycle with the state-dependent operands, then the same
/// descriptor queries after `close`.
///
/// Every row is deterministic on this host, which is why the whole list is one oracle run: the
/// rows that carry an ephemeral port or a descriptor number say so through their [`Rule`], and
/// everything else must agree byte for byte.
fn oracle_matrix() -> Vec<Call> {
    vec![
        // The bare (socket-less) port: the shared replies that need no descriptor.
        Call::req(15, vec![], Rule::Exact),               // GETTYPE  -> einval
        Call::req(14, vec![], Rule::Exact),               // GETFD    -> einval
        Call::req(16, vec![], Rule::Exact),               // GETSTATUS-> 0
        Call::req(5, vec![], Rule::Exact),                // NAME     -> ebadf
        Call::req(4, vec![], Rule::Exact),                // PEER     -> enotconn
        Call::req(60, vec![], Rule::Exact),               // the datagram driver's case
        Call::raw(1, open_arg().to_vec(), Rule::Exact),   // a wrapped opcode
        Call::raw(99, vec![], Rule::Exact),               // a retired opcode
        // The bare port's `tcp_inet_ctl` refusals: the state guard, then the operand length.
        Call::req(27, vec![], Rule::Exact),                // LISTEN len0     -> exbadport
        Call::req(27, vec![0, 5], Rule::Exact),            // LISTEN len2     -> exbadport
        Call::req(3, vec![1, 2], Rule::Exact),             // CONNECT len2    -> exbadport
        Call::req(44, vec![], Rule::Exact),                // TCP_SHUTDOWN    -> enotconn
        Call::req(43, vec![1, 2], Rule::Exact),            // TCP_UNRECV      -> enotconn
        Call::req(42, vec![1, 2], Rule::Exact),            // TCP_RECV len2   -> enotconn
        Call::req(42, vec![0u8; 8], Rule::Exact),          // TCP_RECV len8   -> enotconn
        Call::req(45, vec![], Rule::Exact),                // SENDFILE len0   -> einval
        Call::req(45, vec![0u8; 20], Rule::Exact),         // SENDFILE len20  -> enotconn
        Call::req(26, vec![], Rule::Exact),                // ACCEPT len0     -> einval
        Call::req(26, vec![0u8; 4], Rule::Exact),          // ACCEPT len4     -> einval
        Call::req(1, vec![1, 1], Rule::Exact),             // OPEN len2       -> einval
        // The `inet_ctl` arms: `len` guards, then the `IS_OPEN` guards.
        Call::req(6, vec![], Rule::Exact),                            // BIND len0
        Call::req(6, vec![1, 0, 80, 127, 0, 0, 1], Rule::Exact),       // BIND len7
        Call::req(29, vec![], Rule::Exact),                           // GETLADDRS len0
        Call::req(29, vec![0u8; 4], Rule::Exact),                     // GETLADDRS len4
        Call::req(30, vec![], Rule::Exact),                           // GETPADDRS len0
        Call::req(30, vec![0u8; 4], Rule::Exact),                     // GETPADDRS len4
        Call::req(13, vec![], Rule::Exact),                           // FDOPEN len0
        Call::req(13, vec![9, INET_TYPE_STREAM, 0, 0, 0, 0], Rule::Exact), // FDOPEN family 9
        Call::req(13, vec![INET_AF_INET, 9, 0, 0, 0, 0], Rule::Exact),     // FDOPEN type 9
        Call::req(21, vec![], Rule::Exact),                      // GETIFLIST  -> exbadport
        Call::req(25, vec![], Rule::Exact),                      // GETIFADDRS -> exbadport
        Call::req(22, vec![1], Rule::Exact),                     // IFGET      -> exbadport
        Call::req(23, vec![], Rule::Exact),                      // IFSET      -> exbadport
        Call::req(28, vec![1], Rule::Exact),                     // IGNOREFD   -> enotconn
        Call::req(17, vec![], Rule::Exact),                      // GETSERVBYNAME len0
        Call::req(18, vec![], Rule::Exact),                      // GETSERVBYPORT len0
        Call::req(12, vec![], Rule::Exact),                      // GETHOSTNAME -> [1] ++ name
        Call::req(12, vec![1], Rule::Exact),                     // GETHOSTNAME len1
        Call::req(19, vec![], Rule::Exact),                      // SETNAME len0 -> [1]
        Call::req(19, vec![1], Rule::Exact),                     // SETNAME len1 -> einval
        Call::req(20, vec![], Rule::Exact),                      // SETPEER len0 -> [1]
        Call::req(20, vec![1], Rule::Exact),                     // SETPEER len1 -> einval
        // `SUBSCRIBE`: an outq of 0 (nothing queued between port-task rounds), one record per
        // operand byte. The `badarg` operands ride the guarded probe above — a raise would abort
        // this expression.
        Call::req(24, vec![], Rule::Exact),                      // SUBSCRIBE -> [1]
        Call::req(24, vec![1], Rule::Exact),                     // SUBSCRIBE one record
        Call::req(24, vec![1, 1], Rule::Exact),                  // SUBSCRIBE two records        // The socket lifecycle.
        Call::req(1, open_arg().to_vec(), Rule::Exact),   // OPEN
        Call::req(15, vec![], Rule::Exact),               // GETTYPE  -> the cached stream pair
        Call::req(5, vec![], Rule::Exact),                // NAME     -> the unbound address
        Call::req(29, vec![0u8; 4], Rule::Exact),         // GETLADDRS is the NAME reply
        Call::req(30, vec![0u8; 4], Rule::Exact),         // GETPADDRS-> enotconn
        Call::req(42, vec![1, 2], Rule::Exact),           // TCP_RECV len2 -> enotconn
        Call::req(3, vec![1, 2], Rule::Exact),            // CONNECT len2  -> einval
        Call::req(28, vec![1], Rule::Exact),              // IGNOREFD      -> enotconn
        // The fake address: stored without a socket, reported by `NAME`, cleared again; `PEER`
        // keeps its connection guard in front of it.
        Call::req(19, vec![INET_AF_INET, 0, 80, 1, 2, 3, 4], Rule::Exact), // SETNAME -> [1]
        Call::req(5, vec![], Rule::Exact),                // NAME -> the fake address
        Call::req(20, vec![INET_AF_INET, 0, 81, 127, 0, 0, 1], Rule::Exact), // SETPEER -> [1]
        Call::req(4, vec![], Rule::Exact),                // PEER -> enotconn (the guard wins)
        Call::req(19, vec![], Rule::Exact),               // SETNAME clears
        Call::req(5, vec![], Rule::Exact),                // NAME -> the socket again
        Call::req(20, vec![], Rule::Exact),               // SETPEER clears
        Call::req(19, vec![INET_AF_LOOPBACK, 0, 80], Rule::Exact), // the short form now works
        Call::req(5, vec![], Rule::Exact),                // NAME -> 127.0.0.1:80
        Call::req(19, vec![], Rule::Exact),               // SETNAME clears again
        // The bound listener.
        Call::req(6, loopback_arg(0), Rule::BoundPort),   // BIND (ephemeral port)
        Call::req(27, vec![], Rule::Exact),               // LISTEN len0 -> einval
        Call::req(27, vec![0, 5], Rule::Exact),           // LISTEN
        Call::req(16, vec![], Rule::Exact),               // GETSTATUS-> 9
        Call::req(15, vec![], Rule::Exact),               // GETTYPE  -> stream
        Call::req(14, vec![], Rule::Descriptor),          // GETFD
        Call::req(5, vec![], Rule::PackedAddress),        // NAME     -> the bound address
        Call::req(4, vec![], Rule::Exact),                // PEER     -> enotconn
        Call::req(7, vec![INET_OPT_KEEPALIVE, 0, 0, 0, 1], Rule::Exact), // SETOPTS
        Call::req(8, vec![INET_OPT_KEEPALIVE], Rule::Exact), // GETOPTS
        Call::req(11, vec![], Rule::Exact),               // GETSTAT  -> [1]
        Call::req(42, vec![0u8; 8], Rule::Exact),         // TCP_REQ_RECV on a listener
        Call::req(26, vec![], Rule::Exact),               // ACCEPT (wrong-length operand)
        // After `close`: the descriptor is gone, the cached type pair is not.
        Call::req(2, vec![], Rule::Exact),                // CLOSE
        Call::req(15, vec![], Rule::Exact),               // GETTYPE  -> the cached pair
        Call::req(14, vec![], Rule::Exact),               // GETFD    -> einval
        Call::req(5, vec![], Rule::Exact),                // NAME     -> ebadf
        Call::req(16, vec![], Rule::Exact),               // GETSTATUS-> 0
        Call::req(42, vec![1, 2], Rule::Exact),           // TCP_RECV len2 -> enotconn
        Call::req(29, vec![0u8; 4], Rule::Exact),         // GETLADDRS -> exbadport
    ]
}

/// The **compatibility gate** for the TCP numeric bridge: the same call list driven against the
/// real system `tcp_inet` port and against this crate's port over `port_control`.
///
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn the_numeric_replies_agree_with_the_erl_oracle() {
    let calls = oracle_matrix();
    let Some(dump) = erl_stdout(&oracle_expression("tcp_inet", &calls)) else {
        eprintln!("SKIP: no `erl` on PATH — skipping the tcp_inet numeric-opcode oracle matrix");
        return;
    };
    let oracle = oracle_replies(&dump);
    assert_eq!(
        oracle.len(),
        calls.len(),
        "the oracle answered every call: {dump:?}"
    );

    let mut port = bare_port();
    let rust = port.run(&calls);
    assert_eq!(rust.len(), calls.len(), "this floor answered every call");

    for (index, call) in calls.iter().enumerate() {
        let label = format!("call #{index} (opcode {} , operand {:?})", call.value, call.arg);
        assert_agrees(&label, call.rule, &oracle[index], &rust[index]);
    }
}
