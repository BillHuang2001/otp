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

//! The `tcp_inet` numeric bridge's **operand × state** suite — the other half of
//! [`opcode_tests`](super::opcode_tests).
//!
//! That file pins the *lifecycle* verbs and runs the `erl`-oracle matrix; this one pins the two
//! things the C's guard order makes easy to get wrong, for every arm that has both a state guard
//! and an operand-length guard:
//!
//! * **which refusal wins** — e.g. `TCP_REQ_RECV`'s connection guard precedes its `len != 8` check
//!   (`inet_drv.c:12268`-`:12290`), while `INET_REQ_BIND`'s `len < 2` precedes its `!IS_OPEN`
//!   (`:11409`-`:11420`), so the same wrong operand answers `enotconn` in the one arm and
//!   `exbadport` in the other;
//! * **the promoted shared-`inet_ctl` arms** (`gethostname`, the `fdopen`/name-service length
//!   refusals, the `getifaddrs(3)` family, `ignorefd`, `getladdrs`/`getpaddrs`) and the
//!   fake-address `SETNAME`/`SETPEER` pair, whose *success* replies are exact and therefore
//!   asserted here rather than left to the oracle run.
//!
//! Everything is driven through the runtime's own `erlang:port_control/3` path
//! ([`crate::PortRegistry::port_control`]) — the decimal command `opcode + `[`CONTROL_MAGIC`]` a
//! live `beam_rt` forwards — never by calling [`crate::Driver::control`] directly.
use super::opcode_tests::{bare_port, connection, loopback_arg, op, opened_port, Rig};
use super::sys;
use super::wire::{ctl_ok, ctl_xerror};
use super::{INET_AF_INET, INET_AF_LOOPBACK, INET_REP_OK, INET_TYPE_STREAM};
use super::{TcpInetDriver, DRIVER_NAME};

// -----------------------------------------------------------------------
// The guard order: which refusal wins
// -----------------------------------------------------------------------

/// The C's guard **order** decides which refusal a wrong operand gets. Every row is one arm whose
/// two guards disagree about precedence, in the two states that tell them apart; the `erl` oracle
/// agrees with every one of them (see the matrix in [`opcode_tests`](super::opcode_tests)).
///
/// The ordering rules, straight from `inet_drv.c`:
///
/// * `TCP_REQ_RECV`(`:12268`), `TCP_REQ_UNRECV`(`:12326`), `TCP_REQ_SHUTDOWN`(`:12339`) and
///   `TCP_REQ_SENDFILE`(`:12380`, after its length) — connection first, then the operand;
/// * `INET_REQ_LISTEN`(`:12042`), `INET_REQ_CONNECT`(`:12064`), `INET_REQ_ACCEPT`(`:12144`) —
///   state first, then the operand;
/// * `INET_REQ_BIND`(`:11409`) and `INET_REQ_GETLADDRS`/`GETPADDRS`(`:11324`, `:11236`) — the
///   operand **first**, then `!IS_OPEN`;
/// * `INET_REQ_OPEN`(`:11961`) — the length alone, with the family/type/protocol bytes after it.
#[test]
fn the_state_guard_and_the_operand_guard_are_ordered_as_in_the_c() {
    let mut bare = bare_port();
    // The connection-first arms: no peer means `enotconn` whatever the operand says.
    for (code, label) in [
        (42u32, "TCP_REQ_RECV"),
        (43, "TCP_REQ_UNRECV"),
        (44, "TCP_REQ_SHUTDOWN"),
    ] {
        assert_eq!(
            bare.control(&op(code), &[]),
            ctl_xerror("enotconn"),
            "{label} on a bare port"
        );
        assert_eq!(
            bare.control(&op(code), &[1, 2]),
            ctl_xerror("enotconn"),
            "{label}: the connection guard precedes the operand"
        );
    }
    assert_eq!(
        bare.control(&op(45), &[0u8; 20]),
        ctl_xerror("enotconn"),
        "TCP_REQ_SENDFILE passes its length check first"
    );
    assert_eq!(
        bare.control(&op(45), &[]),
        ctl_xerror("einval"),
        "and fails it first too"
    );

    // The state-first arms on a bare port.
    assert_eq!(bare.control(&op(27), &[]), ctl_xerror("exbadport"), "LISTEN");
    assert_eq!(
        bare.control(&op(27), &[0, 5]),
        ctl_xerror("exbadport"),
        "LISTEN len2 on a bare port is still the state refusal"
    );
    assert_eq!(
        bare.control(&op(3), &[1, 2]),
        ctl_xerror("exbadport"),
        "CONNECT len2 on a bare port is `!IS_OPEN`, not `len < 6`"
    );
    assert_eq!(
        bare.control(&op(26), &[]),
        ctl_xerror("einval"),
        "ACCEPT len0"
    );
    assert_eq!(
        bare.control(&op(26), &[0u8; 4]),
        ctl_xerror("einval"),
        "ACCEPT: one combined `not listening || len != 4` guard"
    );

    // The operand-first arms on a bare port, and the same arms with a live socket.
    assert_eq!(bare.control(&op(6), &[]), ctl_xerror("einval"), "BIND len0");
    assert_eq!(
        bare.control(&op(6), &[INET_AF_INET, 0, 80, 127, 0, 0, 1]),
        ctl_xerror("exbadport"),
        "BIND with a well-formed address is `!IS_OPEN`"
    );
    assert_eq!(
        bare.control(&op(29), &[]),
        ctl_xerror("einval"),
        "GETLADDRS len0"
    );
    assert_eq!(
        bare.control(&op(30), &[]),
        ctl_xerror("einval"),
        "GETPADDRS len0"
    );
    assert_eq!(
        bare.control(&op(1), &[INET_AF_INET, INET_TYPE_STREAM]),
        ctl_xerror("einval"),
        "OPEN needs exactly three bytes"
    );

    let mut opened = opened_port();
    assert_eq!(
        opened.control(&op(27), &[]),
        ctl_xerror("einval"),
        "LISTEN with a socket but no backlog"
    );
    assert_eq!(
        opened.control(&op(3), &[1, 2]),
        ctl_xerror("einval"),
        "CONNECT on an open socket reaches its `len < 6` check"
    );
    assert_eq!(
        opened.control(&op(42), &[1, 2]),
        ctl_xerror("enotconn"),
        "and the opened-but-unconnected socket is still no connection for RECV"
    );
    assert_eq!(
        opened.control(&op(44), &[]),
        ctl_xerror("enotconn"),
        "nor for SHUTDOWN"
    );
}

// -----------------------------------------------------------------------
// The promoted shared-`inet_ctl` arms
// -----------------------------------------------------------------------

/// `INET_REQ_GETHOSTNAME`(12) answers the local host name with no resolver involved, and refuses a
/// non-empty operand (`inet_drv.c:11219`-`:11229`).
#[test]
fn numeric_gethostname_reports_the_local_name_and_refuses_an_operand() {
    let mut port = bare_port();
    let mut expected = ctl_ok();
    expected.extend_from_slice(&sys::hostname().expect("the kernel reports a host name"));
    assert_eq!(port.control(&op(12), &[]), expected, "INET_REQ_GETHOSTNAME");
    assert_eq!(
        port.control(&op(12), &[1]),
        ctl_xerror("einval"),
        "`if (len != 0) return ctl_error(EINVAL)`"
    );
}

/// `INET_REQ_FDOPEN`(13) refuses a wrong length, a foreign family and a foreign type exactly as the
/// C does (`inet_drv.c:12003`-`:12040`); adopting the caller's descriptor is the typed gap.
#[test]
fn numeric_fdopen_refuses_its_operand_exactly() {
    let mut port = bare_port();
    for arg in [vec![], vec![0u8; 4], vec![0u8; 8]] {
        assert_eq!(
            port.control(&op(13), &arg),
            ctl_xerror("einval"),
            "FDOPEN accepts six or ten bytes, not {}",
            arg.len()
        );
    }
    assert_eq!(
        port.control(&op(13), &[9, INET_TYPE_STREAM, 0, 0, 0, 0]),
        ctl_xerror("eafnosupport"),
        "`default: return str_eafnosupport` in `inet_set_address`"
    );
    assert_eq!(
        port.control(&op(13), &[INET_AF_INET, 9, 0, 0, 0, 0]),
        ctl_xerror("einval"),
        "only SOCK_STREAM can be adopted"
    );
}

/// The name-service lookups (`INET_REQ_GETSERVBYNAME`(17), `INET_REQ_GETSERVBYPORT`(18)) serve only
/// their operand-length refusals; the lookup itself stays a typed gap
/// (`inet_drv.c:11493`-`:11540`).
#[test]
fn numeric_name_service_lookups_refuse_their_operand_exactly() {
    let mut port = bare_port();
    assert_eq!(
        port.control(&op(17), &[]),
        ctl_xerror("einval"),
        "GETSERVBYNAME needs at least the two protocol bytes"
    );
    assert_eq!(
        port.control(&op(18), &[]),
        ctl_xerror("einval"),
        "GETSERVBYPORT needs at least the three port/protocol bytes"
    );
}

/// The `getifaddrs(3)` family (`21`/`22`/`23`/`25`) has **no** operand-length guard at all — only
/// `!IS_OPEN ⇒ exbadport` (`inet_drv.c:11067`-`:11107`) — so a bare port answers that for every
/// operand and the open state is the typed gap.
#[test]
fn numeric_interface_requests_are_exbadport_without_a_socket() {
    let mut bare = bare_port();
    for (code, name, arg) in [
        (21u32, "GETIFLIST", Vec::new()),
        (22, "IFGET", vec![1]),
        (23, "IFSET", vec![1, 0, 0, 0, 0]),
        (25, "GETIFADDRS", Vec::new()),
    ] {
        assert_eq!(
            bare.control(&op(code), &arg),
            ctl_xerror("exbadport"),
            "{name} needs a live socket"
        );
    }
}

/// `INET_REQ_IGNOREFD`(28) is guarded on the **connection** first (`:11456`), so every
/// unconnected state — bare and opened alike — answers `enotconn`; silencing a connected stream's
/// readiness stays the typed gap.
#[test]
fn numeric_ignorefd_is_enotconn_until_the_socket_is_connected() {
    let mut bare = bare_port();
    let mut opened = opened_port();
    for suppress in [[1u8], [0u8]] {
        assert_eq!(
            bare.control(&op(28), &suppress),
            ctl_xerror("enotconn"),
            "a bare port is not connected"
        );
        assert_eq!(
            opened.control(&op(28), &suppress),
            ctl_xerror("enotconn"),
            "nor is an opened-but-unconnected one"
        );
    }
}

/// `INET_REQ_GETLADDRS`(29)/`INET_REQ_GETPADDRS`(30) are the SCTP multi-homing requests; their
/// non-SCTP fallback is byte-identical to the `NAME`/`PEER` reply made straight from the **kernel**
/// (`sock_name`/`sock_peer`), never from a `SETNAME`/`SETPEER` fake address
/// (`inet_drv.c:11324`-`:11350`, `:11236`-`:11265`).
#[test]
fn numeric_getladdrs_and_getpaddrs_fall_back_to_the_socket() {
    let mut bare = bare_port();
    assert_eq!(
        bare.control(&op(29), &[0u8; 4]),
        ctl_xerror("exbadport"),
        "GETLADDRS len4 without a socket"
    );
    assert_eq!(
        bare.control(&op(30), &[0u8; 4]),
        ctl_xerror("exbadport"),
        "GETPADDRS len4 without a socket"
    );

    let mut opened = opened_port();
    let name = opened.control(&op(5), &[]);
    assert_eq!(
        name,
        vec![INET_REP_OK, INET_AF_INET, 0, 0, 0, 0, 0, 0],
        "a fresh unbound socket reports 0.0.0.0:0"
    );
    assert_eq!(
        opened.control(&op(29), &[0u8; 4]),
        name,
        "GETLADDRS is the NAME reply"
    );
    assert_eq!(
        opened.control(&op(30), &[0u8; 4]),
        ctl_xerror("enotconn"),
        "GETPADDRS is the PEER reply, guarded on the connection"
    );

    // A fake name does not leak into `GETLADDRS`.
    assert_eq!(
        opened.control(&op(19), &[INET_AF_INET, 0, 80, 1, 2, 3, 4]),
        ctl_ok(),
        "SETNAME stores"
    );
    assert_eq!(
        opened.control(&op(5), &[]),
        vec![INET_REP_OK, INET_AF_INET, 0, 80, 1, 2, 3, 4],
        "NAME reports the fake"
    );
    assert_eq!(
        opened.control(&op(29), &[0u8; 4]),
        name,
        "GETLADDRS still asks the kernel"
    );
}

/// `TCP_REQ_UNRECV`(43) pushes its operand back to the front of the driver's input, and the next
/// `recv` delivers those bytes **before** anything the socket holds (`tcp_push_buffer`,
/// `inet_drv.c:12326`-`:12338`).
#[test]
fn numeric_tcp_unrecv_pushes_the_bytes_back_in_front() {
    // The `Connection` is held for the whole test: dropping it would close the far end and the
    // port's next read would report the peer's close instead of the pushed bytes.
    let mut conn = connection();
    assert_eq!(
        conn.port.control(&op(43), b"abc"),
        ctl_ok(),
        "TCP_REQ_UNRECV answers [1]"
    );
    assert_eq!(
        conn.port.control(&op(42), &[0u8; 8]),
        ctl_ok(),
        "the synchronous recv reads what was pushed"
    );
    assert_eq!(
        conn.port.take_input(8),
        b"abc",
        "the pushed bytes came out first"
    );
    assert_eq!(conn.port.input_len(), 0, "and the queue is drained again");
}

// -----------------------------------------------------------------------
// The fake address: SETNAME(19) / SETPEER(20)
// -----------------------------------------------------------------------

/// `INET_REQ_SETNAME`(19) installs a fake local endpoint that a later `INET_REQ_NAME` reports in
/// place of `getsockname(2)` (`inet_drv.c:11386`-`:11408`, `:11351`-`:11377`).
///
/// The empty operand clears it, a one-byte one is the C's `einval`, and the address grammar is
/// `inet_set_faddress`'s — including the fact that its `INET_AF_ANY`/`INET_AF_LOOPBACK` short form
/// switches on the **descriptor's** cached family, so it needs a socket while the explicit
/// `INET_AF_INET` form does not (`:4811`-`:4855`).
#[test]
fn numeric_setname_installs_a_fake_local_address() {
    let mut port = bare_port();
    assert_eq!(
        port.control(&op(5), &[]),
        ctl_xerror("ebadf"),
        "no descriptor, so `getsockname(2)` fails"
    );
    assert_eq!(
        port.control(&op(19), &[1]),
        ctl_xerror("einval"),
        "a one-byte operand is `len < 2`"
    );
    assert_eq!(
        port.control(&op(19), &[INET_AF_INET, 0, 80, 1, 2]),
        ctl_xerror("einval"),
        "the explicit form needs the six bytes behind its tag"
    );
    assert_eq!(
        port.control(&op(19), &[9, 0, 80]),
        ctl_xerror("eafnosupport"),
        "an unknown family tag is `str_eafnosupport`"
    );
    assert_eq!(
        port.control(&op(19), &[INET_AF_LOOPBACK, 0, 80]),
        ctl_xerror("einval"),
        "the short form switches on the cached family, which is unset before `open`"
    );
    assert_eq!(
        port.control(&op(19), &[INET_AF_INET, 0, 80, 1, 2, 3, 4]),
        ctl_ok(),
        "SETNAME stores the fake address"
    );
    assert_eq!(
        port.control(&op(5), &[]),
        vec![INET_REP_OK, INET_AF_INET, 0, 80, 1, 2, 3, 4],
        "NAME reports it with no socket at all"
    );
    assert_eq!(
        port.control(&op(19), &[]),
        ctl_ok(),
        "the empty operand clears it"
    );
    assert_eq!(
        port.control(&op(5), &[]),
        ctl_xerror("ebadf"),
        "and NAME falls back to the descriptor"
    );

    // With a socket the short form works, and a close does **not** clear the fake.
    let mut opened = opened_port();
    assert_eq!(
        opened.control(&op(19), &[INET_AF_LOOPBACK, 0, 80]),
        ctl_ok(),
        "the cached family is AF_INET now"
    );
    assert_eq!(
        opened.control(&op(5), &[]),
        vec![INET_REP_OK, INET_AF_INET, 0, 80, 127, 0, 0, 1],
        "INET_AF_LOOPBACK expands to 127.0.0.1"
    );
    assert_eq!(opened.control(&op(2), &[]), ctl_ok(), "INET_REQ_CLOSE");
    assert_eq!(
        opened.control(&op(5), &[]),
        vec![INET_REP_OK, INET_AF_INET, 0, 80, 127, 0, 0, 1],
        "`name_ptr` outlives the descriptor, exactly as `desc->name_ptr` does"
    );
}

/// `INET_REQ_SETPEER`(20) installs a fake peer endpoint, reported by `INET_REQ_PEER` — but only
/// **after** that request's `INET_F_ACTIVE` guard, so a fake peer on an unconnected port is still
/// `enotconn` (`inet_drv.c:11301`-`:11323`, `:11266`-`:11299`).
#[test]
fn numeric_setpeer_installs_a_fake_peer_address_behind_the_connection_guard() {
    let mut bare = bare_port();
    assert_eq!(
        bare.control(&op(20), &[1]),
        ctl_xerror("einval"),
        "a one-byte operand is `len < 2`"
    );
    assert_eq!(
        bare.control(&op(20), &[INET_AF_INET, 0, 81, 127, 0, 0, 1]),
        ctl_ok(),
        "SETPEER stores"
    );
    assert_eq!(
        bare.control(&op(4), &[]),
        ctl_xerror("enotconn"),
        "the connection guard precedes the fake pointer"
    );
    assert_eq!(bare.control(&op(20), &[]), ctl_ok(), "and it clears");

    let mut conn = connection();
    let real = conn.port.control(&op(4), &[]);
    assert_eq!(real.len(), 8, "the real peer is [1, family, hi, lo, a, b, c, d]");
    assert_eq!(
        &real[4..8],
        &[127, 0, 0, 1],
        "{DRIVER_NAME} reports the real loopback peer before any fake"
    );
    assert_eq!(
        conn.port.control(&op(20), &[INET_AF_INET, 0, 81, 10, 0, 0, 7]),
        ctl_ok(),
        "SETPEER stores the fake peer"
    );
    assert_eq!(
        conn.port.control(&op(4), &[]),
        vec![INET_REP_OK, INET_AF_INET, 0, 81, 10, 0, 0, 7],
        "and PEER reports it once the connection guard passes"
    );
}

/// The fake addresses live on the **driver**, so a fresh one starts out without either — the C's
/// `inet_init_desc` sets both pointers to `NULL` (`inet_drv.c:10942`-`:10943`) — and one installed
/// on another port is invisible here.
#[test]
fn a_fresh_driver_has_no_fake_address() {
    let mut port = Rig::new(Box::new(TcpInetDriver::new()));
    assert_eq!(
        port.control(&op(5), &[]),
        ctl_xerror("ebadf"),
        "NAME asks the (absent) descriptor"
    );
    assert_eq!(
        port.control(&op(4), &[]),
        ctl_xerror("enotconn"),
        "PEER has no peer to report"
    );
    // A bare port's `BIND` still answers its `!IS_OPEN` refusal through the shared packed address
    // helper, i.e. nothing about the fake-address work changed that arm.
    assert_eq!(
        port.control(&op(6), &loopback_arg(0)),
        ctl_xerror("exbadport")
    );
}
