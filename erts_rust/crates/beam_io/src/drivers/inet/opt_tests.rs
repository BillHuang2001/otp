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

//! The option vocabulary's own tests: the driver-state store, the wire walk, the two `stype`
//! gates, and the **oracle cross-check** of every code it serves against the real system
//! `tcp_inet` driver.
//!
//! The cross-checks drive a real `PortRegistry` port through the reactor and compare its reply
//! bytes to the oracle's, byte for byte, over the socket states a `tcp_inet` port can be in (no
//! socket, opened stream, post-`close`); the opened-datagram state is `udp_tests.rs`'s, and the
//! `stype` gates are additionally pinned here against a synthetic socket type. Every oracle case
//! **skips loudly** when `erl` is not on `PATH` and never asserts a wall-clock value.

use super::*;
use super::oracle::*;
use crate::driver::DriverRegistry;
use crate::drivers::inet::{INET_AF_ANY, INET_TYPE_DGRAM, INET_TYPE_STREAM};
use crate::poller::Poller;
use crate::port::PortRegistry;
use beam_common::BeamError;
use super::super::packet::PacketType;
use super::super::wire::{ctl_ok, ctl_xerror};
use super::super::{DRIVER_NAME, INET_AF_INET, TcpInetDriver, UDP_DRIVER_NAME};

/// The sequence both sides run: `(request code, payload)` pairs.
///
/// It covers the whole dependency-neutral option vocabulary — the defaults, the `stype` gate
/// before `open`, every set/read-back, the `{active, N}` counter, the set-only `line_delim`,
/// an unserved code and a partial record.
fn option_sequence() -> Vec<(u32, Vec<u8>)> {
    vec![
        // ---- before `open`: the C's `stype` is `-1`, so 30/35 are gated out ----
        (INET_REQ_GETOPTS, vec![22, 23, 30, 32, 35]),
        (INET_REQ_SETOPTS, vec![30, 0, 0, 0, 1]),
        (INET_REQ_SETOPTS, vec![35, 0, 0, 0, 1]),
        (INET_REQ_GETOPTS, vec![30, 35]),
        // ---- `open`: the socket becomes a stream and 30/35 come alive ----
        (INET_REQ_OPEN, open_arg()),
        (INET_REQ_GETOPTS, vec![22, 23, 30, 32, 35]),
        // ---- `line_delim` (40) is set-only ----
        (INET_REQ_GETOPTS, vec![40]),
        (INET_REQ_SETOPTS, vec![40, 0, 0, 0, 10]),
        (INET_REQ_GETOPTS, vec![40, 22]),
        // ---- `{active, …}`, including the 7-byte multi form and the counter ----
        (INET_REQ_SETOPTS, vec![22, 0, 0, 0, 1]),
        (INET_REQ_GETOPTS, vec![22]),
        (INET_REQ_SETOPTS, vec![22, 0, 0, 0, 2]),
        (INET_REQ_GETOPTS, vec![22]),
        (INET_REQ_SETOPTS, vec![22, 0, 0, 0, 0]),
        (INET_REQ_GETOPTS, vec![22]),
        (INET_REQ_SETOPTS, vec![22, 0, 0, 0, 3, 0, 5]),
        (INET_REQ_GETOPTS, vec![22]),
        (INET_REQ_SETOPTS, vec![22, 0, 0, 0, 3, 0, 3]),
        (INET_REQ_GETOPTS, vec![22]),
        // A non-multi `{active, …}` zeroes the running counter.
        (INET_REQ_SETOPTS, vec![22, 0, 0, 0, 2]),
        (INET_REQ_GETOPTS, vec![22]),
        // A negative `N` from a zero counter clamps to zero and falls back to `false`.
        (INET_REQ_SETOPTS, vec![22, 0, 0, 0, 3, 255, 251]),
        (INET_REQ_GETOPTS, vec![22]),
        (INET_REQ_SETOPTS, vec![22, 0, 0, 0, 3, 0, 4]),
        (INET_REQ_GETOPTS, vec![22]),
        // ---- the remaining driver-state options ----
        (INET_REQ_SETOPTS, vec![23, 0, 0, 0, 2]),
        (INET_REQ_GETOPTS, vec![23]),
        (INET_REQ_SETOPTS, vec![32, 0, 0, 4, 0]),
        (INET_REQ_GETOPTS, vec![32]),
        (INET_REQ_SETOPTS, vec![35, 0, 0, 0, 1]),
        (INET_REQ_GETOPTS, vec![35]),
        (INET_REQ_SETOPTS, vec![30, 0, 0, 19, 136]),
        (INET_REQ_GETOPTS, vec![30]),
        (INET_REQ_SETOPTS, vec![30, 255, 255, 255, 255]),
        (INET_REQ_GETOPTS, vec![30]),
        // ---- an unserved code, for either verb ----
        (INET_REQ_GETOPTS, vec![200]),
        (INET_REQ_SETOPTS, vec![200, 0, 0, 0, 1]),
        // ---- a partial trailing record is ignored, not an error ----
        (INET_REQ_SETOPTS, vec![22, 0, 0]),
        // ---- two records in one payload ----
        (INET_REQ_SETOPTS, vec![22, 0, 0, 0, 1, 40, 0, 0, 0, 10]),
        (INET_REQ_GETOPTS, vec![22]),
        // ---- `close` does not clear the C's `stype` ----
        (INET_REQ_CLOSE, vec![]),
        (INET_REQ_GETOPTS, vec![30, 35]),
    ]
}

/// The `{mode, …}`/`{deliver, …}`/`{read_packets, N}` matrix over the states a `tcp_inet` port
/// can be in: no socket, an opened stream socket, and post-`close`.
///
/// Codes `24`/`25` must be answered in **every** state (the C has no gate on either), and code
/// `33` must answer `[1]` to `setopts` and contribute no record to `getopts` in every one of
/// them — a `tcp_inet` socket is never a datagram, so the C's `desc->stype == SOCK_DGRAM` test is
/// false throughout, and its `ival <= 0` refusal (which sits *inside* that gate) never runs.
fn mode_deliver_read_packets_sequence() -> Vec<(u32, Vec<u8>)> {
    vec![
        // ---- (a) no socket at all: `inet.stype` is -1 ----
        (INET_REQ_SETOPTS, vec![24, 0, 0, 0, 1]),
        (INET_REQ_GETOPTS, vec![24]),
        (INET_REQ_SETOPTS, vec![25, 0, 0, 0, 0]),
        (INET_REQ_GETOPTS, vec![25]),
        (INET_REQ_SETOPTS, vec![33, 0, 0, 0, 7]),
        (INET_REQ_GETOPTS, vec![33]),
        // The refusal is *inside* the datagram gate, so a non-datagram socket answers `[1]`.
        (INET_REQ_SETOPTS, vec![33, 0, 0, 0, 0]),
        (INET_REQ_GETOPTS, vec![24, 25, 33]),
        // ---- (b) an opened stream socket ----
        (INET_REQ_OPEN, open_arg()),
        (INET_REQ_SETOPTS, vec![24, 0, 0, 0, 0]),
        (INET_REQ_GETOPTS, vec![24, 25, 33]),
        (INET_REQ_SETOPTS, vec![24, 0, 0, 0, 1]),
        (INET_REQ_GETOPTS, vec![24]),
        (INET_REQ_SETOPTS, vec![25, 0, 0, 0, 1]),
        (INET_REQ_GETOPTS, vec![25]),
        (INET_REQ_SETOPTS, vec![33, 0, 0, 0, 9]),
        (INET_REQ_SETOPTS, vec![33, 0, 0, 0, 0]),
        (INET_REQ_GETOPTS, vec![24, 25, 33]),
        // ---- (c) after `close`: the cached `stype` stays `SOCK_STREAM` ----
        (INET_REQ_CLOSE, vec![]),
        (INET_REQ_GETOPTS, vec![24, 25, 33]),
        (INET_REQ_SETOPTS, vec![33, 0, 0, 0, 0]),
        (INET_REQ_GETOPTS, vec![33]),
        (INET_REQ_GETOPTS, vec![24]),
    ]
}

/// The driver-state defaults are the C descriptor's own initial values
/// (`inet_drv.c:10923`-`:11769`), reported by `getopts` in request order.
/// The C's `TRUNCATE_TO(0, ptr); continue;` (`inet_drv.c:9734`-`:9741`): a failed
/// `getsockopt(2)` drops **only** that record — the accumulated prefix survives and later
/// codes are still tried.
///
/// Measured live oracle on a real datagram socket: `getopts <<10>>` (nodelay, which a datagram
/// socket refuses) ⇒ `[1]`; `<<0,10>>` (reuseaddr then nodelay) ⇒ `[1,0,0,0,0,0]` (the prefix
/// survives, the failing record is dropped); `<<10,0>>` (the failure first) ⇒ `[1,0,0,0,0,0]`
/// — the `continue` does not stop the loop.
#[test]
fn a_failed_getsockopt_drops_only_that_record() {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("a datagram socket");
    let fd = std::os::fd::AsRawFd::as_raw_fd(&socket);
    let dgram = u16::from(INET_TYPE_DGRAM);
    // 10 = `nodelay`, which a datagram socket refuses: the record is dropped, not an error.
    assert_eq!(
        get_options(&SocketOptions::default(), dgram, Some(fd), &[10]).expect("no error"),
        ctl_ok()
    );
    // A succeeding code followed by a failing one: the prefix is preserved.
    assert_eq!(
        get_options(&SocketOptions::default(), dgram, Some(fd), &[0, 10]).expect("no error"),
        vec![1, 0, 0, 0, 0, 0]
    );
    // A failing code first: the later record is still emitted.
    assert_eq!(
        get_options(&SocketOptions::default(), dgram, Some(fd), &[10, 0]).expect("no error"),
        vec![1, 0, 0, 0, 0, 0]
    );
}

#[test]
fn the_driver_state_defaults_are_the_c_descriptor_defaults() {
    let options = SocketOptions::default();
    assert_eq!(options.active, INET_PASSIVE);
    assert_eq!(options.active_count, 0);
    assert_eq!(options.packet, 0);
    assert_eq!(options.send_timeout, INET_INFINITY);
    assert_eq!(options.packet_size, 0);
    assert_eq!(options.send_timeout_close, 0);
    assert_eq!(options.line_delim, b'\n');
    assert_eq!(options.mode, 0, "`mode` = list");
    assert_eq!(options.deliver, 1, "`deliver` = term");
    assert_eq!(options.read_packets, 5, "`read_packets` = 5");

    let reply = get_options(
        &options,
        u16::from(INET_TYPE_STREAM),
        None,
        &[22, 23, 30, 32, 35],
    )
    .expect("getopts");
    assert_eq!(
        reply,
        vec![
            1, 22, 0, 0, 0, 0, // `{active, false}`
            23, 0, 0, 0, 0, // the raw packet type
            30, 255, 255, 255, 255, // `send_timeout` = infinity
            32, 0, 0, 0, 0, // `packet_size` = 0
            35, 0, 0, 0, 0, // `send_timeout_close` = false
        ],
        "the defaults are the oracle's own"
    );
}

/// `{active, N}` **accumulates** its counter, resets it on any other form, clamps it at zero
/// and answers the whole-reply error when the sum leaves the signed 16-bit range
/// (`inet_drv.c:6974`-`:7000`).
#[test]
fn an_active_multi_record_accumulates_its_counter() {
    let mut options = SocketOptions::default();
    let stream = u16::from(INET_TYPE_STREAM);
    let set =
        |options: &mut SocketOptions, arg: &[u8]| set_options(options, stream, None, arg).expect("setopts");
    let read = |options: &SocketOptions| get_options(options, stream, None, &[22]).expect("getopts");

    set(&mut options, &[22, 0, 0, 0, 3, 0, 5]);
    assert_eq!(read(&options), vec![1, 22, 0, 0, 0, 3, 0, 5]);
    // The second multi record **adds** to the running total.
    set(&mut options, &[22, 0, 0, 0, 3, 0, 3]);
    assert_eq!(options.active_count, 8);
    assert_eq!(read(&options), vec![1, 22, 0, 0, 0, 3, 0, 8]);
    // Any non-multi `{active, …}` zeroes it.
    set(&mut options, &[22, 0, 0, 0, 2]);
    assert_eq!(options.active_count, 0);
    assert_eq!(read(&options), vec![1, 22, 0, 0, 0, 2]);
    // A negative `N` from a zero counter clamps to zero and falls back to `false`.
    set(&mut options, &[22, 0, 0, 0, 3, 255, 251]);
    assert_eq!(options.active, INET_PASSIVE);
    assert_eq!(options.active_count, 0);
    assert_eq!(read(&options), vec![1, 22, 0, 0, 0, 0]);
    // A sum past `INT16_MAX` replaces the whole reply with `[0]einval` ...
    set(&mut options, &[22, 0, 0, 0, 3, 127, 255]);
    assert_eq!(read(&options), vec![1, 22, 0, 0, 0, 3, 127, 255]);
    assert_eq!(
        set(&mut options, &[22, 0, 0, 0, 3, 0, 1]),
        ctl_xerror("einval")
    );
    // ... and the rejected record leaves the stored option alone.
    assert_eq!(read(&options), vec![1, 22, 0, 0, 0, 3, 127, 255]);
}

/// `line_delim` is **set-only**: `setopts` of `40` stores it and answers `[1]`, but `getopts`
/// of `40` is the C's `default: RETURN_ERROR()`, which replaces the **whole** reply.
#[test]
fn line_delim_is_set_only_and_an_unserved_code_replaces_the_whole_reply() {
    let mut options = SocketOptions::default();
    let stream = u16::from(INET_TYPE_STREAM);
    assert_eq!(
        set_options(&mut options, stream, None, &[40, 0, 0, 0, 10]).expect("setopts"),
        ctl_ok()
    );
    assert_eq!(options.line_delim, 10);
    assert_eq!(
        get_options(&options, stream, None, &[40]).expect("getopts"),
        ctl_xerror("einval")
    );
    // A multi-code request that *includes* `40` never reaches the other codes.
    assert_eq!(
        get_options(&options, stream, None, &[22, 40]).expect("getopts"),
        ctl_xerror("einval")
    );
    // A genuinely unserved code behaves the same for either verb.
    assert_eq!(
        get_options(&options, stream, None, &[200]).expect("getopts"),
        ctl_xerror("einval")
    );
    assert_eq!(
        set_options(&mut options, stream, None, &[200, 0, 0, 0, 1]).expect("setopts"),
        ctl_xerror("einval")
    );
    // The C's `while (len >= 5)` ignores a partial trailing record; empty payloads answer ok.
    assert_eq!(
        set_options(&mut options, stream, None, &[22, 0, 0]).expect("setopts"),
        ctl_ok()
    );
    assert_eq!(
        set_options(&mut options, stream, None, &[]).expect("setopts"),
        ctl_ok()
    );
    assert_eq!(
        get_options(&options, stream, None, &[]).expect("getopts"),
        ctl_ok()
    );
}

/// `send_timeout`/`send_timeout_close` are gated on `stype == SOCK_STREAM`
/// (`inet_drv.c:7104`/`:7115`, `:9237`/`:9247`): before `open` — and on any datagram socket —
/// they are ignored and contribute no `getopts` record.
#[test]
fn the_send_timeout_codes_are_gated_on_a_stream_socket() {
    let mut options = SocketOptions::default();
    let stream = u16::from(INET_TYPE_STREAM);
    let dgram = u16::from(INET_TYPE_DGRAM);
    for stype in [0, dgram] {
        assert_eq!(
            set_options(&mut options, stype, None, &[30, 0, 0, 0, 1]).expect("setopts"),
            ctl_ok()
        );
        assert_eq!(
            set_options(&mut options, stype, None, &[35, 0, 0, 0, 1]).expect("setopts"),
            ctl_ok()
        );
        assert_eq!(options.send_timeout, INET_INFINITY);
        assert_eq!(options.send_timeout_close, 0);
        assert_eq!(
            get_options(&options, stype, None, &[30, 35]).expect("getopts"),
            ctl_ok()
        );
    }

    // On a stream socket both are stored and reported.
    assert_eq!(
        set_options(&mut options, stream, None, &[30, 0, 0, 19, 136]).expect("setopts"),
        ctl_ok()
    );
    assert_eq!(options.send_timeout, 5000);
    assert_eq!(
        get_options(&options, stream, None, &[30, 35]).expect("getopts"),
        vec![1, 30, 0, 0, 19, 136, 35, 0, 0, 0, 0]
    );
}

/// `{mode, …}` (`24`) and `{deliver, …}` (`25`) carry **no** `stype` gate: the C stores and
/// reports both unconditionally (`inet_set_opts`, `inet_drv.c:6939`-`:6956`; `inet_fill_opts`,
/// `:9168`-`:9175`), so the identical replies come back on a port with no socket, on a stream
/// socket and on a datagram socket.
#[test]
fn mode_and_deliver_are_served_on_either_socket_type() {
    for stype in [0, u16::from(INET_TYPE_STREAM), u16::from(INET_TYPE_DGRAM)] {
        let mut options = SocketOptions::default();
        // The defaults: `list` (`0`) and `term` (`1`), in request order.
        assert_eq!(
            get_options(&options, stype, None, &[24, 25]).expect("getopts"),
            vec![1, 24, 0, 0, 0, 0, 25, 0, 0, 0, 1],
            "the driver-state defaults are reported for stype {stype}"
        );
        // Non-default values store and read back.
        assert_eq!(
            set_options(&mut options, stype, None, &record(24, 1)).expect("setopts"),
            ctl_ok()
        );
        assert_eq!(
            set_options(&mut options, stype, None, &record(25, 0)).expect("setopts"),
            ctl_ok()
        );
        assert_eq!(
            get_options(&options, stype, None, &[24, 25]).expect("getopts"),
            vec![1, 24, 0, 0, 0, 1, 25, 0, 0, 0, 0],
            "the mode/deliver values read back for stype {stype}"
        );
        // A multi-code request mixing them with an ungated neighbour keeps request order.
        assert_eq!(
            get_options(&options, stype, None, &[23, 25, 24]).expect("getopts"),
            vec![1, 23, 0, 0, 0, 0, 25, 0, 0, 0, 0, 24, 0, 0, 0, 1],
            "the records are in request order for stype {stype}"
        );
    }
}

/// `{read_packets, N}` (`33`) is gated on the **cached** socket type: the C's
/// `if (desc->stype == SOCK_DGRAM)` guards both the store and the read-back
/// (`inet_drv.c:7138`-`:7146`, `:9258`-`:9270`).
///
/// On a non-datagram socket the store is skipped — **including its `ival <= 0` refusal**, which
/// sits inside the gate, so `setopts [33, 0]` still answers `[1]` — and the read-back
/// contributes no record (not an error).
#[test]
fn read_packets_is_gated_on_the_cached_datagram_socket_type() {
    let mut options = SocketOptions::default();
    let dgram = u16::from(INET_TYPE_DGRAM);

    for stype in [0, u16::from(INET_TYPE_STREAM)] {
        assert_eq!(
            set_options(&mut options, stype, None, &record(33, 7)).expect("setopts"),
            ctl_ok(),
            "a non-datagram socket ignores the read_packets store for stype {stype}"
        );
        assert_eq!(
            set_options(&mut options, stype, None, &record(33, 0)).expect("setopts"),
            ctl_ok(),
            "the `ival <= 0` refusal is inside the datagram gate, for stype {stype}"
        );
        assert_eq!(options.read_packets, INET_PACKET_POLL, "the default is untouched");
        assert_eq!(
            get_options(&options, stype, None, &[33]).expect("getopts"),
            ctl_ok(),
            "no record is contributed for stype {stype}"
        );
        // The neighbours in the same request are unaffected.
        assert_eq!(
            get_options(&options, stype, None, &[24, 33, 25]).expect("getopts"),
            vec![1, 24, 0, 0, 0, 0, 25, 0, 0, 0, 1],
            "the gated code drops only its own record, for stype {stype}"
        );
    }

    // On a datagram socket the value stores, reads back, and a non-positive one is `[0]einval`.
    assert_eq!(
        set_options(&mut options, dgram, None, &record(33, 7)).expect("setopts"),
        ctl_ok()
    );
    assert_eq!(options.read_packets, 7);
    assert_eq!(
        get_options(&options, dgram, None, &[33]).expect("getopts"),
        vec![1, 33, 0, 0, 0, 7]
    );
    assert_eq!(
        set_options(&mut options, dgram, None, &record(33, 0)).expect("setopts"),
        ctl_xerror("einval")
    );
    assert_eq!(
        set_options(&mut options, dgram, None, &record(33, -3)).expect("setopts"),
        ctl_xerror("einval")
    );
    assert_eq!(options.read_packets, 7, "a refused record leaves the value alone");
}

/// A **truncated** `{active, N}` record is the one payload this floor cannot mirror: the C
/// reads the counter's two bytes past the payload (`inet_drv.c:6977`).
#[test]
fn a_truncated_active_multi_record_is_bad_arg() {
    let mut options = SocketOptions::default();
    let stream = u16::from(INET_TYPE_STREAM);
    let err = set_options(&mut options, stream, None, &[22, 0, 0, 0, 3])
        .expect_err("a truncated {active, N} record");
    assert!(matches!(err, BeamError::BadArg), "got {err:?}");
    // The same length with a non-multi value is an ordinary record.
    assert_eq!(
        set_options(&mut options, stream, None, &[22, 0, 0, 0, 2]).expect("setopts"),
        ctl_ok()
    );
    assert_eq!(options.active, INET_ONCE);
}

/// The compatibility gate for the option vocabulary: the **same** sequence run against the
/// real system `tcp_inet` driver must answer the same reply bytes this module answers.
///
/// If `erl` is absent the check **skips loudly**, never fails.
#[test]
fn the_option_code_replies_agree_with_the_erl_oracle() {
    let sequence = option_sequence();
    let rust = rust_replies(crate::drivers::inet::DRIVER_NAME, &sequence);
    assert_eq!(rust.len(), sequence.len(), "one reply per step");

    // The measured bytes, on the record: the driver-state defaults, the `stype` gate, the
    // set-only `line_delim` and the unserved-code errors.
    assert_eq!(
        rust[0],
        vec![1, 22, 0, 0, 0, 0, 23, 0, 0, 0, 0, 32, 0, 0, 0, 0],
        "before `open` the C's `stype` is -1, so 30/35 contribute no record"
    );
    assert_eq!(rust[4], ctl_ok(), "`open`");
    assert_eq!(
        rust[5],
        vec![
            1, 22, 0, 0, 0, 0, 23, 0, 0, 0, 0, 30, 255, 255, 255, 255, 32, 0, 0, 0, 0, 35, 0, 0,
            0, 0,
        ],
        "after `open` the stream-gated options are reported"
    );
    assert_eq!(rust[6], ctl_xerror("einval"), "`getopts` of 40");
    assert_eq!(rust[7], ctl_ok(), "`setopts` of 40");
    assert_eq!(
        rust[8],
        ctl_xerror("einval"),
        "a multi-code `getopts` including 40 replaces the whole reply"
    );
    assert_eq!(rust[16], vec![1, 22, 0, 0, 0, 3, 0, 5], "the active-multi read-back");
    assert_eq!(rust[18], vec![1, 22, 0, 0, 0, 3, 0, 8], "the counter accumulates");
    assert_eq!(
        rust[20],
        vec![1, 22, 0, 0, 0, 2],
        "a non-multi form zeroes the counter"
    );
    assert_eq!(
        rust[22],
        vec![1, 22, 0, 0, 0, 0],
        "a negative `N` from a zero counter falls back to `false`"
    );
    assert_eq!(rust[35], ctl_xerror("einval"), "`getopts` of an unserved code");
    assert_eq!(rust[36], ctl_xerror("einval"), "`setopts` of an unserved code");
    assert_eq!(rust[37], ctl_ok(), "a partial record is ignored");
    assert_eq!(
        rust[41],
        vec![1, 30, 255, 255, 255, 255, 35, 0, 0, 0, 1],
        "`close` does not clear `stype`"
    );

    assert_agrees_with_oracle(crate::drivers::inet::DRIVER_NAME, &sequence, &rust);
}

/// The `{mode, …}`/`{deliver, …}`/`{read_packets, N}` matrix against the live oracle: every row
/// must answer the same bytes on a port with no socket, on an opened stream socket and after a
/// `close`.
///
/// This is the row set that covers the headline `gen_tcp:listen(0, [binary])` failure: `24` is
/// what `inet:setopts(S, [{mode, binary}])` sends, and a `[0]einval` there fails the listen.
#[test]
fn the_mode_deliver_and_read_packets_rows_agree_with_the_erl_oracle() {
    let sequence = mode_deliver_read_packets_sequence();
    let rust = rust_replies(crate::drivers::inet::DRIVER_NAME, &sequence);
    assert_eq!(rust.len(), sequence.len(), "one reply per step");

    // The exact bytes, on the record, for the three states.
    assert_eq!(rust[0], ctl_ok(), "(a) `setopts` of 24 with no socket");
    assert_eq!(rust[1], vec![1, 24, 0, 0, 0, 1], "(a) `getopts` of 24");
    assert_eq!(rust[2], ctl_ok(), "(a) `setopts` of 25 with no socket");
    assert_eq!(rust[3], vec![1, 25, 0, 0, 0, 0], "(a) `getopts` of 25");
    assert_eq!(rust[4], ctl_ok(), "(a) `setopts` of 33 is ignored, not refused");
    assert_eq!(rust[5], ctl_ok(), "(a) `getopts` of 33 contributes nothing");
    assert_eq!(rust[6], ctl_ok(), "(a) the `ival <= 0` refusal is inside the gate");
    assert_eq!(
        rust[7],
        vec![1, 24, 0, 0, 0, 1, 25, 0, 0, 0, 0],
        "(a) a mixed request keeps every ungated record"
    );
    assert_eq!(rust[8], ctl_ok(), "`open`");
    assert_eq!(rust[9], ctl_ok(), "(b) `setopts` of 24 on a stream socket");
    assert_eq!(
        rust[10],
        vec![1, 24, 0, 0, 0, 0, 25, 0, 0, 0, 0],
        "(b) both record back after the reset, 33 contributes nothing"
    );
    assert_eq!(rust[11], ctl_ok(), "(b) the mode=binary record is accepted");
    assert_eq!(rust[12], vec![1, 24, 0, 0, 0, 1], "(b) `getopts` of 24");
    assert_eq!(rust[13], ctl_ok(), "(b) the deliver=term record is accepted");
    assert_eq!(rust[14], vec![1, 25, 0, 0, 0, 1], "(b) `getopts` of 25");
    assert_eq!(rust[15], ctl_ok(), "(b) `setopts` of 33 is ignored on a stream socket");
    assert_eq!(rust[16], ctl_ok(), "(b) `setopts [33, 0]` is still `[1]`");
    assert_eq!(
        rust[17],
        vec![1, 24, 0, 0, 0, 1, 25, 0, 0, 0, 1],
        "(b) the ungated pair is unaffected by the gated code"
    );
    assert_eq!(rust[18], ctl_ok(), "`close`");
    assert_eq!(
        rust[19],
        vec![1, 24, 0, 0, 0, 1, 25, 0, 0, 0, 1],
        "(c) both values survive a close; 33 still contributes nothing"
    );
    assert_eq!(rust[20], ctl_ok(), "(c) `setopts [33, 0]` after a close is still `[1]`");
    assert_eq!(rust[21], ctl_ok(), "(c) `getopts` of 33 after a close");
    assert_eq!(rust[22], vec![1, 24, 0, 0, 0, 1], "(c) the value survives a close");

    assert_agrees_with_oracle(crate::drivers::inet::DRIVER_NAME, &sequence, &rust);
}

/// The `gen_tcp:listen(0, [binary])` shape, end to end: `gen_tcp:listen` opens a `tcp_inet`
/// socket and sends `{mode, binary}` (`24 = 1`) through `inet:setopts`, so a `[0]einval` there
/// fails the whole `listen`.
///
/// The Rust side opens + binds + listens a real `tcp_inet` port through a [`PortRegistry`] and
/// drives the same option records; the oracle side runs `gen_tcp:listen/2` itself and reads the
/// value back with `inet:getopts/2`. Both must decode to `binary` (`1`).
///
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn the_gen_tcp_listen_binary_row_agrees_with_the_oracle() {
    let mut poller = Poller::new().expect("a poll set");
    let registry = DriverRegistry::with_builtins();
    let driver = registry
        .create(super::super::DRIVER_NAME)
        .expect("the built-in tcp_inet driver is registered");
    let mut ports = PortRegistry::new();
    let id = ports.open(&mut poller, driver).expect("the port opens");

    let mut control = |op: &str, arg: &[u8]| -> Vec<u8> {
        ports
            .get_mut(id)
            .expect("the port is live")
            .control(&mut poller, op, arg)
            .unwrap_or_else(|err| panic!("the `{op}` verb failed: {err:?}"))
    };

    assert_eq!(control("open", &open_arg()), ctl_ok(), "open");
    let bound = control("bind", &[INET_AF_ANY, 0, 0]);
    assert_eq!(bound[0], 1, "bind answers `[1] ++ port`: {bound:?}");
    assert_eq!(control("listen", &[0, 0]), ctl_ok(), "listen");
    // `inet:setopts(S, [{mode, binary}])` — the record `gen_tcp:listen(0, [binary])` sends.
    assert_eq!(
        control("setopts", &record(24, 1)),
        ctl_ok(),
        "the mode=binary record is accepted on a listening stream socket"
    );
    assert_eq!(
        control("getopts", &[24]),
        vec![1, 24, 0, 0, 0, 1],
        "`inet:getopts(S, [mode])` reports `binary`"
    );
    let closed = ports.close(&mut poller, id);
    assert!(closed.is_ok(), "the port closes: {closed:?}");

    let expr = "{ok, L} = gen_tcp:listen(0, [binary]), \
                R = inet:getopts(L, [mode, deliver]), \
                gen_tcp:close(L), \
                io:format(\"~p~n\", [R])";
    let Some(stdout) = erl_stdout(expr) else {
        eprintln!("SKIP: no `erl` on PATH — skipping the gen_tcp:listen binary oracle check");
        return;
    };
    assert!(
        stdout.contains("{ok,[{mode,binary},{deliver,term}]}"),
        "the oracle's gen_tcp:listen(0,[binary]) reports mode=binary/deliver=term: {stdout:?}"
    );
}

// ---------------------------------------------------------------------------
// The round trips the per-code tables above leave to this suite
// ---------------------------------------------------------------------------

/// `224.0.0.1` — the IPv4 "all hosts" multicast group, as the signed word the wire carries.
const MULTICAST_GROUP: i32 = 0xe000_0001u32 as i32;
/// `127.0.0.1` — the loopback interface address a membership's local word nominates.
const LOOPBACK: i32 = 0x7f00_0001;

/// The `[1]`-tagged reply carrying one `[code, value_be32]` record (the `opt_sys_tests.rs` shape,
/// repeated here because each sibling suite owns its own copy).
fn one(code: u8, value: i32) -> Vec<u8> {
    let mut reply = ctl_ok();
    reply.extend_from_slice(&record(code, value));
    reply
}

/// The **numeric** route `erlang:port_control/3` really carries: the same `(request code, payload)`
/// steps as [`rust_replies`], but sent as a decimal `Op + ERTS_INET_DRV_CONTROL_MAGIC_NUMBER`
/// command string (what `prim_inet` sends, `inet_drv.c:11958`) instead of the named verb.
fn numeric_replies(driver: &str, sequence: &[(u32, Vec<u8>)]) -> Vec<Vec<u8>> {
    let mut poller = Poller::new().expect("a poll set");
    let registry = DriverRegistry::with_builtins();
    let created = registry
        .create(driver)
        .expect("the built-in driver is registered");
    let mut ports = PortRegistry::new();
    let id = ports.open(&mut poller, created).expect("the port opens");
    let replies = sequence
        .iter()
        .map(|(code, arg)| {
            let op = CONTROL_MAGIC.wrapping_add(*code).to_string();
            ports
                .port_control(&mut poller, id, &op, arg)
                .unwrap_or_else(|err| panic!("the numeric opcode {op} failed: {err:?}"))
        })
        .collect();
    ports.close(&mut poller, id).expect("closes the port");
    replies
}

/// Asserts the numeric opcode route answers **byte-for-byte** what the named verbs answer, then
/// byte-compares the named replies with the live oracle — whose own side already sends
/// `port_control(P, C + M, A)`, i.e. the numeric route, so the pair is pinned against it.
fn check_numeric(driver: &str, sequence: &[(u32, Vec<u8>)]) {
    let named = rust_replies(driver, sequence);
    let numeric = numeric_replies(driver, sequence);
    assert_eq!(
        named, numeric,
        "the numeric `port_control/3` route must answer the named verbs' replies on {driver}"
    );
    assert_agrees_with_oracle(driver, sequence, &named);
}

/// The two `(driver, open request)` pairs every round-trip row below runs over.
fn drivers() -> [(&'static str, (u32, Vec<u8>)); 2] {
    [
        (DRIVER_NAME, (INET_REQ_OPEN, open_arg())),
        (UDP_DRIVER_NAME, (INET_REQ_OPEN, open_arg_dgram())),
    ]
}

// ---------------------------------------------------------------------------
// (1) `sndbuf` (6) / `recvbuf` (7): a real set → kernel read-back, plus the
//     `{buffer, N}` interaction the `INET_FLG_BUFFER_SET` rule makes observable.
// ---------------------------------------------------------------------------

/// `{sndbuf, N}`/`{recvbuf, N}` round-trip through a live socket, and the report is the
/// **kernel's** `getsockopt(2)` value, not the requested one: Linux doubles both requests
/// (`inet_drv.c:7321`-`:7360`'s `sock_setsockopt` + the kernel's `sk_rcvbuf`/`sk_sndbuf` rule;
/// `opt.rs`'s module doc §1), so `4096` reads back as `8192`.
///
/// The `{recbuf, N}` write also raises the driver's own read buffer unless `{buffer, N}` was set
/// explicitly (`INET_FLG_BUFFER_SET`) — the same rule as
/// [`super::super::opt_sys::tests`]'s write-only row, made observable here by reading `7` and `20`
/// back in the same round trip. `{sndbuf, N}` never touches it.
#[test]
fn the_kernel_buffer_sizes_round_trip_through_the_kernel() {
    for (driver, open) in drivers() {
        let sequence = vec![
            open,
            // A small request is doubled by the kernel, so the read-back is 8192 both ways.
            (INET_REQ_SETOPTS, record(INET_OPT_SNDBUF, 4096)),
            (INET_REQ_GETOPTS, vec![INET_OPT_SNDBUF]),
            (INET_REQ_SETOPTS, record(INET_OPT_RCVBUF, 4096)),
            (INET_REQ_GETOPTS, vec![INET_OPT_RCVBUF]),
            // 4096 < the 9216 default, so the driver's own buffer is untouched.
            (INET_REQ_GETOPTS, vec![INET_LOPT_BUFFER]),
            // A request above the stored buffer raises it to the *requested* size.
            (INET_REQ_SETOPTS, record(INET_OPT_RCVBUF, 20_000)),
            (INET_REQ_GETOPTS, vec![INET_LOPT_BUFFER]),
            (INET_REQ_GETOPTS, vec![INET_OPT_RCVBUF]),
            // `{buffer, N}` pins it: a later `recbuf` no longer raises it.
            (INET_REQ_SETOPTS, record(INET_LOPT_BUFFER, 500)),
            (INET_REQ_SETOPTS, record(INET_OPT_RCVBUF, 100)),
            (INET_REQ_GETOPTS, vec![INET_LOPT_BUFFER]),
            (INET_REQ_GETOPTS, vec![INET_OPT_RCVBUF]),
            // `{sndbuf, N}` never raises the driver's buffer (already pinned at 500).
            (INET_REQ_SETOPTS, record(INET_OPT_SNDBUF, 70_000)),
            (INET_REQ_GETOPTS, vec![INET_LOPT_BUFFER]),
        ];
        let replies = rust_replies(driver, &sequence);
        assert_eq!(replies.len(), sequence.len(), "one reply per step");
        for (index, reply) in replies.iter().enumerate() {
            if sequence[index].0 == INET_REQ_SETOPTS || index == 0 {
                assert_eq!(*reply, ctl_ok(), "step {index} of {driver}: {reply:?}");
            }
        }
        assert_eq!(
            replies[2],
            one(INET_OPT_SNDBUF, 8192),
            "Linux doubles a `{{sndbuf, 4096}}` request, so the report is the kernel's 8192"
        );
        assert_eq!(
            replies[4],
            one(INET_OPT_RCVBUF, 8192),
            "Linux doubles a `{{recbuf, 4096}}` request too"
        );
        assert_eq!(
            replies[5],
            one(INET_LOPT_BUFFER, 9216),
            "a 4096 `recbuf` is below the 9216 default, so `buffer` is untouched"
        );
        assert_eq!(
            replies[7],
            one(INET_LOPT_BUFFER, 20_000),
            "a raised `recbuf` raises the driver's own read buffer"
        );
        assert_eq!(replies[8], one(INET_OPT_RCVBUF, 40_000), "and the kernel doubled it");
        assert_eq!(
            replies[11],
            one(INET_LOPT_BUFFER, 500),
            "an explicit `{{buffer, 500}}` pins the size against a later `recbuf`"
        );
        assert_eq!(replies[12], one(INET_OPT_RCVBUF, 200), "the kernel still doubles it");
        assert_eq!(
            replies[14],
            one(INET_LOPT_BUFFER, 500),
            "`sndbuf` never raises the driver's read buffer"
        );
        assert_agrees_with_oracle(driver, &sequence, &replies);
    }
}

// ---------------------------------------------------------------------------
// (2) `reuseport_lb` (18)
// ---------------------------------------------------------------------------

/// `{reuseport_lb, Bool}` (18) stores and reads back on both drivers, and — because Linux resolves
/// `reuseport` (17) *and* `reuseport_lb` (18) onto the same `SO_REUSEPORT` (`opt_sys.rs:176`) — the
/// sibling code reports the same value.
#[test]
fn reuseport_lb_round_trips_on_both_drivers() {
    let mut both_set = one(INET_OPT_REUSEPORT, 1);
    both_set.extend_from_slice(&record(INET_OPT_REUSEPORT_LB, 1));

    for (driver, open) in drivers() {
        let sequence = vec![
            open,
            (INET_REQ_SETOPTS, record(INET_OPT_REUSEPORT_LB, 1)),
            (INET_REQ_GETOPTS, vec![INET_OPT_REUSEPORT_LB]),
            (INET_REQ_GETOPTS, vec![INET_OPT_REUSEPORT, INET_OPT_REUSEPORT_LB]),
            (INET_REQ_SETOPTS, record(INET_OPT_REUSEPORT_LB, 0)),
            (INET_REQ_GETOPTS, vec![INET_OPT_REUSEPORT_LB]),
        ];
        let replies = rust_replies(driver, &sequence);
        assert_eq!(replies[0], ctl_ok(), "`open` on {driver}");
        assert_eq!(replies[1], ctl_ok(), "`setopts` of 18 on {driver}");
        assert_eq!(replies[2], one(INET_OPT_REUSEPORT_LB, 1), "the read-back on {driver}");
        assert_eq!(
            replies[3], both_set,
            "17 and 18 resolve to the same kernel option, so both report 1 on {driver}"
        );
        assert_eq!(replies[4], ctl_ok(), "the clearing write on {driver}");
        assert_eq!(replies[5], one(INET_OPT_REUSEPORT_LB, 0), "and it reads back as 0");
        assert_agrees_with_oracle(driver, &sequence, &replies);
    }
}

// ---------------------------------------------------------------------------
// (3) `raw` (34): a caller-nominated triple written and read back
// ---------------------------------------------------------------------------

/// The `raw` (`34`) write and the `raw` read are the same caller-nominated `(proto, type, bytes)`
/// triple, so a value written through one must come back through the other: `IP_TTL` (`proto 0`,
/// `type 2`) is set to `5` and read back as the four **host-order** bytes the C `memcpy`s
/// (`inet_drv.c:7806`-`:7846` the set, `:9535`-`:9578` the get). A plain `{ttl, _}` read (46)
/// afterwards reports the same `5`, proving the write really landed on the kernel option the
/// plain code also names.
#[test]
fn a_raw_triple_round_trips_on_a_live_socket() {
    /// `IP_TTL` — the `netinet/in.h` name the `raw` triple nominates.
    const TTL_CODE: i32 = 2;

    let mut set = record(INET_OPT_RAW, 0);
    set.extend_from_slice(&TTL_CODE.to_be_bytes());
    set.extend_from_slice(&4i32.to_be_bytes());
    set.extend_from_slice(&5i32.to_ne_bytes());

    // `[opt][proto][type][data_provided = 0][arg_sz = 4]`.
    let mut get = vec![INET_OPT_RAW];
    get.extend_from_slice(&0i32.to_be_bytes());
    get.extend_from_slice(&TTL_CODE.to_be_bytes());
    get.push(0);
    get.extend_from_slice(&4i32.to_be_bytes());

    let mut raw_reply = vec![INET_OPT_RAW];
    raw_reply.extend_from_slice(&0i32.to_be_bytes());
    raw_reply.extend_from_slice(&TTL_CODE.to_be_bytes());
    raw_reply.extend_from_slice(&4u32.to_be_bytes());
    raw_reply.extend_from_slice(&5i32.to_ne_bytes());
    let mut read_back = ctl_ok();
    read_back.extend_from_slice(&raw_reply);

    for (driver, open) in drivers() {
        let sequence = vec![
            open,
            (INET_REQ_SETOPTS, set.clone()),
            (INET_REQ_GETOPTS, get.clone()),
            // The same kernel option through its plain code reports the same value.
            (INET_REQ_GETOPTS, vec![INET_OPT_TTL]),
        ];
        let replies = rust_replies(driver, &sequence);
        assert_eq!(replies[0], ctl_ok(), "`open` on {driver}");
        assert_eq!(replies[1], ctl_ok(), "the `raw` write on {driver}");
        assert_eq!(
            replies[2], read_back,
            "the `raw` read is the caller-nominated triple's answer on {driver}"
        );
        assert_eq!(
            replies[3],
            one(INET_OPT_TTL, 5),
            "`{{ttl, _}}` names the same kernel option the raw write set, on {driver}"
        );
        assert_agrees_with_oracle(driver, &sequence, &replies);
    }
}

// ---------------------------------------------------------------------------
// (4) `pktoptions` (45): the non-empty control-message reply
// ---------------------------------------------------------------------------

/// `pktoptions` (`45`) reports the control messages the kernel stamped onto the **last received**
/// segment, so the only way to get a non-empty reply is a real socket that really received a byte
/// while `{recvtos, true}` (43) and `{recvttl, true}` (47) were armed. Loopback then yields exactly
/// two records — `IP_TTL` as `{ttl, _}` (46, `64`) and `IP_TOS` as `{tos, _}` (9, `0`) — in the
/// kernel's own order, which is the C's `PUT_CMSG_INT32` loop order
/// (`inet_drv.c:9602`-`:9690`); the total-length word is their summed 10 bytes.
///
/// Both sides run the same real connection, and the oracle row is a **custom** expression (the
/// shared [`oracle_expression`] drives a bare port only): it opens a `gen_tcp` pair, arms the two
/// options, sends one byte and asks the port with `port_control(S, Op + M, <<45>>)`.
#[test]
fn pktoptions_reports_the_received_segments_control_messages() {
    use std::io::Write;
    use std::os::unix::io::IntoRawFd;
    use std::net::{TcpListener, TcpStream};

    let expected = vec![
        1, // `INET_REP_OK`
        45, 0, 0, 0, 10, // the code and the summed record length
        46, 0, 0, 0, 64, // `IP_TTL` → `{ttl, 64}` (the peer's loopback default)
        9, 0, 0, 0, 0, // `IP_TOS` → `{tos, 0}`
    ];

    let mut poller = Poller::new().expect("a poll set");
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let address = listener.local_addr().expect("an address");
    let mut peer = TcpStream::connect(address).expect("the peer connects");
    let (server, _unused) = listener.accept().expect("accepts the connection");
    let driver =
        TcpInetDriver::from_socket(server.into_raw_fd()).expect("the driver adopts the socket");
    let mut ports = PortRegistry::new();
    let id = ports.open(&mut poller, Box::new(driver)).expect("the port opens");
    let mut control = |op: &str, arg: &[u8]| -> Vec<u8> {
        ports
            .get_mut(id)
            .expect("the port is live")
            .control(&mut poller, op, arg)
            .unwrap_or_else(|err| panic!("the `{op}` verb failed: {err:?}"))
    };

    // The kernel only stamps the control messages onto a segment it receives while the options are
    // armed, so they must be set **before** the peer writes.
    assert_eq!(control("setopts", &record(INET_OPT_RECVTOS, 1)), ctl_ok());
    assert_eq!(control("setopts", &record(INET_OPT_RECVTTL, 1)), ctl_ok());
    peer.write_all(b"X").expect("the peer writes one byte");

    let mut got = Vec::new();
    for _ in 0..200 {
        assert_eq!(control("recv", b""), ctl_ok(), "the `recv` verb");
        got = ports.get_mut(id).expect("the port is live").drain_input();
        if !got.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(got, b"X", "the byte reaches the port queue before the read-back");
    assert_eq!(
        control("getopts", &[INET_OPT_PKTOPTIONS]),
        expected,
        "the two control messages of the received segment"
    );
    assert!(ports.close(&mut poller, id).is_ok(), "the port closes");

    let expr = "{ok, L} = gen_tcp:listen(0, [binary, {active, false}]), \
                {ok, {_, P}} = inet:sockname(L), \
                register(client, spawn(fun() -> \
                    {ok, C} = gen_tcp:connect({127,0,0,1}, P, [binary, {active, false}]), \
                    receive go -> ok end, \
                    ok = gen_tcp:send(C, <<\"X\">>), \
                    timer:sleep(400), \
                    gen_tcp:close(C) end)), \
                {ok, S} = gen_tcp:accept(L), \
                D = case is_port(S) of \
                        true -> \
                            ok = inet:setopts(S, [{recvtos, true}, {recvttl, true}]), \
                            client ! go, \
                            {ok, _} = gen_tcp:recv(S, 1, 2000), \
                            binary_to_list(iolist_to_binary(erlang:port_control(S, 16#03f1a308, <<45>>))); \
                        false -> noport \
                    end, \
                case D of noport -> io:format(\"noport~n\"); _ -> io:format(\"~w~n\", [D]) end";
    let Some(stdout) = erl_stdout(expr) else {
        eprintln!("SKIP: no `erl` on PATH — skipping the pktoptions oracle check");
        return;
    };
    if stdout.contains("noport") {
        eprintln!("SKIP: this oracle's `gen_tcp` sockets are not ports — skipping");
        return;
    }
    let Some(line) = stdout.lines().find(|line| line.trim_start().starts_with('[')) else {
        panic!("could not parse the oracle's pktoptions reply dump: {stdout:?}");
    };
    let oracle: Vec<u8> = line
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .filter(|part| !part.trim().is_empty())
        .map(|part| {
            part.trim()
                .parse::<u8>()
                .unwrap_or_else(|_| panic!("a non-byte in the oracle dump: {part:?}"))
        })
        .collect();
    eprintln!("oracle pktoptions reply: {oracle:?}");
    assert_eq!(oracle, expected, "the oracle's control-message records");
}

// ---------------------------------------------------------------------------
// (5)+(6) The numeric opcode route for the variable-length records
// ---------------------------------------------------------------------------

/// The **numeric** route `erlang:port_control/3` really carries (`Op + 0x03f1a300`, decoded by
/// `opcode.rs`) answers byte-for-byte what the named verbs do for the records that carry extra
/// payload: `add_membership` (14) / `drop_membership` (15) with their three extra words,
/// `bind_to_device` (42) with its device name, and `netns` (38) with its path — including the
/// `[0]einval` refusals the two codes give (`default: RETURN_ERROR()` on the get side, a negative
/// length on the set side).
///
/// `netns` is worth being explicit about: the C's `INET_REQ_SETOPTS`/`INET_REQ_GETOPTS` arms only
/// **store and report** the path (`inet_set_opts`, `inet_drv.c:7152`-`:7163`;
/// `inet_fill_opts`, `:9279`-`:9287`). `setns(2)` is called from `tcp_inet_open`
/// (`:5134`-`:5186`) and `call_getifaddrs` (`:5749`-`:5800`) — i.e. when a **socket** is opened —
/// never from the option walk, so no namespace is entered here and none needs to be.
#[test]
fn the_variable_length_records_answer_the_same_on_the_numeric_opcode_route() {
    for (driver, open) in drivers() {
        let mut add = record(INET_OPT_ADD_MEMBERSHIP, i32::from(INET_AF_INET));
        add.extend_from_slice(&0i32.to_be_bytes());
        add.extend_from_slice(&MULTICAST_GROUP.to_be_bytes());
        add.extend_from_slice(&LOOPBACK.to_be_bytes());
        let mut drop = add.clone();
        drop[0] = INET_OPT_DROP_MEMBERSHIP;

        let mut bind = record(INET_OPT_BIND_TO_DEVICE, 2);
        bind.extend_from_slice(b"lo");

        let mut netns = record(INET_LOPT_NETNS, 4);
        netns.extend_from_slice(b"/run");

        let mut bind_reply = ctl_ok();
        bind_reply.push(INET_OPT_BIND_TO_DEVICE);
        bind_reply.extend_from_slice(&2u32.to_be_bytes());
        bind_reply.extend_from_slice(b"lo");

        let mut netns_reply = ctl_ok();
        netns_reply.push(INET_LOPT_NETNS);
        netns_reply.extend_from_slice(&4u32.to_be_bytes());
        netns_reply.extend_from_slice(b"/run");

        let sequence = vec![
            open,
            (INET_REQ_SETOPTS, add),
            (INET_REQ_SETOPTS, drop),
            (INET_REQ_SETOPTS, bind),
            (INET_REQ_GETOPTS, vec![INET_OPT_BIND_TO_DEVICE]),
            (INET_REQ_SETOPTS, netns),
            (INET_REQ_GETOPTS, vec![INET_LOPT_NETNS]),
            // A negative path length is the C's `return -1`; the stored path survives it.
            (INET_REQ_SETOPTS, record(INET_LOPT_NETNS, -1)),
            (INET_REQ_GETOPTS, vec![INET_LOPT_NETNS]),
            // The two membership codes are set-only: no get arm ⇒ the whole reply is refused.
            (INET_REQ_GETOPTS, vec![INET_OPT_ADD_MEMBERSHIP]),
            (INET_REQ_GETOPTS, vec![INET_OPT_DROP_MEMBERSHIP]),
        ];
        let named = rust_replies(driver, &sequence);
        let numeric = numeric_replies(driver, &sequence);
        assert_eq!(
            named, numeric,
            "the numeric opcode route must answer the named replies on {driver}"
        );
        assert_eq!(named[0], ctl_ok(), "`open` on {driver}");
        assert_eq!(named[1], ctl_ok(), "the membership write on {driver}");
        assert_eq!(named[2], ctl_ok(), "the membership drop on {driver}");
        assert_eq!(named[3], ctl_ok(), "the `bind_to_device` write on {driver}");
        assert_eq!(named[4], bind_reply, "the `bind_to_device` read-back on {driver}");
        assert_eq!(named[5], ctl_ok(), "the `netns` write on {driver}");
        assert_eq!(named[6], netns_reply, "the `netns` read-back on {driver}");
        assert_eq!(named[7], ctl_xerror("einval"), "a negative `netns` length on {driver}");
        assert_eq!(named[8], netns_reply, "the refused write stored nothing on {driver}");
        assert_eq!(named[9], ctl_xerror("einval"), "`getopts` of 14 on {driver}");
        assert_eq!(named[10], ctl_xerror("einval"), "`getopts` of 15 on {driver}");
        assert_agrees_with_oracle(driver, &sequence, &named);
    }
}

// ---------------------------------------------------------------------------
// (7) The `{packet, http | httph | http_bin | httph_bin | ssl_tls}` family
// ---------------------------------------------------------------------------

/// The five `{packet, …}` values whose decoders this floor does **not** implement (`packet.rs`
/// maps them onto `PacketLength::Unsupported`, `inet/packet.h`'s `TCP_PB_HTTP` family) are a
/// **documented, tested typed gap**: `setopts` accepts the code (`[1]`, as the C's `tcp_inet_ctl`
/// does) and the read path answers the typed `notsup` raise — never a wrong frame and never a
/// delivered byte. `frame_tests.rs` pins the same rule for `{packet, http}` alone; this row is the
/// whole family, driven through a real `tcp_inet` port over loopback.
#[test]
fn the_http_packet_family_is_a_typed_gap_never_a_wrong_frame() {
    use std::io::Write;
    use std::os::unix::io::IntoRawFd;
    use std::net::{TcpListener, TcpStream};

    for packet in [
        PacketType::Http,
        PacketType::HttpH,
        PacketType::HttpBin,
        PacketType::HttpHBin,
        PacketType::SslTls,
    ] {
        let mut poller = Poller::new().expect("a poll set");
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
        let address = listener.local_addr().expect("an address");
        let mut peer = TcpStream::connect(address).expect("the peer connects");
        let (server, _unused) = listener.accept().expect("accepts the connection");
        let driver =
            TcpInetDriver::from_socket(server.into_raw_fd()).expect("the driver adopts the socket");
        let mut ports = PortRegistry::new();
        let id = ports.open(&mut poller, Box::new(driver)).expect("the port opens");

        assert_eq!(
            ports
                .get_mut(id)
                .expect("the port is live")
                .control(&mut poller, "setopts", &record(INET_LOPT_PACKET, i32::from(packet.code())))
                .unwrap_or_else(|err| panic!("the `setopts` verb failed: {err:?}")),
            ctl_ok(),
            "the C accepts the {packet, {:?}} code",
            packet
        );
        peer.write_all(b"GET / HTTP/1.0\r\n\r\n")
            .expect("the peer writes a request");

        let err = ports
            .get_mut(id)
            .expect("the port is live")
            .control(&mut poller, "recv", b"")
            .expect_err("the HTTP family has no decoder at the driver layer");
        assert_eq!(err.as_atom(), Some("notsup"), "{packet:?}: got {err:?}");
        assert_eq!(
            ports.get_mut(id).expect("the port is live").drain_input(),
            Vec::<u8>::new(),
            "{packet:?} delivers nothing at all — a typed gap, never a wrong frame"
        );
        assert!(ports.close(&mut poller, id).is_ok(), "the port closes");
    }
}
