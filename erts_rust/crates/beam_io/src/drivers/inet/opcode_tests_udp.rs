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

//! The `udp_inet` driver's **numeric control-opcode** suite — the datagram half of the bridge in
//! [`super::opcode`], driven through the runtime's own `erlang:port_control/3` path
//! ([`crate::PortRegistry::port_control`]) exactly like the sibling
//! [`opcode_tests`](super::opcode_tests) suite drives the TCP half. The matrix helpers, the port
//! rig and the oracle plumbing are shared from that file rather than copied.
use super::opcode_tests::{
    assert_agrees, assert_outcomes_agree_with_the_oracle, erl_stdout, loopback_arg, op,
    oracle_expression, oracle_replies, reason_of, Call, Rig, Rule, CONTROL_MAGIC,};use super::opt::{INET_OPT_BROADCAST, INET_OPT_REUSEADDR};
use super::wire::{ctl_ok, ctl_xerror};
use super::{
    UdpInetDriver, INET_AF_ANY, INET_AF_INET, INET_AF_LOOPBACK, INET_PROTO_DEFAULT, INET_REP_OK,
    INET_TYPE_DGRAM,
};
use beam_common::BeamError;

/// The packed `open` argument of the datagram driver: `[AF_INET, SOCK_DGRAM, default protocol]`.
fn open_arg() -> [u8; 3] {
    [INET_AF_INET, INET_TYPE_DGRAM, INET_PROTO_DEFAULT]
}

/// A **bare** (socket-less) `udp_inet` port — the state the driver factory starts in.
fn bare_port() -> Rig {
    Rig::new(Box::new(UdpInetDriver::new()))
}

/// An `open`ed `udp_inet` port (a datagram socket exists, nothing is bound).
fn opened_port() -> Rig {
    let mut port = bare_port();
    assert_eq!(
        port.control(&op(1), &open_arg()),
        ctl_ok(),
        "INET_REQ_OPEN(1) creates the socket"
    );
    port
}

/// An `open`ed + `bind`ed `udp_inet` port over the loopback interface; returns the port it bound.
fn bound_port() -> (Rig, u16) {
    let mut port = opened_port();
    let bound = port.control(&op(6), &loopback_arg(0));
    assert_eq!(bound[0], INET_REP_OK, "INET_REQ_BIND succeeds");
    assert_eq!(bound.len(), 3, "the reply is [1, port_hi, port_lo]");
    let bound_port = u16::from_be_bytes([bound[1], bound[2]]);
    assert_ne!(bound_port, 0, "the kernel chose a port for us");
    (port, bound_port)
}

/// The C's `INET_REQ_CONNECT` operand: `[Timeout(4), packed-address]`
/// (`inet_drv.c:14814`-`:14830`). The timeout is ignored for UDP, which never blocks.
fn connect_arg(port: u16) -> Vec<u8> {
    let [hi, lo] = port.to_be_bytes();
    let mut arg = vec![0, 0, 0, 0];
    arg.extend_from_slice(&[INET_AF_INET, hi, lo, 127, 0, 0, 1]);
    arg
}

/// An `open`ed + `bind`ed + `connect`ed `udp_inet` port; returns the local port it bound.
///
/// A datagram `connect(2)` installs a default peer and returns at once — no listener is needed and
/// nothing is sent — so this stays a pure in-process state change.
fn connected_port() -> (Rig, u16) {
    let (mut port, bound) = bound_port();
    assert_eq!(
        port.control(&op(3), &connect_arg(9)),
        ctl_ok(),
        "INET_REQ_CONNECT on a datagram socket"
    );
    (port, bound)
}

/// Asserts that `code` is still one of this floor's **typed** "not served" gaps — a capability
/// whose body the C has and this driver does not, never a fabricated reply.
fn assert_typed_gap(port: &mut Rig, code: u32, arg: &[u8], label: &str) {
    match port.try_control(&op(code), arg) {
        Ok(reply) => panic!("{label}: expected the typed gap, got {reply:?}"),
        Err(err) => assert!(
            reason_of(err).starts_with("unsupported: control opcode"),
            "{label}: not the typed not-served gap"
        ),
    }
}
// -----------------------------------------------------------------------
// The numeric lifecycle verbs
// -----------------------------------------------------------------------

/// `INET_REQ_OPEN`(1) with the datagram argument creates the socket and `INET_REQ_BIND`(6)
/// reports the port the kernel assigned.
#[test]
fn the_numeric_open_and_bind_sequence_on_a_datagram_port() {
    let mut port = bare_port();
    assert_eq!(
        port.control(&op(16), &[]),
        vec![INET_REP_OK, 0, 0, 0, 0],
        "INET_REQ_GETSTATUS on a bare port is the closed state"
    );
    assert_eq!(port.control(&op(1), &open_arg()), ctl_ok(), "INET_REQ_OPEN");

    // The C's `INET_AF_ANY` short form: family tag + the two port bytes, port 0 = "pick one".
    let bound = port.control(&op(6), &[INET_AF_ANY, 0, 0]);
    assert_eq!(bound[0], INET_REP_OK);
    assert_eq!(bound.len(), 3, "[1, port_hi, port_lo]");
    assert_ne!(u16::from_be_bytes([bound[1], bound[2]]), 0);

    // A datagram port never becomes a listener: it reports the plain open state throughout.
    assert_eq!(
        port.control(&op(16), &[]),
        vec![INET_REP_OK, 0, 0, 0, 1],
        "`INET_STATE_OPEN` after open+bind"
    );
}

/// `INET_REQ_SETOPTS`(7)/`INET_REQ_GETOPTS`(8) reach the datagram option store, and the two
/// served option families behave as they do on the named route.
#[test]
fn numeric_setopts_and_getopts_on_a_datagram_port() {
    let mut port = opened_port();
    for code in [INET_OPT_REUSEADDR, INET_OPT_BROADCAST] {
        let mut record = vec![code];
        record.extend_from_slice(&1i32.to_be_bytes());
        assert_eq!(port.control(&op(7), &record), ctl_ok(), "INET_REQ_SETOPTS");

        let mut expected = ctl_ok();
        expected.push(code);
        expected.extend_from_slice(&1i32.to_be_bytes());
        assert_eq!(
            port.control(&op(8), &[code]),
            expected,
            "option {code} reads back"
        );
    }

    // An unserved code replaces the whole reply with the C's `[0]einval`, on this route too.
    assert_eq!(port.control(&op(8), &[200]), ctl_xerror("einval"));
    let mut record = vec![200u8];
    record.extend_from_slice(&1i32.to_be_bytes());
    record.push(0);
    assert_eq!(port.control(&op(7), &record), ctl_xerror("einval"));
}

/// `INET_REQ_GETTYPE`(15) reports `SOCK_DGRAM` for a datagram socket and refuses with `einval`
/// while there is none.
#[test]
fn numeric_gettype_reports_a_datagram_socket() {
    let mut bare = bare_port();
    assert_eq!(
        bare.control(&op(15), &[]),
        ctl_xerror("einval"),
        "the C's `if (!IS_OPEN) return ctl_error(EINVAL)`"
    );

    let mut port = opened_port();
    assert_eq!(
        port.control(&op(15), &[]),
        vec![
            INET_REP_OK,
            0,
            0,
            0,
            INET_AF_INET,
            0,
            0,
            0,
            INET_TYPE_DGRAM
        ],
        "AF_INET + SOCK_DGRAM, big-endian"
    );
}

/// `INET_REQ_GETSTATUS`(16) is the C's state bit set: `1` for an open datagram socket (`0` while
/// there is no socket). A datagram socket has no listening or connected bit to report here.
#[test]
fn numeric_getstatus_on_a_datagram_port() {
    let mut bare = bare_port();
    assert_eq!(bare.control(&op(16), &[]), vec![INET_REP_OK, 0, 0, 0, 0]);

    let mut port = opened_port();
    assert_eq!(port.control(&op(16), &[]), vec![INET_REP_OK, 0, 0, 0, 1]);

    let mut bound = bare_port();
    assert_eq!(bound.control(&op(1), &open_arg()), ctl_ok());
    assert_eq!(bound.control(&op(6), &[INET_AF_ANY, 0, 0])[0], INET_REP_OK);
    assert_eq!(
        bound.control(&op(16), &[]),
        vec![INET_REP_OK, 0, 0, 0, 1],
        "binding does not change the state bits"
    );
}

/// `INET_REQ_PEER`(4) is `enotconn` on a datagram socket (there is no connection to name),
/// `INET_REQ_NAME`(5) reports the bound local address, and `INET_REQ_GETSTAT`(11) answers the
/// bare success tag for an empty request.
#[test]
fn numeric_peer_name_and_getstat_on_a_datagram_port() {
    let mut bare = bare_port();
    assert_eq!(bare.control(&op(4), &[]), ctl_xerror("enotconn"));
    assert_eq!(
        bare.control(&op(5), &[]),
        ctl_xerror("ebadf"),
        "the C's `getsockname(2)` with no descriptor"
    );

    let (mut port, bound) = bound_port();
    assert_eq!(
        port.control(&op(4), &[]),
        ctl_xerror("enotconn"),
        "`INET_F_ACTIVE` is never set by a datagram socket"
    );

    let name = port.control(&op(5), &[]);
    assert_eq!(name.len(), 8, "[1, family, port_hi, port_lo, a, b, c, d]");
    assert_eq!(name[0], INET_REP_OK);
    assert_eq!(name[1], INET_AF_INET);
    assert_eq!(
        u16::from_be_bytes([name[2], name[3]]),
        bound,
        "the port `bind` reported"
    );
    assert_eq!(&name[4..8], &[127, 0, 0, 1], "the loopback bind address");

    assert_eq!(port.control(&op(11), &[]), ctl_ok(), "INET_REQ_GETSTAT");

    // `INET_REQ_GETFD`(14) needs a socket and then reports the driver's own descriptor.
    let mut opened = opened_port();
    let fd = opened.control(&op(14), &[]);
    assert_eq!(fd.len(), 5, "[1] ++ fd_be32");
    assert_eq!(fd[0], INET_REP_OK);
    assert_ne!(i32::from_be_bytes([fd[1], fd[2], fd[3], fd[4]]), 0);
    assert_eq!(bare.control(&op(14), &[]), ctl_xerror("einval"));
}

/// `PACKET_REQ_RECV`(60) is `packet_inet_ctl`'s own receive: it needs a socket **first** (the
/// C's `!IS_OPEN ⇒ `[0]exbadport`) and only then the eight-byte `Timeout(4), Length(4)` operand.
///
/// Divergence from the C, recorded instead of asserted away: with a well-formed operand the C
/// enqueues an asynchronous receive and answers the three-byte op id `[1] ++ async_id_be16`
/// (measured `[1,0,0]`), the datagram arriving later as an `{inet_async, …}` message. This floor
/// has no driver→owner message path, so its `recv` is the synchronous read-now substitution: the
/// **bare** `[1]` when no datagram is pending, `[1] ++ source_address` when one is.
#[test]
fn numeric_packet_recv_validates_its_operand_and_needs_a_socket() {
    let mut bare = bare_port();
    assert_eq!(
        bare.control(&op(60), &[0u8; 8]),
        ctl_xerror("exbadport"),
        "the socket check comes before the operand check (measured)"
    );
    assert_eq!(
        bare.control(&op(60), &[]),
        ctl_xerror("exbadport"),
        "and a missing operand does not change that"
    );

    let mut port = opened_port();
    assert_eq!(
        port.control(&op(60), &[0, 0, 0, 0]),
        ctl_xerror("einval"),
        "with a socket, a wrong-length operand is the C's EINVAL"
    );
    assert_eq!(
        port.control(&op(60), &[0u8; 8]),
        ctl_ok(),
        "the synchronous floor's bare [1]: no datagram is pending"
    );
}

// -----------------------------------------------------------------------
// The scope rules: a case of the *other* switch, a wrapped number, an unserved case
// -----------------------------------------------------------------------

/// The stream-only requests are cases of `tcp_inet_ctl` only, so on a datagram port neither
/// switch owns them and the C's `inet_ctl` default arm answers `[0]exbadport`.
#[test]
fn tcp_only_numeric_opcodes_on_the_datagram_port_are_exbadport() {
    let mut port = opened_port();
    for code in [
        42u32, // TCP_REQ_RECV
        43,    // TCP_REQ_UNRECV
        44,    // TCP_REQ_SHUTDOWN
        45,    // TCP_REQ_SENDFILE
        26,    // INET_REQ_ACCEPT
    ] {
        assert_eq!(
            port.control(&op(code), &[]),
            ctl_xerror("exbadport"),
            "request {code} is a `tcp_inet_ctl` case"
        );
    }
}

/// A command below the magic number wraps in the C's `unsigned int` subtraction and matches no
/// case, so it answers the C's `[0]exbadport` — never a silent success and never a `badarg`.
#[test]
fn a_wrapped_or_unassigned_numeric_command_on_the_datagram_port_is_exbadport() {
    let mut port = opened_port();
    for command in ["0", "1", "99", "-1", &(9u64 + CONTROL_MAGIC).to_string()] {
        assert_eq!(
            port.control(command, &open_arg()),
            ctl_xerror("exbadport"),
            "`{command}` matches no switch's case"
        );
    }
}

/// The opcodes that stay **typed gaps** on this driver — a capability the C has and this floor
/// does not, answered with a typed error rather than a fabricated reply.
///
/// The one survivor is `INET_REQ_FDOPEN`(13) with the operand that reaches the C's **blind**
/// adoption: a ten-byte form whose `bound` word is zero makes `inet_ctl_fdopen` take the
/// descriptor over with no syscall at all (`inet_drv.c:5264`-`:5266`). Adopting a caller's
/// descriptor number would mean taking ownership of an fd handed in from Erlang, which no
/// `DriverFactory` ctor can express (and which the C deliberately never closes,
/// `desc->prebound = 1`). Everything else in the `inet_ctl` arm is served: the name-service
/// lookups (`GETSERVBYNAME`/`GETSERVBYPORT`), the interface family
/// (`GETIFLIST`/`IFGET`/`IFSET`/`GETIFADDRS`) and `INET_REQ_SUBSCRIBE`(24).
///
/// The failure shapes around the gap are the C's own `ctl_error(EINVAL)`/`ctl_xerror(EXBADPORT)`
/// refusals.
#[test]
fn the_surviving_numeric_opcodes_on_the_datagram_port_are_typed_gaps() {
    let mut port = opened_port();
    assert_typed_gap(
        &mut port,
        13,
        &[1, 2, 0, 0, 0, 1, 0, 0, 0, 0],
        "a well-formed INET_REQ_FDOPEN",
    );
    assert_typed_gap(
        &mut port,
        13,
        &[1, 1, 0, 0, 0, 1, 0, 0, 0, 0],
        "a stream FDOPEN",
    );

    // The name-service lookups are served (they are shared `inet_ctl` arms); a missing entry is
    // the C's `[0]einval`, never a typed gap.
    assert_eq!(
        port.control(&op(17), &[3, 100]),
        ctl_xerror("einval"),
        "GETSERVBYNAME of a name the services database does not have"
    );
    assert_eq!(
        port.control(&op(18), &[0, 53, 3]),
        ctl_xerror("einval"),
        "GETSERVBYPORT needs the protocol length byte's string"
    );
}
// -----------------------------------------------------------------------
// `INET_REQ_SUBSCRIBE`(24) on the datagram driver
// -----------------------------------------------------------------------

/// `INET_REQ_SUBSCRIBE`(24, `inet_subscribe` `:10819`) is the shared `inet_ctl` arm, so the
/// datagram driver answers exactly what the stream one does: `[1] ++ (op ++ be32(outq))*`, and the
/// C's `return -1` (`badarg`) for an operand byte its `switch` has no case for.
#[test]
fn subscribe_answers_the_c_outq_record_or_badarg() {
    let mut port = bare_port();
    assert_eq!(port.control(&op(24), &[]), vec![INET_REP_OK], "an empty operand");
    assert_eq!(
        port.control(&op(24), &[1]),
        vec![INET_REP_OK, 1, 0, 0, 0, 0],
        "one INET_SUBS_EMPTY_OUT_Q record, outq 0"
    );
    assert_eq!(
        port.control(&op(24), &[1, 1]),
        vec![INET_REP_OK, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0],
        "one record per operand byte"
    );
    for arg in [vec![2u8], vec![0], vec![1, 2]] {
        assert!(
            matches!(port.try_control(&op(24), &arg), Err(BeamError::BadArg)),
            "operand {arg:?} must be the oracle's badarg"
        );
    }
}

/// The `outq` figure is the port's **real** queued-output depth, not a constant.
///
/// A port command on a socket-less datagram port is refused by the driver
/// ([`super::UdpInetDriver::write_pending`] answers `BadArg`) and its bytes stay queued — exactly
/// the state the C reports `driver_sizeq(desc->port)` for.
#[test]
fn subscribe_reports_the_ports_real_queued_output_depth() {
    let mut port = bare_port();
    let err = port
        .command(&[1, 2, 3])
        .expect_err("a socket-less datagram port refuses the write");
    assert!(matches!(err, BeamError::BadArg), "the refusal is badarg: {err:?}");

    assert_eq!(
        port.control(&op(24), &[1]),
        vec![INET_REP_OK, 1, 0, 0, 0, 3],
        "the reply reports the three bytes the refused command left queued"
    );
    assert_eq!(
        port.control(&op(24), &[1, 1]),
        vec![INET_REP_OK, 1, 0, 0, 0, 3, 1, 0, 0, 0, 3],
        "every record reports that same depth"
    );
    assert_eq!(
        port.control(&op(24), &[]),
        vec![INET_REP_OK],
        "an empty operand reports no depth at all"
    );
}

/// The compatibility gate for the datagram half of `subscribe`, including both `badarg` shapes —
/// which need the guarded probe, since a raise would abort a plain `port_control` expression.
///
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn the_subscribe_outcomes_agree_with_the_erl_oracle() {
    let calls = vec![
        Call::req(24, vec![], Rule::Exact),
        Call::req(24, vec![1], Rule::Exact),
        Call::req(24, vec![1, 1], Rule::Exact),
        Call::req(24, vec![2], Rule::Exact),
        Call::req(24, vec![1, 2], Rule::Exact),
    ];
    assert_outcomes_agree_with_the_oracle("udp_inet", &calls, bare_port);
}

// -----------------------------------------------------------------------
// The C's refusals, promoted to exact replies
// -----------------------------------------------------------------------
/// `INET_REQ_GETTYPE`(15) switches on the descriptor's **cached** `sfamily`/`stype`
/// (`inet_drv.c:11165`-`:11203`) — set by `inet_ctl_open` (`:5214`) and `inet_ctl_fdopen` (`:5265`)
/// and cleared by neither a close nor `inet_stop`. It is therefore **not** an `IS_OPEN` guard: a
/// never-opened port answers `einval` only because `sfamily == 0` matches no case.
///
/// Measured against the oracle: bare ⇒ `[0]einval`; opened ⇒ `[1,0,0,0,1,0,0,0,2]`; **after a
/// close** ⇒ `[1,0,0,0,1,0,0,0,2]` again, while `GETFD`/`GETSTATUS` correctly report the socket is
/// gone. This is the residual a `Option<RawFd>`-based `gettype_reply` could not express.
#[test]
fn numeric_gettype_reports_the_cached_kind_after_a_close() {
    let datagram = vec![
        INET_REP_OK,
        0,
        0,
        0,
        INET_AF_INET,
        0,
        0,
        0,
        INET_TYPE_DGRAM,
    ];

    let mut port = bare_port();
    assert_eq!(
        port.control(&op(15), &[]),
        ctl_xerror("einval"),
        "nothing cached on a never-opened port"
    );
    // A close on a never-opened port changes nothing either (the C's zeroed descriptor).
    assert_eq!(port.control(&op(2), &[]), ctl_ok(), "INET_REQ_CLOSE");
    assert_eq!(port.control(&op(15), &[]), ctl_xerror("einval"), "still nothing cached");

    assert_eq!(port.control(&op(1), &open_arg()), ctl_ok(), "INET_REQ_OPEN");
    assert_eq!(port.control(&op(15), &[]), datagram, "the opened socket's kind");

    assert_eq!(port.control(&op(2), &[]), ctl_ok(), "INET_REQ_CLOSE");
    assert_eq!(port.control(&op(14), &[]), ctl_xerror("einval"), "no descriptor");
    assert_eq!(
        port.control(&op(16), &[]),
        vec![INET_REP_OK, 0, 0, 0, 0],
        "`INET_STATE_CLOSED`"
    );
    assert_eq!(
        port.control(&op(15), &[]),
        datagram,
        "the cached sfamily/stype survive the close"
    );
}

/// `INET_REQ_LISTEN`(27) is a case of `packet_inet_ctl` only inside `#ifdef HAVE_SCTP`
/// (`inet_drv.c:14846`). With SCTP compiled out the number is no case of either switch, so it
/// reaches `inet_ctl`'s `default: ctl_xerror(EXBADPORT)` (`:11551`) — in **every** state and for
/// **every** operand (measured bare and opened, with a 0- and a 2-byte operand).
#[test]
fn numeric_listen_on_a_datagram_port_is_exbadport() {
    for arg in [Vec::new(), vec![0u8, 5]] {
        let mut bare = bare_port();
        assert_eq!(
            bare.control(&op(27), &arg),
            ctl_xerror("exbadport"),
            "bare port, operand {arg:?}"
        );
        let mut opened = opened_port();
        assert_eq!(
            opened.control(&op(27), &arg),
            ctl_xerror("exbadport"),
            "opened port, operand {arg:?}"
        );
    }
}

/// The C's guard **order**, which is what makes the refusals byte-exact: `BIND` checks its operand
/// length before the socket state (`:11417`/`:11419`), while `CONNECT` and `PACKET_REQ_RECV` check
/// the state first (`:14727`/`:14809`, `:14981`/`:14988`).
#[test]
fn numeric_operand_faults_follow_the_c_guard_order() {
    let mut bare = bare_port();
    assert_eq!(
        bare.control(&op(6), &[]),
        ctl_xerror("einval"),
        "BIND: the length guard comes first, even with no socket at all"
    );
    assert_eq!(
        bare.control(&op(6), &[INET_AF_ANY, 0, 0]),
        ctl_xerror("exbadport"),
        "BIND: a well-formed address reaches the state guard"
    );
    assert_eq!(
        bare.control(&op(3), &[0, 0]),
        ctl_xerror("exbadport"),
        "CONNECT: the state guard precedes the length guard"
    );
    assert_eq!(
        bare.control(&op(3), &[]),
        ctl_xerror("exbadport"),
        "and the empty dissolve form is guarded too"
    );
    assert_eq!(
        bare.control(&op(60), &[]),
        ctl_xerror("exbadport"),
        "PACKET_REQ_RECV: the socket check comes first"
    );
    assert_eq!(bare.control(&op(60), &[0u8; 8]), ctl_xerror("exbadport"));

    let mut opened = opened_port();
    assert_eq!(
        opened.control(&op(60), &[0, 0]),
        ctl_xerror("einval"),
        "with a socket, a wrong-length operand is EINVAL"
    );
    assert_eq!(
        opened.control(&op(3), &[0, 0]),
        ctl_xerror("einval"),
        "CONNECT: fewer than six bytes"
    );
    assert_eq!(
        opened.control(&op(3), &[]),
        ctl_ok(),
        "the empty CONNECT dissolves the default peer"
    );
    assert_eq!(
        opened.control(&op(1), &[1, 2]),
        ctl_xerror("einval"),
        "OPEN requires the three-byte [AF, TYPE, PROTO] record"
    );
}

/// `INET_REQ_GETHOSTNAME`(12, `:11219`): `len != 0 ⇒ EINVAL`, else the local `gethostname(2)`
/// payload from `sock_hostname`. No resolver is involved, so the whole arm is exact.
#[test]
fn numeric_gethostname_reports_the_local_host_name() {
    let mut port = opened_port();
    let mut expected = ctl_ok();
    expected.extend_from_slice(&super::sys::hostname().expect("gethostname(2)"));
    assert_eq!(port.control(&op(12), &[]), expected);
    assert!(
        !port.control(&op(12), &[])[1..].contains(&0),
        "the payload is NUL-trimmed"
    );
    for arg in [vec![0u8], vec![0u8, 0]] {
        assert_eq!(
            port.control(&op(12), &arg),
            ctl_xerror("einval"),
            "GETHOSTNAME takes no operand"
        );
    }
}

/// `packet_inet_ctl`'s `INET_REQ_FDOPEN`(13, `:14639`) validates in the C's order: the 6/10-byte
/// length (`:14647`), the family tag (`⇒ eafnosupport`, `:14659`), then the socket type
/// (`⇒ EINVAL`, `:14671`). Only the well-formed adopt itself stays a gap.
#[test]
fn numeric_fdopen_refuses_a_bad_length_family_and_type() {
    let mut port = opened_port();
    for arg in [vec![], vec![1, 2, 0], vec![1, 2, 0, 0, 0], vec![0u8; 11]] {
        assert_eq!(
            port.control(&op(13), &arg),
            ctl_xerror("einval"),
            "FDOPEN operand {arg:?} is not 6 or 10 bytes"
        );
    }
    assert_eq!(
        port.control(&op(13), &[9, 2, 0, 0, 0, 1]),
        ctl_xerror("eafnosupport"),
        "family tag 9 is not AF_INET"
    );
    assert_eq!(
        port.control(&op(13), &[1, 9, 0, 0, 0, 1]),
        ctl_xerror("einval"),
        "type 9 is neither SOCK_STREAM nor SOCK_DGRAM"
    );
    // The well-formed adopt is the typed gap. The `bound == 0` form is used because it is the one
    // that reaches the gap without a descriptor probe: any fd this test could name is either not a
    // socket or not bound, so the `bound = 1` ladder would refuse first.
    assert_typed_gap(
        &mut port,
        13,
        &[1, 2, 0, 0, 0, 1, 0, 0, 0, 0],
        "a well-formed unbound FDOPEN",
    );
}

/// `INET_REQ_GETSERVBYNAME`(17, `:11493`)/`INET_REQ_GETSERVBYPORT`(18, `:11525`): only the operand
/// -length refusals (`len < 2`, `len < 3`) are served; the `getservbyname(3)`/`getservbyport(3)`
/// lookup is a typed gap.
#[test]
fn numeric_getserv_requests_serve_their_length_refusals() {
    let mut port = opened_port();
    for arg in [vec![], vec![1u8]] {
        assert_eq!(
            port.control(&op(17), &arg),
            ctl_xerror("einval"),
            "GETSERVBYNAME needs two bytes"
        );
    }
    for arg in [vec![], vec![0u8], vec![0, 53]] {
        assert_eq!(
            port.control(&op(18), &arg),
            ctl_xerror("einval"),
            "GETSERVBYPORT needs three bytes"
        );
    }
    assert_eq!(
        port.control(&op(17), &[3, 100]),
        ctl_xerror("einval"),
        "a well-formed GETSERVBYNAME the services database does not answer"
    );
    assert_eq!(
        port.control(&op(18), &[0, 53, 3]),
        ctl_xerror("einval"),
        "a well-formed GETSERVBYPORT with no room for the protocol string"
    );
}

/// `INET_REQ_SETNAME`(19, `:11386`)/`INET_REQ_SETPEER`(20, `:11301`): an **empty** operand clears
/// the fake address and answers `[1]`; a one-byte one is `len < 2 ⇒ EINVAL`; anything else is
/// decoded by `inet_set_faddress` (`:4795`) and stored, its `str_einval`/`str_eafnosupport`
/// rendered through `ctl_xerror` (`:11399`/`:11313`).
///
/// The stored address then wins over the socket: `INET_REQ_NAME` reports it **first**
/// (`desc->name_ptr`, `:11351`-`:11360`) — on a socket-less port too — and `INET_REQ_PEER` does so
/// only **after** its connection guard (`:11282`/`:11286`), while `GETLADDRS`/`GETPADDRS` keep
/// calling `sock_name`/`sock_peer` and bypass the fake entirely.
#[test]
fn numeric_setname_and_setpeer_install_a_fake_address() {
    let fake = vec![INET_AF_INET, 4, 210, 1, 2, 3, 4];
    let packed_fake = vec![INET_REP_OK, INET_AF_INET, 4, 210, 1, 2, 3, 4];

    // A bare port: the fake local address is reported even with no socket at all.
    let mut bare = bare_port();
    assert_eq!(bare.control(&op(19), &[]), ctl_ok(), "an empty SETNAME clears");
    assert_eq!(bare.control(&op(20), &[]), ctl_ok(), "an empty SETPEER clears");
    for code in [19u32, 20] {
        assert_eq!(
            bare.control(&op(code), &[INET_AF_INET]),
            ctl_xerror("einval"),
            "a one-byte operand is EINVAL"
        );
    }
    assert_eq!(bare.control(&op(19), &fake), ctl_ok(), "SETNAME");
    assert_eq!(bare.control(&op(5), &[]), packed_fake, "NAME reports the fake");
    assert_eq!(bare.control(&op(19), &[]), ctl_ok(), "clearing it again");
    assert_eq!(
        bare.control(&op(5), &[]),
        ctl_xerror("ebadf"),
        "and `getsockname(2)` has no descriptor to name"
    );

    // The ANY/LOOPBACK short form needs the descriptor's **cached** family: the C's
    // `switch (family)` (`:4826`) picks `INADDR_ANY`/`INADDR_LOOPBACK` only for `AF_INET`, and a
    // never-opened descriptor's `sfamily == 0` falls to `str_einval`.
    assert_eq!(
        bare.control(&op(19), &[INET_AF_ANY, 4, 210]),
        ctl_xerror("einval"),
        "no cached family yet"
    );

    // A stored peer is reported only once the socket is connected.
    assert_eq!(bare.control(&op(20), &fake), ctl_ok(), "SETPEER");
    assert_eq!(
        bare.control(&op(4), &[]),
        ctl_xerror("enotconn"),
        "the connection guard comes first"
    );

    let (mut connected, _) = connected_port();
    assert_eq!(connected.control(&op(20), &fake), ctl_ok(), "SETPEER");
    assert_eq!(
        connected.control(&op(4), &[]),
        packed_fake,
        "a connected socket reports the fake peer"
    );
    assert_eq!(
        connected.control(&op(30), &[0, 0, 0, 0]),
        vec![INET_REP_OK, INET_AF_INET, 0, 9, 127, 0, 0, 1],
        "GETPADDRS bypasses the fake and reports the real peer"
    );
    assert_eq!(connected.control(&op(20), &[]), ctl_ok(), "clearing the fake peer");
    assert_eq!(
        connected.control(&op(4), &[]),
        vec![INET_REP_OK, INET_AF_INET, 0, 9, 127, 0, 0, 1],
        "back to the real peer"
    );

    // The (bound) local address the same asymmetry hands back after a SETNAME.
    let (mut bound, port) = bound_port();
    assert_eq!(bound.control(&op(19), &[INET_AF_ANY, 4, 210]), ctl_ok());
    assert_eq!(
        bound.control(&op(5), &[]),
        vec![INET_REP_OK, INET_AF_INET, 4, 210, 0, 0, 0, 0],
        "NAME reports the fake"
    );
    let real = bound.control(&op(29), &[0, 0, 0, 0]);
    assert_eq!(real.len(), 8, "GETLADDRS: [1, family, port_hi, port_lo, a, b, c, d]");
    assert_eq!(real[0], INET_REP_OK);
    assert_eq!(
        u16::from_be_bytes([real[2], real[3]]),
        port,
        "GETLADDRS reports the socket's own bound port, not the fake one"
    );
    assert_eq!(&real[4..8], &[127, 0, 0, 1]);

    // An opened port has the cached family, so the ANY and LOOPBACK short forms now decode.
    let mut opened = opened_port();
    assert_eq!(opened.control(&op(19), &[INET_AF_ANY, 4, 210]), ctl_ok());
    assert_eq!(
        opened.control(&op(5), &[]),
        vec![INET_REP_OK, INET_AF_INET, 4, 210, 0, 0, 0, 0],
        "the C's inet_set_faddress ANY form"
    );
    assert_eq!(opened.control(&op(19), &[INET_AF_LOOPBACK, 4, 210]), ctl_ok());
    assert_eq!(
        opened.control(&op(5), &[]),
        vec![INET_REP_OK, INET_AF_INET, 4, 210, 127, 0, 0, 1],
        "the LOOPBACK form"
    );
    assert_eq!(opened.control(&op(19), &[]), ctl_ok(), "clear the fake name");

    // The address-decode faults, in the C's inet_set_faddress spelling.
    assert_eq!(
        opened.control(&op(19), &[9, 1, 2]),
        ctl_xerror("eafnosupport"),
        "family tag 9"
    );
    assert_eq!(
        opened.control(&op(20), &[9, 1, 2]),
        ctl_xerror("eafnosupport"),
        "family tag 9"
    );
    assert_eq!(
        opened.control(&op(19), &[INET_AF_INET, 1, 2]),
        ctl_xerror("einval"),
        "a truncated AF_INET record"
    );
}

/// The `getifaddrs(3)`-backed family (`:11067`, `:11089`, `:11100`, `:11078`): each request is
/// `!IS_OPEN ⇒ ctl_xerror(EXBADPORT)` before anything else, which a socket-less port reproduces
/// exactly (measured for all four).
#[test]
fn numeric_getif_requests_refuse_a_socket_less_port() {
    for (code, arg) in [
        (21u32, vec![]),          // GETIFLIST
        (22, vec![1]),            // IFGET
        (23, vec![1]),            // IFSET
        (25, vec![]),             // GETIFADDRS
    ] {
        let mut bare = bare_port();
        assert_eq!(
            bare.control(&op(code), &arg),
            ctl_xerror("exbadport"),
            "request {code} on a socket-less port"
        );
    }
}
/// `INET_REQ_IGNOREFD`(28, `:11456`): `!IS_CONNECTED ⇒ ENOTCONN` (`:11462`), then
/// `stype != SOCK_STREAM ⇒ EINVAL` (`:11464`). A connected socket on this driver is always a
/// datagram one, so the second guard is the terminal answer (measured `[0]einval`); the C then
/// reads `*buf` with **no** length check at all (`:11465`), which is stream-only work.
#[test]
fn numeric_ignorefd_needs_a_connection_then_a_stream() {
    let mut bare = bare_port();
    assert_eq!(
        bare.control(&op(28), &[1]),
        ctl_xerror("enotconn"),
        "the connection guard comes first"
    );
    let mut opened = opened_port();
    assert_eq!(opened.control(&op(28), &[1]), ctl_xerror("enotconn"));
    let (mut connected, _) = connected_port();
    assert_eq!(
        connected.control(&op(28), &[1]),
        ctl_xerror("einval"),
        "a datagram socket is not a stream"
    );
}

/// `INET_REQ_GETLADDRS`(29, `:11324`)/`INET_REQ_GETPADDRS`(30, `:11236`): `len != 4 ⇒ EINVAL`,
/// then `!IS_OPEN ⇒ EXBADPORT`, then the **non-SCTP** fallback — `reply_inet_addrs(1, …)` (`:5013`)
/// over `sock_name`/`sock_peer`, i.e. byte-identical to the `NAME`/`PEER` replies.
#[test]
fn numeric_getladdrs_and_getpaddrs_refuse_before_their_fallback() {
    let mut bare = bare_port();
    for code in [29u32, 30] {
        assert_eq!(
            bare.control(&op(code), &[]),
            ctl_xerror("einval"),
            "request {code} requires the four-byte operand"
        );
        assert_eq!(
            bare.control(&op(code), &[0, 0, 0, 0]),
            ctl_xerror("exbadport"),
            "request {code} needs a socket"
        );
    }

    let mut opened = opened_port();
    assert_eq!(
        opened.control(&op(29), &[0, 0, 0, 0]),
        vec![INET_REP_OK, INET_AF_INET, 0, 0, 0, 0, 0, 0],
        "an unbound socket reports the all-zero wildcard address"
    );
    assert_eq!(
        opened.control(&op(30), &[0, 0, 0, 0]),
        ctl_xerror("enotconn"),
        "GETPADDRS inherits the PEER guard"
    );

    let (mut bound, port) = bound_port();
    let local = bound.control(&op(29), &[0, 0, 0, 0]);
    assert_eq!(local[0], INET_REP_OK);
    assert_eq!(u16::from_be_bytes([local[2], local[3]]), port);
    assert_eq!(&local[4..8], &[127, 0, 0, 1]);
    assert_eq!(
        bound.control(&op(30), &[0, 0, 0, 0]),
        ctl_xerror("enotconn"),
        "binding does not connect"
    );
}

// -----------------------------------------------------------------------
// The two routes cannot drift
// -----------------------------------------------------------------------

/// The named and the numeric route reach the same verbs, so their replies must be identical —
/// except `close`, whose name the registry reserves ([`crate::PortControl::CLOSE`]).
#[test]
fn the_named_and_the_numeric_route_reply_identically() {
    let mut named = bare_port();
    let mut numeric = bare_port();

    assert_eq!(
        named.control("open", &open_arg()),
        numeric.control(&op(1), &open_arg()),
        "open vs INET_REQ_OPEN"
    );

    let named_bind = named.control("bind", &[INET_AF_ANY, 0, 0]);
    let numeric_bind = numeric.control(&op(6), &[INET_AF_ANY, 0, 0]);
    assert_eq!(named_bind.len(), numeric_bind.len(), "bind reply length");
    assert_eq!(named_bind[0], numeric_bind[0], "bind result tag");

    let mut record = vec![INET_OPT_BROADCAST];
    record.extend_from_slice(&1i32.to_be_bytes());
    assert_eq!(
        named.control("setopts", &record),
        numeric.control(&op(7), &record),
        "setopts vs INET_REQ_SETOPTS"
    );
    assert_eq!(
        named.control("getopts", &[INET_OPT_BROADCAST]),
        numeric.control(&op(8), &[INET_OPT_BROADCAST]),
        "getopts vs INET_REQ_GETOPTS"
    );
    // The named `recv` takes no operand; the numeric `PACKET_REQ_RECV` carries the C's
    // `Timeout(4), Length(4)`. With nothing pending both reach the same read-now.
    assert_eq!(
        named.control("recv", &[]),
        numeric.control(&op(60), &[0u8; 8]),
        "recv vs PACKET_REQ_RECV"
    );

    // A command that is not a decimal integer still takes the named path, where an unknown verb
    // is the driver's own `badarg`.
    assert!(matches!(
        numeric.try_control("frobnicate", &[]),
        Err(BeamError::BadArg)
    ));

    // The registry reserves the named `close` (it closes the port and answers no bytes); the
    // numeric `INET_REQ_CLOSE` reaches the driver's own verb and answers its `[1]`.
    assert_eq!(named.control("close", &[]), Vec::<u8>::new());
    assert!(matches!(
        named.try_control("getopts", &[INET_OPT_BROADCAST]),
        Err(BeamError::BadArg)
    ));
    assert_eq!(numeric.control(&op(2), &[]), ctl_ok());
}

// -----------------------------------------------------------------------
// The `erl` oracle matrix (skips loudly when `erl` is absent)
// -----------------------------------------------------------------------

/// The `(opcode, operand)` list the oracle matrix drives, in one port's life: the bare-port
/// refusals and fakes, the datagram lifecycle, the connect/peer half, then the close that leaves
/// the cached socket kind behind.
fn oracle_matrix() -> Vec<Call> {
    let fake = vec![INET_AF_INET, 4, 210, 1, 2, 3, 4];
    vec![
        // ---- the bare (socket-less) port: the shared queries ----
        Call::req(15, vec![], Rule::Exact),             // GETTYPE   -> einval
        Call::req(14, vec![], Rule::Exact),             // GETFD     -> einval
        Call::req(16, vec![], Rule::Exact),             // GETSTATUS -> 0
        Call::req(5, vec![], Rule::Exact),              // NAME      -> ebadf
        Call::req(4, vec![], Rule::Exact),              // PEER      -> enotconn
        Call::req(60, vec![0u8; 8], Rule::Exact),       // PACKET_REQ_RECV without a socket
        Call::req(60, vec![], Rule::Exact),             // ... and without an operand
        Call::req(42, vec![], Rule::Exact),             // a TCP-only case
        Call::req(26, vec![], Rule::Exact),             // a TCP-only case
        // ---- the bare port: the C's own refusals ----
        Call::req(27, vec![], Rule::Exact),              // LISTEN    -> exbadport (SCTP only)
        Call::req(27, vec![0, 5], Rule::Exact),          // ... any operand
        Call::req(6, vec![], Rule::Exact),               // BIND      -> einval (length first)
        Call::req(6, vec![INET_AF_ANY, 0, 0], Rule::Exact), // ... then exbadport
        Call::req(3, vec![0, 0], Rule::Exact),           // CONNECT   -> exbadport (state first)
        Call::req(3, vec![], Rule::Exact),               // ... empty form too
        Call::req(1, vec![1, 2], Rule::Exact),           // OPEN      -> einval (needs 3 bytes)
        Call::req(13, vec![], Rule::Exact),              // FDOPEN    -> einval
        Call::req(13, vec![9, 2, 0, 0, 0, 1], Rule::Exact), // ... eafnosupport
        Call::req(13, vec![1, 9, 0, 0, 0, 1], Rule::Exact), // ... einval (bad type)
        Call::req(12, vec![], Rule::Exact),              // GETHOSTNAME -> [1] ++ host
        Call::req(12, vec![1], Rule::Exact),             // ... einval
        Call::req(17, vec![], Rule::Exact),              // GETSERVBYNAME -> einval
        Call::req(18, vec![], Rule::Exact),              // GETSERVBYPORT -> einval
        Call::req(19, vec![], Rule::Exact),              // SETNAME   -> [1] (clear)
        Call::req(20, vec![], Rule::Exact),              // SETPEER   -> [1] (clear)
        Call::req(19, vec![INET_AF_INET], Rule::Exact),  // ... einval (one byte)
        Call::req(20, vec![INET_AF_INET], Rule::Exact),  // ... einval (one byte)
        Call::req(19, vec![9, 1, 2], Rule::Exact),       // ... eafnosupport
        // `SUBSCRIBE`: an outq of 0 between port-task rounds, one record per operand byte. The
        // `badarg` operands ride the guarded probe above — a raise would abort this expression.
        Call::req(24, vec![], Rule::Exact),              // SUBSCRIBE -> [1]
        Call::req(24, vec![1], Rule::Exact),             // SUBSCRIBE one record
        Call::req(24, vec![1, 1], Rule::Exact),          // SUBSCRIBE two records        Call::req(21, vec![], Rule::Exact),              // GETIFLIST  -> exbadport
        Call::req(22, vec![1], Rule::Exact),             // IFGET      -> exbadport
        Call::req(23, vec![1], Rule::Exact),             // IFSET      -> exbadport
        Call::req(25, vec![], Rule::Exact),              // GETIFADDRS -> exbadport
        Call::req(28, vec![1], Rule::Exact),             // IGNOREFD   -> enotconn
        Call::req(29, vec![0, 0, 0, 0], Rule::Exact),    // GETLADDRS  -> exbadport
        Call::req(29, vec![], Rule::Exact),              // ... einval
        Call::req(30, vec![0, 0, 0, 0], Rule::Exact),    // GETPADDRS  -> exbadport
        Call::req(30, vec![], Rule::Exact),              // ... einval
        // ---- the bare port: the fake addresses SETNAME/SETPEER install ----
        Call::req(19, fake.clone(), Rule::Exact),        // SETNAME   -> [1]
        Call::req(5, vec![], Rule::Exact),               // NAME      -> the fake address
        Call::req(19, vec![INET_AF_ANY, 4, 210], Rule::Exact), // the ANY short form: einval
        Call::req(19, vec![INET_AF_LOOPBACK, 4, 210], Rule::Exact), // the LOOPBACK one too
        Call::req(19, vec![], Rule::Exact),              // clear it again
        Call::req(5, vec![], Rule::Exact),               // NAME      -> ebadf
        Call::raw(1, open_arg().to_vec(), Rule::Exact),  // a wrapped opcode
        Call::raw(99, vec![], Rule::Exact),              // a retired opcode
        // ---- the datagram lifecycle ----
        Call::req(1, open_arg().to_vec(), Rule::Exact), // OPEN
        Call::req(19, vec![INET_AF_ANY, 4, 210], Rule::Exact), // the ANY short form
        Call::req(5, vec![], Rule::Exact),              // NAME      -> 0.0.0.0:1234
        Call::req(19, vec![INET_AF_LOOPBACK, 4, 210], Rule::Exact), // the LOOPBACK form
        Call::req(5, vec![], Rule::Exact),              // NAME      -> 127.0.0.1:1234
        Call::req(19, vec![], Rule::Exact),             // clear it
        Call::req(5, vec![], Rule::Exact),              // NAME      -> the wildcard address
        Call::req(15, vec![], Rule::Exact),             // GETTYPE   -> datagram
        Call::req(16, vec![], Rule::Exact),             // GETSTATUS -> 1
        Call::req(27, vec![], Rule::Exact),             // LISTEN    -> exbadport (opened)
        Call::req(27, vec![0, 5], Rule::Exact),         // ... any operand
        Call::req(29, vec![0, 0, 0, 0], Rule::Exact),    // GETLADDRS -> the wildcard address
        Call::req(30, vec![0, 0, 0, 0], Rule::Exact),    // GETPADDRS -> enotconn
        Call::req(28, vec![1], Rule::Exact),            // IGNOREFD  -> enotconn
        Call::req(60, vec![0, 0], Rule::Exact),         // PACKET_REQ_RECV: einval
        Call::req(3, vec![0, 0], Rule::Exact),          // CONNECT: einval (six bytes needed)
        Call::req(1, vec![1, 2], Rule::Exact),          // OPEN: einval (three bytes needed)
        Call::req(6, loopback_arg(0), Rule::BoundPort), // BIND (ephemeral port)
        Call::req(5, vec![], Rule::PackedAddress),      // NAME      -> the bound address
        Call::req(29, vec![0, 0, 0, 0], Rule::PackedAddress), // GETLADDRS -> the same address
        Call::req(30, vec![0, 0, 0, 0], Rule::Exact),   // GETPADDRS -> still enotconn
        Call::req(7, vec![INET_OPT_BROADCAST, 0, 0, 0, 1], Rule::Exact), // SETOPTS
        Call::req(8, vec![INET_OPT_BROADCAST], Rule::Exact), // GETOPTS
        Call::req(11, vec![], Rule::Exact),             // GETSTAT   -> [1]
        Call::req(4, vec![], Rule::Exact),              // PEER      -> enotconn
        Call::req(60, vec![0, 0, 0, 0], Rule::Exact),   // a wrong-length PACKET_REQ_RECV
        // ---- the connected half: the real peer, the fake one, and the stream-only gap ----
        Call::req(3, connect_arg(9), Rule::Exact),      // CONNECT   -> [1]
        Call::req(16, vec![], Rule::Exact),             // GETSTATUS -> 5 (connected)
        Call::req(4, vec![], Rule::Exact),              // PEER      -> the real peer
        Call::req(30, vec![0, 0, 0, 0], Rule::Exact),   // GETPADDRS -> the real peer
        Call::req(28, vec![1], Rule::Exact),            // IGNOREFD  -> einval (not a stream)
        Call::req(20, fake.clone(), Rule::Exact),       // SETPEER   -> [1]
        Call::req(4, vec![], Rule::Exact),              // PEER      -> the fake peer
        Call::req(30, vec![0, 0, 0, 0], Rule::Exact),   // GETPADDRS -> the real one
        Call::req(20, vec![], Rule::Exact),             // clear the fake peer
        Call::req(4, vec![], Rule::Exact),              // PEER      -> the real peer again
        Call::req(3, vec![], Rule::Exact),              // CONNECT   -> dissolve
        Call::req(16, vec![], Rule::Exact),             // GETSTATUS -> 1
        // ---- the close leaves the cached socket kind behind ----
        Call::req(2, vec![], Rule::Exact),              // CLOSE     -> [1]
        Call::req(15, vec![], Rule::Exact),             // GETTYPE   -> still datagram
        Call::req(16, vec![], Rule::Exact),             // GETSTATUS -> 0
        Call::req(14, vec![], Rule::Exact),             // GETFD     -> einval
        Call::req(5, vec![], Rule::Exact),              // NAME      -> ebadf
        Call::req(2, vec![], Rule::Exact),              // CLOSE is idempotent
    ]
}

/// The **compatibility gate** for the datagram numeric bridge: the same call list driven against
/// the real system `udp_inet` port and against this crate's port over `port_control`.
///
/// The matrix carries only rows both sides can reproduce **byte for byte**. Three families are
/// deliberately left to the non-oracle tests above: the well-formed `PACKET_REQ_RECV` and the
/// `subscribe`/`fdopen`/`getservby*`/`getif*` bodies (this floor's synchronous read-now
/// substitution, and the typed capability gaps — a gap raises rather than replying, so it cannot
/// ride a `port_control` matrix at all), and the `getiflist` reply (a host-dependent
/// `getifaddrs(3)` interface table). Everything else — including the post-close `GETTYPE` row and
/// the `{eol}`-free refusal rows — is compared exactly, with the ephemeral-port/descriptor rows
/// compared shape-wise by [`Rule`].
///
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn the_numeric_replies_agree_with_the_erl_oracle() {
    let calls = oracle_matrix();
    let Some(dump) = erl_stdout(&oracle_expression("udp_inet", &calls)) else {
        eprintln!("SKIP: no `erl` on PATH — skipping the udp_inet numeric-opcode oracle matrix");
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
