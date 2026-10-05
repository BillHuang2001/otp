# mnesia — Distributed Transactional DBMS

## Intent
The `mnesia` OTP application: a distributed, replicated, transactional, soft-real-time DBMS layered on `ets`/`dets`/`disk_log` plus pluggable external storage backends.
Application description is "MNESIA  CXC 138 12"; version is `MNESIA_VSN = 4.27` (`vsn.mk`).
Only `kernel` and `stdlib` are required applications — no external library, but multi-node tables need a running distribution.
Everything (Erlang sources, examples, docs) is built from this directory; `Makefile` sets `SUB_DIRECTORIES = src include examples doc`.

## API Surface
The public API is the single module `mnesia` (see `src/CONTEXT.md` for the module inventory); it is also the default `access_module` (the activity/`mnesia_access` implementation), so there is **no separate `mnesia_activity` module**.
- Start/stop & config: `start/0,1`, `stop/0`, `change_config/2`, `set_debug_level/1`, `info/0`, `system_info/0,1`, `error_description/1`.
- Activities: `transaction/1..3`, `sync_transaction/1..3`, `async_dirty/1,2`, `sync_dirty/1,2`, `ets/1,2`, `activity/2..4`, `abort/1`, `is_transaction/0`.
- Locks: `lock/2,4`, `lock_table/2`, `read_lock_table/1`, `write_lock_table/1`.
- In-activity record access: `read`, `wread`, `write`, `s_write`, `delete`, `delete_object`, `match_object`, `select`, `select_reverse`, `index_read`, `index_match_object`, `all_keys`, `first`/`next`/`last`/`prev`, `foldl`/`foldr`.
- Dirty (no-activity) access: `dirty_read`, `dirty_write`, `dirty_delete`, `dirty_update_counter`, `dirty_select`, `dirty_match_object`, `dirty_index_*`, `dirty_first`/`next`/`last`/`prev`, `dirty_slot`.
- Schema/table management: `create_schema/1,2`, `delete_schema/1`, `create_table/1,2`, `delete_table/1`, `add_table_copy/3`, `del_table_copy/2`, `move_table_copy/3`, `change_table_copy_type/3`, `add_table_index/2`, `del_table_index/2`, `transform_table/3,4`, `change_table_access_mode/2`, `change_table_load_order/2`, `change_table_majority/2`, `change_table_frag/2`, `clear_table/1`, `read/write/delete_table_property/2,3`.
- Load/repair: `wait_for_tables/2`, `force_load_table/1`, `dump_tables/1`, `dump_log/0`, `sync_log/0`, `set_master_nodes/1,2`, `table_info/2,4`, `schema/0,1`.
- Backup/restore/fallback: `backup/1,2`, `traverse_backup/4,6`, `activate_checkpoint/1`, `deactivate_checkpoint/1`, `backup_checkpoint/2,3`, `restore/2`, `install_fallback/1,2`, `uninstall_fallback/0,1`.
- Events/subscription: `subscribe/1`, `unsubscribe/1`, `report_event/1` (delivered by `mnesia_event`).
- Extra interfaces: `qlc` table generators (`table/1,2`), text-file interchange (`load_textfile/1`, `dump_to_textfile/1`), SNMP tables (`snmp_open_table/2`, `snmp_get_row/2`, `snmp_get_next_index/2`, `snmp_get_mnesia_key/2`), external backends (`add_backend_type/2`).
- Behaviours users can implement: `mnesia_access` (activity module, selected by the `access_module` env var; implemented by `mnesia` and `mnesia_frag`), `mnesia_backup` (backup media, `backup_module` env var; default `mnesia_backup`), `mnesia_event` (event handler, `event_module` env var; default `mnesia_event`), `mnesia_frag_hash` (hash function for fragmented tables), and `mnesia_backend_type` (external storage backend, declared via `behaviour_info/1`).
- Application callback: `mnesia_app:start/2` → `mnesia_sup:start_link/1`; top supervisor is `one_for_all`.

## Constraints
- One Mnesia instance per node; the process names in `registered` (`mnesia_tm`, `mnesia_controller`, `mnesia_locker`, `mnesia_monitor`, `mnesia_recover`, `mnesia_rpc`, `mnesia_event`, `mnesia_sup`, `mnesia_kernel_sup`, `mnesia_late_loader`, `mnesia_dumper_load_regulator`, `mnesia_fallback`) are fixed and global.
- Disc-resident tables require a schema: `mnesia:create_schema/1` before starting, plus the `dir` / `schema_location` env vars; a node with `schema_location=disc` must have a schema or Mnesia will not start.
- Distribution must be enabled for tables with copies on other nodes; node names come from table definitions, so unreachable nodes cause waiting/blocking rather than errors.
- Soft-real-time means `disc_copies` durability is asynchronous — use `sync_transaction`/`sync_dirty` or `dump_log`/`sync_log` when durability per transaction is required.
- Do not edit generated `ebin/*.app`/`*.appup`; edit `src/mnesia.app.src` / `src/mnesia.appup.src` (the `%VSN%` placeholder is `sed`-substituted by the Makefile).
- A new module must be added to both `src/Makefile` `MODULES` and the `modules` list in `src/mnesia.app.src`.

## Dependencies
- `src/mnesia.app.src`: `{applications, [kernel, stdlib]}`, `{runtime_dependencies, ["stdlib-5.0","kernel-5.3","erts-9.0"]}`.
- No C sources / NIFs, no OS-specific configure step — pure Erlang, so `lib/mnesia` has no `configure`/`c_src`.
- Optional runtime deps pulled in only if the corresponding feature is used: `sasl` (error logging), `snmp` (via the `mnesia_snmp_hook` MIB integration), `common_test`/`tools` (tests, `cover`), `qlc` (from stdlib) for the query interface.

## Design Decisions
- All shared state lives in the public named ets table `mnesia_gvar` (`mnesia_lib:set/2`, `val/1`, `?catch_val`), created by `mnesia_monitor`, instead of being threaded through processes — config, per-table whereabouts and `cstruct` fields are read from there everywhere.
- Layering: public API/activity layer (`mnesia`, `mnesia_frag`) → transaction & concurrency layer (`mnesia_tm` single TM process with 2PC, `mnesia_locker` lock manager, `mnesia_checkpoint`) → data access (`mnesia_lib`, `mnesia_index`, backend types) → persistence (`mnesia_log`, `mnesia_dumper`, `mnesia_loader`, `mnesia_late_loader`) → schema/config (`mnesia_schema`, `mnesia_monitor`, `mnesia_controller`, `mnesia_recover`).
- The schema is itself a Mnesia table (`schema`), so schema changes are ordinary transactions ordered by `mnesia_schema`.
- Three built-in storage types are implemented inline as `mnesia_lib:db_*` clauses: `ram_copies` (ets), `disc_copies` (ets + transaction log, dumped by `mnesia_dumper`) and `disc_only_copies` (dets).
- External storage backends are pluggable per table via the `mnesia_backend_type` behaviour (`storage_properties` table option / `add_backend_type/2`); `mnesia_lib:db_*` dispatches on the storage type, so a backend is reached through the same access path.
- Fragmentation (`mnesia_frag` + `mnesia_frag_hash`) horizontally partitions a table into `frag`-suffixed tables; `mnesia_frag` is an alternative `access_module` (`-mnesia access_module mnesia_frag`) whose callbacks resolve the wrapper table name to a fragment and then re-enter `mnesia` with the fragment's name. The hash can be swapped via `change_table_frag/2`.
- Backups use a versioned, self-describing format (`#log_header{}` in `src/mnesia.hrl` may never change) so they are usable as an interchange format across releases and by external tools; `mnesia_text` adds a human-readable variant.
- `mnesia_rpc` is a dedicated RPC server so floods of dirty operations cannot overload the standard `rpc` server.
- Fallback files (`install_fallback`) are an offline repair mechanism, handled by `mnesia_bup` with the `mnesia_fallback` process.

## Known Issues
- `mnesia_registry` is an intentional deprecated stub (only a `-removed` attribute, no functions).
- `sync_transaction`/`sync_dirty` are significantly slower — the sync is per write, not batched; heavy mixes of dirty and transactional writes on the same table also serialize on the single TM.
- `ordered_set` is not supported for `disc_only_copies`.
- Iterating (`foldl`/`foldr`/`next`/`prev`/`snmp_get_next_index`) over a table in a transaction that has written to it is compensated for internally and is slow — avoid writes before iteration in the same transaction (documented in `src/mnesia.erl`'s moduledoc).
- `mnesia:activity/2,3,4`, `start/0,1`, `stop/0`, `set_debug_level/1`, `kill/0`, `lkill/0`, `dirty_rpc/4`, `raw_table_info/2` and the `remote_dirty_*` functions are documented as "Not for public use".

## Notes for Agents
- Start here for a user-facing change: `src/mnesia.erl` (API + moduledoc + env-var documentation), then follow into `src/mnesia_tm.erl` (transactions/2PC), `src/mnesia_locker.erl` (locks), `src/mnesia_schema.erl` (DDL), `src/mnesia_bup.erl` (backup).
- The Mnesia Reference Manual is generated from the `-moduledoc`/`-doc` attributes in `src/mnesia.erl` (plus man3 pages from every `src/*.erl` without `moduledoc false.`) — API doc changes belong in the source, not in `doc/`.
- `src/mnesia.hrl` is an internal header (included by the sources and by the test suites), not a user-exported header; `include/` is an empty placeholder.
- Many test cases only run when mnesia is compiled with `-Ddebug` (`make TYPE=debug` in `src/`); the `?eval_debug_fun` hooks let suites make the commit/dump/load protocols deterministic.
- `examples/Makefile` compiles nothing (`TARGET_FILES` is empty); the real compilation of example modules happens in `test/Makefile`.
- `examples/{company,company_o,bup}.*`, `examples/DATA` are git symlinks into `doc/src/`; `examples/{mnesia_meter,mnesia_tpcb}.erl` are byte-identical duplicates of the copies in `test/`.

## Test Strategy
- `make test` in this directory (or `make mnesia_test` at the repo root, with `TEST_NEEDS_RELEASE=true`) runs Common Test via `make/test_target_script.sh` → `ct_run -spec .../mnesia[_<postfix>].spec` from `test/mnesia.spec`.
- The suite is a bespoke hierarchy rooted at `{mnesia_SUITE, all}`; sub-suites are reached as `{Mod, all}` group entries, and `test/mt.erl` provides an interactive driver (`mt:t(all)`, aliases, `mt:struct/1`, `mt:doc/1`).
- Most cases need 2–3 distributed nodes; the harness acquires them with `?acquire_nodes/2` and starts missing ones with `peer`. Run interactively with `erl -sname a -pa lib/mnesia/examples -pa lib/mnesia/src -pa lib/mnesia/ebin` from `test/`.
- Cases needing the debug instrumentation skip themselves unless mnesia was built with `-Ddebug`; a 5-minute per-case watchdog applies (disabled with `erl -mnesia_test_timeout`).
- `heavy`/benchmark work is `mnesia_measure_test` and `mnesia_bench_SUITE` (TPC-B, mostly skipped in `mnesia.spec`); `examples/bench/` is manual-only with no CT coverage.

## Routing Table
| Area | Description |
| --- | --- |
| `src/` | All 30 Erlang modules (public API `mnesia`, TM/locker, persistence, schema, backends), `mnesia.hrl`, `mnesia.app.src`/`mnesia.appup.src` templates — see `src/CONTEXT.md` for the module inventory and cross-module contracts |
| `test/` | Common Test harness: `mnesia_SUITE` root + `mnesia_*_test` sub-suites, `mt` driver, `mnesia_test_lib`, spec/cover files — see `test/CONTEXT.md` |
| `doc/` | ExDoc guides (`guides/`), `notes.md`, guide/test example sources (`src/`), internal design notes (`misc/`), `docs.exs` — see `doc/CONTEXT.md` |
| `examples/` | Tutorial/benchmark programs (`company*`, `bup`, `mnesia_meter`, `mnesia_tpcb`, `DATA`) and the Canadian `bench/` harness — see `examples/CONTEXT.md` |
| `include/` | Placeholder only: `Makefile` with an empty `INCLUDE_FILES`, no exported headers (headers live in `src/`) |
| `priv/` | Empty — only `.gitignore`; no tracked runtime data files |
| `ebin/` | Build output (only `.gitignore` tracked) — never edit |
| `vsn.mk` | Defines `MNESIA_VSN`, consumed by every sub-Makefile as `VSN` |
| `Makefile` | Sets `SUB_DIRECTORIES = src include examples doc`, includes `make/app_targets.mk` |

## See Also
- `../CONTEXT.md` — OTP application conventions (app layout, `vsn.mk`, release targets).
- `../../make/app_targets.mk`, `../../make/doc.mk`, `../../make/test_target_script.sh` — shared build/test rules used here.
- `../stdlib/` (`qlc` is the Mnesia query language, `dets` is the `disc_only_copies` store), `../kernel/` (distribution, `disk_log` for the transaction log, `file`), `../snmp/` (SNMP table integration), `../sasl/` (release handling of the app).
