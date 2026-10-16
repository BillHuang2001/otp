# `inet` driver subtree (`src/drivers/inet/`) — the built-in `tcp_inet` and `udp_inet` socket drivers

## Intent
The built-in **`tcp_inet`** driver: **one** non-blocking TCP socket behind one port — the Rust counterpart of the C driver `gen_tcp`/`inet_tcp` sit on (`erts/emulator/drivers/common/inet_drv.c`).
The built-in **`udp_inet`** driver (`udp.rs`) is the UDP half of the same C file: **one** `AF_INET`/`SOCK_DGRAM` socket behind one port, the driver `gen_udp` sits on.
Frozen registered names **`DRIVER_NAME = "tcp_inet"`** and **`UDP_DRIVER_NAME = "udp_inet"`** (the oracle's `port_info(Port, name)` values and the names `prim_inet` opens, `inet_drv.c:1348`/`:11834` for TCP and `inet_drv.c:1406` for UDP).
Registered with the crate's `DriverRegistry` through `make_tcp_inet()`/`make_udp_inet()` (their `DriverFactory`s); a port gets its socket from the `open` verb, or the driver adopts a caller's descriptor via `TcpInetDriver::from_socket`/`UdpInetDriver::from_socket`.
The TCP driver owns the **read-path packet framing** for `{packet, N}` — a faithful, term-free port of the C `packet_parser` (`packet_get_length`/`packet_get_body`).
Legacy C ground truth (read-only): `erts/emulator/drivers/common/inet_drv.c`, `erts/emulator/beam/packet_parser.{c,h}`, the Erlang half `erts/preloaded/src/prim_inet.erl`.

## API Surface (FROZEN)

### `mod.rs` — the driver
- `DRIVER_NAME: &str = "tcp_inet"`; `make_tcp_inet() -> Result<Box<dyn Driver>>` (the `DriverFactory`).
- `TcpInetDriver` (`Driver::name() == "tcp_inet"`) — one listening or connected socket; `new()` (socket-less, the registry factory; the `open` verb creates the socket, mirroring `tcp_inet_start` + `INET_REQ_OPEN`), `from_socket(fd)` (adopts a descriptor and probes kernel state: `SO_ACCEPTCONN`⇒Listening, `getpeername`⇒Connected, else Open).
- Accessors: `socket() -> Option<RawFd>`, `state() -> InetState`, `eof() -> bool` (read EOF latched), `peer_closed() -> bool` (the peer-close latch), `connect_error() -> Option<&str>` (the late `SO_ERROR` of a non-blocking `connect`, a lowercase errno name).
- `InetState { Closed, Open, Listening, Connecting, Connected }` (`is_connected`/`is_listening`) — mirrors the C descriptor's `INET_STATE_*`: no socket until `open`; `listen` makes a listener; `connect`/`accept` make a connection.
- `PeerClose` (`pub(crate)`-style private) — the C's `TCP_ADDF_DELAYED_CLOSE_RECV`/`_SEND` bookkeeping (`inet_drv.c:855-856`): after a peer close the **first** `recv` and the **first** `send` answer `closed`, every later one `enotconn` (`:12798-12819`, `:13662-13684`).
- **Control verbs** — where the C decodes a magic-offset integer plus a packed binary, this floor takes a **named** op + raw argument bytes in `Driver::control` (the runtime owns the Erlang term decoding).
  The verbs and the C request each mirrors:

| verb | C request | argument | reply |
|---|---|---|---|
| `open` | `INET_REQ_OPEN` (`:11961`) | `[AF, TYPE, PROTO]` (3 bytes) | `[1]` |
| `bind` | `INET_REQ_BIND` (`:11409`) | packed address | `[1, port_hi, port_lo]` |
| `listen` | `INET_REQ_LISTEN` (`:12042`) | `[backlog_hi, backlog_lo]` | `[1]` |
| `accept` | `INET_REQ_ACCEPT` (`:12144`) | — | `[1, fd_be32]`, or `[1]` when nothing is pending |
| `connect` | `INET_REQ_CONNECT` (`:12064`) | packed address | `[1]` |
| `send` | `tcp_inet_command` (`:12629`) | the bytes to send | `[1]` |
| `recv` | `TCP_REQ_RECV` (`:12268`) | — | `[1]` |
| `shutdown` | `TCP_REQ_SHUTDOWN` (`:12339`) | `[how]` | `[1]` |
| `close` | `INET_REQ_CLOSE` (`:12259`) | — | `[1]` |
| `setopts` | `INET_REQ_SETOPTS` (`:11111`) | option records `[opt, val_be32]*` | `[1]`, or `[0] ++ "einval"` |
| `getopts` | `INET_REQ_GETOPTS` (`:11046`) | option codes `[opt]*` | `[1] ++ [opt, val_be32]*`, or `[0] ++ "einval"` |

- **Reply shape** = the C's `ctl_reply` (`:2260-2311`): a one-byte result tag (`INET_REP_OK` 1 / `INET_REP_ERROR` 0, `:869-871`) + payload; `[1]` on success (a `port_be16`/`fd_be32` operand where the C has one), else `[0]` + a lowercase errno name (`erl_errno_id`, `:2249`) or a fixed atom (`einval`/`enotconn`/`eisconn`/`exbadport`).
- **Operational** failures are the `[0]`-tagged reply; a **malformed argument** or an **unknown verb** is `BeamError::BadArg` (the C's `ctl_error(EINVAL)` for the former).
- **Packed address** (`bind`/`connect`, the C's `inet_set_faddress`/`inet_set_address` `:4795`/`:4706`): `[INET_AF_INET(1), port_hi, port_lo, a, b, c, d]` (7 bytes), or `[INET_AF_ANY(3) | INET_AF_LOOPBACK(4), port_hi, port_lo]` (3 bytes); any other family tag is `BadArg` (DNS/IPv6 are out of scope).
- **Readiness & flow control**: the poller is edge-triggered (`EPOLLET`), so every callback drains to `EAGAIN`.
  A socket is **one** descriptor ⇒ **one** registration whose `Interest` mask is flipped: readable while connected, plus writable while the output queue is non-empty (a short write keeps the remainder queued and **arms** write readiness) and while an async `connect` completes; a drained queue drops writability.
  Output still queued when a peer close is latched is dropped (the C's `tcp_clear_output`, `:12786`).
  Writes use `send(2)` with `MSG_NOSIGNAL`.
- **Peer close**: `read(2) == 0` or a reset (`ECONNRESET`/`ECONNABORTED` on read, `ECONNRESET`/`EPIPE` on write) latches one event — registration released, socket never touched again, the one-shot `closed`/`enotconn` replies armed.
- **One socket, one port (the `accept` divergence)**: the C's `INET_REQ_ACCEPT` creates a **second** Erlang port for the accepted socket (`tcp_inet_copy` + `driver_create_port` + `async_ok_port`, `:12236-12252`) and answers a two-byte async id; this seam has no port-creation capability, so `accept` **transfers this port to the connection** — the listener is deregistered/closed, the accepted descriptor becomes the port's socket, and the reply carries the **new descriptor** (`[1] ++ fd_be32`) for correlation.
  Multi-accept is deferred (needs the async-op queue + port creation).
- `pending_input: Vec<u8>` (private) — bytes read but not yet a whole frame, used only under a framing `{packet, N}` (never for `{packet, raw}`); the counterpart of the C's `desc->i_buf`.
  Cleared when the socket is replaced/closed.
  A `{packet, …}`/`{line_delim, …}` change arriving with bytes buffered is **not** flushed: the next `recv` re-decodes them under the new options (a switch **to** `raw` delivers the whole retained buffer verbatim).
- **Ownership**: the driver owns its descriptors (`from_socket` adopts; `open`/`accept` create) — closed on `stop`, on the `close` verb and on `Drop`; nothing else may close them.

### `udp.rs` (`pub`) — the datagram driver
- `UDP_DRIVER_NAME: &str = "udp_inet"`; `make_udp_inet() -> Result<Box<dyn Driver>>` (the `DriverFactory`, the counterpart of `make_tcp_inet`).
- `UdpInetDriver` (`Driver::name() == "udp_inet"`) — one datagram socket; `new()` (socket-less; the `open` verb creates the socket, mirroring `udp_inet_start` + `INET_REQ_OPEN`) and `from_socket(fd)` (adopts a caller's descriptor).
- Accessors: `socket() -> Option<RawFd>`, `state() -> InetState` — the lifecycle reuses the same `InetState` (`Closed` until `open`, `Open` after it, `Connected` after a `connect`).
- **Control verbs** — the same named-op wire as the TCP driver (the runtime owns the Erlang term decoding), with the C request each mirrors:

| verb | C request | argument | reply |
|---|---|---|---|
| `open` | `INET_REQ_OPEN` (`:14577`) | `[AF, TYPE, PROTO]` (3 bytes) | `[1]`, `[0] ++ "eafnosupport"`, `[0] ++ "einval"`, or `[0] ++ "eprotonosupport"` |
| `bind` | `INET_REQ_BIND` (`:11409`) | packed address | `[1, port_hi, port_lo]` |
| `connect` | `INET_REQ_CONNECT` (`:14710`) | packed address | `[1]`, or `[0] ++ errno` |
| `send` | `packet_inet_command` (`:15037`) | `[address][payload]` | `[1]`, or `[0] ++ errno` |
| `recv` | `PACKET_REQ_RECV` (`:14968`) | — | `[1] ++ src_address` with a datagram, the bare `[1]` when none is pending, `[0] ++ errno` |
| `close` | `INET_REQ_CLOSE` (`:14701`) | — | `[1]` |
| `setopts` | `INET_REQ_SETOPTS` (`:11111`) | option records `[opt, val_be32]*` | `[1]`, or `[0] ++ "einval"` |
| `getopts` | `INET_REQ_GETOPTS` (`:11046`) | option codes `[opt]*` | `[1] ++ [opt, val_be32]*`, or `[0] ++ "einval"` |

- **`recv` carries the source address (the one design decision).** A datagram's source cannot ride the port's byte-only input queue and [`crate::PortNotification`] must keep exactly its two variants (an outside crate matches it exhaustively), so reception is **verb-driven**: `recv` performs **one** `recvfrom(2)` into a 64 KiB buffer (`DATAGRAM_CHUNK`, larger than the largest IPv4 UDP payload), pushes the payload to the port input queue (harvested by the runtime as a `Data` notification and rendered as `{udp, S, IP, Port, Data}`) and answers the **eight**-byte `[1] ++ [INET_AF_INET, port_hi, port_lo, a, b, c, d]`; on `EAGAIN` it answers the **bare** `[1]` and delivers nothing, so the reply length distinguishes the two.
  `Driver::ready_input` is a documented **no-op** — the socket still registers read readiness, and the runtime sees the event and issues `recv`.
  This cannot mirror the C's code-60 `PACKET_REQ_RECV` byte for byte: that request accepts the `Timeout(4), Length(4)` payload, enqueues an async operation on the descriptor's `INET_MAX_ASYNC = 1` queue, and answers the three-byte async-op id `[1] ++ async_id_be16` (measured against the oracle as `[1,0,0]`, then `[1,0,1]` on the next call), the datagram itself arriving later as an `{inet_async, Port, Ref, Result}` message.
  Only the **empty** argument is `[0]einval`, which the C rejects without enqueuing anything.
  With no driver→owner message path, the floor replaces that whole asynchronous request/reply exchange with the synchronous read-now verb above.
- **Deliberate non-mirrors** (each chosen so the floor never leaks a socket or answers a wrong result): `open` of a second socket on a live port answers `[0]einval` where the C's `inet_ctl_open` answers `[0]exbadseq` (`:5130`), the same choice the TCP driver makes; `open` accepts only `INET_TYPE_DGRAM` (the C serves `SOCK_STREAM` from either entry point, being one driver file); a `getopts` on a socket-less port answers a well-formed reply where the C's `inet_fill_opts` can walk a `NULL` descriptor.
- **Ownership**: `open` creates the socket and `from_socket` adopts one; either is closed on `stop`, on the `close` verb and on `Drop`; nothing else may close it.
- **Absent by design** (a typed error, never a wrong result): `{active, …}` datagram delivery (see the handoffs below), ancillary data (`recvmsg`/`sendmsg` `IP_PKTINFO`/cmsg flags), the async-op queue, the `INET_REQ_PEER` query (code `4`), and IPv6/SCTP/Unix-domain sockets and DNS resolution.

### `wire.rs` (private) — the wire vocabulary
- Result tags `INET_REP_OK`(1)/`INET_REP_ERROR`(0); address tags `INET_AF_INET`(1)/`INET_AF_ANY`(3)/`INET_AF_LOOPBACK`(4); socket types `INET_TYPE_STREAM`(1)/`INET_TYPE_DGRAM`(2); `INET_PROTO_DEFAULT`(0)/`INET_PROTO_TCP`(1)/`INET_PROTO_UDP`(2); `TCP_SHUT_RD`/`TCP_SHUT_WR`/`TCP_SHUT_RDWR`.
- The reply builders `ctl_ok`/`ctl_error`/`ctl_xerror` (the C's `ctl_reply`/`ctl_reply_ok`/`ctl_error`), `InetAddr` + `parse_address` (the packed-address codec), `pack_address(port, addr)` (its encoder — the address a UDP `recv` reply and a `bind`/`connect` argument carry) and `split_address(arg)` (splits a packed address off the front of a `[address][payload]` argument, for the UDP `send`), and `errno_name` (the lowercase `erl_errno_id` renderer).
  The constants above are re-exported at `crate::drivers::inet::*`; these helpers stay `pub(super)` inside the subtree.

### `opt.rs` (private) — the socket-option vocabulary (`INET_REQ_SETOPTS`/`INET_REQ_GETOPTS`)
- **Wire forms**: `setopts` takes five-byte `[opt, val_be32]` records (`inet_set_opts`, `:6915-6918`; `while (len >= 5)`, so a trailing partial record is ignored) and answers `[1]`; the one **seven**-byte record is `{active, N}` (a trailing big-endian 16-bit `N`, `:6975-6987`).
  `getopts` takes one-byte codes (`while (len--)`, so an empty payload answers a bare `[1]`) and answers `[1] ++ [code, val_be32]*` **in request order** (`inet_fill_opts`, `:9141-9751`); an `{active, N}` read-back appends a two-byte counter (`:9179-9183`).
- **Syscall-backed codes** (a direct `setsockopt`/`getsockopt`): `0` `reuseaddr` (`SO_REUSEADDR`), `1` `keepalive` (`SO_KEEPALIVE`), `4` `broadcast` (`SO_BROADCAST`), `6` `sndbuf` (`SO_SNDBUF`), `7` `recbuf` (`SO_RCVBUF`), `10` `nodelay` (`TCP_NODELAY`), `46` `ttl` (`IP_TTL`).
  `recbuf`/`sndbuf` report the **kernel's** value (Linux doubles a request: 4096 ⇒ 8192, the oracle).
  The one `resolve` table serves both drivers, so `reuseaddr`/`broadcast`/`ttl` are served on a datagram socket too (the C's `INET_OPT_*` table is shared between its TCP and UDP halves).
- **Driver-state codes** (stored in `SocketOptions` and reported with the oracle's defaults; `packet`/`packet_size`/`line_delim` are now **acted on** on the TCP read path): `22` `{active, …}` (default `false`, + `active_count`), `23` `{packet, …}` (default `raw`), `24` `{mode, list|binary}` (default `list`), `25` `{deliver, port|term}` (default `term`), `30` `{send_timeout, …}` (default `infinity`, gettable only on a stream socket), `32` `{packet_size, …}` (default `0`), `33` `{read_packets, N}` (default `5`, datagram-only), `35` `{send_timeout_close, …}` (default `false`, stream-only), `40` `{line_delim, …}` (default `'\n'`, **set-only** — see below).
  The driver-state store is gated on the socket type: `{mode, …}`/`{deliver, …}` are served on a datagram socket (`!stream`), `{read_packets, N}` only there and with `N > 0` (a non-positive value is the C's `[0]einval`), and `{read_packets, …}` contributes no `getopts` record before a socket exists (`SOCK_DGRAM`-gated in the C).
- An **unserved** code is the C's whole-reply `[0] ++ "einval"` (never a partial reply), so a mixed request like `[keepalive, 200]` answers exactly that.
- **Six mirrored quirks**, each measured against `erl` byte-for-byte: (1) a configured buffer reports the kernel value; (2) `setopts` answers `[1]` even when the kernel rejects the option (the C's `propagate = 0`, `:7898`/`:7923-7942`); (3) neither verb has an `IS_OPEN` guard, so on a socket-less port `setopts` answers `[1]` and `getopts` drops a syscall-backed record but an unserved code is still `[0]einval`; (4) `getopts` of `line_delim` (`40`) is the C's `[0]einval` and replaces the **whole** reply (so a multi-code request including `40` answers only `[0]einval`), while `setopts` of `40` is served; (5) `send_timeout`/`send_timeout_close` are gated on `stype == SOCK_STREAM` (before `open` a `getopts` of them contributes no record, a `setopts` is ignored), and `stype` is set by `open` and not cleared by `close`; (6) `{active, N}` **adds** to the running counter (`:6987`), clamps at zero (falling back to `{active, false}`, `:6993-6998`), a sum outside the signed 16-bit range is the whole-reply `[0]einval`, and any other `{active, …}` value resets the counter.
- **Two deliberate non-mirrors**: an unrecoverable `getsockopt(2)` failure on a live socket is the typed `BeamError::Io` (the C silently drops the record and answers a shorter, wrong reply); a **truncated** `{active, N}` record (value but no trailing 16-bit `N`) is `BadArg` (the C reads past the payload).

### `packet.rs` (`pub`) — the pure packet-framing decoder
- `PacketType` — the C's `enum PacketParseType`; the `u8` discriminant **is** the `{packet, N}` wire code (`code()`/`from_code()`), `ALL: [PacketType; 15]`, `is_supported()`.
  Decoded: `Raw`(0), `Len1`(1), `Len2`(2), `Len4`(3), `Asn1`(4), `Rm`(5), `Cdr`(6), `Fcgi`(7), `LineLf`(8), `Tpkt`(9).
  **Unsupported**: the HTTP family `Http`(10)/`HttpH`(11)/`HttpBin`(13)/`HttpHBin`(14) and `SslTls`(12) — their bodies need the C's term-building `PacketCallbacks`.
- `PacketLength { Complete(usize), NeedMore, Invalid, Unsupported }` — the C's `packet_get_length` `> 0`/`0`/`< 0` plus an explicit "not implemented"; `is_complete()`/`complete()`/`as_error()` (`Invalid`⇒`badarg`, `Unsupported`⇒`notsup`).
  `Complete(len)` is the **total** length including the header, known as soon as the header suffices.
- `PacketOptions { packet, delimiter, max_plen, trunc_len }` — the four arguments of `packet_get_length`; `Default` = `{packet, raw}`, delimiter `DEFAULT_DELIMITER` (`\n`), no limits; builders `new`/`with_delimiter`/`with_max_plen`/`with_trunc_len`; `decode(&[u8]) -> PacketLength`.
  `max_plen == 0` means "no limit" (`INT_MAX - hlen`) and bounds the **payload** (`plen`), never the total frame.
- `PacketType::payload_range(&[u8]) -> Result<Range<usize>>` — the C's `packet_get_body` header strip: the header is removed for `Len1`/`Len2`/`Len4` and the FCGI padding subtracted for `Fcgi`; every other type (including `LineLf`) is returned **as is**, so a `{packet, line}` payload keeps its delimiter (the oracle's `gen_tcp:recv`).
- **Three faithfully-reproduced C quirks**: `Raw` ignores `max_plen` (`:258`); line mode compares `max_plen` strictly-but `trunc_len` inclusively before a delimiter is found, and both strictly after (`:298-320`); `Raw` with an empty buffer is `NeedMore`, not `Complete(0)` (`:258`).

### `frame.rs` (private) — the framing glue (read + send)
- The bridge from the driver's stored `SocketOptions` to the pure decoder, called by the driver's read and send paths; a port of the C's `tcp_remain`/`tcp_reply_data`/`tcp_send` pair.
- `options(&SocketOptions) -> PacketOptions` (the C's arguments at `inet_drv.c:12975`; `trunc_len` stays 0 — this driver has no fixed input buffer, it accumulates into a growable `Vec`); `framed(&SocketOptions) -> Option<PacketType>` (`None` only for `Raw` and an out-of-range code); `write_header(&SocketOptions, len) -> Option<Vec<u8>>` (the send-path counterpart of `extract` — the 1-/2-/4-byte big-endian length header the `send` verb prepends under `{packet,1|2|4}`, `None` for every other code); `extract(&mut Vec<u8>, &PacketOptions) -> Result<Option<Vec<u8>>>` — **exactly one** frame's payload per call, `Ok(None)` while more bytes are needed (buffer untouched), `Err(badarg)` for `Invalid`/`Err(notsup)` for `Unsupported`.
  All four are `pub(super)`.

### `sys.rs` (private) — the raw socket syscalls
- Every `unsafe` block of the driver lives here: `socket`/`bind`/`listen`/`accept`/`connect`/`getsockname`/`getsockopt`/`setsockopt`/`read`/`send`/`shutdown`/`close`, each with a `// SAFETY:` rationale.
  Also `get_socket_int_option`/`set_socket_int_option` (the syscall-backed option family).
- The datagram family the UDP driver needs shares the file: `dgram_socket_fd` (the `AF_INET`/`SOCK_DGRAM` socket), `bind_socket`, `connect_socket`, `send_to` (an unconnected socket, `sendto(2)`), `send_connected` (`send(2)` on a connected one) and `recv_from` (one `recvfrom(2)`, returning the payload length together with the source `InetAddr`), over the shared `has_peer`/`local_port`/`close_fd`.

## Constraints
- Errors are `beam_common::{Result, BeamError}` only: an operational socket failure ⇒ the `[0]`-tagged reply; a malformed argument / unknown verb ⇒ `BeamError::BadArg`; an I/O syscall failure ⇒ `BeamError::Io`; an undecodable packet type ⇒ the `badarg`/`notsup` raise.
  Never a wrong result, never a panic.
- **One `Driver` is one port**: no port-creation capability (the `accept` transfer + the port-creation deferral above).
- Safe Rust except `sys.rs`; `unsafe_op_in_unsafe_fn` stays clean; the `Driver`/`DriverContext` seam is frozen.
- Files target ~1000 lines; the suite files (`tests.rs`, `frame_tests.rs`, `udp/udp_tests.rs`) are in-module test modules split from their source half to keep each source file near the target.

## Handoffs (out of this node — the cross-crate seams `gen_tcp`/`gen_udp` still need)
All four need a change **outside** `src/drivers/inet/`; each is a real `gen_tcp` capability the floor refuses honestly today (bullet (a) is the one the datagram driver needs too).
- **(a) `{active, N|true|once}` / `{deliver, port|term}` / `active_count` delivery.** The option is stored and reported, but the driver never delivers input on its own: bytes land in the port input queue (or `pending_input` under framing) and the runtime decides when to fetch them.
  Needs a per-frame input drain plus a driver→owner send path — `DriverContext` reaches only the port id/poller/two byte queues, and a port-task round harvests its input in one step.
  **The datagram driver has exactly this gap, in its most visible form**: `udp_inet`'s `recv` is the only datagram path there is, so `{active, true|once|N}` never fires and `{udp, S, IP, Port, Data}` is never fabricated — a runtime that wants active mode must drive `recv` itself and build the tuple from the reply's source address, then install the missing driver→owner message path.
  **Do not add a variant to `PortNotification`**: `beam_rt` matches it exhaustively (two arms, no wildcard), so a third variant breaks a crate outside this node. The precedent for a per-frame channel is the `{line,N}` work: a **separate** frame struct (`LineNotification`) plus a drain accessor on the registry (`PortRegistry::take_line_notifications`), polled after a round.
- **(b) `send_timeout` / `send_timeout_close`.** Both are stored and reported but not acted on: the C arms a driver multi-timer (`add_multi_timer(..., &tcp_inet_send_timeout)`, `:13789`) that fires while output is queued and answers an **asynchronous** `{inet_reply, S, Status, Mref}`-shaped `{error, timeout}` (closing under `send_timeout_close`, `:12458-12482`).
  Needs a reply channel out of `Driver::timeout()` (its `Result<()>` is discarded by `Port::run_task`'s `Timeout` arm) + a pending-command slot + the `{inet_reply, …}` tag.
- **(c) `controlling_process`.** Not driver state: `inet:tcp_controlling_process/2` (`lib/kernel/src/inet.erl:4848`) is `erlang:port_connect(S, NewOwner)` + an Erlang-side drain of buffered `{tcp, S, Data}`/`{tcp_closed, S}`/`{inet_async, …}` messages; there is no `INET_REQ_*` and the `Driver` seam has no owner notion.
  Needs `beam_process::PortTable::set_owner` (exists, unused) + `beam_rt`'s `PortMeta.owner`.
- **(d) The async-op queue.** `INET_MAX_ASYNC = 1` (`:1069`), `enq_async`, the `[1] ++ async_id_be16` reply and the later `{inet_async, Port, Ref, Result}` message, plus a per-port 16-bit op ref and a **second-port** `accept`.
  This floor's `recv`/`accept`/`connect` are **synchronous** (which is also why `recv` rejects the C's 8-byte `Timeout(4), Length(4)` payload with `BadArg`, and `accept` transfers the port rather than returning an async id).

## Test Strategy
Gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_io` **GREEN** (sandbox `$HOME` read-only — always the wrapper; never `cd`).
- **In-module (147):** `packet.rs` **62**, `tests.rs` **29**, `udp/udp_tests.rs` **22**, `frame_tests.rs` **15**, `frame.rs` **13**, `opt.rs` **6**.
- `tests.rs` — the wire/reply vocabulary, argument decoding, the lifecycle and verb round trips over **real loopback sockets**, and the latched peer-close state machine.
- `udp/udp_tests.rs` — the datagram driver's verbs over real loopback datagram sockets (the open/bind/send/recv round trip with its source address, the connected-socket send, the option wire), the `udp_inet` name, and **two `erl`-oracle cross-checks**: the `open`/`bind`/`close` reply bytes and the oracle's `port_info(Port, name)`.
  The datagram oracle dump is compared as **raw bytes** (the reply carries the kernel-assigned ephemeral port, so the Erlang side switches `standard_io` to `latin1` and the Rust side never runs the output through `from_utf8_lossy`).
- `frame_tests.rs` — the `{packet, N}` read path over real loopback sockets, cross-checking the delivered payloads against the system `gen_tcp` oracle.
- **Crate-level integration `tests/inet.rs` (8):** a loopback byte round trip through a `"tcp_inet"` port created via the `DriverRegistry`; a stalled write arming write-readiness + an unknown verb's `BadArg`; the post-close `[0]closed` shape; a reactor-observed peer close releasing the registration; and **three `erl`-oracle cross-checks** (`the_covered_control_replies_…`, `the_peer_close_replies_…`, `the_socket_option_replies_…`) plus `the_driver_state_option_replies_agree_with_the_erl_oracle` (10 driver-state option replies byte-for-byte).
  Every oracle case **skips loudly** when `erl` is absent (30 s budget), never fails.
- **Crate-level integration `tests/udp_inet.rs` (5):** the datagram driver through the **real port layer** — `"udp_inet"` resolved out of `DriverRegistry::with_builtins`, opened as a `PortId` through `PortRegistry::open` and every verb issued over `Port::control`/`PortRegistry::port_control`, so the registration, the verb dispatch, the reactor round and the `PortNotification` channel are exercised together: a datagram sent from a **second real socket** surfacing through the reactor as the source-carrying eight-byte `recv` reply and then as the `PortNotification::Data` payload the runtime turns into `{Port, {data, Data}}` (the drained socket's bare `[1]` pinning the reply-length distinction); the read/write-readiness rounds (read readiness schedules an `INPUT` task the round runs, write readiness schedules nothing); an unknown verb's `BeamError::BadArg` plus the reserved `close` op closing the port and reported **exactly once** as a `PortNotification::Closed`; the `erl`-oracle cross-check of the covered control replies (`the_covered_control_replies_agree_with_the_erl_oracle`, only the `bind` reply shape-compared); and the code-60 divergence pin (`the_recv_verb_is_the_documented_divergence_from_the_oracles_code_60`), which asserts the oracle's async-op-id shape against this floor's read-now reply and fabricates no shared value.
  Its two `erl` cases **skip loudly** when `erl` is absent (30 s budget), never fails.

## Routing Table
- `mod.rs` → `TcpInetDriver`, `DRIVER_NAME`, `make_tcp_inet`, `InetState`, `PeerClose`, `pending_input`, the control-verb dispatch, the poller read/write paths, `accept`/`close`, module docs (the verb table + the absent-by-design list)
- `udp.rs` → `UdpInetDriver`, `UDP_DRIVER_NAME`, `make_udp_inet`, `DATAGRAM_CHUNK`, the datagram verb dispatch (`open`/`bind`/`connect`/`send`/`recv`/`close`/`setopts`/`getopts`), the `recv`-carries-the-source-address reception model, `stop`/`Drop`
- `wire.rs` → `INET_REP_*`/`INET_AF_*`/`INET_TYPE_*`/`INET_PROTO_*`/`TCP_SHUT_*`, `InetAddr`/`parse_address`/`pack_address`/`split_address`, `ctl_ok`/`ctl_error`/`ctl_xerror`, `errno_name`
- `opt.rs` → `SocketOptions`, `set_options`/`get_options`, `resolve`, the `INET_OPT_*`/`TCP_OPT_NODELAY`/`INET_LOPT_*`/`INET_ACTIVE`/`INET_INFINITY`/`INET_ONCE`/`INET_MULTI`/`INET_PASSIVE`/`INET_MODE_*`/`INET_DELIVER_*`/`INET_PACKET_POLL` codes
- `packet.rs` → `PacketType`/`PacketLength`/`PacketOptions`/`DEFAULT_DELIMITER`, the per-type decoders, `payload_range`, `mod tests` (62)
- `frame.rs` → `options`/`framed`/`write_header`/`extract`, `mod tests` (13)
- `sys.rs` → the raw `libc` socket wrappers + `get_socket_int_option`/`set_socket_int_option`; the datagram family `dgram_socket_fd`/`bind_socket`/`connect_socket`/`send_to`/`send_connected`/`recv_from`
- `tests.rs` → the in-module suite (29)
- `frame_tests.rs` → the in-module framing suite (15)
- `udp/udp_tests.rs` → the in-module datagram suite (22)
- `../../../tests/inet.rs` → the crate-level integration + `erl`-oracle suite (8) (parent node)
- `../../../tests/udp_inet.rs` → the crate-level datagram integration + `erl`-oracle suite (5) (parent node)

## See Also
- `../../CONTEXT.md` → the `beam_io` crate root: the `Driver`/`DriverContext` seam, `PortRegistry`, `Port`/`pub(crate) PortArms`, `DriverRegistry`.
- `../../driver.rs` → the frozen `Driver` trait (`control`/`ready_input`/`ready_output`/`next_timeout`) and `DriverRegistry::with_builtins()` (which registers the socket drivers `"tcp_inet"`/`"udp_inet"`).
- `../../../CONTEXT.md` → the crate-layer contracts (the `beam_io` seam types, the A.4 driver C-ABI contract).
- `../../../../erts/emulator/drivers/common/inet_drv.c` + `../../../../erts/emulator/beam/packet_parser.{c,h}` + `../../../../erts/preloaded/src/prim_inet.erl` → the legacy ground truth (read-only reference).
