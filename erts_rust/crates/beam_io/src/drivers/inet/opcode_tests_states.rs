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
use super::opcode_tests::{
    bare_port, connection, erl_stdout, loopback_arg, op, open_arg, opened_port, Rig,
};
use super::sys;
use super::wire::{ctl_ok, ctl_xerror};
use super::{connect_wait_ms, CONNECT_WAIT_CAP_MS, INET_INFINITY};
use super::{
    INET_AF_INET, INET_AF_LOOPBACK, INET_REP_ERROR, INET_REP_OK, INET_TYPE_STREAM,
};
use super::{TcpInetDriver, DRIVER_NAME};
use std::net::TcpListener;

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

// -----------------------------------------------------------------------
// CONNECT(3): the guard ladder and the synchronous completion
// -----------------------------------------------------------------------

/// The `INET_REQ_CONNECT` operand: `Timeout(4)`, then the packed `127.0.0.1:port` address
/// (`inet_drv.c:12064`'s `/* INPUT: Timeout(4), Port(2), Address(N) */`).
///
/// The C consumes **only** the four timeout bytes and hands the rest to `inet_set_faddress`
/// (`:12084`-`:12089`), so the `Port(2)` its comment names is the address's own port field — the
/// bytes here are `[Timeout(4)] ++ [INET_AF_LOOPBACK, port_hi, port_lo]`. Measured against the
/// oracle: sending two extra port bytes first makes the driver read them as the family tag and
/// answer `[0]eafnosupport`.
fn connect_arg(port: u16, timeout_ms: i32) -> Vec<u8> {
    let mut arg = Vec::with_capacity(7);
    arg.extend_from_slice(&timeout_ms.to_be_bytes());
    arg.extend_from_slice(&loopback_arg(port));
    arg
}

/// `INET_REQ_CONNECT`(3): the C's guard ladder (`!IS_OPEN ⇒ exbadport`, `IS_CONNECTED ⇒ eisconn`,
/// `IS_CONNECTING ⇒ einval`, then `len < 6 ⇒ einval`), and the **synchronous** completion this
/// floor substitutes for the C's async operation — a bare `[1]` and `INET_STATE_CONNECTED`
/// afterwards, or `[0] ++ errno` with the descriptor torn down when the connect fails.
///
/// Measured errno for a refused loopback connect: **`econnrefused`** (`SO_ERROR` after the listener
/// was dropped), which the assertion pins by name.
#[test]
fn connect_guards_and_the_synchronous_completion() {
    // The state guards come first, whatever the operand says.
    let mut bare = bare_port();
    assert_eq!(
        bare.control(&op(3), &connect_arg(1, INET_INFINITY)),
        ctl_xerror("exbadport"),
        "CONNECT without a socket is the C's `!IS_OPEN`"
    );
    assert_eq!(
        bare.control(&op(3), &[0, 0, 0, 0, 0, 0]),
        ctl_xerror("exbadport"),
        "and it precedes the `len < 6` check"
    );
    let mut opened = opened_port();
    assert_eq!(
        opened.control(&op(3), &[0, 0, 0, 0, 0]),
        ctl_xerror("einval"),
        "an open socket with a five-byte operand reaches `len < 6`"
    );

    // Success: `Timeout = -1` is the C's `INET_INFINITY`, i.e. the capped wait here.
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let port_no = listener.local_addr().expect("an address").port();
    let mut connecting = opened_port();
    let reply = connecting.control(&op(3), &connect_arg(port_no, INET_INFINITY));
    assert_eq!(
        reply,
        ctl_ok(),
        "a completed connect answers the bare `[1]` (prim_inet's `{{ok, []}} -> ok`)"
    );
    assert_eq!(reply.len(), 1, "exactly one byte: no async-op id");
    assert_eq!(
        connecting.control(&op(16), &[]),
        vec![INET_REP_OK, 0, 0, 0, 5],
        "INET_STATE_CONNECTED is `1|4` = 5"
    );
    assert_eq!(
        connecting.control(&op(3), &connect_arg(port_no, INET_INFINITY)),
        ctl_xerror("eisconn"),
        "a second connect on the connected socket is `IS_CONNECTED ⇒ eisconn`"
    );
    assert_eq!(
        connecting.control(&op(4), &[]).len(),
        8,
        "and PEER now reports the listener's real endpoint"
    );
    drop(listener);

    // Failure: a port nothing listens on any more answers the kernel's errno, not a `[1]`.
    let dead = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let dead_port = dead.local_addr().expect("an address").port();
    drop(dead);
    let mut refused = opened_port();
    let reply = refused.control(&op(3), &connect_arg(dead_port, INET_INFINITY));
    assert_eq!(reply.first(), Some(&INET_REP_ERROR), "the `[0]` result tag");
    let name = String::from_utf8(reply[1..].to_vec()).expect("an errno name");
    assert_eq!(name, "econnrefused", "the measured refusal errno");
    assert!(
        name.chars().all(|c| c.is_ascii_lowercase()),
        "the C renders the errno lowercase (`erl_errno_id`)"
    );
    assert_eq!(
        refused.control(&op(16), &[]),
        vec![INET_REP_OK, 0, 0, 0, 0],
        "the failed connect tore the descriptor down (`tcp_inet_connect_error`), so the port is \
         back to INET_STATE_CLOSED"
    );
}

/// The `Timeout(4)` operand's translation into a `poll(2)` budget: a negative value (the C's
/// `INET_INFINITY`) is the cap, `0` stays "poll once", and a positive one is clamped.
///
/// This is the timeout path's pin, chosen over a live case on purpose: a connect that really times
/// out needs an address that swallows SYNs, which no sandbox guarantees, and asserting a wall-clock
/// duration would make the suite load-sensitive. The arm itself is
/// `Ok(ctl_xerror("etimedout"))` with the socket left in `INET_STATE_CONNECTING` (the C would
/// deliver `{{inet_async, Port, Ref, {{error, etimedout}}}}` instead) — unreachable without that
/// unreliable peer, and documented on `verb_connect`.
#[test]
fn the_connect_wait_budget_is_clamped_to_the_cap() {
    assert_eq!(
        connect_wait_ms(INET_INFINITY),
        CONNECT_WAIT_CAP_MS,
        "`infinity` becomes the cap"
    );
    assert_eq!(
        connect_wait_ms(i32::MIN),
        CONNECT_WAIT_CAP_MS,
        "any negative operand is `INET_INFINITY`"
    );
    assert_eq!(connect_wait_ms(0), 0, "zero polls once without waiting");
    assert_eq!(connect_wait_ms(1_500), 1_500, "a positive budget is its own");
    assert_eq!(
        connect_wait_ms(CONNECT_WAIT_CAP_MS + 1),
        CONNECT_WAIT_CAP_MS,
        "and is clamped above"
    );
    assert_eq!(connect_wait_ms(i32::MAX), CONNECT_WAIT_CAP_MS);
}

// -----------------------------------------------------------------------
// LISTEN(27): the guards, and the C's *missing* already-listening guard
// -----------------------------------------------------------------------

/// `INET_REQ_LISTEN`(27) reproduces `inet_drv.c:12042`-`:12062` exactly: `CLOSED`/`!IS_OPEN` first
/// (`exbadport`), then `len != 2` (`einval`), then `listen(2)` and `INET_STATE_LISTENING`.
///
/// The C has **no** "already listening" guard, so a second `LISTEN` re-runs `listen(2)` and answers
/// `[1]` again (measured against the oracle) — pinned here so nobody "fixes" it into a refusal.
#[test]
fn listen_guards_and_is_idempotent() {
    let mut bare = bare_port();
    for arg in [vec![], vec![0u8, 5]] {
        assert_eq!(
            bare.control(&op(27), &arg),
            ctl_xerror("exbadport"),
            "the state guard precedes the operand on a socket-less port"
        );
    }

    let mut opened = opened_port();
    for arg in [Vec::new(), vec![0u8], vec![0u8, 5, 0]] {
        assert_eq!(
            opened.control(&op(27), &arg),
            ctl_xerror("einval"),
            "an open socket with {} backlog bytes is `len != 2`",
            arg.len()
        );
    }
    assert_eq!(
        opened.control(&op(26), &[0, 0, 0, 0]),
        ctl_xerror("einval"),
        "ACCEPT's combined `not listening || len != 4` guard"
    );

    let bound = opened.control(&op(6), &loopback_arg(0));
    assert_eq!(bound[0], INET_REP_OK, "INET_REQ_BIND");
    assert_eq!(
        opened.control(&op(16), &[]),
        vec![INET_REP_OK, 0, 0, 0, 1],
        "a bound socket is still only INET_STATE_OPEN"
    );
    assert_eq!(opened.control(&op(27), &[0, 5]), ctl_ok(), "INET_REQ_LISTEN");
    assert_eq!(
        opened.control(&op(16), &[]),
        vec![INET_REP_OK, 0, 0, 0, 9],
        "INET_STATE_LISTENING is `1|8` = 9"
    );
    assert_eq!(
        opened.control(&op(27), &[0, 5]),
        ctl_ok(),
        "a second LISTEN re-runs listen(2) and answers [1]: there is no already-listening guard"
    );
    assert_eq!(
        opened.control(&op(16), &[]),
        vec![INET_REP_OK, 0, 0, 0, 9],
        "and the state is unchanged"
    );
}

/// The `LISTEN`/`CONNECT` rows above, cross-checked byte-for-byte against the system `erl` driving
/// a real `{spawn_driver, "tcp_inet"}` port.
///
/// Three of the C's replies are deliberately **not** compared, because they are not deterministic
/// or not reproducible:
///
/// * the **first** `CONNECT` — the C answers `[1] ++ async_id_be16` when the kernel says
///   `EINPROGRESS` and a bare `[1]` when it completes at once (both happen on loopback), while this
///   floor always answers the bare `[1]` after completing the connect itself. The oracle's row is
///   still printed and its `[1]` tag asserted;
/// * the `{inet_async, …}` message that accompanies the async form — no driver→owner channel
///   exists here, so the oracle is instead waited on with a bounded `GETSTATUS` poll;
/// * an `ACCEPT` with nothing pending (the C answers an async op id).
///
/// Everything compared is therefore a refusal or a state report, so byte equality is meaningful.
/// The rows carry no ephemeral value: the connect address names each runtime's own listener, and
/// that port never appears in a reply.
#[test]
fn the_listen_and_connect_rows_agree_with_the_oracle() {
    let expression = format!(
        "J = fun(L, R) -> io:format(\"~s=~w|\", [L, R]) end, \
         P = open_port({{spawn_driver, \"tcp_inet\"}}, [binary]), \
         J(\"bare_listen\", erlang:port_control(P, {bare_listen}, <<0,5>>)), \
         J(\"bare_listen_len0\", erlang:port_control(P, {bare_listen}, <<>>)), \
         J(\"bare_connect\", erlang:port_control(P, {bare_connect}, <<0,0,0,0,0,0>>)), \
         J(\"bare_accept\", erlang:port_control(P, {bare_accept}, <<0,0,0,0>>)), \
         J(\"open\", erlang:port_control(P, {open}, <<1,1,0>>)), \
         J(\"open_listen_badlen\", erlang:port_control(P, {bare_listen}, <<0>>)), \
         J(\"listen\", erlang:port_control(P, {bare_listen}, <<0,5>>)), \
         J(\"listen_again\", erlang:port_control(P, {bare_listen}, <<0,5>>)), \
         J(\"listen_status\", erlang:port_control(P, {status}, <<>>)), \
         {{ok,L}} = gen_tcp:listen(0, [{{ip,{{127,0,0,1}}}}]), \
         {{ok,Pt}} = inet:port(L), \
         Q = open_port({{spawn_driver, \"tcp_inet\"}}, [binary]), \
         erlang:port_control(Q, {open}, <<1,1,0>>), \
         J(\"first_connect\", erlang:port_control(Q, {bare_connect}, \
                             <<255,255,255,255,4,Pt:16>>)), \
         S = fun F(0) -> erlang:error(the_connect_never_settled); \
                 F(N) -> case erlang:port_control(Q, {status}, <<>>) of \
                             [1,0,0,0,5] -> ok; \
                             _ -> timer:sleep(10), F(N-1) \
                         end end, \
         S(300), \
         J(\"connect_again\", erlang:port_control(Q, {bare_connect}, \
                             <<255,255,255,255,4,Pt:16>>)), \
         J(\"connect_status\", erlang:port_control(Q, {status}, <<>>)), \
         erlang:port_close(Q), erlang:port_close(P), gen_tcp:close(L)",
        bare_listen = op(27),
        bare_connect = op(3),
        bare_accept = op(26),
        open = op(1),
        status = op(16),
    );
    let Some(dump) = erl_stdout(&expression) else {
        eprintln!("SKIP: no `erl` oracle available for the LISTEN/CONNECT rows");
        return;
    };
    let oracle: Vec<(String, Vec<u8>)> = dump
        .split('|')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (label, reply) = part.split_once('=').expect("a `label=reply` row");
            let inner = reply.trim_start_matches('[').trim_end_matches(']');
            let bytes = if inner.is_empty() {
                Vec::new()
            } else {
                inner
                    .split(',')
                    .map(|number| number.trim().parse::<u8>().expect("a reply byte"))
                    .collect()
            };
            (label.to_string(), bytes)
        })
        .collect();
    let row = |label: &str| -> Vec<u8> {
        oracle
            .iter()
            .find(|(name, _)| name == label)
            .unwrap_or_else(|| panic!("the oracle printed no `{label}` row: {dump}"))
            .1
            .clone()
    };

    // The Rust side, at the same state points on one port.
    let mut port = bare_port();
    assert_eq!(
        port.control(&op(27), &[0, 5]),
        row("bare_listen"),
        "LISTEN without a socket"
    );
    assert_eq!(
        port.control(&op(27), &[]),
        row("bare_listen_len0"),
        "LISTEN len0, state guard first"
    );
    assert_eq!(
        port.control(&op(3), &[0, 0, 0, 0, 0, 0]),
        row("bare_connect"),
        "CONNECT without a socket"
    );
    assert_eq!(
        port.control(&op(26), &[0, 0, 0, 0]),
        row("bare_accept"),
        "ACCEPT without a socket"
    );
    assert_eq!(port.control(&op(1), &open_arg()), row("open"), "INET_REQ_OPEN");
    assert_eq!(
        port.control(&op(27), &[0]),
        row("open_listen_badlen"),
        "LISTEN with a socket but a one-byte backlog"
    );
    let bound = port.control(&op(6), &loopback_arg(0));
    assert_eq!(bound[0], INET_REP_OK, "INET_REQ_BIND");
    assert_eq!(port.control(&op(27), &[0, 5]), row("listen"), "first LISTEN");
    assert_eq!(
        port.control(&op(27), &[0, 5]),
        row("listen_again"),
        "a second LISTEN is not refused"
    );
    assert_eq!(
        port.control(&op(16), &[]),
        row("listen_status"),
        "GETSTATUS after listening"
    );

    // The connected half: this floor's first CONNECT answers a bare `[1]` (no async id), so only
    // the post-connect state and the second CONNECT are compared.
    assert_eq!(
        row("first_connect").first(),
        Some(&INET_REP_OK),
        "the oracle's first CONNECT succeeded (its async-id tail is not compared)"
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let port_no = listener.local_addr().expect("an address").port();
    let mut connecting = opened_port();
    assert_eq!(
        connecting.control(&op(3), &connect_arg(port_no, INET_INFINITY)),
        ctl_ok(),
        "the synchronous completion answers [1] where the C answers [1] ++ async_id"
    );
    assert_eq!(
        connecting.control(&op(3), &connect_arg(port_no, INET_INFINITY)),
        row("connect_again"),
        "the second CONNECT on a connected socket"
    );
    assert_eq!(
        connecting.control(&op(16), &[]),
        row("connect_status"),
        "GETSTATUS on the connected socket"
    );
}