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
//! The **syscall / special-shape** option family, per code, in every port state, against the live
//! oracle.
//!
//! [`super`] owns every `setopts`/`getopts` code that reaches the kernel through
//! `setsockopt(2)`/`getsockopt(2)`: the plain `int` records, the four codes whose value is not a
//! plain `int` (`linger`, `bind_to_device`, `raw`, and the two family-selected reads `multicast_if`
//! and `pktoptions`), the `propagate = 1` refusals and the three distinct C misses
//! (`inet_drv.c:874`-`:930` the code table, `:6900`-`:7942` the set switch, `:9066`-`:9757` the
//! get switch).
//!
//! Every row is driven **through a real [`PortRegistry`](crate::port::PortRegistry) port** — the
//! same path the runtime takes — and byte-compared with the real system driver, so a row pins both
//! our behaviour and the oracle's. The states are:
//!
//! * **(a) no socket** — the port has never run `open`, so `desc->s` is invalid and `desc->sfamily`
//!   is the C's unset sentinel. Every `getsockopt` fails and is dropped (the C's
//!   `TRUNCATE_TO(0, ptr); continue;`, which contributes **nothing** and is *not* an error), and
//!   every non-propagating `setsockopt` failure is likewise discarded. The family-gated codes
//!   (`multicast_if` `11`, `pktoptions` `45`) are the C's `RETURN_ERROR()`, i.e. `[0]einval`, and so
//!   are the nine `propagate = 1` codes (`8, 9, 16, 41, 42, 43, 44, 46, 47`).
//! * **(b) an opened stream socket** (`open_arg()`), where every `int` code resolves to a real
//!   `getsockopt` value.
//! * **(c) an opened datagram socket** (`open_arg_dgram()` on `udp_inet`), where the TCP-only options
//!   (`nodelay` `10`, `nopush` `48`, the `keep*`/`user_timeout` family) fail and are dropped and
//!   `multicast_ttl` (`12`) is not clamped to `1` as it is on a stream socket.
//! * **(d) post-`close`** — the socket is gone but the **cached** `sfamily` survives
//!   (`inet_drv.c`'s `desc->sfamily` is not cleared by `close`), so `multicast_if` (`11`) still
//!   answers its family record while `pktoptions` (`45`) — which passed the family gate and then
//!   failed `getsockopt` — drops its record instead of refusing.
//!
//! # The three C misses, preserved and pinned
//!
//! `TRUNCATE_TO(0, ptr)` = the record contributes nothing and is *not* an error (a family/socket
//! gate or a failed `getsockopt`); `*ptr++ = opt; put_int32(0, …)` = a literal `0` for a code the
//! platform does not implement (non-Windows `exclusiveaddruse` `19`, the only one in this family
//! that reports on a socket-less port); `default: RETURN_ERROR()` = the **whole** reply becomes
//! `[0]einval` (the set-only multicast-membership pair `14`/`15` on the get side).
//!
//! # Deliberate non-mirrors (documented, never fabricated)
//!
//! * `multicast_if` (`11`)'s read-back reserves nine reply bytes but advances its length by only
//!   five, so the four value bytes are **uninitialized memory** in the C and vary run to run.
//!   [`super`] answers a deterministic zero there; the row below pins the gate, the length and the
//!   family word, never the value bytes.
//! * a **short multicast-membership** (`14`/`15`) payload is read past the end of the request by the
//!   C; this floor answers the C's own `[0]einval` instead. That is a Rust-only row (it cannot be
//!   compared with an oracle that reads uninitialized bytes).
//! * `sndbuf` (`6`) and `recbuf` (`7`) report the **kernel's** buffer sizes (and Linux doubles a
//!   requested size), so their values are host-dependent: those rows assert the record's shape and
//!   `sndbuf`/`recbuf` equality with the oracle, never a hard-coded number.
//!
//! Every `erl`-dependent case skips **loudly** when `erl` is absent (via
//! [`assert_agrees_with_oracle`] and [`erl_stdout`]) and none of them asserts a wall-clock value.
use super::super::oracle::*;
use super::super::{
    INET_LOPT_BUFFER, INET_OPT_ADD_MEMBERSHIP, INET_OPT_BIND_TO_DEVICE, INET_OPT_BROADCAST,
    INET_OPT_DONTROUTE, INET_OPT_DROP_MEMBERSHIP, INET_OPT_EXCLUSIVEADDRUSE,
    INET_OPT_IPV6_V6ONLY, INET_OPT_KEEPALIVE, INET_OPT_KEEPCNT, INET_OPT_KEEPIDLE,
    INET_OPT_KEEPINTVL, INET_OPT_LINGER, INET_OPT_MULTICAST_IF, INET_OPT_MULTICAST_LOOP,
    INET_OPT_MULTICAST_TTL, INET_OPT_NOPUSH, INET_OPT_OOBINLINE, INET_OPT_PKTOPTIONS,
    INET_OPT_PRIORITY, INET_OPT_RAW, INET_OPT_RCVBUF, INET_OPT_RECVTCLASS, INET_OPT_RECVTOS,
    INET_OPT_RECVTTL, INET_OPT_REUSEADDR, INET_OPT_REUSEPORT, INET_OPT_REUSEPORT_LB,
    INET_OPT_SNDBUF, INET_OPT_TCLASS, INET_OPT_TOS, INET_OPT_TTL, INET_OPT_USER_TIMEOUT,
    TCP_OPT_NODELAY,
};
use crate::drivers::inet::wire::{ctl_ok, ctl_xerror};
use crate::drivers::inet::{DRIVER_NAME, INET_AF_INET, UDP_DRIVER_NAME};

// ---------------------------------------------------------------------------
// Shapes
// ---------------------------------------------------------------------------

/// `224.0.0.1` — the IPv4 "all hosts" multicast group, as the signed word the wire carries.
const MULTICAST_GROUP: i32 = 0xe000_0001u32 as i32;
/// `127.0.0.1` — the loopback interface address the membership's local word nominates.
const LOOPBACK: i32 = 0x7f00_0001;

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

/// `[1]` alone: the record contributed **nothing** — the C's `TRUNCATE_TO(0, ptr)`, which is *not*
/// an error.
fn truncated() -> Vec<u8> {
    ctl_ok()
}

/// `[0]einval`: the whole reply, for a code the C's `return -1`/`RETURN_ERROR()` refuses.
fn einval() -> Vec<u8> {
    ctl_xerror("einval")
}

/// The `linger` record: `[opt][l_onoff_be32][l_linger_be32]` — nine bytes, not the five of a plain
/// `int` record (`inet_drv.c:9505`-`:9520`).
fn linger_reply(onoff: i32, linger: i32) -> Vec<u8> {
    let mut reply = ctl_ok();
    reply.push(INET_OPT_LINGER);
    reply.extend_from_slice(&onoff.to_be_bytes());
    reply.extend_from_slice(&linger.to_be_bytes());
    reply
}

/// The `multicast_if` record: `[opt][4 value bytes][family_be32]` (`inet_drv.c:9494`-`:9520`).
/// This floor's value slot is a deterministic zero (the C's is uninitialized memory).
fn multicast_if_reply() -> Vec<u8> {
    let mut reply = ctl_ok();
    reply.push(INET_OPT_MULTICAST_IF);
    reply.extend_from_slice(&0i32.to_be_bytes());
    reply.extend_from_slice(&u32::from(INET_AF_INET).to_be_bytes());
    reply
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// A `getopts` request asking for exactly one code.
fn get_request(code: u8) -> Request {
    (INET_REQ_GETOPTS, vec![code])
}

/// A `getopts` request asking for several codes, in order — the C's `while (len--)` walk.
fn get_codes(codes: &[u8]) -> Request {
    (INET_REQ_GETOPTS, codes.to_vec())
}
/// A `setopts` request carrying one `[code, value_be32]` record.
fn set_record(code: u8, value: i32) -> Request {
    (INET_REQ_SETOPTS, record(code, value))
}

/// A `setopts` request carrying a raw payload (several records, a name, or an extra word).
fn set_payload(payload: &[u8]) -> Request {
    (INET_REQ_SETOPTS, payload.to_vec())
}

/// A `setopts` request carrying several `[code, value_be32]` records.
fn set_records(records: &[(u8, i32)]) -> Request {
    set_payload(
        &records
            .iter()
            .flat_map(|(code, value)| record(*code, *value))
            .collect::<Vec<u8>>(),
    )
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

/// A `linger` **set** record: `[opt][l_onoff][l_linger]` (`inet_drv.c:7363`-`:7388`).
fn linger_record(onoff: i32, linger: i32) -> Request {
    let mut payload = record(INET_OPT_LINGER, onoff);
    payload.extend_from_slice(&linger.to_be_bytes());
    set_payload(&payload)
}

/// A `bind_to_device` **set** record: `[opt][len_be32][name]` (`inet_drv.c:7848`-`:7868`).
fn bind_to_device(name: &[u8]) -> Request {
    let mut payload = record(INET_OPT_BIND_TO_DEVICE, name.len() as i32);
    payload.extend_from_slice(name);
    set_payload(&payload)
}

/// A multicast-membership **set** record for `INET_AF_INET`:
/// `[opt][domain][ifindex][multiaddr][localaddr]` (`inet_drv.c:7616`-`:7800`).
fn membership(code: u8, ifindex: i32, multiaddr: i32, localaddr: i32) -> Request {
    let mut payload = record(code, i32::from(INET_AF_INET));
    payload.extend_from_slice(&ifindex.to_be_bytes());
    payload.extend_from_slice(&multiaddr.to_be_bytes());
    payload.extend_from_slice(&localaddr.to_be_bytes());
    set_payload(&payload)
}

/// The **truncated** membership payload the C reads past: the domain and the interface index, but no
/// multiaddr/localaddr words (`inet_drv.c:7750`-`:7757` reads them regardless).
fn membership_short(code: u8) -> Request {
    let mut payload = record(code, i32::from(INET_AF_INET));
    payload.extend_from_slice(&0i32.to_be_bytes());
    set_payload(&payload)
}

/// The bytes of one `raw` (`34`) **get** request:
/// `[opt][proto_be32][type_be32][data_provided][arg_sz_be32][arg bytes]` (`inet_drv.c:9554`).
fn raw_bytes(proto: i32, kind: i32, provided: bool, size: i32, data: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(14 + data.len());
    payload.push(INET_OPT_RAW);
    payload.extend_from_slice(&proto.to_be_bytes());
    payload.extend_from_slice(&kind.to_be_bytes());
    payload.push(u8::from(provided));
    payload.extend_from_slice(&size.to_be_bytes());
    payload.extend_from_slice(data);
    payload
}

/// One `raw` (`34`) `getopts` request.
fn raw_get(proto: i32, kind: i32, provided: bool, size: i32, data: &[u8]) -> Request {
    (INET_REQ_GETOPTS, raw_bytes(proto, kind, provided, size, data))
}

/// A `raw` (`34`) **get** request one byte short of the C's `len < 13` floor: the request cursor
/// still carries the code byte, so a well-formed one is 14 bytes and this is 13 (`inet_drv.c:9558`).
fn raw_short(proto: i32, kind: i32, provided: bool) -> Request {
    let mut payload = raw_bytes(proto, kind, provided, 4, &[]);
    payload.pop();
    (INET_REQ_GETOPTS, payload)
}
/// One `raw` (`34`) `setopts` request: `[opt][proto][type][arg_sz][arg bytes]`
/// (`inet_drv.c:7825`-`:7846`).
fn raw_set(proto: i32, kind: i32, size: i32, data: &[u8]) -> Request {
    let mut payload = record(INET_OPT_RAW, proto);
    payload.extend_from_slice(&kind.to_be_bytes());
    payload.extend_from_slice(&size.to_be_bytes());
    payload.extend_from_slice(data);
    set_payload(&payload)
}

// ---------------------------------------------------------------------------
// The comparators
// ---------------------------------------------------------------------------

/// Drives one sequence through a real port, asserts **every** reply against the concrete expected
/// bytes (so a case still means something without `erl`) and then byte-compares the lot with the
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

/// Runs a state's `getopts` row table — one request per row, in table order — on both sides, after
/// a prefix whose replies are all the plain `[1]` (`open`/`close` both are).
fn check_rows(driver: &str, prefix: &[Request], rows: &[Row]) {
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

/// Runs a `(request, expected)` step table.
fn check_steps(driver: &str, steps: &[(Request, Vec<u8>)]) {
    let sequence: Vec<Request> = steps.iter().map(|(request, _)| request.clone()).collect();
    let expected: Vec<Vec<u8>> = steps.iter().map(|(_, reply)| reply.clone()).collect();
    check(driver, &sequence, &expected);
}

/// Compares every reply with the oracle **except** the `reply_index` one, where only the length, the
/// result tag, the option code and the family word are compared — the `multicast_if` value slot is
/// uninitialized memory in the C (`inet_drv.c:9494`-`:9520`).
fn assert_multicast_if_shape(
    driver: &str,
    sequence: &[Request],
    reply_index: usize,
    ours: &[Vec<u8>],
) {
    let Some(stdout) = erl_stdout(&oracle_expression(driver, sequence)) else {
        return; // the oracle is unavailable; the check skipped loudly above
    };
    let Some(oracle) = parse_reply_dump(&stdout) else {
        panic!("could not parse the oracle reply dump: {stdout:?}");
    };
    assert_eq!(oracle.len(), ours.len(), "one reply per step: {oracle:?}");
    for (index, (ours, theirs)) in ours.iter().zip(&oracle).enumerate() {
        if index == reply_index {
            assert_eq!(ours.len(), theirs.len(), "the `multicast_if` record length");
            assert_eq!(ours[0], theirs[0], "the `multicast_if` result tag");
            assert_eq!(ours[1], theirs[1], "the `multicast_if` option code");
            assert_eq!(
                &ours[6..],
                &theirs[6..],
                "the `multicast_if` family word is the C's own"
            );
        } else {
            assert_eq!(ours, theirs, "reply {index} disagrees");
        }
    }
}

// ---------------------------------------------------------------------------
// The per-state getopts row tables
// ---------------------------------------------------------------------------

/// **(a) No socket**: every `getsockopt` fails and its record is dropped (`TRUNCATE_TO(0, ptr)`),
/// the two family-gated codes are `RETURN_ERROR()`, the set-only membership pair has no read-back
/// arm, and `exclusiveaddruse` is the one code that reports a literal zero without a socket
/// (`inet_drv.c:9440`-`:9447`).
fn bare_rows() -> Vec<Row> {
    vec![
        (INET_OPT_REUSEADDR, truncated()),
        (INET_OPT_KEEPALIVE, truncated()),
        (INET_OPT_DONTROUTE, truncated()),
        (INET_OPT_LINGER, truncated()),
        (INET_OPT_BROADCAST, truncated()),
        (INET_OPT_OOBINLINE, truncated()),
        (INET_OPT_SNDBUF, truncated()),
        (INET_OPT_RCVBUF, truncated()),
        (INET_OPT_PRIORITY, truncated()),
        (INET_OPT_TOS, truncated()),
        (TCP_OPT_NODELAY, truncated()),
        // `desc->sfamily` is the unset sentinel, so the family switch's `default` refuses.
        (INET_OPT_MULTICAST_IF, einval()),
        (INET_OPT_MULTICAST_TTL, truncated()),
        (INET_OPT_MULTICAST_LOOP, truncated()),
        // Set-only: no `case` in the get switch — `default: RETURN_ERROR()`.
        (INET_OPT_ADD_MEMBERSHIP, einval()),
        (INET_OPT_DROP_MEMBERSHIP, einval()),
        (INET_OPT_IPV6_V6ONLY, truncated()),
        (INET_OPT_REUSEPORT, truncated()),
        (INET_OPT_REUSEPORT_LB, truncated()),
        // Non-Windows: `*ptr++ = opt; put_int32(0, …); continue;`.
        (INET_OPT_EXCLUSIVEADDRUSE, one(INET_OPT_EXCLUSIVEADDRUSE, 0)),
        (INET_OPT_TCLASS, truncated()),
        (INET_OPT_BIND_TO_DEVICE, truncated()),
        (INET_OPT_RECVTOS, truncated()),
        (INET_OPT_RECVTCLASS, truncated()),
        // The family switch refuses before any `getsockopt` is attempted.
        (INET_OPT_PKTOPTIONS, einval()),
        (INET_OPT_TTL, truncated()),
        (INET_OPT_RECVTTL, truncated()),
        (INET_OPT_NOPUSH, truncated()),
        (INET_OPT_KEEPCNT, truncated()),
        (INET_OPT_KEEPIDLE, truncated()),
        (INET_OPT_KEEPINTVL, truncated()),
        (INET_OPT_USER_TIMEOUT, truncated()),
    ]
}

/// **(b) An opened stream socket**: every `int` code resolves through `getsockopt`. The values are
/// the kernel's option defaults, except `multicast_ttl`/`multicast_loop`, which the kernel reports
/// as `1`, and `ttl`/the `keep*` family, whose Linux defaults are pinned here.
///
/// Three codes are deliberately absent: `sndbuf` (`6`) and `recbuf` (`7`) report the host's buffer
/// sizes (see [`the_kernel_buffer_sizes_agree_with_the_oracle`]) and `multicast_if` (`11`) carries
/// an uninitialized value slot (see
/// [`multicast_if_pins_the_family_word_and_never_the_uninitialized_value`]).
fn opened_stream_rows() -> Vec<Row> {
    vec![
        (INET_OPT_REUSEADDR, one(INET_OPT_REUSEADDR, 0)),
        (INET_OPT_KEEPALIVE, one(INET_OPT_KEEPALIVE, 0)),
        (INET_OPT_DONTROUTE, one(INET_OPT_DONTROUTE, 0)),
        (INET_OPT_LINGER, linger_reply(0, 0)),
        (INET_OPT_BROADCAST, one(INET_OPT_BROADCAST, 0)),
        (INET_OPT_OOBINLINE, one(INET_OPT_OOBINLINE, 0)),
        (INET_OPT_PRIORITY, one(INET_OPT_PRIORITY, 0)),
        (INET_OPT_TOS, one(INET_OPT_TOS, 0)),
        (TCP_OPT_NODELAY, one(TCP_OPT_NODELAY, 0)),
        (INET_OPT_MULTICAST_TTL, one(INET_OPT_MULTICAST_TTL, 1)),
        (INET_OPT_MULTICAST_LOOP, one(INET_OPT_MULTICAST_LOOP, 1)),
        (INET_OPT_ADD_MEMBERSHIP, einval()),
        (INET_OPT_DROP_MEMBERSHIP, einval()),
        // `IPV6_V6ONLY` on an `AF_INET` socket: the `getsockopt` fails and drops the record.
        (INET_OPT_IPV6_V6ONLY, truncated()),
        (INET_OPT_REUSEPORT, one(INET_OPT_REUSEPORT, 0)),
        (INET_OPT_REUSEPORT_LB, one(INET_OPT_REUSEPORT_LB, 0)),
        (INET_OPT_EXCLUSIVEADDRUSE, one(INET_OPT_EXCLUSIVEADDRUSE, 0)),
        (INET_OPT_TCLASS, truncated()),
        // The `bind_to_device` record is `[opt][len_be32][name]`: nothing bound ⇒ length 0.
        (INET_OPT_BIND_TO_DEVICE, one(INET_OPT_BIND_TO_DEVICE, 0)),
        (INET_OPT_RECVTOS, one(INET_OPT_RECVTOS, 0)),
        (INET_OPT_RECVTCLASS, truncated()),
        // `[opt][total_len_be32]` with no control message pending.
        (INET_OPT_PKTOPTIONS, one(INET_OPT_PKTOPTIONS, 0)),
        (INET_OPT_TTL, one(INET_OPT_TTL, 64)),
        (INET_OPT_RECVTTL, one(INET_OPT_RECVTTL, 0)),
        // `TCP_CORK` is the Linux `nopush`.
        (INET_OPT_NOPUSH, one(INET_OPT_NOPUSH, 0)),
        (INET_OPT_KEEPCNT, one(INET_OPT_KEEPCNT, 9)),
        (INET_OPT_KEEPIDLE, one(INET_OPT_KEEPIDLE, 7200)),
        (INET_OPT_KEEPINTVL, one(INET_OPT_KEEPINTVL, 75)),
        (INET_OPT_USER_TIMEOUT, one(INET_OPT_USER_TIMEOUT, 0)),
    ]
}

/// **(c) An opened datagram socket**: `multicast_ttl` is not clamped to `1` on a datagram socket
/// and every TCP-only option (`nodelay`, `nopush`, the `keep*` family, `user_timeout`) fails its
/// `getsockopt` and is dropped.
///
/// `pktoptions` (`45`) is absent: `IP_PKTOPTIONS` is a TCP read on Linux, so `udp_inet` drops the
/// record — see [`the_kernel_buffer_sizes_agree_with_the_oracle`].
fn opened_datagram_rows() -> Vec<Row> {
    vec![
        (INET_OPT_REUSEADDR, one(INET_OPT_REUSEADDR, 0)),
        (INET_OPT_KEEPALIVE, one(INET_OPT_KEEPALIVE, 0)),
        (INET_OPT_DONTROUTE, one(INET_OPT_DONTROUTE, 0)),
        (INET_OPT_LINGER, linger_reply(0, 0)),
        (INET_OPT_BROADCAST, one(INET_OPT_BROADCAST, 0)),
        (INET_OPT_OOBINLINE, one(INET_OPT_OOBINLINE, 0)),
        (INET_OPT_PRIORITY, one(INET_OPT_PRIORITY, 0)),
        (INET_OPT_TOS, one(INET_OPT_TOS, 0)),
        // `TCP_NODELAY` has no meaning on a datagram socket.
        (TCP_OPT_NODELAY, truncated()),
        (INET_OPT_MULTICAST_TTL, one(INET_OPT_MULTICAST_TTL, 1)),
        (INET_OPT_MULTICAST_LOOP, one(INET_OPT_MULTICAST_LOOP, 1)),
        (INET_OPT_ADD_MEMBERSHIP, einval()),
        (INET_OPT_DROP_MEMBERSHIP, einval()),
        (INET_OPT_IPV6_V6ONLY, truncated()),
        (INET_OPT_REUSEPORT, one(INET_OPT_REUSEPORT, 0)),
        (INET_OPT_REUSEPORT_LB, one(INET_OPT_REUSEPORT_LB, 0)),
        (INET_OPT_EXCLUSIVEADDRUSE, one(INET_OPT_EXCLUSIVEADDRUSE, 0)),
        (INET_OPT_TCLASS, truncated()),
        (INET_OPT_BIND_TO_DEVICE, one(INET_OPT_BIND_TO_DEVICE, 0)),
        (INET_OPT_RECVTOS, one(INET_OPT_RECVTOS, 0)),
        (INET_OPT_RECVTCLASS, truncated()),
        // `IP_PKTOPTIONS` is a stream read on Linux, so a datagram socket drops the record.
        (INET_OPT_PKTOPTIONS, truncated()),
        (INET_OPT_TTL, one(INET_OPT_TTL, 64)),
        (INET_OPT_RECVTTL, one(INET_OPT_RECVTTL, 0)),
        (INET_OPT_NOPUSH, truncated()),
        (INET_OPT_KEEPCNT, truncated()),
        (INET_OPT_KEEPIDLE, truncated()),
        (INET_OPT_KEEPINTVL, truncated()),
        (INET_OPT_USER_TIMEOUT, truncated()),
    ]
}

/// **(d) Post-`close`**: the C tears the socket down but keeps the cached `stype`/`sfamily`, so
/// `multicast_if` (`11`) still answers its family record (pinned separately) and `pktoptions` (`45`)
/// passes the family gate and then **drops** its record — it no longer refuses as it does on a bare
/// port. Every other row is the bare row again.
fn post_close_rows() -> Vec<Row> {
    let mut rows = bare_rows();
    for (code, reply) in rows.iter_mut() {
        if *code == INET_OPT_PKTOPTIONS {
            *reply = truncated();
        }
    }
    rows.retain(|(code, _)| *code != INET_OPT_MULTICAST_IF);
    rows
}

// ---------------------------------------------------------------------------
// The per-state getopts rows
// ---------------------------------------------------------------------------

/// **(a)** A `tcp_inet` port that has never opened a socket.
#[test]
fn the_bare_stream_port_drops_every_syscall_record() {
    check_rows(DRIVER_NAME, &[], &bare_rows());
}

/// **(a)** A `udp_inet` port that has never opened a socket: both drivers resolve the same
/// `(level, name)` pairs, so every row is the stream row.
#[test]
fn the_bare_datagram_port_reports_the_same_rows() {
    check_rows(UDP_DRIVER_NAME, &[], &bare_rows());
}

/// **(b)** An opened stream socket: the kernel answers every `int` record.
#[test]
fn the_opened_stream_port_reports_the_kernel_values() {
    check_rows(DRIVER_NAME, &[open_stream()], &opened_stream_rows());
}

/// **(c)** An opened datagram socket: the TCP-only options drop out.
#[test]
fn the_opened_datagram_port_reports_the_kernel_values() {
    check_rows(
        UDP_DRIVER_NAME,
        &[open_datagram()],
        &opened_datagram_rows(),
    );
}

/// **(d)** Post-`close` on a stream port: every `getsockopt` fails again, but `pktoptions` now
/// drops its record instead of refusing — the family gate reads the cached family.
#[test]
fn the_post_close_stream_port_keeps_only_the_cached_family() {
    check_rows(
        DRIVER_NAME,
        &[open_stream(), close_request()],
        &post_close_rows(),
    );
}

/// **(d)** Post-`close` on a datagram port: the same rows.
#[test]
fn the_post_close_datagram_port_keeps_only_the_cached_family() {
    check_rows(
        UDP_DRIVER_NAME,
        &[open_datagram(), close_request()],
        &post_close_rows(),
    );
}

/// The get walk's **drop and continue** (`TRUNCATE_TO(0, ptr); continue;`): a failed `getsockopt`
/// costs only its own record — the records before it stay and the codes after it are still tried —
/// while a code with no read-back arm discards the whole reply (`default: RETURN_ERROR()`).
#[test]
fn a_dropped_record_does_not_end_the_walk() {
    let mut kept = ctl_ok();
    kept.extend_from_slice(&record(INET_OPT_TTL, 64));
    kept.extend_from_slice(&record(INET_OPT_REUSEADDR, 0));

    check(
        DRIVER_NAME,
        &[
            open_stream(),
            // `ipv6_v6only` fails on an `AF_INET` socket and is dropped; the codes around it answer.
            get_codes(&[INET_OPT_IPV6_V6ONLY, INET_OPT_TTL, INET_OPT_REUSEADDR]),
            // A code with no arm replaces the whole reply, taking the record before it with it.
            get_codes(&[INET_OPT_REUSEADDR, INET_OPT_ADD_MEMBERSHIP]),
        ],
        &[ctl_ok(), kept, einval()],
    );

    // On a socket-less port every syscall code drops, so a mixed request answers `[1]` alone.
    check(
        DRIVER_NAME,
        &[
            get_codes(&[INET_OPT_TOS, INET_OPT_TTL]),
            get_request(INET_OPT_REUSEADDR),
        ],
        &[ctl_ok(), ctl_ok()],
    );
}

// ---------------------------------------------------------------------------
// The special shapes
// ---------------------------------------------------------------------------/// `multicast_if` (`11`) is the C's most broken record: it reserves nine reply bytes
/// (`*ptr++ = opt; PLACE_FOR(4, ptr)`) but only fills five, so the four value bytes are
/// **uninitialized memory** and change run to run, while the interface itself is written past the
/// reported length. This floor answers the deterministic form — `[opt][0,0,0,0][family_be32]` — and
/// the row pins what *is* well defined: the length, the tag, the code and the family word, on both
/// sides.
#[test]
fn multicast_if_pins_the_family_word_and_never_the_uninitialized_value() {
    // The bare port's family gate refuses before the record is placed at all.
    let bare = rust_replies(DRIVER_NAME, &[get_request(INET_OPT_MULTICAST_IF)]);
    assert_eq!(bare[0], einval(), "a bare port's family gate");

    for (driver, prefix) in [
        (DRIVER_NAME, open_stream()),
        (UDP_DRIVER_NAME, open_datagram()),
    ] {
        let sequence = [prefix, get_request(INET_OPT_MULTICAST_IF)];
        let replies = rust_replies(driver, &sequence);
        assert_multicast_if_shape(driver, &sequence, 1, &replies);
        assert_eq!(
            replies[1],
            multicast_if_reply(),
            "the deterministic floor form on {driver}"
        );
    }
    let sequence = [
        open_stream(),
        close_request(),
        get_request(INET_OPT_MULTICAST_IF),
    ];
    let replies = rust_replies(DRIVER_NAME, &sequence);
    assert_eq!(replies[2], multicast_if_reply(), "post-close keeps the family");
    assert_multicast_if_shape(DRIVER_NAME, &sequence, 2, &replies);
}

/// `sndbuf` (`6`) and `recbuf` (`7`) report the **kernel's** buffer sizes, and Linux doubles a
/// requested size, so no value may be hard-coded: the row asserts the record's shape and
/// byte-compares with the oracle. Both codes are the two the `int` family tables above leave out
/// precisely because their values are host-dependent.
#[test]
fn the_kernel_buffer_sizes_agree_with_the_oracle() {
    for (driver, open) in [(DRIVER_NAME, open_stream()), (UDP_DRIVER_NAME, open_datagram())] {
        let codes = [INET_OPT_SNDBUF, INET_OPT_RCVBUF];
        let sequence: Vec<Request> = std::iter::once(open.clone())
            .chain(codes.iter().map(|code| get_request(*code)))
            .collect();
        let replies = rust_replies(driver, &sequence);
        assert_eq!(replies[0], ctl_ok(), "the `open` reply");
        for (reply, code) in replies[1..].iter().zip(codes) {
            assert_eq!(reply[0], 1, "code {code} answers `[1] …`: {reply:?}");
            assert_eq!(reply.len(), 6, "code {code} answers one record: {reply:?}");
            assert_eq!(reply[1], code);
            assert!(
                i32::from_be_bytes([reply[2], reply[3], reply[4], reply[5]]) > 0,
                "code {code} reports the kernel's buffer size: {reply:?}"
            );
        }
        assert_agrees_with_oracle(driver, &sequence, &replies);
    }
}

// ---------------------------------------------------------------------------
// `raw` (34)
// ---------------------------------------------------------------------------

/// The `raw` read's request is 14 bytes (`[opt][proto][type][data_provided][arg_sz]`); the C
/// refuses a shorter one (`inet_drv.c:9558`) and drops the record when the `getsockopt` fails. With
/// no socket nothing can be read, so a well-formed request contributes nothing at all.
#[test]
fn the_bare_port_drops_every_raw_read() {
    check_steps(
        DRIVER_NAME,
        &[
            // One byte short of the C's `len < 13` floor — the request cursor still carries the
            // code byte, so a well-formed `raw` read is 14 bytes (`inet_drv.c:9558`).
            (raw_short(0, TTL, false), einval()),
            (raw_get(0, 0, false, 4, &[]), truncated()),
            (raw_get(0, TTL, false, 4, &[]), truncated()),
            (raw_get(6, 1, false, 4, &[]), truncated()),
            // A provided argument is consumed from the request, then still never read.
            (raw_get(0, TTL, true, 4, &[0, 0, 0, 9]), truncated()),
        ],
    );
}

/// The `inet_drv.c:9554`-`:9578` arm on a live socket: the reply is
/// `[opt][proto][type][arg_sz][arg bytes]`, where the returned size (not the requested one) is what
/// the record reports. A refused `getsockopt` drops the record, a short provided argument and an
/// out-of-range size are the C's `RETURN_ERROR()`, and the cursor keeps walking afterwards.
///
/// The `arg bytes` are **byte-verbatim in both directions**: the C `memcpy`s the caller's bytes into
/// the `getsockopt` buffer and reports whatever the kernel wrote back, with no endian conversion
/// (only the header words go through `put_int32`), so an `int` option's value is in **host** byte
/// order — hence the `to_ne_bytes` expectations below.
#[test]
fn the_raw_read_reports_the_kernels_answer() {
    const TTL_CODE: i32 = 2;
    let mut trailing = raw_bytes(0, TTL_CODE, false, 4, &[]);
    trailing.push(INET_OPT_REUSEADDR);

    let mut sequence = vec![open_stream()];
    let mut expected = vec![ctl_ok()];

    // `IP_TTL` reads back the kernel's default 64 through the caller-nominated triple.
    sequence.push(raw_get(0, TTL_CODE, false, 4, &[]));
    let mut reply = ctl_ok();
    reply.extend_from_slice(&raw_record(0, TTL_CODE, &64i32.to_ne_bytes()));
    expected.push(reply);

    // A zero requested size still answers a 13-byte header with a zero length.
    sequence.push(raw_get(0, TTL_CODE, false, 0, &[]));
    let mut reply = ctl_ok();
    reply.extend_from_slice(&raw_record(0, TTL_CODE, &[]));
    expected.push(reply);

    // Nominating more bytes than the kernel returns reports the **returned** size, not the
    // requested one: `sock_getopt` writes `arg_sz` back and the C reports that (`:9573`-`:9578`).
    sequence.push(raw_get(0, TTL_CODE, false, 100, &[]));
    let mut reply = ctl_ok();
    reply.extend_from_slice(&raw_record(0, TTL_CODE, &64i32.to_ne_bytes()));
    expected.push(reply);
    // A provided argument is copied in first and then overwritten by the `getsockopt`.
    sequence.push(raw_get(0, TTL_CODE, true, 4, &[0, 0, 0, 9]));
    let mut reply = ctl_ok();
    reply.extend_from_slice(&raw_record(0, TTL_CODE, &64i32.to_ne_bytes()));
    expected.push(reply);

    // A provided argument the request does not carry is `return -1`.
    sequence.push(raw_get(0, TTL_CODE, true, 8, &[0, 0, 0, 0]));
    expected.push(einval());

    // A negative and an oversized argument are both `return -1` (`arg_sz > INET_MAX_OPT_BUFFER`).
    sequence.push(raw_get(0, TTL_CODE, false, -1, &[]));
    expected.push(einval());
    sequence.push(raw_get(0, TTL_CODE, false, 70_000, &[]));
    expected.push(einval());

    // An unknown protocol makes the `getsockopt` fail, so the record is dropped.
    sequence.push(raw_get(65_535, 1, false, 4, &[]));
    expected.push(truncated());

    // The cursor walks on to the next code after a `raw` record (14 bytes + data consumed).
    sequence.push((INET_REQ_GETOPTS, trailing));
    let mut reply = ctl_ok();
    reply.extend_from_slice(&raw_record(0, TTL_CODE, &64i32.to_ne_bytes()));
    reply.extend_from_slice(&record(INET_OPT_REUSEADDR, 0));
    expected.push(reply);

    check(DRIVER_NAME, &sequence, &expected);
}

/// `IP_TTL`'s C code, spelled here so the `raw` rows read as a named triple.
const TTL: i32 = 2;

/// One `raw` read's reply record: `[opt][proto_be32][type_be32][arg_sz_be32][arg bytes]`.
fn raw_record(proto: i32, kind: i32, data: &[u8]) -> Vec<u8> {
    let mut record = vec![INET_OPT_RAW];
    record.extend_from_slice(&proto.to_be_bytes());
    record.extend_from_slice(&kind.to_be_bytes());
    record.extend_from_slice(&(data.len() as u32).to_be_bytes());
    record.extend_from_slice(data);
    record
}

// ---------------------------------------------------------------------------
// The setopts rows
// ---------------------------------------------------------------------------

/// **(a) No socket**: the nine `propagate = 1` codes turn their failed `setsockopt` into the whole
/// reply `[0]einval`, while every other code's failure is discarded (`inet_drv.c:7923`-`:7942`).
/// The family-gated `multicast_if` (`11`) and multicast-membership (`14`/`15`) codes refuse before
/// any syscall, a short `linger`/`bind_to_device`/`raw` payload is the C's own `return -1`, and an
/// unserved code is `default: return -1`.
#[test]
fn the_bare_port_settles_the_propagate_split() {
    check_steps(
        DRIVER_NAME,
        &[
            (set_record(INET_OPT_REUSEADDR, 1), ctl_ok()),
            (set_record(INET_OPT_KEEPALIVE, 1), ctl_ok()),
            (set_record(INET_OPT_DONTROUTE, 1), ctl_ok()),
            // A `linger` record without its `l_linger` word is `return -1`.
            (set_record(INET_OPT_LINGER, 1), einval()),
            (linger_record(1, 7), ctl_ok()),
            (set_record(INET_OPT_BROADCAST, 1), ctl_ok()),
            (set_record(INET_OPT_OOBINLINE, 1), ctl_ok()),
            (set_record(INET_OPT_SNDBUF, 4096), ctl_ok()),
            (set_record(INET_OPT_RCVBUF, 4096), ctl_ok()),
            // The propagating nine: `priority`, `tos`, `ipv6_v6only`, `tclass`, `bind_to_device`,
            // `recvtos`, `recvtclass`, `ttl`, `recvttl`.
            (set_record(INET_OPT_PRIORITY, 1), einval()),
            (set_record(INET_OPT_TOS, 1), einval()),
            (set_record(TCP_OPT_NODELAY, 1), ctl_ok()),
            (set_record(INET_OPT_MULTICAST_IF, 1), einval()),
            (set_record(INET_OPT_MULTICAST_TTL, 1), ctl_ok()),
            (set_record(INET_OPT_MULTICAST_LOOP, 1), ctl_ok()),
            (membership(INET_OPT_ADD_MEMBERSHIP, 0, MULTICAST_GROUP, LOOPBACK), einval()),
            (membership(INET_OPT_DROP_MEMBERSHIP, 0, MULTICAST_GROUP, LOOPBACK), einval()),
            (set_record(INET_OPT_IPV6_V6ONLY, 1), einval()),
            (set_record(INET_OPT_REUSEPORT, 1), ctl_ok()),
            (set_record(INET_OPT_REUSEPORT_LB, 1), ctl_ok()),
            (set_record(INET_OPT_EXCLUSIVEADDRUSE, 1), ctl_ok()),
            (set_record(INET_OPT_TCLASS, 1), einval()),
            // A `bind_to_device` record whose name the request does not carry is `return -1`.
            (set_record(INET_OPT_BIND_TO_DEVICE, 1), einval()),
            (bind_to_device(b"lo"), einval()),
            (bind_to_device(&[b'l'; 17]), einval()),
            (set_record(INET_OPT_RECVTOS, 1), einval()),
            (set_record(INET_OPT_RECVTCLASS, 1), einval()),
            (set_record(INET_OPT_TTL, 1), einval()),
            (set_record(INET_OPT_RECVTTL, 1), einval()),
            (set_record(INET_OPT_NOPUSH, 1), ctl_ok()),
            (set_record(INET_OPT_KEEPCNT, 1), ctl_ok()),
            (set_record(INET_OPT_KEEPIDLE, 1), ctl_ok()),
            (set_record(INET_OPT_KEEPINTVL, 1), ctl_ok()),
            (set_record(INET_OPT_USER_TIMEOUT, 1), ctl_ok()),
            // A `raw` write is not a propagating code: its failed `setsockopt` is discarded.
            (raw_set(0, TTL, 4, &[0, 0, 0, 0]), ctl_ok()),
            (raw_set(0, TTL, 0, &[]), ctl_ok()),
            // `pktoptions` is a **get-only** code: there is no set arm, so the C's
            // `default: return -1` refuses it whatever the port's state.
            (set_record(INET_OPT_PKTOPTIONS, 1), einval()),
            (set_record(200, 1), einval()),
        ],
    );
}

/// **(b)** The propagating codes on a live stream socket: a `setsockopt` the kernel refuses is the
/// whole reply `[0]einval`, and an accepted one is `[1]` and reads back through the get side.
#[test]
fn the_opened_stream_port_propagates_a_refused_setsockopt() {
    const TOS_CODE: i32 = 1;
    check_steps(
        DRIVER_NAME,
        &[
            (open_stream(), ctl_ok()),
            // `SO_PRIORITY` refuses a negative priority.
            (set_record(INET_OPT_PRIORITY, -1), einval()),
            (set_record(INET_OPT_TOS, 16), ctl_ok()),
            (get_request(INET_OPT_TOS), one(INET_OPT_TOS, 16)),
            // `IPV6_V6ONLY`/`IPV6_TCLASS`/`IPV6_RECVTCLASS` have no meaning on an `AF_INET` socket.
            (set_record(INET_OPT_IPV6_V6ONLY, 1), einval()),
            (set_record(INET_OPT_IPV6_V6ONLY, 0), einval()),
            (set_record(INET_OPT_TCLASS, 1), einval()),
            (set_record(INET_OPT_RECVTCLASS, 1), einval()),
            (set_record(INET_OPT_TTL, 5), ctl_ok()),
            (get_request(INET_OPT_TTL), one(INET_OPT_TTL, 5)),
            (set_record(INET_OPT_RECVTTL, 1), ctl_ok()),
            (get_request(INET_OPT_RECVTTL), one(INET_OPT_RECVTTL, 1)),
            (set_record(INET_OPT_RECVTOS, 1), ctl_ok()),
            (get_request(INET_OPT_RECVTOS), one(INET_OPT_RECVTOS, 1)),
            // `bind_to_device`: an accepted name round-trips, an empty one is the kernel's refusal
            // (there is no device called `""`), and one longer than `IFNAMSIZ` is `return -1`.
            (bind_to_device(b"lo"), ctl_ok()),
            (get_request(INET_OPT_BIND_TO_DEVICE), bind_to_device_reply(b"lo")),
            (bind_to_device(b""), einval()),
            (get_request(INET_OPT_BIND_TO_DEVICE), bind_to_device_reply(b"lo")),
            (bind_to_device(&[b'l'; 17]), einval()),
            (get_request(INET_OPT_BIND_TO_DEVICE), bind_to_device_reply(b"lo")),
            // The `34` raw write goes through the caller-nominated triple; `IP_TOS` is legal.
            (raw_set(0, TOS_CODE, 4, &8i32.to_ne_bytes()), ctl_ok()),
            (get_request(INET_OPT_TOS), one(INET_OPT_TOS, 8)),
            // A propagating failure discards the records the **same** request already accepted:
            // `priority` refuses a negative value, so the whole reply is `[0]einval`.
            (
                set_records(&[(INET_OPT_KEEPALIVE, 1), (INET_OPT_PRIORITY, -1)]),
                einval(),
            ),
            // …while a non-propagating failure leaves the reply `[1]` and the walk carries on.
            (
                set_records(&[(TCP_OPT_NODELAY, 1), (INET_OPT_KEEPALIVE, 0)]),
                ctl_ok(),
            ),
        ],
    );
}

/// The `bind_to_device` get record: `[opt][len_be32][name]` (`inet_drv.c:9600`-`:9620`).
fn bind_to_device_reply(name: &[u8]) -> Vec<u8> {
    let mut reply = ctl_ok();
    reply.push(INET_OPT_BIND_TO_DEVICE);
    reply.extend_from_slice(&(name.len() as u32).to_be_bytes());
    reply.extend_from_slice(name);
    reply
}

/// **(b)** Every plain `int` code whose value the kernel stores verbatim: the set is `[1]` and the
/// get reports the value back. Two rows are the kernel's own normalisation — `multicast_ttl` clamps
/// to `1` on a stream socket and `exclusiveaddruse` always reads `0` (it is never stored).
#[test]
fn the_opened_stream_port_stores_the_plain_int_codes() {
    check_steps(
        DRIVER_NAME,
        &[
            (open_stream(), ctl_ok()),
            (set_record(INET_OPT_REUSEADDR, 1), ctl_ok()),
            (get_request(INET_OPT_REUSEADDR), one(INET_OPT_REUSEADDR, 1)),
            (set_record(INET_OPT_KEEPALIVE, 1), ctl_ok()),
            (get_request(INET_OPT_KEEPALIVE), one(INET_OPT_KEEPALIVE, 1)),
            (set_record(INET_OPT_DONTROUTE, 1), ctl_ok()),
            (get_request(INET_OPT_DONTROUTE), one(INET_OPT_DONTROUTE, 1)),
            (set_record(INET_OPT_BROADCAST, 1), ctl_ok()),
            (get_request(INET_OPT_BROADCAST), one(INET_OPT_BROADCAST, 1)),
            (set_record(INET_OPT_OOBINLINE, 1), ctl_ok()),
            (get_request(INET_OPT_OOBINLINE), one(INET_OPT_OOBINLINE, 1)),
            (set_record(TCP_OPT_NODELAY, 1), ctl_ok()),
            (get_request(TCP_OPT_NODELAY), one(TCP_OPT_NODELAY, 1)),
            // A stream socket clamps `IP_MULTICAST_TTL` to its maximum of `1`.
            (set_record(INET_OPT_MULTICAST_TTL, 4), ctl_ok()),
            (get_request(INET_OPT_MULTICAST_TTL), one(INET_OPT_MULTICAST_TTL, 1)),
            (set_record(INET_OPT_MULTICAST_LOOP, 0), ctl_ok()),
            (get_request(INET_OPT_MULTICAST_LOOP), one(INET_OPT_MULTICAST_LOOP, 0)),
            (set_record(INET_OPT_REUSEPORT, 1), ctl_ok()),
            (get_request(INET_OPT_REUSEPORT), one(INET_OPT_REUSEPORT, 1)),
            // `nopush` is `TCP_CORK` on Linux.
            (set_record(INET_OPT_NOPUSH, 1), ctl_ok()),
            (get_request(INET_OPT_NOPUSH), one(INET_OPT_NOPUSH, 1)),
            (set_record(INET_OPT_KEEPCNT, 3), ctl_ok()),
            (get_request(INET_OPT_KEEPCNT), one(INET_OPT_KEEPCNT, 3)),
            (set_record(INET_OPT_KEEPIDLE, 100), ctl_ok()),
            (get_request(INET_OPT_KEEPIDLE), one(INET_OPT_KEEPIDLE, 100)),
            (set_record(INET_OPT_KEEPINTVL, 30), ctl_ok()),
            (get_request(INET_OPT_KEEPINTVL), one(INET_OPT_KEEPINTVL, 30)),
            (set_record(INET_OPT_USER_TIMEOUT, 1000), ctl_ok()),
            (get_request(INET_OPT_USER_TIMEOUT), one(INET_OPT_USER_TIMEOUT, 1000)),
            // Non-Windows: accepted and ignored, and the read-back is a literal zero.
            (set_record(INET_OPT_EXCLUSIVEADDRUSE, 1), ctl_ok()),
            (
                get_request(INET_OPT_EXCLUSIVEADDRUSE),
                one(INET_OPT_EXCLUSIVEADDRUSE, 0),
            ),
        ],
    );
}

/// The `linger` record's sticky second word: the kernel keeps `l_linger` when a later `setsockopt`
/// clears `l_onoff`, so `{0, 0}` after `{1, 7}` reads back as `{0, 7}` — a kernel behaviour both
/// sides share because they issue the same syscall.
#[test]
fn the_linger_record_keeps_its_second_word() {
    check_steps(
        DRIVER_NAME,
        &[
            (open_stream(), ctl_ok()),
            (linger_record(1, 7), ctl_ok()),
            (get_request(INET_OPT_LINGER), linger_reply(1, 7)),
            (linger_record(0, 0), ctl_ok()),
            (get_request(INET_OPT_LINGER), linger_reply(0, 7)),
            (linger_record(1, 0), ctl_ok()),
            (get_request(INET_OPT_LINGER), linger_reply(1, 0)),
            // A short payload refuses and stores nothing.
            (set_record(INET_OPT_LINGER, 0), einval()),
            (get_request(INET_OPT_LINGER), linger_reply(1, 0)),
        ],
    );
}

/// **(c)** A datagram socket's plain `int` codes: the TCP-only options are accepted-and-discarded
/// (their `setsockopt` fails, and none of them propagates) and `multicast_ttl` is **not** clamped
/// to `1`.
#[test]
fn the_opened_datagram_port_stores_the_plain_int_codes() {
    check_steps(
        UDP_DRIVER_NAME,
        &[
            (open_datagram(), ctl_ok()),
            (set_record(TCP_OPT_NODELAY, 1), ctl_ok()),
            (get_request(TCP_OPT_NODELAY), truncated()),
            (set_record(INET_OPT_NOPUSH, 1), ctl_ok()),
            (get_request(INET_OPT_NOPUSH), truncated()),
            (set_record(INET_OPT_KEEPIDLE, 100), ctl_ok()),
            (get_request(INET_OPT_KEEPIDLE), truncated()),
            (set_record(INET_OPT_MULTICAST_TTL, 4), ctl_ok()),
            (get_request(INET_OPT_MULTICAST_TTL), one(INET_OPT_MULTICAST_TTL, 4)),
            (set_record(INET_OPT_TTL, 5), ctl_ok()),
            (get_request(INET_OPT_TTL), one(INET_OPT_TTL, 5)),
            (set_record(INET_OPT_RECVTTL, 1), ctl_ok()),
            (get_request(INET_OPT_RECVTTL), one(INET_OPT_RECVTTL, 1)),
            (bind_to_device(b"lo"), ctl_ok()),
            (get_request(INET_OPT_BIND_TO_DEVICE), bind_to_device_reply(b"lo")),
        ],
    );
}

/// **(d)** The cached family survives a `close`, so a post-`close` `multicast_if` write is served
/// (`[1]`) where a bare port's family gate refuses — while the propagating codes keep refusing.
#[test]
fn the_post_close_port_still_passes_the_family_gate() {
    check_steps(
        DRIVER_NAME,
        &[
            (open_stream(), ctl_ok()),
            (close_request(), ctl_ok()),
            (set_record(INET_OPT_MULTICAST_IF, 1), ctl_ok()),
            (set_record(INET_OPT_REUSEADDR, 1), ctl_ok()),
            (set_record(INET_OPT_OOBINLINE, 1), ctl_ok()),
            (set_record(INET_OPT_TTL, 5), einval()),
            (set_record(INET_OPT_LINGER, 1), einval()),
        ],
    );
}

/// The `{recbuf, N}` write also raises the driver's own read buffer
/// (`inet_drv.c:7321`-`:7360`) — capped at `INET_UDP_THEORETICAL_MAX_MTU` on a datagram socket —
/// unless `{buffer, N}` was set explicitly (`INET_FLG_BUFFER_SET`). `{sndbuf, N}` never does.
#[test]
fn the_recbuf_write_raises_the_drivers_read_buffer() {
    check_steps(
        DRIVER_NAME,
        &[
            (open_stream(), ctl_ok()),
            (get_request(INET_LOPT_BUFFER), one(INET_LOPT_BUFFER, 9216)),
            (set_record(INET_OPT_SNDBUF, 70_000), ctl_ok()),
            (get_request(INET_LOPT_BUFFER), one(INET_LOPT_BUFFER, 9216)),
            (set_record(INET_OPT_RCVBUF, 70_000), ctl_ok()),
            (get_request(INET_LOPT_BUFFER), one(INET_LOPT_BUFFER, 70_000)),
            (set_record(INET_LOPT_BUFFER, 500), ctl_ok()),
            (set_record(INET_OPT_RCVBUF, 100), ctl_ok()),
            (get_request(INET_LOPT_BUFFER), one(INET_LOPT_BUFFER, 500)),
        ],
    );

    check_steps(
        UDP_DRIVER_NAME,
        &[
            (open_datagram(), ctl_ok()),
            (get_request(INET_LOPT_BUFFER), one(INET_LOPT_BUFFER, 9216)),
            (set_record(INET_OPT_RCVBUF, 70_000), ctl_ok()),
            (get_request(INET_LOPT_BUFFER), one(INET_LOPT_BUFFER, 65_536)),
            (set_record(INET_OPT_RCVBUF, 200_000), ctl_ok()),
            (get_request(INET_LOPT_BUFFER), one(INET_LOPT_BUFFER, 65_536)),
            (set_record(INET_LOPT_BUFFER, 500), ctl_ok()),
            (set_record(INET_OPT_RCVBUF, 100), ctl_ok()),
            (get_request(INET_LOPT_BUFFER), one(INET_LOPT_BUFFER, 500)),
        ],
    );
}

/// The multicast-membership **write** on a live socket: the `struct ip_mreqn` is well formed, so the
/// record is `[1]` whatever the kernel answers (the code does not propagate its failure). Only the
/// get side has no arm at all (the `[0]einval` rows above).
#[test]
fn the_multicast_membership_writes_are_accepted() {
    for (driver, open) in [(DRIVER_NAME, open_stream()), (UDP_DRIVER_NAME, open_datagram())] {
        check(
            driver,
            &[
                open,
                membership(INET_OPT_ADD_MEMBERSHIP, 0, MULTICAST_GROUP, LOOPBACK),
                membership(INET_OPT_DROP_MEMBERSHIP, 0, MULTICAST_GROUP, LOOPBACK),
                // The interface index form (`imr_ifindex`) is the same record.
                membership(INET_OPT_ADD_MEMBERSHIP, 1, MULTICAST_GROUP, 0),
                membership(INET_OPT_DROP_MEMBERSHIP, 1, MULTICAST_GROUP, 0),
            ],
            &[ctl_ok(), ctl_ok(), ctl_ok(), ctl_ok(), ctl_ok()],
        );
    }
}

/// A **short** multicast-membership payload is a deliberate non-mirror: the C reads the `multiaddr`
/// and `localaddr` words past the end of the request (`inet_drv.c:7750`-`:7757` has no length
/// guard), so its `setsockopt` gets whatever bytes follow — uninitialized data whose failure is not
/// propagated, which is why the oracle answers `[1]` on a live socket. This floor refuses the
/// record instead of inventing the missing words: the C's own `return -1`, i.e. the whole reply
/// `[0]einval`. Never compared with the oracle — there is nothing well defined to compare.
#[test]
fn a_short_membership_payload_is_refused_instead_of_read_past() {
    for (driver, open) in [(DRIVER_NAME, open_stream()), (UDP_DRIVER_NAME, open_datagram())] {
        let replies = rust_replies(
            driver,
            &[
                open,
                membership_short(INET_OPT_ADD_MEMBERSHIP),
                membership_short(INET_OPT_DROP_MEMBERSHIP),
            ],
        );
        assert_eq!(replies[0], ctl_ok(), "the `open` reply on {driver}");
        assert_eq!(
            replies[1],
            einval(),
            "a truncated membership payload on {driver}"
        );
        assert_eq!(replies[2], einval());
    }
}
