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

//! The **driver-state** option family, per port state, against the live oracle.
//!
//! [`super`] owns the codes the C keeps in the `inet`/`tcp`/`udp` descriptor instead of the kernel
//! (`inet_set_opts`, `inet_drv.c:6931`-`:7210`; `inet_fill_opts`, `:9141`-`:9320`). This suite
//! pins each one **through a real [`PortRegistry`](crate::port::PortRegistry) port** in every state
//! a port can be in, and byte-compares every reply with the real system driver:
//!
//! * **(a) no socket** — the port has never run `open`, so `desc->stype`/`sfamily`/`sprotocol` are
//!   the C's unset sentinels and every `stype`/protocol gate truncates its record;
//! * **(b) an opened stream socket** (`open_arg()`), where the stream and protocol gates open;
//! * **(c) post-`close`** — the C tears the descriptor's socket down but **not** its cached
//!   `stype`/`sprotocol`, so every gate still behaves exactly as in (b);
//! * **(d) an opened datagram socket** (`open_arg_dgram()` on `udp_inet`), where the datagram gate
//!   opens while the protocol gate stays shut — plus its post-`close` twin.
//!
//! A **bound, listening** `tcp_inet` port is the sixth case: it must report exactly the opened rows.
//! Its `bind` reply carries the kernel's ephemeral port, which necessarily differs between the two
//! sides, so that one case cross-checks every reply from the `listen` step on (the only comparison
//! here that is not whole-sequence).
//!
//! The set-side rows pin the store/report round trips, the refusals and the three **distinct** C
//! misses this family has to reproduce: `TRUNCATE_TO(0, ptr)` (a gate, a failed `getsockopt` or a
//! missing value contributes **nothing** and is *not* an error), `*ptr++ = opt; put_int32(0, …)`
//! (a literal zero where libc has no support — `non_block_send`) and `default: RETURN_ERROR()`
//! (the **whole** reply becomes `[0]einval`, never a partial one).
//!
//! The round trips for `{active, …}` (22), `{packet, …}` (23), `{mode, …}` (24), `{deliver, …}`
//! (25), `{send_timeout, …}` (30), `{packet_size, …}` (32), `{read_packets, N}` (33),
//! `{send_timeout_close, …}` (35) and the set-only `{line_delim, …}` (40) live in the sibling
//! `opt_tests.rs`; this suite pins them only where a **state** changes their reply, and drives the
//! codes that suite does not.
//!
//! # Stored and reported only
//!
//! `{high_msgq_watermark, …}` (36), `{low_msgq_watermark, …}` (37) and `{netns, Path}` (38) are
//! **stored and reported** faithfully — so their reported values are the oracle's — but nothing
//! enforces them: the frozen [`Driver`](crate::driver::Driver) seam has no busy-message-queue
//! limit to apply them to and no `setns(2)` to call. That is a documented capability gap of the
//! floor, not a divergence in the option surface.
//!
//! Every `erl`-dependent case **skips loudly** when `erl` is absent (via
//! [`assert_agrees_with_oracle`]) and none of them asserts a wall-clock value.

use super::super::oracle::*;
use super::super::{
    INET_LOPT_ACTIVE, INET_LOPT_BUFFER, INET_LOPT_DEBUG, INET_LOPT_DELIVER,
    INET_LOPT_EXITONCLOSE, INET_LOPT_HEADER, INET_LOPT_LINE_DELIM, INET_LOPT_MODE,
    INET_LOPT_MSGQ_HIWTRMRK, INET_LOPT_MSGQ_LOWTRMRK, INET_LOPT_NETNS, INET_LOPT_NON_BLOCK_SEND,
    INET_LOPT_PACKET, INET_LOPT_PACKET_SIZE, INET_LOPT_TCP_DELAY_SEND, INET_LOPT_TCP_HIWTRMRK,
    INET_LOPT_TCP_LOWTRMRK, INET_LOPT_TCP_READ_AHEAD, INET_LOPT_TCP_SEND_TIMEOUT,
    INET_LOPT_TCP_SEND_TIMEOUT_CLOSE, INET_LOPT_TCP_SHOW_ECONNRESET, INET_LOPT_UDP_READ_PACKETS,
    INET_OPT_EXCLUSIVEADDRUSE, INET_OPT_RCVBUF,
};
use crate::drivers::inet::wire::{ctl_ok, ctl_xerror};
use crate::drivers::inet::{DRIVER_NAME, INET_AF_INET, UDP_DRIVER_NAME};

// ---------------------------------------------------------------------------
// Shapes
// ---------------------------------------------------------------------------

/// One `(request code, payload)` step of a sequence both sides run.
type Request = (u32, Vec<u8>);

/// One oracle-pinned row: the `getopts` option code and the **whole** reply it produces.
type Row = (u8, Vec<u8>);

/// The `[1]`-tagged reply carrying one `[code, value_be32]` record.
fn one(code: u8, value: i32) -> Vec<u8> {
    let mut reply = ctl_ok();
    reply.extend_from_slice(&record(code, value));
    reply
}

/// The `[1]`-tagged reply carrying one boolean record — the C's `put_int32(!!(flag))`.
fn flag(code: u8, set: bool) -> Vec<u8> {
    one(code, i32::from(set))
}

/// `[1]` alone: a gated (or unset) code contributes **nothing** — the C's `TRUNCATE_TO(0, ptr)`,
/// which is *not* an error.
fn truncated() -> Vec<u8> {
    ctl_ok()
}

/// `[0]einval`: the whole reply, for a code the C's `return -1`/`RETURN_ERROR()` refuses.
fn einval() -> Vec<u8> {
    ctl_xerror("einval")
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// A `getopts` request asking for exactly one code.
fn get_request(code: u8) -> Request {
    (INET_REQ_GETOPTS, vec![code])
}

/// A `setopts` request carrying one `[code, value_be32]` record.
fn set_record(code: u8, value: i32) -> Request {
    (INET_REQ_SETOPTS, record(code, value))
}

/// A `setopts` request carrying a raw payload (several records, a path, or an extra counter).
fn set_payload(payload: &[u8]) -> Request {
    (INET_REQ_SETOPTS, payload.to_vec())
}

/// The `open` request for a stream socket.
fn open_stream() -> Request {
    (INET_REQ_OPEN, open_arg())
}

/// The `open` request for a datagram socket.
fn open_datagram() -> Request {
    (INET_REQ_OPEN, open_arg_dgram())
}

/// The `close` request.
fn close_request() -> Request {
    (INET_REQ_CLOSE, Vec::new())
}

/// The packed loopback address `bind` takes, with a zero port so the kernel picks the ephemeral
/// one (no fixed port is ever required, and two runs can never collide).
fn loopback_port_zero() -> Vec<u8> {
    vec![INET_AF_INET, 0, 0, 127, 0, 0, 1]
}

// ---------------------------------------------------------------------------
// The comparators
// ---------------------------------------------------------------------------

/// Drives one sequence through a real port, asserts **every** reply against the concrete expected
/// bytes (so the case still means something without `erl`) and then byte-compares the lot with the
/// live oracle.
fn check(driver: &str, sequence: &[Request], expected: &[Vec<u8>]) {
    assert_eq!(
        expected.len(),
        sequence.len(),
        "one expected reply per step"
    );
    let replies = rust_replies(driver, sequence);
    for (index, (reply, expected)) in replies.iter().zip(expected).enumerate() {
        let (code, arg) = &sequence[index];
        assert_eq!(
            reply, expected,
            "reply {index} (request code {code}, payload {arg:?}) on {driver}"
        );
    }
    assert_agrees_with_oracle(driver, sequence, &replies);
}

/// Runs a state's `getopts` row table — one request per row, in table order — on both sides.
///
/// The state's prefix steps are expected to answer the plain `"[1]"` (`open`/`close` both do); the
/// one prefix whose reply is not deterministic (a `bind`'s ephemeral port) is the listening case's
/// own test.
fn check_gets(driver: &str, prefix: &[Request], rows: &[Row]) {
    let sequence: Vec<Request> = prefix
        .iter()
        .cloned()
        .chain(rows.iter().map(|(code, _)| get_request(*code)))
        .collect();
    let expected: Vec<Vec<u8>> = std::iter::repeat_with(ctl_ok)
        .take(prefix.len())
        .chain(rows.iter().map(|(_, reply)| reply.clone()))
        .collect();
    check(driver, &sequence, &expected);
}

// ---------------------------------------------------------------------------
// The per-state row tables
// ---------------------------------------------------------------------------

/// **(a) No socket**: every `stype`/protocol gate is shut, so the stream-gated
/// `27`/`28`/`30`/`31`/`35` and the protocol-gated `39`/`49` truncate while the datagram-gated
/// `33` does too; the rest report the C's `inet_start` descriptor defaults
/// (`inet_drv.c:10890`-`:10976`): `bufsz` 9216, `hsz` 0, `htype` raw 0, `psize` 0, `mode` list 0,
/// `exitf` **1**, `deliver` term 1, `active` passive 0, `debug` 0, `high`/`low` 8192/4096, msgq
/// 8192/4096, `read_ahead` true.
fn bare_stream_rows() -> Vec<Row> {
    vec![
        // Non-Windows: `*ptr++ = opt; put_int32(0, …)` — a literal zero (`:9440`-`:9447`).
        (INET_OPT_EXCLUSIVEADDRUSE, one(INET_OPT_EXCLUSIVEADDRUSE, 0)),
        (INET_LOPT_BUFFER, one(INET_LOPT_BUFFER, 9216)),
        (INET_LOPT_HEADER, one(INET_LOPT_HEADER, 0)),
        (INET_LOPT_ACTIVE, one(INET_LOPT_ACTIVE, 0)),
        (INET_LOPT_PACKET, one(INET_LOPT_PACKET, 0)),
        (INET_LOPT_MODE, one(INET_LOPT_MODE, 0)),
        (INET_LOPT_DELIVER, one(INET_LOPT_DELIVER, 1)),
        (INET_LOPT_EXITONCLOSE, one(INET_LOPT_EXITONCLOSE, 1)),
        // The stream-gated group: `desc->stype` is unset, so `TRUNCATE_TO(0, ptr)`.
        (INET_LOPT_TCP_HIWTRMRK, truncated()),
        (INET_LOPT_TCP_LOWTRMRK, truncated()),
        (INET_LOPT_TCP_SEND_TIMEOUT, truncated()),
        (INET_LOPT_TCP_DELAY_SEND, truncated()),
        (INET_LOPT_PACKET_SIZE, one(INET_LOPT_PACKET_SIZE, 0)),
        (INET_LOPT_UDP_READ_PACKETS, truncated()),
        (INET_LOPT_TCP_SEND_TIMEOUT_CLOSE, truncated()),
        // The msgq watermarks have no gate (the C reads the *port's* live limits).
        (INET_LOPT_MSGQ_HIWTRMRK, one(INET_LOPT_MSGQ_HIWTRMRK, 8192)),
        (INET_LOPT_MSGQ_LOWTRMRK, one(INET_LOPT_MSGQ_LOWTRMRK, 4096)),
        // No path stored yet.
        (INET_LOPT_NETNS, truncated()),
        // `tcp_inet`'s `desc->sprotocol` is IPPROTO_TCP from the start, so the protocol gate is
        // *open* on this driver even before a socket exists (`:9298`-`:9320`).
        (INET_LOPT_TCP_SHOW_ECONNRESET, flag(INET_LOPT_TCP_SHOW_ECONNRESET, false)),
        // Set-only: `getopts` of `40` is the C's `default: RETURN_ERROR()`.
        (INET_LOPT_LINE_DELIM, einval()),
        (INET_LOPT_TCP_READ_AHEAD, flag(INET_LOPT_TCP_READ_AHEAD, true)),
        // SCTP-only flag: always false off SCTP.
        (INET_LOPT_NON_BLOCK_SEND, one(INET_LOPT_NON_BLOCK_SEND, 0)),
        (INET_LOPT_DEBUG, one(INET_LOPT_DEBUG, 0)),
    ]
}

/// **(b) An opened stream socket**: the stream-gated group un-truncates with the descriptor
/// defaults (`high`/`low` 8192/4096, `send_timeout` `INET_INFINITY` = −1, `delay_send` false), and
/// `39`/`49` report their flags. Everything else is the bare row.
fn opened_stream_rows() -> Vec<Row> {
    vec![
        (INET_OPT_EXCLUSIVEADDRUSE, one(INET_OPT_EXCLUSIVEADDRUSE, 0)),
        (INET_LOPT_BUFFER, one(INET_LOPT_BUFFER, 9216)),
        (INET_LOPT_HEADER, one(INET_LOPT_HEADER, 0)),
        (INET_LOPT_ACTIVE, one(INET_LOPT_ACTIVE, 0)),
        (INET_LOPT_PACKET, one(INET_LOPT_PACKET, 0)),
        (INET_LOPT_MODE, one(INET_LOPT_MODE, 0)),
        (INET_LOPT_DELIVER, one(INET_LOPT_DELIVER, 1)),
        (INET_LOPT_EXITONCLOSE, one(INET_LOPT_EXITONCLOSE, 1)),
        (INET_LOPT_TCP_HIWTRMRK, one(INET_LOPT_TCP_HIWTRMRK, 8192)),
        (INET_LOPT_TCP_LOWTRMRK, one(INET_LOPT_TCP_LOWTRMRK, 4096)),
        // `INET_INFINITY` is −1, reported as four `0xff` bytes.
        (INET_LOPT_TCP_SEND_TIMEOUT, one(INET_LOPT_TCP_SEND_TIMEOUT, -1)),
        (INET_LOPT_TCP_DELAY_SEND, flag(INET_LOPT_TCP_DELAY_SEND, false)),
        (INET_LOPT_PACKET_SIZE, one(INET_LOPT_PACKET_SIZE, 0)),
        // A stream socket is not a datagram, so `read_packets` stays truncated.
        (INET_LOPT_UDP_READ_PACKETS, truncated()),
        (INET_LOPT_TCP_SEND_TIMEOUT_CLOSE, one(INET_LOPT_TCP_SEND_TIMEOUT_CLOSE, 0)),
        (INET_LOPT_MSGQ_HIWTRMRK, one(INET_LOPT_MSGQ_HIWTRMRK, 8192)),
        (INET_LOPT_MSGQ_LOWTRMRK, one(INET_LOPT_MSGQ_LOWTRMRK, 4096)),
        (INET_LOPT_NETNS, truncated()),
        (INET_LOPT_TCP_SHOW_ECONNRESET, flag(INET_LOPT_TCP_SHOW_ECONNRESET, false)),
        (INET_LOPT_LINE_DELIM, einval()),
        (INET_LOPT_TCP_READ_AHEAD, flag(INET_LOPT_TCP_READ_AHEAD, true)),
        (INET_LOPT_NON_BLOCK_SEND, one(INET_LOPT_NON_BLOCK_SEND, 0)),
        (INET_LOPT_DEBUG, one(INET_LOPT_DEBUG, 0)),
    ]
}

/// **(d) An opened/bound datagram socket**: the protocol-TCP pair truncates (`udp_inet`'s
/// `desc->sprotocol` is IPPROTO_UDP) while the datagram gate opens `read_packets` at its default 5,
/// and the stream-gated group stays truncated. Everything else is the bare row.
fn opened_datagram_rows() -> Vec<Row> {
    vec![
        (INET_OPT_EXCLUSIVEADDRUSE, one(INET_OPT_EXCLUSIVEADDRUSE, 0)),
        (INET_LOPT_BUFFER, one(INET_LOPT_BUFFER, 9216)),
        (INET_LOPT_HEADER, one(INET_LOPT_HEADER, 0)),
        (INET_LOPT_ACTIVE, one(INET_LOPT_ACTIVE, 0)),
        (INET_LOPT_PACKET, one(INET_LOPT_PACKET, 0)),
        (INET_LOPT_MODE, one(INET_LOPT_MODE, 0)),
        (INET_LOPT_DELIVER, one(INET_LOPT_DELIVER, 1)),
        (INET_LOPT_EXITONCLOSE, one(INET_LOPT_EXITONCLOSE, 1)),
        (INET_LOPT_TCP_HIWTRMRK, truncated()),
        (INET_LOPT_TCP_LOWTRMRK, truncated()),
        (INET_LOPT_TCP_SEND_TIMEOUT, truncated()),
        (INET_LOPT_TCP_DELAY_SEND, truncated()),
        (INET_LOPT_PACKET_SIZE, one(INET_LOPT_PACKET_SIZE, 0)),
        (INET_LOPT_UDP_READ_PACKETS, one(INET_LOPT_UDP_READ_PACKETS, 5)),
        (INET_LOPT_TCP_SEND_TIMEOUT_CLOSE, truncated()),
        (INET_LOPT_MSGQ_HIWTRMRK, one(INET_LOPT_MSGQ_HIWTRMRK, 8192)),
        (INET_LOPT_MSGQ_LOWTRMRK, one(INET_LOPT_MSGQ_LOWTRMRK, 4096)),
        (INET_LOPT_NETNS, truncated()),
        (INET_LOPT_TCP_SHOW_ECONNRESET, truncated()),
        (INET_LOPT_LINE_DELIM, einval()),
        (INET_LOPT_TCP_READ_AHEAD, truncated()),
        (INET_LOPT_NON_BLOCK_SEND, one(INET_LOPT_NON_BLOCK_SEND, 0)),
        (INET_LOPT_DEBUG, one(INET_LOPT_DEBUG, 0)),
    ]
}

// ---------------------------------------------------------------------------
// The per-state getopts rows
// ---------------------------------------------------------------------------

/// **(a)** A port that has never opened a socket.
#[test]
fn the_bare_stream_port_reports_the_descriptor_defaults() {
    check_gets(DRIVER_NAME, &[], &bare_stream_rows());
}

/// **(b)** An opened stream socket: the stream- and protocol-gated rows come alive.
#[test]
fn the_opened_stream_port_untruncates_the_stream_gated_rows() {
    check_gets(DRIVER_NAME, &[open_stream()], &opened_stream_rows());
}

/// **(c)** Post-`close`: the C clears the socket but keeps `stype`/`sprotocol`, so **every** row
/// is still the opened row — the gates do not reopen or close, and the stored values survive.
#[test]
fn the_post_close_stream_port_keeps_the_cached_socket_state() {
    check_gets(
        DRIVER_NAME,
        &[open_stream(), close_request()],
        &opened_stream_rows(),
    );
}

/// **(d)** A bare datagram port: `sprotocol` is IPPROTO_UDP, so the protocol-TCP pair truncates
/// here while it reports a flag on `tcp_inet` — the one row where the two drivers differ before a
/// socket exists.
#[test]
fn the_bare_datagram_port_truncates_the_protocol_gated_pair() {
    let mut rows = bare_stream_rows();
    for (code, reply) in rows.iter_mut() {
        if *code == INET_LOPT_TCP_SHOW_ECONNRESET || *code == INET_LOPT_TCP_READ_AHEAD {
            *reply = truncated();
        }
    }
    check_gets(UDP_DRIVER_NAME, &[], &rows);
}

/// **(d)** An opened datagram socket: `read_packets` un-truncates at its default 5.
#[test]
fn the_opened_datagram_port_serves_read_packets() {
    check_gets(
        UDP_DRIVER_NAME,
        &[open_datagram()],
        &opened_datagram_rows(),
    );
}

/// **(d)** Post-`close` on a datagram port: the cached datagram type survives, so `read_packets`
/// still reports and the protocol pair still truncates.
#[test]
fn the_post_close_datagram_port_keeps_the_cached_socket_state() {
    check_gets(
        UDP_DRIVER_NAME,
        &[open_datagram(), close_request()],
        &opened_datagram_rows(),
    );
}

/// **(f)** A bound, listening stream port reports exactly the opened rows: none of these codes
/// reads the descriptor's connection state, only its cached socket type/protocol.
///
/// The `bind` reply carries the kernel's ephemeral port, which cannot agree between two
/// processes, so the oracle comparison here starts at the deterministic `listen` reply.
#[test]
fn the_listening_stream_port_reports_the_opened_rows() {
    let prefix = [
        open_stream(),
        (INET_REQ_BIND, loopback_port_zero()),
        (INET_REQ_LISTEN, vec![0, 5]),
    ];
    let rows = opened_stream_rows();
    let sequence: Vec<Request> = prefix
        .iter()
        .cloned()
        .chain(rows.iter().map(|(code, _)| get_request(*code)))
        .collect();
    let replies = rust_replies(DRIVER_NAME, &sequence);

    assert_eq!(replies[0], ctl_ok(), "open");
    assert_eq!(replies[1][0], 1, "bind answers `[1] ++ port`: {:?}", replies[1]);
    assert_eq!(replies[1].len(), 3, "bind carries the ephemeral port");
    assert_eq!(replies[2], ctl_ok(), "listen");
    for ((code, expected), reply) in rows.iter().zip(&replies[prefix.len()..]) {
        assert_eq!(reply, expected, "a listener's `getopts` of code {code}");
    }

    let Some(stdout) = erl_stdout(&oracle_expression(DRIVER_NAME, &sequence)) else {
        return; // the oracle is unavailable; it skipped loudly above
    };
    let Some(oracle) = parse_reply_dump(&stdout) else {
        panic!("could not parse the oracle reply dump: {stdout:?}");
    };
    assert_eq!(oracle.len(), sequence.len(), "one reply per step");
    // Skip the `bind` reply (index 1) and `open` (index 0); every later reply is deterministic.
    for (index, (ours, theirs)) in replies.iter().zip(&oracle).enumerate().skip(2) {
        assert_eq!(ours, theirs, "reply {index} disagrees");
    }
}

// ---------------------------------------------------------------------------
// The setopts rows
// ---------------------------------------------------------------------------

/// The store/report round trips for the codes whose **value** is all there is: `{header, …}` (21),
/// `{exit_on_close, …}` (26), the two msgq watermarks (36/37), `{netns, Path}` (38), the SCTP-only
/// `non_block_send` (50) and `{debug, …}` (99), plus the `{buffer, N}` (20) clamp to
/// `INET_BUFFER_MIN`.
#[test]
fn the_state_only_codes_store_and_report_their_values() {
    // `-1`/`5` for `debug`/`exit_on_close` pin that the value is reported **as stored** where the
    // C stores it verbatim, and normalised to `0`/`1` where it does not.
    let sequence = vec![
        set_record(INET_LOPT_HEADER, 12),
        get_request(INET_LOPT_HEADER),
        set_record(INET_LOPT_HEADER, -1),
        get_request(INET_LOPT_HEADER),
        set_record(INET_LOPT_EXITONCLOSE, 0),
        get_request(INET_LOPT_EXITONCLOSE),
        set_record(INET_LOPT_EXITONCLOSE, 5),
        get_request(INET_LOPT_EXITONCLOSE),
        set_record(INET_LOPT_MSGQ_HIWTRMRK, 16384),
        get_request(INET_LOPT_MSGQ_HIWTRMRK),
        set_record(INET_LOPT_MSGQ_LOWTRMRK, 1),
        get_request(INET_LOPT_MSGQ_LOWTRMRK),
        set_record(INET_LOPT_NON_BLOCK_SEND, 1),
        get_request(INET_LOPT_NON_BLOCK_SEND),
        // `desc->debug = ival ? TRUE : FALSE` — any non-zero value reports `1`.
        set_record(INET_LOPT_DEBUG, -1),
        get_request(INET_LOPT_DEBUG),
        set_record(INET_LOPT_DEBUG, 0),
        get_request(INET_LOPT_DEBUG),
        // `if (ival < INET_BUFFER_MIN) ival = INET_BUFFER_MIN;` — 0 and a negative value clamp to 1.
        set_record(INET_LOPT_BUFFER, 0),
        get_request(INET_LOPT_BUFFER),
        set_record(INET_LOPT_BUFFER, -5),
        get_request(INET_LOPT_BUFFER),
    ];
    let expected = vec![
        ctl_ok(),
        one(INET_LOPT_HEADER, 12),
        ctl_ok(),
        one(INET_LOPT_HEADER, -1),
        ctl_ok(),
        one(INET_LOPT_EXITONCLOSE, 0),
        ctl_ok(),
        one(INET_LOPT_EXITONCLOSE, 5),
        ctl_ok(),
        one(INET_LOPT_MSGQ_HIWTRMRK, 16384),
        ctl_ok(),
        one(INET_LOPT_MSGQ_LOWTRMRK, 1),
        ctl_ok(),
        // Accepted and ignored: the C's `continue` for a non-SCTP socket (`:7870`-`:7896`).
        one(INET_LOPT_NON_BLOCK_SEND, 0),
        ctl_ok(),
        one(INET_LOPT_DEBUG, 1),
        ctl_ok(),
        one(INET_LOPT_DEBUG, 0),
        ctl_ok(),
        one(INET_LOPT_BUFFER, 1),
        ctl_ok(),
        one(INET_LOPT_BUFFER, 1),
    ];
    check(DRIVER_NAME, &sequence, &expected);
}

/// The busy-msgq pair refuses a value below `ERL_DRV_BUSY_MSGQ_LIM_MIN` (1) with the **whole**
/// reply `[0]einval` and stores nothing, while `ERL_DRV_BUSY_MSGQ_LIM_MAX` (`SIZE_MAX - 1`) can
/// never trip on a 32-bit `ival`. An accepted value is stored by the emulator's
/// `erl_drv_busy_msgq_limits` (`erts/emulator/beam/erl_port_task.c:772`), which cross-clamps the
/// pair so `low <= high` always holds.
#[test]
fn the_msgq_watermarks_refuse_a_value_below_one() {
    let sequence = vec![
        set_record(INET_LOPT_MSGQ_HIWTRMRK, 0),
        get_request(INET_LOPT_MSGQ_HIWTRMRK),
        get_request(INET_LOPT_MSGQ_LOWTRMRK),
        set_record(INET_LOPT_MSGQ_LOWTRMRK, 0),
        get_request(INET_LOPT_MSGQ_LOWTRMRK),
        set_record(INET_LOPT_MSGQ_LOWTRMRK, -1),
        get_request(INET_LOPT_MSGQ_HIWTRMRK),
        // A refusal stores nothing: both watermarks are still at their start-up defaults.
        get_request(INET_LOPT_MSGQ_LOWTRMRK),
        // Both codes **cross-clamp the pair** (the emulator's `erl_drv_busy_msgq_limits`,
        // `erts/emulator/beam/erl_port_task.c:800`-`:820`): lowering `high` to `1` drags `low`
        // down with it, so the pair never inverts.
        set_record(INET_LOPT_MSGQ_HIWTRMRK, 1),
        get_request(INET_LOPT_MSGQ_HIWTRMRK),
        get_request(INET_LOPT_MSGQ_LOWTRMRK),
        // ... and raising `low` again lifts `high` with it.
        set_record(INET_LOPT_MSGQ_LOWTRMRK, 20000),
        get_request(INET_LOPT_MSGQ_HIWTRMRK),
        get_request(INET_LOPT_MSGQ_LOWTRMRK),
    ];
    let expected = vec![
        einval(),
        one(INET_LOPT_MSGQ_HIWTRMRK, 8192),
        one(INET_LOPT_MSGQ_LOWTRMRK, 4096),
        einval(),
        one(INET_LOPT_MSGQ_LOWTRMRK, 4096),
        einval(),
        one(INET_LOPT_MSGQ_HIWTRMRK, 8192),
        one(INET_LOPT_MSGQ_LOWTRMRK, 4096),
        ctl_ok(),
        one(INET_LOPT_MSGQ_HIWTRMRK, 1),
        one(INET_LOPT_MSGQ_LOWTRMRK, 1),
        ctl_ok(),
        one(INET_LOPT_MSGQ_HIWTRMRK, 20000),
        one(INET_LOPT_MSGQ_LOWTRMRK, 20000),
    ];
    check(DRIVER_NAME, &sequence, &expected);
}

/// The two stream watermarks **cross-clamp** each other (`inet_drv.c:7050`-`:7085`): raising
/// `high` above `low` pulls `low` up with it, lowering `high` below `low` drags `low` down, and a
/// negative value clamps to 0 (which then drags the partner down too).
#[test]
fn the_stream_watermarks_cross_clamp() {
    let sequence = vec![
        open_stream(),
        // `low` (4096) is above the new `high`, so the C pulls `low` down to it.
        set_record(INET_LOPT_TCP_HIWTRMRK, 100),
        get_request(INET_LOPT_TCP_HIWTRMRK),
        get_request(INET_LOPT_TCP_LOWTRMRK),
        // Now `high` is below the new `low`, so `high` follows it up.
        set_record(INET_LOPT_TCP_LOWTRMRK, 20000),
        get_request(INET_LOPT_TCP_LOWTRMRK),
        get_request(INET_LOPT_TCP_HIWTRMRK),
        // A negative value clamps to 0, and `low > 0` so `low` becomes 0 as well.
        set_record(INET_LOPT_TCP_HIWTRMRK, -5),
        get_request(INET_LOPT_TCP_HIWTRMRK),
        get_request(INET_LOPT_TCP_LOWTRMRK),
    ];
    let expected = vec![
        ctl_ok(),
        ctl_ok(),
        one(INET_LOPT_TCP_HIWTRMRK, 100),
        one(INET_LOPT_TCP_LOWTRMRK, 100),
        ctl_ok(),
        one(INET_LOPT_TCP_LOWTRMRK, 20000),
        one(INET_LOPT_TCP_HIWTRMRK, 20000),
        ctl_ok(),
        one(INET_LOPT_TCP_HIWTRMRK, 0),
        one(INET_LOPT_TCP_LOWTRMRK, 0),
    ];
    check(DRIVER_NAME, &sequence, &expected);
}

/// The stream gates are **not** refusals: `setopts` of a stream-gated code on a socket-less port
/// answers `[1]` and stores nothing, so the value only becomes visible once a stream socket
/// exists — and the C's `stype` is what gates it, not the live socket.
#[test]
fn the_stream_gated_rows_ignore_a_value_without_a_stream_socket() {
    let sequence = vec![
        // (a) no socket: every stream-gated set is accepted and dropped.
        set_record(INET_LOPT_TCP_HIWTRMRK, 100),
        set_record(INET_LOPT_TCP_LOWTRMRK, 100),
        set_record(INET_LOPT_TCP_SEND_TIMEOUT, 5000),
        set_record(INET_LOPT_TCP_DELAY_SEND, 1),
        set_record(INET_LOPT_TCP_SEND_TIMEOUT_CLOSE, 1),
        get_request(INET_LOPT_TCP_HIWTRMRK),
        get_request(INET_LOPT_TCP_SEND_TIMEOUT),
        get_request(INET_LOPT_TCP_SEND_TIMEOUT_CLOSE),
        // (b) opened: the same sets are stored.
        open_stream(),
        set_record(INET_LOPT_TCP_SEND_TIMEOUT, 5000),
        get_request(INET_LOPT_TCP_SEND_TIMEOUT),
        set_record(INET_LOPT_TCP_SEND_TIMEOUT_CLOSE, 1),
        get_request(INET_LOPT_TCP_SEND_TIMEOUT_CLOSE),
        // A negative `send_timeout` is stored verbatim (`INET_INFINITY` is exactly −1).
        set_record(INET_LOPT_TCP_SEND_TIMEOUT, -3),
        get_request(INET_LOPT_TCP_SEND_TIMEOUT),
        // (c) post-close: the cached stream type keeps the gate open.
        close_request(),
        get_request(INET_LOPT_TCP_SEND_TIMEOUT),
        get_request(INET_LOPT_TCP_HIWTRMRK),
    ];
    let expected = vec![
        ctl_ok(),
        ctl_ok(),
        ctl_ok(),
        ctl_ok(),
        ctl_ok(),
        truncated(),
        truncated(),
        truncated(),
        ctl_ok(),
        ctl_ok(),
        one(INET_LOPT_TCP_SEND_TIMEOUT, 5000),
        ctl_ok(),
        one(INET_LOPT_TCP_SEND_TIMEOUT_CLOSE, 1),
        ctl_ok(),
        one(INET_LOPT_TCP_SEND_TIMEOUT, -3),
        ctl_ok(),
        one(INET_LOPT_TCP_SEND_TIMEOUT, -3),
        // `high`/`low` were never stored (the bare-port sets were dropped), so the defaults stand.
        one(INET_LOPT_TCP_HIWTRMRK, 8192),
    ];
    check(DRIVER_NAME, &sequence, &expected);
}

/// `{delay_send, …}` is a **flag**, not a value: `2` reports `1` and `0` reports `0`
/// (`inet_drv.c:7122`-`:7136`, `:9256`-`:9266`).
#[test]
fn the_delay_send_row_reports_a_boolean() {
    let sequence = vec![
        open_stream(),
        set_record(INET_LOPT_TCP_DELAY_SEND, 2),
        get_request(INET_LOPT_TCP_DELAY_SEND),
        set_record(INET_LOPT_TCP_DELAY_SEND, 0),
        get_request(INET_LOPT_TCP_DELAY_SEND),
        set_record(INET_LOPT_TCP_DELAY_SEND, -1),
        get_request(INET_LOPT_TCP_DELAY_SEND),
    ];
    let expected = vec![
        ctl_ok(),
        ctl_ok(),
        flag(INET_LOPT_TCP_DELAY_SEND, true),
        ctl_ok(),
        flag(INET_LOPT_TCP_DELAY_SEND, false),
        ctl_ok(),
        flag(INET_LOPT_TCP_DELAY_SEND, true),
    ];
    check(DRIVER_NAME, &sequence, &expected);
}

/// The protocol-TCP pair (`39` `show_econnreset`, `49` `read_ahead`) is gated on
/// `desc->sprotocol`, not on the socket type: a `udp_inet` port truncates both and silently drops
/// their `setopts`, while a `tcp_inet` port stores and reports them — on a bare port too, because
/// `sprotocol` is set at start-up rather than at `open`.
#[test]
fn the_protocol_gated_pair_truncates_on_a_datagram_port() {
    let datagram = vec![
        open_datagram(),
        set_record(INET_LOPT_TCP_SHOW_ECONNRESET, 1),
        get_request(INET_LOPT_TCP_SHOW_ECONNRESET),
        set_record(INET_LOPT_TCP_READ_AHEAD, 0),
        get_request(INET_LOPT_TCP_READ_AHEAD),
        // ... and the same on a bare datagram port.
        close_request(),
        set_record(INET_LOPT_TCP_SHOW_ECONNRESET, 1),
        get_request(INET_LOPT_TCP_SHOW_ECONNRESET),
        get_request(INET_LOPT_TCP_READ_AHEAD),
    ];
    let datagram_expected = vec![
        ctl_ok(),
        ctl_ok(),
        truncated(),
        ctl_ok(),
        truncated(),
        ctl_ok(),
        ctl_ok(),
        truncated(),
        truncated(),
    ];
    check(UDP_DRIVER_NAME, &datagram, &datagram_expected);

    let stream = vec![
        set_record(INET_LOPT_TCP_SHOW_ECONNRESET, 1),
        get_request(INET_LOPT_TCP_SHOW_ECONNRESET),
        set_record(INET_LOPT_TCP_READ_AHEAD, 0),
        get_request(INET_LOPT_TCP_READ_AHEAD),
    ];
    let stream_expected = vec![
        ctl_ok(),
        flag(INET_LOPT_TCP_SHOW_ECONNRESET, true),
        ctl_ok(),
        flag(INET_LOPT_TCP_READ_AHEAD, false),
    ];
    check(DRIVER_NAME, &stream, &stream_expected);
}

/// `{netns, Path}` (38) stores the `ival` bytes that follow its record and reports them back as
/// `[opt][len_be32][path bytes]`; a path longer than the payload is the C's `return -1`, and an
/// empty path is a stored, empty value rather than "unset".
#[test]
fn the_netns_path_rides_the_record() {
    let mut long = vec![INET_LOPT_NETNS, 0, 0, 0, 9];
    long.extend_from_slice(b"foo");
    let sequence = vec![
        set_payload(&[INET_LOPT_NETNS, 0, 0, 0, 3, b'f', b'o', b'o']),
        get_request(INET_LOPT_NETNS),
        // The path is longer than the bytes that follow the record.
        set_payload(&long),
        get_request(INET_LOPT_NETNS),
        // A negative length is refused too (`if (ival < 0) return -1`).
        set_record(INET_LOPT_NETNS, -1),
        get_request(INET_LOPT_NETNS),
        // An empty path is stored: the reply is a record with a zero length, not a truncation.
        set_payload(&[INET_LOPT_NETNS, 0, 0, 0, 0]),
        get_request(INET_LOPT_NETNS),
    ];
    let expected = vec![
        ctl_ok(),
        one_path(b"foo"),
        einval(),
        one_path(b"foo"),
        einval(),
        one_path(b"foo"),
        ctl_ok(),
        one_path(b""),
    ];
    check(DRIVER_NAME, &sequence, &expected);
}

/// The `netns` reply for `path` — `[1, 38, len_be32, path bytes]`.
fn one_path(path: &[u8]) -> Vec<u8> {
    let mut reply = ctl_ok();
    reply.push(INET_LOPT_NETNS);
    reply.extend_from_slice(&(path.len() as u32).to_be_bytes());
    reply.extend_from_slice(path);
    reply
}

/// `setopts` of `{line_delim, …}` (40) is served and answered `[1]`, but `getopts` of it is the C's
/// `default: RETURN_ERROR()` — the **whole** reply is `[0]einval`, dropping any record that came
/// before it.
#[test]
fn the_line_delim_set_only_row_refuses_the_whole_getopts_reply() {
    let sequence = vec![
        set_record(INET_LOPT_LINE_DELIM, 10),
        get_request(INET_LOPT_LINE_DELIM),
        // A served code ahead of it does not survive: the buffer is discarded whole.
        (INET_REQ_GETOPTS, vec![INET_LOPT_ACTIVE, INET_LOPT_LINE_DELIM]),
    ];
    let expected = vec![ctl_ok(), einval(), einval()];
    check(DRIVER_NAME, &sequence, &expected);
}

/// A code this family does not serve refuses **both** verbs on **every** driver with the whole
/// reply `[0]einval` — never a partial reply and never a fabricated value.
#[test]
fn an_unserved_code_refuses_both_verbs_on_every_driver() {
    let sequence = vec![
        (INET_REQ_GETOPTS, vec![200]),
        (INET_REQ_SETOPTS, vec![200, 0, 0, 0, 1]),
        // `default: RETURN_ERROR()` refuses the **whole reply**, but the records the walk already
        // passed keep their effect — here `header` is stored before code `200` is reached.
        (INET_REQ_SETOPTS, vec![INET_LOPT_HEADER, 0, 0, 0, 7, 200, 0, 0, 0, 1]),
        get_request(INET_LOPT_HEADER),
    ];
    let expected = vec![einval(), einval(), einval(), one(INET_LOPT_HEADER, 7)];
    check(DRIVER_NAME, &sequence, &expected);
    check(UDP_DRIVER_NAME, &sequence, &expected);
}

/// `{read_packets, N}` (33) is datagram-gated *including* its `ival <= 0` refusal: a stream port
/// answers `[1]` to both `set 33 0` and `set 33 7` (storing nothing), while a datagram port serves
/// `7` and refuses `0`/a negative value with the whole reply — leaving the stored value alone.
#[test]
fn the_read_packets_refusal_sits_inside_the_datagram_gate() {
    let stream = vec![
        set_record(INET_LOPT_UDP_READ_PACKETS, 7),
        get_request(INET_LOPT_UDP_READ_PACKETS),
        set_record(INET_LOPT_UDP_READ_PACKETS, 0),
        get_request(INET_LOPT_UDP_READ_PACKETS),
    ];
    let stream_expected = vec![ctl_ok(), truncated(), ctl_ok(), truncated()];
    check(DRIVER_NAME, &stream, &stream_expected);

    let datagram = vec![
        open_datagram(),
        get_request(INET_LOPT_UDP_READ_PACKETS),
        set_record(INET_LOPT_UDP_READ_PACKETS, 7),
        get_request(INET_LOPT_UDP_READ_PACKETS),
        // The refusal sits *inside* the gate, so it replaces the whole reply here.
        set_record(INET_LOPT_UDP_READ_PACKETS, 0),
        get_request(INET_LOPT_UDP_READ_PACKETS),
        set_record(INET_LOPT_UDP_READ_PACKETS, -3),
        get_request(INET_LOPT_UDP_READ_PACKETS),
        // A refused record stores nothing: the last accepted value (7) still stands.
        close_request(),
        get_request(INET_LOPT_UDP_READ_PACKETS),
    ];
    let datagram_expected = vec![
        ctl_ok(),
        one(INET_LOPT_UDP_READ_PACKETS, 5),
        ctl_ok(),
        one(INET_LOPT_UDP_READ_PACKETS, 7),
        einval(),
        one(INET_LOPT_UDP_READ_PACKETS, 7),
        einval(),
        one(INET_LOPT_UDP_READ_PACKETS, 7),
        ctl_ok(),
        one(INET_LOPT_UDP_READ_PACKETS, 7),
    ];
    check(UDP_DRIVER_NAME, &datagram, &datagram_expected);
}

/// `INET_FLG_BUFFER_SET` couples `{buffer, N}` (20) to `{recbuf, N}` (7): an explicit `{buffer, …}`
/// pins the reported size against every later `recbuf`, while without it a `recbuf` **larger** than
/// the buffer raises the buffer (`inet_drv.c:7319`-`:7356`).
///
/// The `7` arm itself is the syscall family's (the sibling `opt_sys_tests.rs`); what is pinned here
/// is only what `getopts` of `20` reports afterwards, which is this module's stored value.
#[test]
fn the_buffer_flag_pins_the_size_against_recbuf() {
    // No explicit `{buffer, …}`: a larger `recbuf` raises the reported buffer (9216 → 20000).
    let raised = vec![
        set_record(INET_OPT_RCVBUF, 20000),
        get_request(INET_LOPT_BUFFER),
        // A *smaller* `recbuf` never lowers it.
        set_record(INET_OPT_RCVBUF, 4096),
        get_request(INET_LOPT_BUFFER),
    ];
    let raised_expected = vec![
        ctl_ok(),
        one(INET_LOPT_BUFFER, 20000),
        ctl_ok(),
        one(INET_LOPT_BUFFER, 20000),
    ];
    check(DRIVER_NAME, &raised, &raised_expected);

    // An explicit `{buffer, 100}` sets the flag, so the `recbuf` arm no longer touches it.
    let pinned = vec![
        set_record(INET_LOPT_BUFFER, 100),
        set_record(INET_OPT_RCVBUF, 4096),
        get_request(INET_LOPT_BUFFER),
        set_record(INET_OPT_RCVBUF, 65536),
        get_request(INET_LOPT_BUFFER),
    ];
    let pinned_expected = vec![
        ctl_ok(),
        ctl_ok(),
        one(INET_LOPT_BUFFER, 100),
        ctl_ok(),
        one(INET_LOPT_BUFFER, 100),
    ];
    check(DRIVER_NAME, &pinned, &pinned_expected);
}

/// The `{active, N}` **counter** rides the record: `setopts` consumes the trailing big-endian
/// 16-bit `N`, `getopts` echoes it back, and the running total accumulates across records. A sum
/// that leaves the signed 16-bit range is the whole reply `[0]einval` and stores nothing (the C
/// has already written `desc->active` at that point), a negative total clamps to zero and falls
/// back to `{active, false}`, and any non-multi form zeroes the counter.
///
/// The plain `{active, …}` round trips are `opt_tests.rs`'s; this case adds the refusal and the
/// accumulation against the live oracle.
#[test]
fn the_active_multi_counter_accumulates_and_refuses_out_of_range() {
    // The record with a trailing counter: `[22, active_be32, n_be16]`.
    let multi = |n: i16| {
        let mut payload = record(INET_LOPT_ACTIVE, 3);
        payload.extend_from_slice(&n.to_be_bytes());
        set_payload(&payload)
    };
    let sequence = vec![
        open_stream(),
        multi(7),
        get_request(INET_LOPT_ACTIVE),
        // The second multi record **adds** to the running total.
        multi(7),
        get_request(INET_LOPT_ACTIVE),
        multi(30000),
        get_request(INET_LOPT_ACTIVE),
        // 30014 + 30000 leaves the signed 16-bit range: the whole reply is `[0]einval` ...
        multi(30000),
        // ... and the stored total is untouched.
        get_request(INET_LOPT_ACTIVE),
        // Any non-multi form zeroes the counter, so the record carries no trailing bytes.
        set_record(INET_LOPT_ACTIVE, 0),
        get_request(INET_LOPT_ACTIVE),
        set_record(INET_LOPT_ACTIVE, 2),
        get_request(INET_LOPT_ACTIVE),
        // A negative `N` from a zero counter clamps to zero and falls back to `{active, false}`.
        multi(-5),
        get_request(INET_LOPT_ACTIVE),
    ];
    let active = |value: i32, count: i16| {
        let mut reply = one(INET_LOPT_ACTIVE, value);
        reply.extend_from_slice(&count.to_be_bytes());
        reply
    };
    let expected = vec![
        ctl_ok(),
        ctl_ok(),
        active(3, 7),
        ctl_ok(),
        active(3, 14),
        ctl_ok(),
        active(3, 30014),
        einval(),
        active(3, 30014),
        ctl_ok(),
        one(INET_LOPT_ACTIVE, 0),
        ctl_ok(),
        one(INET_LOPT_ACTIVE, 2),
        ctl_ok(),
        one(INET_LOPT_ACTIVE, 0),
    ];
    check(DRIVER_NAME, &sequence, &expected);
}
