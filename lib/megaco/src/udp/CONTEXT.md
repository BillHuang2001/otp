# Megaco UDP/IP Transport

## Intent
The UDP/IP transport implementation for Megaco/H.248 (`megaco` app), the datagram counterpart of `../tcp/`.
Because UDP has no connection concept, the user creates an explicit send handle (remote address+port) per peer and passes it to `megaco:connect/4,5` alongside the receive handle.
The transport is not part of the `megaco` application supervision tree (`megaco.app.src` does not register/start it) — it is started on demand by `megaco_udp:start_transport/0`, which lets a user run several independent UDP transports.

## API Surface
Public transport-control API, all in `megaco_udp`:
- `start_transport/0` → `{ok, TransportRef}`: starts `megaco_udp_sup` and initializes the `megaco_udp_stats` ETS table (`megaco_stats:init/1`).
- `stop_transport/1`: unlinks and `exit/2`s the transport supervisor (`-doc false`, and `exit/2` is deprecated).
- `open/2` → `{ok, Handle, ControlPid} | {error, Reason}`: opens a `gen_udp` socket and starts a `megaco_udp_server` that becomes its controlling process.
- `create_send_handle/3` (Handle, Host, Port) → `send_handle()`: resolves `Host` with `inet:getaddr/2` and creates per-peer SNMP counters.
- `send_message/2` (send handle, iodata) → result of `gen_udp:send/4`; increments `medGwyGatewayNumOutMessages`/`OutOctets` on success.
- `socket/1`, `close/1`, `block/1`, `unblock/1`: all accept either a `handle()` or a `send_handle()`.
- `upgrade_receive_handle/2` (ControlPid, `#megaco_receive_handle{}`).
- Statistics: `get_stats/0,1,2`, `reset_stats/0,1` over the `megaco_udp_stats` table.
- Types: `handle()` is `inet:socket()` (the socket *is* the handle), `send_handle()` is the opaque `#send_handle{socket,addr,port}` record (defined in the `.erl`, not exported via any header), `counter()` is the `medGwyGatewayNum*` SNMP counter set.
Behaviour `megaco_transport` (`../engine/megaco_transport.erl`): only the mandatory `send_message/2` is implemented; the optional callbacks `send_message/3` and `resend_message/2` are not (see Known Issues).
Internal modules:
- `megaco_udp_sup` — `simple_one_for_one` supervisor (intensity 500/100) of `megaco_udp_server` children; `start_child/2`.
- `megaco_udp_server` — `gen_server` owning one socket; state is the `#megaco_udp{}` record; handles `{udp, ...}` messages, `stop`, `{upgrade_receive_handle, _}`.
- `megaco_udp.hrl` — `#megaco_udp{}` record plus `?udp_*` trace macros and `?GC_MSG_LIMIT`/`?HEAP_SIZE` limits.
Build: `modules.mk` lists `MODULES`/`INTERNAL_HRL_FILES`; `Makefile` installs sources to `$(RELSYSDIR)/src/udp` and compiles to the app-wide `../../ebin` (no build outputs in this directory).

## Constraints
- The socket's controlling process is the `megaco_udp_server`, not the caller that invoked `open/2`; incoming datagrams are delivered to that server.
- `open/2` requires the `port` and `receive_handle` options; anything missing yields `{error, {missing_options, [...]}}` and unknown tags `{error, {bad_option, _}}`.
- Accepted options: `port`, `udp_options` (raw `gen_udp` option list), `receive_handle`, `module` (callback module for received messages, default `megaco`), `serialize`, `inet_backend` (`default | inet | socket`).
- No framing or session: one Megaco message equals one UDP datagram, and the transport does no segmentation/reassembly (that is handled by Megaco's own message segmentation).
- Delivery (default `serialize = false`): datagrams smaller than `?GC_MSG_LIMIT` (1000 bytes) are dispatched inline in the server via `Module:receive_message/4`; larger ones are handled by a linked spawned process (`min_heap_size = ?HEAP_SIZE(Size)`) calling `Module:process_received_message/4`.
- With `serialize = true` every datagram is processed inline via `Module:process_received_message/4` (`receive_message/4` is never used).
- After each datagram the socket is re-armed with `{active, once}`; `block/1` sets `{active, false}`, `unblock/1` restores `{active, once}`.
- `upgrade_receive_handle/2` only swaps the `receive_handle` field of the already running server's state — there is no per-connection process to upgrade as in TCP; it is used after a negotiated protocol-version change.
- Incoming messages also create a send handle for the sender (`create_send_handle/3`) and increment the `*InMessages`/`*InOctets` counters, so statistics are keyed per remote peer.
- Options are validated by `parse_options/3`; the `inet_backend` handling includes a Windows-only `post_process_opts` path that picks a usable local address (`ip`/`ifaddr`) when the `socket` backend is used.

## Design Decisions
- Send handles are explicit instead of connection-based because UDP peers are just addresses; this is why `create_send_handle/3` is public and why `megaco_udp` is named in the *receive handle's* `send_mod` (that is how `megaco_messenger` finds `send_message/2`).
- The transport supervisor is decoupled from `megaco_sup` so users can start/stop (and run multiple) UDP transports independently of the application.
- `module` is an option so that a user can substitute their own receive callback (`receive_message/4` / `process_received_message/4`) instead of the Megaco app's.
- The socket-vs-handle split in `#megaco_udp{}` (both carry the same socket) comes from supporting the `socket` inet backend, where the handle and the underlying socket are of different kinds.

## Known Issues
- Resends are unsupported by this transport: only `send_message/2` is exported, so with the `resend_indication` config option set to `true` or `flag` the messenger's `apply(MegacoUdp, resend_message|send_message, [...])` fails and the send is reported as `{error, {send_message_failed, _}}` with the error counter incremented (the default `false` works).
- `close/1` does not return `ok` as its spec claims: `megaco_udp_server:do_stop/1` calls back into `megaco_udp:close/1` from inside the server process, so the owner lookup finds the server itself and the resulting `gen` call hits the self-call guard (`exit(calling_self)`); that value is caught and becomes the reply, and the socket is closed as a side effect of the owning server process terminating.
- `erlang:exit/2` in `stop_transport/2` is deprecated and suppressed with `-compile([{nowarn_deprecated_function, ...}])`.
- `open/2`'s spec advertises `{options, list()}` but `parse_options/3` only accepts that tag as `udp_options`; `serialize` is accepted but not documented in the `open/2` doc.
- `-spec send_message/2 -> ok` is optimistic: it can return `{error, {bad_send_handle, _}}` or a `gen_udp:send/4` error tuple.
- There is no `depend.mk` (unlike `../tcp/`): header dependencies are hand-written at the bottom of the `Makefile` and include a stale rule for `megaco_udp_test.erl`, a module that no longer exists.
- `#send_handle{}` is private to `megaco_udp`, so nothing else can construct or match it — statistics keys containing it are only meaningful through `megaco_udp`'s own functions.
- The socket-failure path is untested: `socket_failure/1` in `../../test/megaco_udp_SUITE.erl` is a `?SKIP(not_yet_implemented)` stub.

## Notes for Agents
- Because `handle()` is the socket itself, `block/1`/`unblock/1`/`close/1` simply unwrap a send handle's socket; `socket/1` is the identity for a plain handle.
- Changing `#megaco_udp{}` (e.g. adding a field) requires matching updates at: `../engine/megaco_filter.erl`, which keeps its own copy of the record (it cannot include both the udp and tcp headers) that is not in sync with the current fields (`handle`, `inet_backend` are missing), and the dependency lines in the `Makefile`.
- The socket options `binary`, `{reuseaddr, true}` and `{active, once}` are always added by `open/2` before the user's `udp_options`, so a user option can override them.
- Debug/trace output uses `?udp_debug`/`?udp_report_*` from `megaco_udp.hrl`, which route through `megaco:report_event/5` and therefore require a running `megaco` trace setup to be visible.
- Usage examples of the raw API (start_transport → open → create_send_handle → connect, and close) are in `../../examples/simple/` (`megaco_simple_mg.erl`, `megaco_simple_mgc.erl`).

## Test Strategy
- `../../test/megaco_udp_SUITE.erl` drives the transport directly (not through `megaco`) with cases `start_normal`, `start_invalid_opt`, `start_and_stop`, `sendreceive`, `block_unblock` and the skipped `socket_failure`.
- The suite runs each case once per inet backend group (`inet_backend_default`, `inet_backend_inet`, `inet_backend_socket`); the non-default backends are only exercised when the `megaco_test_lib:test_inet_backends()` flag is enabled, which is set for a single dedicated host.
- End-to-end use of this transport (with MG/MGC roles, `send_mod = megaco_udp`) is covered by `../../test/megaco_test_mg.erl` and `../../test/megaco_test_mgc.erl`.
- `../../test/megaco_test_deliver.erl` includes `megaco_udp.hrl` and forwards `receive_message/4` / `process_received_message/4` callbacks, so the hrl must stay includable from the test dir.

## See Also
- `../engine/megaco_transport.erl` — the transport behaviour this module (partially) implements.
- `../engine/megaco_messenger.erl` (with `megaco_messenger_misc.erl`) — calls `send_mod:send_message/2,3` / `resend_message/2` on the send handle.
- `../engine/megaco_stats.erl` — ETS-backed counter table used by `megaco_udp_stats`.
- `../tcp/` — the connection-based sibling transport (different handle model, has `depend.mk`).
- `../../include/megaco.hrl` — `#megaco_receive_handle{}` (`send_mod`) and other public records.
