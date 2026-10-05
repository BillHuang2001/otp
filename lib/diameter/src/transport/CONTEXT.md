# Diameter Transports

## Intent
Implements the bottom (transport) layer of the Diameter stack (RFC 6733): the `diameter_transport` behaviour, the concrete transports, and the supervisor subtree in which every transport process runs.
A Diameter service names a transport per `{transport_module, M}` / `{transport_config, Cfg}` pair in `diameter:add_transport/2`; `M` must implement `diameter_transport` and `Cfg` is passed verbatim to `M:start/3`.
One transport process maintains exactly one connection/association with one remote peer; a service typically has a connecting (client) and/or a listening (accept) transport.
Transports are deliberately thin: application-level state (capabilities exchange, watchdog, Diameter request handling) lives in `../base/diameter_peer_fsm.erl` and above.

## API Surface
`diameter_transport.erl` — the behaviour and its public documentation (man page `diameter_transport(3)`).
- `-callback start({Type, Ref}, Svc, Config) -> {ok, Pid} | {ok, Pid, [inet:ip_address()]} | {error, Reason}`, `Type = connect | accept`, `Ref = diameter:transport_ref()`, `Svc = #diameter_service{}`, `Config` = the `transport_config` term.
- Messages the transport process receives from its parent (all of the form `{diameter, Term}`): `{send, Msg | false}`, `{close, Pid}`, `{tls, Ref, Type, Bool}`.
- Messages the transport process sends to its parent: `{self(), connected}` (accept), `{self(), connected, Remote}` / `{self(), connected, Remote, LAddrs}` (connect), `ack`, `{recv, [Msg]}`, `{tls, Ref}` (TLS handshake completed).
- `Msg` is a `binary()` or a `diameter_codec:packet()`; inbound `transport_data` set on a request is echoed back on the corresponding answer.
- `{diameter, ack}` requests acknowledgement of every `recv`: each subsequent `recv` request is answered by a `send` message or by `false` when the request was discarded; both `diameter_tcp` and `diameter_sctp` request it once when a `message_cb` is configured, so outstanding requests can be counted for flow control.
- Exported start-function helpers (`-doc false`): `start/3`, `select/3`, `eval/3` let `diameter_transport` itself be used as a `transport_module` with a fun/`{M,F,A}` config (evaluated via `diameter_lib:eval/1`); no in-tree caller.

`diameter_tcp.erl` — TCP transport using `gen_tcp`, with optional TLS via `ssl` (man page `diameter_tcp(3)`); other exports: `info/1` (service_info callback), `listener/1` (`diameter_sync` callback), `ports/0,1` (test/debug helpers, no in-tree callers), `start_link/1`, `init/1`.
- Options: `raddr` and `rport` (connect only, exactly one each, `rport` defaults to 3868), `accept` (listen only; address or regex match, repeatable), `ssl_options` (`true` = TLS handshake at connection establishment, list = ssl options used after capabilities exchange negotiates TLS), `port` (listen, default 3868), `sender`, `message_cb`, `fragment_timer` (default 1000 ms), leading `{module, M}` to substitute a gen_tcp-like module, and any `gen_tcp`/`ssl` option.

`diameter_sctp.erl` — SCTP transport using `gen_sctp` (man page `diameter_sctp(3)`); same auxiliary exports as `diameter_tcp` (`info/1`, `listener/1`, `ports/0,1`).
- Options: `raddr` (connect only, repeatable — addresses are tried in sequence), `rport` (default 3868), `accept` (listen only), `port` (listen default 3868, connect default 0), multiple `ip` for multihoming, `packet` (`false` | `true` (default, `{stream, Id}` in `transport_data`) | `raw` (`#sctp_sndrcvinfo{}` in `transport_data`)), `unordered` (`false` | `true` | max outbound stream count), `sender`, `message_cb`, plus `gen_sctp:open/1` options except `mode`, `binary`, `list`, `active`, `sctp_events`.
- Outbound stream selection: `{outstream, Id}` in `transport_data` (modulo negotiated streams), else rotation over the outbound streams.

`diameter_etcp.erl` — transport that carries Diameter in Erlang messages; presents a `gen_tcp`-shaped shim (`listen/2`, `accept/1`, `connect/3`, `send/2`, `close/1`, `setopts/2`, `sockname/1`, `peername/1`, `getstat/1`) that `diameter_tcp` uses via the leading `{module, diameter_etcp}` option; `start/3` just delegates to `diameter_tcp:start/3` with that option.

Supervision (all supervisors `simple_one_for_one`, children `temporary`, started with a tag tuple `{listen|connect|accept, ...}` or `#monitor{}` that selects the supervisor):
- `diameter_transport_sup` — started empty by `diameter_sup` at application start; `start_child(Name, Module)` dynamically adds per-protocol supervisors.
- `diameter_tcp_sup` — adds `diameter_tcp_transport_sup`, `diameter_tcp_listener_sup`, `diameter_tcp_monitor_sup`; accepts/connects and monitor processes go under the first, the listen process under the second, the per-connection "monitor" process under the third.
- `diameter_sctp_sup` — adds `diameter_sctp_transport_sup` and `diameter_sctp_listener_sup` (no separate monitor sup; monitor processes run under the transport sup).
- `diameter_etcp_sup` — single supervisor, started on demand by `diameter_etcp:start/3`.

Informal extra interface used by `../base/diameter_service.erl` (`connection_info/2`, `peer_info/1`, `info_port/1`) and the info/debug modules: the transport stores `{Module, info}` in its process dictionary and implements `info/1` returning a proplist — keys `socket`, `peer`, `statistics`, plus `ssl` (with `peercert`) for TLS over TCP.

## Constraints
The caller of `start/3` is the transport's parent — it is the `diameter_peer_fsm` process (`diameter_peer:start/1` is called from `diameter_peer_fsm:start_transport/2`); `#diameter_service.pid` is the *service* process, which is a different process.
A transport must monitor the parent and terminate when it dies, must not link to the parent, and must exit when its connection to the peer is lost (the parent monitors the transport and dies with it).
`start/3` must not report success before the connection is established (connect) or the listening socket exists (accept); both modules use `proc_lib:start_link` + `gen_server:enter_loop` + `proc_lib:init_ack` for this.
Transport option `pool_size` (handled by `../base/diameter_service.erl`, not by the transport modules) makes diameter call `start/3` N times for one transport config/ref, so modules must tolerate several concurrent processes per ref sharing a single listening socket.
Local addresses must come from the `Host-IP-Address` list in `Svc`/config and be reported either as the `start/3` return `LAddrs` or in the `connected` message; a TCP connect process reports them via `diameter_peer:up/3`.
Message delivery over the interface is asynchronous in both directions; nothing is acknowledged at transport level except via `ack`/`message_cb` — Diameter-level ordering/retransmission is the watchdog's and handlers' concern.
A TCP listen transport requires `ssl_options` iff it advertises `Inband-Security-Id` including TLS (1): with a list configured but no TLS negotiated the process waits forever for the `tls` message (recovered only by watchdog failover), while `{ssl_options, true}` performs the handshake at connection establishment instead.
`diameter_sctp` stops on `{diameter, {tls, _Ref, _Type, _Bool}}` — TLS is implemented for TCP only (TLS/SCTP per RFC 3436/6083 and DTLS/SCTP are not implemented).
`diameter_tcp` rejects `binary`, `packet`, `active` and `diameter_sctp` rejects `binary`, `list`, `mode`, `active`, `sctp_events` among the pass-through options, raising `{reserved_options, Opts}`.
Configuration errors raise `erlang:error({Tag, Module, Line})` via `?ERROR`; runtime failures exit with `{shutdown, Reason}` via `x/1`.
Adding or removing a transport restarts the whole service (`../base/diameter_config.erl`), so transports must tolerate being killed at any time; a listen process additionally receives `{transport, remove, _}` through its `diameter_config:subscribe(Ref, transport)` registration and closes its socket.

## Design Decisions
`diameter_tcp`/`diameter_sctp` are `gen_server`s but are started through `proc_lib` so that blocking connect/accept happens inside a synchronous `start/3` (init_ack) while the process then enters the gen_server loop.
Each connection may have an auxiliary "monitor" process (always for TCP, on first send for SCTP with `sender`): it kills the transport if the parent dies while accept/connect is still blocking, and can take over sending so that a blocking send cannot stall reception; its child spec sits in a separate supervisor because a child cannot start another child before `supervisor:start_child/2` returns (hence the extra supervisors).
The TCP listen process only owns the listening socket: every accepting transport calls `accept/1` (or `ssl:transport_accept/1` + `ssl:handshake/2`) on the shared socket itself; the listener monitors the service process and closes the socket on service death or transport removal.
`diameter_sync:call/4` with the key `{Module, listener, Ref}` serializes listener creation, so concurrent accepting transports cannot create duplicate listening sockets.
TCP is a byte stream, so `acc/1-3` reassembles messages from the Message Length field while keeping the length to avoid quadratic binary copying; `fragment_timer` flushes a fragment after two successive timeouts, and a length below the 20-byte Diameter header triggers a best-effort resync by treating the accumulated bytes as a message.
SCTP has no `accept`, so the listener pre-starts transport processes and assigns peeled-off associations through two pending queues in `#listener.pending` (tagged by sign of the count); an accepting process started by diameter before an association exists waits `?ACCEPT_TIMEOUT` (5000 ms) for one.
SCTP sends on stream 0 (ordered) until the second inbound message so that CER/CEA reach the peer before other requests, after which rotation over outbound streams and optional unordered delivery (`sctp_default_send_param`) start.
`diameter_etcp` replaces the socket with Erlang messages: the listen process registers `{diameter_etcp, listener, LAddr, LPort}` in `diameter_reg`, a connector finds it with `diameter_reg:match/1` (via `rpc:call/4` when `raddr` is `{Node, Addr}`), asks for an acceptor, and then exchanges `{recv, Bin}`/`{send, Bin}` with it.

## Known Issues / Notes for Agents
`diameter_etcp` is undocumented (`-moduledoc false`, no man page) and has no test suite; its `sockname/1`, `peername/1`, `getstat/1` return `{error, diameter_etcp}` while `diameter_tcp` matches `{ok, {LAddr, _}} = sockname(Mod, LSock>` in the listen-process init and calls `ok(sockname(...))` in the connect path (`up/4`), so etcp appears non-functional in the current tree — treat it as a demo/experimental transport.
SCTP availability is platform-dependent (`gen_sctp:open/1` may return `esocktnosupport`); the tests skip SCTP that way, and `diameter_util:have_sctp/0` exists for that purpose.
`ssl` is listed in `runtime_dependencies` in `../diameter.app.src` but deliberately commented out of `applications`, so the user must ensure the `ssl` application is started before configuring TLS on a TCP transport.
Port 3868 is the default local port for both TCP and SCTP listening transports and the default `rport` for both connecting transports; a connecting SCTP transport opens its local socket on port 0.
`sender` without a `message_cb` can queue outgoing messages without bound in the sender process (documented warning in `diameter_tcp`/`diameter_sctp`).
A small `recbuf` can make an SCTP peer resend, and a small `sndbuf` can make outgoing SCTP messages be discarded — documented in `diameter_sctp` as a warning.
Both modules are compiled with `{nowarn_deprecated_function, [{erlang, exit, 2}]}` because the monitor/transport interactions use `exit/2`.
`diameter_sctp` relies on `sctp_events` not being configurable (it is rejected as a reserved option) so that every inbound message carries `#sctp_sndrcvinfo{}`; adaptation-layer events are disabled by default and unhandled.
`diameter_sctp` takes an association id from the first reception when it does not match the peeled-off id, to cope with OS differences (noted for Solaris); `assoc_id/1` also handles the case where the first event is not `#sctp_assoc_change{}`.
`diameter_tcp:ports/0,1`, `diameter_sctp:ports/0,1` and `diameter_transport:start/3`, `select/3`, `eval/3` have no in-tree callers.
Matching of `accept` options is done by `diameter_peer:match/2`: a string that does not parse as an address is treated as a case-insensitive regular expression on the textual address.

## Test Strategy
`../test/diameter_transport_SUITE.erl` is the primary test of the transport interface: it drives `diameter_tcp` and `diameter_sctp` against a raw `gen_tcp`/`gen_sctp` peer that speaks `{diameter, ...}` messages directly (accept, connect, reconnect, `message_cb`), so it is the reference for what the behaviour really requires.
End-to-end Diameter-level coverage of the transports is in `../test/diameter_gen_tcp_SUITE.erl`, `../test/diameter_gen_sctp_SUITE.erl`, `../test/diameter_tls_SUITE.erl` (TLS over TCP) and `../test/diameter_pool_SUITE.erl` (`pool_size`), with shared address/transport config helpers in `../test/diameter_util.erl`.
There is no test for `diameter_etcp` and none for the `info/1` / `ports/0,1` auxiliary exports; `diameter_transport_SUITE` is self-contained (`run/0`) but also wrapped for Common Test.

## See Also
- `../base/diameter_peer_fsm.erl` / `../base/diameter_peer.erl` — the parent side of the transport interface (owner of the message protocol above).
- `../base/diameter_config.erl`, `../base/diameter_service.erl` — start transports, pass `transport_config`, handle `pool_size` and restart-on-config-change.
- `../base/diameter_reg.erl`, `../base/diameter_sync.erl` — registration of listeners/transports and serialized listener creation.
- `../base/diameter.erl` — `transport_opt()` documentation (including `pool_size`) and `diameter_tcp(3)`/`diameter_sctp(3)` see-also links.
