# SNMP Agent — `lib/snmp/src/agent`

## Intent
Implements the agent side (master agent + sub-agent support) of the `snmp` application: PDU message processing, MIB server, built-in MIB database, SNMPv3 USM/VACM security, notification/trap sending, the standard MIBs and the public `snmpa` API.
Sources are compiled into `lib/snmp/ebin` and installed into `lib/snmp-<VSN>/ebin` (sources under `lib/snmp-<VSN>/src/agent`) by the `release_spec` target.

## API Surface

### (a) Public, documented modules (`-moduledoc`)
- `snmpa.erl` — main agent interface: start/stop, MIB load/unload, notification & trap sending, agent info, symbolic-name translation, filter/mechanism registration; it also defines the agent-specific types consumed by other modules here.
- `snmpa_conf.erl` — read/write/append helpers for the agent config files (`agent.conf`, community/target/notify/…).
- `snmpa_supervisor.erl` — documented start/stop API for agent, sub-agents and master agent, and the reference for the agent process tree.
- `snmpa_error.erl` + `snmpa_error_io.erl` + `snmpa_error_logger.erl` — error-reporting entry point and the two shipped report modules.
- `snmpa_local_db.erl` — public API of the built-in MIB database (volatile ets / persistent dets).
- `snmpa_mpd.erl` — version-independent Message Processing & Dispatch, meant to be called from network-interface processes (see `snmp_agent_netif.md`).
- `snmp_generic.erl`, `snmp_index.erl` — helpers for implementing MIB tables/variables and the SNMP index ADT.
- `snmp_community_mib`, `snmp_framework_mib`, `snmp_notification_mib`, `snmp_standard_mib`, `snmp_target_mib`, `snmp_user_based_sm_mib`, `snmp_view_based_acm_mib` — the standard MIBs the agent implements.

### (b) Behaviours users implement/plug in (declared in `BEHAVIOUR_MODULES`)
- `snmpa_mib_storage` — MIB variable storage backend.
- `snmpa_mib_data` — MIB-server data structure.
- `snmpa_network_interface` — the agent net-if process.
- `snmpa_network_interface_filter` — accept/reject hook on send/receive; default implementation is `snmpa_net_if_filter`.
- `snmpa_authentication_service` — access control; default implementation is `snmpa_acm`.
- `snmpa_get_mechanism` / `snmpa_set_mechanism` — replaceable GET/GET-NEXT/GET-BULK and SET engines; default implementations are `snmpa_get` / `snmpa_set`.
- `snmpa_discovery_handler` — discovery of new SNMPv3 users; default implementation is `snmpa_discovery_handler_default`.
- `snmpa_error_report` — error-report callback; implementations `snmpa_error_io`, `snmpa_error_logger`.
- `snmpa_notification_filter` — modify/suppress outgoing notifications.
- `snmpa_notification_delivery_info_receiver` — receives delivery info for confirmed notifications.

### (c) MIB server, storage and instrumentation internals
- `snmpa_mib.erl` — the MIB-server process.
- `snmpa_mib_data_tttn.erl` — the only shipped `snmpa_mib_data` implementation (TTTN = TupleTreeTupleNodes: mib-storage + OID tree + registered subagents).
- `snmpa_mib_storage_ets.erl`, `snmpa_mib_storage_dets.erl`, `snmpa_mib_storage_mnesia.erl` — the three shipped `snmpa_mib_storage` backends.
- `snmpa_mib_lib.erl`, `snmpa_get_lib.erl`, `snmpa_set_lib.erl`, `snmpa_svbl.erl` — helpers; `snmpa_svbl` does varbind-list sorting/splitting for GET/SET.
- `snmpa_general_db.erl` (generic ets/mnesia table interface), `snmp_generic_mnesia.erl`, `snmp_shadow_table.erl`, `snmpa_symbolic_store.erl` (name/alias ↔ OID store kept outside the MIB to avoid deadlock).

### (d) Processes, supervision and startup
- `snmpa_app.erl` — converts legacy application-env config into the current start options.
- `snmpa_agent_sup.erl` — supervises master agent and sub-agents (sub-agents default to plain `snmpa_mib_storage_ets`).
- `snmpa_misc_sup.erl` — supervises the mib-server, net-if and note-store processes (one mib process per agent).
- `snmpa_agent.erl` — the master-agent process (largest module).
- `snmpa_supervisor` children: `snmpa_misc_sup`, `snmpa_target_cache`, `snmpa_symbolic_store`, `snmpa_local_db`, `snmpa_agent_sup`.

### (e) SNMPv3 security and notification handling
- `snmpa_usm.erl` (USM, RFC 3414; AES, RFC 3826), `snmpa_vacm.erl` (+ `snmpa_vacm.hrl`), `snmpa_acm.erl`.
- `snmpa_trap.erl` (notification/trap construction and sending), `snmpa_target_cache.erl` (cached target/notify parameters).
- `snmpa_net_if.erl` (default UDP network interface) and `snmpa_net_if_filter.erl` (default net-if filter implementation).

### (f) Internal headers (`INTERNAL_HRL_FILES`)
- `snmpa_internal.hrl` — includes `snmp/src/app/snmp_internal.hrl`; defines `DEFAULT_LOCAL_ENGINE_ID` (master agent only), `DEFAULT_NOTIF_EXTRA_INFO`, `DEFAULT_GB_MAX_VBS` and the `snmpa_info/warning/error` logging macros.
- `snmpa_vacm.hrl` — VACM table column indices.
- `snmpa_atl.hrl` — audit-trail log name/file.
- `EXT_HRL_FILES` is empty: the agent exports no public headers; the three internal headers are installed under `src/agent`, not `include/`.

## Build / Constraints
- `Makefile.in` is the tracked source; the generated `Makefile` is git-ignored (`lib/snmp/.gitignore` lists `src/agent/Makefile`) — never edit the generated file.
- `modules.mk` declares `MODULES` (the 43 compiled modules, incl. `MIB_MODULES`), `BEHAVIOUR_MODULES` (11), `MIB_MODULES` (7) and `INTERNAL_HRL_FILES`; `depend.mk` lists per-module `.erl`/header dependencies.
- Compile-order constraint (from the `modules.mk` comment): `snmpa` also defines agent-specific types and must be compiled before the modules that use them, including the behaviour modules; some MIB modules likewise define types used elsewhere and must be built before the remaining modules — keep `snmpa` and `MIB_MODULES` first in `MODULES`.
- `opt` runs `behaviour_targets` (`BEHAVIOUR_MODULES`) before `targets` (`MODULES`).
- `ERL_COMPILE_FLAGS`: `-pa $(ERL_TOP)/lib/snmp/ebin`, `-I../../include -I../app -I../misc -I$(ERL_TOP)/lib/stdlib`, `-Dversion=\"$(VSN)$(PRE_VSN)\"`, `+'{parse_transform,sys_pre_attributes}'`, `+nowarn_missing_spec_documented`, `+nowarn_deprecated_catch`.
- Warnings are errors by default (`-Werror`, disable with `SNMP_WARNING_AS_ERROR=false`); `+warn_unused_vars` is on by default (`WARN_UNUSED_VARS=false` to disable).
- `SNMP_FLAGS` are controlled by `SNMP_DEFAULT_VERBOSITY`, `SNMP_DEBUG=d`, `SNMP_QC`, `SNMP_EXT_VERBOSITY` and `SNMP_EMPTY_PDU_SIZE`.

## Design Decisions
- Behaviour modules make the network interface, MIB storage/data, access control, GET/SET engines, discovery and error reporting user-replaceable without touching the agent core; each is selected via application env / `snmpa` config.
- `snmpa_mpd` is deliberately independent of version and transport so custom network interfaces can drive PDU processing directly.
- The "generic MIB" mechanism (`snmp_generic` + `snmp_index` + `snmpa_local_db`/Mnesia) supplies default instrumentation so simple tables need no user code.
- MIB data layout is pluggable via `snmpa_mib_data`, with `snmpa_mib_data_tttn` as the default implementation.

## Notes for Agents
- No `.erl`/`.hrl` file in this directory is generated — all are handwritten sources; only `Makefile` is generated from `Makefile.in`.
- Module docs come from the `-moduledoc` attributes in the sources; user-facing prose lives in `lib/snmp/doc/guides/` (`snmp_agent_funct_descr.md`, `snmp_agent_netif.md`, `snmp_agent_config_files.md`, `snmp_advanced_agent.md`, `snmp_impl_example_agent.md`).
- Screenshots/diagrams for the agent docs live in `lib/snmp/doc/src/` (`structure.gif`, `MIB_mechanism.fig`).

## Test Strategy
- Agent tests live in the sibling `lib/snmp/test/` (`snmp_agent_SUITE`, `snmp_agent_conf_SUITE`, `snmp_agent_mibs_SUITE`, `snmp_agent_nfilter_SUITE`, plus `snmp_agent_test_lib`/`snmp_agent_test_get`) and are run with `make test` in `lib/snmp`.
- Nothing under this directory is test code; `lib/snmp/test/Makefile` (`MODULES`, `RELSYSDIR`) controls installation of test code.

## Routing Table
- Public agent API / config files → `snmpa.erl`, `snmpa_conf.erl`
- Startup, application env conversion, supervision → `snmpa_app.erl`, `snmpa_agent_sup.erl`, `snmpa_misc_sup.erl`, `snmpa_supervisor.erl`, `snmpa_agent.erl`
- MIB server & storage → `snmpa_mib.erl`, `snmpa_mib_data.erl`, `snmpa_mib_data_tttn.erl`, `snmpa_mib_storage{,_ets,_dets,_mnesia}.erl`, `snmpa_mib_lib.erl`
- MIB instrumentation / DB helpers → `snmp_generic.erl`, `snmp_generic_mnesia.erl`, `snmp_index.erl`, `snmpa_local_db.erl`, `snmpa_general_db.erl`, `snmp_shadow_table.erl`, `snmpa_symbolic_store.erl`
- Standard MIBs → `snmp_community_mib.erl`, `snmp_framework_mib.erl`, `snmp_notification_mib.erl`, `snmp_standard_mib.erl`, `snmp_target_mib.erl`, `snmp_user_based_sm_mib.erl`, `snmp_view_based_acm_mib.erl`
- SNMPv3 security → `snmpa_usm.erl`, `snmpa_vacm.erl`, `snmpa_vacm.hrl`, `snmpa_acm.erl`
- Message processing & network interface → `snmpa_mpd.erl`, `snmpa_net_if.erl`, `snmpa_net_if_filter.erl`
- Notifications/traps → `snmpa_trap.erl`, `snmpa_target_cache.erl`, `snmpa_notification_filter.erl`, `snmpa_notification_delivery_info_receiver.erl`
- GET/SET engines → `snmpa_get.erl`, `snmpa_get_lib.erl`, `snmpa_get_mechanism.erl`, `snmpa_set.erl`, `snmpa_set_lib.erl`, `snmpa_set_mechanism.erl`, `snmpa_svbl.erl`
- Error reporting → `snmpa_error.erl`, `snmpa_error_report.erl`, `snmpa_error_io.erl`, `snmpa_error_logger.erl`
- Discovery / authentication services → `snmpa_discovery_handler.erl`, `snmpa_discovery_handler_default.erl`, `snmpa_authentication_service.erl`
- `../misc/` → shared PDUs, config, USM and verbosity/debug headers (sibling — read-only, escalate writes to `../`)
- `../app/` → snmp application resource, umbrella API and `snmp_internal.hrl` (sibling — read-only, escalate writes to `../`)
- `../manager/` → manager-side implementation (sibling — read-only from here)
- `../../test/` → agent test suites and test libraries (read-only; escalate writes to `../../`)
- `../../examples/ex1/` → example agent MIB instrumentation (`EX1-MIB.funcs`)

## See Also
- `../CONTEXT.md` — snmp `src/` build layout (`subdirs.mk`, per-dir `Makefile`/`modules.mk`/`depend.mk` conventions).
- `../../CONTEXT.md` and repo-root `CONTEXT.md` — snmp application and OTP build overview.
