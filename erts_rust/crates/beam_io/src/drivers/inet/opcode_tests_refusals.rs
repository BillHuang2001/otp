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

//! The `tcp_inet`/`udp_inet` **refusal** suite: the numeric opcodes whose reply is a **refusal**
//! the C itself answers rather than a capability gap — `INET_REQ_FDOPEN`(13)'s pre-adoption ladder
//! and the four interface requests `GETIFLIST`(21), `IFGET`(22), `IFSET`(23) and
//! `GETIFADDRS`(25).
//!
//! The numeric-dispatch suites ([`opcode_tests`](super::opcode_tests) and its datagram sibling)
//! pin one `(opcode, operand)` matrix per driver in a single port's lifetime. This suite takes the
//! states that matrix cannot reach: a caller-supplied **descriptor** handed to `fdopen` (whose
//! refusal depends on the descriptor's own kind) and the host's **interface tables** (a database
//! the kernel owns, so the reply has to be compared rather than spelled out).
//!
//! Two comparison styles, chosen per row by whether the answer is process-independent:
//!
//! * an **`erl` oracle comparison** wherever the operand names no descriptor of this process — the
//!   ladder refusals, the interface walks and the `ifset` operands. No descriptor number means the
//!   same thing in two processes, so `fdopen`'s `bound` rows are only compared for descriptor
//!   numbers that are closed in *both* runtimes;
//! * a **Rust-side** assertion where the operand names an fd this process owns (a real `AF_INET`
//!   socket, an `AF_INET6` one, `/dev/null`) or where the C would adopt the number and could no
//!   longer shut its port down cleanly (the ten-byte `bound == 0` form).
//!
//! The helpers (`Rig`, the oracle plumbing, the reply builders) are shared from the two dispatch
//! suites rather than copied.
use super::opcode_tests::{
    assert_outcomes_agree_with_the_oracle, erl_stdout, not_served_reason, op,
    open_arg as tcp_open_arg, oracle_expression, oracle_replies, reason_of, bare_port as tcp_bare_port,
    Call, Rig, Rule,
};
use super::opcode_tests_udp::{bare_port as udp_bare_port, open_arg as udp_open_arg};
use super::wire::{ctl_ok, ctl_xerror};
use super::{INET_AF_INET, INET_REP_OK, INET_TYPE_DGRAM, INET_TYPE_STREAM};
use std::os::unix::io::AsRawFd;

/// The Linux loopback interface name — the fallback [`loopback_name`] uses on a host whose
/// interface list it cannot recognise. macOS spells the loopback `lo0`.
const LINUX_LOOPBACK: &[u8] = b"lo";

/// An interface name the kernel does **not** have. A `SIOCGIF*` walk over it is the C's
/// `break`-without-a-record path, so its records are the "nothing answered" forms.
const MISSING_INTERFACE: &[u8] = b"nonexist0";

/// A descriptor number no process can have open (`EMFILE`-beyond on every platform this crate
/// targets), so `sock_name(2)` on it is `EBADF` in the oracle and in this process alike — the one
/// `fdopen` operand whose reply is comparable across two runtimes.
const CLOSED_FD: i32 = 999_999;

/// The two drivers reached from the shared `inet_ctl` arms, with the pieces their
/// `INET_REQ_FDOPEN` arm needs differing per driver.
struct Case {
    /// The driver's registered name — what the `erl` probe passes to `open_port`.
    name: &'static str,
    /// The packed `open` argument that creates the driver's socket (`IS_OPEN` is a precondition of
    /// the interface arms).
    open_arg: [u8; 3],
    /// A **socket-less** port of this driver — the state `open_port` gives the oracle.
    bare: fn() -> Rig,
    /// The `INET_TYPE_*` bytes the driver's `fdopen` type guard lets through (`packet_inet_ctl`
    /// accepts the stream type too, `tcp_inet_ctl` only the stream one).
    types: &'static [u8],
    /// An `INET_TYPE_*` byte that guard refuses with `[0]einval`.
    refused_type: u8,
}

fn cases() -> [Case; 2] {
    [
        Case {
            name: "tcp_inet",
            open_arg: tcp_open_arg(),
            bare: tcp_bare_port,
            types: &[INET_TYPE_STREAM],
            refused_type: INET_TYPE_DGRAM,
        },
        Case {
            name: "udp_inet",
            open_arg: udp_open_arg(),
            bare: udp_bare_port,
            types: &[INET_TYPE_DGRAM],
            refused_type: 3,
        },
    ]
}

/// A port of `case`'s driver with its socket created — the state every interface arm requires
/// (`!IS_OPEN ⇒ [0]exbadport`).
fn opened(case: &Case) -> Rig {
    let mut port = (case.bare)();
    assert_eq!(
        port.control(&op(1), &case.open_arg),
        ctl_ok(),
        "{}: INET_REQ_OPEN",
        case.name
    );
    port
}

/// The `fdopen` operand of a `bound` descriptor: `[af, type, fd_be32, bound_be32]`.
fn bound_operand(type_byte: u8, fd: i32) -> Vec<u8> {
    let mut operand = vec![INET_AF_INET, type_byte];
    operand.extend_from_slice(&fd.to_be_bytes());
    operand.extend_from_slice(&1i32.to_be_bytes());
    operand
}

// -----------------------------------------------------------------------
// `INET_REQ_FDOPEN`(13) — the pre-adoption refusal ladder
// -----------------------------------------------------------------------

/// The `INET_REQ_FDOPEN`(13) ladder of each driver over descriptors **this process owns**, where no
/// oracle comparison is possible.
///
/// The C's arm (`inet_drv.c:12003`-`:12037`) is `len` guard → family `switch` → type guard → and
/// only then `inet_ctl_fdopen`, which for a non-zero `bound` runs `sock_name(2)` first
/// (`:5252`-`:5257`): a closed number is `[0]ebadf`, an open non-socket `[0]enotsock`, and a real
/// `AF_INET` socket passes into the **adoption** this floor does not implement (the typed gap). A
/// real `AF_INET6` socket fails the family comparison the adoption makes against `domain`, i.e.
/// `[0]einval`.
///
/// The descriptor numbers here are minted by this test process, so the `erl` probe — a **second**
/// process — cannot see them; the process-independent rows of the same ladder are compared in
/// [`the_fdopen_operand_refusals_agree_with_the_oracle`].
///
/// A bare number such as the `9` of the older suites is deliberately **avoided**: whether fd 9 is
/// open, and what it is, differs between the two runtimes (it answers `enotsock` in an `erl`
/// process only because an unrelated descriptor happens to sit there), so the only
/// cross-process-safe descriptor number is one that is closed everywhere ([`CLOSED_FD`]).
#[test]
fn fdopen_bound_operands_answer_the_c_pre_adoption_refusals() {
    for case in cases() {
        let mut port = opened(&case);
        let type_byte = case.types[0];

        // A `bound` descriptor that is not open at all.
        assert_eq!(
            port.control(&op(13), &bound_operand(type_byte, CLOSED_FD)),
            ctl_xerror("ebadf"),
            "{}: `sock_name(2)` on a closed descriptor is its `EBADF`",
            case.name
        );

        // An open descriptor that is not a socket.
        let file = std::fs::File::open("/dev/null").expect("`/dev/null` opens");
        assert_eq!(
            port.control(&op(13), &bound_operand(type_byte, file.as_raw_fd())),
            ctl_xerror("enotsock"),
            "{}: `sock_name(2)` on a character device is `[0]enotsock`",
            case.name
        );

        // A real `AF_INET` socket: the pre-adoption check passes and the C would hand the
        // descriptor to the port (`desc->prebound = 1`), which is exactly what this floor cannot
        // do — adopting would let the port's `stop`/`Drop` close a descriptor the caller owns.
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("a loopback datagram socket");
        let err = port
            .try_control(&op(13), &bound_operand(type_byte, socket.as_raw_fd()))
            .expect_err("an AF_INET socket reaches the adoption, which is the typed gap");
        assert_eq!(
            reason_of(err),
            not_served_reason("fdopen", "inet_ctl"),
            "{}: the gap names the request and its switch",
            case.name
        );

        // A real `AF_INET6` socket: a different family from the IPv4 tag the operand carries, i.e.
        // the C's `name.sa.sa_family != domain ⇒ ctl_error(EINVAL)`.
        match std::net::UdpSocket::bind("[::1]:0") {
            Ok(six) => assert_eq!(
                port.control(&op(13), &bound_operand(type_byte, six.as_raw_fd())),
                ctl_xerror("einval"),
                "{}: an AF_INET6 descriptor under an INET_AF_INET tag is `[0]einval`",
                case.name
            ),
            Err(err) => eprintln!(
                "SKIP: no IPv6 loopback on this host ({err}) — skipping {}'s AF_INET6 fdopen row",
                case.name
            ),
        }
    }
}

/// The `bound == 0` form of `INET_REQ_FDOPEN`(13) — the one operand that reaches the C's **blind**
/// adoption, i.e. the surviving typed gap of this arm.
///
/// With a ten-byte operand whose `bound` word is zero the C takes the descriptor over with **no
/// syscall at all** (`inet_drv.c:5264`-`:5266`), which is why the reply is `[1]` even for a
/// garbage number (measured `[1]` for `<<1,2,0,0,0,9,0,0,0,0>>`). This floor answers the typed gap
/// instead of fabricating that `[1]`.
///
/// **Rust-side only**: the same operand against the oracle adopts a descriptor the C then holds in
/// `desc->inet.s`, and its port shutdown path works on it, which leaves the `erl` expression
/// unable to close its port cleanly — a corpus hazard rather than a behavioural question, so the
/// row is pinned here and stated in the subtree docs instead.
#[test]
fn a_fdopen_with_bound_zero_is_still_the_adoption_gap() {
    for case in cases() {
        let mut port = opened(&case);
        for type_byte in case.types {
            let mut operand = vec![INET_AF_INET, *type_byte];
            operand.extend_from_slice(&CLOSED_FD.to_be_bytes());
            operand.extend_from_slice(&0i32.to_be_bytes());
            let err = port
                .try_control(&op(13), &operand)
                .expect_err("a `bound == 0` descriptor is adopted blind, which this floor cannot do");
            assert_eq!(
                reason_of(err),
                not_served_reason("fdopen", "inet_ctl"),
                "{}: type {type_byte}",
                case.name
            );
        }
    }
}

/// The process-independent half of the `fdopen` ladder, compared with the real system driver.
///
/// Every row here either carries no descriptor at all (the `len`/family/type guards refuse the
/// operand before any syscall) or names [`CLOSED_FD`], which is closed in both runtimes — so the
/// two replies must agree byte for byte. The rows whose answer depends on a descriptor this
/// process owns are asserted locally by
/// [`fdopen_bound_operands_answer_the_c_pre_adoption_refusals`].
///
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn the_fdopen_operand_refusals_agree_with_the_oracle() {
    for case in cases() {
        let mut closed = vec![INET_AF_INET, case.types[0]];
        closed.extend_from_slice(&CLOSED_FD.to_be_bytes());
        let mut closed_bound = closed.clone();
        closed_bound.extend_from_slice(&1i32.to_be_bytes());
        let calls = vec![
            // The length guard: six or ten bytes, nothing else.
            Call::req(13, vec![], Rule::Exact),
            Call::req(13, vec![0u8; 4], Rule::Exact),
            Call::req(13, vec![0u8; 8], Rule::Exact),
            Call::req(13, vec![0u8; 11], Rule::Exact),
            // The family switch: only `INET_AF_INET` here (see the divergence note in the subtree
            // docs — the C's switch also knows the IPv6 and Unix tags).
            Call::req(13, vec![7, case.types[0], 0, 0, 0, 0], Rule::Exact),
            Call::req(13, vec![0, case.types[0], 0, 0, 0, 0], Rule::Exact),
            // The type guard.
            Call::req(13, vec![INET_AF_INET, case.refused_type, 0, 0, 0, 0], Rule::Exact),
            Call::req(13, vec![INET_AF_INET, 9, 0, 0, 0, 0], Rule::Exact),
            // The pre-adoption `sock_name(2)` on a descriptor that is closed everywhere.
            Call::req(13, closed, Rule::Exact),
            Call::req(13, closed_bound, Rule::Exact),
        ];
        assert_outcomes_agree_with_the_oracle(case.name, &calls, case.bare);
    }
}

// -----------------------------------------------------------------------
// `GETIFLIST`(21) / `GETIFADDRS`(25) — the two interface **list** walks
// -----------------------------------------------------------------------

/// Parses the NUL-terminated interface names of a `GETIFLIST` reply's payload.
fn parse_names(bytes: &[u8]) -> Vec<Vec<u8>> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

/// The host's loopback interface name, taken from the port's **own** `GETIFLIST` reply.
///
/// An `ifget`/`ifset` row names an interface, and that name differs per platform (`lo` on Linux,
/// `lo0` on macOS), so the rows ask the live driver instead of hardcoding one; [`LINUX_LOOPBACK`]
/// is only the fallback for a host whose interface list the search does not recognise.
fn loopback_name(port: &mut Rig) -> Vec<u8> {
    let names = parse_names(&port.control(&op(21), &[])[1..]);
    names
        .iter()
        .find(|name| name.starts_with(b"lo"))
        .cloned()
        .unwrap_or_else(|| LINUX_LOOPBACK.to_vec())
}

/// `INET_REQ_GETIFLIST`(21) — `inet_ctl_getiflist` (`inet_drv.c:5671`) — lists every
/// `AF_INET`/`AF_INET6` interface in `SIOCGIFCONF` order, each name NUL-terminated behind the `[1]`
/// tag.
///
/// **The comparison is the tag plus the parsed name list, not the raw bytes.** The bytes are the
/// kernel's `SIOCGIFCONF` answer, i.e. **host state** read independently by the oracle process and
/// by this one; a NIC appearing or disappearing between the two runs (a VPN coming up, a veth
/// created by a parallel test) changes them legitimately, while a difference between the parsed
/// name lists is always a real divergence. The reply's framing is pinned separately (the length must
/// be the `[1]` tag plus one NUL per name), so a missing terminator cannot hide behind the parsing.
///
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn getiflist_lists_the_host_interfaces_like_the_oracle() {
    for case in cases() {
        let calls = vec![
            Call::req(1, case.open_arg.to_vec(), Rule::Exact),
            Call::req(21, vec![], Rule::Exact),
        ];
        let Some(dump) = erl_stdout(&oracle_expression(case.name, &calls)) else {
            eprintln!(
                "SKIP: no `erl` on PATH — skipping {}'s GETIFLIST cross-check",
                case.name
            );
            continue;
        };
        let oracle = oracle_replies(&dump);
        assert_eq!(
            oracle.len(),
            calls.len(),
            "{}: the oracle answered every call: {dump:?}",
            case.name
        );

        let mut port = opened(&case);
        let rust = port.control(&op(21), &[]);

        assert_eq!(rust[0], INET_REP_OK, "{}: the `[1]` tag: {rust:?}", case.name);
        assert_eq!(rust[0], oracle[1][0], "{}: the oracle's result tag", case.name);
        let rust_names = parse_names(&rust[1..]);
        assert_eq!(
            rust.len(),
            1 + rust_names
                .iter()
                .map(|name| name.len() + 1)
                .sum::<usize>(),
            "{}: every name is NUL-terminated and nothing else is in the reply: {rust:?}",
            case.name
        );
        assert!(!rust_names.is_empty(), "{}: the host has interfaces: {rust:?}", case.name);
        assert_eq!(
            rust_names,
            parse_names(&oracle[1][1..]),
            "{}: the interface name list disagrees",
            case.name
        );
    }
}

/// One `getifaddrs(3)` node of a `GETIFADDRS` reply: the interface name plus its `[tag][payload]`
/// records in wire order.
#[derive(Debug, PartialEq, Eq)]
struct IfAddrNode {
    name: Vec<u8>,
    records: Vec<(u8, Vec<u8>)>,
}

/// Parses a `GETIFADDRS` reply's payload into its node streams (`inet_drv.c:6611`-`:6650`): a name
/// (latin-1), a NUL, then one `[code][payload]` record per answered option, a NUL closing the node.
///
/// The payload widths are the encoding's own (`sockaddr_to_buf`, `:5440`): an address record is a
/// family tag plus that family's address bytes, `MTU`/`FLAGS` a big-endian 32-bit value and
/// `HWADDR` a big-endian 16-bit length followed by that many bytes.
fn parse_ifaddrs(bytes: &[u8]) -> Vec<IfAddrNode> {
    let mut nodes = Vec::new();
    let mut at = 0usize;
    while at < bytes.len() {
        let name_start = at;
        while at < bytes.len() && bytes[at] != 0 {
            at += 1;
        }
        let name = bytes[name_start..at].to_vec();
        at += 1;
        let mut records = Vec::new();
        while at < bytes.len() {
            let code = bytes[at];
            at += 1;
            if code == 0 {
                break;
            }
            let width = match code {
                // `ADDR`/`BROADADDR`/`DSTADDR`/`NETMASK`: a family tag plus its address.
                1..=3 | 5 => match bytes.get(at).copied() {
                    Some(INET_AF_INET) => 1 + 4,
                    Some(_) => 1 + 16,
                    None => 0,
                },
                // `MTU`/`FLAGS`: a big-endian 32-bit value.
                4 | 6 => 4,
                // `HWADDR`: a big-endian 16-bit length plus that many bytes.
                7 => match bytes.get(at..at + 2) {
                    Some([hi, lo]) => 2 + usize::from(u16::from_be_bytes([*hi, *lo])),
                    _ => 0,
                },
                _ => 0,
            };
            let end = (at + width).min(bytes.len());
            records.push((code, bytes[at..end].to_vec()));
            at = end;
        }
        nodes.push(IfAddrNode { name, records });
    }
    nodes
}

/// The `(name, record tags)` view of a parsed `GETIFADDRS` stream — the host-independent half.
fn node_shape(nodes: &[IfAddrNode]) -> Vec<(&[u8], Vec<u8>)> {
    nodes
        .iter()
        .map(|node| {
            (
                node.name.as_slice(),
                node.records.iter().map(|(code, _)| *code).collect(),
            )
        })
        .collect()
}

/// The per-node record payload widths of a parsed `GETIFADDRS` stream.
fn node_widths(nodes: &[IfAddrNode]) -> Vec<Vec<usize>> {
    nodes
        .iter()
        .map(|node| node.records.iter().map(|(_, payload)| payload.len()).collect())
        .collect()
}

/// `INET_REQ_GETIFADDRS`(25) — `inet_ctl_getifaddrs` (`inet_drv.c:6611`) — streams the
/// `getifaddrs(3)` list as `[1] ++ (name NUL [code][payload]* NUL)*`.
///
/// **The comparison is structural, not byte-for-byte.** The stream carries the host's live
/// addresses, interface flags and hardware addresses — state the oracle process and this one read
/// independently — in the libc's own `getifaddrs(3)` order. Comparing the parsed node list (the
/// name sequence, every record's tag and every record's encoded width) catches exactly the
/// divergences that are this driver's (a missing node, a wrong record code, a mis-sized payload, a
/// missing NUL) without turning an interface coming up between the two runs into a failure. The
/// loopback node's full bytes are pinned separately in
/// [`getifaddrs_pins_the_loopback_node`].
///
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn getifaddrs_streams_the_host_interface_list_like_the_oracle() {
    for case in cases() {
        let calls = vec![
            Call::req(1, case.open_arg.to_vec(), Rule::Exact),
            Call::req(25, vec![], Rule::Exact),
        ];
        let Some(dump) = erl_stdout(&oracle_expression(case.name, &calls)) else {
            eprintln!(
                "SKIP: no `erl` on PATH — skipping {}'s GETIFADDRS cross-check",
                case.name
            );
            continue;
        };
        let oracle = oracle_replies(&dump);
        assert_eq!(
            oracle.len(),
            calls.len(),
            "{}: the oracle answered every call: {dump:?}",
            case.name
        );

        let mut port = opened(&case);
        let rust = port.control(&op(25), &[]);
        assert_eq!(rust[0], INET_REP_OK, "{}: the `[1]` tag: {rust:?}", case.name);

        let rust_nodes = parse_ifaddrs(&rust[1..]);
        let oracle_nodes = parse_ifaddrs(&oracle[1][1..]);
        assert!(!rust_nodes.is_empty(), "{}: the host has interfaces", case.name);
        assert_eq!(
            node_shape(&rust_nodes),
            node_shape(&oracle_nodes),
            "{}: the node/record stream disagrees",
            case.name
        );
        assert_eq!(
            node_widths(&rust_nodes),
            node_widths(&oracle_nodes),
            "{}: a record changed width",
            case.name
        );
    }
}

/// The loopback node of a `GETIFADDRS` reply, pinned byte for byte.
///
/// Loopback is the one interface whose records this suite can spell out: `UP | LOOPBACK | RUNNING`
/// (`0x15` — the C emulates `RUNNING` from `IFF_UP`, `:5481`), a six-byte all-zero hardware address
/// (a link-layer record is only reported when its encoding is longer than three bytes, `:6769`),
/// then, in the `AF_INET` pass, `127.0.0.1` and its `/8` netmask and **no** broadcast/destination
/// address (loopback is not `IFF_BROADCAST`; that member is one union slot, `:6703`-`:6710`). It is
/// the anchor the structural comparison above cannot give: a payload that keeps the right width
/// while carrying the wrong bytes still fails here.
#[test]
fn getifaddrs_pins_the_loopback_node() {
    let case = &cases()[0];
    let mut port = opened(case);
    let reply = port.control(&op(25), &[]);
    assert_eq!(reply[0], INET_REP_OK, "the `[1]` tag: {reply:?}");

    let loopback = loopback_name(&mut port);
    let nodes = parse_ifaddrs(&reply[1..]);
    let node = nodes
        .iter()
        .find(|node| node.name == loopback)
        .unwrap_or_else(|| panic!("no {loopback:?} node in {nodes:?}"));

    // The link-layer pass: the flags record, then the six-byte zero hardware address.
    assert_eq!(
        node.records[0],
        (6, vec![0, 0, 0, 0x15]),
        "the loopback FLAGS record"
    );
    assert_eq!(
        node.records[1],
        (7, vec![0, 6, 0, 0, 0, 0, 0, 0]),
        "the loopback HWADDR record"
    );

    // The `AF_INET` pass: 127.0.0.1 with its /8 netmask and no broadcast/destination address.
    let ipv4 = nodes
        .iter()
        .filter(|node| node.name == loopback)
        .find(|node| {
            node.records
                .iter()
                .any(|(code, payload)| *code == 1 && payload.first() == Some(&INET_AF_INET))
        })
        .unwrap_or_else(|| panic!("no AF_INET {loopback:?} node in {nodes:?}"));
    assert_eq!(
        ipv4.records,
        vec![
            (6, vec![0, 0, 0, 0x15]),
            (1, vec![INET_AF_INET, 127, 0, 0, 1]),
            (5, vec![INET_AF_INET, 255, 0, 0, 0]),
        ],
        "the loopback AF_INET record stream"
    );
}

// -----------------------------------------------------------------------
// `IFGET`(22) — the per-interface option read
// -----------------------------------------------------------------------

/// The `IFGET`/`IFSET` operand: `Name-Length(1) Name Option-Code(1)*`.
fn ifreq_operand(name: &[u8], options: &[u8]) -> Vec<u8> {
    let mut operand = vec![name.len() as u8];
    operand.extend_from_slice(name);
    operand.extend_from_slice(options);
    operand
}

/// The `IFGET` rows of `name` — the same operand/reply list for both drivers, since the two share
/// the `inet_ctl` arm.
///
/// A `None` reply is one this suite cannot spell out host-independently (`MTU` reports the host's
/// own loopback MTU: 65536 on Linux, 16384 on macOS); the oracle comparison pins it everywhere.
fn ifget_rows(name: &[u8]) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
    let flags = vec![INET_REP_OK, 6, 0, 0, 0, 0x15];
    let addr = vec![INET_REP_OK, 1, INET_AF_INET, 127, 0, 0, 1];
    let netmask = vec![INET_REP_OK, 5, INET_AF_INET, 255, 0, 0, 0];
    let broadaddr = vec![INET_REP_OK, 2, INET_AF_INET, 0, 0, 0, 0];
    let dstaddr = vec![INET_REP_OK, 3, INET_AF_INET, 127, 0, 0, 1];
    // `SIOCGIFHWADDR` copies `IFHWADDRLEN` (six) raw bytes behind a big-endian length (`:5861`).
    let hwaddr = vec![INET_REP_OK, 7, 0, 6, 0, 0, 0, 0, 0, 0];
    vec![
        // The name guard: `if ((len == 0) || (namlen > len)) goto error` (`:5831`), plus the
        // determinisation of the `namlen == len` operand the C walks past (measured `[0]einval`).
        (vec![], Some(ctl_xerror("einval"))),
        (vec![1], Some(ctl_xerror("einval"))),
        // An empty interface name with no options: nothing requested, nothing appended.
        (vec![0], Some(ctl_ok())),
        (ifreq_operand(name, &[6]), Some(flags)),
        (ifreq_operand(name, &[1]), Some(addr)),
        (ifreq_operand(name, &[5]), Some(netmask)),
        (ifreq_operand(name, &[4]), None),
        (ifreq_operand(name, &[7]), Some(hwaddr)),
        (ifreq_operand(name, &[2]), Some(broadaddr)),
        (ifreq_operand(name, &[3]), Some(dstaddr)),
        // A code the C's `switch` has no case for is its `default: goto error` ⇒ `[0]einval`.
        (ifreq_operand(name, &[9]), Some(ctl_xerror("einval"))),
        // An interface the kernel does not have: every `SIOCGIF*` fails ⇒ no record — except
        // `FLAGS`, whose failure still reports `0` (`:5955`).
        (
            ifreq_operand(MISSING_INTERFACE, &[6]),
            Some(vec![INET_REP_OK, 6, 0, 0, 0, 0]),
        ),
    ]
}

/// `INET_REQ_IFGET`(22) — `inet_ctl_ifget` (`inet_drv.c:5821`) — answers one `[code][payload]`
/// record per requested option, in operand order.
///
/// Each row is asserted byte for byte where the answer is host-independent and then compared with
/// the real system driver, which is what pins the host-dependent `MTU` record. The interface is the
/// host's **own** loopback name ([`loopback_name`]), so the operands mean the same interface on
/// both sides.
///
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn ifget_answers_the_measured_records_like_the_oracle() {
    for case in cases() {
        let mut port = opened(&case);
        let name = loopback_name(&mut port);

        for (operand, expected) in ifget_rows(&name) {
            let reply = port.control(&op(22), &operand);
            if let Some(expected) = expected {
                assert_eq!(
                    reply, expected,
                    "{}: IFGET of {operand:?} (name {name:?})",
                    case.name
                );
            }
        }

        // The oracle half: the same operands, with the `open` that `IS_OPEN` requires as the first
        // call so both sides enter the arm from the same state.
        let mut calls = vec![Call::req(1, case.open_arg.to_vec(), Rule::Exact)];
        calls.extend(
            ifget_rows(&name)
                .into_iter()
                .map(|(operand, _)| Call::req(22, operand, Rule::Exact)),
        );
        assert_outcomes_agree_with_the_oracle(case.name, &calls, case.bare);
    }
}

// -----------------------------------------------------------------------
// `IFSET`(23) — the per-interface option write
// -----------------------------------------------------------------------

/// The `IFSET` rows of `name`: the C's `goto error` refusals and the well-formed operands.
///
/// **The reply does not depend on the interface's existence.** `inet_ctl_ifset` parses the operand
/// and issues one `SIOCSIF*` per option with `(void) ioctl(…)`, discarding every result
/// (`:6055`-`:6127`), so a fully parsed operand answers `[1]` whether the interface exists or not.
fn ifset_rows(name: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    vec![
        // The name guard, exactly as `ifget`'s.
        (vec![], ctl_xerror("einval")),
        // A name that exactly fills the operand leaves the C's `buf` past `b_end`, so its value
        // loop runs zero times and the arm answers `[1]`; an empty name likewise has nothing to
        // set.
        (vec![1], ctl_ok()),
        (vec![0], ctl_ok()),
        // `FLAGS` takes a bare big-endian 32-bit value.
        (ifreq_operand(name, &[6, 0, 0, 0, 1]), ctl_ok()),
        // …and nothing may follow it but another known option: `1` (ADDR) needs a
        // `buf_to_sockaddr` record (`buf_check(buf, b_end, 4)`), which is missing here.
        (ifreq_operand(name, &[6, 0, 0, 0, 1, 1]), ctl_xerror("einval")),
        // A code the C's `switch` has no case for.
        (ifreq_operand(name, &[9]), ctl_xerror("einval")),
        // `HWADDR` declares a big-endian 16-bit length (`0x0200`) no operand can hold.
        (ifreq_operand(name, &[7, 2, 0, 0]), ctl_xerror("einval")),
        // Truncated `FLAGS`: `buf_check(buf, b_end, 4)` fails with two bytes left.
        (ifreq_operand(name, &[6, 0, 0]), ctl_xerror("einval")),
        // MTU 0 — parsed, then issued as a `(void)`-discarded `SIOCSIFMTU`.
        (ifreq_operand(name, &[4, 0, 0, 0, 0]), ctl_ok()),
    ]
}

/// `INET_REQ_IFSET`(23) — `inet_ctl_ifset` (`inet_drv.c:6017`) — parses the operand and answers
/// `[1]`, or the C's `goto error` ⇒ `[0]einval`.
///
/// # Why the oracle comparison uses a *nonexistent* interface
///
/// The C really does issue the `SIOCSIF*` writes (discarding every result), so probing these
/// operands against the system driver with a live interface name would rewrite that interface on a
/// host where the probe runs as root — a test must never mutate the machine's network
/// configuration. The reply cannot tell the difference (the `(void) ioctl` above), so the oracle
/// half is driven over [`MISSING_INTERFACE`], where every write fails with `ENODEV` and is
/// discarded, while the byte-for-byte half uses the host's own loopback name against **this** floor,
/// whose arm performs no write at all.
///
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn ifset_answers_the_measured_refusals_like_the_oracle() {
    for case in cases() {
        let mut port = opened(&case);
        let name = loopback_name(&mut port);

        for (operand, expected) in ifset_rows(&name) {
            assert_eq!(
                port.control(&op(23), &operand),
                expected,
                "{}: IFSET of {operand:?} (name {name:?})",
                case.name
            );
        }
        // The same rows over an interface the kernel does not have answer identically.
        for (operand, expected) in ifset_rows(MISSING_INTERFACE) {
            assert_eq!(
                port.control(&op(23), &operand),
                expected,
                "{}: IFSET of {operand:?} (missing interface)",
                case.name
            );
        }

        let mut calls = vec![Call::req(1, case.open_arg.to_vec(), Rule::Exact)];
        calls.extend(
            ifset_rows(MISSING_INTERFACE)
                .into_iter()
                .map(|(operand, _)| Call::req(23, operand, Rule::Exact)),
        );
        assert_outcomes_agree_with_the_oracle(case.name, &calls, case.bare);
    }
}
