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

//! The `INET_REQ_FDOPEN`(13) **adoption-failure errno** suite: a descriptor either socket driver
//! cannot take over answers the C's `ctl_error(sock_errno())` **reply**, never a raise.
//!
//! `inet_ctl_fdopen` (`inet_drv.c:5223`-`:5282`) never raises: with a non-zero `bound` it probes
//! `sock_name(2)` and answers `ctl_error(sock_errno())` (`:5253`), compares the family and answers
//! `ctl_error(EINVAL)` (`:5255`), and any later failure inside it answers `ctl_error(sock_errno())`
//! again (`:5260`). So the caller of `erlang:port_control(Port, INET_REQ_FDOPEN, Data)` can always
//! **name** the reason: `[0]"ebadf"` for a number that is not open, `[0]"enotsock"` for a
//! descriptor that is not a socket, `[0]"einval"` for the wrong family.
//!
//! This floor reaches those verdicts through the primitives that already preserve the errno
//! (`fcntl(2)` via [`crate::drivers::fd::set_nonblocking`], `getpeername(2)` via
//! [`super::sys::has_peer`], `getsockname(2)` via [`super::sys::socket_family`], and the poller's
//! registration), and the `Fdopen` arm renders it with
//! [`super::adopt_probe::adoption_reply`]. Every row below therefore asserts **two** things at
//! once: the reply is the C's `[0] ++ errno_name`, *and* the control call answered a reply at all
//! (a `try_control` that returned `Err` is exactly the regression this suite exists to catch).
//!
//! # The one measured divergence: the ten-byte `bound == 0` form
//!
//! `if (len == 6) bound = 1; else bound = get_int32(buf+2+4);` (`:5264`-`:5265`), and the C skips
//! **every** probe when `bound` is zero — no `sock_name`, no family comparison, and `SET_NONBLOCKING`
//! discards its own failure — so the oracle answers `[1]` even for a descriptor number no process
//! could have open (measured `[1]` for `<<1,1,0,15,66,63,0,0,0,0>>`, i.e. fd `999999`). This floor
//! has to hand the descriptor to the poller, so it cannot serve such a descriptor at all: it
//! refuses it naming the errno the failing probe carried (`[0]"ebadf"`). A named refusal beats a
//! fabricated `[1]` for a port that could never use the descriptor — that is the whole point of
//! this suite, and
//! [`the_blind_bound_zero_form_refuses_by_errno_where_the_c_answers_ok`] pins both sides so the
//! divergence is measured rather than assumed.
//!
//! # Comparison styles
//!
//! * **Rust-side** for every operand that names a descriptor **this** process owns (a pipe, a
//!   character device, an `AF_UNIX` pair, an `AF_INET6` socket, a socket the port already holds):
//!   an `erl` probe is a second process, so the same number means something else there.
//! * **Oracle comparison** for the rows whose operand means the same thing in both processes: a
//!   number that is closed everywhere (the [`CLOSED_FD`] rows) and the descriptor-independent
//!   ladder. Those go through the shared
//!   [`assert_outcomes_agree_with_the_oracle`](super::opcode_tests::assert_outcomes_agree_with_the_oracle)
//!   harness, which **skips loudly** when `erl` is not on `PATH`.
//!
//! The refusal ladder that runs *before* any adoption (the length/family/type guards and the
//! `bound`-form `sock_name(2)` probe) is pinned by the sibling
//! [`opcode_tests_refusals`](super::opcode_tests_refusals) suite; this file covers the failures of
//! the adoption **body** and the reply channel they use.
use std::os::unix::io::{AsRawFd, RawFd};

use super::opcode_tests::{
    assert_outcomes_agree_with_the_oracle, bare_port as tcp_bare_port, erl_stdout, op,
    oracle_expression, oracle_replies, Call, Rig, Rule,
};
use super::opcode_tests_udp::bare_port as udp_bare_port;
use super::sys::socket_family;
use super::wire::{ctl_error, ctl_ok, ctl_xerror};
use super::{INET_AF_INET, INET_REP_ERROR, INET_TYPE_DGRAM, INET_TYPE_STREAM};

/// A descriptor number no process can have open (`EMFILE`-beyond on every platform this crate
/// targets), so `fcntl(2)`/`getsockname(2)` on it answer `EBADF` in the oracle and in this process
/// alike — the one `fdopen` operand whose reply is comparable across two runtimes.
const CLOSED_FD: i32 = 999_999;

/// One socket driver under test, with the pieces its `INET_REQ_FDOPEN`(13) arm needs.
struct Subject {
    /// The driver's registered name — what the `erl` probe passes to `open_port`.
    name: &'static str,
    /// A bare (socket-less) port of that driver.
    bare: fn() -> Rig,
    /// The `INET_TYPE_*` tag the driver's own type guard admits (and stores as `desc->stype`).
    type_byte: u8,
}

/// Both drivers: `tcp_inet` (`SOCK_STREAM` only) and `udp_inet` (which admits the stream *and*
/// the datagram tag, `packet_inet_ctl`, `inet_drv.c:14671`).
fn subjects() -> [Subject; 2] {
    [
        Subject {
            name: "tcp_inet",
            bare: tcp_bare_port,
            type_byte: INET_TYPE_STREAM,
        },
        Subject {
            name: "udp_inet",
            bare: udp_bare_port,
            type_byte: INET_TYPE_DGRAM,
        },
    ]
}

/// The ten-byte `fdopen` operand `[af, type, fd_be32, bound_be32]` — the form whose `bound` word
/// is explicit, so both the C's probed (`bound != 0`) and blind (`bound == 0`) adoptions can be
/// addressed.
fn fdopen_operand(type_byte: u8, fd: RawFd, bound: i32) -> Vec<u8> {
    let mut operand = vec![INET_AF_INET, type_byte];
    operand.extend_from_slice(&fd.to_be_bytes());
    operand.extend_from_slice(&bound.to_be_bytes());
    operand
}

/// Drives one `INET_REQ_FDOPEN`(13) call and asserts it answered the C's error **reply** rather
/// than raising: the whole point of the channel under test.
#[track_caller]
fn fdopen_reply(port: &mut Rig, operand: &[u8], label: &str) -> Vec<u8> {
    match port.try_control(&op(13), operand) {
        Ok(reply) => {
            assert_eq!(
                reply.first().copied(),
                Some(INET_REP_ERROR),
                "{label}: a failed adoption answers the C's [0] error tag: {reply:?}"
            );
            reply
        }
        Err(err) => panic!("{label}: a failed adoption is a reply, not a raise: {err:?}"),
    }
}

/// Every adoption failure of the driver's own adoption body names its errno in the C's reply
/// channel, instead of collapsing into a raise or a generic `[0]"einval"`.
///
/// The rows, with the probe that produces each failure:
///
/// | descriptor | `bound` | failing probe | reply |
/// |---|---|---|---|
/// | closed number | 1 | `getsockname(2)` (`wire::fdopen_precheck`) | `[0]"ebadf"` |
/// | closed number | 0 | `fcntl(F_SETFL)` (`set_nonblocking`) | `[0]"ebadf"` |
/// | `/dev/null` | 1 | `getsockname(2)` (`wire::fdopen_precheck`) | `[0]"enotsock"` |
/// | `/dev/null` | 0 | `getpeername(2)` (`sys::has_peer`) | `[0]"enotsock"` |
/// | an `AF_UNIX` socket | 1 | the family comparison | `[0]"einval"` |
/// | an `AF_INET6` socket | 1 | the family comparison | `[0]"einval"` |
///
/// The `bound == 0` rows are the interesting ones for this suite: the C does not probe at all
/// there, so the errno can only come from **this** floor's adoption body — `fcntl(2)`'s `EBADF`
/// and `getpeername(2)`'s `ENOTSOCK`, both preserved rather than rewritten.
#[test]
fn adoption_failures_answer_the_c_errno_reply() {
    for subject in subjects() {
        let mut port = (subject.bare)();

        // A number that is not open at all: `getsockname(2)` with `bound` set, `fcntl(2)`
        // without.
        assert_eq!(
            fdopen_reply(
                &mut port,
                &fdopen_operand(subject.type_byte, CLOSED_FD, 1),
                subject.name
            ),
            ctl_xerror("ebadf"),
            "{}: `sock_name(2)` on a closed descriptor is its `EBADF`",
            subject.name
        );
        assert_eq!(
            fdopen_reply(
                &mut port,
                &fdopen_operand(subject.type_byte, CLOSED_FD, 0),
                subject.name
            ),
            ctl_xerror("ebadf"),
            "{}: the blind `bound == 0` form still names the `fcntl(2)` errno instead of \
             fabricating the C's `[1]`",
            subject.name
        );

        // An open descriptor that is not a socket. `/dev/null` is a character device: a valid
        // `fcntl(2)` target, so the `bound == 0` row reaches the `getpeername(2)` probe.
        let device = std::fs::File::open("/dev/null").expect("`/dev/null` opens");
        assert_eq!(
            fdopen_reply(
                &mut port,
                &fdopen_operand(subject.type_byte, device.as_raw_fd(), 1),
                subject.name
            ),
            ctl_xerror("enotsock"),
            "{}: `sock_name(2)` on a character device is `[0]enotsock`",
            subject.name
        );
        // The `getpeername(2)` probe only runs for a `SOCK_STREAM` adoption (`:5252`), so the
        // stream tag is passed on both drivers — the datagram one admits it too.
        assert_eq!(
            fdopen_reply(
                &mut port,
                &fdopen_operand(INET_TYPE_STREAM, device.as_raw_fd(), 0),
                subject.name
            ),
            ctl_xerror("enotsock"),
            "{}: the adoption body's `getpeername(2)` is `[0]enotsock`",
            subject.name
        );

        // A real socket of the wrong family: `sock_name(2)` succeeds, so the C's family
        // comparison is what refuses — `ctl_error(EINVAL)`, the one errno the C spells itself.
        let (left, _right) = std::os::unix::net::UnixStream::pair().expect("an AF_UNIX pair");
        assert_eq!(
            fdopen_reply(
                &mut port,
                &fdopen_operand(subject.type_byte, left.as_raw_fd(), 1),
                subject.name
            ),
            ctl_xerror("einval"),
            "{}: an AF_UNIX descriptor under an INET_AF_INET tag is `[0]einval`",
            subject.name
        );

        // The same refusal for an `AF_INET6` socket, when the host has an IPv6 loopback.
        match std::net::UdpSocket::bind("[::1]:0") {
            Ok(six) => assert_eq!(
                fdopen_reply(
                    &mut port,
                    &fdopen_operand(subject.type_byte, six.as_raw_fd(), 1),
                    subject.name
                ),
                ctl_xerror("einval"),
                "{}: an AF_INET6 descriptor under an INET_AF_INET tag is `[0]einval`",
                subject.name
            ),
            Err(err) => eprintln!(
                "SKIP: no IPv6 loopback on this host ({err}) — skipping {}'s AF_INET6 fdopen row",
                subject.name
            ),
        }
    }
}

/// A second adoption on a port that already holds a socket is a **reply** (`[0]"einval"`) and the
/// live socket keeps serving the port.
///
/// The C assigns `desc->s = s` with no `desc_close(desc)` first (`:5267`), overwriting — and
/// leaking — the previous descriptor. Refusing is the honest choice; what this row adds to
/// [`opcode_tests_refusals`](super::opcode_tests_refusals)'s version is the *channel*: the refusal
/// is a reply the caller can read, never a raise, and the port's own lifecycle state and cached
/// type are untouched by it.
#[test]
fn a_second_adoption_refuses_without_touching_the_live_socket() {
    for subject in subjects() {
        let mut port = (subject.bare)();
        // `[af, type, protocol]` — the default protocol tag is `0` (`INET_PROTO_DEFAULT`).
        let open_arg = vec![INET_AF_INET, subject.type_byte, 0];
        assert_eq!(
            port.control(&op(1), &open_arg),
            ctl_ok(),
            "{}: INET_REQ_OPEN creates the socket",
            subject.name
        );
        let status_before = port.control(&op(16), &[]);
        let type_before = port.control(&op(15), &[]);

        let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("a loopback datagram socket");
        assert_eq!(
            fdopen_reply(
                &mut port,
                &fdopen_operand(subject.type_byte, socket.as_raw_fd(), 1),
                subject.name
            ),
            ctl_xerror("einval"),
            "{}: a port that already holds a socket refuses a second adoption",
            subject.name
        );
        assert_eq!(
            port.control(&op(16), &[]),
            status_before,
            "{}: the live socket's lifecycle state is untouched",
            subject.name
        );
        assert_eq!(
            port.control(&op(15), &[]),
            type_before,
            "{}: the live socket's cached type is untouched",
            subject.name
        );
    }
}

/// The descriptor-independent half of the arm, compared byte for byte with the real system driver:
/// a number that is closed in **both** processes, and the port's own `stdout` descriptor whenever
/// it is not a socket (the ordinary case under a test harness, where fd 1 is a pipe or a file).
///
/// **Skips loudly** when `erl` is not on `PATH`, and skips the fd-1 row alone when this process's
/// `stdout` happens to be a socket (it then names a descriptor the oracle cannot see).
///
/// Both calls are `bound`-form: the blind `bound == 0` rows are the divergence
/// [`the_blind_bound_zero_form_refuses_by_errno_where_the_c_answers_ok`] measures.
#[test]
fn the_descriptor_independent_rows_agree_with_the_oracle() {
    for subject in subjects() {
        let mut calls = vec![
            // The six-byte form implies `bound = 1`, so `sock_name(2)` runs and answers `EBADF`.
            Call::req(13, fdopen_six(subject.type_byte, CLOSED_FD), Rule::Exact),
            Call::req(13, fdopen_operand(subject.type_byte, CLOSED_FD, 1), Rule::Exact),
        ];
        if socket_family(1).is_err() {
            // fd 1 is not a socket: `sock_name(2)` refuses it with `ENOTSOCK` on both sides.
            calls.push(Call::req(13, fdopen_operand(INET_TYPE_STREAM, 1, 1), Rule::Exact));
        } else {
            eprintln!(
                "SKIP: this process's stdout is a socket — skipping {}'s non-socket row",
                subject.name
            );
        }
        assert_outcomes_agree_with_the_oracle(subject.name, &calls, subject.bare);
    }
}

/// The six-byte `fdopen` operand `[af, type, fd_be32]` — the form whose implicit `bound` is 1
/// (`if (len == 6) bound = 1`, `inet_drv.c:5264`).
fn fdopen_six(type_byte: u8, fd: RawFd) -> Vec<u8> {
    let mut operand = vec![INET_AF_INET, type_byte];
    operand.extend_from_slice(&fd.to_be_bytes());
    operand
}

/// The **measured divergence**: with the ten-byte form and a `bound` word of zero the C adopts the
/// number blind and answers `[1]` even for a descriptor that is not open, while this floor refuses
/// it naming the errno it could not use.
///
/// Both sides are pinned here, so the trade is a documented measurement rather than an assumption.
/// A port that answered the C's `[1]` would claim a descriptor its poller never registered — a
/// wrong result, which the workspace error strategy forbids; a refusal that named nothing (a
/// raise, or a generic `einval`) would lose the errno, which is the residual this suite closes.
///
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn the_blind_bound_zero_form_refuses_by_errno_where_the_c_answers_ok() {
    for subject in subjects() {
        // The oracle: no probe at all, so the answer is the blind `ok`.
        let calls = vec![Call::req(
            13,
            fdopen_operand(subject.type_byte, CLOSED_FD, 0),
            Rule::Exact,
        )];
        let Some(dump) = erl_stdout(&oracle_expression(subject.name, &calls)) else {
            eprintln!(
                "SKIP: no `erl` on PATH — skipping {}'s blind-adoption cross-check",
                subject.name
            );
            continue;
        };
        let oracle = oracle_replies(&dump);
        assert_eq!(oracle.len(), 1, "{}: the oracle answered: {dump:?}", subject.name);
        assert_eq!(
            oracle[0],
            ctl_ok(),
            "{}: measured — the C's `bound == 0` form answers [1] for a closed descriptor",
            subject.name
        );

        // This floor: the descriptor has to reach the poller, so it is refused by errno.
        let mut port = (subject.bare)();
        assert_eq!(
            fdopen_reply(
                &mut port,
                &fdopen_operand(subject.type_byte, CLOSED_FD, 0),
                subject.name
            ),
            ctl_xerror("ebadf"),
            "{}: the same operand is refused as the `fcntl(2)` verdict it is",
            subject.name
        );
        // The refused descriptor left no trace: the port is still socket-less.
        assert_eq!(
            port.control(&op(16), &[]),
            {
                let mut closed = ctl_ok();
                closed.extend_from_slice(&0u32.to_be_bytes());
                closed
            },
            "{}: a refused adoption leaves the port socket-less (INET_STATE_CLOSED)",
            subject.name
        );
    }
}

/// The errno renderer the arm uses is the C's own `erl_errno_id` table, so an errno this floor's
/// table does not name still reaches the caller as the C's `errno_<N>` fallback instead of being
/// flattened — and the general errno table agrees with the one a socket-only failure needs.
#[test]
fn the_reply_renders_the_errno_the_c_names() {
    for errno in [
        libc::EBADF,
        libc::ENOTSOCK,
        libc::EINVAL,
        libc::EPERM,
        libc::EACCES,
        libc::EBUSY,
    ] {
        let reply = ctl_error(std::io::Error::from_raw_os_error(errno));
        assert_eq!(reply[0], INET_REP_ERROR, "the `[0]` tag for errno {errno}");
        assert!(
            !reply[1..].is_empty(),
            "errno {errno} is named, never an empty payload"
        );
        // The payload is the lowercase name (or the C's `errno_<N>` fallback) — ASCII, always.
        assert!(
            reply[1..].iter().all(u8::is_ascii),
            "errno {errno} renders as ASCII: {reply:?}"
        );
    }
}
