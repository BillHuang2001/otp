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

//! [`TcpInetDriver`] — the built-in `tcp_inet` TCP socket driver.
//!
//! The Rust counterpart of the legacy C TCP socket driver (`tcp_inet`), the driver
//! `gen_tcp`/`inet_tcp` sit on: it owns **one** socket (a listening one or a connected one),
//! registers it with the poller and moves bytes between the socket and the port's two byte
//! queues.
//!
//! # Registered name and the control vocabulary
//!
//! The driver registers under `"tcp_inet"` — the name the oracle's `port_info(Port, name)`
//! reports for a TCP port, and the name the C passes to `driver_create_port` when it makes a
//! port for an accepted connection (`erts/emulator/drivers/common/inet_drv.c:1348`,
//! `:11834`).
//!
//! [`Driver::control`] serves **both** wire forms. The Erlang side sends an integer, which
//! arrives as its decimal string: a decimal command is decoded as the C's `unsigned int` control
//! opcode ([`self::opcode`] — the `0x03f1a300` magic subtracted with wrapping) and routed to the
//! verb that code number's `case` selects; any non-decimal command is a **named** verb, this
//! floor's own vocabulary for a direct caller. The *operand* is the C's packed request payload in
//! both forms and is decoded here. The verbs, and the C request each one mirrors, are:
//!
//! | verb | C request | argument | reply |
//! |---|---|---|---|
//! | `open` | `INET_REQ_OPEN` (`:11961`) | `[AF, TYPE, PROTO]` (3 bytes) | `[1]` |
//! | `bind` | `INET_REQ_BIND` (`:11409`) | packed address | `[1, port_hi, port_lo]` |
//! | `listen` | `INET_REQ_LISTEN` (`:12042`) | `[backlog_hi, backlog_lo]` | `[1]` |
//! | `accept` | `INET_REQ_ACCEPT` (`:12144`) | — | `[1, fd_be32]`, or `[1]` when nothing is pending † |
//! | `connect` | `INET_REQ_CONNECT` (`:12064`) | `Timeout(4) ++` packed address | `[1]`, or `[0] ++ errno` / `[0]etimedout` |
//! | `send` | `tcp_inet_command` (`:12629`) | the bytes to send | `[1]` |
//! | `recv` | `TCP_REQ_RECV` (`:12268`) | — | `[1]` † |
//! | `shutdown` | `TCP_REQ_SHUTDOWN` (`:12339`) | `[how]` | `[1]` |
//! | `close` | `INET_REQ_CLOSE` (`:12259`) | — | `[1]` |
//! | `setopts` | `INET_REQ_SETOPTS` (`:11111`) | option records `[opt, val_be32]*` | `[1]`, or `[0] ++ "einval"` |
//! | `getopts` | `INET_REQ_GETOPTS` (`:11046`) | option codes `[opt]*` | `[1] ++ [opt, val_be32]*`, or `[0] ++ "einval"` |
//!
//! † The rows marked with a dagger describe the **named** verbs, this floor's own read-now
//! vocabulary for a direct caller. The **numeric** C requests `INET_REQ_ACCEPT`(26) and
//! `TCP_REQ_RECV`(42) are asynchronous: they answer `[1] ++ <<Id:16/big>>` and complete later
//! through the completion channel — see *The asynchronous-operation queue* below.
//!
//! A reply is the C's `ctl_reply` shape: a one-byte result tag ([`INET_REP_OK`] or
//! [`INET_REP_ERROR`], `:869-871`) followed by a payload — `ctl_reply`/`ctl_reply_ok`/
//! `ctl_error`/`ctl_xerror` (`:2260`-`:2311`). An **operational** failure (a socket that is
//! not connected, a descriptor that was never opened) is that `[0]`-tagged reply carrying the
//! lowercase errno name the oracle uses (`erl_errno_id`, `:2249`), while a **malformed
//! argument** or an **unknown verb** is [`beam_common::BeamError::BadArg`] — the crate's
//! argument-fault convention. The C reports the former as `ctl_error(EINVAL)`; this floor
//! reports it as `BadArg`.
//!
//! # The packed address
//!
//! `bind`/`connect` decode the C's packed address (`inet_set_faddress`/`inet_set_address`,
//! `:4795`/`:4706`): `[INET_AF_INET, port_hi, port_lo, a, b, c, d]` (7 bytes) for an explicit
//! IPv4 address, or `[INET_AF_ANY | INET_AF_LOOPBACK, port_hi, port_lo]` (3 bytes) for
//! `0.0.0.0`/`127.0.0.1` (`INET_AF_*`, `:756-763`). DNS resolution and IPv6/UDP are out of
//! scope for this floor, so any other family tag is a `BadArg`.
//!
//! # The socket options
//!
//! `setopts`/`getopts` speak the C's option wire verbatim and are documented in full in
//! [`self::opt`]: `setopts` takes `[opt, val_be32]` five-byte records (plus the trailing 16-bit
//! `N` of an `{active, N}` record) and answers `[1]`; `getopts` takes one-byte option codes and
//! answers `[1] ++ [opt, val_be32]*` in request order. Two families are served.
//!
//! The **syscall-backed** options — `keepalive` (`SO_KEEPALIVE`), `sndbuf` (`SO_SNDBUF`),
//! `recbuf` (`SO_RCVBUF`) and `nodelay` (`TCP_NODELAY`) — are each a direct
//! `setsockopt(2)`/`getsockopt(2)`, so `recbuf`/`sndbuf` report **the kernel's own value**
//! (on Linux a doubled request: 4096 reads back as 8192, exactly as the oracle reports it).
//!
//! The **driver-state** options — `{active, …}` (`22`), `{packet, …}` (`23`),
//! `{mode, …}` (`24`), `{deliver, …}` (`25`), `{send_timeout, …}` (`30`),
//! `{packet_size, …}` (`32`), `{read_packets, N}` (`33`), `{send_timeout_close, …}` (`35`) and
//! `{line_delim, …}` (`40`) — are stored and reported with the oracle's defaults and byte-exact
//! read-backs. `{packet, …}`/`{packet_size, …}`/`{line_delim, …}` are now **acted on** on the
//! read path (see the framing bullet below); the remaining behaviours behind these options
//! (active-mode delivery, the asynchronous send-timeout reply) remain the handoffs named below.
//! `{line_delim, …}` is **set-only** — `getopts` of `40` is the C's `[0]einval`, which replaces
//! the whole reply exactly as the oracle does. `{mode, …}` is *stored and reported* only: the
//! term a consumer builds from it (`{Port, {data, Binary | List}}`) is the runtime's to render.
//!
//! Any other code is the C's `[0] ++ "einval"` — see [`self::opt`] for the exact served set.
//!
//! # What the real `gen_tcp` still needs
//!
//! The verbs above are the whole of what this floor implements. Everything below is **absent
//! by design** and answers a **clear error** (`[0]einval`, `BadArg`, or the C's own
//! state error) rather than a wrong result; each names what it would require:
//!
//! * **The `{active, false|true|once|N}` modes** (`INET_LOPT_ACTIVE`, `:22`) — the option is
//!   **stored and reported** (with the C's `{active, N}` counter accumulation) but this driver
//!   never delivers input on its own. Bytes land in the port input queue and the runtime
//!   decides when to fetch them (the port-task round's [`crate::PortNotification`] seam).
//!   Active mode is the C's `tcp_deliver`/`tcp_recv` loop driving the port's *output* side
//!   itself, plus a delivery mode (`{deliver, port|term}`, `INET_LOPT_DELIVER`, `:25`) and the
//!   `active_count` bookkeeping (`:6968`-`:7010`) — a driver-owned message path, deliberately
//!   outside this crate's seam.
//! * **The `{packet, N}`/`line`/`raw` header decoding** (`INET_LOPT_PACKET`, `:23`) — the C's
//!   `htype` machinery (`inet_set_opts`'s store, `tcp_remain`/`tcp_reply_data`,
//!   `:12967`/`:4279`, and the `PacketParseType` handlers in `packet_parser.c`). It **is**
//!   wired on the read path: with a framing `{packet, N}` the driver accumulates socket bytes
//!   in its own buffer ([`TcpInetDriver::pending_input`]) instead of the port input queue, and
//!   the `recv` verb takes **one** decoded frame's payload out of it — the header stripped and
//!   the payload delivered as one message ([`self::frame`], over the pure decoder in
//!   [`self::packet`]). `{packet_size, …}` (`32`) bounds a frame's body (`max_plen`) and
//!   `{line_delim, …}` (`40`) sets the line delimiter. The `1`/`2`/`4` codes frame **both**
//!   directions: the read path strips the length header and the `send` verb prepends it (the
//!   C's `tcp_sendv`/`tcp_send`, `:13745`-`:13769`, `:13861`-`:13877`), while every other code
//!   takes the C's `default:` arm and writes no header. Decoded: `raw` (`0`), `1`/`2`/`4`
//!   (`1`/`2`/`3`), `asn1` (`4`), `sunrm`/`rm` (`5`), `cdr` (`6`), `fcgi` (`7`), `line` (`8`)
//!   and `tpkt` (`9`) — `line` delivers the delimiter (`gen_tcp:recv`'s behaviour, the C's
//!   `packet_get_body` "as is"). **Absent by design** (a typed error, never a wrong frame): the
//!   HTTP family `http`/`httph`/`http_bin`/`httph_bin` (`10`/`11`/`13`/`14`) and `ssl_tls`
//!   (`12`), whose bodies need the C's term-building `PacketCallbacks`.
//! * **Per-operation send timeouts** — `send_timeout`/`send_timeout_close`
//!   (`INET_LOPT_TCP_SEND_TIMEOUT`(`:30`)/`_CLOSE`(`:35`)). Both options are **stored and
//!   reported** (on a stream socket, the gate the C applies) but not acted on: the C arms a
//!   driver multi-timer (`add_multi_timer(..., &tcp_inet_send_timeout)`, `:13789`) that fires
//!   while output is queued and answers an **asynchronous** `inet_reply`-shaped
//!   `{error, timeout}` (closing the socket under `send_timeout_close`, `:12458`-`:12482`). The
//!   deadline is recorded for `getopts` (byte-exact against the oracle) and any such
//!   `{error, timeout}` reply is a handoff. A **receive** request's own `Timeout(4)` operand
//!   (`:12277`) *is* honoured: it is the deadline of the asynchronous operation (see *The
//!   asynchronous-operation queue* below), and `TCP_REQ_RECV`'s `Length(4)` operand is honoured
//!   for the raw header type — "exactly `n` bytes".
//! * **`controlling_process`** is **not** driver state: `inet:tcp_controlling_process/2`
//!   (`lib/kernel/src/inet.erl:4848`) is `erlang:port_connect(S, NewOwner)` plus an Erlang-side
//!   drain of already-buffered `{tcp, S, Data}`/`{inet_async, …}` messages. There is no
//!   `INET_REQ_*` for it, and the [`crate::Driver`] seam has no notion of an owner — see the
//!   handoff note in the crate `CONTEXT.md`.
//! * **IPv6, UDP, SCTP and Unix-domain sockets, and DNS resolution** — `open` accepts only
//!   `AF_INET`/`SOCK_STREAM` (`:11961`-`:11990`) and `bind`/`connect` only the packed
//!   IPv4/`INADDR_ANY`/`INADDR_LOOPBACK` forms (`:4795`); any other family tag is `BadArg`.
//!
//! # The asynchronous-operation queue
//!
//! The C's `recv`/`accept` cannot be answered by their control call — the data (or the
//! connection) is not there yet — so they are request/reply exchanges: the reply is an **id**
//! (`[1] ++ <<Id:16/big>>`) and the outcome arrives later through the crate's
//! [`completion`](crate::completion) channel as `{inet_async, Port, Id, Result}`. That queue is
//! implemented for **both** numeric requests ([`self::op_queue`]):
//!
//! * `TCP_REQ_RECV`(42) takes the C's `Timeout(4), Length(4)` operand and occupies the
//!   **single** receive slot (`INET_MAX_ASYNC = 1`, `:1069`), so a second concurrent receive is
//!   the C's `[0]ealready`. The data is delivered as
//!   [`InetAsyncResult::Data`](crate::completion::InetAsyncResult::Data) — one
//!   [`ReadMessage`](crate::port_task::ReadMessage) per read, the `{packet, N}` header already
//!   stripped.
//! * `INET_REQ_ACCEPT`(26) takes the four-byte `Timeout` and joins the **multi-operation** accept
//!   list (`enq_multi_op`, `:12194`/`:12209`), so N pending accepts each get their own id, all
//!   complete in enqueue order, and `INET_REQ_GETSTATUS` reports the C's
//!   `INET_F_ACC`/`INET_F_MULTI_CLIENT` bits while the list is non-empty.
//!
//! The id counter is the C's `NEW_ASYNC_ID` (`op_ref++ & 0xffff`, `:1549`): it starts at `0` and
//! is taken *before* the capacity check, so a refused enqueue still advances it. A completion is
//! produced at exactly three points — socket readiness ([`Driver::ready_input`]), the port timer
//! ([`Driver::next_timeout`]/[`Driver::timeout`], armed from the operation's deadline), and the
//! round's async drive point ([`Driver::drive_async`], which serves data that arrived *before* the
//! operation was enqueued) — never inside the control call. A close drains every pending
//! operation with `{error, closed}` (`async_error_am_all`, `:2597`-`:2608`) before the descriptor
//! goes away. A command's `[n:be16][n ETF bytes]` **caller tag** is likewise answered through it
//! (`{inet_reply, Port, Reply, Tag}`, `inet_reply_finish` `:2614`-`:2637`).
//!
//! **Divergence:** the C addresses a completion to `desc->caller`; the frozen [`crate::Driver`]
//! seam has no caller notion, so a completion is reported to the **port's owner** (see the crate
//! `CONTEXT.md`).
//!
//! # Readiness and flow control
//!
//! The poller's backend is **edge-triggered** (`mio` registers descriptors with `EPOLLET`), so
//! every callback drains to `EAGAIN`: [`Driver::ready_input`] reads until the socket is empty
//! (or reports EOF) and the write path loops until the output queue is drained or the kernel
//! send buffer is full.
//!
//! Unlike the built-in [`crate::drivers::FdDriver`] — which has a separate read fd and write
//! fd, and therefore two poller registrations — a socket is **one** descriptor, so this
//! driver keeps a single registration and flips its [`Interest`] mask: readable while
//! connected, plus writable while the output queue is non-empty (a short write leaves the
//! remainder queued and **arms** write readiness) and while an asynchronous `connect` is
//! completing. A fully drained queue drops writability again so the port cannot spin.
//!
//! `read(2) == 0` is end-of-file, and a connection reset (`ECONNRESET`/`ECONNABORTED` on a
//! read, `ECONNRESET`/`EPIPE` on a write) is the same event: the peer's close is **latched**
//! ([`TcpInetDriver::peer_closed`]), the socket's poller registration is released so a dead
//! descriptor cannot keep producing readiness, and the socket is not touched again. Every
//! later `recv`/`send` then answers the C's `[0] ++ "closed"` **once** and `[0] ++ "enotconn"`
//! after that — the C's `TCP_ADDF_DELAYED_CLOSE_RECV`/`TCP_ADDF_DELAYED_CLOSE_SEND`
//! bookkeeping (`inet_drv.c:855`-`:856`, `:12279`, `:12682`), which is how a caller that
//! ignores its send errors still learns about the close from its next receive — instead of
//! reading `0` repeatedly or writing into a socket the peer has gone from. Output still queued
//! when the close is latched can never be delivered, so it is dropped (the C's
//! `tcp_clear_output`, `inet_drv.c:12786`). Writes use `send(2)` with `MSG_NOSIGNAL`, so a
//! vanished peer is never a process-killing `SIGPIPE`.
//!
//! # One socket, one port (the `accept` divergence)
//!
//! The C's `INET_REQ_ACCEPT` creates a **second** Erlang port for the accepted socket
//! (`tcp_inet_copy` + `driver_create_port` + `async_ok_port`, `:12236`-`:12252`) and answers
//! the control call with a two-byte async id. A [`crate::Driver`] cannot create a port, so the
//! **numeric** request queues on the multi-operation accept list and reports what it took off the
//! listener inside its completion
//! ([`InetAsyncResult::Accepted`](crate::completion::InetAsyncResult::Accepted) — the
//! descriptor, put into non-blocking mode). The runtime opens the second port itself
//! ([`TcpInetDriver::from_socket`], or the already-served `INET_REQ_FDOPEN`); the descriptor is
//! handed over **owned**, so the driver never stores it, no teardown path ([`Driver::stop`],
//! `Drop`) can close it, and the listener keeps accepting.
//!
//! The **named** `accept` verb is the older, this floor's own shape: it **transfers this port to
//! the connection** (the listener is deregistered and closed, the accepted descriptor becomes the
//! port's socket, registered [`Interest::READABLE`], and the reply is `[1] ++ fd_be32`). It exists
//! for a direct caller that wants a read-now answer; `{inet_async, …}` is never produced by it.
//!
//! # Ownership
//!
//! The driver **owns** its descriptors: [`TcpInetDriver::from_socket`] adopts a caller's
//! socket, `open` creates its own, the named `accept` verb adopts the connection it took, and
//! every one of them is closed on `stop`, on the `close` verb and on `Drop`; nothing else may
//! close them. The **one** exception is a descriptor handed over by a numeric `accept`
//! completion: it belongs to the completion's consumer from the moment it is reported, and no
//! path here closes it. All raw socket syscalls
//! (`socket`/`bind`/`listen`/`accept`/`connect`/`getsockname`/`getsockopt`/`setsockopt`/`read`/
//! `send`/`shutdown`/`close`) live in the private `sys` submodule, and every `unsafe` block
//! carries a `// SAFETY:` rationale.
//!
//! Legacy C ground truth (read-only): `erts/emulator/drivers/common/inet_drv.c` and the
//! Erlang half `erts/preloaded/src/prim_inet.erl` (the magic number and the packed argument
//! encoders).

use std::os::unix::io::RawFd;
use std::time::Instant;

use beam_common::Result;

use crate::driver::{Driver, DriverContext};
use crate::drivers::fd::set_nonblocking;
use crate::error;
use crate::poller::{Interest, Token};

mod frame;
mod opcode;
mod opt;
// The asynchronous-operation queue (`TCP_REQ_RECV`/`INET_REQ_ACCEPT` and the
// `{inet_async, Port, Id, Result}` completions). The file cannot be named `async.rs` *as a
// module* (`async` is a keyword), so it is declared through a `#[path]` alias.
#[path = "async.rs"]
mod op_queue;
pub mod packet;
mod stat;
mod sys;
mod udp;
mod wire;

// The wire vocabulary (control codes, packed-address codec, errno renderer) lives in the
// private `wire` submodule; the constants are re-exported so callers keep reaching them at
// `crate::drivers::inet::*`.
pub use self::wire::{
    INET_AF_ANY, INET_AF_INET, INET_AF_LOOPBACK, INET_PROTO_DEFAULT, INET_PROTO_TCP,
    INET_PROTO_UDP, INET_REP_ERROR, INET_REP_OK, INET_TYPE_DGRAM, INET_TYPE_STREAM, TCP_SHUT_RD,
    TCP_SHUT_RDWR, TCP_SHUT_WR,
};
use self::wire::{
    ctl_error, ctl_ok, ctl_xerror, errno_name, parse_address, parse_fake_address, FakeAddress,
    InetAddr,
};

// The built-in datagram driver (the `udp_inet` entry point) is a sibling module that reuses this
// module's wire vocabulary, option store and `sys` wrappers; its public surface is re-exported
// the same way the TCP driver's is.
pub use self::udp::{make_udp_inet, UdpInetDriver, UDP_DRIVER_NAME};

// The `INET_REQ_SETOPTS`/`INET_REQ_GETOPTS` option vocabulary lives in the private `opt`
// submodule: the four syscall-backed codes (`keepalive`/`sndbuf`/`recbuf`/`nodelay`) plus the
// six driver-state codes (`{active, …}`/`{packet, …}`/`{send_timeout, …}`/`{packet_size, …}`/
// `{send_timeout_close, …}`/`{line_delim, …}`) and their value vocabulary. Its codes are
// re-exported the same way the wire vocabulary is.
pub use self::opt::{
    INET_ACTIVE, INET_INFINITY, INET_LOPT_ACTIVE, INET_LOPT_LINE_DELIM, INET_LOPT_PACKET,
    INET_LOPT_PACKET_SIZE, INET_LOPT_TCP_SEND_TIMEOUT, INET_LOPT_TCP_SEND_TIMEOUT_CLOSE,
    INET_MULTI, INET_ONCE, INET_OPT_KEEPALIVE, INET_OPT_RCVBUF, INET_OPT_SNDBUF, INET_PASSIVE,
    TCP_OPT_NODELAY,
};
use self::opt::{get_options, set_options, OptScope, SocketOptions};

// The read-path framing glue (the bridge from the stored socket options to the pure decoder in
// `packet`) lives in the private `frame` submodule.
use self::packet::PacketType;

// The numeric control-opcode bridge: `erlang:port_control/3` sends the C's `INET_REQ_*` number
// (`Op + ERTS_INET_DRV_CONTROL_MAGIC_NUMBER`) as a decimal command string, so a numeric command is
// decoded here into the same verbs the named ones reach.
use self::opcode::{decode as decode_opcode, Opcode};

// The asynchronous-operation queue of this descriptor — the C's `enq_async` slot plus its
// `enq_multi_op` accept list, the `NEW_ASYNC_ID` counter and the completion production points
// (`ready_input`/`timeout`/`drive_async`). Its own module doc is `src/drivers/inet/async.rs`.
use self::op_queue::{drain_closed, AsyncQueue};
// The raw `libc` socket wrappers (`socket`/`accept`/`bind`/`listen`/`getsockopt`/`shutdown`/
// `close`) live in the private `sys` submodule, so every `unsafe` block of the driver sits in
// one place.
use self::sys::{
    close_fd, has_peer, is_listening, local_port, shutdown_socket, socket_error, socket_fd,
    wait_connected,
};

/// The driver's registered name — the C `tcp_inet` entry (`inet_drv.c:1348`), which is also
/// what the oracle reports from `port_info(Port, name)` for a TCP port.
pub const DRIVER_NAME: &str = "tcp_inet";

/// The read buffer size of a single [`Driver::ready_input`] syscall.
const READ_CHUNK: usize = 64 * 1024;

/// The longest a synchronous `connect` completion waits for the kernel, in milliseconds.
///
/// The C's `INET_REQ_CONNECT` is an **async** operation: it hands the request's `Timeout(4)`
/// operand (`-1`, the C's `INET_INFINITY`, is what `prim_inet:connect/3` sends for `infinity`) to
/// its async-op queue and returns immediately. Connect is the one request this floor still answers
/// inline rather than through the async queue (see *The asynchronous-operation queue* in the module
/// docs), so the timeout becomes a **blocking** budget, and an infinity (or any other negative
/// value) has to be capped: the callback runs on the runtime's single-threaded pump, which must
/// never be parked for a silently dropped SYN (the kernel would retry for minutes).
const CONNECT_WAIT_CAP_MS: i32 = 30_000;

/// The `poll(2)` budget, in milliseconds, a `connect` request's `Timeout(4)` operand asks for,
/// clamped to [`CONNECT_WAIT_CAP_MS`].
///
/// A negative operand is the C's `INET_INFINITY` and becomes the cap; `0` means "poll once without
/// waiting" (the C's `timeout == 0` behaviour); a positive one is its own value, clamped.
const fn connect_wait_ms(timeout_ms: i32) -> i32 {
    if timeout_ms < 0 {
        CONNECT_WAIT_CAP_MS
    } else if timeout_ms < CONNECT_WAIT_CAP_MS {
        timeout_ms
    } else {
        CONNECT_WAIT_CAP_MS
    }
}
/// The lifecycle state of a TCP port.
///
/// It mirrors the shape of the C descriptor's `inet.state` (`INET_STATE_*`): the socket does
/// not exist until `open`, `bind` leaves it open, `listen` makes it a listener, and
/// `connect`/`accept` make it a connection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InetState {
    /// No socket (the initial state, and the state [`TcpInetDriver::close`] leaves behind).
    Closed,
    /// A socket exists but is neither listening nor connected.
    Open,
    /// The socket is listening (`listen`).
    Listening,
    /// An asynchronous `connect` is in progress.
    Connecting,
    /// The socket is connected.
    Connected,
}

impl InetState {
    /// Whether the socket is connected ([`InetState::Connected`]).
    pub fn is_connected(self) -> bool {
        self == InetState::Connected
    }

    /// Whether the socket is listening ([`InetState::Listening`]).
    pub fn is_listening(self) -> bool {
        self == InetState::Listening
    }

    /// The C's `INET_STATE_*` bit set for this state, as the `INET_REQ_GETSTATUS` reply carries it
    /// (`inet_drv.c:1026`-`:1031`).
    ///
    /// The C's descriptor holds a bit mask — `INET_STATE_CLOSED = 0`, `INET_STATE_OPEN = 1`,
    /// `INET_F_ACTIVE = 4` (connected), `INET_F_LISTEN = 8`, `INET_F_CON = 16` (connecting) — so a
    /// connected socket is `1|4 = 5`, a listener `1|8 = 9` and a connect in progress `1|16 = 17`.
    /// Measured against the oracle: a bare port reports `0`, a TCP socket after `open`+`listen`
    /// reports `9`, a connected TCP socket reports `5`, and a UDP socket after `open`+`bind`
    /// reports `1`.
    pub(super) fn state_code(self) -> u32 {
        /// `INET_STATE_OPEN` (`inet_drv.c:1027`).
        const OPEN: u32 = 1;
        /// `INET_F_ACTIVE` (`inet_drv.c:1029`).
        const ACTIVE: u32 = 4;
        /// `INET_F_LISTEN` (`inet_drv.c:1030`).
        const LISTEN: u32 = 8;
        /// `INET_F_CON` (`inet_drv.c:1031`).
        const CONNECTING: u32 = 16;
        match self {
            InetState::Closed => 0,
            InetState::Open => OPEN,
            InetState::Connected => OPEN | ACTIVE,
            InetState::Listening => OPEN | LISTEN,
            InetState::Connecting => OPEN | CONNECTING,
        }
    }
}

/// The peer-close bookkeeping the C keeps in `TCP_ADDF_DELAYED_CLOSE_RECV`/
/// `TCP_ADDF_DELAYED_CLOSE_SEND` (`inet_drv.c:855`-`:856`).
///
/// Once the peer's close (end-of-file, or a connection reset) has been observed the socket is
/// unusable, but the C does **not** reply `closed` forever: the *first* `recv` and the *first*
/// `send` after the close report `closed`, and every later one reports `enotconn`. That is what
/// lets a caller which ignores its send errors still learn about the close from its next
/// receive (`inet_drv.c:12798`-`:12819`, `:13662`-`:13684`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct PeerClose {
    /// The peer's end-of-file (or a connection reset) has been observed.
    seen: bool,
    /// Whether a `recv` has already reported the close.
    recv_reported: bool,
    /// Whether a `send` has already reported the close.
    send_reported: bool,
}

impl PeerClose {
    /// Records that the peer's close has just been observed, arming the one-shot `closed`
    /// answer for **both** directions.
    fn observe(&mut self) {
        self.seen = true;
        self.recv_reported = false;
        self.send_reported = false;
    }

    /// The reply the next `recv` owes, or `None` while the connection is still usable.
    fn recv_reply(&mut self) -> Option<&'static str> {
        if !self.seen {
            return None;
        }
        if self.recv_reported {
            return Some("enotconn");
        }
        self.recv_reported = true;
        Some("closed")
    }

    /// The reply the next `send` owes, or `None` while the connection is still usable.
    fn send_reply(&mut self) -> Option<&'static str> {
        if !self.seen {
            return None;
        }
        if self.send_reported {
            return Some("enotconn");
        }
        self.send_reported = true;
        Some("closed")
    }
}

/// The TCP socket driver: one listening or connected socket behind one port.
///
/// Constructed either socket-less ([`TcpInetDriver::new`], the [`crate::DriverRegistry`]
/// factory the runtime uses, which then calls the `open` verb) or around an existing socket
/// ([`TcpInetDriver::from_socket`]).
pub struct TcpInetDriver {
    /// The socket this port owns, if one has been opened.
    socket: Option<RawFd>,
    /// The poller token of the socket's registration, once registered.
    token: Option<Token>,
    /// The interest mask currently registered for the socket (one registration: a socket is
    /// a single descriptor, so read and write readiness share the token).
    interest: Interest,
    /// The lifecycle state.
    state: InetState,
    /// Set once the socket reported end-of-file, so no further reads are attempted.
    read_eof: bool,
    /// How far the peer's close has been observed and reported ([`PeerClose`]).
    peer_close: PeerClose,
    /// A `shutdown` deferred because the output queue was not empty yet (the C's
    /// `TCP_ADDF_PENDING_SHUT_WR`/`TCP_ADDF_PENDING_SHUT_RDWR`, `inet_drv.c:12362`) — applied
    /// as soon as the queue drains.
    pending_shutdown: Option<u8>,
    /// The lowercase errno name of a failed **asynchronous** `connect`, if one failed after
    /// the control call had already answered `ok` (there is no reply channel left by then).
    connect_error: Option<String>,
    /// The driver-state socket options the `setopts`/`getopts` verbs store and report
    /// ([`SocketOptions`]): `{active, …}`/`{packet, …}`/`{send_timeout, …}`/`{packet_size, …}`/
    /// `{send_timeout_close, …}`/`{line_delim, …}`.
    options: SocketOptions,
    /// The bytes read from the socket that are **not yet** a whole frame, used only when
    /// `{packet, N}` asks for framing (`{packet, raw}` never touches it). It is the counterpart
    /// of the C descriptor's input buffer (`desc->i_buf`): [`TcpInetDriver::read_available`]
    /// appends freshly read bytes here instead of the port input queue, and the `recv` verb
    /// takes one decoded frame's payload out of it (see [`self::frame`]). Cleared whenever the
    /// socket is replaced or closed (`adopt`/`mark_connected`/`close_socket`), so a fresh
    /// connection never inherits a stale partial frame.
    ///
    /// A `{packet, …}`/`{line_delim, …}` change arriving while bytes sit here is **not**
    /// flushed: the buffer is retained and the *next* `recv` decodes it under the new options
    /// ([`self::frame::options`] is rebuilt from [`TcpInetDriver::options`] on every call).
    /// Re-decoding the retained bytes at the option boundary is the clean choice — the driver
    /// holds no absolute stream position to re-parse from, and dropping the bytes would lose
    /// data. A switch **to** [`PacketType::Raw`] therefore delivers the whole retained buffer
    /// (plus everything after it) verbatim, so raw mode stays authoritative for queued bytes.
    pending_input: Vec<u8>,
    /// The C descriptor's cached address family (`inet.sfamily`, `inet_drv.c:10925`), as the
    /// wire's [`INET_AF_INET`] tag, with `0` meaning "unset". Set by `open`/`fdopen` and
    /// **never** cleared by a close, which is why `INET_REQ_GETTYPE` still answers after one.
    family: u16,
    /// The C descriptor's cached socket type (`inet.stype`), as the wire's
    /// [`INET_TYPE_STREAM`]/[`INET_TYPE_DGRAM`] tag; `0` = unset. Same lifetime as [`Self::family`].
    stype: u16,
    /// The **fake** local endpoint an `INET_REQ_SETNAME` installed, reported by a later
    /// `INET_REQ_NAME` in place of `getsockname(2)` (`desc->name_ptr`, `inet_drv.c:1269`,
    /// `:11370`-`:11377`). `None` means "ask the kernel", which is also what the clearing form
    /// (`len == 0`) restores. Like the C's, it is **not** cleared by a close.
    name_addr: Option<InetAddr>,
    /// The fake peer endpoint an `INET_REQ_SETPEER` installed, reported by a later
    /// `INET_REQ_PEER` in place of `getpeername(2)` (`desc->peer_ptr`, `inet_drv.c:1268`,
    /// `:11284`-`:11290`) — **after** that request's `INET_F_ACTIVE` guard, so a fake peer on an
    /// unconnected port is still `enotconn`.
    peer_addr: Option<InetAddr>,
    /// The descriptor's statistics counters (the C's `inet_descriptor` block,
    /// `inet_drv.c:11560`-`:11623`; see [`stat::InetStat`]): advanced where a payload is
    /// delivered to the port (input) or a command is issued (output), and rendered by
    /// `INET_REQ_GETSTAT`. Zeroed by construction, as `inet_init_desc` leaves it.
    stat: stat::InetStat,
    /// The C's `desc->prebound` flag (`inet_drv.c:5266`): set by the `INET_REQ_FDOPEN` adoption,
    /// which takes a **caller-owned** descriptor over. A prebound descriptor is never `close(2)`d
    /// — the C's `erl_inet_close` only drops the select registration for one and leaves
    /// `desc->s`/`desc->state` untouched (`:5102`-`:5117`) — so neither a close nor a `Drop` may
    /// release it (see [`TcpInetDriver::adopt_prebound`] and
    /// [`TcpInetDriver::close_socket`]). Every route that installs a driver-**owned** socket
    /// (`open`/`accept`/[`TcpInetDriver::from_socket`]) clears it.
    prebound: bool,
    /// The descriptor's **asynchronous-operation queue** — the C's single `enq_async` receive slot
    /// plus its `enq_multi_op` accept list, and the `NEW_ASYNC_ID` counter behind both
    /// ([`AsyncQueue`], `src/drivers/inet/async.rs`).
    ///
    /// A queued operation is answered by its control op with `[1] ++ <<Id:16/big>>` and completed
    /// later through the driver→owner completion channel
    /// ([`DriverContext::deliver_completion`]); see that module's docs for the three production
    /// points and the one divergence (every completion is addressed to the port's **owner**, since
    /// the frozen [`Driver`] seam has no `driver_caller` notion).
    async_ops: AsyncQueue,
    /// The caller tag stripped from the **queue head** of the command currently being written
    /// ([`TcpInetDriver::strip_caller_ref`]) — the C's `desc->caller_ref.tag_buf`.
    ///
    /// The C carries it for the duration of one command and finishes it in `inet_reply_finish`
    /// (`inet_drv.c:2614-2637`). The same per-command state is needed here because a short write
    /// leaves the payload queued and the remaining bytes are **no longer** prefixed, so the strip
    /// must happen once per command, not once per `ready_output`.
    caller_ref: Option<Vec<u8>>,
    /// Whether the current command's caller-tag prefix has already been stripped from the output
    /// queue (see [`TcpInetDriver::caller_ref`]).
    caller_stripped: bool,
}

impl TcpInetDriver {
    /// A TCP driver with no socket yet: the state a freshly opened port starts in.
    ///
    /// The socket is created by the `open` verb (or adopted by
    /// [`TcpInetDriver::from_socket`]), mirroring the C where `tcp_inet_start` builds only the
    /// descriptor and `INET_REQ_OPEN` creates the socket.
    pub fn new() -> TcpInetDriver {
        TcpInetDriver {
            socket: None,
            token: None,
            interest: Interest::READABLE,
            state: InetState::Closed,
            read_eof: false,
            peer_close: PeerClose::default(),
            pending_shutdown: None,
            connect_error: None,
            options: SocketOptions::default(),
            pending_input: Vec::new(),
            family: 0,
            stype: 0,
            name_addr: None,
            peer_addr: None,
            stat: stat::InetStat::default(),
            prebound: false,
            async_ops: AsyncQueue::new(),
            caller_ref: None,
            caller_stripped: false,
        }
    }

    /// Adopts an existing socket, taking ownership of `fd` (closed on `stop`/`close`/`Drop`).
    ///
    /// The socket's state is read from the kernel, exactly as the C's `INET_REQ_FDOPEN` does
    /// (`inet_ctl_fdopen`, `inet_drv.c:5250`-`:5260`): a listening socket (`SO_ACCEPTCONN`)
    /// starts in [`InetState::Listening`], a socket that has a peer (`getpeername(2)`) in
    /// [`InetState::Connected`], and anything else in [`InetState::Open`]. `fd` is put into
    /// non-blocking mode by [`Driver::start`].
    ///
    /// A socket whose peer has already closed is adopted as connected; the close is discovered
    /// by the first read (which answers [`TcpInetDriver::peer_closed`]).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when `fd` is not a usable socket (`getsockopt` fails).
    pub fn from_socket(fd: RawFd) -> Result<TcpInetDriver> {
        let state = if is_listening(fd)? {
            InetState::Listening
        } else if has_peer(fd)? {
            InetState::Connected
        } else {
            InetState::Open
        };
        Ok(TcpInetDriver {
            socket: Some(fd),
            token: None,
            interest: Interest::READABLE,
            state,
            read_eof: false,
            peer_close: PeerClose::default(),
            pending_shutdown: None,
            connect_error: None,
            options: SocketOptions::default(),
            pending_input: Vec::new(),
            family: u16::from(INET_AF_INET),
            stype: sys::socket_type(fd)?,
            name_addr: None,
            peer_addr: None,
            stat: stat::InetStat::default(),
            prebound: false,
            async_ops: AsyncQueue::new(),
            caller_ref: None,
            caller_stripped: false,
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

    /// Whether the socket reported end-of-file.
    pub fn eof(&self) -> bool {
        self.read_eof
    }

    /// Whether the peer's close has been observed — a read end-of-file, or a connection reset
    /// seen by a read or a write.
    ///
    /// Once this is true the connection is gone: the socket is not touched again, output still
    /// queued was dropped, and the driver's `recv`/`send` answer the C's `[0] ++ "closed"`
    /// (once) and then `[0] ++ "enotconn"` (see [`PeerClose`]). The runtime reads it to emit
    /// its `{tcp_closed, Port}` notification and to fail a command the driver could not
    /// deliver with `{error, closed}` — it is the **only** new accessor the runtime needs; no
    /// [`crate::PortNotification`] variant is involved.
    pub fn peer_closed(&self) -> bool {
        self.peer_close.seen
    }

    /// The lowercase errno name of a `connect` that failed, if one has.
    ///
    /// A connect is completed **inside** the control call
    /// (see [`TcpInetDriver::verb_connect`]), so a failure is also answered there as
    /// `[0] ++ errno`; this accessor is what the runtime reads it back from, and the only path that
    /// leaves it set *without* a failing reply is the wait budget expiring (`[0]etimedout`) and the
    /// attempt then failing later, observed through [`Driver::ready_output`]. It stays set until
    /// the socket is replaced or closed.
    pub fn connect_error(&self) -> Option<&str> {
        self.connect_error.as_deref()
    }

    /// Whether a socket has been opened (`INET_STATE_OPEN` in the C's sense: anything but
    /// [`InetState::Closed`]).
    fn has_socket(&self) -> bool {
        self.socket.is_some()
    }

    /// The connected socket, or `BadArg` when there is none.
    fn connected_socket(&self) -> Result<RawFd> {
        if !self.state.is_connected() {
            return Err(error::badarg());
        }
        self.socket.ok_or_else(error::badarg)
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
                self.read_eof = false;
                self.peer_close = PeerClose::default();
                self.connect_error = None;
                // A fresh socket starts a fresh byte stream: any partial frame buffered for
                // the previous socket no longer belongs to this one.
                self.pending_input.clear();
                // The C's `inet_ctl_open` records the family and type here
                // (`inet_drv.c:5214`-`:5216`), and nothing clears them on a close: `inet.stype`
                // is `SOCK_STREAM` from `open` on, which is what gates the `send_timeout`
                // codes and the datagram-only `{read_packets, N}`.
                self.family = u16::from(INET_AF_INET);
                self.stype = u16::from(INET_TYPE_STREAM);
                // An `open`/`accept` descriptor is the driver's own: it is closed on teardown, so
                // this port is no longer (or never was) holding a caller's `INET_REQ_FDOPEN`
                // descriptor.
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
    /// (`inet_drv.c:5223`-`:5282`), the body behind `tcp_inet_ctl`'s `INET_REQ_FDOPEN` arm.
    ///
    /// This is [`TcpInetDriver::adopt`]'s prebound twin, and the difference is **ownership**: the
    /// descriptor belongs to whoever handed it to `INET_REQ_FDOPEN`, so it is never closed here —
    /// not on a failed `set_nonblocking(2)`, not on a failed poller registration, not by
    /// [`TcpInetDriver::close_socket`] and not by `Drop`. `prebound` records that, exactly as the
    /// C's `desc->prebound = 1` does (`:5266`), which its close path honours by only dropping the
    /// select registration (`erl_inet_close`, `:5102`-`:5117`).
    ///
    /// `stype` is the `INET_TYPE_*` tag the request's operand carried (the caller has already run
    /// the length/family/type ladder and the `sock_name(2)` pre-check, [`wire::fdopen_precheck`]).
    /// The lifecycle state is the C's: [`InetState::Open`], promoted to
    /// [`InetState::Connected`] when a `SOCK_STREAM` adoption has a peer — the C's `sock_peer`
    /// probe (`:5252`-`:5260`) — which is why an adopted bound socket reports `INET_STATE_OPEN`
    /// (`GETSTATUS` `[1,0,0,0,1]`) and an adopted accepted connection
    /// `INET_STATE_CONNECTED` (`[1,0,0,0,5]`).
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
            // which is the one behavioural difference from [`TcpInetDriver::adopt`].
            Err(err) => return Err(err),
        };
        self.socket = Some(fd);
        self.token = Some(token);
        self.interest = Interest::READABLE;
        self.read_eof = false;
        self.peer_close = PeerClose::default();
        self.connect_error = None;
        // A fresh socket starts a fresh byte stream: any partial frame buffered for the previous
        // socket no longer belongs to this one.
        self.pending_input.clear();
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
    /// Marks the socket as a fresh, usable connection.
    ///
    /// A `connect`/`accept` that succeeded replaces whatever the previous socket was doing, so
    /// the peer-close bookkeeping starts over too.
    fn mark_connected(&mut self) {
        self.state = InetState::Connected;
        self.read_eof = false;
        self.peer_close = PeerClose::default();
        // The new connection's byte stream begins now: drop any partial frame buffered for the
        // socket this one replaces.
        self.pending_input.clear();
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
    /// Used after [`TcpInetDriver::deregister`] and by `Drop` (which has no poller to
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
        self.read_eof = false;
        self.peer_close = PeerClose::default();
        self.pending_shutdown = None;
        self.pending_input.clear();
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

    /// Reads the socket into the port input queue (the `{packet, raw}` default) or, when the
    /// `{packet, N}` option asks for framing, into the driver's own framing buffer
    /// ([`TcpInetDriver::pending_input`]) so the `recv` verb can take whole frames out of it.
    ///
    /// A no-op unless the socket is connected (a listening socket is read through `accept`,
    /// not `read(2)`) and the peer's close has not already been observed. Returns the number of
    /// bytes read.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when the read fails irrecoverably.
    fn read_available(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        let mut bytes = Vec::new();
        let read = self.read_bytes(ctx, &mut bytes)?;
        if self.framed().is_some() {
            // `{packet, N}`: the bytes are not a message yet — hold them for the frame decoder.
            self.pending_input.extend_from_slice(&bytes);
        } else {
            // `{packet, raw}`: deliver any bytes retained from an earlier framing mode first, so
            // raw mode is authoritative for everything already queued (see the field's note).
            let retained = core::mem::take(&mut self.pending_input);
            // `inet_input_count(desc, len)` at `inet_drv.c:13064` (`tcp_deliver`): the C counts
            // the bytes it **delivers** to the port, and in raw mode that is everything pushed
            // onto the port input queue here — any retained (previously buffered) bytes plus
            // whatever the read just collected, each counted exactly once.
            let delivered = retained.len() + bytes.len();
            self.stat.input(delivered);
            ctx.input().push(&retained);
            ctx.input().push(&bytes);
        }
        Ok(read)
    }

    /// Reads the socket to `EAGAIN` into `out`, appending every byte the kernel hands over.
    ///
    /// A no-op unless the socket is connected and the peer's close has not already been observed.
    /// Returns the number of bytes read. The edge-triggered poller means a caller must drain to
    /// `EAGAIN` per readiness callback, which this loop does.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when the read fails irrecoverably.
    fn read_bytes(&mut self, ctx: &mut DriverContext<'_>, out: &mut Vec<u8>) -> Result<usize> {
        if !self.state.is_connected() || self.peer_close.seen {
            return Ok(0);
        }
        let fd = self.connected_socket()?;
        let mut read = 0usize;
        let mut buf = [0u8; READ_CHUNK];
        loop {
            // SAFETY: `fd` is an open, non-blocking socket owned by this driver and `buf` is
            // a writable array of `READ_CHUNK` bytes, so `read(2)` writes at most `READ_CHUNK`
            // bytes into it.
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), READ_CHUNK) };
            if n > 0 {
                let n = n as usize;
                out.extend_from_slice(&buf[..n]);
                read += n;
                // Edge-triggered readiness: keep reading until the socket is empty.
                continue;
            }
            if n == 0 {
                // The peer closed its end: latch the close so this `recv` answers `closed`
                // and no further read is ever attempted.
                self.read_eof = true;
                self.latch_peer_close(ctx)?;
                return Ok(read);
            }
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EAGAIN) => return Ok(read),
                // A reset (or an aborted connection) is the peer's close, not a read failure:
                // the C's `tcp_recv` routes `ECONNRESET` to `tcp_recv_closed`
                // (`inet_drv.c:13202`).
                Some(libc::ECONNRESET) | Some(libc::ECONNABORTED) => {
                    self.read_eof = true;
                    self.latch_peer_close(ctx)?;
                    return Ok(read);
                }
                _ => return Err(error::io(err)),
            }
        }
    }

    /// The `{packet, N}` type the stored options select for framing, or `None` when received
    /// bytes are delivered raw ([`PacketType::Raw`] and an out-of-range code — see
    /// [`self::frame::framed`]).
    fn framed(&self) -> Option<PacketType> {
        frame::framed(&self.options)
    }

    /// Latches the peer's close and drops the output that can no longer be delivered.
    ///
    /// This is the C's close path (`tcp_recv_closed`/`tcp_send_error`): the socket is
    /// de-registered from the poller (a dead descriptor must not keep reporting readiness, the
    /// rule `crate::drivers::FdDriver` follows at end-of-file) and its queued output is
    /// discarded (`tcp_clear_output`, `inet_drv.c:12786`), because nothing can ever reach the
    /// peer again. The **next** `recv`/`send` answers `closed` and every later one `enotconn`
    /// (see [`PeerClose`]).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when the poller rejects the de-registration.
    fn latch_peer_close(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        self.peer_close.observe();
        self.pending_shutdown = None;
        // The peer is gone, so every queued operation can only end one way: the C's
        // `async_error_am_all(am_closed)` (`inet_drv.c:11887`, `:2597`-`:2608`). It runs before
        // the queued output is dropped so a failure to stage a completion is not swallowed by
        // the teardown below.
        drain_closed(&mut self.async_ops, ctx)?;
        ctx.output().clear();
        self.deregister(ctx)
    }

    /// Drains the port's output queue to the socket, returning the number of bytes written.
    ///
    /// A short write (`EAGAIN`) leaves the remainder queued and arms write readiness; a fully
    /// drained queue drops write readiness again (keeping readability armed) and applies a
    /// `shutdown` that was deferred because the queue was not empty (the C's
    /// `TCP_ADDF_PENDING_SHUT_*`).
    ///
    /// A write that fails because the peer is gone (`ECONNRESET`/`EPIPE`) is **not** an I/O
    /// error: it latches the peer's close (see [`TcpInetDriver::peer_closed`]), exactly as the
    /// C's `tcp_send_error` routes to `tcp_recv_closed` (`inet_drv.c:13440`).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when the socket is not connected — the C's `tcp_inet_command`
    /// fails the port on a socket it cannot write (`inet_drv.c:12629`); the queued bytes are
    /// **not** consumed on that path, so the refusal is visible to the caller rather than silently
    /// dropping data — or `BeamError::Io` when the write fails irrecoverably.
    fn write_pending(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        if self.peer_close.seen {
            // The peer is gone: the queued output was dropped when the close was latched and
            // the socket must not be touched again.
            return Ok(0);
        }
        let fd = self.connected_socket()?;
        let mut written = 0usize;
        let mut stalled = false;
        loop {
            let front = ctx.output().front();
            if front.is_empty() {
                break;
            }
            let len = front.len();
            // SAFETY: `fd` is an open, non-blocking socket owned by this driver and `front`
            // points to `len` initialized bytes — `send(2)` reads at most that many and never
            // retains the pointer. `MSG_NOSIGNAL` keeps a vanished peer's `EPIPE` a return
            // value instead of a process-killing signal.
            let n = unsafe { libc::send(fd, front.as_ptr().cast(), len, libc::MSG_NOSIGNAL) };
            if n > 0 {
                let n = n as usize;
                ctx.output().consume(n);
                written += n;
                continue;
            }
            if n == 0 {
                // No progress: treat it like a full buffer and let readiness re-drive us
                // instead of spinning here.
                stalled = true;
                break;
            }
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                // A signal interrupted the write; retry.
                Some(libc::EINTR) => continue,
                // The kernel send buffer is full (`EWOULDBLOCK` shares `EAGAIN`'s value on
                // every platform this crate targets).
                Some(libc::EAGAIN) => {
                    stalled = true;
                    break;
                }
                // The peer vanished: a reset, or a write once its close was known. That is the
                // connection ending, so it is latched (and reported by the caller's reply)
                // rather than surfaced as a bare I/O error.
                Some(libc::ECONNRESET) | Some(libc::EPIPE) => {
                    self.latch_peer_close(ctx)?;
                    return Ok(written);
                }
                _ => return Err(error::io(err)),
            }
        }

        if ctx.output().is_empty() {
            // The command is fully written, so the next one must be examined for a caller tag
            // afresh. `caller_ref` is deliberately **not** cleared here: the reply it produces is
            // staged by the caller ([`TcpInetDriver::settle_command`]).
            self.caller_stripped = false;
            if let Some(how) = self.pending_shutdown.take() {
                shutdown_socket(fd, how)?;
            }
            if self.interest.is_writable() {
                self.set_interest(ctx, Interest::READABLE)?;
            }
        } else if stalled && !self.interest.is_writable() {
            self.set_interest(ctx, Interest::BOTH)?;
        }
        Ok(written)
    }

    /// Completes an asynchronous `connect` signalled by write readiness.
    ///
    /// The pending socket error is taken (`SO_ERROR`, which `getsockopt` also clears): zero
    /// means the connection is up (read readiness is armed and the state becomes
    /// [`InetState::Connected`]); anything else means it failed, and the descriptor is closed
    /// — mirroring the C, which tears the descriptor down on a failed connect — with the
    /// errno recorded in [`TcpInetDriver::connect_error`].
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when there is no socket, or whatever the poller returns.
    fn finish_connect(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        let fd = self.socket.ok_or_else(error::badarg)?;
        let err = socket_error(fd)?;
        if err == 0 {
            self.mark_connected();
            self.set_interest(ctx, Interest::READABLE)
        } else {
            self.release_socket(ctx)?;
            self.connect_error = Some(errno_name(err));
            Ok(())
        }
    }

    /// The `open` verb: creates the socket and registers it for read readiness.
    ///
    /// # Errors
    ///
    /// [`beam_common::BeamError::BadArg`] for an argument that is not the three-byte
    /// `[AF_INET, SOCK_STREAM, default|tcp]` record; `BeamError::Io` when the socket cannot be
    /// created.
    fn verb_open(&mut self, ctx: &mut DriverContext<'_>, arg: &[u8]) -> Result<Vec<u8>> {
        if self.has_socket() {
            // The C has no such guard (its OPEN would leak the old socket); this floor
            // refuses to replace a live socket with the operational-error reply instead.
            return Ok(ctl_xerror("einval"));
        }
        let [family, kind, protocol] = arg else {
            return Err(error::badarg());
        };
        // AF_INET / SOCK_STREAM / default-or-TCP only; IPv6, UDP and SCTP are out of scope.
        if *family != INET_AF_INET || *kind != INET_TYPE_STREAM {
            return Err(error::badarg());
        }
        if *protocol != INET_PROTO_DEFAULT && *protocol != INET_PROTO_TCP {
            return Err(error::badarg());
        }
        let fd = socket_fd()?;
        self.adopt(ctx, fd)?;
        self.state = InetState::Open;
        Ok(ctl_ok())
    }

    /// The `bind` verb: binds the socket and reports the port it ended up on.
    ///
    /// # Errors
    ///
    /// `BeamError::BadArg` for a malformed packed address; `BeamError::Io` when `bind(2)` or
    /// the follow-up `getsockname(2)` fails.
    fn verb_bind(&mut self, arg: &[u8]) -> Result<Vec<u8>> {
        if !self.has_socket() {
            return Ok(ctl_xerror("exbadport"));
        }
        let address = parse_address(arg)?;
        let fd = self.socket.ok_or_else(error::badarg)?;
        let sa = address.sockaddr_in();
        // SAFETY: `fd` is an open socket owned by this driver and `sa` is a fully initialized
        // `sockaddr_in`, so the kernel reads at most `size_of::<sockaddr_in>()` bytes of it
        // and never retains the pointer.
        let rc = unsafe {
            libc::bind(
                fd,
                std::ptr::addr_of!(sa).cast(),
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Ok(ctl_error(std::io::Error::last_os_error()));
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

    /// The `listen` verb: turns the socket into a listener.
    ///
    /// # Errors
    ///
    /// `BeamError::BadArg` for an argument that is not a two-byte backlog; `BeamError::Io`
    /// when `listen(2)` fails.
    fn verb_listen(&mut self, arg: &[u8]) -> Result<Vec<u8>> {
        if !self.has_socket() {
            return Ok(ctl_xerror("exbadport"));
        }
        let [hi, lo] = arg else {
            return Err(error::badarg());
        };
        let backlog = i32::from(u16::from_be_bytes([*hi, *lo])).min(i32::MAX);
        let fd = self.socket.ok_or_else(error::badarg)?;
        // SAFETY: `fd` is an open socket owned by this driver and `listen(2)` takes no
        // pointer arguments.
        if unsafe { libc::listen(fd, backlog) } != 0 {
            return Ok(ctl_error(std::io::Error::last_os_error()));
        }
        self.state = InetState::Listening;
        Ok(ctl_ok())
    }

    /// The `connect` verb: connects the socket to a literal IPv4 address.
    ///
    /// A connection the kernel completes immediately arms read readiness and answers `[1]`. One
    /// that reports `EINPROGRESS` is **completed synchronously inside this call** by
    /// [`wait_connected`], under the `poll(2)` budget `timeout_ms` asks for (clamped to
    /// [`CONNECT_WAIT_CAP_MS`], a negative value being the C's `INET_INFINITY`):
    ///
    /// * connected ⇒ [`TcpInetDriver::mark_connected`], read readiness armed, `[1]`;
    /// * failed ⇒ the descriptor is torn down (the C's `tcp_inet_connect_error` closes it), the
    ///   errno is recorded in [`TcpInetDriver::connect_error`] and the reply is `[0] ++ errno`;
    /// * budget spent ⇒ `[0]etimedout` with the socket left in [`InetState::Connecting`] and
    ///   **write** readiness armed, so a completion that arrives later is still observed through
    ///   [`Driver::ready_output`] (and [`TcpInetDriver::connect_error`]).
    ///
    /// The C never answers a bare `[1]` for the in-progress case: it rewrites the request into an
    /// async operation, replies `[1] ++ async_id_be16` and delivers `{inet_async, Port, Ref,
    /// Status}` — connect is the one family this floor still completes inline instead, which is
    /// exactly why the wait above takes its place. A timed-out C operation delivers
    /// `{inet_async, Port, Ref, {error, etimedout}}` where this answers `[0]etimedout`.
    ///
    /// # Errors
    ///
    /// `BeamError::BadArg` for a malformed packed address, or `BeamError::Io` when the socket
    /// cannot be watched at all.
    fn verb_connect(
        &mut self,
        ctx: &mut DriverContext<'_>,
        arg: &[u8],
        timeout_ms: i32,
    ) -> Result<Vec<u8>> {
        match self.state {
            InetState::Closed => return Ok(ctl_xerror("exbadport")),
            InetState::Connected => return Ok(ctl_xerror("eisconn")),
            InetState::Connecting => return Ok(ctl_xerror("einval")),
            InetState::Open | InetState::Listening => {}
        }
        let address = parse_address(arg)?;
        let fd = self.socket.ok_or_else(error::badarg)?;
        let sa = address.sockaddr_in();
        // SAFETY: `fd` is an open, non-blocking socket owned by this driver and `sa` is a
        // fully initialized `sockaddr_in`, so the kernel reads at most
        // `size_of::<sockaddr_in>()` bytes of it and never retains the pointer.
        let rc = unsafe {
            libc::connect(
                fd,
                std::ptr::addr_of!(sa).cast(),
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if rc == 0 {
            self.mark_connected();
            self.set_interest(ctx, Interest::READABLE)?;
            return Ok(ctl_ok());
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINPROGRESS) => {
                self.state = InetState::Connecting;
                match wait_connected(fd, connect_wait_ms(timeout_ms))? {
                    // The connection is up: the C's `tcp_inet_connect_ok` moves to
                    // `INET_STATE_CONNECTED` and switches to read readiness (`:12410`).
                    Some(0) => {
                        self.mark_connected();
                        self.set_interest(ctx, Interest::READABLE)?;
                        Ok(ctl_ok())
                    }
                    // The connect failed: the C's `tcp_inet_connect_error` closes the descriptor
                    // (`:12422`) and answers the errno through its async reply.
                    Some(errno) => {
                        self.release_socket(ctx)?;
                        self.connect_error = Some(errno_name(errno));
                        Ok(ctl_error(std::io::Error::from_raw_os_error(errno)))
                    }
                    // Nothing happened within the budget: leave the attempt running so a later
                    // completion is still observed, and report the timeout the async form would
                    // have reported as a message.
                    None => {
                        self.set_interest(ctx, Interest::WRITABLE)?;
                        Ok(ctl_xerror("etimedout"))
                    }
                }
            }
            // `EISCONN` and `EALREADY` are the racy spellings of "it is already connected or
            // connecting"; report them as the C does rather than failing the control call.
            Some(libc::EISCONN) => {
                self.mark_connected();
                self.set_interest(ctx, Interest::READABLE)?;
                Ok(ctl_ok())
            }
            _ => Ok(ctl_error(err)),
        }
    }
    /// The `send` verb: queues `arg` for output and drains as much as the kernel accepts.
    ///
    /// This is the `tcp_inet_command` path (`inet_drv.c:12629`), which is the port's *output*
    /// callback in the C; the remainder stays queued (with write readiness armed) exactly as
    /// a short `write(2)` leaves it.
    ///
    /// Under a `{packet, 1|2|4}` code the payload is **framed on the wire**: the C's
    /// `tcp_sendv`/`tcp_send` prepend a 1-/2-/4-byte big-endian length header
    /// (`inet_drv.c:13745`-`:13769`, `:13861`-`:13877`), so the peer's own
    /// `packet_get_length` reads the message back as a frame — the write-side counterpart of the
    /// read path's header strip (both live in [`self::frame`]). An **empty** payload still writes
    /// the bare header: the C's shortcut (`if ((h_len == 0) && (len == 0)) return 0`,
    /// `inet_drv.c:13907`) fires only when there is no header either, and the oracle agrees.
    ///
    /// Once the peer's close is known the verb answers `[0] ++ "closed"` **once** and
    /// `[0] ++ "enotconn"` after that (the C's `tcp_inet_commandv` plus its
    /// `TCP_ADDF_DELAYED_CLOSE_SEND` flag, `inet_drv.c:12682`); a `send` whose write is what
    /// discovers the close answers `closed` too.
    ///
    /// # The `prim_inet` caller-reference prefix is *not* stripped here (a seam gap)
    ///
    /// `prim_inet:send/3` prefixes the payload with `[tag_len:be16][tag_bytes]` — the tag being
    /// `term_to_binary(Mref, [local])` for `Mref = monitor(port, S)`
    /// (`erts/preloaded/src/prim_inet.erl:575`-`:620`). The C strips it in
    /// `init_caller`/`init_caller_iov` (`inet_drv.c:1820`-`:1890`), but **only when
    /// `driver_caller(port)` is an internal pid** — and it echoes the tag back as the fourth
    /// element of the later `{inet_reply, S, Reply, Ref}` message (`inet_reply_caller_ref`,
    /// `:2610`-`:2660`).
    ///
    /// This crate's frozen [`Driver`]/[`DriverContext`] seam carries **no caller**, and the TCP
    /// payload after the prefix is a structureless byte stream, so — unlike the `udp_inet` `send`
    /// verb, whose post-prefix grammar starts with an address tag and therefore discriminates
    /// reliably — there is **no sound way to tell a prefixed payload from a data payload that
    /// merely begins with those bytes**. Guessing would corrupt ordinary sends, so this verb
    /// does not guess: a `gen_tcp:send` payload arrives with its prefix intact and is written as
    /// data. Closing the gap needs a caller-carrying seam on `DriverContext` (or the runtime
    /// pre-stripping the prefix above this crate); the `{inet_reply, …}` echo needs the
    /// driver→owner message channel this crate does not have either.
    ///
    /// Note also that the `{packet, 1|2|4}` framing described above is applied by **this verb**
    /// only: [`TcpInetDriver::output`]/[`TcpInetDriver::ready_output`] (the
    /// `erlang:port_command/2` path) go straight to [`TcpInetDriver::write_pending`], which
    /// writes the queued bytes unframed.    ///
    /// A `shutdown(SHUT_WR|SHUT_RDWR)` **deferred** because output was still queued
    /// ([`TcpInetDriver::pending_shutdown`]) rejects a further `send`, exactly as the C does:
    /// `tcp_inet_commandv` routes it to `tcp_shutdown_error(desc, EPIPE)` (`inet_drv.c:12695`),
    /// which runs the close path and answers `{error, closed}` (`tcp_send_or_shutdown_error`,
    /// `:13654`-`:13670`). So this send answers `[0] ++ "closed"`, the queued output is dropped
    /// and every later one answers `enotconn`.
    ///
    /// # Errors
    ///
    /// `BeamError::BadArg` when the socket is not connected, or the write failure
    /// [`TcpInetDriver::write_pending`] reports.
    fn verb_send(&mut self, ctx: &mut DriverContext<'_>, arg: &[u8]) -> Result<Vec<u8>> {
        if !self.state.is_connected() {
            return Ok(ctl_xerror("enotconn"));
        }
        if let Some(name) = self.peer_close.send_reply() {
            return Ok(ctl_xerror(name));
        }
        if self.pending_shutdown.is_some() {
            // A write shutdown could not be applied yet (the queue was not empty), and the C
            // refuses a further send with `EPIPE` rather than queueing behind it
            // (`inet_drv.c:12695`): the close path runs, so this send answers `closed` —
            // consuming the send direction's one-shot reply — the output that can never be
            // delivered is dropped, the socket is de-registered and every later send answers
            // `enotconn` (`tcp_send_or_shutdown_error`, `:13654`-`:13670`).
            self.latch_peer_close(ctx)?;
            self.peer_close.send_reported = true;
            return Ok(ctl_xerror("closed"));
        }
        // `{packet, 1|2|4}` frames the payload with a length header before the bytes reach the
        // queue; every other code (raw, line, ASN.1/…, the HTTP family) takes the C's `default:`
        // arm and writes no header at all.
        //
        // This named verb carries **no** caller tag, so the command it queues must never be
        // examined for one: without this, a partially-written named send's remainder could later
        // be read as a `prim_inet` prefix by `ready_output` and stripped from the middle of the
        // data stream. `write_pending` clears the flag again once the queue drains, so the next
        // command (the `erlang:port_command/2` one, [`TcpInetDriver::write_command`]) is examined
        // afresh.
        self.caller_stripped = true;
        self.caller_ref = None;
        let mut header_len = 0;
        if let Some(header) = frame::write_header(&self.options, arg.len()) {
            header_len = header.len();
            ctx.output().push(&header);
        }
        ctx.output().push(arg);
        // `inet_output_count(desc, len + h_len)` at `inet_drv.c:13763` (`tcp_send`; the
        // `tcp_sendv` counterpart is `:13879`): the payload **plus** the framing header,
        // counted once per command at issue time — so a partial write (or a write that then
        // discovers the peer gone) still counted it, exactly as the C does before it sends.
        self.stat.output(arg.len() + header_len);
        let _written = self.write_pending(ctx)?;
        if self.peer_close.seen {
            // This write is what found the peer gone, so it answers `closed` itself; the C
            // does the same (`inet_reply_error_am(…, am_closed)`, `inet_drv.c:13672`) and
            // arms the next receive for `closed` as well.
            self.peer_close.send_reported = true;
            return Ok(ctl_xerror("closed"));
        }
        Ok(ctl_ok())
    }

    /// The `shutdown` verb: shuts one or both directions of the connection down.
    ///
    /// A `SHUT_WR`/`SHUT_RDWR` with output still queued is **deferred** until the queue drains
    /// (the C's `TCP_ADDF_PENDING_SHUT_WR`/`TCP_ADDF_PENDING_SHUT_RDWR`), so no queued byte is
    /// silently dropped.
    ///
    /// # Errors
    ///
    /// `BeamError::BadArg` for an argument that is not a single `how` byte.
    fn verb_shutdown(&mut self, ctx: &mut DriverContext<'_>, arg: &[u8]) -> Result<Vec<u8>> {
        if !self.state.is_connected() {
            return Ok(ctl_xerror("enotconn"));
        }
        let [how] = arg else {
            return Err(error::badarg());
        };
        let fd = self.connected_socket()?;
        if *how != TCP_SHUT_RD && !ctx.output().is_empty() {
            if *how == TCP_SHUT_WR || *how == TCP_SHUT_RDWR {
                self.pending_shutdown = Some(*how);
                return Ok(ctl_ok());
            }
        }
        match shutdown_socket(fd, *how) {
            Ok(()) => Ok(ctl_ok()),
            Err(err) => Ok(ctl_error(err)),
        }
    }

    /// The `close` verb: de-registers and closes the socket, leaving the port open but idle.
    ///
    /// [`crate::PortControl::CLOSE`] reserves this name at the [`crate::PortRegistry`] level,
    /// so this verb is reached only by a caller driving the port's driver directly.
    ///
    /// # Errors
    ///
    /// `BeamError::Io` when the poller rejects the de-registration.
    fn verb_close(&mut self, ctx: &mut DriverContext<'_>) -> Result<Vec<u8>> {
        // Every queued operation is answered `{error, closed}` first (the C's
        // `tcp_close_check` → `async_error_am_all(am_closed)`, `inet_drv.c:11887`) and only then
        // does the descriptor go away.
        drain_closed(&mut self.async_ops, ctx)?;
        self.release_socket(ctx)?;
        Ok(ctl_ok())
    }

    /// The `setopts` verb: applies the C's `INET_REQ_SETOPTS` option records.
    ///
    /// The payload is a sequence of five-byte `[opt, val_be32]` records (the C's
    /// `inet_set_opts`), plus the trailing 16-bit counter of an `{active, N}` record; a partial
    /// trailing record is ignored. See [`self::opt`] for the served codes and the mirrored
    /// `propagate = 0` semantics: the reply is the C's `[1]` unless a code is unserved, which is
    /// `[0] ++ "einval"`.
    ///
    /// Like the C (`inet_drv.c:11111`, which has no `IS_OPEN` guard), a port that has no socket
    /// yet still validates the codes, stores the driver-state options and answers `[1]` — a
    /// `setopts` before `open` is not an error, exactly as the oracle behaves (only the gates the
    /// C reads off the cached `inet.stype` then keep their values out: the two `send_timeout`
    /// codes because there is no stream socket yet, and `{read_packets, N}` because there is no
    /// datagram one — either still answering `[1]`).
    ///
    /// # Errors
    ///
    /// Never for a served option (a rejected `setsockopt(2)` is not propagated, as in the C);
    /// `BeamError::BadArg` for a truncated `{active, N}` record.
    fn verb_setopts(&mut self, arg: &[u8]) -> Result<Vec<u8>> {
        let scope = OptScope::new(self.stype, self.family, libc::IPPROTO_TCP);
        set_options(&mut self.options, scope, self.socket, arg)
    }

    /// The `getopts` verb: reads the C's `INET_REQ_GETOPTS` options back.
    ///
    /// The payload is a sequence of one-byte option codes; the reply is the C's `[1]` followed
    /// by an `[opt, val_be32]` record per code, in request order (plus the trailing 16-bit
    /// counter of an `{active, N}` read-back). A code this floor does not serve is
    /// `[0] ++ "einval"` — the C's `default: RETURN_ERROR()`, which replaces the **whole**
    /// reply — which is where the set-only `line_delim` (`40`) and any genuinely unserved code
    /// land: a clear "not available", never a fabricated value.
    ///
    /// # Errors
    ///
    /// `BeamError::Io` is **not** raised for a failed `getsockopt(2)` on a live socket: that
    /// record is dropped and the search continues, as in the C (see [`self::opt`]).
    fn verb_getopts(&mut self, arg: &[u8]) -> Result<Vec<u8>> {
        let scope = OptScope::new(self.stype, self.family, libc::IPPROTO_TCP);
        get_options(&self.options, scope, self.socket, arg)
    }

    /// Routes one decoded numeric control opcode (`inet_drv.c`'s `INET_REQ_*` numbers) to the verb
    /// or the shared-`inet_ctl` reply that serves it.
    ///
    /// This is the TCP half of the bridge documented in [`self::opcode`]: `tcp_inet_ctl`
    /// (`:11953`) owns the socket lifecycle and the stream-only requests, delegates the rest to
    /// `inet_ctl` (`:10988`), and any number **neither** switch has a case for reaches
    /// `inet_ctl`'s `default:` arm — `ctl_xerror(EXBADPORT)` ⇒ `[0]exbadport` (`:11551`). That is
    /// also what a wrapped command (an `Op` below the magic number) answers, so `1` is
    /// **not** `open`.
    ///
    /// # Errors
    ///
    /// A typed "not served" reason for an opcode the C's switches do serve but this floor does
    /// not implement (never a fabricated reply), plus the [`verb_open`](Self::verb_open) family's
    /// own argument faults (`BeamError::BadArg` for a malformed operand).
    fn control_opcode(
        &mut self,
        ctx: &mut DriverContext<'_>,
        op: Opcode,
        arg: &[u8],
    ) -> Result<Vec<u8>> {
        match op {
            // `INET_REQ_OPEN` (`:11961`): the length is checked **before** the family, type and
            // protocol bytes, and each of those answers its own refusal rather than an argument
            // fault — only the numeric route reports the C's `ctl_*error` replies, the named
            // route keeps its `BadArg`.
            Opcode::Open => {
                if arg.len() != 3 {
                    return Ok(ctl_xerror("einval"));
                }
                if arg[0] != INET_AF_INET {
                    return Ok(ctl_xerror("eafnosupport"));
                }
                if arg[1] != INET_TYPE_STREAM {
                    return Ok(ctl_xerror("einval"));
                }
                if arg[2] != INET_PROTO_DEFAULT && arg[2] != INET_PROTO_TCP {
                    return Ok(ctl_xerror("eprotonosupport"));
                }
                self.verb_open(ctx, arg)
            }
            Opcode::Close => self.verb_close(ctx),
            // `inet_ctl`'s `INET_REQ_BIND` (`:11409`): `len < 2` first, **then** the state.
            Opcode::Bind => {
                if arg.len() < 2 {
                    return Ok(ctl_xerror("einval"));
                }
                if self.state != InetState::Open {
                    return Ok(ctl_xerror("exbadport"));
                }
                self.verb_bind(arg)
            }
            Opcode::Setopts => self.verb_setopts(arg),
            Opcode::Getopts => self.verb_getopts(arg),
            // `INET_REQ_LISTEN` (`:12042`): the state guard comes first, the operand length last.
            Opcode::Listen => {
                if !self.has_socket() {
                    return Ok(ctl_xerror("exbadport"));
                }
                if arg.len() != 2 {
                    return Ok(ctl_xerror("einval"));
                }
                self.verb_listen(arg)
            }
            // `INET_REQ_CONNECT` (`:12064`): the socket-state guards come first
            // (`!IS_OPEN ⇒ exbadport`, `IS_CONNECTED ⇒ eisconn`, `IS_CONNECTING ⇒ einval`) and
            // only then `len < 6 ⇒ einval`. The leading four operand bytes are the async-connect
            // timeout; everything after them is the packed address, which the C hands straight to
            // `inet_set_faddress` (`:12084`-`:12089` — the `Port(2)` its comment names is the
            // address's own port field). The C also ports the request onto the async queue; this
            // floor answers connect inline (the one family that does), so the timeout bounds the
            // completion inside the call instead of arming an operation.
            Opcode::Connect => {
                match self.state {
                    InetState::Closed => return Ok(ctl_xerror("exbadport")),
                    InetState::Connected => return Ok(ctl_xerror("eisconn")),
                    InetState::Connecting => return Ok(ctl_xerror("einval")),
                    InetState::Open | InetState::Listening => {}
                }
                if arg.len() < 6 {
                    return Ok(ctl_xerror("einval"));
                }
                let timeout_ms = i32::from_be_bytes([arg[0], arg[1], arg[2], arg[3]]);
                self.verb_connect(ctx, &arg[4..], timeout_ms)
            }
            // `INET_REQ_ACCEPT` (`:12144`): one combined "not listening **or** wrong operand"
            // guard, so both faults answer `einval`. A well-formed operand joins the C's
            // `enq_multi_op` list and the reply is the asynchronous id —
            // `[1] ++ <<Id:16/big>>` (`:12209`), with the accepted connection delivered later as
            // `{inet_async, Port, Id, {ok, Descriptor}}`.
            Opcode::Accept => self.async_accept(arg),
            // `TCP_REQ_RECV` (`:12268`): the whole asynchronous-receive guard ladder — the
            // connection guard (including the C's one-shot `closed`/`econnreset` reply, modelled
            // by [`PeerClose`]), `active || len != 8 ⇒ einval`, the framing/`Length` rule, the
            // size cap and finally the single-slot `ealready` — and the `[1] ++ <<Id:16/big>>`
            // reply with the data delivered later as `{inet_async, Port, Id, {ok, Data}}`
            // ([`self::op_queue`]).
            Opcode::TcpRecv => self.async_recv(ctx, arg),
            // `TCP_REQ_UNRECV` (`:12326`): the connection guard, then `tcp_push_buffer`: the
            // operand's bytes go back to the front of the driver's input buffer and the C
            // answers `[1]`. The raw read path drains `pending_input` into the port queue before
            // touching the socket ([`TcpInetDriver::read_available`]), so pushed bytes are
            // delivered ahead of anything the socket holds — and a framed `recv` re-decodes them.
            Opcode::TcpUnrecv => {
                if !self.state.is_connected() {
                    return Ok(ctl_xerror("enotconn"));
                }
                self.pending_input.splice(0..0, arg.iter().copied());
                Ok(ctl_ok())
            }
            // `TCP_REQ_SHUTDOWN` (`:12339`): the connection guard first, then the one-byte
            // `TCP_SHUT_*` operand.
            Opcode::TcpShutdown => {
                if !self.state.is_connected() {
                    return Ok(ctl_xerror("enotconn"));
                }
                if arg.len() != 1 {
                    return Ok(ctl_xerror("einval"));
                }
                self.verb_shutdown(ctx, arg)
            }
            // `TCP_REQ_SENDFILE` (`:12380`): `required_len` is
            // `sizeof(desc->sendfile.dup_file_fd) + 2*sizeof(Uint64)` = 4 + 16 = 20 on every
            // platform this crate targets, and the length is checked **before** the connection.
            // The sendfile itself is not implemented, so a connected stream stays a typed gap.
            Opcode::TcpSendfile => {
                if arg.len() != 20 {
                    return Ok(ctl_xerror("einval"));
                }
                if !self.state.is_connected() {
                    return Ok(ctl_xerror("enotconn"));
                }
                Err(op.not_served())
            }
            // `INET_REQ_GETHOSTNAME` (`:11219`): `len != 0 ⇒ einval`, else the local
            // `gethostname(2)` payload — no resolver is involved, so the reply is exact.
            Opcode::Gethostname => {
                if !arg.is_empty() {
                    return Ok(ctl_xerror("einval"));
                }
                let mut reply = ctl_ok();
                reply.extend_from_slice(&sys::hostname()?);
                Ok(reply)
            }
            // `INET_REQ_FDOPEN` (`:12003`): the length/family/type refusals are exact, and so is
            // the pre-adoption `sock_name`/family check of a `bound` descriptor
            // ([`wire::fdopen_precheck`]). Once that ladder passes, the descriptor is adopted
            // **prebound** ([`TcpInetDriver::adopt_prebound`], the C's `inet_ctl_fdopen`,
            // `:5223`-`:5282`): the port uses it for its whole life but never closes it, because
            // it belongs to the caller. A port that already holds a socket is refused instead.
            Opcode::Fdopen => {
                if arg.len() != 6 && arg.len() != 10 {
                    return Ok(ctl_xerror("einval"));
                }
                if arg[0] != INET_AF_INET {
                    return Ok(ctl_xerror("eafnosupport"));
                }
                if arg[1] != INET_TYPE_STREAM {
                    return Ok(ctl_xerror("einval"));
                }
                match wire::fdopen_precheck(arg) {
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
                        // The `bound` word of the ten-byte form is already accounted for: a
                        // non-zero one made `fdopen_precheck` run `sock_name(2)` and the family
                        // comparison, so there is nothing left to check and nothing to skip.
                        self.adopt_prebound(ctx, fd, u16::from(INET_TYPE_STREAM))?;
                        Ok(ctl_ok())
                    }
                    Err(reply) => Ok(reply),
                }
            }
            // The name-service lookups (`:11493`, `:11525`): the shared `inet_ctl` arms, served
            // from the system services database through [`sys::service_port`]/
            // [`sys::service_name`]. The C's operand arithmetic — including the `n >= len` /
            // `n > len` asymmetry — lives in [`wire::getservbyname_reply`]/
            // [`wire::getservbyport_reply`], which the datagram driver shares.
            Opcode::Getservbyname => Ok(wire::getservbyname_reply(arg)),
            Opcode::Getservbyport => Ok(wire::getservbyport_reply(arg)),
            // `INET_REQ_SETNAME`/`SETPEER` (`:11386`, `:11301`): the empty operand clears the fake
            // address and answers `[1]`, and a well-formed one is stored so that a later
            // `NAME`/`PEER` reports it instead of asking the kernel (`:11370`, `:11284`). The
            // address grammar — including the `len < 2` and family refusals — is
            // [`self::wire::parse_fake_address`]'s.
            Opcode::Setname | Opcode::Setpeer => {
                let stored = match parse_fake_address(self.family, arg) {
                    FakeAddress::Clear => None,
                    FakeAddress::Store(address) => Some(address),
                    FakeAddress::Refuse(name) => return Ok(ctl_xerror(name)),
                };
                if op == Opcode::Setname {
                    self.name_addr = stored;
                } else {
                    self.peer_addr = stored;
                }
                Ok(ctl_ok())
            }
            // The interface family (`:11067`, `:11078`, `:11089`, `:11100`): four shared
            // `inet_ctl` arms, each opening with `!IS_OPEN ⇒ ctl_xerror(EXBADPORT)` and then
            // walking the host's interfaces through [`super::sys`] — `SIOCGIFCONF` for the name
            // list, the `SIOCGIF*` ioctls for one interface's records and `getifaddrs(3)` for the
            // whole list. [`wire::interface_reply`] builds all four.
            Opcode::Getiflist | Opcode::Ifget | Opcode::Ifset | Opcode::Getifaddrs => {
                Ok(wire::interface_reply(op, self.socket, arg))
            }
            // `INET_REQ_IGNOREFD` (`:11456`): `!IS_CONNECTED ⇒ enotconn`, then
            // `stype != SOCK_STREAM ⇒ einval`. A connected stream stays a typed gap: its reply
            // means "suppress read **and** write readiness", which this driver would not honour
            // (and the C then reads `*buf` with no length check at all).
            Opcode::Ignorefd => {
                if !self.state.is_connected() {
                    return Ok(ctl_xerror("enotconn"));
                }
                if self.stype != u16::from(INET_TYPE_STREAM) {
                    return Ok(ctl_xerror("einval"));
                }
                Err(op.not_served())
            }
            // `INET_REQ_GETLADDRS`/`GETPADDRS` (`:11324`, `:11236`): `len != 4 ⇒ einval`, then
            // `!IS_OPEN ⇒ exbadport`, and the non-SCTP fallback of each is byte-identical to the
            // `NAME`/`PEER` reply, so both are single-sourced through those helpers.
            Opcode::Getladdrs => {
                if arg.len() != 4 {
                    return Ok(ctl_xerror("einval"));
                }
                if !self.has_socket() {
                    return Ok(ctl_xerror("exbadport"));
                }
                wire::name_reply(self.socket)
            }
            Opcode::Getpaddrs => {
                if arg.len() != 4 {
                    return Ok(ctl_xerror("einval"));
                }
                if !self.has_socket() {
                    return Ok(ctl_xerror("exbadport"));
                }
                wire::peer_reply(self.socket, self.state.is_connected())
            }
            // The shared-`inet_ctl` replies both drivers build identically. `NAME`/`PEER`
            // short-circuit on a fake address an `INET_REQ_SETNAME`/`SETPEER` installed —
            // `PEER` after its connection guard, exactly as the C orders them (`:11270`-`:11290`,
            // `:11365`-`:11377`). `GETLADDRS`/`GETPADDRS` deliberately do **not**: their
            // non-SCTP fallback calls `sock_name`/`sock_peer` directly and never looks at
            // `name_ptr`/`peer_ptr` (`:11341`, `:11261`).
            Opcode::Peer => {
                if !self.state.is_connected() {
                    return Ok(ctl_xerror("enotconn"));
                }
                match self.peer_addr {
                    Some(address) => Ok(wire::packed_addr_reply(address)),
                    None => wire::peer_reply(self.socket, true),
                }
            }
            Opcode::Name => match self.name_addr {
                Some(address) => Ok(wire::packed_addr_reply(address)),
                None => wire::name_reply(self.socket),
            },
            Opcode::Gettype => Ok(wire::gettype_reply(self.family, self.stype)),
            Opcode::Getfd => Ok(wire::getfd_reply(self.socket)),
            // The descriptor state, including the C's `INET_F_ACC`/`INET_F_MULTI_CLIENT` bits
            // while accept operations are queued (`:1031`-`:1032`, `:11160`). A listener with one
            // accept pending is `9 | 0x20` = 41 (`[1,0,0,1,41]`, measured on the oracle) and with
            // two `9 | 0x120` = 297.
            Opcode::Getstatus => Ok(wire::getstatus_reply(
                self.state.state_code() | self.accept_state_bits(),
            )),
            Opcode::Getstat => wire::getstat_reply(arg, &self.stat, ctx.output().len()),
            // `INET_REQ_SUBSCRIBE` (`inet_subscribe`, `:10819`): the shared `inet_ctl` arm, so
            // both socket drivers build it through one helper. The reply is exact —
            // `[1] ++ (op ++ be32(outq byte count))*` — and an operand byte that is not
            // `INET_SUBS_EMPTY_OUT_Q` is the C's `return -1`, i.e. `badarg`. What this floor
            // cannot honour is the *subscription* itself (no driver→owner message channel, so no
            // `{empty_out_q, Port}` is ever sent); see [`wire::subscribe_reply`].
            Opcode::Subscribe => wire::subscribe_reply(ctx.output().len(), arg),            // `PACKET_REQ_RECV` is a case of `packet_inet_ctl` only, so on a TCP port it is a case
            // of neither `tcp_inet_ctl` nor `inet_ctl`: the C answers `[0]exbadport` (measured),
            // exactly like a wrapped or unassigned number.
            Opcode::PacketRecv | Opcode::Unknown => Ok(ctl_xerror("exbadport")),
        }
    }
}

impl Default for TcpInetDriver {
    fn default() -> TcpInetDriver {
        TcpInetDriver::new()
    }
}

impl Driver for TcpInetDriver {
    fn name(&self) -> &str {
        DRIVER_NAME
    }

    fn start(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        // A socket-less port (the `"tcp_inet"` factory) registers nothing here: its socket is
        // created by the `open` verb, exactly as the C's `tcp_inet_start` leaves the socket
        // to `INET_REQ_OPEN`.
        if let Some(fd) = self.socket {
            set_nonblocking(fd)?;
            self.token = Some(ctx.register(fd, Interest::READABLE)?);
            self.interest = Interest::READABLE;
        }
        Ok(())
    }

    fn output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        // The command's bytes carry the `prim_inet` caller tag: strip it, write the payload and
        // answer the caller through the completion channel ([`TcpInetDriver::write_command`]).
        self.write_command(ctx)
    }

    fn ready_input(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        // A listener serves its queued `accept` operations here; a connection completes its
        // pending `recv` — the C's `tcp_inet_input`/`tcp_inet_accept` dispatch
        // ([`TcpInetDriver::async_ready_input`]). Without a queued operation the bytes take the
        // passive path into the port input queue.
        self.async_ready_input(ctx)
    }

    fn ready_output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        if self.state == InetState::Connecting {
            self.finish_connect(ctx)?;
            return Ok(0);
        }
        self.write_command(ctx)
    }

    fn timeout(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        // The port's single `TIMEOUT` task, armed from [`Driver::next_timeout`]: every due
        // operation completes with `{error, timeout}`.
        self.async_timeout(ctx)
    }

    fn next_timeout(&self) -> Option<Instant> {
        self.async_ops.next_deadline()
    }

    fn has_pending_async(&self) -> bool {
        self.queued_ops().has_pending()
    }

    fn drive_async(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        self.drive_async_ops(ctx)
    }

    fn control(
        &mut self,
        ctx: &mut DriverContext<'_>,
        command: &str,
        arg: &[u8],
    ) -> Result<Vec<u8>> {
        // The Erlang side speaks the C's numeric `INET_REQ_*` opcodes
        // (`erlang:port_control/3`), so a decimal command is decoded and routed to the same verbs
        // the named forms reach — before the name match, which a numeric command must never fall
        // into. A non-decimal command is `None` here and takes the named path unchanged.
        if let Some(op) = decode_opcode(command) {
            return self.control_opcode(ctx, op, arg);
        }
        match command {
            "open" => self.verb_open(ctx, arg),
            "bind" => self.verb_bind(arg),
            "listen" => self.verb_listen(arg),
            "accept" => self.verb_accept(ctx),
            // The named form has no `Timeout(4)` operand (it is this floor's own convenience verb,
            // not the C request), so it asks for the C's `INET_INFINITY` — i.e. the capped wait.
            "connect" => self.verb_connect(ctx, arg, INET_INFINITY),
            "send" => self.verb_send(ctx, arg),
            "recv" => self.verb_recv(ctx, arg),
            "shutdown" => self.verb_shutdown(ctx, arg),
            "close" => self.verb_close(ctx),
            "setopts" => self.verb_setopts(arg),
            "getopts" => self.verb_getopts(arg),
            _ => Err(error::badarg()),
        }
    }

    fn stop(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        // Every queued operation is answered with `{error, closed}` **before** the descriptor goes
        // away — the C's `async_error_am_all(am_closed)` (`inet_drv.c:2597`-`:2608`) — and the
        // release outcome is kept: neither half is dropped on the other's failure.
        let drained = drain_closed(&mut self.async_ops, ctx);
        let released = self.release_socket(ctx);
        drained?;
        released
    }
}

impl Drop for TcpInetDriver {
    fn drop(&mut self) {
        // `stop` already released the socket on the normal teardown path; this covers a driver
        // dropped without ever being stopped (a `start` that failed inside
        // `PortRegistry::open`, for instance). A `Drop` cannot report anything, so the
        // descriptor is simply released — the same rule `FdDriver` follows.
        self.close_socket();
    }
}

/// The [`crate::DriverFactory`] of the built-in `"tcp_inet"` driver.
///
/// A plain `fn` (not a closure) because [`crate::DriverFactory`] is a function pointer: it
/// captures nothing and builds a fresh socket-less [`TcpInetDriver`] each call.
pub fn make_tcp_inet() -> Result<Box<dyn Driver>> {
    Ok(Box::new(TcpInetDriver::new()))
}

#[cfg(test)]
mod frame_tests;
#[cfg(test)]
mod opcode_tests;
#[cfg(test)]
mod opcode_tests_refusals;
#[cfg(test)]
mod opcode_tests_states;
#[cfg(test)]
mod opcode_tests_udp;
#[cfg(test)]
mod stat_tests;
#[cfg(test)]
mod tests;