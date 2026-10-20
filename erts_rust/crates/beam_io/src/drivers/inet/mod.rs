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
//! | `shutdown` | `TCP_REQ_SHUTDOWN` (`:12339`) | `[how]` | `[1]`, or `[0]enotconn` / `[0]einval` |
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
//! reports it as `BadArg` — **except** where the C's own arm answers a `ctl_error` for the same
//! operand fault, which is then reproduced as the reply: `TCP_REQ_SHUTDOWN`'s `len != 1` is
//! `ctl_error(EINVAL)` (`:12352`), so a wrong-length shutdown operand is `[0]einval`, and after
//! its connection guard (`:12349`).
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
//! implemented for **both** numeric requests ([`self::op_queue`], with the accept list itself in
//! [`self::accept`]):
//!
//! * `TCP_REQ_RECV`(42) takes the C's `Timeout(4), Length(4)` operand and occupies the
//!   **single** receive slot (`INET_MAX_ASYNC = 1`, `:1069`), so a second concurrent receive is
//!   the C's `[0]ealready`. The data is delivered as
//!   [`InetAsyncResult::Data`](crate::completion::InetAsyncResult::Data) — one
//!   [`ReadMessage`](crate::port_task::ReadMessage) per read, the `{packet, N}` header already
//!   stripped.
//! * `INET_REQ_ACCEPT`(26) takes the four-byte `Timeout` and joins the **multi-operation** accept
//!   list (`enq_multi_op`, `:12194`/`:12209`), so N pending accepts each get their own id, all
//!   complete in enqueue order, and `INET_REQ_GETSTATUS` reports the C's `INET_F_ACC` bit while an
//!   accept is queued plus the **sticky** `INET_F_MULTI_CLIENT` bit from the second enqueue until
//!   the list fully drains ([`self::accept`]).
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

use crate::completion::InetReason;
use crate::driver::{Driver, DriverContext, DriverFlags};
use crate::drivers::fd::set_nonblocking;
use crate::error;
use crate::poller::{Interest, Token};
use crate::port_task::{ActiveInputEvent, ActiveMode, ActiveSpend};

// The TCP accept state machine (`INET_REQ_ACCEPT`'s multi-operation list, its per-port timer
// entries, the sticky `INET_F_MULTI_CLIENT` bit and the listener's readiness arming), split out of
// the shared async-op queue it plugs into. Its own module doc is `src/drivers/inet/accept.rs`.
mod accept;
mod adopt_probe;
mod frame;
// The `{header, N}` split of one delivered payload (the C's `desc->hsz`), shared by the active
// and the passive delivery paths. Its own module doc is `src/drivers/inet/header.rs`.
mod header;
mod opcode;
mod opt;
// The asynchronous-operation queue (`TCP_REQ_RECV`/`INET_REQ_ACCEPT` and the
// `{inet_async, Port, Id, Result}` completions). The file cannot be named `async.rs` *as a
// module* (`async` is a keyword), so it is declared through a `#[path]` alias.
#[path = "async.rs"]
mod op_queue;
pub mod packet;
// The C's passive peer-close teardown (`tcp_desc_close`, `inet_drv.c:11932`): the half of a
// latched close that closes the descriptor, kept out of this (already grown) source half. Its
// own module doc is `src/drivers/inet/peer_close.rs`.
mod peer_close;
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
// The errno an `INET_REQ_FDOPEN`(13) adoption failure carries, and the C reply it becomes
// (`adoption_reply`): the two drivers' `Fdopen` arms answer it instead of raising.
use self::adopt_probe::adoption_reply;

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

// The asynchronous-operation queue of this descriptor — the C's `enq_async` slot (with the
// `enq_multi_op` accept list its sibling `accept` module owns), the `NEW_ASYNC_ID` counter and the
// completion production points (`ready_input`/`timeout`/`drive_async`). Its own module doc is
// `src/drivers/inet/async.rs`.
use self::op_queue::{drain_closed, drain_error_all, AsyncQueue};
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
    /// No socket (the initial state, and the state the `close` verb leaves behind).
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

/// The peer-close bookkeeping the C keeps in three independent bits of `desc->tcp_add_flags`:
/// `TCP_ADDF_DELAYED_CLOSE_RECV` (`inet_drv.c:855`), `TCP_ADDF_DELAYED_CLOSE_SEND` (`:856`) and
/// `TCP_ADDF_DELAYED_ECONNRESET` (`:862`).
///
/// Once the peer's close (a clean end-of-file, or a connection **reset**) has been observed the
/// socket is unusable, but the C does not report `enotconn` at once: the observation arms one-shot
/// flags for the **next** `recv` and/or the **next** `send`, and what each of them answers depends
/// on *which* flags the observation armed. The three bits are set by three different call sites:
///
/// * `DELAYED_CLOSE_SEND` — armed by a **write** that found the peer gone
///   (`tcp_send_or_shutdown_error`, `:13662`) *and* by a **clean end-of-file** on the read side
///   (`tcp_recv_closed`, `:12816`: "if next op is a send, make sure it returns `{error,closed}`
///   rather than `{error,enotconn}`").
/// * `DELAYED_CLOSE_RECV` — armed by that write path (`:13678`): "make sure that the next
///   receive operation gets an `{error,closed}` result rather than `{error,enotconn}`, so the
///   caller can safely ignore errors in the send operations and handle them in the receive
///   operation". A read that fails with `ECONNRESET` under `{show_econnreset,true}`
///   (`tcp_recv_error`, `:12849`) arms **nothing at all** — not even the reset bit.
///
///   It is also armed — a documented substitution, not a C flag — by a read-side discovery the
///   **reactor** makes with no receive queued ([`ReadReport::OwedToNextRecv`]). In the C that
///   situation cannot arise in passive mode: read interest is armed only by a queued
///   `TCP_REQ_RECV` (`sock_select(FD_READ|FD_CLOSE, 1)`, `:12315`), so the receive that armed it
///   is always the one the end-of-file is answered to — through `async_error_am_all(am_closed)`
///   (`tcp_recv_closed`, `:12829`) for an end-of-file, or `async_error_am_all(error_atom(err))`
///   (`tcp_recv_error`, `:12877`) for a reset. This floor's poller is **edge-triggered** and keeps
///   the socket's `READABLE` registration for its whole life, so `ready_input` routinely reads the
///   end-of-file first, with nobody to answer. The reply that C discovery would have given the
///   pending receive then belongs to the user's **next** `recv`, so it is armed here instead of
///   being lost — keeping the *user-visible* sequence `closed` (then `enotconn`) whichever party
///   made the observation.
/// * `DELAYED_ECONNRESET` — armed only when that write-path observation was an `ECONNRESET`
///   **and** `{show_econnreset,true}` was in effect at that moment (`:13615`, `:13683`-`:13685`).
///   It turns the `closed` of either ladder into `econnreset`. (The reactor substitution above
///   arms it too, for the `econnreset` the deferred reply owes — the C reaches that answer through
///   `tcp_recv_error`'s `error_atom(err)` rather than through this bit, `:12877`.)
///
/// The two reply ladders are **asymmetric** and reproduce the C's exactly:
///
/// * the receive ladder (`TCP_REQ_RECV`, `:12279`-`:12288`) answers `closed`/`econnreset` only
///   while `DELAYED_CLOSE_RECV` is set, and answering clears **both** one-shots (and the reset
///   bit);
/// * the send ladder (`tcp_inet_commandv`, `:12681`-`:12692`) answers `closed`/`econnreset` while
///   `DELAYED_CLOSE_SEND` is set and clears **only** that one — the reset bit is deliberately left
///   set "for the next receive operation" (`:12685`'s comment);
/// * every other state answers `enotconn`.
///
/// `observed` is not a C flag either: it is the reason the **discovering** operation reports —
/// `None` meaning `closed` — so a read reset under `{show_econnreset,true}` answers `econnreset`
/// and any other read failure answers its own `error_atom(err)`, the split the C makes between
/// `tcp_recv_error` and `tcp_recv_closed` (`:13190`-`:13196`) when it answers the discovering
/// operation inline ([`PeerClose::observed_reason`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct PeerClose {
    /// The peer's end-of-file (or a connection reset) has been observed.
    seen: bool,
    /// The C's `TCP_ADDF_DELAYED_CLOSE_RECV`.
    delayed_close_recv: bool,
    /// The C's `TCP_ADDF_DELAYED_CLOSE_SEND`.
    delayed_close_send: bool,
    /// The C's `TCP_ADDF_DELAYED_ECONNRESET`, generalised into the reason the deferred reply
    /// owes: `None` is the `closed` both ladders answer by default, `Some(reason)` is the
    /// `econnreset` of a write-path reset under `{show_econnreset,true}` **and** the
    /// `error_atom(err)` a reactor-discovered **read error** owes
    /// (`tcp_recv_error`'s `async_error_am_all(error_atom(err))`, `inet_drv.c:12877`).
    delayed_reason: Option<InetReason>,
    /// The reason the **discovering** operation reports, `None` being `closed`: `econnreset` for
    /// a reset under `{show_econnreset,true}`, the failing errno for a read error.
    observed: Option<InetReason>,
}

/// How the peer's close was observed — which decides both the flags
/// [`PeerClose::observe`] arms and the reason a pending asynchronous operation is completed with.
///
/// The three shapes are the C's three discovery sites, and they are **not** interchangeable: each
/// arms a different subset, and only a read-side discovery can report a reset *inline*.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CloseSource {
    /// A **read** discovered it: `tcp_recv`'s `n == 0` or a reset-under-`{show_econnreset,false}`
    /// (`tcp_recv_closed`, `inet_drv.c:12783`), or `tcp_recv_error`'s `ECONNRESET`-under-show /
    /// any-other-errno route (`:12849`).
    Read {
        /// The errno a **read failure** carried — `tcp_recv_error`'s route — or `None` for the
        /// clean end-of-file that `tcp_recv_closed` handles. `tcp_recv` (`:13185`-`:13210`) sends
        /// `ECONNRESET` to `tcp_recv_error` **only** under `{show_econnreset,true}`, so a reset
        /// without the option is `None` here and still `tcp_recv_closed`'s clean close.
        error: Option<i32>,
        /// Who the discovery's `closed`/`econnreset` reply belongs to (see [`ReadReport`]).
        report: ReadReport,
    },
    /// A **write** — or a `shutdown` refused because the connection was already gone — discovered
    /// it (`tcp_send_or_shutdown_error`, `inet_drv.c:13613`).
    Write {
        /// `err == ECONNRESET && (tcp_add_flags & TCP_ADDF_SHOW_ECONNRESET)` (`:13615`).
        reset: bool,
    },
}

/// Who a **read**-side discovery's `closed`/`econnreset` reply belongs to.
///
/// The C never has to ask: in passive mode the read interest is armed **only** by a queued
/// `TCP_REQ_RECV` (`sock_select(FD_READ|FD_CLOSE, 1)`, `inet_drv.c:12315`), so the party that can
/// read the end-of-file is always the receive it answers. This floor's poller is
/// **edge-triggered** and keeps the socket's `READABLE` registration for its whole life (see
/// `src/drivers/CONTEXT.md`), so the reactor may be the discoverer with no receive in sight — and
/// then the answer the C gives that receive is still owed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ReadReport {
    /// The party that made the discovery also answers it: a queued `TCP_REQ_RECV`
    /// (`async_error_am_all(am_closed)`, `inet_drv.c:12829`; `async_error_am_all(error_atom(err))`,
    /// `:12877`) or this floor's synchronous `recv` verb (`tcp_recv_closed`/`tcp_recv_error`'s own
    /// return value, `:13185`-`:13196`). An active-mode delivery is the same shape: the close is
    /// reported through `{tcp_closed, S}`/`{tcp_error, S, Reason}` (`:12841`/`:12871`) instead of a
    /// receive reply.
    Answered,
    /// Nobody was there to answer it: the **reactor** read the end-of-file (or the reset) with no
    /// operation queued, so the reply is armed as `TCP_ADDF_DELAYED_CLOSE_RECV` for the user's
    /// **next** `recv` instead of being lost.
    OwedToNextRecv,
}

impl CloseSource {
    /// The reason the **discovering** operation reports, `None` being `closed`: the C's
    /// `tcp_recv_error` vs `tcp_recv_closed` split (`inet_drv.c:13190`-`:13196`) is exactly
    /// `error_atom(err)` vs `am_closed`, and a write-path reset (`TCP_ADDF_DELAYED_ECONNRESET`,
    /// `:13615`) is its `econnreset`. It is the input to both the inline reply and the
    /// `TCP_ADDF_DELAYED_*` latch.
    fn reason(self) -> Option<InetReason> {
        match self {
            CloseSource::Read { error, .. } => error.map(InetReason::from_errno),
            CloseSource::Write { reset } => reset.then(|| InetReason::from_errno(libc::ECONNRESET)),
        }
    }
}

impl PeerClose {
    /// Latches the observation and arms exactly the one-shot flags that discovery path arms.
    fn observe(&mut self, source: CloseSource) {
        self.seen = true;
        self.observed = source.reason();
        match source {
            // `tcp_recv_error` (`inet_drv.c:12849`-`:12890`) sets **no** flag: the error is
            // delivered on the spot — `async_error_am_all(error_atom(err))` (`:12877`) to a
            // passive caller, `tcp_error_message`/`tcp_closed_message` (`:12880`-`:12881`) to an
            // active owner — and every later operation answers `enotconn`.
            //
            // `tcp_recv_closed` (`:12810`-`:12819`) arms the *send* one-shot only — a clean
            // end-of-file owes the receive nothing beyond the reply it already gave.
            //
            // Both hold for `ReadReport::Answered`, where the discovering operation takes that
            // reply. A reactor-made discovery has no such operation, so the reply it owes is armed
            // on the receive ladder — carrying `error_atom(err)` for a read failure and the plain
            // `closed` for a clean end-of-file, because `tcp_recv_error` names the reason in its
            // `async_error_am_all` argument while `tcp_recv_closed` names `am_closed`.
            CloseSource::Read { error, report } => {
                self.delayed_reason = error.map(InetReason::from_errno);
                if error.is_none() {
                    self.delayed_close_send = true;
                }
                if report == ReadReport::OwedToNextRecv {
                    self.delayed_close_recv = true;
                }
            }
            CloseSource::Write { reset } => {
                // `tcp_send_or_shutdown_error` (`:13660`-`:13685`): the send one-shot, then the
                // receive one-shot, then — only for a reset under `{show_econnreset,true}` — the
                // reason that turns both of their `closed` into `econnreset`.
                self.delayed_close_send = true;
                self.delayed_close_recv = true;
                if reset {
                    self.delayed_reason = Some(InetReason::from_errno(libc::ECONNRESET));
                }
            }
        }
    }

    /// The reply the next `recv` owes, or `None` while the connection is still usable.
    ///
    /// The C's `TCP_REQ_RECV` ladder (`inet_drv.c:12279`-`:12288`): with
    /// `TCP_ADDF_DELAYED_CLOSE_RECV` set it clears **both** one-shots and answers `econnreset` /
    /// the deferred `error_atom(err)` (also clearing `TCP_ADDF_DELAYED_ECONNRESET`) or `closed`;
    /// in every other state the reply is `enotconn`.
    fn recv_reply(&mut self) -> Option<InetReason> {
        if !self.seen {
            return None;
        }
        if !self.delayed_close_recv {
            return Some(InetReason::from_errno(libc::ENOTCONN));
        }
        self.delayed_close_recv = false;
        self.delayed_close_send = false;
        Some(self.delayed_reason.take().unwrap_or(InetReason::Closed))
    }

    /// The reply the next `send` owes, or `None` while the connection is still usable.
    ///
    /// The C's `tcp_inet_commandv` ladder (`inet_drv.c:12681`-`:12692`): with
    /// `TCP_ADDF_DELAYED_CLOSE_SEND` set it clears **only** that one and answers `econnreset` — the
    /// reset bit staying armed for the next receive, as that arm's comment insists — or `closed`;
    /// in every other state the reply is `enotconn`.
    fn send_reply(&mut self) -> Option<InetReason> {
        if !self.seen {
            return None;
        }
        if !self.delayed_close_send {
            return Some(InetReason::from_errno(libc::ENOTCONN));
        }
        self.delayed_close_send = false;
        Some(self.delayed_reason.unwrap_or(InetReason::Closed))
    }

    /// The reason the operation that **discovered** the close reports: `econnreset` for a reset
    /// under `{show_econnreset,true}`, the failing errno for a read error, else `closed` (the C's
    /// `tcp_recv_error` vs `tcp_recv_closed` split, `inet_drv.c:13190`-`:13196`).
    fn observed_reason(&self) -> InetReason {
        self.observed.unwrap_or(InetReason::Closed)
    }

    /// Consumes the send one-shot that an **inline** reply has just answered.
    ///
    /// The C's `tcp_send_or_shutdown_error` arms `TCP_ADDF_DELAYED_CLOSE_SEND` only when there is
    /// no operation to reply to (`is_not_internal_pid(desc->inet.caller)`, `:13662`); for the
    /// operation it answers on the spot (`:13664`-`:13669`) the one-shot is not armed at all. This
    /// floor's synchronous `send` verb has no caller tag, so it answers inline — and must therefore
    /// consume the one-shot it just armed, or the next send would answer `closed` a second time.
    /// `TCP_ADDF_DELAYED_ECONNRESET` is deliberately left set: it belongs to the next receive.
    fn consumed_by_send(&mut self) {
        self.delayed_close_send = false;
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
    /// Whether the write half was shut in a way that must not be reported as the peer's reset — the
    /// C's `TCP_ADDF_SHUTDOWN_WR_DONE` (`inet_drv.c:863`). Set in exactly the two places the C sets
    /// it: a **failed** write-side `shutdown(2)` (`:12372`, before the error reply) and the
    /// **deferred** drain `tcp_shutdown_async` (`:13973`, on success only) — never by a direct
    /// successful `shutdown` (the C's `else return ctl_reply_ok(…)` at `:12377`-`:12378` latches
    /// nothing). It keeps a later `EPIPE` an `EPIPE` instead of rewriting it to `ECONNRESET`
    /// (`tcp_send_error`, `:13717`), which is the flag
    /// `tcp_send_or_shutdown_error`'s `err == ECONNRESET && TCP_ADDF_SHOW_ECONNRESET` reads
    /// (`:13615`): the deferred drain therefore answers `closed` unconditionally, while a direct
    /// successful write shutdown answers `econnreset` under `{show_econnreset,true}`.
    shutdown_wr_done: bool,
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
    /// A close the socket reported that has **not** been latched yet.
    ///
    /// [`TcpInetDriver::read_bytes`] never latches: it reports the observation and leaves the
    /// decision to its caller. A queued receive (`try_complete_recv`) that read data **and** the
    /// peer's end of file in one drain must hand the data over first — the C's `tcp_recv`
    /// answers a complete unit before it ever reaches the `n == 0` arm (`inet_drv.c:13216`,
    /// `:13206`), so `{error, closed}` is owed to the *next* receive. Every other caller latches
    /// straight away, which is the C's ordering for a discovery no operation is parked on.
    deferred_close: Option<CloseSource>,
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
    /// plus its `enq_multi_op` accept list (the latter in `src/drivers/inet/accept.rs`), and the
    /// `NEW_ASYNC_ID` counter behind both ([`AsyncQueue`], `src/drivers/inet/async.rs`).
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
    /// Whether the current command's **write-side framing** has already been decided — the
    /// `{packet, 1|2|4}` length header is on the output queue in front of the payload (or the
    /// command's `{packet, …}` code writes no header at all).
    ///
    /// The C decides this once per command, inside `tcp_sendv`/`tcp_send`
    /// (`inet_drv.c:13744`-`:13769`, `:13861`-`:13885`), from the `desc->inet.htype` in force at
    /// issue time. It must not be decided again for the retained remainder of a short write: a
    /// second header would announce the remainder's length and the peer would read a corrupted
    /// stream. Like [`TcpInetDriver::caller_stripped`] it is set on the **first** examination of a
    /// command and cleared where that flag is — when the output queue drains
    /// ([`TcpInetDriver::write_pending`]) — so the two per-command latches cannot drift.
    out_framed: bool,
    /// Whether read readiness may move bytes out of the kernel on the **reactor's** behalf — the
    /// C's `FD_READ` **selection** state.
    ///
    /// `tcp_deliver` deselects the socket the moment a delivery spends its `{active, …}` grant
    /// (`if (! desc->inet.active) { … sock_select(INETP(desc), (FD_READ|FD_CLOSE), 0); … }`,
    /// `inet_drv.c:13106`-`:13114`), so in the C a peer write during that window produces no read
    /// callback at all and waits in the socket; `inet_set_opts` re-selects the socket when an
    /// active grant is armed (`sock_select(desc, (FD_READ|FD_CLOSE), (desc->active>0))`, `:7982`)
    /// and a queued `TCP_REQ_RECV` selects it directly (`:12315`).
    ///
    /// This floor keeps the poller registration armed for the socket's whole life (`Interest` has
    /// no empty mask and `mio`'s epoll is edge-triggered), so that gate has to live here: with
    /// this flag `false`, [`TcpInetDriver::async_ready_input`] returns **without** reading, which
    /// is what leaves the peer's bytes in the kernel for the next arm instead of spilling them
    /// onto the passive [`PortNotification::Data`](crate::PortNotification::Data) channel and
    /// losing them from the next `{active, …}` delivery.
    ///
    /// It is `true` by construction and on every fresh connection ([`TcpInetDriver::adopt`],
    /// [`TcpInetDriver::adopt_prebound`], [`TcpInetDriver::mark_connected`],
    /// [`TcpInetDriver::close_socket`]): a port that was never armed keeps this floor's eager
    /// passive read, which is the one deliberate divergence from a C socket that is deselected
    /// from the start. It gates the **reactor's** read only — every explicit/queued read
    /// (the `recv` verbs, `TCP_REQ_RECV`) is unaffected.
    read_armed: bool,
    /// The `{active, …}` grants a **delivery** has spent since the last drain, oldest first — the
    /// notification half of the seam whose read half is [`Driver::active_mode`]
    /// ([`ActiveSpend`], drained by [`Driver::take_active_spends`]).
    ///
    /// Recorded in exactly one place, the per-delivery spend site
    /// ([`TcpInetDriver::count_delivery`], the C's `INET_CHECK_ACTIVE_TO_PASSIVE`,
    /// `inet_drv.c:1552`-`:1561`): an `{active, once}` grant spent silently gets one
    /// [`ActiveSpend::Once`], an `{active, N}` counter reaching zero one [`ActiveSpend::Multi`].
    /// The `setopts` option walk that drives a counter to zero (`landed_at_zero`, the C's
    /// `:6993`-`:6998`) delivers nothing, so it records nothing — the C spends a grant only per
    /// delivered packet.
    ///
    /// **Invariant:** a burst spends at most one grant, because the delivery loop ends the moment
    /// the mode flips passive (`if (! desc->inet.active)`, `:13108`-`:13114`), so this holds zero
    /// or one entry per round in practice; it is a `Vec` rather than an `Option` so the drain has
    /// one shape whatever a later tier does. `{active, true}` and `{active, false}` never record.
    active_spends: Vec<ActiveSpend>,
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
            shutdown_wr_done: false,
            pending_shutdown: None,
            connect_error: None,
            options: SocketOptions::default(),
            pending_input: Vec::new(),
            deferred_close: None,
            family: 0,
            stype: 0,
            name_addr: None,
            peer_addr: None,
            stat: stat::InetStat::default(),
            prebound: false,
            async_ops: AsyncQueue::new(),
            caller_ref: None,
            caller_stripped: false,
            out_framed: false,
            read_armed: true,
            active_spends: Vec::new(),
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
    ///
    /// The errno is the failing `getsockopt(2)`/`getpeername(2)` one and is **preserved** (never
    /// rewritten into [`beam_common::BeamError::BadArg`]): unlike [`TcpInetDriver::adopt_prebound`],
    /// this entry point has **no** control reply channel to answer through — the runtime calls it
    /// for an `inet_async` `Accepted { fd }` hand-off ([`crate::InetAsyncResult::Accepted`]), whose
    /// failure is the caller's to report — so the typed I/O error is the honest outcome.
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
            shutdown_wr_done: false,
            pending_shutdown: None,
            connect_error: None,
            options: SocketOptions::default(),
            pending_input: Vec::new(),
            deferred_close: None,
            family: u16::from(INET_AF_INET),
            stype: sys::socket_type(fd)?,
            name_addr: None,
            peer_addr: None,
            stat: stat::InetStat::default(),
            prebound: false,
            async_ops: AsyncQueue::new(),
            caller_ref: None,
            caller_stripped: false,
            out_framed: false,
            // A freshly adopted socket starts with its read side selected (see the field's note).
            read_armed: true,
            // Nothing has been delivered on a brand-new socket yet, so no grant is spent.
            active_spends: Vec::new(),
        })
    }

    /// Adopts the socket `listener` just accepted, **inheriting the listener's options** — the
    /// C's `tcp_inet_copy` (`inet_drv.c:11795`, copy list `:11814`-`:11831`, busy-msgq limits
    /// `:11842`/`:11844`).
    ///
    /// This is the API a runtime consumer calls instead of [`TcpInetDriver::from_socket`] when it
    /// adopts an [`crate::InetAsyncResult::Accepted`] `{ fd }` hand-off
    /// (`{inet_async, Port, Id, {ok, NewPort}}`): the descriptor is adopted exactly as
    /// `from_socket` does, and the option store plus the two cached socket tags are then taken
    /// from `listener` through `SocketOptions::inherited_from` (the `opt` module) — `inet.mode`,
    /// `inet.exitf`,
    /// `inet.deliver`, `inet.htype`, `inet.psize`, `inet.hsz`, `inet.bufsz`, `high`, `low`,
    /// `send_timeout`, `send_timeout_close`, the `SHOW_ECONNRESET`/`NO_READ_AHEAD` flags and the
    /// parent's busy-msgq limits; `inet.stype`/`inet.sfamily` are copied verbatim (the C copies
    /// them too, overriding [`TcpInetDriver::from_socket`]'s `sys::socket_type` probe).
    ///
    /// Everything else is `from_socket`'s behaviour, unchanged: the lifecycle state is read from
    /// the kernel, so an accepted descriptor is [`InetState::Connected`] (its `getpeername(2)`
    /// succeeds), `pending_input` starts empty (a fresh connection cannot inherit a partial frame),
    /// and the async queue and statistics start fresh. The descriptor is **owned**: it is
    /// `close(2)`d on [`Driver::stop`]/`close`/`Drop`, unlike the **borrowed** `prebound` shape
    /// `TcpInetDriver::adopt_prebound` installs.
    ///
    /// Deliberately **not** inherited, each matching the C: `{active, …}` (and its `{active, N}`
    /// counter) start **passive** — `inet_start` resets them and the copy list omits them, and
    /// re-applying them is `prim_inet:accept_opts/3`'s half — as do `debug`, `netns`,
    /// `read_packets`, `line_delim` and the `{buffer, N}`-was-set flag; `{delay_send, …}` is
    /// outside the C's mask and is applied by that same Erlang half, not here.
    ///
    /// The `INET_REQ_FDOPEN` route gets **no** inheritance hook on purpose: `inet_ctl_fdopen`
    /// (`inet_drv.c:5223`-`:5282`) copies nothing but `prebound`/`stype`/`sfamily`
    /// (`:5269`-`:5274`), so `TcpInetDriver::adopt_prebound` stays the plain adoption it is —
    /// this constructor is the driver's only copy path.
    ///
    /// # Errors
    ///
    /// The same typed `BeamError::Io` [`TcpInetDriver::from_socket`] returns when `fd` is not a
    /// usable socket (the failing `getsockopt(2)`/`getpeername(2)` errno, preserved), and
    /// [`beam_common::BeamError::Io`] for a socket whose type cannot be read. Nothing is inherited
    /// on that path: a failed adoption produces no driver at all, so `fd` is still the caller's to
    /// close.
    ///
    /// Pinned by the pure unit suite in `inherit_tests.rs`.
    pub fn from_accepted(fd: RawFd, listener: &TcpInetDriver) -> Result<TcpInetDriver> {
        let mut accepted = TcpInetDriver::from_socket(fd)?;
        // The C's copy list over `prep_tcp_inet_start`'s defaults (`:11814`-`:11831`).
        accepted.options = accepted.options.inherited_from(&listener.options);
        // `inet.stype`/`inet.sfamily` (`:11820`/`:11821`) — copied verbatim, so the accepted port
        // reports the listener's cached tags in `INET_REQ_GETTYPE` rather than a fresh probe.
        accepted.stype = listener.stype;
        accepted.family = listener.family;
        Ok(accepted)
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
        self.shutdown_wr_done = false;
                self.connect_error = None;
                // A fresh socket starts a fresh byte stream: any partial frame buffered for
                // the previous socket no longer belongs to this one.
                self.pending_input.clear();
                self.deferred_close = None;
                // ... and a fresh read side: nothing has been delivered on the new socket yet, so
                // no `{active, …}` grant can be spent (see [`TcpInetDriver::read_armed`]).
                self.read_armed = true;
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
    ///
    /// Every one of those failures carries the failing syscall's **errno** (`fcntl(2)`'s `EBADF`
    /// for a number that is not open, `getpeername(2)`'s `ENOTSOCK` for a descriptor that is not a
    /// socket, the poller's own errno), which is what the `INET_REQ_FDOPEN` arm renders as the C's
    /// `ctl_error(sock_errno())` reply (`inet_ctl_fdopen`, `inet_drv.c:5253`/`:5260`) instead of
    /// raising — see [`adopt_probe::adoption_reply`]. The single failure with **no** errno is the
    /// poller's exhausted token space ([`beam_common::BeamError::SystemLimit`]); that one keeps the
    /// typed error, because the C has no errno to name there either.
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
        self.shutdown_wr_done = false;
        self.connect_error = None;
        // A fresh socket starts a fresh byte stream: any partial frame buffered for the previous
        // socket no longer belongs to this one.
        self.pending_input.clear();
        self.deferred_close = None;
        // The read side is selected again: the new socket has delivered nothing yet.
        self.read_armed = true;
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
        self.shutdown_wr_done = false;
        // The new connection's byte stream begins now: drop any partial frame buffered for the
        // socket this one replaces.
        self.pending_input.clear();
        self.deferred_close = None;
        // ... and nothing has been delivered on it yet, so its read side is selected again.
        self.read_armed = true;
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
    /// This is the `erl_inet_close` half of the C's `tcp_desc_close` (`inet_drv.c:5102`-`:5117`,
    /// reached from `:11932`-`:11939`), and it carries that function's **unconditional**
    /// `tcp_clear_input` (`:11706`) in front of it: the driver's own framing buffer (`desc->i_buf`
    /// → [`TcpInetDriver::pending_input`]) belongs to the descriptor's byte stream, so a **partial
    /// `{packet, N}` frame buffered before the close is dropped by it** — for a prebound
    /// descriptor too, because `tcp_desc_close` runs `tcp_clear_input` before the ownership test.
    /// The port's *input queue* is not touched: `tcp_clear_input` clears `desc->i_buf` only, never
    /// the `ioq` side the port owns.
    ///
    /// A **prebound** descriptor is then not this driver's to close: `erl_inet_close` only drops
    /// the select registration for one (`sock_select(…, 0)` + `event_mask = 0`), leaves `desc->s`
    /// and `desc->state` exactly as they were and issues no `close(2)` at all. The same rule holds
    /// here — nothing is closed, the peer/shutdown bookkeeping is left alone and the state is
    /// **kept**, so the descriptor number stays valid in the OS (owned by whoever handed it to
    /// `INET_REQ_FDOPEN`) and a post-close `GETSTATUS`/`GETFD`/`NAME`/`GETTYPE` still answers what
    /// it answered before. That is exactly what the C does and what the oracle reports: measured
    /// on OTP 29.1.1 / erts-17.1, a prebound port answers `GETSTATUS` `[1,0,0,0,5]` (still
    /// `INET_STATE_CONNECTED`) and the same `GETFD`/`NAME`/`GETTYPE` bytes after
    /// `INET_REQ_CLOSE`, where an owned descriptor answers `GETSTATUS` `[1,0,0,0,0]`
    /// (`INET_STATE_CLOSED`) and `GETFD` `[0]einval`. `Drop` runs this too, so a prebound
    /// descriptor is never closed by a dropped driver either.
    fn close_socket(&mut self) {
        // The descriptor half is `erl_inet_close`'s; the latch reset below is this floor's own
        // "start over" for a **port** teardown (the `close` verb, `Driver::stop`, `Drop`). A
        // prebound descriptor is not ours: nothing is closed and the state is kept.
        if !self.close_descriptor() {
            return;
        }
        self.read_eof = false;
        self.peer_close = PeerClose::default();
        self.shutdown_wr_done = false;
        self.pending_shutdown = None;
        // A torn-down port has no spent grant left to remember (see
        // [`TcpInetDriver::read_armed`]).
        self.read_armed = true;
    }
    /// De-registers **and** closes the current socket, returning the de-registration outcome.
    ///
    /// This is the `erl_inet_close` half of the C's teardown only — it does **not** empty the
    /// port's output queue. The close path that does is [`TcpInetDriver::desc_close`]; a caller
    /// that is not a close (the `INET_REQ_CONNECT` failure arm, which the C answers without
    /// touching the queue at all) keeps this narrower seam.
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

    /// The C's `tcp_desc_close` (`inet_drv.c:11932`-`:11939`) — the body of the **`INET_REQ_CLOSE`
    /// verb** (`:12258`-`:12266`) and of every peer-close teardown the C routes through it
    /// (`:12264`, `:12511`, `:12521`, `:12572`, `:12825`, `:12873`, `:13653`, `:13670`). It is
    /// deliberately **not** the driver teardown: `tcp_inet_stop` (`:11893`-`:11924`) runs
    /// `tcp_close_check` + `tcp_clear_input` + `inet_stop` and never touches the `ioq` — the queue
    /// is the flush's and the port layer's business there (see [`TcpInetDriver::flush`] and
    /// [`Driver::stop`]'s body).
    ///
    /// ```c
    /// static void tcp_desc_close(tcp_descriptor* desc)
    /// {
    ///     tcp_clear_input(desc);      /* the framing buffer -> close_socket below */
    ///     tcp_clear_output(desc);     /* driver_deq(ix, driver_sizeq(ix))     */
    ///     erl_inet_close(INETP(desc));/* -> release_socket                    */
    /// }
    /// ```
    ///
    /// `tcp_clear_output` (`:11718`-`:11724`) has **no guard whatsoever**: every close discards
    /// whatever `port_command`/`send` could not push into the socket, and it also runs for a
    /// **prebound** descriptor, because it sits in front of the ownership test. That is what makes
    /// a post-close `GETSTAT` `send_pend`/`port_info` `queue_size` report `0` on the oracle instead
    /// of the stale remainder. Measured on OTP 29.1.1 / erts-17.1: a 53 556-byte remainder on a
    /// connected `gen_tcp` socket ⇒ `send_pend` 53 556, `INET_REQ_CLOSE` reply `[1]`,
    /// `send_pend` 0 afterwards.
    ///
    /// [`TcpInetDriver::close_socket`] cannot do this (it takes no [`DriverContext`], and `Drop`
    /// has no port to clear), so the queue half lives here, on the `ctx`-carrying close path.
    ///
    /// The **port** teardown deliberately does not come through here — see
    /// [`Driver::stop`]'s body, which is the C's `tcp_inet_stop` and keeps its hands off the `ioq`.
    ///
    /// # Errors
    ///
    /// `BeamError::Io` when the poller rejects the de-registration.
    fn desc_close(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        // `tcp_clear_output` first — the same order the C uses, so a de-registration failure
        // below still leaves the queue empty rather than reporting bytes that can never move.
        ctx.output().clear();
        // Then `tcp_clear_input` (inside `close_socket`) and `erl_inet_close`.
        self.release_socket(ctx)
    }

    /// Reads the socket into the port input queue (the `{packet, raw}` default) or, when the
    /// `{packet, N}` option asks for framing, into the driver's own framing buffer
    /// ([`TcpInetDriver::pending_input`]) so the `recv` verb can take whole frames out of it.
    ///
    /// A no-op unless the socket is connected (a listening socket is read through `accept`,
    /// not `read(2)`) and the peer's close has not already been observed. Returns the number of
    /// bytes read.
    ///
    /// `report` says whether **this caller** answers a peer close the read discovers
    /// ([`ReadReport`]): the `recv` verbs do, the reactor (`ready_input`, with no operation
    /// queued) does not and owes the reply to the user's next `recv`.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when the read fails irrecoverably.
    fn read_available(
        &mut self,
        ctx: &mut DriverContext<'_>,
        report: ReadReport,
    ) -> Result<usize> {
        let mut bytes = Vec::new();
        let close = self.read_bytes(&mut bytes, report)?;
        let read = bytes.len();
        if let Some(source) = close {
            // The close this read discovered is answered by **this** caller, and the latch runs
            // before the bytes are queued so that a `{packet, N}` frame read in the same call
            // survives the teardown's `tcp_clear_input` (the C's discovery sites clear and only
            // then deliver, `inet_drv.c:12824`-`:12829`).
            self.latch_peer_close(ctx, source)?;
        }
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
    /// The edge-triggered poller means a caller must drain to `EAGAIN` per readiness callback,
    /// which this loop does.
    ///
    /// Answers the peer-close **observation** when this drain is the one that made it, and latches
    /// nothing itself: it only records [`TcpInetDriver::read_eof`] and hands the [`CloseSource`]
    /// back, so the caller decides when the close is applied (the C's discovery sites clear and
    /// only then deliver, `inet_drv.c:12824`-`:12829`) — see [`TcpInetDriver::read_available`]'s
    /// latch and [`TcpInetDriver::deferred_close`] for the payload-first ordering. `report` is
    /// threaded into the returned [`CloseSource::Read`] — see [`ReadReport`].
    ///
    /// # Errors
    ///
    /// [`TcpInetDriver::connected_socket`]'s `BadArg` (a connected state with no descriptor). A
    /// **read** failure is not one of them: it is classified through [`CloseSource::Read`] instead
    /// of being raised.
    fn read_bytes(
        &mut self,
        out: &mut Vec<u8>,
        report: ReadReport,
    ) -> Result<Option<CloseSource>> {
        if !self.state.is_connected() || self.peer_close.seen {
            return Ok(None);
        }
        let fd = self.connected_socket()?;
        let mut buf = [0u8; READ_CHUNK];
        loop {
            // SAFETY: `fd` is an open, non-blocking socket owned by this driver and `buf` is
            // a writable array of `READ_CHUNK` bytes, so `read(2)` writes at most `READ_CHUNK`
            // bytes into it.
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), READ_CHUNK) };
            if n > 0 {
                let n = n as usize;
                out.extend_from_slice(&buf[..n]);
                // Edge-triggered readiness: keep reading until the socket is empty.
                continue;
            }
            if n == 0 {
                // The peer closed its end cleanly: `tcp_recv`'s `n == 0` arm
                // (`inet_drv.c:13206`) goes to `tcp_recv_closed`, which arms only the *send*
                // one-shot (`:12816`) — the *receive* side is owed the `closed` of
                // `async_error_am_all(am_closed)` (`:12829`), which this floor's reactor must
                // defer to the next `recv` (`report`).
                self.read_eof = true;
                return Ok(Some(CloseSource::Read { error: None, report }));
            }
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EAGAIN) => return Ok(None),
                // The C's `tcp_recv` classification (`inet_drv.c:13185`-`:13210`), in its order:
                // `ECONNRESET` goes to `tcp_recv_error` — which reports `error_atom(err)` and
                // arms nothing — **only** while `TCP_ADDF_SHOW_ECONNRESET` is set
                // (`{show_econnreset,true}`); without it the same errno takes `tcp_recv_closed`,
                // the clean-close path. **Every other** errno (`ECONNABORTED` included — its
                // `ECONNRESET` translation is the Windows `FD_CLOSE` arm only, `:13430`-`:13437`)
                // is `tcp_recv_error`'s "real error" arm.
                other => {
                    let errno = other.unwrap_or(libc::EIO);
                    let error = if errno == libc::ECONNRESET && !self.options.show_econnreset {
                        None
                    } else {
                        Some(errno)
                    };
                    self.read_eof = true;
                    return Ok(Some(CloseSource::Read { error, report }));
                }
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
    /// A **passive** socket is then torn down, as the C does — the descriptor is closed and the
    /// state becomes [`InetState::Closed`], which is what turns `GETSTATUS`/`GETFD`/`PEER`/
    /// `NAME`/`OPEN` into their `INET_STATE_CLOSED` answers. Which discoveries tear down (a
    /// write always, a read only under the default `{exit_on_close,true}`) is
    /// [`TcpInetDriver::tears_down_on_close`]'s, and the close itself
    /// [`TcpInetDriver::close_descriptor`]'s: it is deliberately **not** [`Self::close_socket`],
    /// because the C keeps the `tcp_add_flags` one-shots across `tcp_desc_close` and they are
    /// what make the next armed `recv` answer `closed`/`econnreset`.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when the poller rejects the de-registration; the teardown runs
    /// either way (a dead descriptor must be closed, and the C has no such failure).
    fn latch_peer_close(
        &mut self,
        ctx: &mut DriverContext<'_>,
        source: CloseSource,
    ) -> Result<()> {
        // `tcp_recv_error`'s **active** arm (`inet_drv.c:12875`-`:12881`) reports
        // `tcp_error_message(desc, err)` and only then `tcp_closed_message(desc)`: the error
        // event is delivered **first** and the close follows it — as a second message in the C,
        // and here as the `port_closed()` observation this latch sets right after. A passive
        // socket sends no such message (it ends in `async_error_am_all(error_atom(err))`, the
        // drain below).
        if self.options.active > INET_PASSIVE {
            if let Some(reason) = source.reason() {
                ctx.deliver_active_input(ActiveInputEvent::Error(reason))?;
            }
        }
        self.peer_close.observe(source);
        self.pending_shutdown = None;
        // The peer is gone, so every queued operation can only end one way: the C's
        // `async_error_am_all(am_closed)` (`inet_drv.c:11887`, `:2597`-`:2608`) for a clean close
        // and `async_error_am_all(error_atom(err))` (`:12877`) for a read error. It runs before
        // the queued output is dropped so a failure to stage a completion is not swallowed by
        // the teardown below.
        drain_error_all(
            &mut self.async_ops,
            ctx,
            source.reason().unwrap_or(InetReason::Closed),
        )?;
        ctx.output().clear();
        let released = self.deregister(ctx);
        // `tcp_recv_closed`/`tcp_recv_error`/`tcp_send_or_shutdown_error`'s passive
        // `tcp_desc_close` (`inet_drv.c:12825`, `:12873`, `:13670`).
        if self.tears_down_on_close(source) {
            self.close_descriptor();
        }
        released
    }

    /// Latches a close an earlier [`TcpInetDriver::read_bytes`] reported but no caller has
    /// consumed yet, answering whether it ran.
    ///
    /// A queued receive that read its payload and the peer's end of file in one drain defers the
    /// close ([`TcpInetDriver::deferred_close`]) so the payload reaches the operation first, and
    /// runs this the moment nothing complete is left to deliver.
    fn latch_deferred(&mut self, ctx: &mut DriverContext<'_>) -> Result<bool> {
        match self.deferred_close.take() {
            Some(source) => {
                self.latch_peer_close(ctx, source)?;
                Ok(true)
            }
            None => Ok(false),
        }
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
    /// `BeamError::Io` when the write fails irrecoverably. The `BeamError::BadArg` of a socket
    /// that is not connected ([`TcpInetDriver::connected_socket`]) is defensive only: both callers
    /// — [`TcpInetDriver::write_command`] and [`TcpInetDriver::verb_send`] — test
    /// [`InetState::is_connected`] and the latched peer close first, so no reachable path arrives
    /// here unconnected or after the peer's close.
    fn write_pending(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        if self.peer_close.seen {
            // The peer is gone: the queued output was dropped when the close was latched and
            // the socket must not be touched again. Defensive only — both callers answer the send
            // ladder (and drop what they queued) before they write — but kept so a future caller
            // cannot reach `send(2)` on a dead socket.
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
                    // `tcp_send_error` (`inet_drv.c:13714`-`:13720`) rewrites `EPIPE` to
                    // `ECONNRESET` unless `TCP_ADDF_SHUTDOWN_WR_DONE` is set — which only a
                    // **failed** write shutdown (`:12372`) or an already-applied **deferred** one
                    // (`:13973`) does — and `tcp_send_or_shutdown_error` then reads
                    // `err == ECONNRESET && (tcp_add_flags & TCP_ADDF_SHOW_ECONNRESET)`
                    // (`:13615`) to decide between the `econnreset` and the `closed` reply.
                    let errno = err.raw_os_error().unwrap_or(libc::ECONNRESET);
                    let reset = (errno == libc::ECONNRESET
                        || (errno == libc::EPIPE && !self.shutdown_wr_done))
                        && self.options.show_econnreset;
                    self.latch_peer_close(ctx, CloseSource::Write { reset })?;
                    return Ok(written);
                }
                _ => return Err(error::io(err)),
            }
        }

        if ctx.output().is_empty() {
            // The command is fully written, so the next one must be examined afresh — for a caller
            // tag *and* for its write-side framing. The two latches are cleared together, at the
            // one point that knows the command is over, so neither can outlive it (a framing header
            // kept past the drain would be prepended to an unrelated command's bytes).
            // `caller_ref` is deliberately **not** cleared here: the reply it produces is staged by
            // the caller ([`TcpInetDriver::settle_command`]).
            self.caller_stripped = false;
            self.out_framed = false;
            if let Some(how) = self.pending_shutdown.take() {
                shutdown_socket(fd, how)?;
                if how == TCP_SHUT_WR || how == TCP_SHUT_RDWR {
                    self.shutdown_wr_done = true;
                }
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
    /// A port that is **not** `Closed` answers the C's sequence error, not an argument fault:
    /// `inet_ctl_open` opens with
    /// `if (desc->state != INET_STATE_CLOSED) return ctl_xerror(EXBADSEQ, rbuf, rsize);`
    /// (`inet_drv.c:5130`-`:5131`), i.e. `[0] ++ "exbadseq"` — measured on OTP 29.1.1 /
    /// erts-17.1 as `[0,101,120,98,97,100,115,101,113]` for a second `INET_REQ_OPEN` on an
    /// opened, a listening and a connected port alike. The C's operand decoding runs *before*
    /// that guard (`tcp_inet_ctl`, `:11968`-`:11995`), so a malformed operand still answers its
    /// own refusal; the numeric route does that in
    /// [`control_opcode`](Self::control_opcode) before reaching here.
    ///
    /// A live descriptor **is** a non-`Closed` state on this floor (an owned close takes the
    /// descriptor away and sets [`InetState::Closed`]; a `prebound` `INET_REQ_FDOPEN` one keeps
    /// both across a close, exactly as `erl_inet_close`'s prebound arm does), so the guard is
    /// [`has_socket`](Self::has_socket) — which also means a live socket is never leaked by
    /// replacing it.
    ///
    /// # Errors
    ///
    /// [`beam_common::BeamError::BadArg`] for an argument that is not the three-byte
    /// `[AF_INET, SOCK_STREAM, default|tcp]` record; `BeamError::Io` when the socket cannot be
    /// created.
    fn verb_open(&mut self, ctx: &mut DriverContext<'_>, arg: &[u8]) -> Result<Vec<u8>> {
        if self.has_socket() {
            return Ok(ctl_xerror("exbadseq"));
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
    /// The C does **not** arm accept readiness here: its `sock_select(FD_ACCEPT, 1)` happens only
    /// on the first `INET_REQ_ACCEPT` whose `sock_accept` answered `ERRNO_BLOCK`
    /// (`inet_drv.c:12224`), and the disarm on the last completion/timeout. This floor's listener
    /// is already registered [`Interest::READABLE`] by [`TcpInetDriver::adopt`] (via the `open`
    /// verb), so the arm/disarm pair in the async queue is the whole rule — nothing to do here.
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
    /// pre-stripping the prefix above this crate). The reply itself is not the gap: an
    /// `{inet_reply, …}` rides [`DriverContext::deliver_completion`], which is how the
    /// `erlang:port_command/2` path ([`TcpInetDriver::write_command`]) answers its caller.
    ///
    /// Note that `{packet, 1|2|4}` framing is **not** particular to this verb:
    /// [`TcpInetDriver::output`]/[`TcpInetDriver::ready_output`] (the `erlang:port_command/2`
    /// path) reach [`TcpInetDriver::write_command`], which frames the queued payload with the very
    /// same [`frame::write_header`] — the C frames on both routes (`tcp_send` and `tcp_sendv`).
    /// What differs is only the **length** the header announces: the payload left after the
    /// caller-tag strip, so the tag bytes are never counted in the frame.
    ///
    /// A `shutdown(SHUT_WR|SHUT_RDWR)` **deferred** because output was still queued
    /// ([`TcpInetDriver::pending_shutdown`]) rejects a further `send`, exactly as the C does:
    /// `tcp_inet_commandv` routes it to `tcp_shutdown_error(desc, EPIPE)` (`inet_drv.c:12695`),
    /// which runs the close path and answers `{error, closed}` (`tcp_send_or_shutdown_error`,
    /// `:13654`-`:13670`). So this send answers `[0] ++ "closed"`, the queued output is dropped
    /// and every later one answers `enotconn`.
    ///
    /// # Errors
    ///
    /// The write failure [`TcpInetDriver::write_pending`] reports — `BeamError::Io`, or the
    /// `unsupported` raise of a `{packet, …}` code with no decoder. An unconnected port is the
    /// `[0]enotconn` reply below, never an `Err`.
    fn verb_send(&mut self, ctx: &mut DriverContext<'_>, arg: &[u8]) -> Result<Vec<u8>> {
        // The C's `!IS_CONNECTED` arm consults the send ladder **first**
        // (`tcp_inet_commandv`, `inet_drv.c:12681`-`:12692`, reached from `:12674`): a latched
        // `closed`/`econnreset` one-shot is what the caller is answered with, and only an unarmed
        // ladder is the bare `enotconn`. The order matters because a passive close under the
        // default `{exit_on_close,true}` ends in `tcp_desc_close` → `erl_inet_close`, which
        // clears `INET_STATE_CONNECTED` (`:5078`) and sets `INET_STATE_CLOSED` (`:5106`) — so a
        // state-first guard would swallow the one-shot `tcp_recv_closed` armed (`:12816`).
        if let Some(reason) = self.peer_close.send_reply() {
            return Ok(ctl_xerror(&reason.atom_name()));
        }
        if !self.state.is_connected() {
            return Ok(ctl_xerror("enotconn"));
        }
        if self.pending_shutdown.is_some() {
            // A write shutdown could not be applied yet (the queue was not empty), and the C
            // refuses a further send with `EPIPE` rather than queueing behind it
            // (`inet_drv.c:12695`): the close path runs, so this send answers `closed` —
            // consuming the send direction's one-shot reply — the output that can never be
            // delivered is dropped, the socket is de-registered and every later send answers
            // `enotconn` (`tcp_send_or_shutdown_error`, `:13654`-`:13670`).
            // `tcp_shutdown_error(desc, EPIPE)` (`:12695`) hands `EPIPE` — never `ECONNRESET` —
            // to `tcp_send_or_shutdown_error`, so this observation is never a reset and the reply
            // is `closed` whatever `{show_econnreset}` says.
            self.latch_peer_close(ctx, CloseSource::Write { reset: false })?;
            self.peer_close.consumed_by_send();
            return Ok(ctl_xerror("closed"));
        }
        // `{packet, 1|2|4}` frames the payload with a length header before the bytes reach the
        // queue; every other code (raw, line, ASN.1/…, the HTTP family) takes the C's `default:`
        // arm and writes no header at all. This is the **same** framing the
        // `erlang:port_command/2` route applies ([`TcpInetDriver::write_command`]): the C frames in
        // `tcp_send` (`inet_drv.c:13861`-`:13885`) and in `tcp_sendv` (`:13744`-`:13769`) alike.
        //
        // This named verb carries **no** caller tag, so the command it queues must never be
        // examined for one: without this, a partially-written named send's remainder could later
        // be read as a `prim_inet` prefix by `ready_output` and stripped from the middle of the
        // data stream. `write_pending` clears the flag again once the queue drains, so the next
        // command (the `erlang:port_command/2` one, [`TcpInetDriver::write_command`]) is examined
        // afresh. The framing latch is set with it for the same reason: this header is already on
        // the queue, so a re-driven remainder must never be framed a second time.
        self.caller_stripped = true;
        self.out_framed = true;
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
            // This write is what found the peer gone, so it answers the close itself — `closed`,
            // or `econnreset` when the observation was a reset under `{show_econnreset,true}`
            // (`inet_reply_error`/`inet_reply_error_am`, `inet_drv.c:13664`-`:13669`). The C arms
            // the next receive (`flags |= TCP_ADDF_DELAYED_CLOSE_RECV`, `:13678`) and, for a
            // reset, leaves `TCP_ADDF_DELAYED_ECONNRESET` armed too (`:13683`); the send
            // one-shot is *not* armed, because this call answered it on the spot.
            let name = self.peer_close.observed_reason().atom_name();
            self.peer_close.consumed_by_send();
            return Ok(ctl_xerror(&name));
        }
        Ok(ctl_ok())
    }

    /// The `shutdown` verb: shuts one or both directions of the connection down.
    ///
    /// The C's `TCP_REQ_SHUTDOWN` arm (`inet_drv.c:12339`-`:12370`) runs the **connection** guard
    /// first — `!IS_CONNECTED ⇒ ctl_error(ENOTCONN)` (`:12349`) — and the operand-length guard
    /// only afterwards — `len != 1 ⇒ ctl_error(EINVAL)` (`:12352`, `:12355`). Both are `[0]`-tagged
    /// **replies**, never an argument raise, and the order is observable: a port with no peer
    /// answers `[0]enotconn` whatever the operand's length is, while a **connected** one answers
    /// `[0]einval`. Measured on OTP 29.1.1 against a raw `{spawn_driver,"tcp_inet"}` port:
    /// `port_control(P, 44+magic, <<>>)` is `[0]"einval"` when connected and `[0]"enotconn"`
    /// otherwise, and a valid one-byte operand on a connected port is `[1]`.
    ///
    /// A `SHUT_WR`/`SHUT_RDWR` with output still queued is **deferred** until the queue drains
    /// (the C's `TCP_ADDF_PENDING_SHUT_WR`/`TCP_ADDF_PENDING_SHUT_RDWR`, `:12356`-`:12363`), so no
    /// queued byte is silently dropped. Exactly as in the C, the deferral is gated on
    /// `how != TCP_SHUT_RD` — a read-side shutdown is applied at once — and on `how` being one of
    /// the two write values: any other byte falls through to `shutdown(2)`, which answers the
    /// kernel's `EINVAL` as `[0]einval`.
    ///
    /// `TCP_ADDF_SHUTDOWN_WR_DONE` is latched in exactly the two places the C latches it, both
    /// **away** from the direct success arm: a **failed** `shutdown(2)` for a write-side `how`,
    /// before the error reply (`inet_drv.c:12371`-`:12374`), and the **deferred** drain
    /// `tcp_shutdown_async` (`:13973`). The direct success arm (`:12377`-`:12378`) sets nothing, so
    /// a successful `shutdown(write)` leaves the later `EPIPE` to be rewritten to `ECONNRESET`
    /// (`tcp_send_error`, `:13717`) — which `tcp_send_or_shutdown_error`'s
    /// `err == ECONNRESET && TCP_ADDF_SHOW_ECONNRESET` (`:13615`) renders `econnreset` under
    /// `{show_econnreset,true}` and `closed` (`am_closed`) otherwise.
    ///
    /// # Errors
    ///
    /// Never for an operand the C's arm replies to: a wrong length is the `[0]einval` reply and a
    /// failing `shutdown(2)` is `[0] ++ errno`. Only a state contradiction (connected with no
    /// descriptor) stays typed.
    fn verb_shutdown(&mut self, ctx: &mut DriverContext<'_>, arg: &[u8]) -> Result<Vec<u8>> {
        if !self.state.is_connected() {
            return Ok(ctl_xerror("enotconn"));
        }
        let [how] = arg else {
            // `if (len != 1) return ctl_error(EINVAL, rbuf, rsize)` (`inet_drv.c:12352`) — the
            // C's `ctl_error(EINVAL)`, i.e. the `[0] ++ "einval"` reply.
            return Ok(ctl_xerror("einval"));
        };
        let how = *how;
        let fd = self.connected_socket()?;
        if how != TCP_SHUT_RD && !ctx.output().is_empty() {
            if how == TCP_SHUT_WR || how == TCP_SHUT_RDWR {
                self.pending_shutdown = Some(how);
                return Ok(ctl_ok());
            }
        }
        match shutdown_socket(fd, how) {
            // `else return ctl_reply_ok(rbuf, rsize);` (`inet_drv.c:12377`-`:12378`) — the C's
            // success arm latches **nothing**, so `TCP_ADDF_SHUTDOWN_WR_DONE` stays clear and a
            // later `EPIPE` from this deliberately shut write half is still rewritten to
            // `ECONNRESET` (`tcp_send_error`, `:13717`), i.e. reported as the reset under
            // `{show_econnreset,true}` (`tcp_send_or_shutdown_error`, `:13615`).
            Ok(()) => Ok(ctl_ok()),
            Err(err) => {
                // `if (how != TCP_SHUT_RD) desc->tcp_add_flags |= TCP_ADDF_SHUTDOWN_WR_DONE;`
                // runs **before** the error reply (`inet_drv.c:12371`-`:12374`), so a write-side
                // shutdown the kernel refused still counts as "the write half is shut": the
                // `EPIPE`→`ECONNRESET` rewrite in `tcp_send_error` is skipped afterwards
                // (`:13717` tests exactly this flag), so a later `EPIPE` surfaces as `epipe`
                // rather than `econnreset`.
                if how != TCP_SHUT_RD {
                    self.shutdown_wr_done = true;
                }
                Ok(ctl_error(err))
            }
        }
    }

    /// The `close` verb: de-registers and closes the socket, leaving the port open but idle.
    ///
    /// [`crate::PortControl::CLOSE`] reserves this name at the [`crate::PortRegistry`] level,
    /// so this verb is reached only by a caller driving the port's driver directly (the
    /// `erlang:port_control/3` side arrives as the numeric `INET_REQ_CLOSE`). The body is the
    /// C's `tcp_close_check` followed by `tcp_desc_close` (`inet_drv.c:12258`-`:12266`).
    ///
    /// # Errors
    ///
    /// `BeamError::Io` when the poller rejects the de-registration.
    fn verb_close(&mut self, ctx: &mut DriverContext<'_>) -> Result<Vec<u8>> {
        // Every queued operation is answered `{error, closed}` first (the C's
        // `tcp_close_check` → `async_error_am_all(am_closed)`, `inet_drv.c:11887`) and only then
        // does the descriptor go away — `tcp_desc_close`, which also empties the output queue.
        drain_closed(&mut self.async_ops, ctx)?;
        self.desc_close(ctx)?;
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
    ///
    /// This is the **ctx-free** half of the request: the port's two wire forms go through
    /// [`TcpInetDriver::setopts_with_delivery`], which is this walk plus the tail of
    /// `inet_set_opts` (the passive message and the inline read). Kept as the reference the
    /// suite pins that tail against, byte for byte.
    #[allow(dead_code)]
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
            // `INET_REQ_SETOPTS` (`:11127`-`:11150`): the reply comes from `inet_set_opts`, whose
            // own tail may arm read readiness and, for `{active, once}`/`{active, N}`, trigger an
            // **inline** read (`sres == 2`). Both side effects are `setopts_with_delivery`'s.
            Opcode::Setopts => self.setopts_with_delivery(ctx, arg),
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
            // `{inet_async, Port, Id, {ok, Descriptor}}`. Queueing it also arms the listener's
            // accept (read) readiness (`sock_select(FD_ACCEPT, 1)`, `:12224`).
            Opcode::Accept => self.async_accept(ctx, arg),
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
                        //
                        // An adoption failure is the C's **reply**, never a raise: the C's
                        // `inet_ctl_fdopen` answers `ctl_error(sock_errno())` for every failure
                        // inside it (`:5253`, `:5260`), so the errno the failing probe already
                        // carries (`fcntl`/`getpeername`/the poller) becomes `[0] ++ errno_name`
                        // ([`adoption_reply`]). Only a failure with **no** errno — an internal
                        // limit, never a descriptor verdict — keeps the typed error.
                        match self.adopt_prebound(ctx, fd, u16::from(INET_TYPE_STREAM)) {
                            Ok(()) => Ok(ctl_ok()),
                            Err(err) => match adoption_reply(&err) {
                                Some(reply) => Ok(reply),
                                None => Err(err),
                            },
                        }
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
            // The descriptor state, including the C's `INET_F_ACC`/`INET_F_MULTI_CLIENT` state bits
            // (`:1031`-`:1032`, `:11160`). A listener with one accept pending is `9 | 0x20` = 41
            // (`[1,0,0,1,41]`, measured on the oracle) and with two `9 | 0x120` = 297 — and the
            // multi bit is **sticky**: the C restores `INET_STATE_LISTENING` only once the accept
            // list drains (`:12607`-`:12611`, `:13536`-`:13540`), so a survivor keeps reporting 297.
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

    /// `ERL_DRV_FLAG_SOFT_BUSY` in the C's `tcp_inet` entry — together with
    /// `ERL_DRV_FLAG_USE_PORT_LOCKING`, the entry's whole flag word (`inet_drv.c:1374`).
    ///
    /// It is the driver's answer to `erlang:port_command(Port, Data, [force])`: the BIF refuses
    /// that option with `error:notsup` for any driver whose entry lacks the bit
    /// (`erl_bif_port.c:195-198`), and with it the C clears its busy mask entirely
    /// (`busy_flgs = 0` under `ERTS_PORT_SIG_FLG_FORCE`, `io.c:1731`), so a forced command is
    /// never the busy arm. The measured oracle (`tcp_inet` vs its siblings):
    /// `open_port({spawn_driver,"tcp_inet"},[])` + `port_command(P, <<"abc">>, [force])` answers
    /// `true`, while the same call on a `"udp_inet"` or a `"ram_file_drv"` port raises `notsup`.
    /// Pinned by `soft_busy_tests.rs`.
    fn flags(&self) -> DriverFlags {
        DriverFlags::SOFT_BUSY
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
        // One readiness event, **one** reading of it — the C's `tcp_inet_input` dispatch
        // (`inet_drv.c:13461`), which is on the descriptor's state and not on its mode: a
        // listener takes the accept ladder (`:13469`/`:13529`), and a connected socket gets
        // exactly one `tcp_recv(desc, 0)` (`:13598`-`:13600`) whose delivery then splits on
        // `desc->inet.active` — passive ⇒ `inet_async_data` onto the port input queue
        // (`tcp_reply_data`, `:4294`-`:4295`), otherwise `tcp_message` to the owner (`:4296`-
        // `:4297`). So an `{active, …}` connection delivers through the **active-input channel**
        // (one `{tcp, S, Data}` per delivered unit, never the port input queue) and never through
        // both:
        //
        // * [`TcpInetDriver::owns_readiness_for_active`] is the C's connected-and-active arm —
        //   [`TcpInetDriver::active_delivery`] owns the socket's read;
        // * everything else (a listener's queued `accept`, a queued `TCP_REQ_RECV`, or a passive
        //   connection's bytes into the port input queue) stays with
        //   [`TcpInetDriver::async_ready_input`].
        if self.owns_readiness_for_active() {
            self.active_delivery(ctx)
        } else {
            self.async_ready_input(ctx)
        }
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
    /// The **wait-side** twin of [`Driver::has_pending_async`]: an answer a later port round still
    /// owes a parked caller.
    ///
    /// Identical to the drive answer here, and **accept-inclusive by construction**: a queued
    /// `accept` carries a real caller's `{inet_async, Port, Id, Result}` (`prim_inet:accept0/3`
    /// parks in a bare `receive` waiting for it — see `opcode.rs`'s `INET_REQ_ACCEPT`), and a
    /// listener with no queued accept is deregistered by `disarm_accepts`, so there is no standing
    /// readiness interest without a caller behind it.
    fn awaits_callers_async_op(&self) -> bool {
        self.queued_ops().has_pending()
    }

    /// The latched peer close ([`TcpInetDriver::peer_closed`]) — the accessor the runtime reads
    /// once per round to deliver the oracle's `{tcp_closed, Port}`.
    fn port_closed(&self) -> bool {
        self.peer_closed()
    }

    /// The port's `{active, …}` state — the C's `desc->inet.active` (`INET_PASSIVE`/`INET_ACTIVE`/
    /// `INET_ONCE`/`INET_MULTI`, `inet_drv.c:786`-`:789`) plus the running `active_count` when the
    /// mode is the counter form (`:1552`-`:1560`).
    ///
    /// Both values live in this driver's own option store, written by the `setopts` walk
    /// (`opt_state::set_active`, the C's `:6967`-`:7000`) and spent by
    /// [`TcpInetDriver::count_delivery`] — so this is the only way a runtime can learn the
    /// running counter, which no owner-facing message carries.
    fn active_mode(&self) -> ActiveMode {
        ActiveMode::from_parts(self.options.active, self.options.active_count)
    }

    /// Drains the grants a **delivery** spent since the last call, oldest first — the notification
    /// half of [`Driver::active_mode`]'s state machine.
    ///
    /// Recorded only at [`TcpInetDriver::count_delivery`] (the C's
    /// `INET_CHECK_ACTIVE_TO_PASSIVE`, `inet_drv.c:1552`-`:1561`), never by the `setopts` walk that
    /// drives an `{active, N}` counter to zero (`:6993`-`:6998`), which spends nothing because it
    /// delivers nothing. Taking consumes, so a caller drains after every round.
    fn take_active_spends(&mut self) -> Vec<ActiveSpend> {
        core::mem::take(&mut self.active_spends)
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
            "setopts" => self.setopts_with_delivery(ctx, arg),
            "getopts" => self.verb_getopts(arg),
            _ => Err(error::badarg()),
        }
    }

    /// The C's `tcp_inet_flush` (`inet_drv.c:12708`-`:12731`) — the driver entry's `flush` slot
    /// (`:1368`), which the emulator calls from `flush_port` (`erts/emulator/beam/io.c:3659`-`:3700`,
    /// the `drv_ptr->flush != NULL` test at `:3664` and the call at `:3687`) while a closing port
    /// still holds an `ioq`, **before** `tcp_inet_stop` can run:
    ///
    /// ```c
    /// static void tcp_inet_flush(ErlDrvData e)
    /// {
    ///     tcp_descriptor* desc = (tcp_descriptor*)e;
    ///     int discard_output;
    ///     /* Discard send queue to avoid hanging port (OTP-7615) */
    ///     discard_output = !(desc->inet.event_mask & FD_WRITE);
    ///     discard_output |= desc->tcp_add_flags & TCP_ADDF_LINGER_ZERO;
    ///     ...
    ///     if (discard_output) {
    ///         tcp_clear_output(desc);
    ///     }
    /// }
    /// ```
    ///
    /// So the queue is dropped in exactly two situations, and **kept otherwise** — kept, that is,
    /// for the port layer to write out during its `Closing` drain
    /// ([`crate::Port::close`] → [`crate::PortState::Closing`]):
    ///
    /// * **write readiness is not armed for this socket.** `event_mask & FD_WRITE` is what the C
    ///   sets while a write can still make progress: `sock_select(…, FD_WRITE, 1)` after a short
    ///   write re-armed the buffer (`tcp_send`'s `INET_ECONN_BLOCK` arm) and for a connect in
    ///   flight. This floor's counterpart is its single registration's `Interest` mask — the
    ///   `Interest::WRITABLE`/`Interest::BOTH` that
    ///   [`TcpInetDriver::write_pending`] arms when a remainder is queued
    ///   (`self.interest.is_writable()`) — **and** the registration itself: the C's
    ///   `erl_inet_close` (`:5101`-`:5117`) zeroes the mask in both of its arms
    ///   (`desc_close`'s `desc->event_mask = 0;`, `:5073`, and the prebound arm's
    ///   `sock_select(desc, FD_READ|FD_WRITE|FD_CLOSE|ERL_DRV_USE_NO_CALLBACK, 0);`
    ///   then `desc->event_mask = 0;`, `:5108`-`:5109`), which is this driver's
    ///   [`TcpInetDriver::deregister`] taking the token away, so a closed or peer-closed
    ///   descriptor is "not armed" even though the last `Interest` snapshot still says otherwise.
    /// * **`{linger, {true, 0}}` is in effect** — [`SocketOptions::linger_zero`], the C's
    ///   `TCP_ADDF_LINGER_ZERO` (`:864`, set by `inet_set_opts`'s `INET_OPT_LINGER` arm,
    ///   `:7382`-`:7386`). A zero-linger close aborts with an `RST`, so queued bytes have nowhere
    ///   to go.
    ///
    /// The C's `tcp_clear_output` also calls `send_empty_out_q_msgs` (`:11724`, the
    /// `INET_REQ_SUBSCRIBE` notification half) — that channel has no counterpart here yet, so the
    /// discard is the whole effect.
    ///
    /// `sendfile`'s `HAVE_SENDFILE` arm (`:12717`-`:12723`) forces the discard; this floor
    /// refuses `TCP_REQ_SENDFILE`(45) outright, so there is no sendfile state to abort.
    ///
    /// # Errors
    ///
    /// Never: the C entry returns `void`, and a discard cannot fail. The [`Result`] is the frozen
    /// seam's shape.
    fn flush(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        // `discard_output = !(desc->inet.event_mask & FD_WRITE) | TCP_ADDF_LINGER_ZERO`.
        let write_armed = self.token.is_some() && self.interest.is_writable();
        if !write_armed || self.options.linger_zero {
            // `tcp_clear_output`: `driver_deq(ix, driver_sizeq(ix))` (`:11718`-`:11724`).
            ctx.output().clear();
        }
        Ok(())
    }
    fn stop(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        // Every queued operation is answered with `{error, closed}` **before** the descriptor goes
        // away — the C's `async_error_am_all(am_closed)` (`inet_drv.c:2597`-`:2608`) — and the
        // release outcome is kept: neither half is dropped on the other's failure.
        //
        // The teardown is the C's `tcp_inet_stop` (`:11895`-`:11924`) — `tcp_close_check`, then
        // `tcp_clear_input`, then `inet_stop` (`:10847`, `erl_inet_close` + free) — and it
        // **never touches the port's `ioq`**: the queued output is the flush's business
        // ([`TcpInetDriver::flush`], which drops it only when write readiness is unarmed or
        // `{linger, {true, 0}}` is set) and the port layer's `Closing` drain's, which is why the
        // queue reaching here is normally empty already. On a **forced** close (the driver-failure
        // path, `io.c:7220`'s `prt->ioq.size = 0`, and this crate's
        // `PortRegistry`'s `close_now`) the remainder is dropped by the teardown's own `stopq`
        // (`io.c:3768`) — i.e. with the port — and never written.
        //
        // So this is [`TcpInetDriver::release_socket`], not [`TcpInetDriver::desc_close`]: the
        // close **verb** is the only caller of the C's `tcp_desc_close` body here (see
        // [`TcpInetDriver::verb_close`]).
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

// The `{active, true|once|N}` delivery path — the C's `tcp_deliver`/`tcp_recv`
// (`inet_drv.c:13045`-`:13260`) that sends the owner a `{tcp, S, Data}` message instead of
// filling the port input queue.
mod active;
// The `{active, …}` **state seam** (`Driver::active_mode` / `Driver::take_active_spends`) as both
// socket drivers implement it: the running `{active, N}` counter, and the grants a **delivery**
// spent (never the `setopts` walk's own zero).
#[cfg(test)]
mod active_seam_tests;
#[cfg(test)]
mod active_tests;
#[cfg(test)]
mod accept_tests;
// The **passive** half of the read-error delivery: a queued `TCP_REQ_RECV` completed by the
// peer's reset (the failing errno) or by its clean close (`closed`) — the
// `drain_error_all`-vs-`drain_closed` split `async.rs` makes.
#[cfg(test)]
#[path = "async_error_tests.rs"]
mod async_error_tests;
#[cfg(test)]
#[path = "close_tests.rs"]
mod close_tests;
#[cfg(test)]
#[path = "error_delivery_tests.rs"]
mod error_delivery_tests;
#[cfg(test)]
mod fdopen_errno_tests;
// `tcp_inet`'s `flush` override — the C's `tcp_inet_flush` (`inet_drv.c:12708`-`:12731`), whose
// `discard_output` decides whether a close may keep the queued output or has to drop it.
#[cfg(test)]
#[path = "flush_tests.rs"]
mod flush_tests;
#[cfg(test)]
mod frame_tests;
#[cfg(test)]
mod header_tests;
#[cfg(test)]
#[path = "inherit_tests.rs"]
mod inherit_tests;
#[cfg(test)]
mod opcode_tests;
#[cfg(test)]
mod opcode_tests_refusals;
#[cfg(test)]
mod opcode_tests_states;
#[cfg(test)]
mod opcode_tests_udp;
#[cfg(test)]
#[path = "open_reply_tests.rs"]
mod open_reply_tests;
#[cfg(test)]
#[path = "peer_close_tests.rs"]
mod peer_close_tests;
#[cfg(test)]
#[path = "soft_busy_tests.rs"]
mod soft_busy_tests;
#[cfg(test)]
mod stat_tests;
#[cfg(test)]
#[path = "tcp_shutdown_tests.rs"]
mod tcp_shutdown_tests;
#[cfg(test)]
#[path = "shutdown_contract_tests.rs"]
mod shutdown_contract_tests;
#[cfg(test)]
mod tests;
// The **write-side** framing pin: the `{packet, 1|2|4}` length header the C prepends in
// `tcp_sendv`/`tcp_send` (`inet_drv.c:13744`-`:13769`, `:13861`-`:13885`) on the
// `erlang:port_command/2` route every `gen_tcp:send/2` takes — byte-compared against the far end of
// a real loopback connection and against the `erl` oracle.
#[cfg(test)]
#[path = "write_frame_tests.rs"]
mod write_frame_tests;
// The **read-path ordering** pin: a complete buffered unit wins over a pending close — the C's
// `tcp_recv` delivers what the drain collected (`inet_drv.c:13216`-`:13217`/`:13229`-`:13230`)
// before a later call's `n == 0` may reach `tcp_recv_closed` (`:13206`-`:13208`).
#[cfg(test)]
#[path = "write_then_close_tests.rs"]
mod write_then_close_tests;
/// The **length-framed** wire format the `erl`-oracle dumps use (a 4-byte big-endian byte count
/// per reply) — the Rust half of the framing the suites' oracle expressions write, so the two
/// halves state the format in one place.
#[cfg(test)]
mod oracle_dump;