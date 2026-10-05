# Megaco TCP/IP (TPKT) Transport

## Intent
Separately-started TPKT (RFC 1006 style "TPKT over TCP/IP") transport for Megaco/H.248.
Contains the MGC-side listener plus the per-connection handler used on both MGC and MG sides, and the send/block/close primitives the megaco engine calls through its `send_mod` (which defaults to `megaco_tcp`).
It is **not** part of the `megaco` application supervision tree (`megaco.app.src` lists the modules but not in `registered`, and `../engine/megaco_sup.erl` does not start them): a user must call `megaco_tcp:start_transport/0` explicitly before using this transport.

## API Surface
Public module `megaco_tcp` (the only `-moduledoc`-documented module here; it is NOT declared `-behaviour(megaco_transport)`):
- transport service: `start_transport/0` (returns `{ok, TransportRef}` = the `megaco_tcp_sup` pid, *linked* to the caller, and creates the `megaco_tcp_stats` ETS table via `megaco_stats:init/1`), `stop_transport/1` (`-doc false`; `unlink` then `exit(Pid, shutdown)`).
- `listen(TransportRef, Options)`: MGC/login side; options `{port, P}`, `{receive_handle, RH}`, `{tcp_options, list()}`, `{inet_backend, default|inet|socket}` (`port`, `receive_handle` mandatory).
- `connect(TransportRef, Options)`: client side; additionally mandatory `host`, plus `{module, Mod}` (callback module, default `megaco`) and `{serialize, boolean()}`; returns `{ok, Handle, ControlPid}`.
- per-connection/`send_mod` callbacks: `send_message/2` (i.e. `SendMod:send_message(SendHandle, Bin)`), `block/1`, `unblock/1`, `close/1`, `socket/1`, `upgrade_receive_handle(ControlPid, NewRecvHandle)`.
- statistics: `get_stats/0,1,2`, `reset_stats/0,1` over the `megaco_tcp_stats` table.
- exports types `handle/0 :: inet:socket()` and `counter/0` (`medGwyGateway*` SNMP counters).
Internal modules (all `-moduledoc false`): `megaco_tcp_sup`, `megaco_tcp_accept_sup`, `megaco_tcp_accept`, `megaco_tcp_connection_sup`, `megaco_tcp_connection`.
Internal header `megaco_tcp.hrl`: `#megaco_tcp{}` protocol-option record, `?GC_MSG_LIMIT` (1000), `?HEAP_SIZE(S)`, and the `?tcp_report*`/`?tcp_debug` event-trace macros.
Build inputs: `Makefile` (includes `../app/megaco.mk`), `modules.mk`, `depend.mk`; sources are released to `$(RELEASE_PATH)/lib/megaco-$(VSN)/src/tcp`.

## Supervision Tree
`megaco_tcp_sup` (`one_for_one`, 5 restarts/1s) has three permanent children:
- `megaco_tcp_accept_sup` — `simple_one_for_one` supervisor of `megaco_tcp_accept` acceptors,
- `megaco_tcp_connection_sup` — `simple_one_for_one` supervisor of `megaco_tcp_connection` connection processes,
- `megaco_tcp` — gen_server "server" holding the listen sockets and a `linkdb` of `{AcceptorPid, {TcpRec, ListenSocket}}`.
`megaco_tcp:listen/2` finds the `megaco_tcp` child of the transport supervisor and calls `{add_listener, Options}`; `setup/2` opens the listen socket and starts one acceptor per listener.

## Constraints
- Wire framing is TPKT: sockets are opened with `binary`, `{packet, tpkt}`, `{active, once}`; the driver does **not** strip the 4-byte header, so receive code matches `<<3:8, _X:8, Length:16, Msg/binary>>` and `send_message/2` must add the header itself (`add_tpkt_header/1`, which also yields the octet count for `medGwyGatewayNumOutOctets`).
- The `handle()`/`send_handle` is the bare `inet:socket()`; the connection process passes it as the `SendHandle` argument of `Mod:receive_message/4` (small messages) / `Mod:process_received_message/4` (large or serialized messages), and megaco later sends with `megaco_tcp:send_message(Socket, Bin)`.
- Only `send_message/2` exists here: `megaco_transport`'s optional `send_message/3` and `resend_message/2` are **not** exported by `megaco_tcp`.
- The connection process is the controlling process of the socket and re-arms `inet:setopts(Socket, [{active, once}])` after every received message.
- `{receive_handle, RH}` must be a `#megaco_receive_handle{}`; it can only be replaced by `megaco_tcp:upgrade_receive_handle/2`, which is a `gen_server` call into the connection process that swaps the record in its state.
- New record fields in `megaco_tcp.hrl` must be appended last (see Known Issues).

## Design Decisions
- The transport supervisor is started on demand and linked to the calling process rather than to `megaco_sup`, so a node can start/stop transport services independently of the megaco application and per `TransportRef`.
- `serialize=false` (default) means: messages with `Length < ?GC_MSG_LIMIT` (1000) are processed inline in the connection process via `Mod:receive_message/4`; larger ones are handled by a spawned process (`spawn_opt` with `{min_heap_size, ?HEAP_SIZE(Length)}`) calling `Mod:process_received_message/4`. `serialize=true` always takes the inline `process_received_message/4` path.
- The acceptor is a `proc_lib:spawn_link`ed plain loop (`net_accept/4` → `gen_tcp:accept`), not a gen_server; `megaco_tcp` links it and, on its `{'EXIT', ...}`, restarts it with the same listen socket (`resetup/3`) instead of relying on `megaco_tcp_accept_sup`.
- Each accepted socket inherits the listener's `{active, once}` mode (the inet driver copies `inet.mode` to the accepted descriptor), which is what lets the first packet reach the connection process after `gen_tcp:controlling_process/2`.
- The listener/`megaco_tcp` server passes the top transport supervisor pid down to the acceptor, which starts connections via `megaco_tcp:start_connection/2` → `megaco_tcp_sup:which_connection_sup/1` → `megaco_tcp_connection_sup:start_child/2`.
- `block/1`/`unblock/1` simply set `{active, false}` / `{active, once}` on one socket, i.e. flow control only applies to incoming traffic of that connection.
- One set of SNMP counters is created per connection socket (`create_snmp_counters/1`, keys `{Socket, Counter}` in `megaco_tcp_stats`), which are also what `get_stats/0` enumerates.

## Known Issues
- `parse_options/3` accepts `{tcp_options, List}`, while `listen/2`/`connect/2` document `{options, List}`; the documented name is rejected as `{error, {bad_option, ...}}` (the `../udp/` sibling has the same `udp_options` vs `options` mismatch).
- The `-spec listen(TransportRef, Options) -> ok` is wrong: `listen/2` can return `{error, no_tcp_server}` or `{error, {could_not_start_listener, Reason, DBStatus}}`.
- `megaco_tcp.hrl` cannot be included together with `../udp/megaco_udp.hrl` (both define `HEAP_SIZE` and `GC_MSG_LIMIT`), which is why `../engine/megaco_filter.erl` duplicates the `#megaco_tcp{}` record inline; that copy has no `inet_backend` field, so new fields must be appended at the end to stay compatible.
- The `megaco_tcp_stats` ETS table only exists after `start_transport/0`; `megaco_stats:init/1` fails if the table already exists (caught and ignored on a second `start_transport/0`), and the connection increments counters with a raw `ets:update_counter/3`, so counter updates fail if the table is missing or was dropped.
- `megaco_tcp_connection:terminate/2` logs a detailed "abormal termination" report (scheduler/process/memory info) for every non-`shutdown` stop, i.e. abnormal closes are noisy by design.

## Notes for Agents
- Debug/trace logging uses `?d/?d1/?d2` from `../app/megaco_internal.hrl`, which are no-ops unless `megaco_debug` is defined (`MEGACO_DEBUG=true`); the `-Ddebug` added by the `Makefile` for `TYPE=debug` is unrelated, and `sz/1` in `megaco_tcp.erl` is only compiled when `megaco_debug` is set.
- `ebin/` output is gitignored; the tracked build inputs are `Makefile`, `modules.mk`, `depend.mk` (the last lists the include dependencies: `../../include/megaco.hrl`, `../app/megaco_internal.hrl`, and `megaco_tcp.hrl`).
- `megaco_tcp_accept` deliberately drains stale `{tcp,tcp_closed,tcp_error}` messages (`tcp_clear/1`) before closing a socket whose connection process could not be started.
- The `module` option selects the megaco callback module used for received messages; `megaco` (the app module) is the default.

## See Also
- `../engine/megaco_transport.erl` — the documented behaviour for user-supplied transport modules (`send_message/2,3`, `resend_message/2`).
- `../engine/megaco_messenger_misc.erl` — calls `SendMod:send_message(SendHandle, Bin)` / `resend_message/2` / `send_message/3` depending on the `resend_indication` config.
- `../engine/megaco_messenger.erl`, `../engine/megaco_config.erl` — store `send_handle` (= this socket) and `control_pid` (= this connection process) in `#conn_data{}`.
- `../engine/megaco_stats.erl` — ETS counter helper backing `megaco_tcp_stats` (`init/1` is called by `start_transport/0`).
- `../udp/` — sibling transport with the same module/supervision shape (sibling; read-only).
- `../../test/` — `megaco_tcp_SUITE.erl`, `megaco_test_tcp_generator.erl`, `megaco_test_mgc.erl`/`megaco_test_mg.erl` (they start this transport with `megaco_tcp:start_transport/0`) (sibling; read-only).
