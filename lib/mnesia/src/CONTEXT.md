# mnesia/src — Mnesia DBMS sources

## Intent
All Erlang sources of the Mnesia application (one OTP app, `MNESIA_VSN = 4.27`, described as "MNESIA CXC 138 12").
Implements a distributed, replicated, transactional (soft-real-time) DBMS on top of `ets`/`dets`/`disk_log` plus pluggable external backends.
No subdirectories — every module and the shared header `mnesia.hrl` live directly here.

## Module Inventory (by layer)
Public API / activity layer:
- `mnesia.erl` — the public interface AND the default activity/`access_module` (implements the `mnesia_access` behaviour); exports the whole user API plus internal helpers marked "Not for public use". Contains the large `-moduledoc` (tables, props, config params).
- `mnesia_frag.erl` — `mnesia_access` callbacks for *fragmented* tables: routes each read/write/lock op to the right fragment; also fragment-management API.
- `mnesia_frag_hash.erl` — defines the `mnesia_frag_hash` callback behaviour and the default *dynamic (linear) hashing* implementation.

Transaction / concurrency layer:
- `mnesia_tm.erl` — the single transaction-manager process; coordinator/participant bookkeeping (`#prep{}`), 2-phase commit (sym/asym, sync/async variants), dirty ops, activity IDs, checkpoint/snmp hooks, `sync/0`.
- `mnesia_locker.erl` — lock manager: read/write/table locks, lock queues, queueing/wait & cyclic (deadlock) resolution (`#cyclic{}`).
- `mnesia_checkpoint.erl` — named table-snapshot manager; `tm_*` callbacks invoked by `mnesia_tm`, plus checkpoint iterate/deactivate API.
- `mnesia_checkpoint_sup.erl` — `simple_one_for_one` supervisor for checkpoint processes.

Data-access / global-variable layer:
- `mnesia_lib.erl` — "everything that does not fit elsewhere": the `mnesia_gvar` global-variable access (`set/2`, `val/1`, `unset/1`, `add/del/add_list`), node/storage-type helpers (`cs_to_nodes`, `cs_to_storage_type`), `db_*` record-access wrappers, counters, `fatal/error/verbose/dbg_out`, and `eval_debug_fun/4`.
- `mnesia_index.erl` — secondary-index maintenance (per `{Table,Attr}` index tables; add/del/clear index, dirty index lookups).
- `mnesia_backend_type.erl` — *behaviour definition* only (`behaviour_info(callbacks)`); declares the contract for external/plugin storage backends. All callbacks are treated as mandatory.

Persistence / storage layer:
- `mnesia_dumper.erl` — dumps the transaction log into the `.DCD`/`.DCL`/dat files (`perform_dump`, `dump_to_logfile`, `load_from_logfile`); owns the `mnesia_dumper_load_regulator` process.
- `mnesia_log.erl` — administers three log kinds: the transaction log, the mnesia_down log and the schema/backup log; also `tab_copier`.
- `mnesia_loader.erl` — loads a table from local disc (`disc_load_table/3`) or from a remote node (`net_load_table/4`, `send_table/4`).
- `mnesia_late_loader.erl` — background server (registered `mnesia_late_loader`) for asynchronous "late" disc loading; also triggers `merge_schema`.
- `mnesia_backup.erl` — the default backup/restore *media* callback module (`mnesia_backup` behaviour); treats its `OpaqueData` argument as private.
- `mnesia_bup.erl` — orchestrates backup/restore and the *fallback* mechanism (`iterate/4`, `create_schema`, `install_fallback`, `tm_fallback_start`, `restore`).
- `mnesia_text.erl` — text-file backup format (`load_textfile/1`, `dump_to_textfile/1`, `parse/1`) usable as an interchange format.
- `mnesia_registry.erl` — deprecated stub: only a `-removed([{create_table,...}])` attribute, no functions.

Schema / config / controller layer:
- `mnesia_schema.erl` — schema transactions and table DDL: create/delete table, add/del table copies, indices, table properties; schema `init/1`, backend & index-plugin registration, `#cstruct{}` manipulation.
- `mnesia_monitor.erl` — `gen_server` that owns environment/config (`get_env`, `patch_env`), creates the shared ets tables (`mnesia_gvar`, `mnesia_stats`, `mnesia_debug`) and other `mktab` tables, and provides `start_proc/terminate_proc` helpers.
- `mnesia_controller.erl` — the "mnesia_init" process: manages table *whereabouts*, load/send/dump request queues, table activation/deactivation and node connectivity.
- `mnesia_recover.erl` — log/transaction recovery, `mnesia_down` tracking, neighbour handshake, master-node info, tid-serial maintenance.
- `mnesia_subscr.erl` — table subscriber registry and events (`subscribe/2`, `report_table_event/*`, `report_activity/1`).
- `mnesia_event.erl` — the default `event_module` handler (a `gen_event` handler) started as `mnesia_event`; emits/logs Mnesia system events.
- `mnesia_rpc.erl` — dedicated RPC `gen_server` (registered `mnesia_rpc`) that avoids overloading the system rpc server during heavy dirty operations.
- `mnesia_snmp_hook.erl` — SNMP integration hooks called from `mnesia` (`check_ustruct`, `key_to_oid`, `get_row`, `update`, …).

Supervision / application layer:
- `mnesia_app.erl` — `application` callback (`start/2`, `stop/1` → `mnesia_sup:start_link/1`).
- `mnesia_sup.erl` — top supervisor (`one_for_all`) for the whole app; starts `mnesia_event`, `mnesia_ext_sup`, `mnesia_kernel_sup`; `kill/0` debug helper.
- `mnesia_kernel_sup.erl` — supervises the core workers in fixed order: `mnesia_monitor`, `mnesia_subscr`, `mnesia_locker`, `mnesia_recover`, `mnesia_tm`, `mnesia_rpc`, `mnesia_checkpoint_sup`, `mnesia_controller`, `mnesia_late_loader`.
- `mnesia_ext_sup.erl` — supervisor for external (plug-in) processes (`start_proc/stop_proc`).
- `mnesia_sp.erl` — tiny helper (`init_proc/4`) that runs a start MFA under a catch to produce nicer crash reports; intentionally non-purgeable.

## Cross-module contracts
- `mnesia_gvar` — a `set, public, named_table` ets table created by `mnesia_monitor:init/1`; the shared in-memory DB of config, table whereabouts and cstruct fields.
  All access goes through `mnesia_lib:set/2` / `val/1` or the `?catch_val/1` macro (`mnesia.hrl`), which returns `{'EXIT',{badarg,[]}}` when a var is missing. Keys are atoms (e.g. `mnesia_status`, `dir`) or `{Table, Prop}` tuples.
- Records in `mnesia.hrl`: `#cstruct{}` (per-table config, the master table description), `#tid{}` (`counter` MUST stay first because tids are compared), `#tidstore{}` (nested-transaction store), `#commit{}`/`#decision{}` (log + commit-protocol records), `#cyclic{}` (lock-wait info), and `#log_header{}` which may **never** change (very old backups depend on it).
- `mnesia_access` behaviour — the API between `mnesia.erl` (dispatcher/default impl) and an activity module; ops are called as `AccessMod:read(ActivityId, Opaque, Tab, Key, LockKind)` etc. `mnesia` and `mnesia_frag` implement it (`%-behaviour(mnesia_access)` is commented out in both).
- `mnesia_backend_type:behaviour_info(callbacks)` — the external-backend callback contract (`create_table/3`, `load_table/4`, `lookup/3`, `insert/3`, `select/1`, `select/3`, `select/4`, `add_aliases/1`, `real_suffixes/0`, …); `mnesia_schema:add_backend_type/2` verifies a module's exports against this list.
- `mnesia_tm` drives `mnesia_checkpoint:tm_*`, `mnesia_locker`, `mnesia_log`/`mnesia_dumper` and the participants during the commit protocol.

## Constraints
- Compile flags are strict (`src/Makefile`): `-Werror +warn_obsolete_bool_op +nowarn_deprecated_catch` + parse transform `sys_pre_attributes` + inserted `vsn` attribute. Warnings fail the build.
- Any new module must be added to both `MODULES` (`src/Makefile`) and the `modules` list in `mnesia.app.src` — they must stay in sync.
- Do not change the `#log_header{}` record in `mnesia.hrl`.
- Sources are compiled with `-include("mnesia.hrl")`; it is the only internal header.

## Design Decisions
- Global state lives in the `mnesia_gvar` ets table (public named table) instead of being threaded through processes — every module reads table/config info via `mnesia_lib` (or a local `val/1` wrapper that borrows the `?catch_val_and_stack` path).
- There is exactly one transaction-manager process (`mnesia_tm`); the activity runs in the caller and coordinates with it. `mnesia_lib` (`other_val/2`) turns a missing gvar into a clean `mnesia:abort/1` instead of a raw `badarg`.
- Activity semantics are pluggable via the `mnesia_access` behaviour: `mnesia` is the default module and `mnesia_frag` overrides it for fragmented tables.
- `mnesia_rpc` exists so a flood of dirty operations cannot overload the standard `rpc` server.

## Known Issues / gotchas
- `mnesia.erl` is both the public API and the default activity module — there is **no** `mnesia_activity.erl` in this tree; describe activity logic as living in `mnesia.erl`.
- `mnesia_gvar` is a *public named* ets table: it is visible/`ets`-reachable by any process, and lookups on absent vars raise `badarg` (hence `?catch_val`).
- The `tm_*` functions in `mnesia_checkpoint` are called by `mnesia_tm`, not the user; the user-facing checkpoint API is the second export block.
- `mnesia_registry` is intentionally empty (deprecated shell).

## Notes for Agents
- `-Ddebug` (i.e. `make TYPE=debug`) enables the `?eval_debug_fun` macro (`mnesia.hrl`); the calls are compiled as `ok` otherwise. Under debug they call `mnesia_lib:eval_debug_fun/4`, which lets test suites (`mnesia_debug` ets table, `activate_debug_fun`) bind funs to named hook points (commit, dump, fallback, load, …) to make tests deterministic. Many hooks exist across `mnesia_tm`, `mnesia_dumper`, `mnesia_bup`, `mnesia_loader`, `mnesia_log`, `mnesia_controller`.
- `mnesia.app.src` / `mnesia.appup.src` are templates; the `%VSN%` placeholder is `sed`-substituted by the Makefile (`MNESIA_VSN`). `mnesia.appup.src` currently records only `restart_application` upgrades/downgrades from 4.1x/4.2x.
- The module-name to public-role mapping above is the fastest way in; start at `mnesia.erl` for the user surface and follow into `mnesia_tm` → `mnesia_locker`/`mnesia_checkpoint` for transaction behaviour.
- `mnesia.hrl` is also `-include`d by the test suites in `../test/`.

## See Also
- `../CONTEXT.md` — the mnesia application node (layout, `vsn.mk`, tests, docs).
- `../test/` — Common Test suites (`mnesia_*_SUITE.erl`) that rely on the debug hooks and `-include("mnesia.hrl")`.
- `../examples/`, `../doc/` — sample record definitions (`company.hrl`) and reference-manual sources.
- `../include/` — placeholder only (empty `INCLUDE_FILES`); `mnesia.hrl` here is **internal**, not a user-exported header.
