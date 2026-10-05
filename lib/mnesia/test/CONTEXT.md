# mnesia — Common Test suites

## Intent
Common Test directory of the `mnesia` OTP application (`MNESIA_VSN` 4.27).
Not a flat set of CT suites: `mnesia_SUITE` is the root of a bespoke hierarchical harness in which every other `*_test`/`*_backup` module is a *sub-suite* reached through group entries `{Mod, all}`.
The harness predates Common Test (author Hakan Mattsson) and is still driven through CT; `README` is its authoritative manual.
Code under test is in `../src/`; `../examples/` and `../doc/src/` are compiled into this directory by the Makefile and used by the tests.

## API Surface (entry points)
- `mnesia_SUITE.erl` — the CT suite for the DBMS. `all/0` = `[app, appup, doctests, {group,light}, {group,medium}, {group,heavy}, {group,external}, clean_up_suite]`; `suite/0` installs `ts_install_cth` with `nodenames=2`. `mnesia_SUITE:silly/0` runs the install test directly, bypassing the harness.
- `mnesia_SUITE:groups/0` — `light` = install/nice/evil/frag/qlc/index_plugins/config/examples; `medium` = install/atomicity/isolation/durability/recovery/consistency/majority; `heavy` = measure; plus `benchmarks`, `otp_r4b`, `otp_2363`, `external`.
- `mnesia_bench_SUITE.erl` — second real CT suite: TPC-B conflict benchmark (`tpcb` group, `{repeat,2}`), described by `mnesia_bench.spec`.
- Sub-suite modules — the `mnesia_*_test` modules (`install`, `nice_coverage`, `evil_coverage`, `trans_access`, `dirty_access`, `atomicity`, `consistency`, `isolation`, `durability`, `recovery`, `schema_recovery`, `majority`, `frag`, `qlc`, `examples`, `measure`, `index_plugin`, `external_backend`), plus `mnesia_config_test` and `mnesia_evil_backup`.
  Each exports `all/0`, `groups/0`, `init_per_testcase/2`, `end_per_testcase/2` and has **no** `all/1`, so none can be run as a top-level CT suite.
  They are reached from another sub-suite's `all/0` (`mnesia_evil_coverage_test` includes `{mnesia_dirty_access_test, all}`, `{mnesia_trans_access_test, all}`, `{mnesia_evil_backup, all}`; `mnesia_recovery_test` includes `{mnesia_schema_recovery_test, all}`) or from `mnesia_SUITE:groups/0`.
- `mt.erl` — interactive driver: `mt:t/0,1,2,3`, `mt:loop/1,2,3`, `mt:struct/0,1`, `mt:doc/0,1`, `mt:read_config/0`, `mt:write_config/1`, `mt:ping/0`, `mt:start_nodes/0`, `mt:shutdown/0`, `mt:start_peers/1`.
  Aliases: `all|suite`→`mnesia_SUITE`, `light|medium|heavy`→groups, `backup`→`mnesia_evil_backup`, `evil`→`mnesia_evil_coverage_test`, `ixp`→`mnesia_index_plugin_test`, plus one per other sub-suite (`trans`, `dirty`, `config`, `recovery`, `frag`, `qlc`, `measure`, `install`, `nice`, `examples`, `majority`, `external`, …); a plain atom means `{Mod, all}`.
- `mnesia_test_lib.erl` + `mnesia_test_lib.hrl` — harness engine (driver, node preparation, result protocol) and its macros (`?match`, `?error`, `?skip`, `?acquire_nodes`, `?is_debug_compiled`, `?needs_disc`, `?verify_mnesia`, `?BACKEND`).
- Support/plugin modules: `ext_test`/`ext_test_server` (external storage backend, `mnesia_backend_type` behaviour), `gen_tcp_blocking_dist` (custom `-proto_dist` simulating blocked nodes), `mnesia_config_backup` (impostor for the `mnesia_backup` callbacks), `mnesia_config_event`/`mnesia_inconsistent_database_test` (gen_event modules used as `event_module`), `mnesia_cost`/`mnesia_dbn_meters` (bench drivers), `mnesia_frag_hash_test` (`test/0` only — manual old/new fragment-hash comparison, part of no suite).
- Run configuration: `mnesia.spec` (releasetest spec for `mnesia_SUITE` plus a long `skip_cases` list), `mnesia_bench.spec` (category `bench`), `mnesia.cover` (`{incl_app,mnesia,details}`), and `mt` (UNIX script starting the main node plus two `peer` nodes in terminals, `-mnesia_test_verbose true`, optionally `-proto_dist gen_tcp_blocking`).

## Test Strategy
Two ways to run:
1. Common Test — what `make test`/CI does: in this directory `make test` runs `../../make/test_target_script.sh`, which releases OTP when needed and calls `ct_run -spec $MAKE_TEST_REL_DIR/mnesia[_<postfix>].spec` (the `_gh` variant wins if present; CI sets `SPEC_POSTFIX=gh` and runs `make mnesia_test` with `TEST_NEEDS_RELEASE=true`). The spec runs `{suites,"../mnesia_test",[mnesia_SUITE]}` and applies its skip list.
   `{Mod, all}` group entries are expanded by CT as sub-suites (`ct_framework:get_suite/2` → `Mod:all/0` + `Mod:groups/0`), so each sub-suite's own `init_per_testcase/2` (forwarding to `mnesia_test_lib`) runs for its cases.
2. Interactive harness (`README`) — start one distributed node with the code path covering `mnesia/examples`, `mnesia/src`, `mnesia/ebin` and this directory, then drive it with `mt:t(silly)` up to `mt:t(all)`. Extra nodes start with `peer` on demand (`mt:start_nodes()`, `mt:ping()`); `mt:t()` re-runs the case recorded in `mnesia_test_case_info` (config persists in `mnesia_test_case_config`).
Hierarchy semantics: a case is `{Mod, Fun}`; the driver calls `Mod:Fun(suite)` to obtain sub-cases (recursion) or `[]` for a leaf, and leaf cases run as `Mod:Fun(Config)`. `Fun(doc) -> [String]` clauses document a case and feed `mt:doc/0,1` (generates `index.html`).
Nodes: distribution must be enabled (short names fine). Cases call `?acquire_nodes(N, Config)` for N = 1..3; names come from the `nodes` config (`default_config/0` derives up to three names from the local node, e.g. `a1`, `a2`, `a3`) or from the CT-provided `nodenames` config, and down nodes start via `peer` with `-mnesia debug <mnesia:system_info(debug)>` so instrumentation matches the local node.
`?acquire_nodes/2` also sets `schema_location=opt_disc` (unless `diskless`), kills mnesia on the selected nodes, deletes/recreates the schema, starts `ext_test_server` on every node, starts `[mnesia]` and syncs tables — cases may assume a running disc-resident cluster.
Result protocol: `?error` reports to the registered `mnesia_global_logger`/`mnesia_test_case_sup`, so any logged `?error` makes a case `failed`; the case process otherwise exits `{test_case_ok, Res}`, `{skipped, Reason}` (from `?skip`) or crashes.
Timeout and debug prerequisites: a watchdog kills a case after 5 minutes by default (`tc_timeout` config overrides, scaled by the timetrap factor) and logs "Watchdog in test case timed out"; `erl -mnesia_test_timeout` disables it. Cases guarded by `?is_debug_compiled` skip themselves unless mnesia was built with `-Ddebug` (`mnesia_lib:is_debug_compiled/0`; rebuild via `make clean && make ERLC=$(pwd)/../../bin/erlc ERL_COMPILE_FLAGS="-Ddebug"`).
Network blocking: with `-proto_dist gen_tcp_blocking` (`?NETWORK_BLOCKER_DIST_OPTS`) the harness can block/unblock node pairs (`block_peer/2`, `block_pair/2`, `unblock_*`); tests needing it call `skip_if_no_network_blocker/1`.

## Constraints
- Compile with `erl -make` (per `README`) or the app's `make`; flags are `+nowarn_missing_spec_documented -Werror +nowarn_deprecated_catch` with `+deterministic` filtered out — a bad spec or doc comment fails the build.
- `EBIN = .`: test modules compile into this directory, together with `../doc/src/{company,company_o,bup}.erl` and `../examples/{mnesia_meter,mnesia_tpcb}.erl`. `release_tests_spec` installs the `.erl`/`.hrl`/`.spec`/`.cover` files and `mt` to `$(RELEASE_PATH)/mnesia_test`.
- A new test module must be added to `MODULES` in `Makefile`; a new sub-suite must export `all/0`, `groups/0`, `init_per_testcase/2`, `end_per_testcase/2` and be referenced from a `groups/0` entry (`{Mod, all}`) or an `mt` alias, else it is unreachable.
- Cases take a `Config` list, should keep their `(suite)`/`(doc)` clauses, and must obtain nodes via `?acquire_nodes/2`.
- Tests write `Mnesia*` directories, `MnesiaCore*`, `tempfile*` and `bup*.BUP` into the current directory.

## Known Issues
- `mt:alias(recover)` points at `mnesia_recover_test`, which does not exist here; the working alias is `recovery`.
- `mnesia_trans_access_test`/`mnesia_dirty_access_test`/`mnesia_evil_backup` are only reached via `mnesia_evil_coverage_test:all/0` (hence via `light`) or the `trans`/`dirty`/`backup` aliases, and `mnesia_schema_recovery_test` only via `mnesia_recovery_test`; `mnesia_frag_hash_test:test/0` is never invoked by any suite.
- `mnesia_external_backend_test` carries its own `suite/0` (`ts_install_cth`, `nodenames=1`) that is ignored while it runs as a sub-suite of `mnesia_SUITE` (2 node names).
- Cases are skipped by design in several places: `mnesia.spec` skips most of `mnesia_measure_test` ("Takes to long time") and many "Not yet implemented" cases, and `mnesia_external_backend_test:init_per_testcase/2` skips `backup_and_restore_should_work_with_external_backend`. Long-running work is `heavy` → `mnesia_measure_test` and `mnesia_bench_SUITE` (TPC-B ×2).

## Notes for Agents
- Read `README` first; it documents the harness, node setup, timeout flag and the `-Ddebug` rebuild recipe.
- For CI-equivalent runs prefer the spec route (`make test`) and consult `mnesia.spec` when a case seems absent from a run.
- Debug instrumentation entry points live in `../src/mnesia_lib.erl` (`activate_debug_fun/5`, `eval_debug_fun/4`); `mnesia_frag_hash_test` relies on `mnesia_frag_hash:init_state/2`, `add_frag/1`, `key_to_frag_number/2`.
- The `?init(N, Config)` macros local to `mnesia_config_test`, `mnesia_examples_test` and `mnesia_measure_test` wrap `mnesia_test_lib:prepare_test_case/5` without starting Mnesia.

## See Also
- `../src/` (instrumented Mnesia implementation), `../include/` (headers).
- `../examples/` (`mnesia_meter`, `mnesia_tpcb`, `company*`, `bup`, `bench/`) and `../doc/src/` (doc example modules) — both compiled into this directory.
- `../../common_test/` (Common Test and the legacy `test_server`/`ts` framework, `ts_install_cth`, spec/cover handling); `../../HOWTO/TESTING.md` and `../../make/test_target_script.sh`.
