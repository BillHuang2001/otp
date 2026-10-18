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
