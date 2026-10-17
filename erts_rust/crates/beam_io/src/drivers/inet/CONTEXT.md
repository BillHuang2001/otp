# `inet` driver subtree (`src/drivers/inet/`) — the built-in `tcp_inet` and `udp_inet` socket drivers

## Intent
The built-in **`tcp_inet`** driver: **one** non-blocking TCP socket behind one port — the Rust counterpart of the C driver `gen_tcp`/`inet_tcp` sit on (`erts/emulator/drivers/common/inet_drv.c`).
The built-in **`udp_inet`** driver (`udp.rs`) is the UDP half of the same C file: **one** `AF_INET`/`SOCK_DGRAM` socket behind one port, the driver `gen_udp` sits on.
Frozen registered names **`DRIVER_NAME = "tcp_inet"`** and **`UDP_DRIVER_NAME = "udp_inet"`** (the oracle's `port_info(Port, name)` values and the names `prim_inet` opens, `inet_drv.c:1348`/`:11834` for TCP and `inet_drv.c:1406` for UDP).
Registered with the crate's `DriverRegistry` through `make_tcp_inet()`/`make_udp_inet()` (their `DriverFactory`s); a port gets its socket from the `open` verb, or the driver adopts a caller's descriptor via `TcpInetDriver::from_socket`/`UdpInetDriver::from_socket`.
The TCP driver owns the **read-path packet framing** for `{packet, N}` — a faithful, term-free port of the C `packet_parser` (`packet_get_length`/`packet_get_body`).
Both drivers own the **numeric control-opcode decode** (`opcode.rs`): `erlang:port_control/3`'s integer reaches them as a decimal string and is turned into the C's `INET_REQ_*` case here; only the *operand* remains the runtime's to decode.
Legacy C ground truth (read-only): `erts/emulator/drivers/common/inet_drv.c`, `erts/emulator/beam/packet_parser.{c,h}`, `erts/emulator/beam/sys.h` (the magic number), the Erlang half `erts/preloaded/src/prim_inet.erl`.

## API Surface (FROZEN)

### `opcode.rs` — the numeric control-opcode bridge
The Erlang side never sends a control *verb*: `erlang:port_control(Port, Op, Data)` carries an **integer**, `beam_rt` forwards it to `beam_io::PortRegistry::port_control` as its **decimal string**, and that passes any non-`"close"` op verbatim to `Driver::control`. Both drivers therefore decode it here, before their named-verb match.
- `CONTROL_MAGIC_NUMBER: u32 = 0x03f1_a300` — `ERTS_INET_DRV_CONTROL_MAGIC_NUMBER` (`erts/emulator/beam/sys.h:1466`), subtracted at the top of both `tcp_inet_ctl` (`inet_drv.c:11958`) and `packet_inet_ctl` (`:14574`).
- `decode(command: &str) -> Option<Opcode>` — `Some` only for a **decimal** command (an optional leading `-`, then ASCII digits, accumulated **mod-2³²**, i.e. exactly the C's `(unsigned int)` conversion: `4294967297` ⇒ `1`, `-1` ⇒ `u32::MAX`), then `wrapping_sub(CONTROL_MAGIC_NUMBER)`; a code no table entry matches is `Opcode::Unknown`. A **non-decimal** command is `None`, so every named verb keeps its own path and reply byte-for-byte.
- **The wrapping is observable.** An `Op` below the magic number becomes a huge value matching no `case`, so it is *not* an opcode: measured `port_control(P, 1, <<1,2,0>>)` ⇒ `[0]exbadport`, not `INET_REQ_OPEN`, and `port_control(P, 99, <<>>)` ⇒ the same.
- `Scope { Tcp, Udp, Shared }` — the C's three switches: `tcp_inet_ctl` (`:11953`), `packet_inet_ctl` (`:14570`, the datagram driver) and the shared `inet_ctl` (`:10988`) that both delegate to. The numbers are **one** numberspace (`:805-834`), but each number is a `case` of exactly one switch; a number that is a case of *the other driver's* switch is a case of neither here ⇒ the C's `[0]exbadport`.
- `Opcode` — `Open`(1) `Close`(2) `Connect`(3) `Peer`(4) `Name`(5) `Bind`(6) `Setopts`(7) `Getopts`(8) `Getstat`(11) `Gethostname`(12) `Fdopen`(13) `Getfd`(14) `Gettype`(15) `Getstatus`(16) `Getservbyname`(17) `Getservbyport`(18) `Setname`(19) `Setpeer`(20) `Getiflist`(21) `Ifget`(22) `Ifset`(23) `Subscribe`(24) `Getifaddrs`(25) `Accept`(26) `Listen`(27) `Ignorefd`(28) `Getladdrs`(29) `Getpaddrs`(30) `TcpRecv`(42) `TcpUnrecv`(43) `TcpShutdown`(44) `TcpSendfile`(45) `PacketRecv`(60) `Unknown`; `ALL`/`code`/`from_code`/`scope`/`name`, plus `not_served() -> BeamError` — the typed `error:unsupported: control opcode \`X\` (<switch>) is not served by this driver floor` raise.

#### Served vs refused, per driver
`op` = `code + 0x03f1_a300`. "Served" means the decoded opcode is routed to the **same verb body** the named form uses (never a second implementation).
Every row is cross-checked byte-for-byte against the system `tcp_inet` port by the `erl` matrix in `opcode_tests.rs::oracle_matrix` (bare / opened / listening / closed states, one `erl` run).

**Served — the reply is exact.** The whole arm is implemented, success included.

| code | opcode | TCP | UDP | operand handling | reply |
|---|---|---|---|---|---|
| 1 | `Open` | served | served | `[AF, TYPE, PROTO]` | the `open` verb's |
| 2 | `Close` | served | served | — | `[1]` |
| 3 | `Connect` | served | served | `len >= 6`: `Timeout(4), Port(2), Address(N)`; shorter ⇒ `[0]einval` | the `connect` verb's |
| 4 | `Peer` | served | served | — | `[0]enotconn` unless connected, else the packed peer — the **fake** `peer_addr` when `SETPEER` installed one |
| 5 | `Name` | served | served | — | the packed local endpoint — the **fake** `name_addr` when `SETNAME` installed one, else `getsockname`; socket-less and no fake ⇒ `[0]ebadf` |
| 6 | `Bind` | served | served | packed address | `[1] ++ port_hi ++ port_lo` |
| 7 | `Setopts` | served | served | `[opt, val_be32]*` records | `[1]`, or the whole-reply `[0]einval` |
| 8 | `Getopts` | served | served | one-byte codes | `[1] ++ [opt, val_be32]*`, or `[0]einval` |
| 11 | `Getstat` | served | served | empty ⇒ `[1]`; a bad `INET_STAT_*` op ⇒ `badarg` (the C's `-1` return) | |
| 12 | `Gethostname` | served (`len != 0` ⇒ `[0]einval`) | typed gap | — | `[1] ++ gethostname(2)` |
| 14 | `Getfd` | served | served | — | `[1] ++ fd_be32`, socket-less ⇒ `[0]einval` |
| 15 | `Gettype` | served | served | — | the **cached** `[1,0,0,0,family_be32,type_be32]` — `[1,0,0,0,1,0,0,0,1]` for a stream, `…,2` for a datagram, and the cached pair **survives a `close`**; nothing cached ⇒ `[0]einval` |
| 16 | `Getstatus` | served | served | — | `[1,0,0,0,Code]` — `InetState::state_code()`: Closed 0, Open 1, Connected 5, Listening 9, Connecting 17 |
| 19 | `Setname` | served | typed gap | `len == 0` clears, `len == 1` ⇒ `[0]einval`, else `inet_set_faddress` | `[1]` (stored), or `[0]einval`/`[0]eafnosupport` |
| 20 | `Setpeer` | served | typed gap | as `Setname` | `[1]` (stored), or `[0]einval`/`[0]eafnosupport` |
| 26 | `Accept` | served (`len == 4`, else `[0]einval`) | `[0]exbadport` | `Timeout(4)`, validated then unused | the `accept` verb's |
| 27 | `Listen` | served | `[0]exbadport` (the C's arm is SCTP-only, `:14846`) | `[backlog_hi, backlog_lo]` | `[1]` |
| 29 | `Getladdrs` | served (`len != 4` ⇒ `[0]einval`, then `!IS_OPEN` ⇒ `[0]exbadport`) | typed gap | — | **byte-identical to the `NAME` reply** — the C's non-SCTP fallback calls `sock_name` and never reads `name_ptr` (`:11341`) |
| 30 | `Getpaddrs` | served (same guards) | typed gap | — | **byte-identical to the `PEER` reply**, likewise ignoring `peer_ptr` (`:11261`) |
| 42 | `TcpRecv` | served (connection guard **first**, then `active || len != 8` ⇒ `[0]einval`) | `[0]exbadport` | `Timeout(4), Length(4)`, validated then unused | the synchronous `recv` verb's |
| 43 | `TcpUnrecv` | served (connection guard first) | `[0]exbadport` | the bytes to push back | `[1]`; the operand is spliced to the **front** of `pending_input`, so the next `recv` yields it first |
| 44 | `TcpShutdown` | served (connection guard first, then `len != 1` ⇒ `[0]einval`) | `[0]exbadport` | the `TCP_SHUT_*` byte | the `shutdown` verb's |
| 60 | `PacketRecv` | `[0]exbadport` | served (socket-less ⇒ `[0]exbadport`; `len != 8` ⇒ `[0]einval`) | `Timeout(4), Length(4)`, validated then unused | the synchronous `recv` verb's |

**Refusal-only — the C's refusal is reproduced exactly for every state/operand that selects it; the *success* reply stays a typed gap.** Each row names the capability its success path would need.

| code | opcode | TCP / UDP | refusals served | the missing capability behind the success path |
|---|---|---|---|---|
| 13 | `Fdopen` | TCP refusal-only, UDP typed gap | `len` not 6/10 ⇒ `[0]einval`; family ≠ `INET_AF_INET` ⇒ `[0]eafnosupport`; type ≠ `INET_TYPE_STREAM` ⇒ `[0]einval` | adopting a caller's descriptor into the live port and re-registering it (`beam_rt` owns the `open_port({fd,N})` path today) |
| 17 | `Getservbyname` | TCP refusal-only, UDP typed gap | `len < 2` ⇒ `[0]einval` | the `getservbyname(3)` lookup and its term-encoded service record |
| 18 | `Getservbyport` | TCP refusal-only, UDP typed gap | `len < 3` ⇒ `[0]einval` | the `getservbyport(3)` lookup (same record shape) |
| 21, 22, 23, 25 | `Getiflist` / `Ifget` / `Ifset` / `Getifaddrs` | TCP refusal-only, UDP typed gap | `!IS_OPEN` ⇒ `[0]exbadport` | a `getifaddrs(3)` interface walk and the C's address-record encoding (`:11067`-`:11109`) |
| 28 | `Ignorefd` | TCP refusal-only, UDP typed gap | `!IS_CONNECTED` ⇒ `[0]enotconn`; `stype != SOCK_STREAM` ⇒ `[0]einval` | "suppress read **and** write readiness" — this driver keeps one flipped `Interest` mask, so it could not honour the reply it gave |
| 45 | `TcpSendfile` | TCP refusal-only, UDP `[0]exbadport` | `len != 20` ⇒ `[0]einval` (**before** the connection check); then `!IS_CONNECTED` ⇒ `[0]enotconn` | `sendfile(2)` plumbing plus the C's dup-file/offset bookkeeping (`:12380`) |

**Typed gap in every state** — no refusal is modelled because the C has no refusal to reproduce:

| code | opcode | why |
|---|---|---|
| 24 | `Subscribe` | its only op registers a driver-side subscriber for the empty-output-queue notification (`inet_subscribe`, `:10819`) — a driver→owner message channel this crate's `Driver`/`PortNotification` seam does not have. `Err(Opcode::not_served())` rather than a `[1]` promise that would never be kept. |

**Neither switch owns the number** ⇒ `[0]exbadport` for both drivers: `Unknown`, a wrapped/sub-magic `Op`, and a number that is a case of *the other* driver's switch (`PACKET_REQ_RECV` on TCP, `TCP_REQ_*`/`Accept` on UDP). On the datagram driver `Listen`(27) also lands here (`[0]exbadport`, SCTP compiled out).

A refused opcode is **never** a fabricated success and never an invented number, and every named verb keeps answering exactly what it answered before (`tests::*` pin those replies byte-for-byte). The five unserved shared arms (`Gethostname`/`Fdopen`/`Getserv{by,byname}`/… on **UDP**) are flat typed gaps there — the datagram driver models none of their refusals.

### `mod.rs` — the driver
- `DRIVER_NAME: &str = "tcp_inet"`; `make_tcp_inet() -> Result<Box<dyn Driver>>` (the `DriverFactory`).
- `TcpInetDriver` (`Driver::name() == "tcp_inet"`) — one listening or connected socket; `new()` (socket-less, the registry factory; the `open` verb creates the socket, mirroring `tcp_inet_start` + `INET_REQ_OPEN`), `from_socket(fd)` (adopts a descriptor and probes kernel state: `SO_ACCEPTCONN`⇒Listening, `getpeername`⇒Connected, else Open).
- Accessors: `socket() -> Option<RawFd>`, `state() -> InetState`, `eof() -> bool` (read EOF latched), `peer_closed() -> bool` (the peer-close latch), `connect_error() -> Option<&str>` (the late `SO_ERROR` of a non-blocking `connect`, a lowercase errno name).
- `InetState { Closed, Open, Listening, Connecting, Connected }` (`is_connected`/`is_listening`, `state_code()`) — mirrors the C descriptor's `INET_STATE_*`: no socket until `open`; `listen` makes a listener; `connect`/`accept` make a connection.
- The **fake endpoint** pair — `name_addr: Option<InetAddr>` / `peer_addr: Option<InetAddr>`, the C's `desc->name_ptr`/`peer_ptr` (`inet_drv.c:1269`/`:1268`). Installed/cleared by `Opcode::Setname`/`Setpeer` through `wire::parse_fake_address`, reported by `Opcode::Name` (in place of `getsockname`) and `Opcode::Peer` (**after** its connection guard, in place of `getpeername`), and deliberately **not** read by `Opcode::Getladdrs`/`Getpaddrs` (the C's non-SCTP fallbacks call `sock_name`/`sock_peer` directly). Like the C's pointers they are cleared only by the `len == 0` form — a `close` leaves them installed.
- `PeerClose` (`pub(crate)`-style private) — the C's `TCP_ADDF_DELAYED_CLOSE_RECV`/`_SEND` bookkeeping (`inet_drv.c:855-856`): after a peer close the **first** `recv` and the **first** `send` answer `closed`, every later one `enotconn` (`:12798-12819`, `:13662-13684`).
- **Control verbs** — `Driver::control` takes the C's packed request payload as raw bytes in **both** wire forms: a decimal command is an [`opcode`](#opcoders--the-numeric-control-opcode-bridge)-decoded numeric opcode, any other command is one of the names below. The named verb and the C request each mirrors:

| verb | C request | argument | reply |
|---|---|---|---|
| `open` | `INET_REQ_OPEN` (`:11961`) | `[AF, TYPE, PROTO]` (3 bytes) | `[1]` |
| `bind` | `INET_REQ_BIND` (`:11409`) | packed address | `[1, port_hi, port_lo]` |
| `listen` | `INET_REQ_LISTEN` (`:12042`) | `[backlog_hi, backlog_lo]` | `[1]` |
| `accept` | `INET_REQ_ACCEPT` (`:12144`) | — | `[1, fd_be32]`, or `[1]` when nothing is pending |
| `connect` | `INET_REQ_CONNECT` (`:12064`) | packed address | `[1]` |
| `send` | `tcp_inet_command` (`:12629`) | the bytes to send | `[1]` |
| `recv` | `TCP_REQ_RECV` (`:12268`) | — (a **non-empty** argument is `BadArg`; the numeric route validates its 8-byte payload and forwards `&[]`) | `[1]` |
| `shutdown` | `TCP_REQ_SHUTDOWN` (`:12339`) | `[how]` | `[1]` |
| `close` | `INET_REQ_CLOSE` (`:12259`) | — | `[1]` |
| `setopts` | `INET_REQ_SETOPTS` (`:11111`) | option records `[opt, val_be32]*` | `[1]`, or `[0] ++ "einval"` |
| `getopts` | `INET_REQ_GETOPTS` (`:11046`) | option codes `[opt]*` | `[1] ++ [opt, val_be32]*`, or `[0] ++ "einval"` |

- **Reply shape** = the C's `ctl_reply` (`:2260-2311`): a one-byte result tag (`INET_REP_OK` 1 / `INET_REP_ERROR` 0, `:869-871`) + payload; `[1]` on success (a `port_be16`/`fd_be32` operand where the C has one), else `[0]` + a lowercase errno name (`erl_errno_id`, `:2249`) or a fixed atom (`einval`/`enotconn`/`eisconn`/`exbadport`/`ebadf`).
- **Operational** failures are the `[0]`-tagged reply; a **malformed argument** or an **unknown (non-numeric) verb** is `BeamError::BadArg` (the C's `ctl_error(EINVAL)` for the former).
- **Packed address** (`bind`/`connect`, the C's `inet_set_faddress`/`inet_set_address` `:4795`/`:4706`): `[INET_AF_INET(1), port_hi, port_lo, a, b, c, d]` (7 bytes), or `[INET_AF_ANY(3) | INET_AF_LOOPBACK(4), port_hi, port_lo]` (3 bytes); any other family tag is `BadArg` (DNS/IPv6 are out of scope).
- **Readiness & flow control**: the poller is edge-triggered (`EPOLLET`), so every callback drains to `EAGAIN`.
  A socket is **one** descriptor ⇒ **one** registration whose `Interest` mask is flipped: readable while connected, plus writable while the output queue is non-empty (a short write keeps the remainder queued and **arms** write readiness) and while an async `connect` completes; a drained queue drops writability.
  Output still queued when a peer close is latched is dropped (the C's `tcp_clear_output`, `:12786`).
  Writes use `send(2)` with `MSG_NOSIGNAL`.
- **A command on a port with no connection is refused, not dropped.** `output`/`ready_output` → `write_pending` → `connected_socket()?` answers the typed `BeamError::BadArg` and the queued bytes are **never consumed** (so a re-driven `OUTPUT` task refuses identically instead of losing data). That `Err` reaches `erlang:port_command/2`'s caller through `Port::check_write_accounting`'s `result?` → `PortRegistry::execute_port_tasks` → `beam_rt`'s `write_command`. C ground truth: `tcp_inet_command` (`:12629`) with `init_caller` (`:1854`) and the `IS_CONNECTED` guard (`:12645`); see *Known Issues* for the two oracle shapes this cannot reproduce.
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
- **Control verbs** — the same two wire forms as the TCP driver (a decimal command is an [`opcode`](#opcoders--the-numeric-control-opcode-bridge) opcode; anything else is a named verb), with the C request each named verb mirrors:

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
  The numeric route validates that 8-byte payload and then takes the same synchronous read-now path, so it answers a bare `[1]` (or `[1] ++ src_address`) instead of an op id.
  With no driver→owner message path, the floor replaces that whole asynchronous request/reply exchange with the synchronous read-now verb above.
- **A command with no destination peer is refused, not dropped.** `write_pending` answers the typed `BeamError::BadArg` with the queued bytes left **unconsumed** — both for a socket-less port and for an opened-but-unconnected one (a bare command carries no `[address][payload]` grammar, so nothing in the queue can name a peer). C ground truth: `packet_inet_command`'s `!IS_OPEN` guard (`:15051`).
- **Deliberate non-mirrors** (each chosen so the floor never leaks a socket or answers a wrong result): `open` of a second socket on a live port answers `[0]einval` where the C's `inet_ctl_open` answers `[0]exbadseq` (`:5130`), the same choice the TCP driver makes; `open` accepts only `INET_TYPE_DGRAM` (the C serves `SOCK_STREAM` from either entry point, being one driver file); a `getopts` on a socket-less port answers a well-formed reply where the C's `inet_fill_opts` can walk a `NULL` descriptor.
- **Ownership**: `open` creates the socket and `from_socket` adopts one; either is closed on `stop`, on the `close` verb and on `Drop`; nothing else may close it.
- **Absent by design** (a typed error, never a wrong result): `{active, …}` datagram delivery (see the handoffs below), ancillary data (`recvmsg`/`sendmsg`/`IP_PKTINFO` cmsg flags), the async-op queue, the `INET_REQ_PEER`/`INET_REQ_NAME` **named**-verb forms (the numeric `Peer`(4)/`Name`(5) opcodes ARE served), and IPv6/SCTP/Unix-domain sockets and DNS resolution.

### `wire.rs` (private) — the wire vocabulary
- Result tags `INET_REP_OK`(1)/`INET_REP_ERROR`(0); address tags `INET_AF_INET`(1)/`INET_AF_ANY`(3)/`INET_AF_LOOPBACK`(4); socket types `INET_TYPE_STREAM`(1)/`INET_TYPE_DGRAM`(2); `INET_PROTO_DEFAULT`(0)/`INET_PROTO_TCP`(1)/`INET_PROTO_UDP`(2); `TCP_SHUT_RD`/`TCP_SHUT_WR`/`TCP_SHUT_RDWR`.
- The reply builders `ctl_ok`/`ctl_error`/`ctl_xerror` (the C's `ctl_reply`/`ctl_reply_ok`/`ctl_error`), `InetAddr` + `parse_address` (the packed-address codec), `pack_address(port, addr)` (its encoder — the address a UDP `recv` reply and a `bind`/`connect` argument carry) and `split_address(arg)` (splits a packed address off the front of a `[address][payload]` argument, for the UDP `send`), and `errno_name` (the lowercase `erl_errno_id` renderer; `EBADF` ⇒ `"ebadf"`).
- The **shared `inet_ctl` reply bodies**, so no driver hand-builds one: `getstat_reply(&[u8])`, `gettype_reply(Option<RawFd>, stream: bool)`, `getfd_reply(Option<RawFd>)`, `getstatus_reply(state_code: u32)`, `name_reply(Option<RawFd>)`, `peer_reply(Option<RawFd>, connected: bool)` — each reproduces the C arm exactly, including the socket-less refusals (`einval` for `GETTYPE`/`GETFD`, `ebadf` for `NAME`, `enotconn` for `PEER`).
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
  Also `get_socket_int_option`/`set_socket_int_option` (the syscall-backed option family), and `local_address(fd)`/`peer_address(fd)` — one `getsockname(2)`/`getpeername(2)` into a `sockaddr_storage`, decoded by a private `decode_inet_address` (**`AF_INET` only**; any other family is the C's failure, so `INET_REQ_NAME`/`INET_REQ_PEER` answer their errno reply rather than a wrong address).
- The datagram family the UDP driver needs shares the file: `dgram_socket_fd` (the `AF_INET`/`SOCK_DGRAM` socket), `bind_socket`, `connect_socket`, `send_to` (an unconnected socket, `sendto(2)`), `send_connected` (`send(2)` on a connected one) and `recv_from` (one `recvfrom(2)`, returning the payload length together with the source `InetAddr`), over the shared `has_peer`/`local_port`/`close_fd`.

## Constraints
- Errors are `beam_common::{Result, BeamError}` only: an operational socket failure ⇒ the `[0]`-tagged reply; a malformed argument / unknown named verb ⇒ `BeamError::BadArg`; an I/O syscall failure ⇒ `BeamError::Io`; an undecodable packet type ⇒ the `badarg`/`notsup` raise; a control opcode the C serves but this floor does not ⇒ `Err(Opcode::not_served())`.
  Never a wrong result, never a panic.
- **One `Driver` is one port**: no port-creation capability (the `accept` transfer + the port-creation deferral above).
- Safe Rust except `sys.rs`; `unsafe_op_in_unsafe_fn` stays clean; the `Driver`/`DriverContext` seam is frozen.
- Files target ~1000 lines; the suite files (`tests.rs`, `opcode_tests*.rs`, `frame_tests.rs`, `udp/udp_tests.rs`) are in-module test modules split from their source half to keep each source file near the target.

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
  This floor's `recv`/`accept`/`connect` are **synchronous** — which is why `TCP_REQ_RECV`/`PACKET_REQ_RECV` validate the C's 8-byte `Timeout(4), Length(4)` payload and *ignore* it, `ACCEPT` validates its 4-byte timeout and ignores it, and `accept` transfers the port rather than returning an async id.

## Known Issues
- **Two oracle shapes for a socket-less command are unreachable, and the third is a real divergence.** Measured on a bare `{spawn_driver,"tcp_inet"}` port: `port_command(P,<<0,0,1>>)` (a payload carrying the 2-byte caller-ref prefix a distribution-style command needs) ⇒ the oracle answers `true`, the port stays **alive**, and `{error,enotconn}` is delivered out of band (`inet_drv.c:12645` — the real `IS_CONNECTED` guard); `port_command(P,<<1>>)` (no prefix — what a plain CLI probe sends) ⇒ the oracle answers `true` and then the port is **dead** (`port_info(P, name)` is `undefined`, `init_caller` (`:1854`) → `driver_failure_posix(EINVAL)`, `:12634`), every later command raising `badarg`.
  Neither shape is reachable through the frozen `Driver` trait (no `driver_failure` seam, no asynchronous `{inet_reply, …}` channel), so the driver answers the typed `BeamError::BadArg` with the bytes retained and `erlang:port_command/2` raises. That is the honest driver-side refusal, and it is what the divergence record should describe — not "the oracle raises `badarg` on a socket-less port".
- **The async-op queue does not exist, so `recv`/`recv`-alike answer a synchronous read-now.** The C's `TCP_REQ_RECV`(42), `PACKET_REQ_RECV`(60) and `INET_REQ_ACCEPT`(26) each enqueue an operation on the descriptor's `INET_MAX_ASYNC = 1` queue and answer an op id — `[1] ++ async_id_be16` for the two receives (`inet_drv.c:12268`-`:12322`, `:14968`-`:14990`) — with the result arriving later as an `{inet_async, Port, Ref, Result}` message. This floor has no driver→owner message channel, so all three validate their operand exactly as the C does and then take a **synchronous** path: `recv` answers the plain `[1]` (the bytes in the port input queue) and `accept` answers `[1] ++ fd_be32` (the transferred descriptor). Behaviourally visible only to a caller that inspects the reply tag/length; the delivered data is identical.
- **`INET_REQ_FDOPEN`(13) never adopts a descriptor, `INET_REQ_IGNOREFD`(28) never suppresses readiness, `TCP_REQ_SENDFILE`(45) never sends a file, and `GETIFLIST`/`IFGET`/`IFSET`/`GETIFADDRS`(21/22/23/25) never walk the interfaces** — each models the C's refusals exactly (wrong length / wrong family / `!IS_OPEN` / `!IS_CONNECTED`) and returns `Err(Opcode::not_served())` for the success path, naming the missing capability. `GETSERVBYNAME`(17)/`GETSERVBYPORT`(18) do the same for their `len` refusals.
- **`SUBSCRIBE`(24) is a typed gap in every state** — its only op registers a driver-side subscriber for the empty-output-queue notification (`inet_subscribe`, `:10819`), i.e. a driver→owner message channel this crate's `Driver`/`PortNotification` seam has no room for. Answering the C's `[1] ++ op ++ be32(outq)` without ever sending that notification would be a wrong promise.
- **The fake endpoint is a *reporting* substitution only.** `SETNAME`/`SETPEER` store an `InetAddr` that `NAME`/`PEER` then render; nothing binds, connects or resolves to it, and `GETLADDRS`/`GETPADDRS` keep asking the kernel — exactly the C's split (`desc->name_ptr`/`peer_ptr` are read only by `NAME`/`PEER`, `:11365`, `:11270`).
- **File sizes (`wc -l`, this subtree).** Over the baseline: `tests.rs` **1789** (a source-half-less
  suite, the documented split-from-source pattern) and `packet.rs` **1245** (one decoder per
  `{packet, N}` type — splitting it would scatter a single table). At/over 1000 in `mod.rs` **1484**
  (the driver + its verb table + docs; its suite already lives in `tests.rs`). Under 1000:
  `opt.rs` 937, `opcode_tests.rs` 886, `udp.rs` 810, `frame_tests.rs` 769, `udp/udp_tests.rs` 647,
  `sys.rs` 532, `opcode.rs` 488, `opcode_tests_udp.rs` 448, `frame.rs` 387, `wire.rs` 377.
  If a source half grows, split its test module into a sibling file — the patterns here are
  `opcode_tests.rs`/`opcode_tests_udp.rs` and `udp/udp_tests.rs`.
- Non-obvious framing divergences recorded in code: an out-of-range `{packet,N}` code is treated as **no framing** where the C's `packet_get_length` `default:` rejects it (`inet/frame.rs:96-105`); an unknown packed-address family is `badarg` where the C answers `eafnosupport` (`inet/wire.rs`).

## Test Strategy
Gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_io` **GREEN** (sandbox `$HOME` read-only — always the wrapper; never `cd`). The crate is at **718 passed / 0 failed / 2 ignored** (518 lib + 197 across the 23 `tests/*.rs` suites + 3 doctests; 2 doctests `ignore`d).
- **In-module, this subtree — 188 tests:** `packet.rs` **62**, `tests.rs` **33**, `udp/udp_tests.rs` **25**, `opcode_tests.rs` **16**, `frame_tests.rs` **15**, `frame.rs` **13**, `opcode_tests_udp.rs` **11**, `opcode.rs` **7**, `opt.rs` **6**.
- `tests.rs` — the wire/reply vocabulary, argument decoding, the lifecycle and verb round trips over **real loopback sockets**, the latched peer-close state machine, and the socket-less / unconnected / connected **port-command** contract (`Driver::output` **and** `Driver::ready_output`).
- `opcode_tests.rs` / `opcode_tests_udp.rs` — the **numeric opcode bridge** driven through a **real `PortRegistry` port** with `PortRegistry::port_control(port, &opcode.to_string(), &arg)` (the runtime's own route): the lifecycle verbs, `Gettype`/`Getstat`/`Getstatus`/`Getfd`/`Peer`, the operand validation of `Connect`/`Accept`/`TcpRecv`/`PacketRecv`, the cross-driver `[0]exbadport` refusals, the wrapped/sub-magic (`"1"`, `"99"`) refusals, the typed `not_served` gaps, **named-vs-numeric reply equality**, and one `erl` **opcode matrix per driver** (41 compared rows) cross-checked byte-for-byte.
- `udp/udp_tests.rs` — the datagram driver's verbs over real loopback datagram sockets (the open/bind/send/recv round trip with its source address, the connected-socket send, the option wire), the socket-less/unconnected command refusals, the `udp_inet` name, and **two `erl`-oracle cross-checks** (the open/bind/close reply bytes and the oracle's `port_info(Port, name)`).
- `frame_tests.rs` — the `{packet, N}` read path over real loopback sockets, cross-checking the delivered payloads against the system `gen_tcp` oracle.
- **Crate-level integration `tests/inet.rs` (8):** a loopback byte round trip through a `"tcp_inet"` port created via the `DriverRegistry`; a stalled write arming write-readiness + an unknown verb's `BadArg`; the post-close `[0]closed` shape; a reactor-observed peer close releasing the registration; and **three `erl`-oracle cross-checks** (`the_covered_control_replies_…`, `the_peer_close_replies_…`, `the_socket_option_replies_…`) plus `the_driver_state_option_replies_agree_with_the_erl_oracle`.
- **Crate-level integration `tests/udp_inet.rs` (5):** the datagram driver through the **real port layer** — a datagram surfacing as the source-carrying eight-byte `recv` reply and then as a `PortNotification::Data`; the read/write-readiness rounds; an unknown verb's `BadArg` plus the reserved `close`; the `erl` cross-check of the covered control replies; and the code-60 async-op-id divergence pin.
- **Every `erl`/`cc` oracle case skips loudly** (`SKIP:` banner + early `return`) when the tool is absent or exceeds its 30 s budget, never fails; comparisons are **byte-exact** except where a reply carries an ephemeral port number or an fd (then tag/length/family/octets are compared and the doc says why).

## Routing Table
- `mod.rs` → `TcpInetDriver`, `DRIVER_NAME`, `make_tcp_inet`, `InetState` (+`state_code`), `PeerClose`, `pending_input`, the `control` two-form dispatch + `control_opcode` (the TCP half), the poller read/write paths, `write_pending` (the no-connection refusal), `accept`/`close`, module docs (the verb table + the absent-by-design list)
- `opcode.rs` → `CONTROL_MAGIC_NUMBER`, `Scope`, `Opcode` (+`code`/`from_code`/`scope`/`name`/`not_served`/`ALL`), `decode`, `decimal_to_u32` (the wrapping mod-2³² decimal decode)
- `udp.rs` → `UdpInetDriver`, `UDP_DRIVER_NAME`, `make_udp_inet`, `DATAGRAM_CHUNK`, the datagram `control` two-form dispatch + `control_opcode` (the UDP half), the `recv`-carries-the-source-address reception model, `write_pending` (the no-peer refusal), `stop`/`Drop`
- `wire.rs` → `INET_REP_*`/`INET_AF_*`/`INET_TYPE_*`/`INET_PROTO_*`/`TCP_SHUT_*`, `InetAddr`/`parse_address`/`pack_address`/`split_address`, `ctl_ok`/`ctl_error`/`ctl_xerror`, `errno_name`, and the shared reply builders `getstat_reply`/`gettype_reply`/`getfd_reply`/`getstatus_reply`/`name_reply`/`peer_reply`
- `opt.rs` → `SocketOptions`, `set_options`/`get_options`, `resolve`, the `INET_OPT_*`/`TCP_OPT_NODELAY`/`INET_LOPT_*`/`INET_ACTIVE`/`INET_INFINITY`/`INET_ONCE`/`INET_MULTI`/`INET_PASSIVE`/`INET_MODE_*`/`INET_DELIVER_*`/`INET_PACKET_POLL` codes
- `packet.rs` → `PacketType`/`PacketLength`/`PacketOptions`/`DEFAULT_DELIMITER`, the per-type decoders, `payload_range`, `mod tests` (62)
- `frame.rs` → `options`/`framed`/`write_header`/`extract`, `mod tests` (13)
- `sys.rs` → the raw `libc` socket wrappers + `get_socket_int_option`/`set_socket_int_option` + `local_address`/`peer_address`; the datagram family `dgram_socket_fd`/`bind_socket`/`connect_socket`/`send_to`/`send_connected`/`recv_from`
- `tests.rs` → the in-module suite (33)
- `opcode_tests.rs` → the numeric-opcode suite + `erl` opcode matrix for `tcp_inet` (16), and the shared matrix helpers the UDP sibling imports
- `opcode_tests_udp.rs` → the numeric-opcode suite + `erl` opcode matrix for `udp_inet` (11)
- `frame_tests.rs` → the in-module framing suite (15)
- `udp/udp_tests.rs` → the in-module datagram suite (25)
- `../../../tests/inet.rs` → the crate-level integration + `erl`-oracle suite (8) (parent node)
- `../../../tests/udp_inet.rs` → the crate-level datagram integration + `erl`-oracle suite (5) (parent node)

## See Also
- `../../CONTEXT.md` → the `beam_io` crate root: the `Driver`/`DriverContext` seam, `PortRegistry`, `Port`/`pub(crate) PortArms`, `DriverRegistry`.
- `../../driver.rs` → the frozen `Driver` trait (`control`/`ready_input`/`ready_output`/`next_timeout`) and `DriverRegistry::with_builtins()` (which registers the socket drivers `"tcp_inet"`/`"udp_inet"`).
- `../../../CONTEXT.md` → the crate-layer contracts (the `beam_io` seam types, the A.4 driver C-ABI contract).
- `../../../../erts/emulator/drivers/common/inet_drv.c` + `../../../../erts/emulator/beam/packet_parser.{c,h}` + `../../../../erts/emulator/beam/sys.h` + `../../../../erts/preloaded/src/prim_inet.erl` → the legacy ground truth (read-only reference).
