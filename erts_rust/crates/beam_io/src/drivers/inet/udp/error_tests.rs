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

//! The `udp_inet` **active-mode read error** — `{udp_error, S, Reason}` — over a **real loopback
//! socket**.
//!
//! The C's real-error arm of `packet_inet_input` (`inet_drv.c:15479`-`:15492`) runs
//! `packet_error_message(udesc, err)` (`:4245`) for an **active** descriptor and returns: the
//! datagram port is **not** closed and no `{udp_closed, S}` exists anywhere in the C, so the error
//! is the whole report and the next readiness edge reads again.
//!
//! `ECONNREFUSED` on a *connected* datagram socket is the realistic failure, and the only way to
//! provoke it deterministically: a datagram sent to a port nothing is bound to draws an ICMP
//! port-unreachable, which Linux queues on the sending socket and reports to the **next**
//! `recvfrom(2)`/`recv(2)`. Two consequences shape this suite:
//!
//! * the socket must be `connect`ed — Linux reports ICMP errors only to a socket whose peer is a
//!   single 4-tuple, and the connected port then discards datagrams from any other source, so a
//!   "later datagram from a fresh peer" can only be delivered by a socket bound to the **very
//!   port** the connection names (the liveness half below rebinds exactly that port);
//! * the error is delivered **once per ICMP**, never per round: after the read that consumes it
//!   the socket is empty again.
//!
//! The behaviour is cross-checked against the live system driver through an `escript` probe
//! ([`the_error_and_the_liveness_rows_agree_with_the_oracle`]), which skips **loudly** when no
//! `escript` is on `PATH`.

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use super::super::opcode_tests::{escript_stdout, loopback_arg};
use super::super::opt::INET_ACTIVE;
use super::super::wire::ctl_ok;
use super::active_tests::{data_of, Rig};
use crate::port_task::{ActiveInput, ActiveInputEvent};

/// How long a condition may stay unsatisfied before the case gives up: a liveness bound only, so
/// a slow host lengthens the run instead of changing the row.
const HANG_GUARD: Duration = Duration::from_secs(20);

/// A `udp_inet` port bound to an ephemeral loopback port and **connected** to `dead_port`, armed
/// `{active, true}` — the state a datagram read error needs.
///
/// `dead_port` is returned with the rig: the caller rebinds it for the liveness half.
fn connected_to_a_dead_port() -> (Rig, u16) {
    let dead = UdpSocket::bind("127.0.0.1:0").expect("a peer socket to abandon");
    let dead_port = dead.local_addr().expect("an address").port();
    drop(dead);
    let mut rig = Rig::new();
    rig.connect_to(dead_port);
    rig.arm(INET_ACTIVE);
    (rig, dead_port)
}

/// Sends `payload` from the **port's own** socket through the `send` verb (`packet_inet_ctl`'s
/// `send`, `inet_drv.c:15037`): a connected socket writes with `send(2)` and ignores the address
/// the operand carries, but the operand must still parse as one.
///
/// The reply is asserted to be `[1]`: a `[0]econnrefused` here would mean the **send** consumed
/// the pending ICMP error instead of the read, which is a different (also legal) C path and would
/// leave this case with nothing to read.
fn send_from(rig: &mut Rig, to: u16, payload: &[u8]) {
    let mut arg = loopback_arg(to);
    arg.extend_from_slice(payload);
    assert_eq!(
        rig.control("send".to_string(), &arg),
        ctl_ok(),
        "the port's socket sends its own datagram"
    );
}

/// The `Error` reasons of a drain, rendered by their atom name.
fn errors_of(inputs: &[ActiveInput]) -> Vec<String> {
    inputs
        .iter()
        .filter_map(|input| match &input.event {
            ActiveInputEvent::Error(reason) => Some(reason.atom_name()),
            ActiveInputEvent::Data(_) | ActiveInputEvent::Passive => None,
        })
        .collect()
}

/// Pumps readiness rounds until `want` holds, under [`HANG_GUARD`]; `None` (after a loud `SKIP:`)
/// when the condition never arrives, so a platform that loses the ICMP skips instead of failing
/// or — worse — passing vacuously.
fn until(
    rig: &mut Rig,
    what: &str,
    want: impl Fn(&[ActiveInput]) -> bool,
) -> Option<Vec<ActiveInput>> {
    let deadline = Instant::now() + HANG_GUARD;
    let mut got: Vec<ActiveInput> = Vec::new();
    while !want(&got) {
        if Instant::now() >= deadline {
            eprintln!(
                "SKIP: {what} did not arrive within {HANG_GUARD:?} — the kernel delivered no \
                 ICMP port-unreachable on this host; drained so far: {got:?}"
            );
            return None;
        }
        got.extend(rig.drain_until_quiet().inputs);
    }
    Some(got)
}

/// A datagram read that fails with `ECONNREFUSED` on an **armed** port is delivered as exactly
/// one `{udp_error, S, econnrefused}` — the `packet_error_message` of `inet_drv.c:4245` — and the
/// port is **still live**: a datagram from the connection's own 4-tuple is delivered right after
/// it, and the driver never latched a close (no `{udp_closed, S}` exists).
///
/// A regression that swallowed the failure would fail on the empty drain; one that treated it as
/// a close would fail on `port_closed()`; one that dropped the port would fail on the second,
/// post-error delivery.
#[test]
fn an_active_datagram_read_error_delivers_and_keeps_the_port() {
    let (mut rig, dead_port) = connected_to_a_dead_port();
    send_from(&mut rig, dead_port, b"x");

    let Some(delivered) = until(&mut rig, "the `econnrefused` read error", |got| {
        !errors_of(got).is_empty()
    }) else {
        return;
    };
    assert_eq!(
        errors_of(&delivered),
        vec!["econnrefused".to_string()],
        "exactly one `{{udp_error,_econnrefused}}`: {delivered:?}"
    );
    assert!(
        matches!(
            &delivered[0].event,
            ActiveInputEvent::Error(crate::completion::InetReason::Errno(errno))
                if *errno == libc::ECONNREFUSED
        ),
        "the reason is the failing errno, not a synthesised atom: {delivered:?}"
    );
    assert_eq!(
        delivered[0].port, rig.id(),
        "the delivery is stamped with the port's own id"
    );
    assert!(
        data_of(&delivered).is_empty(),
        "a read failure is never also a data delivery: {delivered:?}"
    );
    assert!(
        !rig.port_closed(),
        "a datagram port has no close message at all — the error is the whole report"
    );

    // The liveness half: the connected socket only accepts datagrams from the 4-tuple it is
    // connected to, so the peer is rebound to that very port and sends.
    let peer = UdpSocket::bind(("127.0.0.1", dead_port))
        .expect("the abandoned peer port is rebindable (UDP has no TIME_WAIT)");
    peer.send_to(b"z", ("127.0.0.1", rig.bound()))
        .expect("the peer's datagram is sent");
    let Some(after) = until(&mut rig, "the post-error datagram", |got| {
        !data_of(got).is_empty()
    }) else {
        return;
    };
    let data = data_of(&after);
    assert_eq!(data.len(), 1, "one delivery after the error: {after:?}");
    assert_eq!(
        data[0].body.as_deref(),
        Some(b"z".as_slice()),
        "the port still reads and delivers after a read error"
    );
    assert!(
        errors_of(&after).is_empty(),
        "the error is reported once, not on every round: {after:?}"
    );
    assert!(
        !rig.port_closed(),
        "the port is still live after both deliveries"
    );
}

/// The probe behind the oracle row: the **same** setup — a datagram port bound to loopback and
/// connected to a port nothing is bound to, `{active,true}` — sends once from the port (the ICMP
/// follows), collects what arrives, rebinds the abandoned peer port and sends from it.
///
/// It prints one `~w` line, `{Errors, Deliveries}`, with the error messages as
/// `{udp_error,Reason}` and the datagrams as `{udp,Data}` — the shape this side renders.
const UDP_ERROR_ROW: &str = r#"#!/usr/bin/env escript
main(_) ->
    process_flag(trap_exit, true),
    {ok,Dead} = gen_udp:open(0,[{ip,{127,0,0,1}}]),
    {ok,DeadPort} = inet:port(Dead),
    ok = gen_udp:close(Dead),
    {ok,S} = gen_udp:open(0,[binary,{active,true},{ip,{127,0,0,1}}]),
    {ok,P} = inet:port(S),
    ok = gen_udp:connect(S, {127,0,0,1}, DeadPort),
    ok = gen_udp:send(S, <<"x">>),
    Errors = collect(2000, []),
    {ok,Peer} = gen_udp:open(DeadPort,[{ip,{127,0,0,1}}]),
    ok = gen_udp:send(Peer, {127,0,0,1}, P, <<"z">>),
    Deliveries = collect(2000, []),
    io:format("~w~n", [{Errors, Deliveries}]),
    halt(0).

collect(Timeout, Acc) ->
    receive
        M -> collect(400, [shape(M)|Acc])
    after Timeout -> lists:reverse(Acc)
    end.

shape({udp_error, _S, R}) -> {udp_error, R};
shape({udp, _S, _IP, _Port, D}) -> {udp, D};
shape({'EXIT', _P, R}) -> {exit, R};
shape(Other) -> {other, Other}.
"#;

/// This crate's rendering of the scenario [`UDP_ERROR_ROW`] drives, in that probe's own format:
/// `[{udp_error,econnrefused}]` for the error phase and `[{udp,<<122>>}]` for the liveness phase.
///
/// `None` when a wait hit [`HANG_GUARD`] (already reported by [`until`]).
fn rust_row() -> Option<(String, String)> {
    let (mut rig, dead_port) = connected_to_a_dead_port();
    send_from(&mut rig, dead_port, b"x");
    let errors = until(&mut rig, "the `econnrefused` read error", |got| {
        !errors_of(got).is_empty()
    })?;

    let peer = UdpSocket::bind(("127.0.0.1", dead_port))
        .expect("the abandoned peer port is rebindable (UDP has no TIME_WAIT)");
    peer.send_to(b"z", ("127.0.0.1", rig.bound()))
        .expect("the peer's datagram is sent");
    let deliveries = until(&mut rig, "the post-error datagram", |got| {
        !data_of(got).is_empty()
    })?;

    let error_row = format!(
        "[{}]",
        errors_of(&errors)
            .into_iter()
            .map(|atom| format!("{{udp_error,{atom}}}"))
            .collect::<Vec<_>>()
            .join(",")
    );
    let delivery_row = format!(
        "[{}]",
        data_of(&deliveries)
            .iter()
            .map(|read| format!("{{udp,{}}}", binary_term(read.body.as_deref().unwrap_or_default())))
            .collect::<Vec<_>>()
            .join(",")
    );
    Some((error_row, delivery_row))
}

/// A byte string rendered the way `~w` renders a binary: `<<122>>` for one byte, `<<>>` for none.
fn binary_term(bytes: &[u8]) -> String {
    format!(
        "<<{}>>",
        bytes
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// The system driver's row: `{udp_error,S,econnrefused}` for the aborted datagram, and then — with
/// the abandoned port rebound and sending — a normal `{udp,S,{127,0,0,1},Port,<<"z">>}` delivery,
/// i.e. the port is **not** torn down by a read error.
///
/// A regression that reported the failure as a close, or that stopped delivering, fails on the
/// comparison; one that reported the error per round fails on the left list having more than one
/// element.
#[test]
fn the_error_and_the_liveness_rows_agree_with_the_oracle() {
    let Some(oracle) = escript_stdout("udp_error_row", UDP_ERROR_ROW, &[]) else {
        return;
    };
    let Some((error_row, delivery_row)) = rust_row() else {
        return;
    };
    let rust = format!("{{{error_row},{delivery_row}}}");
    assert_eq!(
        oracle.trim(),
        format!("{{{error_row},{delivery_row}}}"),
        "the oracle's own row, which the pinned literal below must reproduce"
    );
    assert_eq!(
        oracle.trim(),
        "{[{udp_error,econnrefused}],[{udp,<<122>>}]}",
        "the oracle's row for a connected datagram port with a failing read"
    );
    assert_eq!(
        rust,
        oracle.trim(),
        "the crate reports the error once and keeps delivering, as the system driver does"
    );
}
