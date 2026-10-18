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

//! [`UdpInetDriver`] — the built-in `udp_inet` datagram socket driver.
//!
//! The Rust counterpart of the legacy C driver's **UDP half**: the driver `gen_udp`/`inet_udp`
//! sit on. It owns **one** `AF_INET`/`SOCK_DGRAM` socket, registers it with the poller for read
//! readiness, and moves datagrams between the socket and the port.
//!
//! The C keeps its UDP and TCP drivers in one file (`erts/emulator/drivers/common/inet_drv.c`)
//! and shares every control-verb decoder between them; the two entry points differ only in the
//! socket type they create — `udp_inet_driver_entry` (`:1406`-`:1419`) starts through
//! `udp_inet_start` (`:14515`) → `packet_inet_start(port, args, IPPROTO_UDP)` (`:14496`). This
//! floor splits the *drivers* (`tcp_inet` in [`super`], `udp_inet` here) while keeping the
//! **wire vocabulary shared**, so the reply/error classes and the address codec stay
//! single-sourced in [`super::wire`] and the option wire in [`super::opt`].
//!
//! # Registered name and the control vocabulary
//!
//! The driver registers under `"udp_inet"` — what the oracle's `port_info(Port, name)` reports
//! for a `gen_udp:open/1` socket, and the C entry `udp_inet_driver_entry` (`:1406`).
//!
//! [`Driver::control`] serves **both** wire forms: a decimal command is the C's `unsigned int`
//! control opcode, decoded with wrapping offset subtraction ([`super::opcode`]) and routed to the
//! verb its `case` selects; a non-decimal command is a **named** verb for a direct caller. The
//! *operand* is the C's packed request payload in both forms and is decoded here. The verbs, and
//! the C request each one mirrors, are:
//!
//! | verb | C request | argument | reply |
//! |---|---|---|---|
//! | `open` | `INET_REQ_OPEN` (`:14577`) | `[AF, TYPE, PROTO]` (3 bytes) | `[1]`, `[0] ++ "eafnosupport"`, `[0] ++ "einval"` or `[0] ++ "eprotonosupport"` |
//! | `bind` | `INET_REQ_BIND` (`:11409`) | packed address | `[1, port_hi, port_lo]` |
//! | `connect` | `INET_REQ_CONNECT` (`:14710`) | packed address (the C's 4-byte timeout prefix is accepted and skipped) | `[1]`, or `[0] ++ errno` |
//! | `send` | `packet_inet_command` (`:15037`) | `[address][anc_len:be32][data]`, optionally behind a `prim_inet` caller-reference prefix `[n:be16][tag]` | `[1]`, or `[0] ++ errno` |
//! | `recv` | `PACKET_REQ_RECV` (`:14968`) | — (read-now) | `[1] ++ src_address` with a datagram, the bare `[1]` when none is pending, `[0] ++ errno` on failure |
//! | `close` | `INET_REQ_CLOSE` (`:14701`) | — | `[1]` |
//! | `setopts` | `INET_REQ_SETOPTS` (`:11111`) | option records `[opt, val_be32]*` | `[1]`, or `[0] ++ "einval"` |
//! | `getopts` | `INET_REQ_GETOPTS` (`:11046`) | option codes `[opt]*` | `[1] ++ [opt, val_be32]*`, or `[0] ++ "einval"` |
//!
//! A reply is the C's `ctl_reply` shape, exactly as [`super`] documents it: a one-byte result tag
//! ([`super::INET_REP_OK`] / [`super::INET_REP_ERROR`], `:869`-`:871`) followed by a payload. An
//! **operational** failure is that `[0]`-tagged reply carrying the lowercase errno name the
//! oracle uses (`erl_errno_id`, `:2249`); a **malformed argument** or an **unknown verb** is
//! [`beam_common::BeamError::BadArg`], the crate's argument-fault convention (the C answers
//! `ctl_error(EINVAL)` for the former).
//!
//! # The reception model (the one deliberate divergence)
//!
//! A datagram's **source address** cannot ride the port's input queue — [`crate::ByteQueue`] is
//! byte-only — and [`crate::PortNotification`] must keep exactly its two variants (an outside
//! crate matches it exhaustively). So reception here is **verb-driven**:
//!
//! * `recv` performs **one** `recvfrom(2)` into a 64 KiB buffer (the largest IPv4 UDP payload).
//!   On a datagram it pushes the payload to the port input queue (the runtime harvests it as a
//!   [`crate::PortNotification::Data`] and forms `{udp, S, IP, Port, Data}`) and answers
//!   `[1] ++ [INET_AF_INET, port_hi, port_lo, a, b, c, d]` — eight bytes — carrying the source.
//!   On `EAGAIN` it answers the bare `[1]` (one byte) and delivers nothing, exactly as
//!   [`super::TcpInetDriver`]'s drained-socket reply does; the reply **length** distinguishes
//!   the two. A real syscall error is `[0] ++ errno_name`.
//! * `ready_input` is a **documented no-op**. The socket still registers read readiness, so the
//!   runtime observes the event and issues `recv`; the driver cannot push anything itself.
//!
//! This cannot mirror the oracle's `PACKET_REQ_RECV` byte for byte: the C's code-60 request accepts the `Timeout(4), Length(4)` payload, enqueues an async operation on the descriptor's `INET_MAX_ASYNC = 1` queue, and answers the three-byte async-op id `[1] ++ async_id_be16` — measured against the oracle as `[1,0,0]`, then `[1,0,1]` on the next call — with the datagram itself arriving later as an `{inet_async, Port, Ref, Result}` message.
//! Only the **empty** argument is `[0]einval`, which the C rejects without enqueuing anything.
//! With no driver→owner message path, the floor replaces that whole asynchronous request/reply exchange with the synchronous read-now verb above, delivering the datagram in the reply itself.
//!
//! # What the real `gen_udp` still needs
//!
//! Everything below is **absent by design** and answers a clear error rather than a wrong
//! result; each names what it would require:
//!
//! * **`{active, true|once|N}` datagram delivery** — the option is stored and reported by the
//!   shared [`super::opt`] store, but the driver never delivers input on its own: bytes land in
//!   the port input queue and the runtime decides when to fetch them. Active-mode delivery is
//!   the C's `tcp_deliver`/`tcp_recv` loop driving the port's *output* side itself plus the
//!   `{deliver, port|term}` mode — a driver-owned message path, deliberately outside
//!   [`crate::DriverContext`]'s reach (it exposes only the port id, the poller and the two byte
//!   queues). `{udp, S, IP, Port, Data}` is therefore never fabricated here.
//! * **Ancillary data (`recvmsg`/`sendmsg` `IP_PKTINFO`, cmsg flags)** — the C's UDP halves keep
//!   `recv_cmsgflags`/ancillary state; out of scope, so this floor uses `recvfrom(2)`/`sendto(2)`
//!   and answers a typed error rather than dropping the ancillary half silently.
//! * **The async-op queue** — `INET_MAX_ASYNC = 1` (`:1069`), `enq_async`, the
//!   `[1] ++ async_id_be16` reply and the later `{inet_async, Port, Ref, Result}` message. The
//!   C's `recv`/`connect` are asynchronous request/reply exchanges through it; this floor's are
//!   synchronous, which is also why `recv` rejects the C's `Timeout(4), Length(4)` payload with
//!   `BadArg` rather than accepting arguments it cannot honor.
//! * **`INET_REQ_PEER` (code `4`)** — the UDP half does not implement the peer query, so it is
//!   not a verb here at all (it is reached only through the control code, which this node does
//!   not decode).
//! * **The `{inet_reply, S, Reply, Ref}` echo of the caller reference** — `send` **accepts** the
//!   `prim_inet` caller-reference prefix and strips it (see
//!   [`UdpInetDriver::verb_send`]), but the C echoes the tag back as that message's fourth
//!   element (`inet_reply_caller_ref`, `:2610`-`:2660`), which needs a driver→owner message
//!   channel this crate does not have. The reply stays the synchronous `ctl_reply` bytes.
//! * **IPv6, SCTP and Unix-domain sockets, and DNS resolution** — `open` accepts only
//!   `AF_INET`/`SOCK_DGRAM` and `bind`/`connect`/`send` only the packed
//!   IPv4/`INADDR_ANY`/`INADDR_LOOPBACK` forms.
//!
//! # Deliberate non-mirrors
//!
//! * **`open` of a second socket on a live port** answers `[0]einval` where the C's
//!   `inet_ctl_open` answers `[0]exbadseq` (`:5130`); the same choice
//!   [`super::TcpInetDriver`] makes, so both drivers refuse identically instead of leaking the
//!   previous socket.
//! * **`open` accepts only the datagram type.** The C's `packet_inet_ctl` accepts
//!   `INET_TYPE_STREAM` for *either* entry point (it is one driver file), so the oracle's
//!   `udp_inet` port answers `[1]` to `open [AF_INET, SOCK_STREAM, …]`. This floor's
//!   `udp_inet` is a datagram driver, so a stream request is `[0]einval`.
//! * **A `getopts` on a socket-less port** answers a well-formed reply (the driver-state
//!   records, and no record for a syscall-backed code). The C's `inet_fill_opts` can walk a
//!   `NULL` pointer there and answer `[0]exbadseq`; that bug is deliberately **not**
//!   reproduced.
//!
//! # Ownership
//!
//! The driver **owns** its descriptor: [`UdpInetDriver::from_socket`] adopts a caller's socket,
//! `open` creates its own, and either is closed on `stop`, on the `close` verb and on `Drop`;
//! nothing else may close it. All raw socket syscalls live in the private [`super::sys`]
//! submodule and every `unsafe` block there carries a `// SAFETY:` rationale.
//!
//! Legacy C ground truth (read-only): `erts/emulator/drivers/common/inet_drv.c` (the UDP halves
//! `packet_inet_start` `:14496`, `udp_inet_start` `:14515`, `packet_inet_stop` `:14533`,
//! `packet_inet_ctl` `:14564`, `packet_inet_command` `:15037`, `packet_inet_input` `:15361`) and
//! the Erlang half `erts/preloaded/src/prim_inet.erl`.

use std::os::unix::io::RawFd;

use beam_common::Result;

use crate::driver::{Driver, DriverContext};
use crate::drivers::fd::set_nonblocking;
use crate::error;
use crate::poller::{Interest, Token};

use super::opt::{get_options, set_options, SocketOptions};
use super::opcode::{decode as decode_opcode, Opcode};
use super::sys::{
    bind_socket, close_fd, connect_socket, dgram_socket_fd, has_peer, hostname, local_port,
    recv_from, send_connected, send_to, socket_type,
};
use super::wire::{
    ctl_error, ctl_ok, ctl_xerror, pack_address, parse_address, split_address, split_caller_ref,
    split_datagram, DatagramCommand, InetAddr, INET_AF_ANY, INET_AF_INET, INET_AF_LOOPBACK,
    INET_PROTO_DEFAULT, INET_PROTO_UDP, INET_TYPE_DGRAM, INET_TYPE_STREAM,
};
use super::InetState;

/// The driver's registered name — the C `udp_inet` entry (`inet_drv.c:1406`), which is also what
/// the oracle reports from `port_info(Port, name)` for a `gen_udp:open/1` socket.
pub const UDP_DRIVER_NAME: &str = "udp_inet";

/// The size of the buffer a single `recv` reads a datagram into: 64 KiB, which exceeds the
/// largest IPv4 UDP payload (65507 bytes) so a datagram is never truncated.
const DATAGRAM_CHUNK: usize = 64 * 1024;

/// The UDP socket driver: one `AF_INET`/`SOCK_DGRAM` socket behind one port.
///
/// Constructed either socket-less ([`UdpInetDriver::new`], the [`crate::DriverRegistry`] factory
/// the runtime uses, which then calls the `open` verb) or around an existing socket
/// ([`UdpInetDriver::from_socket`]).
pub struct UdpInetDriver {
    /// The socket this port owns, if one has been opened.
    socket: Option<RawFd>,
    /// The poller token of the socket's registration, once registered.
    token: Option<Token>,
    /// The interest mask currently registered for the socket. A datagram socket is **one**
    /// descriptor, so read and write readiness share the single token.
    interest: Interest,
    /// The lifecycle state: [`InetState::Open`] once `open` created the socket,
    /// [`InetState::Connected`] after a successful `connect`, [`InetState::Closed`] otherwise.
    state: InetState,
    /// The driver-state socket options the `setopts`/`getopts` verbs store and report
    /// ([`SocketOptions`]). The driver's socket is always a datagram socket, so the shared
    /// store is always driven with its stream gate off.
    options: SocketOptions,
    /// The C descriptor's **cached** address family (`desc->sfamily`) as a wire tag, `0` meaning
    /// "unset". `INET_REQ_GETTYPE` reports this cache rather than probing the socket
    /// (`inet_drv.c:11165`-`:11203`), so it is written when a socket is created or adopted
    /// (`inet_ctl_open`, `:5214`; `inet_ctl_fdopen`, `:5265`) and **never** cleared by a close.
    family: u16,
    /// The C descriptor's **cached** socket type (`desc->stype`), same lifecycle as
    /// [`UdpInetDriver::family`] — this is what makes a closed port still report
    /// `[1,0,0,0,1,0,0,0,2]` where a never-opened one reports `[0]einval`.
    stype: u16,
    /// The C's `desc->name_ptr`/`name_addr`: a **fake** local address installed by
    /// `INET_REQ_SETNAME` (`inet_drv.c:11386`), which `INET_REQ_NAME` reports in place of
    /// `getsockname(2)` until an empty `SETNAME` clears it (`:11351`-`:11360`).
    name_addr: Option<InetAddr>,
    /// The C's `desc->peer_ptr`/`peer_addr`: the `INET_REQ_SETPEER` counterpart of
    /// [`UdpInetDriver::name_addr`], reported by `INET_REQ_PEER` **after** its connection guard
    /// (`inet_drv.c:11282`-`:11290`).
    peer_addr: Option<InetAddr>,
    /// The descriptor's statistics counters (the C's `inet_descriptor` block,
    /// `inet_drv.c:11560`-`:11623`; see [`super::stat::InetStat`]): advanced where a datagram is
    /// delivered to the port (input) or a command is issued (output), and rendered by
    /// `INET_REQ_GETSTAT`. Zeroed by construction, as `inet_init_desc` leaves it.
    stat: super::stat::InetStat,
    /// The C's `desc->prebound` flag (`inet_drv.c:5266`): set by the `INET_REQ_FDOPEN` adoption,
    /// which takes a **caller-owned** descriptor over. A prebound descriptor is never `close(2)`d
    /// — the C's `erl_inet_close` only drops the select registration for one and leaves
    /// `desc->s`/`desc->state` untouched (`:5102`-`:5117`) — so neither a close nor a `Drop` may
    /// release it (see [`UdpInetDriver::adopt_prebound`] and [`UdpInetDriver::close_socket`]).
    /// Every route that installs a driver-**owned** socket (`open`/[`UdpInetDriver::from_socket`])
    /// clears it.
    prebound: bool,
}
impl UdpInetDriver {
    /// A UDP driver with no socket yet: the state a freshly opened port starts in.
    ///
    /// The socket is created by the `open` verb (or adopted by
    /// [`UdpInetDriver::from_socket`]), mirroring the C where `udp_inet_start` builds only the
    /// descriptor and `INET_REQ_OPEN` creates the socket (`packet_inet_ctl`, `:14577`).
    pub fn new() -> UdpInetDriver {
        UdpInetDriver {
            socket: None,
            token: None,
            interest: Interest::READABLE,
            state: InetState::Closed,
            options: SocketOptions::default(),
            // A freshly created descriptor carries no cached family/type yet (`inet_init_desc`),
            // so `INET_REQ_GETTYPE` answers the C's `einval` until a socket exists.
            family: 0,
            stype: 0,
            name_addr: None,
            peer_addr: None,
            stat: super::stat::InetStat::default(),
            prebound: false,
        }
    }
    /// Adopts an existing datagram socket, taking ownership of `fd` (closed on
    /// `stop`/`close`/`Drop`).
    ///
    /// The socket's state is read from the kernel, exactly as the C's `INET_REQ_FDOPEN` does
    /// (`inet_ctl_fdopen`, `inet_drv.c:5250`-`:5265`): a socket that answers `getpeername(2)`
    /// has a default peer and starts in [`InetState::Connected`], anything else in
    /// [`InetState::Open`]. The adopted descriptor's cached family/type — what `INET_REQ_GETTYPE`
    /// reports — are recorded from the adoption, the way `inet_ctl_fdopen` stores
    /// `desc->sfamily`/`desc->stype` (`:5265`). `fd` is put into non-blocking mode by
    /// [`Driver::start`].
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when `getpeername(2)` fails for a reason other than "not
    /// connected" (i.e. when `fd` is not a usable socket), or when the kernel cannot report its
    /// `SO_TYPE`.
    pub fn from_socket(fd: RawFd) -> Result<UdpInetDriver> {
        let state = if has_peer(fd)? {
            InetState::Connected
        } else {
            InetState::Open
        };
        Ok(UdpInetDriver {
            socket: Some(fd),
            token: None,
            interest: Interest::READABLE,
            state,
            options: SocketOptions::default(),
            family: u16::from(INET_AF_INET),
            stype: socket_type(fd)?,
            name_addr: None,
            peer_addr: None,
            stat: super::stat::InetStat::default(),
            prebound: false,
        })
    }
    /// The socket this port owns, if one has been opened.
    pub fn socket(&self) -> Option<RawFd> {
        self.socket
    }

    /// The lifecycle state.
    pub fn state(&self) -> InetState {
        self.state
    }

    /// Whether a socket has been opened (`INET_STATE_OPEN` in the C's sense: anything but
    /// [`InetState::Closed`]).
    fn has_socket(&self) -> bool {
        self.socket.is_some()
    }

    /// Makes `fd` this driver's socket and registers it [`Interest::READABLE`].
    ///
    /// A failed registration closes `fd` and leaves the driver socket-less, so nothing leaks.
    fn adopt(&mut self, ctx: &mut DriverContext<'_>, fd: RawFd) -> Result<()> {
        set_nonblocking(fd)?;
        match ctx.register(fd, Interest::READABLE) {
            Ok(token) => {
                self.socket = Some(fd);
                self.token = Some(token);
                self.interest = Interest::READABLE;
                // An `open` descriptor is the driver's own (closed on teardown), so this port is
                // no longer holding a caller's `INET_REQ_FDOPEN` descriptor.
                self.prebound = false;
                Ok(())
            }
            Err(err) => {
                close_fd(fd);
                Err(err)
            }
        }
    }

    /// Adopts a **caller-owned** descriptor: the C's `inet_ctl_fdopen`
    /// (`inet_drv.c:5223`-`:5282`), the body behind `packet_inet_ctl`'s `INET_REQ_FDOPEN` arm.
    ///
    /// This is [`UdpInetDriver::adopt`]'s prebound twin, and the difference is **ownership**: the
    /// descriptor belongs to whoever handed it to `INET_REQ_FDOPEN`, so it is never closed here —
    /// not on a failed `set_nonblocking(2)`, not on a failed poller registration, not by
    /// [`UdpInetDriver::close_socket`] and not by `Drop`. `prebound` records that, exactly as the
    /// C's `desc->prebound = 1` does (`:5266`), which its close path honours by only dropping the
    /// select registration (`erl_inet_close`, `:5102`-`:5117`).
    ///
    /// `stype` is the `INET_TYPE_*` tag the request's operand carried — the datagram driver
    /// admits both the stream and the datagram tag (`packet_inet_ctl`, `:14671`), and the caller
    /// has already run the length/family/type ladder plus the `sock_name(2)` pre-check
    /// ([`super::wire::fdopen_precheck`]). The lifecycle state is the C's: [`InetState::Open`],
    /// promoted to [`InetState::Connected`] only when a `SOCK_STREAM` adoption has a peer — the
    /// C's `sock_peer` probe (`:5252`-`:5260`), which the C also runs solely for a stream, so an
    /// adopted datagram socket is always reported as `INET_STATE_OPEN`.
    ///
    /// # Errors
    ///
    /// `BeamError::Io` when `fd` cannot be put into non-blocking mode, when the `getpeername(2)`
    /// probe fails for a reason other than "not connected", or when the poller rejects it. The
    /// descriptor is **not** closed on any of those paths, and the driver is left untouched when
    /// one of them fires.
    fn adopt_prebound(
        &mut self,
        ctx: &mut DriverContext<'_>,
        fd: RawFd,
        stype: u16,
    ) -> Result<()> {
        set_nonblocking(fd)?;
        // The C's `sock_peer` probe for a stream (`:5252`-`:5260`), taken **before** any field is
        // touched so a failing probe leaves the driver exactly as it was.
        let connected = stype == u16::from(INET_TYPE_STREAM) && has_peer(fd)?;
        let token = match ctx.register(fd, Interest::READABLE) {
            Ok(token) => token,
            // Deliberately **no** `close_fd`: the descriptor is the caller's (see the docs above),
            // which is the one behavioural difference from [`UdpInetDriver::adopt`].
            Err(err) => return Err(err),
        };
        self.socket = Some(fd);
        self.token = Some(token);
        self.interest = Interest::READABLE;
        self.state = if connected {
            InetState::Connected
        } else {
            InetState::Open
        };
        // `desc->prebound = 1; desc->stype = type; desc->sfamily = domain` (`:5266`-`:5269`).
        self.prebound = true;
        self.family = u16::from(INET_AF_INET);
        self.stype = stype;
        Ok(())
    }
    /// Changes the socket's registered interest mask (registering it the first time).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when there is no socket, or whatever the poller returns.
    fn set_interest(&mut self, ctx: &mut DriverContext<'_>, interest: Interest) -> Result<()> {
        let fd = self.socket.ok_or_else(error::badarg)?;
        match self.token {
            Some(token) => ctx.reregister(fd, token, interest)?,
            None => self.token = Some(ctx.register(fd, interest)?),
        }
        self.interest = interest;
        Ok(())
    }

    /// Removes the socket's poller registration, if it has one.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when the poller rejects the de-registration.
    fn deregister(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        match (self.token.take(), self.socket) {
            (Some(token), Some(fd)) => ctx.deregister(fd, token),
            _ => Ok(()),
        }
    }

    /// Closes the owned descriptor without touching the poll set, and forgets it.
    ///
    /// Used after [`UdpInetDriver::deregister`] and by `Drop` (which has no poller to
    /// de-register with). Taking the field first makes it idempotent.
    ///
    /// A **prebound** descriptor is not this driver's to close: the C's `erl_inet_close`
    /// (`inet_drv.c:5102`-`:5117`) only drops the select registration for one, leaves `desc->s`
    /// and `desc->state` exactly as they were and issues no `close(2)` at all. The same rule holds
    /// here — nothing is closed and no field is reset, so the descriptor number stays valid in the
    /// OS (owned by whoever handed it to `INET_REQ_FDOPEN`) and a post-close
    /// `GETSTATUS`/`GETFD`/`NAME` still answers what it answered before, which is what the C
    /// does. `Drop` runs this too, so a prebound descriptor is never closed by a dropped driver
    /// either.
    fn close_socket(&mut self) {
        if self.prebound {
            return;
        }
        if let Some(fd) = self.socket.take() {
            close_fd(fd);
        }
        self.state = InetState::Closed;
    }
    /// De-registers **and** closes the current socket, returning the de-registration outcome.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when the poller rejects the de-registration; the descriptor is
    /// closed either way.
    fn release_socket(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        let result = self.deregister(ctx);
        self.close_socket();
        result
    }

    /// Drains the port's output queue to the socket as datagrams, returning the number of bytes
    /// written.
    ///
    /// This is the port's *output* callback (the C's `packet_inet_command`), reached only when
    /// something queued bytes and scheduled an `OUTPUT` task. A datagram send is atomic, so a
    /// "short write" cannot occur — but the queue may still hold more than one command, and each
    /// iteration sends the contiguous remainder as one datagram.
    ///
    /// Only a **connected** socket can be written from a queue: a datagram needs a destination,
    /// and the unconnected case carries it in the `send` verb's argument (see
    /// [`UdpInetDriver::verb_send`], which calls `sendto(2)` directly and never queues). So a
    /// queued command on a socket-less or unconnected port is a **refusal**, not a silent success:
    /// the C's `packet_inet_command` guards the send with `!IS_OPEN` and fails the port
    /// (`inet_drv.c:15051`), and its queue-draining form expects the `[address][data]` grammar —
    /// a bare port command carries no address at all, so nothing in the queue can name a peer.
    /// **The queued bytes are never consumed** on that path: the caller learns the command was
    /// refused, and the queue keeps exactly what it was given.
    ///
    /// # Errors
    ///
    /// [`beam_common::BeamError::BadArg`] when the port has no socket, or has one that is not
    /// connected (`inet_drv.c:15051`) — the queued bytes stay queued, never silently dropped.
    /// `BeamError::Io` when a send fails irrecoverably.
    fn write_pending(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        if ctx.output().is_empty() {
            return Ok(0);
        }
        // No socket, or a socket with no default peer: the queue holds a bare command with no
        // destination, which the C refuses (`:15051`). Nothing is consumed.
        let Some(fd) = self.socket.filter(|_| self.state.is_connected()) else {
            return Err(error::badarg());
        };
        let mut written = 0usize;
        loop {
            let front = ctx.output().front();
            let len = front.len();
            if len == 0 {
                break;
            }
            match send_connected(fd, front) {
                Ok(n) => {
                    // A datagram send is all-or-nothing, so `n` is the whole `len`; consuming it
                    // keeps the queue's invariant (front is always the contiguous remainder).
                    ctx.output().consume(n);
                    written += n;
                }
                Err(err) => match err.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    // The kernel buffer is full: leave the remainder queued and arm write
                    // readiness so the port re-drives us.
                    Some(libc::EAGAIN) => {
                        if !self.interest.is_writable() {
                            self.set_interest(ctx, Interest::BOTH)?;
                        }
                        break;
                    }
                    Some(libc::ECONNREFUSED) | Some(libc::ECONNRESET) | Some(libc::EPIPE) => {
                        // The peer's port is gone (an `ICMP` port-unreachable surfaces as
                        // `ECONNREFUSED` on a connected datagram socket): the C's UDP send path
                        // reports the errno, so the queued remainder is dropped and the caller
                        // learns from the reply.
                        ctx.output().clear();
                        break;
                    }
                    _ => return Err(error::io(err)),
                },
            }
        }
        if ctx.output().is_empty() && self.interest.is_writable() {
            self.set_interest(ctx, Interest::READABLE)?;
        }
        Ok(written)
    }

    /// The `open` verb: creates the datagram socket and registers it for read readiness.
    ///
    /// Mirrors the C's `INET_REQ_OPEN` decode (`packet_inet_ctl`, `:14577`-`:14617`): a
    /// non-`AF_INET` family is `[0] ++ "eafnosupport"` (`:14593`), a non-datagram type
    /// `[0] ++ "einval"` (`:14604`), and a protocol that is neither default nor UDP
    /// `[0] ++ "eprotonosupport"` (`:14614`).
    ///
    /// # Errors
    ///
    /// [`beam_common::BeamError::BadArg`] for an argument that is not the three-byte
    /// `[AF_INET, SOCK_DGRAM, default|udp]` record; `BeamError::Io` when the socket cannot be
    /// created.
    fn verb_open(&mut self, ctx: &mut DriverContext<'_>, arg: &[u8]) -> Result<Vec<u8>> {
        if self.has_socket() {
            // The C's `inet_ctl_open` answers `[0]exbadseq` here (`:5130`); this floor refuses
            // to replace a live socket with the ordinary operational error instead — the same
            // choice `TcpInetDriver::verb_open` makes (see the module docs).
            return Ok(ctl_xerror("einval"));
        }
        let [family, kind, protocol] = arg else {
            return Err(error::badarg());
        };
        // AF_INET only; IPv6, SCTP and Unix-domain sockets are out of scope.
        if *family != INET_AF_INET {
            return Ok(ctl_xerror("eafnosupport"));
        }
        if *kind != INET_TYPE_DGRAM {
            return Ok(ctl_xerror("einval"));
        }
        if *protocol != INET_PROTO_DEFAULT && *protocol != INET_PROTO_UDP {
            return Ok(ctl_xerror("eprotonosupport"));
        }
        let fd = dgram_socket_fd()?;
        self.adopt(ctx, fd)?;
        // The C caches the descriptor's family and type here (`inet_ctl_open`, `:5214`-`:5216`),
        // which is what `INET_REQ_GETTYPE` reports for the rest of the port's life — a `close`
        // never clears them.
        self.family = u16::from(INET_AF_INET);
        self.stype = u16::from(INET_TYPE_DGRAM);
        self.state = InetState::Open;
        Ok(ctl_ok())
    }

    /// The `bind` verb: binds the socket and reports the port it ended up on.
    ///
    /// Mirrors `INET_REQ_BIND` (`:11409`): the C requires `INET_STATE_OPEN`, so a port with no
    /// socket — and a socket that has been connected — answers `[0] ++ "exbadport"`. A second
    /// `bind` reaches the kernel, which refuses it with `EINVAL` (`[0] ++ "einval"`), exactly as
    /// the oracle reports.
    ///
    /// # Errors
    ///
    /// `BeamError::BadArg` for a malformed packed address; `BeamError::Io` when the follow-up
    /// `getsockname(2)` fails.
    fn verb_bind(&mut self, arg: &[u8]) -> Result<Vec<u8>> {
        if self.state != InetState::Open {
            return Ok(ctl_xerror("exbadport"));
        }
        let address = parse_address(arg)?;
        self.bind_address(address)
    }

    /// The body of `INET_REQ_BIND` once the address has been decoded: the C's `sock_bind` and the
    /// port re-read (`inet_drv.c:11426`-`:11447`).
    ///
    /// The numeric route decodes the argument itself (so its `inet_set_faddress` refusals can be
    /// rendered instead of raised) and lands here; the named route reaches it through
    /// [`UdpInetDriver::verb_bind`].
    ///
    /// # Errors
    ///
    /// `BeamError::Io` when the follow-up `getsockname(2)` fails.
    fn bind_address(&mut self, address: InetAddr) -> Result<Vec<u8>> {
        let fd = self.socket.ok_or_else(error::badarg)?;
        if let Err(err) = bind_socket(fd, &address) {
            return Ok(ctl_error(err));
        }
        // A zero port asks the kernel for one, exactly as the C re-reads the address then
        // (`inet_drv.c:11445`); report the port actually bound either way.
        let port = if address.port == 0 {
            local_port(fd)?
        } else {
            address.port
        };
        let mut reply = ctl_ok();
        reply.extend_from_slice(&port.to_be_bytes());
        Ok(reply)
    }

    /// The `connect` verb: records a default peer for the socket.
    ///
    /// A UDP `connect(2)` is **synchronous** — it installs a filter and a default destination and
    /// returns immediately — so a `0` return means the socket is connected (`InetState::
    /// Connected`); `EISCONN` is the racy spelling of "already connected" and is treated the
    /// same way. An empty argument is the C's **dissolve** form (`:14802`: a connect to the null
    /// address, which drops the association), leaving the socket merely open.
    ///
    /// The C's argument is `[Timeout(4), packed-address]` (the timeout is ignored for UDP, which
    /// never blocks); this floor accepts either that shape or a bare packed address, deciding by
    /// whether the argument already starts with a family tag.
    ///
    /// # Errors
    ///
    /// `BeamError::BadArg` for an argument that carries no parsable packed address.
    fn verb_connect(&mut self, arg: &[u8]) -> Result<Vec<u8>> {
        if !self.has_socket() {
            return Ok(ctl_xerror("exbadport"));
        }
        if arg.is_empty() {
            // The dissolve form: drop the default peer, back to a merely open socket.
            self.state = InetState::Open;
            return Ok(ctl_ok());
        }
        let address = self.connect_address(arg)?;
        self.connect_to(address)
    }

    /// The C's UDP `connect(2)` body (`inet_drv.c:14824`-`:14850`) once the default peer has been
    /// decoded: install it, or report the kernel's `EISCONN` spelling of "already connected".
    ///
    /// # Errors
    ///
    /// `BeamError::BadArg` when the socket vanished between the guard and here (unreachable from
    /// the verbs, which all check first).
    fn connect_to(&mut self, address: InetAddr) -> Result<Vec<u8>> {
        let fd = self.socket.ok_or_else(error::badarg)?;
        match connect_socket(fd, &address) {
            Ok(()) => {
                self.state = InetState::Connected;
                Ok(ctl_ok())
            }
            Err(err) => match err.raw_os_error() {
                Some(libc::EISCONN) => {
                    self.state = InetState::Connected;
                    Ok(ctl_ok())
                }
                _ => Ok(ctl_error(err)),
            },
        }
    }

    /// The packed address of a `connect` argument: a bare packed address, or the C's
    /// `[Timeout(4)]`-prefixed form (`inet_drv.c:14816`-`:14830`, which skips four bytes and
    /// requires at least six left).
    ///
    /// # Errors
    ///
    /// `BeamError::BadArg` when neither shape yields a parsable packed address.
    fn connect_address(&self, arg: &[u8]) -> Result<super::wire::InetAddr> {
        let leads_with_tag = arg.len() >= 3 && split_address(arg).is_ok();
        if leads_with_tag {
            let (address, _) = split_address(arg)?;
            return Ok(address);
        }
        match arg.get(4..) {
            Some(rest) => parse_address(rest),
            None => Err(error::badarg()),
        }
    }

    /// The `send` verb: sends one datagram.
    ///
    /// Mirroring `packet_inet_command` (`:15037`-`:15230`), the argument carries the C's
    /// `[address][anc_len:be32][anc_data][data]` grammar. The address is always parsed and
    /// consumed; whether it is **used** depends on the socket: a **connected** socket sends with
    /// `send(2)` and ignores it (the C's `INET_F_ACTIVE` branch, `:15164`-`:15175`), an
    /// unconnected one with `sendto(2)`.
    ///
    /// # The two accepted wire forms
    ///
    /// The C strips the `prim_inet` **caller-reference prefix** before this grammar, but only
    /// when `driver_caller(port)` is an internal pid (`init_caller`/`init_caller_iov`,
    /// `inet_drv.c:1820`-`:1890`). This crate's [`Driver`]/[`DriverContext`] seam carries no
    /// caller, so the form is recognised from the payload (see [`split_caller_ref`]):
    ///
    /// 1. **caller-reference form, tried first** — `[tag_len:be16][tag_bytes][address]
    ///    [anc_len:be32][anc_data][data]`, where `tag_len` is non-zero, fits, and whose first tag
    ///    byte is the ETF `VERSION_MAGIC` `131` (`prim_inet:send/3`'s
    ///    `[<<Size:16, MrefBin/binary>>, Data]` and `do_sendto/4`'s
    ///    `[<<Size:16, RefBin/binary>>, Address, AncLen, AncData, Data]`,
    ///    `erts/preloaded/src/prim_inet.erl:575`-`:620`, `:650`-`:700`). The C's own
    ///    `len < 4` / `len < anc_len` guards (`:15205`/`:15212`) answer `[0] ++ "einval"`; a
    ///    **non-zero** `anc_len` is the typed `unsupported` raise (this floor has no `sendmsg`
    ///    cmsg path — the C's `:15220`-`:15230` arm).
    /// 2. **plain form** — `[address][data]`, the grammar every direct `port_control` caller and
    ///    every `port_command`-shaped probe uses. See [`split_address`] for why the ancillary
    ///    length word is not part of it.
    ///
    /// **The one residual ambiguity.** A *plain* command whose first two bytes happen to satisfy
    /// rule 1 (a 16-bit length that fits, followed by `0x83`) is read as a caller-reference form.
    /// For a plain UDP command that means byte 0 would have to be `0x00` (n = 1..255) and byte 2
    /// = `0x83`, i.e. an address family tag of `0x00` — never a valid address tag (1/3/4), so no
    /// *valid* plain command is misparsed.
    ///
    /// **Unsendable from this crate:** the C echoes the caller reference back as the fourth
    /// element of the later `{inet_reply, S, Reply, Ref}` message (`inet_reply_finish` /
    /// `inet_reply_caller_ref`, `:2610`-`:2660`). The frozen [`Driver`]/[`DriverContext`] seam
    /// has no driver→owner message channel — and this floor's `send` is a **synchronous** reply
    /// (see the module docs on the async-op queue) — so that message is not sent. The reply here
    /// is the C's `ctl_reply` bytes, which is what a `port_control` caller reads; the
    /// `{inet_reply, …}` shape stays a runtime-side gap. The CLI `gen_udp:send` path is not
    /// touched by this verb at all: a queued `port_command` reaches
    /// [`UdpInetDriver::write_pending`] instead, which still requires a connected socket.
    ///
    /// # Errors
    ///
    /// `BeamError::BadArg` for an argument with no parsable leading address, and the typed
    /// `unsupported` raise for a caller-reference form carrying ancillary data.
    fn verb_send(&mut self, arg: &[u8]) -> Result<Vec<u8>> {
        let Some(fd) = self.socket else {
            // The C's `!IS_OPEN` guard answers `inet_reply_error(desc, EINVAL)` (`:15051`).
            return Ok(ctl_xerror("einval"));
        };
        // Rule 1 first (the `prim_inet` caller-reference form), then the plain grammar; both
        // arms yield the packed address the C's `inet_set_faddress` would have produced plus the
        // payload that follows the ancillary-length word.
        let (address, payload) = match split_caller_ref(arg) {
            Some(command) => match split_datagram(command)? {
                DatagramCommand::Send(address, payload) => (address, payload),
                // The C's `return_einval` (`:15205`/`:15212`).
                DatagramCommand::Einval => return Ok(ctl_xerror("einval")),
            },
            None => split_address(arg)?,
        };
        // `inet_output_count(desc, len)` at `inet_drv.c:15163` (`packet_inet_command`, the
        // empty-ancillary-data arm; `:15220` is the ancillary one): only the **payload** is
        // counted — never the caller-reference prefix, the packed address or the ancillary
        // length word — and it is counted at issue time, before the `send(2)`/`sendto(2)`
        // attempt, so a failed send still counted it.
        self.stat.output(payload.len());
        let outcome = if self.state.is_connected() {
            send_connected(fd, payload)
        } else {
            send_to(fd, payload, &address)
        };
        match outcome {
            Ok(_) => Ok(ctl_ok()),
            Err(err) => Ok(ctl_error(err)),
        }
    }

    /// The `recv` verb: reads one datagram and reports its source (see the module docs for the
    /// reception model).
    ///
    /// The reply length carries the outcome: `[1] ++ src_address` (eight bytes) delivered a
    /// datagram, the bare `[1]` (one byte) found none pending, and `[0] ++ errno_name` is a
    /// syscall failure.
    ///
    /// # Errors
    ///
    /// `BeamError::BadArg` for a non-empty argument (the C's `PACKET_REQ_RECV` carries a timeout
    /// and a length this floor does not honor, so accepting them would silently ignore them —
    /// the same rule `TcpInetDriver::verb_recv` follows).
    fn verb_recv(&mut self, ctx: &mut DriverContext<'_>, arg: &[u8]) -> Result<Vec<u8>> {
        let Some(fd) = self.socket else {
            // The C's `!IS_OPEN` guard answers `inet_reply_error(desc, EINVAL)` (`:14980`).
            return Ok(ctl_xerror("einval"));
        };
        if !arg.is_empty() {
            return Err(error::badarg());
        }
        let mut buf = [0u8; DATAGRAM_CHUNK];
        loop {
            match recv_from(fd, &mut buf) {
                Ok((len, source)) => {
                    // `inet_input_count(desc, n)` at `inet_drv.c:15513`/`:15526` (`udp_deliver`):
                    // the received **datagram payload** length.
                    self.stat.input(len);
                    ctx.input().push(&buf[..len]);
                    let mut reply = ctl_ok();
                    reply.extend_from_slice(&pack_address(source.port, source.addr));
                    return Ok(reply);
                }
                Err(err) => match err.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    // Nothing pending: deliver nothing, exactly as `TcpInetDriver::verb_recv`
                    // answers a drained socket.
                    Some(libc::EAGAIN) => return Ok(ctl_ok()),
                    _ => return Ok(ctl_error(err)),
                },
            }
        }
    }

    /// The `close` verb: de-registers and closes the socket, leaving the port open but idle.
    ///
    /// The C's `INET_REQ_CLOSE` (`:14701`) succeeds on a socket-less port too, which
    /// [`UdpInetDriver::release_socket`] naturally does (there is nothing to release).
    ///
    /// # Errors
    ///
    /// `BeamError::Io` when the poller rejects the de-registration.
    fn verb_close(&mut self, ctx: &mut DriverContext<'_>) -> Result<Vec<u8>> {
        self.release_socket(ctx)?;
        Ok(ctl_ok())
    }

    /// The `setopts` verb: applies the C's `INET_REQ_SETOPTS` option records.
    ///
    /// The shared [`super::opt`] store is driven with its stream gate **off** (this driver's
    /// socket is always a datagram socket), so the UDP-relevant syscall codes
    /// (`reuseaddr`/`keepalive`/`broadcast`/`sndbuf`/`recbuf`/`ttl`) and the datagram
    /// driver-state codes (`{mode, …}`/`{deliver, …}`/`{read_packets, …}`) are served and the
    /// rest answer the C's `[0] ++ "einval"`.
    ///
    /// # Errors
    ///
    /// `BeamError::BadArg` for a truncated `{active, N}` record (never for a served option — a
    /// rejected `setsockopt(2)` is not propagated, mirroring the C's `propagate = 0`).
    fn verb_setopts(&mut self, arg: &[u8]) -> Result<Vec<u8>> {
        set_options(&mut self.options, false, self.socket, arg)
    }

    /// The `getopts` verb: reads the C's `INET_REQ_GETOPTS` options back.
    ///
    /// Driven with the same stream gate off as [`UdpInetDriver::verb_setopts`]; a code the
    /// datagram store does not serve is the C's whole-reply `[0] ++ "einval"`.
    ///
    /// # Errors
    ///
    /// `BeamError::Io` when a served option's `getsockopt(2)` fails on a live socket.
    fn verb_getopts(&mut self, arg: &[u8]) -> Result<Vec<u8>> {
        get_options(&self.options, false, self.socket, arg)
    }

    /// The `INET_REQ_NAME` reply (`inet_drv.c:11329`).
    ///
    /// The C answers the descriptor's cached `name_ptr` **first** (`:11351`-`:11360`) — the fake
    /// local address an `INET_REQ_SETNAME` installed — and reaches `getsockname(2)` only when it is
    /// NULL. Measured against the oracle: `SETNAME <<1,4,210,1,2,3,4>>` then `NAME` ⇒
    /// `[1,1,4,210,1,2,3,4]`, on a bare port as well as on an opened one, and the plain
    /// `getsockname(2)` reply (or its errno) comes back once an empty `SETNAME` clears it.
    ///
    /// # Errors
    ///
    /// Whatever [`super::wire::name_reply`] returns for the `getsockname(2)` path.
    fn name_reply(&self) -> Result<Vec<u8>> {
        match self.name_addr {
            Some(address) => Ok(packed_ok(address)),
            None => super::wire::name_reply(self.socket),
        }
    }

    /// The `INET_REQ_PEER` reply (`inet_drv.c:11272`).
    ///
    /// The C's order is the connection guard (`INET_F_ACTIVE`, `:11282`) **then** the `peer_ptr`
    /// short-circuit (`:11286`), so a fake peer installed by `INET_REQ_SETPEER` is reported only on
    /// a connected socket. Measured: `SETPEER` + `PEER` on a bare port ⇒ `[0]enotconn`; after a
    /// `connect` ⇒ the fake address, while `INET_REQ_GETPADDRS` keeps reporting the real one (it
    /// calls `sock_peer` directly, `:11267`).
    ///
    /// # Errors
    ///
    /// Whatever [`super::wire::peer_reply`] returns for the `getpeername(2)` path.
    fn peer_reply(&self) -> Result<Vec<u8>> {
        if !self.state.is_connected() {
            return Ok(ctl_xerror("enotconn"));
        }
        match self.peer_addr {
            Some(address) => Ok(packed_ok(address)),
            None => super::wire::peer_reply(self.socket, true),
        }
    }

    /// Routes one decoded numeric control opcode to the verb or shared reply that serves it.
    ///
    /// This is the datagram half of the bridge documented in [`super::opcode`]: `packet_inet_ctl`
    /// (`inet_drv.c:14570`) owns `{OPEN, FDOPEN, CLOSE, CONNECT, LISTEN (SCTP only),
    /// SCTP_REQ_BINDX, SCTP_REQ_PEELOFF, PACKET_REQ_RECV}` and delegates the rest to `inet_ctl`
    /// (`:10988`). A number **neither** switch has a case for — the TCP-only stream requests, or a
    /// wrapped command — reaches `inet_ctl`'s `default:` arm and answers `ctl_xerror(EXBADPORT)`
    /// ⇒ `[0]exbadport` (measured: `TCP_REQ_RECV` 42 and `ACCEPT` 26 on a UDP port).
    ///
    /// Every arm reproduces the C's **own guard order**, which is what makes the refusals
    /// byte-exact: a length check before a state check for `BIND`, the reverse for `CONNECT` and
    /// `PACKET_REQ_RECV`. A `ctl_error(EINVAL)`-class refusal is an `Ok([0]… "einval")` reply, never
    /// a raise; only an opcode the C does serve but this floor cannot is a typed
    /// [`beam_common::BeamError`].
    ///
    /// # Errors
    ///
    /// A typed "not served" reason for an opcode the C's switches do serve but this floor does not
    /// implement, plus the verbs' own argument faults (`BeamError::BadArg`).
    fn control_opcode(
        &mut self,
        ctx: &mut DriverContext<'_>,
        op: Opcode,
        arg: &[u8],
    ) -> Result<Vec<u8>> {
        match op {
            // The verbs whose operand is already the C's request payload.
            //
            // `packet_inet_ctl`'s `INET_REQ_OPEN` rejects a payload that is not the three-byte
            // `[AF, TYPE, PROTO]` record with `ctl_error(EINVAL)` *before* decoding it (`:14580`).
            Opcode::Open => {
                if arg.len() != 3 {
                    return Ok(ctl_xerror("einval"));
                }
                self.verb_open(ctx, arg)
            }
            // `INET_REQ_BIND` (`inet_ctl`, `:11409`) checks `len < 2 ⇒ EINVAL` (`:11417`) **before**
            // the state guard (`state != INET_STATE_OPEN ⇒ EXBADPORT`, `:11419`) and only then
            // decodes the address with `inet_set_faddress` (`:11421`). So a bare port answers
            // `einval` for an empty operand and `exbadport` for a well-formed address (measured),
            // and the address decode's own refusals are the C's `ctl_xerror` names.
            Opcode::Bind => {
                if arg.len() < 2 {
                    return Ok(ctl_xerror("einval"));
                }
                if self.state != InetState::Open {
                    return Ok(ctl_xerror("exbadport"));
                }
                match faddress(self.family, arg) {
                    Ok(address) => self.bind_address(address),
                    Err(name) => Ok(ctl_xerror(name)),
                }
            }
            Opcode::Setopts => self.verb_setopts(arg),
            Opcode::Getopts => self.verb_getopts(arg),
            Opcode::Close => self.verb_close(ctx),
            // `packet_inet_ctl`'s `INET_REQ_CONNECT` (`:14712`) guards in this order: `!IS_OPEN ⇒
            // EXBADPORT` (`:14727`), the **empty** payload = "dissolve the default peer" (`:14803`),
            // then `len < 6 ⇒ EINVAL` (`:14809`). The payload is `Timeout(4), Address(N)`, so the
            // address is decoded from `arg[4..]` with the C's `inet_set_faddress` classification
            // (`:14814`) instead of being guessed at by [`UdpInetDriver::connect_address`].
            Opcode::Connect => {
                if !self.has_socket() {
                    return Ok(ctl_xerror("exbadport"));
                }
                if arg.is_empty() {
                    return self.verb_connect(arg);
                }
                if arg.len() < 6 {
                    return Ok(ctl_xerror("einval"));
                }
                match faddress(self.family, &arg[4..]) {
                    Ok(address) => self.connect_to(address),
                    Err(name) => Ok(ctl_xerror(name)),
                }
            }
            // `PACKET_REQ_RECV` is `packet_inet_ctl`'s own receive: it requires a socket
            // (`!IS_OPEN ⇒ `[0]exbadport`, `:14981`) and an eight-byte `Timeout(4), Length(4)`
            // payload (`:14988`).
            //
            // **Documented divergence:** the C enqueues an async operation and answers the
            // three-byte op id `[1] ++ async_id_be16` (measured `[1,0,0]`), the datagram arriving
            // later as an `{inet_async, …}` message. This floor has no driver→owner message path,
            // so `recv` is a synchronous read-now: it answers `[1] ++ source_address` with a
            // datagram (or the bare `[1]`) and never an op id. The operand is validated and then
            // ignored.
            Opcode::PacketRecv => {
                if self.socket.is_none() {
                    return Ok(ctl_xerror("exbadport"));
                }
                if arg.len() != 8 {
                    return Ok(ctl_xerror("einval"));
                }
                self.verb_recv(ctx, &[])
            }
            // The shared-`inet_ctl` replies both drivers build identically.
            Opcode::Peer => self.peer_reply(),
            Opcode::Name => self.name_reply(),
            // `INET_REQ_GETTYPE` (`:11165`-`:11203`) has **no `IS_OPEN` guard**: it switches on the
            // descriptor's cached `sfamily`/`stype`, which a close never clears. Hence a bare port
            // answers `[0]einval` (nothing cached) and a closed one still reports what it was opened
            // with — measured `[1,0,0,0,1,0,0,0,2]` after a `udp_inet` close.
            Opcode::Gettype => Ok(super::wire::gettype_reply(self.family, self.stype)),
            Opcode::Getfd => Ok(super::wire::getfd_reply(self.socket)),
            Opcode::Getstatus => Ok(super::wire::getstatus_reply(self.state.state_code())),
            Opcode::Getstat => super::wire::getstat_reply(arg, &self.stat, ctx.output().len()),
            // `INET_REQ_GETHOSTNAME` (`:11219`): `len != 0 ⇒ EINVAL`, else the local
            // `gethostname(2)` payload. `sock_hostname` is a plain syscall — no resolver is
            // involved — so the whole arm is exact (measured `[1] ++ "billmini"`).
            Opcode::Gethostname => {
                if !arg.is_empty() {
                    return Ok(ctl_xerror("einval"));
                }
                let mut reply = ctl_ok();
                reply.extend_from_slice(&hostname()?);
                Ok(reply)
            }
            // `packet_inet_ctl`'s `INET_REQ_FDOPEN` (`:14639`) guards in this order:
            // `len != 6 && len != 10 ⇒ EINVAL` (`:14647`), then the family tag
            // (`⇒ eafnosupport`, `:14659`), then the type (`⇒ EINVAL`, `:14671` — both the stream
            // and the datagram tag are admitted), and only then the pre-adoption check
            // `inet_ctl_fdopen` makes on a `bound` descriptor ([`super::wire::fdopen_precheck`]):
            // a failing `sock_name(2)` is `enotsock`/`ebadf`, a family mismatch `einval`. Once that
            // ladder passes the descriptor is adopted **prebound**
            // ([`UdpInetDriver::adopt_prebound`], the C's `inet_ctl_fdopen`, `:5223`-`:5282`): the
            // port uses it for its whole life but never closes it, because it belongs to the
            // caller (`desc->prebound = 1`, `:5266`). A port that already holds a socket is
            // refused instead.
            Opcode::Fdopen => {
                if arg.len() != 6 && arg.len() != 10 {
                    return Ok(ctl_xerror("einval"));
                }
                if arg[0] != INET_AF_INET {
                    return Ok(ctl_xerror("eafnosupport"));
                }
                if arg[1] != INET_TYPE_STREAM && arg[1] != INET_TYPE_DGRAM {
                    return Ok(ctl_xerror("einval"));
                }
                match super::wire::fdopen_precheck(arg) {
                    Ok(()) => {
                        let [a, b, c, d] = [arg[2], arg[3], arg[4], arg[5]];
                        let fd = RawFd::from_be_bytes([a, b, c, d]);
                        // A negative word cannot name a descriptor; the C would cast it to an
                        // unsigned `SOCKET` and fail later, so refuse it as the `EBADF` it is
                        // instead of handing it to the poller.
                        if fd < 0 {
                            return Ok(ctl_xerror("ebadf"));
                        }
                        if self.socket.is_some() {
                            // The C overwrites `desc->s` here and leaks the previous socket (no
                            // `desc_close` first, `:5267`). Refusing loudly is the honest choice:
                            // the existing socket is left **untouched** and keeps serving the port.
                            return Ok(ctl_xerror("einval"));
                        }
                        // The operand's own type tag is what the C stores as `desc->stype`
                        // (`:5268`), so it is passed through rather than assumed to be a datagram.
                        self.adopt_prebound(ctx, fd, u16::from(arg[1]))?;
                        Ok(ctl_ok())
                    }
                    Err(reply) => Ok(reply),
                }
            }
            // The name-service lookups (`:11493`, `:11525`): the shared `inet_ctl` arms, built by
            // the same helpers the stream driver uses — the system services database behind
            // `getservbyname(3)`/`getservbyport(3)`, with the C's exact operand arithmetic and its
            // `NULL` ⇒ `[0]einval`.
            Opcode::Getservbyname => Ok(super::wire::getservbyname_reply(arg)),
            Opcode::Getservbyport => Ok(super::wire::getservbyport_reply(arg)),
            // `INET_REQ_SETNAME` (`:11386`): an **empty** operand clears the fake address
            // (`desc->name_ptr = NULL`, `:11392`) and answers `[1]`; a one-byte one is the C's
            // `len < 2 ⇒ EINVAL` (`:11396`); anything else is decoded by `inet_set_faddress`
            // (`:4795`) and stored, its `str_einval`/`str_eafnosupport` rendered through
            // `ctl_xerror` (`:11399`). `INET_REQ_NAME` then reports the stored address.
            Opcode::Setname => {
                if arg.is_empty() {
                    self.name_addr = None;
                    return Ok(ctl_ok());
                }
                if arg.len() < 2 {
                    return Ok(ctl_xerror("einval"));
                }
                match faddress(self.family, arg) {
                    Ok(address) => {
                        self.name_addr = Some(address);
                        Ok(ctl_ok())
                    }
                    Err(name) => Ok(ctl_xerror(name)),
                }
            }
            // `INET_REQ_SETPEER` (`:11301`): the same shape, one field over — the fake peer is
            // reported by `INET_REQ_PEER` (only while connected) and cleared by an empty operand.
            Opcode::Setpeer => {
                if arg.is_empty() {
                    self.peer_addr = None;
                    return Ok(ctl_ok());
                }
                if arg.len() < 2 {
                    return Ok(ctl_xerror("einval"));
                }
                match faddress(self.family, arg) {
                    Ok(address) => {
                        self.peer_addr = Some(address);
                        Ok(ctl_ok())
                    }
                    Err(name) => Ok(ctl_xerror(name)),
                }
            }
            // The interface family (`:11067`, `:11078`, `:11089`, `:11100`): the same four shared
            // `inet_ctl` arms the stream driver reaches — each opens with
            // `!IS_OPEN ⇒ ctl_xerror(EXBADPORT)` and then walks the host's interfaces
            // (`SIOCGIFCONF`, the `SIOCGIF*` ioctls, `getifaddrs(3)`), built once in
            // [`super::wire::interface_reply`].
            Opcode::Getiflist | Opcode::Ifget | Opcode::Ifset | Opcode::Getifaddrs => {
                Ok(super::wire::interface_reply(op, self.socket, arg))
            }
            // `INET_REQ_IGNOREFD` (`:11456`): `!IS_CONNECTED ⇒ ENOTCONN` (`:11462`), then
            // `stype != SOCK_STREAM ⇒ EINVAL` (`:11464`) — and then the C reads `*buf` with **no
            // length check at all** (`:11465`). A *connected* socket is reachable here through the
            // `connect` verb, but it is always a datagram socket, so this driver always lands on
            // the `einval` arm (measured `[0]einval` on a connected datagram port); the
            // ignore/unignore body is stream-only and therefore a typed gap.
            Opcode::Ignorefd => {
                if !self.state.is_connected() {
                    return Ok(ctl_xerror("enotconn"));
                }
                if self.stype != u16::from(INET_TYPE_STREAM) {
                    return Ok(ctl_xerror("einval"));
                }
                Err(op.not_served())
            }
            // `INET_REQ_GETLADDRS`/`GETPADDRS` (`:11324`, `:11236`): `len != 4 ⇒ EINVAL`, then
            // `!IS_OPEN ⇒ EXBADPORT`, and the **non-SCTP fallback** of each is byte-identical to
            // the `NAME`/`PEER` reply (`reply_inet_addrs(1, …)` = `[1] ++ address`, `:5013`), so
            // both are single-sourced through those helpers — with the one C asymmetry preserved:
            // the fallbacks call `sock_name`/`sock_peer` **directly**, so a `SETNAME`/`SETPEER`
            // fake address is *not* reported here (measured: after `SETNAME`, `GETLADDRS` still
            // answers the socket's own address).
            Opcode::Getladdrs => {
                if arg.len() != 4 {
                    return Ok(ctl_xerror("einval"));
                }
                if !self.has_socket() {
                    return Ok(ctl_xerror("exbadport"));
                }
                super::wire::name_reply(self.socket)
            }
            Opcode::Getpaddrs => {
                if arg.len() != 4 {
                    return Ok(ctl_xerror("einval"));
                }
                if !self.has_socket() {
                    return Ok(ctl_xerror("exbadport"));
                }
                super::wire::peer_reply(self.socket, self.state.is_connected())
            }
            // `INET_REQ_SUBSCRIBE` (`inet_subscribe`, `:10819`): the shared `inet_ctl` arm, built
            // by the same helper the stream driver uses — `[1] ++ (op ++ be32(outq byte count))*`,
            // with an operand byte other than `INET_SUBS_EMPTY_OUT_Q` the C's `return -1`
            // (`badarg`). The *subscription* (the later `{empty_out_q, Port}` message) is what
            // this floor cannot honour — no driver→owner channel exists — not the reply.
            Opcode::Subscribe => super::wire::subscribe_reply(ctx.output().len(), arg),
            // `INET_REQ_LISTEN` is a case of `packet_inet_ctl` only inside `#ifdef HAVE_SCTP`
            // (`:14846`). With SCTP compiled out the case does not exist and the `default:`
            // delegates to `inet_ctl`, whose own `default:` is `ctl_xerror(EXBADPORT)`
            // (`:11551`): the answer is `[0]exbadport` in **every** state and for **every**
            // operand (measured bare and opened, `len` 0 and 2).
            Opcode::Listen => Ok(ctl_xerror("exbadport")),
            // The TCP-only cases (a `tcp_inet_ctl` case, so neither switch owns them for a
            // datagram socket) plus any wrapped/unassigned number: the C's `[0]exbadport`.
            Opcode::TcpRecv
            | Opcode::TcpUnrecv
            | Opcode::TcpShutdown
            | Opcode::TcpSendfile
            | Opcode::Accept
            | Opcode::Unknown => Ok(ctl_xerror("exbadport")),
        }
    }
}

/// Renders a single stored [`InetAddr`] as the C's `reply_inet_addrs` reply
/// (`inet_drv.c:5013`): `ctl_reply(INET_REP_OK, tbuf, sz, …)` over one packed address —
/// `[1, family, port_hi, port_lo, a, b, c, d]`.
///
/// The `INET_REQ_NAME`/`INET_REQ_PEER` `name_ptr`/`peer_ptr` short-circuits (`:11351`-`:11360`,
/// `:11286`) build exactly this shape, which is why the fake-address path and the `getsockname(2)`
/// path ([`super::wire::name_reply`]) are byte-compatible with each other.
fn packed_ok(address: InetAddr) -> Vec<u8> {
    let mut reply = ctl_ok();
    reply.extend_from_slice(&pack_address(address.port, address.addr));
    reply
}

/// Classifies a packed address the way the C's `inet_set_faddress` (`inet_drv.c:4795`) does,
/// returning the parsed [`InetAddr`] or the `ctl_xerror` atom name the C would answer.
///
/// The C's tag switch (`:4810`-`:4843`) accepts `INET_AF_INET` (a 7-byte `[tag, port_hi, port_lo,
/// a, b, c, d]` record, via `inet_set_address`), and `INET_AF_ANY`/`INET_AF_LOOPBACK` (a 3-byte
/// `[tag, port_hi, port_lo]` record); every other tag — `INET_AF_INET6` included, this floor being
/// IPv4-only — falls to `default: return str_eafnosupport` (`:4840`). A record too short for its
/// own tag is `str_einval` (the `*len < 1` / `*len < 2` guards, `:4800`/`:4825`).
///
/// Measured against the oracle on a bare `udp_inet` port: `SETNAME <<1,4,210,1,2,3,4>>` ⇒ `[1]`,
/// `SETNAME <<3,4,210>>` (the ANY form) ⇒ `[1]` reported as `<<1,4,210,0,0,0,0>>`,
/// `SETNAME <<1>>` ⇒ `[0]einval`, `SETNAME <<9,1,2>>` ⇒ `[0]eafnosupport`.
fn faddress(family: u16, arg: &[u8]) -> std::result::Result<InetAddr, &'static str> {
    let tag = match arg.first() {
        Some(tag) => *tag,
        None => return Err("einval"),
    };
    match tag {
        t if t == INET_AF_INET => parse_address(arg).map_err(|_| "einval"),
        t if t == INET_AF_ANY || t == INET_AF_LOOPBACK => {
            if arg.len() < 3 {
                return Err("einval");
            }
            // The C's `switch (family)` on the descriptor's **cached** family (`:4826`): with
            // `AF_INET` the ANY/LOOPBACK tag picks `INADDR_ANY`/`INADDR_LOOPBACK`; with anything
            // else (a never-opened descriptor's `sfamily == 0`) the arm is `str_einval`.
            if family != u16::from(INET_AF_INET) {
                return Err("einval");
            }
            Ok(InetAddr {
                port: u16::from_be_bytes([arg[1], arg[2]]),
                addr: if tag == INET_AF_ANY {
                    [0, 0, 0, 0]
                } else {
                    [127, 0, 0, 1]
                },
            })
        }
        _ => Err("eafnosupport"),
    }
}

impl Default for UdpInetDriver {
    fn default() -> UdpInetDriver {
        UdpInetDriver::new()
    }
}

impl Driver for UdpInetDriver {
    fn name(&self) -> &str {
        UDP_DRIVER_NAME
    }

    fn start(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        // A socket-less port (the `"udp_inet"` factory) registers nothing here: its socket is
        // created by the `open` verb, exactly as the C's `udp_inet_start` leaves the socket to
        // `INET_REQ_OPEN`.
        if let Some(fd) = self.socket {
            set_nonblocking(fd)?;
            self.token = Some(ctx.register(fd, Interest::READABLE)?);
            self.interest = Interest::READABLE;
        }
        Ok(())
    }

    fn output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        self.write_pending(ctx)
    }

    fn ready_input(&mut self, _ctx: &mut DriverContext<'_>) -> Result<()> {
        // A deliberate no-op: a datagram's source address has no channel in this crate's
        // `Driver`/`PortNotification` seam (the port input queue is byte-only and
        // `PortNotification` keeps exactly its two variants for an outside crate to match
        // exhaustively), so reception is verb-driven — the runtime observes this readiness
        // event and issues the `recv` verb, which performs the `recvfrom(2)` and reports the
        // source in its own reply (see the module docs).
        Ok(())
    }

    fn ready_output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        self.write_pending(ctx)
    }

    fn control(
        &mut self,
        ctx: &mut DriverContext<'_>,
        command: &str,
        arg: &[u8],
    ) -> Result<Vec<u8>> {
        // A decimal command is the C's numeric `INET_REQ_*`/`PACKET_REQ_*` opcode (see
        // [`super::opcode`]) and takes the opcode path; a named verb falls through unchanged.
        if let Some(op) = decode_opcode(command) {
            return self.control_opcode(ctx, op, arg);
        }
        match command {
            "open" => self.verb_open(ctx, arg),
            "bind" => self.verb_bind(arg),
            "connect" => self.verb_connect(arg),
            "send" => self.verb_send(arg),
            "recv" => self.verb_recv(ctx, arg),
            "close" => self.verb_close(ctx),
            "setopts" => self.verb_setopts(arg),
            "getopts" => self.verb_getopts(arg),
            _ => Err(error::badarg()),
        }
    }

    fn stop(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        self.release_socket(ctx)
    }
}

impl Drop for UdpInetDriver {
    fn drop(&mut self) {
        // `stop` already released the socket on the normal teardown path; this covers a driver
        // dropped without ever being stopped (a `start` that failed inside
        // `PortRegistry::open`, for instance). A `Drop` cannot report anything, so the
        // descriptor is simply released — the same rule `TcpInetDriver`/`FdDriver` follow.
        self.close_socket();
    }
}

/// The [`crate::DriverFactory`] of the built-in `"udp_inet"` driver.
///
/// A plain `fn` (not a closure) because [`crate::DriverFactory`] is a function pointer: it
/// captures nothing and builds a fresh socket-less [`UdpInetDriver`] each call.
pub fn make_udp_inet() -> Result<Box<dyn Driver>> {
    Ok(Box::new(UdpInetDriver::new()))
}

#[cfg(test)]
mod udp_tests;
