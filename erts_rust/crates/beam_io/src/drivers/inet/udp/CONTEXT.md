# `udp_inet` — the datagram half of the built-in inet driver

## Intent
The built-in `udp_inet` datagram socket driver — the Rust counterpart of the legacy C driver's UDP half (`erts/emulator/drivers/common/inet_drv.c`'s `udp_inet_start`/`packet_inet_start`/`packet_inet_ctl`/`packet_inet_command`/`packet_inet_input`/`packet_inet_stop`), i.e. the driver `gen_udp`/`inet_udp` sit on.
It owns **one** `AF_INET`/`SOCK_DGRAM` socket behind one port, registers it with the poller for read readiness, and moves datagrams between socket and port.
The C keeps TCP and UDP in one file sharing every control-verb decoder; this floor splits the *drivers* (`../` = `tcp_inet`, `../udp.rs` = `udp_inet`) while keeping the vocabulary single-sourced — `../wire.rs` (reply/errno classes, packed addresses), `../opt*.rs` (the option wire), `../opcode.rs` (the numeric bridge), `../packet.rs`/`../frame.rs` and `../sys.rs` are shared, so a change there changes both drivers.
This node owns the driver's two behaviour **halves** — `active.rs` (the `{active,…}` delivery) and `async.rs` (`PACKET_REQ_RECV`(60) + the send path's completion) — plus their suites; the driver body is `../udp.rs`.

## API Surface

### Names, constants, construction
- `UDP_DRIVER_NAME = "udp_inet"` (the registered name, what `port_info(Port, name)` reports); `make_udp_inet() -> Result<Box<dyn Driver>>` is the `DriverFactory` (a plain `fn`, so it captures nothing and builds a fresh socket-less driver).
- `DATAGRAM_CHUNK = 64 * 1024` — one `recv` buffer, larger than the largest IPv4 UDP payload (65507), so a datagram is never truncated.
- `new()` — socket-less (`InetState::Closed`, `family = stype = 0`); the `open` verb creates the socket.
- `from_socket(fd)` — adopts **owned**, probing the kernel (`has_peer` ⇒ `Connected`, else `Open`) and caching `family = INET_AF_INET`/`stype = socket_type(fd)`; a probe failure is the typed `BeamError::Io` (there is no port yet to answer).
- `adopt_prebound(ctx, fd, stype)` — the `INET_REQ_FDOPEN` form, **borrowed**: `prebound` is set, so `close_socket` returns early, `Drop` skips it, and a `set_nonblocking`/poller failure leaves the caller's descriptor open; admits both `INET_TYPE_DGRAM` and `INET_TYPE_STREAM` (the latter promoted to `Connected` only with a peer).

### Capability word, flush, teardown
- `Driver::flags()` is not overridden, so it is **`DriverFlags::empty()`** — the C entry declares only `ERL_DRV_FLAG_USE_PORT_LOCKING` and no `SOFT_BUSY`; consequently `erlang:port_command(U, D, [force])` is the oracle's `error:notsup`, never `true` (pinned beside `ram_file_drv` in `../soft_busy_tests.rs`).
- `Driver::flush` is **not** overridden either: the C entry's `flush` slot is `NULL` (`inet_drv.c`'s `udp_inet` `erl_drv_entry`, the position `tcp_inet` fills with `tcp_inet_flush`), so a close keeps queued output and the port layer's `PortState::Closing` drain writes it.
- `stop` is `packet_inet_stop`: answer every pending operation `{error, closed}` (`drain_pending_closed`) **before** `release_socket`, so a parked caller is never left waiting; `Drop` closes an owned socket and nothing else.

### Control verbs (`Driver::control`)
- Both wire forms reach one body: a decimal command decodes through `../opcode.rs` (mod-2³² accumulation, wrapping subtraction of `CONTROL_MAGIC_NUMBER`) onto `control_opcode`, any other command is the **named** verb of the same row, so numeric and named routes answer identical bytes.
- A reply is the C's `ctl_reply` — a one-byte tag (`[1]`/`[0]`) plus payload; an operational failure is the `[0]`-tagged reply with the lowercase errno name, a malformed argument or unknown named verb is `BeamError::BadArg`.
- `open` — `[family, type, protocol]`; a socket already present ⇒ `[0]exbadseq`, a non-`INET_AF_INET` family ⇒ `eafnosupport`, a bad protocol ⇒ `eprotonosupport`, a type other than `INET_TYPE_DGRAM` ⇒ `einval`. It caches `family`/`stype`, which is what `GETTYPE` reports for the port's whole life (a `close` does not clear them).
- `bind` — any state but `Open` ⇒ `exbadport`; answers `[1, port_hi, port_lo]` with the bound (possibly kernel-assigned) port.
- `connect` — no socket ⇒ `exbadport`; an **empty operand dissolves** the peer back to `Open`; `EISCONN` reads as "already connected"; the C's 4-byte timeout prefix is accepted and skipped.
- `send` — the three wire forms below; `[1]` or `[0] ++ errno`.
- `getopts`/`setopts` — the shared `../opt.rs` record walk; `setopts` goes through `setopts_with_effects`, the `DriverContext`-carrying form, because that walk also re-arms read readiness and can stage a `Passive`.
- `recv` — this floor's **read-now** convenience (the C has no such control op): one `recvfrom(2)`; a datagram pending ⇒ its payload onto the port input queue plus the **eight**-byte `[1, INET_AF_INET, port_hi, port_lo, a, b, c, d]`; none pending (`EAGAIN`) ⇒ the **bare** `[1]`, so the reply length distinguishes the two. No socket ⇒ `einval`; a non-empty operand ⇒ `BadArg`; a real errno ⇒ `[0] ++ errno_name`.
- `close` — the C's `INET_REQ_CLOSE`, reachable by a direct caller only (the port layer reserves `"close"` for its own teardown); it answers `[1]` and, like the C, discards nothing from the output queue, because a datagram write cannot block on a peer's window the way a stream write can.

### Two reception paths, and why
- A datagram's **source address** cannot ride the port's input queue (`ByteQueue` is byte-only) and `PortNotification` keeps exactly its two variants, so the address rides only the asynchronous path.
- `PACKET_REQ_RECV`(60) (`async.rs`) is the C's own request: the control reply is `[1] ++ <<Id:16/big>>` and **nothing else**, and the datagram arrives on a later round as `{inet_async, Port, Id, Result}` with `InetAsyncResult::Data(ReadMessage)` carrying the 7 packed source bytes as the read's `header` and the payload as its `body` — what lets the runtime render `[list]` mode's flat `[1, PortHi, PortLo, A, B, C, D, …data]` and `[binary]` mode's improper `[1, …, D | <<Payload>>]`.
- Its guard ladder is the C's, in the C's order: no socket ⇒ `[0]exbadport`; `options.active != 0 || arg.len() != 8` ⇒ `[0]einval`; the single shared receive slot taken ⇒ `[0]ealready`. The operand's `Length` is carried and **ignored** (no `enomem` arm).
- `Driver::ready_input` completes a queued operation when one exists and with none queued reads nothing — the property that makes the `{active,…}` path safe (a passive socket's datagram stays in the kernel for the runtime's `recv`).
- A completion is produced only by readiness, the port timer, or the round's async drive — never inside the control call, so a `Timeout` of `0` reports `{error, timeout}` on a later round, after the reply; a close/peer teardown answers `{error, closed}` **exactly once** per pending op, after which a receive is `[0]enotconn`.

### The `send` wire forms, in decode order
1. `split_address`'s `[address][payload]`, tried **plain first** — an unconnected `gen_udp:send/4` really sends, because a queued command needs only an *open* socket (`send(2)` when connected, `sendto(2)` with the operand address otherwise).
2. The `prim_inet` caller-reference prefix `[n:be16][n tag bytes]` (`n >= 1`, `2 + n` fits, `arg[2] == 131`), stripped once per command and echoed verbatim in `{inet_reply, Port, Reply, Tag}`; an empty or non-ETF tag (`echoable_caller_tag`) stages **no** reply, the C's `NULL` caller.
3. `split_datagram`'s `[address][anc_len:be32][anc][data]` — a truncated operand is `einval`, a non-zero `anc_len` the typed `unsupported` with the command left queued.
- The queued path (`Driver::output`/`ready_output` → `write_command`) mirrors that order through `strip_command_prefix`; an empty queue is `Ok(0)` and latches nothing.
- A malformed prefix consumes the queue and latches `signal_failure(DriverFailure::posix(EINVAL))` (port reaped after the round, `port_command/2` still `true`, later commands `badarg`); `!IS_OPEN` and a `write_pending` `BadArg` consume the command and stage `{error, einval}` with the port **alive**; `EAGAIN` keeps the exact remainder queued and arms `Interest::BOTH`; `ECONNREFUSED`/`ECONNRESET`/`EPIPE` record `send_error`, clear the output queue and stop.
- `stat.send_*` counts payload octets only (`inet_output_count`), while the value returned to the port layer is the whole command's queue shrinkage that `Port::check_write_accounting` verifies.

### The `{active,…}` datagram half (`active.rs`)
- `active_delivery` runs from `Driver::ready_input` **after** `async_ready_input`, and is a no-op when a queued receive exists, when `options.active == INET_PASSIVE`, when `options.deliver != INET_DELIVER_TERM` (the only delivery mode the channel can spell), or when there is no socket.
- One `ActiveInputEvent::Data(ReadMessage)` per datagram: `header` = the 7 packed source bytes, `body` = the payload, identical in `[list]` and `[binary]` mode because the channel is term-free; `stat.input(len)` per delivery.
- The burst is bounded by `options.read_packets` (`INET_PACKET_POLL = 5` by default, `../opt.rs`) and breaks on `EAGAIN`; it stops the instant the mode flips to passive, so a burst spends at most one grant.
- A burst that ended by spending its whole allowance re-arms `Interest::READABLE` — the edge-triggered poller's substitute for the C's level-triggered `sock_select`; `delivered > 0` keeps a misconfigured `{read_packets, 0}` from re-arming forever.
- A real `recvfrom(2)` errno emits `ActiveInputEvent::Error(InetReason::from_errno(..))` — the `{udp_error, S, Reason}` — and returns: the port stays live, **there is no `{udp_closed, S}`** (the C has none for a datagram port either), and the next readiness edge reads again; in passive mode the same failure is an `{error, Reason}` completion instead.
- `after_delivery` is `INET_CHECK_ACTIVE_TO_PASSIVE`: `{active, once}` flips **silently** and records `ActiveSpend::Once`; `{active, N}` decrements and on exactly `0` flips, records `ActiveSpend::Multi` and stages one `ActiveInputEvent::Passive`; `{active, true}` records nothing.
- A `Passive` therefore has exactly two sources — a delivery that spent the last credit, and the `setopts` walk itself (`after_setopts`/`setopts_with_effects` on `landed_at_zero`), which spends no grant because it delivers nothing.
- Nothing is delivered **inside** a `setopts` control call: a passive→armed transition only re-arms read readiness (both `/* XXX: UDP sockets could also trigger immediate read here NIY */` comments are the C's own admission that its inline `trigger_recv` is `SOCK_STREAM`-only).
- `{deliver, port}` is stored and reported but never delivers — this path puts nothing on the port input queue, so `{udp, S, IP, Port, Data}` is never fabricated here.
- `Driver::active_mode()` is `ActiveMode::from_parts(options.active, options.active_count)` (the **running** counter); `take_active_spends()` drains oldest first, consuming on take.

### Empty datagrams
- A zero-length datagram **is** a delivery on every channel: `ReadMessage { header: <7 bytes>, body: Some(vec![]) }` — a present, empty body, never `body: None` (that is a header-only reply, a different message) and never nothing at all.

### Two deliberate non-mirrors of the C
- `open` accepts **only** `INET_TYPE_DGRAM`: the C's `packet_inet_ctl` accepts `INET_TYPE_STREAM` for *either* entry point (one driver file), so the oracle's `udp_inet` port answers `[1]` to a stream `open` while this floor answers `[0]einval`.
- `getopts` on a socket-less port answers a **well-formed** reply (the driver-state records, none for a syscall-backed code) where the C's `inet_fill_opts` can walk a `NULL` descriptor and answer `[0]exbadseq`; that bug is not reproduced.

## Constraints
- Errors are `beam_common::{Result, BeamError}` only: operational socket failure ⇒ the `[0]`-tagged reply, malformed argument/unknown verb ⇒ `BadArg`, syscall failure ⇒ `BeamError::Io`, an opcode the C serves but this floor does not ⇒ `Opcode::not_served()`; never a panic/`unwrap`/`todo!` on runtime data.
- **No `unsafe` in this node** and none in `../udp.rs`: every raw socket syscall lives in the private `../sys.rs`, each block carrying a `// SAFETY:` rationale.
- The driver **owns** the descriptor it created or adopted — except the `prebound` (`INET_REQ_FDOPEN`) one, which stays the caller's for the port's lifetime; nothing else may close it.
- The byte path is **edge-triggered**: a read callback drains to `EAGAIN`, and a short write keeps the exact unwritten remainder queued and arms write readiness.
- Datagrams are never truncated (`DATAGRAM_CHUNK` exceeds the maximum IPv4 payload) and never merged: one `recvfrom(2)` is one delivery on whichever channel is in use.
- One `Driver` is one port: no port-creation capability and no caller notion, so a staged `{inet_reply, …}` is addressed to the port's **owner** (`PortCompletion`), not the C's `driver_caller`.
- The `Driver`/`DriverContext` seam is frozen; the datagram half grows only through additive, defaulted members, and `PortNotification` keeps its two variants.

## Known Issues
- **`{header,N}` cannot be applied here and cannot be expressed in the frozen read shape.** `ReadMessage` has one header slot and this driver spends it on the 7 packed source bytes the runtime decodes into `{IP, Port}`, so the C's `hsz`-sized `{header,…}` prefix has nowhere to go; closing it needs a **third** component on the delivered read. Owner: parent node (`../CONTEXT.md` › *Handoffs* (a0)).
- **Ancillary data is refused, never dropped silently:** a `send` operand with a non-zero `anc_len` is the typed `unsupported` with the command left queued, and the receive path has no `recvmsg(2)`/cmsg channel at all (`IP_PKTINFO`, cmsg flags) — this floor uses `sendto(2)`/`recvfrom(2)` only.
- **`send_timeout`/`send_timeout_close` (codes 30/35) are stored and reported, not acted on:** the answer should be an asynchronous `{inet_reply, Port, {error, timeout}}` (closing under `send_timeout_close`), which needs a pending-command slot plus a deadline merged into `Driver::next_timeout`. Owner: parent node (`../CONTEXT.md` › *Handoffs* (b)).
- **`{deliver, port}` never delivers**, and **`SUBSCRIBE`(24)'s later `{empty_out_q, Port}` is never sent** — the `Driver`/`PortNotification` seam can carry neither (owner: parent node).
- **`../udp.rs`'s module doc is stale on one point:** its "*What the real `gen_udp` still needs*" list still says `{active, true|once|N}` delivery is absent by design and that the driver never delivers input on its own, while `active.rs` implements exactly that (`active_delivery`, the `ActiveInput` channel, the full state machine). `active.rs`'s module doc is the current statement.
- **`INET_DELIVER_TERM` is declared twice** — once in `../opt.rs` (where the default is set) and again privately in `active.rs`, because the former is not visible here; one C constant, two declarations, so widening `../opt.rs`'s and importing it is the fix (owner: parent node).
- **The test harness exists in two copies:** `udp_tests.rs` re-declares `CONTROL_MAGIC`, the `INET_REQ_*` numbers it uses, `ERL_TIMEOUT`, `open_arg` and `loopback_arg` although `../opcode_tests.rs` already exposes `pub(super)` equivalents that `async_tests`/`empty_datagram_tests`/`error_tests` do import. Its `Rig` must stay its own (it drives the driver directly rather than through `PortRegistry`), but the helpers could be imported.

## Test Strategy
Gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_io` — the wrapper is mandatory, because the sandbox `$HOME` is read-only and cargo's default `CARGO_HOME` is unwritable.
- **One mechanism:** a real loopback datagram socket, driven through `PortRegistry` + `Poller` + `erlang:port_control/3`'s decimal opcode route (`active_tests`, `async_tests`, `empty_datagram_tests`, `error_tests`, `oracle_dump_tests`) or directly through a hand-built `DriverContext` (`udp_tests`' port-less `Rig`), with the injector **outside** the port — a plain `std::net::UdpSocket`, or `os:cmd("printf … > /dev/udp/127.0.0.1/P")` on the oracle side — **never a second VM**.
- Waits pump `poller.poll` → `handle_events` → the relevant `take_*` drain until a **condition** holds under a 10–20 s hang guard (`error_tests` uses 20 s), never a fixed sleep, so a loaded host cannot fail a case.
- Every `erl`/`escript`/`cc` oracle case prints a `SKIP:` banner and returns when the tool is absent or past its 30 s budget (`ERL_TIMEOUT`) — it never fails; comparisons are byte-exact except for an ephemeral port or fd.
- `udp_tests.rs` (**40**, `-p beam_io --lib`) — the driver's main suite: real loopback round trips, every control-reply class (the `open` guards, `bind`/`connect`/`close` ladders), the option wire (`getopts` open/closed/socket-less, `setopts` refusals), both reception paths, the three `send` wire forms, the write path's accounting (empty-queue re-drive, socket-less tagged command, malformed-command reaping), ownership/`Drop`, the registered name, and the `erl` cross-checks.
- `active_tests.rs` (**15**) — the `{active,…}` half: the delivery shape (7-byte packed source address as the header, identical in `[list]`/`[binary]` mode), the per-delivery accounting, the state machine (`once` silent, `N` ⇒ `{udp_passive, S}` after the credit-spending datagram, `true` never), the passive floor (no delivery **and nothing consumed** from the kernel), both `Passive` sources, the bounded burst, and every behavioural row cross-checked against the live driver.
- `async_tests.rs` (**16**) — `PACKET_REQ_RECV`(60) and its `{inet_async, …}` completions plus the send path's `{inet_reply, …}`: the guard ladder in the C's order, the **separation of the two halves** (the reply is the id and nothing else), the completion shape, the deadline (`{error, timeout}` after the reply), the teardown (`{error, closed}` exactly once), and the caller tag echoed verbatim with a zero-length tag staging nothing.
- `empty_datagram_tests.rs` (**4**) — `an_empty_datagram_is_delivered_with_a_present_empty_body`, `every_empty_datagram_is_its_own_delivery` (never coalesced), `an_empty_datagram_completes_a_queued_passive_receive`, plus the oracle row (whose probes also capture the C's `{deliver, port}` shape); the sender is a real `UdpSocket` because `printf` would send nothing for zero length.
- `error_tests.rs` (**2**) — the active read error: an ICMP port-unreachable is provoked deterministically by sending from a **connected** datagram socket to a port nothing is bound to (Linux queues the error for the next `recvfrom(2)`); the pin is exactly one `Error(InetReason::Errno(ECONNREFUSED))` per ICMP with the port **still live** and no close message, the liveness half rebinding that very port, and the second case cross-checks both rows through an `escript` probe.
- `oracle_dump_tests.rs` (**2**) — the oracle-dump **framing**: a `bind` reply carries a port whose low byte can be any value, so a delimiter-split dump silently corrupts the comparison. One case binds an explicit loopback port whose low byte is the old delimiter `0x7C` and whose high byte is not (three replies stay three, the middle one two bytes), the other pins the ordinary ephemeral-bind reply's shape (`[1, hi, lo]`, non-zero port, the two runs legitimately differing); only the resource-allocation step is retried, never an assertion.
- `../oracle_dump.rs` (the parent's `pub(super)` helper it consumes) is why: `ERL_FRAME_ELEMENT` splices each reply into the Erlang dump as a big-endian length word plus its bytes, and `decode`/`encode` split and build them — so a reply holding the `|` delimiter, an empty reply, a NUL byte, a length-word-shaped body or a truncated tail cannot collide with the framing.

## Routing Table
- `../udp.rs` → the driver body (in the parent `inet` directory, not this node): `UdpInetDriver`, its whole `Driver` impl, the three constructors, every verb body, `decode_queued_command`, the socket/interest plumbing (`release_socket`/`close_socket`/`set_interest`), the option store, and `UDP_DRIVER_NAME`/`DATAGRAM_CHUNK`/`make_udp_inet` — all described above
- `active.rs` → the `{active,…}` half: `active_delivery`, `recv_retrying`, `after_delivery`, `after_setopts`, `setopts_with_effects`, `INET_DELIVER_TERM`; suite `active_tests.rs`
- `async.rs` → the asynchronous half, declared as `#[path = "udp/async.rs"] mod op_queue` (`async` is a reserved word): `async_packet_recv`, the completion's production points, `async_timeout`, `drain_pending_closed`, and the queued write path (`write_command`/`strip_command_prefix`/`fail_malformed_command`/`settle_command`); it reuses `../async.rs`'s `AsyncQueue`/`async_ok_reply`/`deadline_for`/`parse_recv_operand`/`drain_closed` rather than re-implementing them
- `udp_tests.rs` → the driver's main suite (40 tests; `mod udp_tests;` from `../udp.rs`) — what it sweeps is under *Test Strategy*; it also declares the `#[path = "oracle_dump_tests.rs"] mod oracle_dump_tests;` child, which is why that suite imports its helpers from `super::` (here) and not from `../opcode_tests.rs` like its siblings
- `active_tests.rs` → the `{active,…}` suite (15 tests; `pub(in crate::drivers::inet) mod active_tests;` from `../udp.rs`, because `../active_seam_tests.rs` drives its registry-based `Rig`)
- `async_tests.rs` → the asynchronous suite (16 tests; `mod async_tests;`), importing the registry-driven harness from `../opcode_tests.rs`
- `empty_datagram_tests.rs` → the zero-length-datagram pin (4 tests; `mod empty_datagram_tests;`)
- `error_tests.rs` → the active read-error pin (2 tests; `mod error_tests;`)
- `oracle_dump_tests.rs` → the oracle-dump framing pin (2 tests; the `#[path]` child of `udp_tests.rs`), consuming the parent's `../oracle_dump.rs`

## See Also
- `../CONTEXT.md` → the `inet` subtree node: the shared control vocabulary and packed-address codec, the option surface, the close path, `tcp_inet`'s halves, and the cross-crate handoffs (the `{header,N}` gap is (a0) there).
- `../../CONTEXT.md` → the built-in-drivers node: `{line,N}` framing, the write-path reply round trip, the per-driver close path (`flush` → drain → `stop`).
- `../../../CONTEXT.md` → the `beam_io` crate root: the frozen `Driver`/`DriverContext` seam, `Port`/`PortRegistry`, `PortCompletion`, `ActiveInput`/`ActiveSpend`.
- Legacy C ground truth (read-only): `erts/emulator/drivers/common/inet_drv.c`'s `packet_inet_*` family (`packet_inet_start`, `udp_inet_start`, `packet_inet_stop`, `packet_inet_ctl`, `packet_inet_command`, `packet_inet_input`, `packet_binary_message`, `packet_passive_message`, `packet_error_message`, `packet_reply_binary_data`) plus the Erlang half `erts/preloaded/src/prim_inet.erl`.
