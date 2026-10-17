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

//! The `inet` drivers' **numeric control-opcode bridge**.
//!
//! The Erlang side never sends a control *verb*: `erlang:port_control(Port, Op, Data)` carries an
//! integer and the C drivers decode it by subtracting the driver magic number from an
//! `unsigned int` (`ERTS_INET_DRV_CONTROL_MAGIC_NUMBER`, `erts/emulator/beam/sys.h:1466`), so
//! what reaches the driver is `Op + 0x03f1a300`:
//!
//! ```c
//! cmd -= ERTS_INET_DRV_CONTROL_MAGIC_NUMBER;   /* both drivers, inet_drv.c:11958 / :14574 */
//! switch (cmd) { case INET_REQ_OPEN: … }
//! ```
//!
//! Because the subtraction happens in **unsigned** arithmetic it **wraps**: an `Op` below the
//! magic number becomes a huge value that matches no `case` and reaches the switch's delegation
//! to `inet_ctl` (`:10988`), whose own `default:` arm answers `[0] ++ "exbadport"`
//! (`:11551`). Measured against the oracle on a bare port: `port_control(P, 1, <<1,2,0>>)` is
//! `[0]exbadport` — it is **not** `INET_REQ_OPEN` — and `port_control(P, 99, <<>>)` is the same.
//!
//! # Two switches, one numberspace
//!
//! The opcodes are one numberspace (`inet_drv.c:800`-`:847`) but each driver entry owns its own
//! switch, so the *same* number can be a case of one driver's switch and of neither for the other:
//!
//! * `tcp_inet_ctl` (`:11953`) — [`Scope::Tcp`]: the socket lifecycle and the stream-only
//!   requests `{OPEN, FDOPEN, LISTEN, CONNECT, ACCEPT, CLOSE, TCP_REQ_RECV, TCP_REQ_UNRECV,
//!   TCP_REQ_SHUTDOWN, TCP_REQ_SENDFILE}`;
//! * `packet_inet_ctl` (`:14570`, the datagram driver) — [`Scope::Udp`]: `{OPEN, FDOPEN, CLOSE,
//!   CONNECT, LISTEN (SCTP only), SCTP_REQ_BINDX, SCTP_REQ_PEELOFF, PACKET_REQ_RECV}`;
//! * `inet_ctl` (`:10988`) — [`Scope::Shared`]: everything both of them delegate, from `BIND` and
//!   the option pair to `PEER`/`NAME`/`GETTYPE`/`GETSTATUS`/`GETFD`/`GETSTAT`.
//!
//! Everything else is a case of **neither** switch and answers the `[0]exbadport` above. Measured:
//! `PACKET_REQ_RECV` (60) on a `tcp_inet` port and `TCP_REQ_RECV` (42) on a `udp_inet` one are both
//! `[0]exbadport`.
//!
//! The named-verb path ([`super`]'s `Driver::control` vocabulary) is untouched by this module: a
//! command that is not a decimal integer is `None` here and falls through to it unchanged.
//!
//! Legacy C ground truth (read-only): `erts/emulator/drivers/common/inet_drv.c`,
//! `erts/emulator/beam/sys.h`.

use beam_common::BeamError;

use crate::error;/// The driver magic number the Erlang side adds to every control opcode
/// (`ERTS_INET_DRV_CONTROL_MAGIC_NUMBER`, `erts/emulator/beam/sys.h:1466`), subtracted again by
/// both `*_inet_ctl` entries (`inet_drv.c:11958`, `:14574`).
pub(super) const CONTROL_MAGIC_NUMBER: u32 = 0x03f1_a300;

/// Which of the C's three control switches carries an opcode's `case`.
///
/// The classification is the C's own: a number is a case of exactly one of
/// `tcp_inet_ctl`/`packet_inet_ctl` when its driver owns the request, otherwise it is delegated
/// to the shared `inet_ctl`, and a number no switch has a case for never matches anything.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Scope {
    /// A case of `tcp_inet_ctl` only (`inet_drv.c:11953`).
    Tcp,
    /// A case of `packet_inet_ctl` only (`inet_drv.c:14570`).
    Udp,
    /// A case of the shared `inet_ctl` (`inet_drv.c:10988`), which both drivers delegate to.
    Shared,
}

/// A decoded `INET_REQ_*`/`TCP_REQ_*`/`PACKET_REQ_*` control opcode (`inet_drv.c:805`-`:847`).
///
/// [`Opcode::Unknown`] is what a wrapped or unassigned number decodes to; it is a variant rather
/// than an `Option::None` so that "this is a numeric opcode, and no switch serves it" stays
/// distinguishable from "this is not an opcode at all" ([`decode`]'s `None`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Opcode {
    /// `INET_REQ_OPEN` (1).
    Open,
    /// `INET_REQ_CLOSE` (2).
    Close,
    /// `INET_REQ_CONNECT` (3).
    Connect,
    /// `INET_REQ_PEER` (4).
    Peer,
    /// `INET_REQ_NAME` (5).
    Name,
    /// `INET_REQ_BIND` (6).
    Bind,
    /// `INET_REQ_SETOPTS` (7).
    Setopts,
    /// `INET_REQ_GETOPTS` (8).
    Getopts,
    /// `INET_REQ_GETSTAT` (11).
    Getstat,
    /// `INET_REQ_GETHOSTNAME` (12).
    Gethostname,
    /// `INET_REQ_FDOPEN` (13).
    Fdopen,
    /// `INET_REQ_GETFD` (14).
    Getfd,
    /// `INET_REQ_GETTYPE` (15).
    Gettype,
    /// `INET_REQ_GETSTATUS` (16).
    Getstatus,
    /// `INET_REQ_GETSERVBYNAME` (17).
    Getservbyname,
    /// `INET_REQ_GETSERVBYPORT` (18).
    Getservbyport,
    /// `INET_REQ_SETNAME` (19).
    Setname,
    /// `INET_REQ_SETPEER` (20).
    Setpeer,
    /// `INET_REQ_GETIFLIST` (21).
    Getiflist,
    /// `INET_REQ_IFGET` (22).
    Ifget,
    /// `INET_REQ_IFSET` (23).
    Ifset,
    /// `INET_REQ_SUBSCRIBE` (24).
    Subscribe,
    /// `INET_REQ_GETIFADDRS` (25).
    Getifaddrs,
    /// `INET_REQ_ACCEPT` (26).
    Accept,
    /// `INET_REQ_LISTEN` (27).
    Listen,
    /// `INET_REQ_IGNOREFD` (28).
    Ignorefd,
    /// `INET_REQ_GETLADDRS` (29).
    Getladdrs,
    /// `INET_REQ_GETPADDRS` (30).
    Getpaddrs,
    /// `TCP_REQ_RECV` (42).
    TcpRecv,
    /// `TCP_REQ_UNRECV` (43).
    TcpUnrecv,
    /// `TCP_REQ_SHUTDOWN` (44).
    TcpShutdown,
    /// `TCP_REQ_SENDFILE` (45).
    TcpSendfile,
    /// `PACKET_REQ_RECV` (60).
    PacketRecv,
    /// A number that is a case of no switch (a wrapped or unassigned opcode).
    Unknown,
}

impl Opcode {
    /// Every opcode with a C number, in `INET_REQ_*` order (the [`Opcode::Unknown`] sentinel is
    /// not one of them).
    pub(super) const ALL: [Opcode; 33] = [
        Opcode::Open,
        Opcode::Close,
        Opcode::Connect,
        Opcode::Peer,
        Opcode::Name,
        Opcode::Bind,
        Opcode::Setopts,
        Opcode::Getopts,
        Opcode::Getstat,
        Opcode::Gethostname,
        Opcode::Fdopen,
        Opcode::Getfd,
        Opcode::Gettype,
        Opcode::Getstatus,
        Opcode::Getservbyname,
        Opcode::Getservbyport,
        Opcode::Setname,
        Opcode::Setpeer,
        Opcode::Getiflist,
        Opcode::Ifget,
        Opcode::Ifset,
        Opcode::Subscribe,
        Opcode::Getifaddrs,
        Opcode::Accept,
        Opcode::Listen,
        Opcode::Ignorefd,
        Opcode::Getladdrs,
        Opcode::Getpaddrs,
        Opcode::TcpRecv,
        Opcode::TcpUnrecv,
        Opcode::TcpShutdown,
        Opcode::TcpSendfile,
        Opcode::PacketRecv,
    ];

    /// The C's request number, or `None` for [`Opcode::Unknown`].
    pub(super) fn code(self) -> Option<u32> {
        let code = match self {
            Opcode::Open => 1,
            Opcode::Close => 2,
            Opcode::Connect => 3,
            Opcode::Peer => 4,
            Opcode::Name => 5,
            Opcode::Bind => 6,
            Opcode::Setopts => 7,
            Opcode::Getopts => 8,
            Opcode::Getstat => 11,
            Opcode::Gethostname => 12,
            Opcode::Fdopen => 13,
            Opcode::Getfd => 14,
            Opcode::Gettype => 15,
            Opcode::Getstatus => 16,
            Opcode::Getservbyname => 17,
            Opcode::Getservbyport => 18,
            Opcode::Setname => 19,
            Opcode::Setpeer => 20,
            Opcode::Getiflist => 21,
            Opcode::Ifget => 22,
            Opcode::Ifset => 23,
            Opcode::Subscribe => 24,
            Opcode::Getifaddrs => 25,
            Opcode::Accept => 26,
            Opcode::Listen => 27,
            Opcode::Ignorefd => 28,
            Opcode::Getladdrs => 29,
            Opcode::Getpaddrs => 30,
            Opcode::TcpRecv => 42,
            Opcode::TcpUnrecv => 43,
            Opcode::TcpShutdown => 44,
            Opcode::TcpSendfile => 45,
            Opcode::PacketRecv => 60,
            Opcode::Unknown => return None,
        };
        Some(code)
    }

    /// The opcode a C request number names, or `None` for a number no switch has a case for.
    ///
    /// The reverse lookup goes through [`Opcode::ALL`]/[`Opcode::code`] so the forward and reverse
    /// directions can never drift apart.
    fn from_code(code: u32) -> Option<Opcode> {
        Opcode::ALL
            .into_iter()
            .find(|opcode| opcode.code() == Some(code))
    }

    /// The typed "not served" error for an opcode the C's own switches DO serve but this floor
    /// does not implement.
    ///
    /// [`Opcode::scope`] names the switch that owns the `case`, so the diagnostic says whether the
    /// gap sits in the driver's own entry (`tcp_inet_ctl`/`packet_inet_ctl`) or in the shared
    /// `inet_ctl`. Both drivers report their gaps through this one function, so the wording cannot
    /// drift between them.
    pub(super) fn not_served(self) -> BeamError {
        let switch = match self.scope() {
            Scope::Tcp => "tcp_inet_ctl",
            Scope::Udp => "packet_inet_ctl",
            Scope::Shared => "inet_ctl",
        };
        error::unsupported(&format!(
            "control opcode `{}` ({switch}) is not served by this driver floor",
            self.name()
        ))
    }
    /// The C's switch that carries this opcode's `case`.
    ///
    /// [`Opcode::Unknown`] belongs to no switch: it reaches `inet_ctl`'s delegation chain and
    /// lands in its `default:` arm, so it is reported as [`Scope::Shared`].
    pub(super) fn scope(self) -> Scope {
        match self {
            Opcode::Open
            | Opcode::Close
            | Opcode::Connect
            | Opcode::Accept
            | Opcode::Listen
            | Opcode::TcpRecv
            | Opcode::TcpUnrecv
            | Opcode::TcpShutdown
            | Opcode::TcpSendfile => Scope::Tcp,
            Opcode::PacketRecv => Scope::Udp,
            _ => Scope::Shared,
        }
    }

    /// The lowercase C name of the request, without its `INET_REQ_`/`TCP_REQ_`/`PACKET_REQ_`
    /// prefix — what a typed "not served" diagnostic names.
    pub(super) fn name(self) -> &'static str {
        match self {
            Opcode::Open => "open",
            Opcode::Close => "close",
            Opcode::Connect => "connect",
            Opcode::Peer => "peer",
            Opcode::Name => "name",
            Opcode::Bind => "bind",
            Opcode::Setopts => "setopts",
            Opcode::Getopts => "getopts",
            Opcode::Getstat => "getstat",
            Opcode::Gethostname => "gethostname",
            Opcode::Fdopen => "fdopen",
            Opcode::Getfd => "getfd",
            Opcode::Gettype => "gettype",
            Opcode::Getstatus => "getstatus",
            Opcode::Getservbyname => "getservbyname",
            Opcode::Getservbyport => "getservbyport",
            Opcode::Setname => "setname",
            Opcode::Setpeer => "setpeer",
            Opcode::Getiflist => "getiflist",
            Opcode::Ifget => "ifget",
            Opcode::Ifset => "ifset",
            Opcode::Subscribe => "subscribe",
            Opcode::Getifaddrs => "getifaddrs",
            Opcode::Accept => "accept",
            Opcode::Listen => "listen",
            Opcode::Ignorefd => "ignorefd",
            Opcode::Getladdrs => "getladdrs",
            Opcode::Getpaddrs => "getpaddrs",
            Opcode::TcpRecv => "tcp_recv",
            Opcode::TcpUnrecv => "tcp_unrecv",
            Opcode::TcpShutdown => "tcp_shutdown",
            Opcode::TcpSendfile => "tcp_sendfile",
            Opcode::PacketRecv => "packet_recv",
            Opcode::Unknown => "unknown",
        }
    }
}

/// Decodes a `Driver::control` command into the opcode it names, or `None` when `command` is not a
/// decimal integer at all (a **named** verb, which the driver's own match still serves).
///
/// The value the Erlang side sent is `Op + CONTROL_MAGIC_NUMBER`, and the C gets back to the
/// opcode with an **`unsigned int`** subtraction that wraps (`inet_drv.c:11958`, `:14574`), so:
///
/// * a decimal integer is truncated to `u32` (the C's `int` → `unsigned int` conversion),
/// * [`CONTROL_MAGIC_NUMBER`] is subtracted **wrapping**,
/// * the result is looked up in the table; a number no switch has a case for is
///   `Some(`[`Opcode::Unknown`]`)` — the `[0]exbadport` arm, never a "not a verb" `badarg`.
///
/// An optional leading `'-'` is accepted (Erlang can send a negative integer; the C's conversion
/// makes it a large `u32` that matches no case). Anything else — the empty string, a name, a
/// string carrying any other non-digit — is `None`, so the named-verb path is reached untouched.
pub(super) fn decode(command: &str) -> Option<Opcode> {
    let value = decimal_to_u32(command)?;
    let code = value.wrapping_sub(CONTROL_MAGIC_NUMBER);
    Some(Opcode::from_code(code).unwrap_or(Opcode::Unknown))
}

/// The `u32` the C's `unsigned int` conversion yields for a decimal command, or `None` when
/// `command` is not a decimal integer.
///
/// The digits are accumulated **modulo 2^32**, which is exactly what `(value as u32)` does for
/// every value an `i64` can hold: `4294967297` truncates to `1`, and a negative literal becomes
/// its two's-complement image (`-1` ⇒ `u32::MAX`). Accumulating rather than parsing an `i64` also
/// keeps an out-of-range literal an unmistakable opcode instead of silently becoming a named verb.
fn decimal_to_u32(command: &str) -> Option<u32> {
    let (negative, digits) = match command.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, command),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let mut value: u32 = 0;
    for digit in digits.bytes() {
        value = value
            .wrapping_mul(10)
            .wrapping_add(u32::from(digit - b'0'));
    }
    Some(if negative { value.wrapping_neg() } else { value })
}

#[cfg(test)]
mod tests {
    use super::{decode, Opcode, Scope, CONTROL_MAGIC_NUMBER};

    #[test]
    fn every_opcode_round_trips_through_its_code() {
        for opcode in Opcode::ALL {
            let code = opcode.code().expect("a C request number");
            let command = (u64::from(code) + u64::from(CONTROL_MAGIC_NUMBER)).to_string();
            assert_eq!(
                decode(&command),
                Some(opcode),
                "{opcode:?} (code {code}) must decode from {command}"
            );
        }
        assert_eq!(Opcode::Unknown.code(), None, "the sentinel has no number");
    }

    #[test]
    fn the_codes_are_strictly_increasing_in_table_order() {
        let codes: Vec<u32> = Opcode::ALL
            .iter()
            .map(|opcode| opcode.code().expect("a C request number"))
            .collect();
        assert!(
            codes.windows(2).all(|pair| pair[0] < pair[1]),
            "the table is in `INET_REQ_*` order: {codes:?}"
        );
    }

    #[test]
    fn the_opcode_table_matches_the_c_request_numbers() {
        // The handful the bridge actually serves, against `inet_drv.c:805`-`:847`.
        assert_eq!(Opcode::Open.code(), Some(1));
        assert_eq!(Opcode::Close.code(), Some(2));
        assert_eq!(Opcode::Connect.code(), Some(3));
        assert_eq!(Opcode::Peer.code(), Some(4));
        assert_eq!(Opcode::Name.code(), Some(5));
        assert_eq!(Opcode::Bind.code(), Some(6));
        assert_eq!(Opcode::Setopts.code(), Some(7));
        assert_eq!(Opcode::Getopts.code(), Some(8));
        assert_eq!(Opcode::Getstat.code(), Some(11));
        assert_eq!(Opcode::Gettype.code(), Some(15));
        assert_eq!(Opcode::Getstatus.code(), Some(16));
        assert_eq!(Opcode::Accept.code(), Some(26));
        assert_eq!(Opcode::Listen.code(), Some(27));
        assert_eq!(Opcode::TcpRecv.code(), Some(42));
        assert_eq!(Opcode::TcpShutdown.code(), Some(44));
        assert_eq!(Opcode::PacketRecv.code(), Some(60));
    }

    #[test]
    fn the_scopes_are_the_c_switches() {
        assert_eq!(Opcode::Accept.scope(), Scope::Tcp);
        assert_eq!(Opcode::Listen.scope(), Scope::Tcp);
        assert_eq!(Opcode::TcpRecv.scope(), Scope::Tcp);
        assert_eq!(Opcode::PacketRecv.scope(), Scope::Udp);
        assert_eq!(Opcode::Bind.scope(), Scope::Shared);
        assert_eq!(Opcode::Gettype.scope(), Scope::Shared);
        assert_eq!(
            Opcode::Unknown.scope(),
            Scope::Shared,
            "no switch has a case for it; it reaches inet_ctl's default arm"
        );
    }

    #[test]
    fn the_magic_offset_decodes_and_a_wrapped_or_unknown_opcode_does_not() {
        // The opcode the magic offset renames.
        assert_eq!(
            decode(&(1 + i64::from(CONTROL_MAGIC_NUMBER)).to_string()),
            Some(Opcode::Open)
        );
        // Below the magic number the `unsigned int` subtraction wraps: no case matches.
        for command in ["1", "99", "-1"] {
            assert_eq!(
                decode(command),
                Some(Opcode::Unknown),
                "{command} wraps to a huge value that matches no case"
            );
        }
        // The truncation is the C's `int` -> `unsigned int` one.
        assert_eq!(
            decode(&u32::MAX.to_string()),
            Some(Opcode::Unknown),
            "u32::MAX - magic matches no case either"
        );
        // An unassigned number above the magic offset.
        assert_eq!(
            decode(&(9 + i64::from(CONTROL_MAGIC_NUMBER)).to_string()),
            Some(Opcode::Unknown),
            "9 is a retired request number (`:811`)"
        );
    }

    #[test]
    fn a_named_verb_is_not_an_opcode() {
        assert_eq!(decode("open"), None);
        assert_eq!(decode(""), None);
        assert_eq!(decode("-"), None);
        assert_eq!(decode("1x"), None);
        assert_eq!(decode("1.0"), None);
        assert_eq!(decode("+7"), None);
        assert_eq!(decode("open\0"), None);
    }

    #[test]
    fn the_name_is_the_c_request_without_its_prefix() {
        assert_eq!(Opcode::Open.name(), "open");
        assert_eq!(Opcode::TcpShutdown.name(), "tcp_shutdown");
        assert_eq!(Opcode::PacketRecv.name(), "packet_recv");
        assert_eq!(Opcode::Unknown.name(), "unknown");
    }
}
