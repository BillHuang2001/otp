# diameter — core runtime (`src/base`)

## Intent
Handwritten runtime modules of the `diameter` application (OTP app version `DIAMETER_VSN = 2.7.2`): everything needed to run a Diameter node — service/configuration management, connection and RFC 3539/3588 state machines, message routing, encode/decode.
All 25 modules here are listed in `../modules.mk` under `RT_MODULES` and inserted into the generated `diameter.app` (`APP_MODULES = $(RT_MODULES) $(DICT_MODULES)`), i.e. this directory is the application core.
Not here: transport implementations (`../transport/`), the dictionary compiler (`../compiler/`), debug/info modules (`../info/`), generated dictionary modules (`../gen/`, built at build time from `../dict/*.dia`).

## API Surface
Public, user-facing (documented `-moduledoc`, usable from outside the app):
- `diameter.erl` (~2080 lines) — main user API: `start/0`, `stop/0`, `start_service/2`, `stop_service/1`, `add_transport/2`, `remove_transport/2`, `call/3,4`, `session_id/1`, `origin_state_id/0`, `subscribe/1`, `unsubscribe/1`, `services/0`, `is_service/1`, `peer_info/1`, `peer_find/1`, `service_info/2`, `which_transports|watchdogs|connections/0,1`.
  Also exports the `diameter:*` AVP data types (`'OctetString'`, `'Unsigned32'`, `'Grouped'`, `'DiameterIdentity'`, …) that dictionary modules and user code reference.
- `diameter_app.erl` — two roles: the `diameter_app` **callback behaviour** for user application modules (`-callback peer_up/3`, `peer_down/3`, `pick_peer/4`, `prepare_request/3`, `prepare_retransmit/3`, `handle_request/3`, `handle_answer/4`, `handle_error/4`), and the OTP **application callback** (`-behaviour(application)`; `start/2` → `diameter_sup:start_link()`, wired via `{mod, {diameter_app, []}}` in `../diameter.app.src`).
- `diameter_codec.erl` — public encode/decode API (`encode/2,3`, `decode/2,3,4`, `collect_avps/1`, `decode_header/1`, `sequence_numbers/1`, `hop_by_hop_id/2`, `msg_name/2`, `msg_id/1`) plus the `message()`, `packet()`, `avp()`, `header()` and `dictionary()` types; `pack_data/2` and `pack_avp/2` are exported "towards diameter_gen" only.
- `diameter_service.erl` carries a `-moduledoc(#{since => "OTP 27.1"})` but an empty doc body — treat it as internal; users reach services only through `diameter.erl`.

Internal plumbing (all `-moduledoc false`), by subsystem:
- Service/configuration layer: `diameter_service`, `diameter_service_sup`, `diameter_config`, `diameter_config_sup`.
- Connection layer: `diameter_peer`, `diameter_peer_fsm`, `diameter_peer_fsm_sup`, `diameter_capx` (capabilities exchange), `diameter_watchdog`, `diameter_watchdog_sup`.
- Traffic/routing layer: `diameter_traffic`, `diameter_dist`, `diameter_callback`.
- Registry/utility servers: `diameter_reg`, `diameter_stats`, `diameter_sync`, `diameter_misc_sup`, `diameter_sup`.
- Codec/type/utility layer: `diameter_gen`, `diameter_types`, `diameter_session`, `diameter_lib`.
- `diameter_internal.hrl` — private include (released as an INTERNAL header via `INTERNAL_HRLS` in `../modules.mk`).

### Layering (data flow)
- Codec: `diameter_codec` is the generic dispatcher; it applies the *generated* dictionary module (`../gen/diameter_gen_<dict>.erl`: `id/0`, `msg_header/1`, `rec2msg/1`, `msg_name/2`, `avp_header/1`, `avp/4`, `encode_avps/3`, `decode_avps/3`), whose bodies are generated from `include/diameter_gen.hrl` and delegate the real work back into `diameter_gen` (`encode_avps/3`, `decode_avps/3`, `grouped_avp/4`, `empty_group/2`, `empty/2`).
  `diameter_gen` in turn uses `diameter_types` (basic/derived AVP formats, via `diameter_codec:pack_data/2`) and `diameter_codec:pack_avp/2`/`pack_data/2`.
  There is **no** `-behaviour(diameter_codec)` anywhere — the "dictionary module interface" is an implementation detail (documented as such in `diameter_codec`'s moduledoc), not a declared behaviour.
- Header/sequence helpers: `diameter_session` owns `sequence/0,1`, `session_id/1`, `origin_state_id/0` (Hop-by-Hop / End-to-End / Session-Id counters in the `diameter_sequence` ETS table).
- Traffic: `diameter_traffic` handles all messages *except* CER/CEA, DWR/DWA, DPR/DPA (i.e. application requests/answers), for outgoing (`send_request/4` from `diameter:call/4`) and incoming requests (`receive_message/5` called by `diameter_watchdog` in the watchdog process).
  It calls `diameter_codec` (encode/decode/collect_avps/sequence_numbers/msg_id/hop_by_hop_id/decode_header), `diameter_session:sequence`, `diameter_stats:incr`, `diameter_reg` (peer registration/lookup), `diameter_service:pick_peer/3` + `find_incoming_app/4`, and `diameter_lib` (eval, reports).

### Process / supervision structure
- `diameter_sup` is the root supervisor (`one_for_one, 1, 5`); its `init/1` creates the public named ETS tables `diameter_sequence` (set), `diameter_service` (set, keypos 3), `diameter_request` (set) and `diameter_config` (bag, keypos 2) and calls `diameter_session:init/0` (so the tables outlive any worker).
- Root children, in order: `diameter_misc_sup`, `diameter_config_sup`, `diameter_watchdog_sup`, `diameter_peer_fsm_sup`, `diameter_transport_sup` (defined in `../transport/`), `diameter_service_sup`.
- `diameter_misc_sup` (`one_for_one`) hosts the long-lived static servers: `diameter_sync`, `diameter_stats`, `diameter_reg`, `diameter_peer`, `diameter_dist`, `diameter_config`.
- `diameter_service_sup`, `diameter_watchdog_sup`, `diameter_peer_fsm_sup`, `diameter_config_sup` are all `simple_one_for_one` supervisors whose children are `temporary` — restart/failover of services, watchdogs, peer FSMs and config monitors is performed by application code, not by the supervisor.
- Registered names (from `../modules.mk` `REGISTERED`): `diameter_config`, `diameter_peer`, `diameter_reg`, `diameter_stats`, `diameter_sync`; the supervisors are additionally registered locally under their module names.
- Per-connection chain: a `diameter_service` process starts one `diameter_watchdog` per transport reference; the watchdog starts/restarts a `diameter_peer_fsm` (the connection) as the RFC 3539 state machine requires; the peer FSM starts the transport process (`../transport/diameter_transport` implementation) and performs CER/CEA exchange via `diameter_capx`.
  `diameter_traffic:peer_up/1` additionally starts an anonymous monitor process that `receive_message/5`/`send_request/4` request processes monitor, so they die when the connection is lost (failover does not kill the connection, so this process exists separately).
- `diameter_config` writes service/transport config into the `diameter_config` ETS bag (only it writes; `diameter_service` reads) so config survives service-process death, and it restarts services on death.
  For each transport configuration it additionally starts a short-lived `diameter_config` child via `diameter_config_sup:start_child/1` whose only purpose is to be published in `diameter_reg` so `diameter_reg:subscribe/2` can signal config removal.

## Constraints
- Every module includes the public header as `-include_lib("diameter/include/diameter.hrl")`.
  Most also include the private header with a *relative* path: `-include("diameter_internal.hrl")` (resolved from this directory); `diameter_app`, `diameter_callback`, `diameter_codec`, `diameter_gen`, `diameter_dist` and `diameter_types` do not include it.
- No compile-time dependency beyond those two headers: dictionary/gen modules are loaded at runtime, so `diameter_gen` must keep working with modules generated by `../compiler/diameter_codegen.erl`.
- Do not hand-edit `../gen/diameter_gen_*.erl` (generated at build time, untracked); change `include/diameter_gen.hrl` or `diameter_gen.erl` instead.
- `diameter_internal.hrl` must stay in sync with the modules here: it is released to users as `diameter/include/diameter_internal.hrl` (internal use, not covered by API compatibility guarantees).
- `diameter_service`, `diameter_peer_fsm`, `diameter_watchdog` and `diameter_peer` are compiled with `{nowarn_deprecated_function, [{erlang,exit,2}]}` (they exit/`exit/2` peers deliberately).
- Public API changes in `diameter.erl`/`diameter_app.erl`/`diameter_codec.erl` are user-visible and must keep the `-moduledoc`/`-doc`/`-since` metadata consistent.

## Design Decisions
- Error convention: `?FAILURE(Reason)` = `{{?MODULE}, {Reason}}` and `?THROW`/`?LOG`/`?UNEXPECTED`/`?ERROR` macros live in `diameter_internal.hrl`; always catch `?FAILURE(X)`, never bare `X`, since the tagging guards against catching across module boundaries.
  A few historical throws in `diameter_peer_fsm` are deliberately *not* 2-tuples and are caught elsewhere.
- Constants in `diameter_internal.hrl`: `?DIAMETER_VERSION` (1), `?APP_ID_COMMON` (0), `?APP_ID_RELAY` (16#FFFFFFFF), plus the RFC 3588 peer/realm table records `#diameter_peer{}`/`#diameter_realm{}`.
- `diameter_sup` owns shared ETS tables rather than the servers that use them, so that a crashed server does not lose counter/registry state.
- `diameter_service` keeps per-service private ETS tables (a watchdog table plus fingerprinted local/remote lookup tables built from `ets:new/1`) holding `#watchdog{}` and `#peer{}` records; it also keeps a copy of the service's `#diameter_service{}` record and the parsed service-option map.
- `diameter_sync` serializes requests as named queues, each request applied in its own process; `diameter_config` uses it to serialize per *service* (rather than globally) so that stopping several services can be done concurrently by the caller.
- `diameter_stats` stores counters keyed `{Counter, Pid|Ref}` and folds a dying contributor's counters into the `Ref` entry, so per-connection counters survive connection death for service-level sums.
- `diameter_reg` is a simple term→pid bag registry with `wait/1` (blocking, over a match pattern) and `subscribe/2` (notification), with per-pid monitors to keep the notify map bounded; it replaces the RFC's abstract "peer table"/"realm routing table" for discovery purposes.
- `diameter_dist` implements the `spawn_opt` transport hooks (`spawn_local/1,2`, `route_session/1,2`) to distribute incoming requests across local/remote handler processes using consistent hashing (`hash/2,3`); it keeps the node/service tables `diameter_dist_node` and `diameter_dist_service` and requires `attach/1` on nodes willing to take work.
- `diameter_capx` builds CER/CEA from service configuration; the old per-dictionary `cer/0` callback is still consulted for backwards compatibility but is undocumented and deprecated.
- Generated `.app` wiring: this directory is self-contained at runtime — `diameter_peer_fsm` deliberately does *not* include a common-dictionary hrl because the common dictionary is a runtime argument to `start/3`.

## Known Issues / Notes for Agents
- `diameter_request` ETS table: the name is historic. It no longer holds outstanding requests; it maps transport pid → the failover monitor process started by `diameter_traffic:init/1`.
- `diameter.erl` has no functions before line ~1090: the file is one huge `-moduledoc` plus `-export`/`-export_type` lists, and the two `-include` directives sit *after* them (line ~1088). Any code added above that line will not see the records.
- The RFC 3539 state machines here deviate slightly from the RFC: there is no `INITIAL -> DOWN` transition in RFC 3539, the one in `diameter_watchdog` is a consequence of `stop`.
- `diameter_lib:spawn_opts/2` sets `min_heap_size` to 75000 (server) / 5000 (worker) for historical reasons (it "can bloat many processes"); it can be disabled with the `-diameter min_heap_size false` application env var.
- `#diameter_packet.errors` defaults to `[]` rather than `undefined`, so "set nothing" is signalled with the atom `false` in `diameter_traffic:make_answer_packet` (changing the default would break callers relying on the old value).
- Grouped AVPs are represented as improper lists internally (`-dialyzer(no_improper_lists)` in `diameter_traffic`); a Grouped AVP's decoded component list is *not* retained when packed into a message record — the `AVP` field stays flat (historical).
- Grouped/Enumerated types are handled directly by `diameter_gen.hrl`/`diameter_codec`, *not* by `diameter_types` (see the comment at the top of `diameter_types`).
- External funs (`fun Mod:F/Arity`) must not be stored in long-lived configuration (service/transport options) because they pin old code across upgrades.
- `incoming_maxlen` is handled in `diameter_peer_fsm`, not `diameter_traffic`, although the option is carried in `diameter_traffic`'s `#recvdata{}` codec map.
- `diameter_peer_fsm` cannot guarantee at connect time that the service/transport capabilities can actually build an encodable CER/CEA — that is only checked once the exchange happens.
- Message ordering/numbering: `sequence/1` masks the Hop-by-Hop and End-to-End counters out of the shared `sequence` counter; changing the mask affects both.

## Dependencies
- Runtime: `stdlib` + `kernel` only are declared; `ssl` is required only if TLS transports are configured, and `syntax_tools`/`runtime_tools` only if the dictionary compiler / info modules are loaded.
- `diameter_lib:getenv/1` reads the `diameter` application env var `min_heap_size` (used by `spawn_opts/2`); it is the only env var read here.

## Test Strategy
- Suites live in `../test/` and are run with Common Test; the app Makefile sets `TEST_NEEDS_RELEASE=true`, so tests run against an installed release.
- Relevant suites by area (do not edit from here — `../test/` is a sibling): `diameter_traffic_SUITE`, `diameter_watchdog_SUITE`, `diameter_capx_SUITE`, `diameter_codec_SUITE` (+ `diameter_codec_SUITE_data`, `diameter_codec_test.erl`), `diameter_config_SUITE`, `diameter_reg_SUITE`, `diameter_stats_SUITE`, `diameter_sync_SUITE`, `diameter_dist_SUITE`, `diameter_distribution_SUITE`, `diameter_app_SUITE`, `diameter_event_SUITE`, `diameter_failover_SUITE`, `diameter_dpr_SUITE`, `diameter_3xxx_SUITE`, `diameter_length_SUITE`, `diameter_pool_SUITE`, `diameter_relay_SUITE`, `diameter_gen_tcp_SUITE`, `diameter_gen_sctp_SUITE`.
- Shared test helpers: `../test/diameter_ct.erl`, `../test/diameter_util.erl`, `../test/diameter_event_logger.erl`.

## Routing Table
Directory is flat; route by subsystem:
- Service / configuration: `diameter_service.erl`, `diameter_service_sup.erl`, `diameter_config.erl`, `diameter_config_sup.erl` — service lifecycle, per-service lookup tables, config persistence + restart.
- Connection / capabilities: `diameter_peer.erl` (transport-facing gen_server), `diameter_peer_fsm.erl`, `diameter_peer_fsm_sup.erl`, `diameter_capx.erl`, `diameter_watchdog.erl`, `diameter_watchdog_sup.erl` — connection establishment, CER/CEA, RFC 3539 liveness.
- Traffic / routing: `diameter_traffic.erl` (requests, answers, relaying, counters), `diameter_dist.erl` (node-level distribution), `diameter_callback.erl` (selective/arity-adjusting callback dispatch).
- Shared services: `diameter_sup.erl` (root supervisor + shared ETS), `diameter_misc_sup.erl`, `diameter_reg.erl`, `diameter_stats.erl`, `diameter_sync.erl`.
- Codec / types: `diameter_codec.erl`, `diameter_gen.erl`, `diameter_types.erl`, `include/diameter_gen.hrl` (in `../include/`).
- Utility / public API: `diameter.erl`, `diameter_app.erl`, `diameter_lib.erl`, `diameter_session.erl`, `diameter_internal.hrl`.

## See Also
- `../CONTEXT.md` — the `diameter` application as a whole (app version, `modules.mk` partitions, dictionary compiler, examples).
- `../transport/` — `diameter_transport` behaviour plus TCP/SCTP/`diameter_etcp` implementations (sibling — read-only, escalate writes to the parent).
- `../compiler/` — `diameter_codegen` and the `diameterc` compiler that generate the dictionary modules this directory's codec drives (sibling — read-only).
- `../include/diameter.hrl`, `../include/diameter_gen.hrl` — public records and the generated-codec prelude (sibling — read-only).
- `../test/` — test suites for this area (sibling — read-only).
- `../info/` — `diameter_info`/`diameter_dbg` debug helpers that inspect these processes (sibling — read-only).
