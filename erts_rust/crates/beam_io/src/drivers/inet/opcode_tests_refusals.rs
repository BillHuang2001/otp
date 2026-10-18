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
    assert_outcomes_agree_with_the_oracle, bare_port as tcp_bare_port, erl_stdout, op,
    open_arg as tcp_open_arg, oracle_expression, oracle_replies, Call, Rig, Rule,
};use super::opcode_tests_udp::{bare_port as udp_bare_port, open_arg as udp_open_arg};
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

        // A real `AF_INET` socket passes the pre-adoption ladder, so the C reaches the adoption —
        // but this port **already holds a socket**, and the C's arm would overwrite `desc->s`
        // without closing it (`inet_drv.c:5267`), leaking the previous one. This floor refuses a
        // second adoption instead and leaves the live socket untouched; the adoption **success**
        // path from a socket-less port is
        // [`fdopen_adopts_a_bound_stream_socket_with_the_c_prebound_semantics`].
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("a loopback datagram socket");
        assert_eq!(
            port.control(&op(13), &bound_operand(type_byte, socket.as_raw_fd())),
            ctl_xerror("einval"),
            "{}: a port that already holds a socket refuses a second adoption",
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

/// The `bound == 0` form of `INET_REQ_FDOPEN`(13) — the operand that reaches the C's **blind**
/// adoption.
///
/// With a ten-byte operand whose `bound` word is zero the C takes the descriptor over with **no
/// syscall at all** (`inet_drv.c:5264`-`:5266`), which is why it answers `[1]` even for a garbage
/// number (measured `[1]` for `<<1,2,0,0,0,9,0,0,0,0>>`). This floor adopts on the same terms now
/// (the prebound path), but the descriptor still has to reach the poller, so a number that is not
/// open at all is refused **naming its errno** — `[0]"ebadf"`, the `fcntl(2)` verdict — rather
/// than the C's fabricated `[1]` (or, as this row asserted before the errno channel existed, a
/// bare typed error the caller could not name). That is the honest answer and the one thing this
/// row pins: a blind adoption never invents a success. The measured divergence against the oracle
/// is pinned in
/// [`fdopen_errno_tests::the_blind_bound_zero_form_refuses_by_errno_where_the_c_answers_ok`].
///
/// The refusal rows of the same arm (a closed number with `bound != 0` ⇒ `[0]ebadf`, a
/// non-socket ⇒ `[0]enotsock`) live in
/// [`fdopen_bound_operands_answer_the_c_pre_adoption_refusals`]; the **success** path lives in
/// [`fdopen_adopts_a_bound_stream_socket_with_the_c_prebound_semantics`].
#[test]
fn a_fdopen_with_bound_zero_never_fabricates_an_adoption_reply() {
    for case in cases() {
        // A **socket-less** port, so the "already holds a socket" refusal cannot mask the answer.
        let mut port = (case.bare)();
        for type_byte in case.types {
            let mut operand = vec![INET_AF_INET, *type_byte];
            operand.extend_from_slice(&CLOSED_FD.to_be_bytes());
            operand.extend_from_slice(&0i32.to_be_bytes());
            assert_eq!(
                port.control(&op(13), &operand),
                ctl_xerror("ebadf"),
                "{}: a `bound == 0` adoption of a descriptor that is not open names the \
                 `fcntl(2)` errno it could not serve, never a fabricated `[1]`",
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

// -----------------------------------------------------------------------
// `INET_REQ_FDOPEN`(13) — the prebound **adoption** success path
// -----------------------------------------------------------------------
//
// The C's adoption body is `inet_ctl_fdopen` (`inet_drv.c:5223`-`:5282`): it stores the caller's
// descriptor, registers it, sets `INET_STATE_OPEN` (promoted to `INET_STATE_CONNECTED` when a
// `SOCK_STREAM` adoption answers `sock_peer`), records `desc->stype`/`desc->sfamily`, and sets
// `desc->prebound = 1` so the descriptor is **never** `close(2)`d — a prebound close only drops the
// select registration (`erl_inet_close`, `:5102`-`:5117`).
//
// Every row below mints its own descriptor **with an owning socket object held for the whole test**
// and hands the driver `as_raw_fd()`, so the assertions about "the caller's descriptor is still
// open" are observable through [`super::sys`] rather than inferred.
//
// The sockets are declared **before** the `Rig` in each test so they outlive the port: locals drop
// in reverse declaration order, so the driver is dropped (with its prebound-aware `Drop`) before the
// descriptor's owner closes it.

/// The `fdopen` operand of an **explicitly bound** descriptor: `[af, type, fd_be32, bound_be32]`.
///
/// [`bound_operand`] is the `bound = 1` case of this; the helper stays separate because the
/// refusal suite only ever needs that one form.
fn fdopen_bound_operand(type_byte: u8, fd: i32, bound: i32) -> Vec<u8> {
    let mut operand = vec![INET_AF_INET, type_byte];
    operand.extend_from_slice(&fd.to_be_bytes());
    operand.extend_from_slice(&bound.to_be_bytes());
    operand
}

/// `INET_REQ_GETTYPE`(15)'s reply for a socket of `stype`: `[1] ++ af_be32 ++ stype_be32`
/// (`inet_drv.c:11165`-`:11203`).
fn gettype_reply(stype: u8) -> Vec<u8> {
    let mut reply = ctl_ok();
    reply.extend_from_slice(&u32::from(INET_AF_INET).to_be_bytes());
    reply.extend_from_slice(&u32::from(stype).to_be_bytes());
    reply
}

/// `INET_REQ_GETSTATUS`(16)'s reply for a lifecycle state: `[1] ++ state_be32`
/// (`inet_drv.c:11128`). The code is [`super::InetState::state_code`], i.e. the C's `INET_STATE_*`
/// bit set, so the expected values are spelled as states rather than as magic numbers — measured
/// against the oracle: `Open` ⇒ 1, `Connected` ⇒ 5, `Listening` ⇒ 9.
fn getstatus_reply(state: super::InetState) -> Vec<u8> {
    let mut reply = ctl_ok();
    reply.extend_from_slice(&state.state_code().to_be_bytes());
    reply
}
/// `INET_REQ_NAME`(5)/`INET_REQ_PEER`(4)'s reply for `127.0.0.1:port`: the packed
/// `[1, INET_AF_INET, port_hi, port_lo, 127, 0, 0, 1]`.
fn loopback_endpoint(port: u16) -> Vec<u8> {
    let [hi, lo] = port.to_be_bytes();
    let mut reply = ctl_ok();
    reply.extend_from_slice(&[INET_AF_INET, hi, lo, 127, 0, 0, 1]);
    reply
}

/// Asserts that `fd` is still a live, bound `AF_INET` socket listening on `bound` — i.e. that
/// nothing in this crate issued a `close(2)` on the caller's descriptor.
fn assert_still_open_and_bound(fd: std::os::unix::io::RawFd, bound: u16, label: &str) {
    let family = super::sys::socket_family(fd)
        .unwrap_or_else(|err| panic!("{label}: the caller's descriptor is still a socket: {err}"));
    assert_eq!(
        libc::c_int::from(family),
        libc::AF_INET,
        "{label}: the caller's descriptor is still an AF_INET socket"
    );
    assert_eq!(
        super::sys::local_port(fd).unwrap_or_else(|err| panic!(
            "{label}: the caller's descriptor still answers getsockname(2): {err}"
        )),
        bound,
        "{label}: the caller's descriptor is still bound to its own port"
    );
}

/// `INET_REQ_FDOPEN`(13) on a **bound, unconnected** `AF_INET`/`SOCK_STREAM` descriptor — the C's
/// adoption success path and the state it leaves behind.
///
/// `inet_ctl_fdopen` registers the descriptor, caches `sfamily = AF_INET`, `stype = SOCK_STREAM`
/// (`:5265`) and probes `sock_peer` (`:5252`-`:5260`): a bound socket with no peer answers
/// `INET_STATE_OPEN`, measured `[1,0,0,0,1]`, and `NAME` then names the caller's real endpoint.
/// `LISTEN` on the adopted descriptor really listens — it is the caller's socket, so the syscall
/// lands on it.
///
/// **Rust-side only**: the operand names a descriptor **this** process owns, so an `erl` probe (a
/// second process) cannot see it. The process-independent rows of the same arm are compared with
/// the oracle in [`the_fdopen_operand_refusals_agree_with_the_oracle`].
#[test]
fn fdopen_adopts_a_bound_stream_socket_with_the_c_prebound_semantics() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let bound = listener.local_addr().expect("the listener's address").port();
    let fd = listener.as_raw_fd();

    let mut port = tcp_bare_port();
    // The six-byte form: `len == 6 ⇒ bound = 1` (`:5264`), so `sock_name(2)` runs first.
    let mut operand = vec![INET_AF_INET, INET_TYPE_STREAM];
    operand.extend_from_slice(&fd.to_be_bytes());
    assert_eq!(
        port.control(&op(13), &operand),
        ctl_ok(),
        "INET_REQ_FDOPEN adopts the caller's descriptor"
    );
    // `GETTYPE` reports the cache the adoption wrote — a stream socket of the IPv4 family.
    assert_eq!(
        port.control(&op(15), &[]),
        gettype_reply(INET_TYPE_STREAM),
        "the adopted descriptor's cached type"
    );
    // No peer: the C's `sock_peer` probe fails, so the lifecycle state stays `INET_STATE_OPEN`.
    assert_eq!(
        port.control(&op(16), &[]),
        getstatus_reply(super::InetState::Open),
        "a bound, unconnected adoption is Open"
    );
    // The descriptor is the caller's own socket, so `NAME` asks the kernel and reports its real
    // bound endpoint rather than a fabricated one.
    assert_eq!(
        port.control(&op(5), &[]),
        loopback_endpoint(bound),
        "the adopted descriptor's real local endpoint"
    );
    // `LISTEN` really listens on it, exactly as the C's `INET_REQ_LISTEN` would.
    assert_eq!(port.control(&op(27), &[0, 5]), ctl_ok(), "INET_REQ_LISTEN on the adoption");
    assert_eq!(
        port.control(&op(16), &[]),
        getstatus_reply(super::InetState::Listening),
        "after LISTEN the port reports INET_STATE_LISTENING"
    );
    // The caller's descriptor is untouched by all of that.
    assert_still_open_and_bound(fd, bound, "after a successful adoption");
}

/// `INET_REQ_FDOPEN`(13) on an **accepted connection** — the C's `INET_STATE_CONNECTED` branch.
///
/// The only difference from the bound case is the `sock_peer` probe: it succeeds, so
/// `inet_ctl_fdopen` promotes the state to `INET_STATE_CONNECTED` (`:5247`-`:5260`), measured
/// `[1,0,0,0,5]`, and `PEER` reports the real far end.
#[test]
fn fdopen_adopts_a_connected_stream_socket_as_connected() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let peer = std::net::TcpStream::connect(listener.local_addr().expect("an address"))
        .expect("the far end connects");
    let (server, peer_addr) = listener.accept().expect("the port's end is accepted");
    let fd = server.as_raw_fd();
    let bound = server.local_addr().expect("the accepted socket's address").port();

    let mut port = tcp_bare_port();
    assert_eq!(
        port.control(&op(13), &bound_operand(INET_TYPE_STREAM, fd)),
        ctl_ok(),
        "INET_REQ_FDOPEN adopts the accepted connection"
    );
    assert_eq!(
        port.control(&op(16), &[]),
        getstatus_reply(super::InetState::Connected),
        "a connected adoption is INET_STATE_CONNECTED"
    );
    assert_eq!(
        port.control(&op(4), &[]),
        loopback_endpoint(peer_addr.port()),
        "PEER reports the real far end"
    );
    assert_eq!(
        port.control(&op(5), &[]),
        loopback_endpoint(bound),
        "NAME reports the real local end"
    );
    drop(peer);
    assert_still_open_and_bound(fd, bound, "after adopting a connection");
}

/// The **prebound invariant**: neither the driver's own `close` verb nor `Drop` may close a
/// descriptor the caller handed over.
///
/// The C's `erl_inet_close` (`inet_drv.c:5102`-`:5117`) is explicit about it — for a prebound
/// descriptor it only drops the select registration (`sock_select(..., 0)`), leaves `desc->s` and
/// `desc->state` alone and issues no `close(2)`. This test drives the numeric `INET_REQ_CLOSE`(2)
/// (the **name** `"close"` is reserved by [`crate::PortControl`] for teardown, so it never reaches
/// the verb), then drops the whole registry — the driver's `Drop` — and checks the descriptor
/// after each step.
#[test]
fn a_prebound_descriptor_is_never_closed_by_the_close_verb_or_by_drop() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let bound = listener.local_addr().expect("the listener's address").port();
    let fd = listener.as_raw_fd();

    {
        let mut port = tcp_bare_port();
        assert_eq!(
            port.control(&op(13), &bound_operand(INET_TYPE_STREAM, fd)),
            ctl_ok(),
            "INET_REQ_FDOPEN adopts the caller's descriptor"
        );
        assert_eq!(
            port.control(&op(2), &[]),
            ctl_ok(),
            "INET_REQ_CLOSE answers [1] for a prebound descriptor"
        );
        // The descriptor is the caller's, so it survived the close...
        assert_still_open_and_bound(fd, bound, "after the close verb");
        // ...and the port kept its state (`desc->state` is untouched by a prebound close), which is
        // why `GETSTATUS` still reports the same code.
        assert_eq!(
            port.control(&op(16), &[]),
            getstatus_reply(super::InetState::Open),
            "a prebound close leaves the lifecycle state alone"
        );
    }
    // Dropping the `Rig` drops the `PortRegistry`, the `Port` and finally the driver, whose `Drop`
    // runs the same prebound-aware teardown.
    assert_still_open_and_bound(fd, bound, "after the driver was dropped");

    // The strongest form of the invariant: the caller's socket is still fully usable — the kernel
    // completes a connection to it, which it could not do for a closed or unlistening descriptor.
    let peer = std::net::TcpStream::connect(listener.local_addr().expect("an address"))
        .expect("the caller's listener still accepts connections");
    drop(peer);
}

/// A second `INET_REQ_FDOPEN` on a port that **already holds a socket** is refused, and the live
/// socket is left untouched.
///
/// The C's arm assigns `desc->s = s` with no `desc_close(desc)` first (`:5267`), so a second
/// adoption overwrites — and leaks — the first descriptor. Refusing loudly is the honest
/// alternative: `[0]einval`, with the existing socket still answering for the port.
#[test]
fn fdopen_on_a_port_that_already_has_a_socket_is_refused() {
    for case in cases() {
        let mut port = opened(&case);
        let before = port.control(&op(16), &[]);
        let socket =
            std::net::UdpSocket::bind("127.0.0.1:0").expect("a loopback datagram socket");

        assert_eq!(
            port.control(&op(13), &bound_operand(case.types[0], socket.as_raw_fd())),
            ctl_xerror("einval"),
            "{}: a port that already holds a socket refuses a second adoption",
            case.name
        );
        // The live socket still answers for the port — its state did not move and it still reports
        // its own cached type.
        assert_eq!(
            port.control(&op(16), &[]),
            before,
            "{}: the existing socket's state is untouched",
            case.name
        );
        assert_eq!(
            port.control(&op(15), &[]),
            gettype_reply(case.types[0]),
            "{}: the existing socket's cached type is untouched",
            case.name
        );
    }
}

/// `INET_REQ_FDOPEN`(13) on a `udp_inet` port: the adopted datagram descriptor is wired into the
/// data path, so the driver's own `send`/`recv` verbs move real datagrams across it.
///
/// This is what separates a real adoption from a fabricated `[1]`: the port's socket **is** the
/// caller's descriptor, so a datagram the driver sends arrives at the far end coming from the
/// caller's bound port, and one the far end sends back is read by the driver's `recv` verb
/// (payload into the port input queue, source address in the reply).
#[test]
fn fdopen_adopts_a_datagram_socket_that_carries_real_data() {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("a loopback datagram socket");
    let local = socket.local_addr().expect("the adopted socket's address");
    let far = std::net::UdpSocket::bind("127.0.0.1:0").expect("a second loopback datagram socket");
    let far_addr = far.local_addr().expect("the far address");
    let fd = socket.as_raw_fd();

    let mut port = udp_bare_port();
    // The ten-byte form with an explicit `bound` word of 1: the same `sock_name(2)`/family ladder
    // as the six-byte form.
    assert_eq!(
        port.control(&op(13), &fdopen_bound_operand(INET_TYPE_DGRAM, fd, 1)),
        ctl_ok(),
        "udp_inet INET_REQ_FDOPEN adopts the caller's descriptor"
    );
    assert_eq!(
        port.control(&op(15), &[]),
        gettype_reply(INET_TYPE_DGRAM),
        "the adopted descriptor's cached type is the datagram one"
    );
    // The C probes `sock_peer` only for a `SOCK_STREAM` adoption (`:5252`), so a datagram socket is
    // always `INET_STATE_OPEN` here — never `Connected`, even though a datagram socket may have a
    // default peer.
    assert_eq!(
        port.control(&op(16), &[]),
        getstatus_reply(super::InetState::Open),
        "a datagram adoption is Open"
    );
    // `NAME` asks the kernel through the adopted descriptor.
    assert_eq!(
        port.control(&op(5), &[]),
        loopback_endpoint(local.port()),
        "the adopted datagram socket's real local endpoint"
    );

    // The send path: the `send` verb's `[packed address][payload]` grammar, through the adopted
    // descriptor.
    let mut datagram = vec![INET_AF_INET];
    datagram.extend_from_slice(&far_addr.port().to_be_bytes());
    datagram.extend_from_slice(&[127, 0, 0, 1]);
    datagram.extend_from_slice(b"hello");
    assert_eq!(
        port.control("send", &datagram),
        ctl_ok(),
        "the send verb writes through the adopted descriptor"
    );
    let mut buf = [0u8; 32];
    let (len, from) = far.recv_from(&mut buf).expect("the far end receives it");
    assert_eq!(&buf[..len], b"hello", "the payload crossed the adopted descriptor");
    assert_eq!(
        from.port(),
        local.port(),
        "it came from the caller's own bound port"
    );

    // The receive path: the `recv` verb reads through the adopted descriptor, reports the source
    // and hands the payload to the port's input queue.
    far.send_to(b"world", local).expect("the far end sends back");
    let reply = port.control("recv", &[]);
    assert_eq!(
        reply.len(),
        8,
        "a received datagram answers [1] ++ its 8-byte source address: {reply:?}"
    );
    assert_eq!(reply[0], INET_REP_OK, "the result tag: {reply:?}");
    assert_eq!(reply[1], INET_AF_INET, "the source family tag: {reply:?}");
    assert_eq!(
        u16::from_be_bytes([reply[2], reply[3]]),
        far_addr.port(),
        "the source port: {reply:?}"
    );
    assert_eq!(&reply[4..8], &[127, 0, 0, 1], "the source octets: {reply:?}");
    assert_eq!(
        port.take_input(5),
        b"world",
        "the datagram's payload rides the port input queue"
    );

    // The refused type byte keeps its exact refusal on a **socket-less** port (the ladder runs
    // before the adoption, so nothing was ever adopted here).
    let mut bare = udp_bare_port();
    assert_eq!(
        bare.control(&op(13), &bound_operand(3, fd)),
        ctl_xerror("einval"),
        "a type tag the datagram switch does not admit"
    );
}