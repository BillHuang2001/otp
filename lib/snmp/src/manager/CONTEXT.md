# SNMP Manager Implementation (`snmpm`)

## Intent
The SNMP **manager** (client/requesting) side of the `snmp` application.
Implements the manager entity: registering users and agents, issuing sync/async get/get-next/get-bulk/set requests, handling traps/informs/reports, SNMPv3 USM security, and audit-trail logging.
Built as the `manager` subdirectory of `../` (`SUB_DIRECTORIES = compile app misc agent manager`); the agent side lives in `../agent/`.

## API Surface
Public (documented) modules:
- `snmpm.erl` — the only public manager API (`start`/`start_link`/`stop`, `register_user[_monitor]`, `register_agent`, `register_usm_user`, `sync_get2`/`async_get2`/`sync_get_next2`/`sync_get_bulk2`/`sync_set2` + async variants, `cancel_async_request`, `load_mib`/`unload_mib`, `log_to_txt`/`log_to_io`/`change_log_size`/`get_log_type`/`set_log_type`, `reconfigure`, `info`, `verbosity`, `restart`). Exports the public types (`target_name`, `user_id`, `agent_config`, `usm_config_item`, `pdu_type`, `snmp_reply`, …). Internals `format_reason/1,2`, `start_link/3`, `target_name/1,2` are application-internal.
- `snmpm_conf.erl` — utilities to write/append/read the manager **config files** (`manager.conf`, `users.conf`, `agents.conf`, `usm.conf`): `*_entry/*`, `write_*_config`, `append_*_config`, `read_*_config`. Distinct from the runtime store in `snmpm_config`.
- `snmpm_mpd.erl` — version-independent Message Processing & Dispatch for the manager (`process_msg`, `generate_msg`, `generate_response_msg`, `next_msg_id`, `next_req_id`); documented for use by a Network Interface process.

Behaviour definitions a user must implement (external contract):
- `snmpm_user.erl` — behaviour of the manager user: callbacks `handle_error/3`, `handle_agent/5`, `handle_pdu/4`, `handle_trap/3`, `handle_inform/3`, `handle_report/3`, and optional `handle_invalid_result/2`. Exports types `snmp_gen_info/0`, `snmp_v1_trap_info/0`.
- `snmpm_network_interface.erl` — behaviour of the manager net-if: `start_link/2`, `stop/1`, `send_pdu/7`, `inform_response/4`, `note_store/2`, `info/1`, `get_log_type/1`, `set_log_type/2`, `verbosity/2`.
- `snmpm_network_interface_filter.erl` — behaviour of the net-if filter: `accept_recv/2`, `accept_send/2`, `accept_recv_pdu/3`, `accept_send_pdu/3`; provides `verify/1`.
- `snmpm_user_old.erl` — legacy (pre-R16) user behaviour, exposed only via `behaviour_info/1` with old `IpAddr`/`PortNumber`-style arities.

Shipped default/implementation modules (internal, `-moduledoc false`):
- `snmpm_server.erl` — main manager `gen_server`; implements registration, request/response handling, the user-callback proxy (`do_handle_*`), audit-trail log, and async timeout handling. Exports the server-level API called by `snmpm` (`sync_get`/`async_get`/…, `register_user/4`, `info/0`, …).
- `snmpm_config.erl` — central `gen_server` + ETS `snmpm_config_table` holding users, agents, USM users, MIBs, engine id, and stats counters; the read/write config store for `snmpm_server` and the net-if.
- `snmpm_net_if.erl` — default network interface (`gen_server` implementing `snmpm_network_interface`); UDP transport, `send_pdu/6,7,8`, ATL log management, calls `snmpm_mpd` for message processing.
- `snmpm_net_if_mt.erl` — multi-threaded net-if variant, literally `-include("snmpm_net_if.erl")` under the `snmpm_net_if_mt` define.
- `snmpm_net_if_filter.erl` — default filter, accepts everything (implements `snmpm_network_interface_filter`).
- `snmpm_user_default.erl` — default `snmpm_user` implementation that logs (`error_logger:info_msg`) and returns `ignore`/`no_reply`.
- `snmpm_usm.erl` — manager-side USM: `init/0`, `reset/0`, `process_incoming_msg/4`, `generate_outgoing_msg/5`.
Supervisors:
- `snmpm_supervisor.erl` — top manager supervisor (`one_for_all`): children `snmpm_config`, `snmpm_misc_sup`, `snmpm_server_sup`.
- `snmpm_server_sup.erl` — `one_for_all` supervisor wrapping the single `snmpm_server` worker.
- `snmpm_misc_sup.erl` — supervises the `net_if` child (started with a configurable net-if module via `start_net_if/2`) and the shared `snmp_note_store` child.
Internal headers (NOT public API):
- `snmpm_usm.hrl` — `#usm_user{}` record.
- `snmpm_atl.hrl` — audit-trail log name/file defines (`audit_trail_log_name`, `audit_trail_log_file`).
- `snmpm_internal.hrl` — logging macros `?snmpm_info/2`, `?snmpm_warning/2`, `?snmpm_error/2`, default extra-info/context defines, and `-include_lib("snmp/src/app/snmp_internal.hrl")`.

## Build
- `Makefile`, `modules.mk`, `depend.mk` are tracked directly (this dir has no `Makefile.in`; only `../agent/Makefile` is generated from one).
- `modules.mk` splits `BEHAVIOUR_MODULES` (the four behaviour modules) from `MODULES` and lists `INTERNAL_HRL_FILES`; both behaviour and normal modules are compiled to `../../ebin`.
- Flags (from `Makefile`): `-Werror`, `+warn_unused_vars`, `-I../../include -I../misc`, `-Dversion=\"$(VSN)$(PRE_VSN)\"`, `+'{parse_transform,sys_pre_attributes}'`, `SNMP_FLAGS` (`-Ddefault_verbosity=silence` unless overridden; `-Dsnmp_debug` when `SNMP_DEBUG=d`).
- `release_spec` installs `*.erl` and internal `*.hrl` to `$(RELSYSDIR)/src/manager` and the beams to `$(RELSYSDIR)/ebin`.

## Constraints
- Code compiles with `-Werror` and `+warn_unused_vars` — warnings break the build.
- The four behaviour modules (`snmpm_user`, `snmpm_user_old`, `snmpm_network_interface`, `snmpm_network_interface_filter`) are external contracts: callback names/arities/return contracts are public API and must stay backward compatible.
- The `*.hrl` here are internal; do not treat them as public headers (public types are exported from `snmpm` and `snmp` in `../misc/`).
- Legacy arities must keep working for backwards compatibility (`send_pdu/6,7,8` in net-if, `process_msg/7` in mpd, old net-if filter arguments on transport-less configs).

## Design Decisions
- The manager user and the network interface are pluggable **behaviours** so applications can supply their own callback handling and transport; the application ships defaults (`snmpm_user_default`, `snmpm_net_if*`, `snmpm_net_if_filter`).
- Single- vs multi-threaded net-if: `snmpm_net_if_mt` is not separate source but `snmpm_net_if.erl` compiled again under the `snmpm_net_if_mt` define (different socket/message handling branches); the active module is selected via the `net_if` `{module, …}` config option (default `snmpm_net_if`). Both use UDP.
- `snmpm_user_old` preserves the pre-R16 user behaviour (callbacks with separate `IpAddr`/`PortNumber` args) for users who have not migrated to the domain-based `snmpm_user`.
- `snmpm_server` runs user callbacks through a proxy (`do_handle_*`/`cbproxy_loop`) so a misbehaving user callback cannot block the manager server.
- `snmpm_conf` (file I/O) is deliberately separate from `snmpm_config` (in-memory runtime store).

## Notes for Agents
- `snmpm_net_if_mt.erl` deliberately has no code of its own (it `-include`s `snmpm_net_if.erl`) — do not "split" or duplicate logic into it; edit `snmpm_net_if.erl`.
- `snmpm_server.erl`, `snmpm_config.erl` and `snmpm.erl` are very large (>2000 lines each) — search by export/callback name rather than reading whole files.
- `snmpm_usm` here is the manager side; the agent side / shared USM record is `snmp_usm.erl` + `snmp_usm.hrl` in `../misc/`.

## Test Strategy
- Manager tests live in the sibling `../../test/` directory (read-only from here): `snmp_manager_SUITE`, `snmp_manager_config_SUITE`, `snmp_manager_user_SUITE`, with helpers `snmp_manager_user.erl`, `snmp_manager_user_old.erl`, `snmp_manager_user_test_lib.erl`, `snmp_test_manager.erl`.
- Run with Common Test from `lib/snmp` (`make test` / `ct_run`) against a built release; there are no test suites inside this directory.

## Routing Table
Flat directory — module groups map to files (no child subdirectories):
- `snmpm.erl`, `snmpm_conf.erl` → public manager API and config-file utilities.
- `snmpm_user.erl`, `snmpm_user_old.erl`, `snmpm_network_interface.erl`, `snmpm_network_interface_filter.erl` → behaviour definitions (public contracts).
- `snmpm_server.erl`, `snmpm_server_sup.erl`, `snmpm_supervisor.erl`, `snmpm_misc_sup.erl` → manager server + supervision tree.
- `snmpm_net_if.erl`, `snmpm_net_if_mt.erl`, `snmpm_net_if_filter.erl`, `snmpm_user_default.erl` → shipped default implementations.
- `snmpm_mpd.erl`, `snmpm_usm.erl`, `snmpm_config.erl` → message processing/dispatch, USM security, runtime config store.
- `snmpm_usm.hrl`, `snmpm_atl.hrl`, `snmpm_internal.hrl` → internal headers.
- `Makefile`, `modules.mk`, `depend.mk` → build wiring.

Cross-references (siblings — read-only, escalate writes to `./lib/snmp/src`):
- `../misc/` → shared PDUs/config/USM (`snmp_usm.erl`, `snmp_usm.hrl`, `snmp_note_store.erl`, `snmp_debug.hrl`, `snmp_verbosity.hrl`).
- `../agent/` → the SNMP agent counterpart (analogous `snmpa_*` behaviours/servers).
- `../app/` → application module and `snmp.app.src` module list.
- `../../include/` → public headers (`snmp_types.hrl`, `SNMP-*-MIB.hrl`, …).
- `../../test/` → manager test suites (see Test Strategy).

## See Also
- `../CONTEXT.md` — the `snmp/src` subdirectory layout; `../misc/CONTEXT.md` for shared components.
- `lib/snmp/doc/guides/snmp_manager_netif.md`, `snmp_manager_funct_descr.md`, `snmp_manager_config_files.md` — manager documentation.
