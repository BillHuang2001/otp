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
    assert_agrees, erl_stdout, loopback_arg, op, oracle_expression, oracle_replies, reason_of, Call,
    Rig, Rule, CONTROL_MAGIC,
};
use super::opt::{INET_OPT_BROADCAST, INET_OPT_REUSEADDR};
use super::wire::{ctl_ok, ctl_xerror};
use super::{
    UdpInetDriver, INET_AF_ANY, INET_AF_INET, INET_PROTO_DEFAULT, INET_REP_OK, INET_TYPE_DGRAM,
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

/// An opcode the C's switches **do** serve but this floor does not implement is a typed gap.
///
/// Divergence from the C, reported rather than silently mirrored: `INET_REQ_LISTEN`(27) on a
/// **datagram** socket reaches `packet_inet_ctl`'s SCTP-only `case`, whose first statement is
/// `if (!IS_SCTP(desc)) return ctl_xerror(EXBADPORT)` (`inet_drv.c:14846`-`:14852`) — so the
/// oracle answers `[0]exbadport` (measured) while this floor's `control_opcode` lists `Listen` in
/// its "the C's switches DO serve these, but this floor does not" group and raises the typed gap.
/// The gap's diagnostic names `tcp_inet_ctl` because [`super::Opcode::scope`] classifies
/// `INET_REQ_LISTEN` as a `tcp_inet_ctl` case (which it is, for a *stream* port); the datagram
/// driver's own SCTP-only arm for the same number is not distinguished.
#[test]
fn an_unserved_numeric_opcode_on_the_datagram_port_is_a_typed_gap() {
    let mut port = opened_port();
    let err = port
        .try_control(&op(27), &[0, 5])
        .expect_err("INET_REQ_LISTEN is not served here");
    assert_eq!(
        reason_of(err),
        "unsupported: control opcode `listen` (tcp_inet_ctl) is not served by this driver floor"
    );

    // The name-resolution / interface requests are gaps on this driver too.
    for (code, name) in [(12u32, "gethostname"), (13, "fdopen"), (29, "getladdrs")] {
        let err = port
            .try_control(&op(code), &[])
            .expect_err("an unserved opcode is a typed gap");
        assert_eq!(
            reason_of(err),
            format!("unsupported: control opcode `{name}` (inet_ctl) is not served by this driver floor")
        );
    }
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

/// The `(opcode, operand)` list the oracle matrix drives: the bare-port queries, then the
/// datagram lifecycle, then the shared `inet_ctl` replies and the operand faults.
fn oracle_matrix() -> Vec<Call> {
    vec![
        // The bare (socket-less) port.
        Call::req(15, vec![], Rule::Exact),             // GETTYPE   -> einval
        Call::req(14, vec![], Rule::Exact),             // GETFD     -> einval
        Call::req(16, vec![], Rule::Exact),             // GETSTATUS -> 0
        Call::req(5, vec![], Rule::Exact),              // NAME      -> ebadf
        Call::req(4, vec![], Rule::Exact),              // PEER      -> enotconn
        Call::req(60, vec![0u8; 8], Rule::Exact),       // PACKET_REQ_RECV without a socket
        Call::req(42, vec![], Rule::Exact),             // a TCP-only case
        Call::req(26, vec![], Rule::Exact),             // a TCP-only case
        Call::raw(1, open_arg().to_vec(), Rule::Exact), // a wrapped opcode
        Call::raw(99, vec![], Rule::Exact),             // a retired opcode
        // The datagram lifecycle.
        Call::req(1, open_arg().to_vec(), Rule::Exact), // OPEN
        Call::req(15, vec![], Rule::Exact),             // GETTYPE   -> datagram
        Call::req(16, vec![], Rule::Exact),             // GETSTATUS -> 1
        Call::req(6, loopback_arg(0), Rule::BoundPort), // BIND (ephemeral port)
        Call::req(5, vec![], Rule::PackedAddress),      // NAME      -> the bound address
        Call::req(7, vec![INET_OPT_BROADCAST, 0, 0, 0, 1], Rule::Exact), // SETOPTS
        Call::req(8, vec![INET_OPT_BROADCAST], Rule::Exact), // GETOPTS
        Call::req(11, vec![], Rule::Exact),             // GETSTAT   -> [1]
        Call::req(4, vec![], Rule::Exact),              // PEER      -> enotconn
        Call::req(60, vec![0, 0, 0, 0], Rule::Exact),   // a wrong-length PACKET_REQ_RECV
    ]
}

/// The **compatibility gate** for the datagram numeric bridge: the same call list driven against
/// the real system `udp_inet` port and against this crate's port over `port_control`.
///
/// Two opcodes are deliberately **not** in the matrix because the C's reply cannot be reproduced
/// by a floor with no driver→owner message path or no SCTP: `PACKET_REQ_RECV` with a
/// well-formed operand (the C answers the async op id `[1] ++ async_id_be16`, this floor the
/// synchronous read-now) and `INET_REQ_LISTEN` (the C answers `[0]exbadport`, this floor the
/// typed gap). Both are pinned by the non-oracle tests above.
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
