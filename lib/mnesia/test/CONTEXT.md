# mnesia — Common Test suites

## Intent
Common Test directory of the `mnesia` OTP application (`MNESIA_VSN` 4.27).
It is not a flat set of CT suites: `mnesia_SUITE` is the root of a bespoke hierarchical harness in which every other `*_test`/`*_backup` module is a *sub-suite* reached through group entries `{Mod, all}`.
The harness predates Common Test (author Hakan Mattsson) and is still driven through CT; `README` is its authoritative manual.
Modules under test live in `../src/`; `../examples/` and `../doc/src/` are compiled into this directory by the Makefile and used by the tests.

## API Surface (entry points)
- `mnesia_SUITE.erl` — the only full CT suite for the DBMS. `all/0` = `[app, appup, doctests, {group,light}, {group,medium}, {group,heavy}, {group,external}, clean_up_suite]`; `suite/0` installs `ts_install_cth` with `nodenames=2`. `mnesia_SUITE:silly/0` runs the install test directly, bypassing the harness.
- `mnesia_SUITE:groups/0` — `light` = install/nice/evil/frag/qlc/index_plugins/config/examples; `medium` = install/atomicity/isolation/durability/recovery/consistency/majority; `heavy` = measure; plus `benchmarks`, `otp_r4b`, `otp_2363`, `external`.
- `mnesia_bench_SUITE.erl` — second real CT suite: TPC-B conflict benchmark (`tpcb` group, `{repeat,2}`), described by `mnesia_bench.spec`.
- Sub-suite modules (`mnesia_install_test`, `mnesia_config_test`, `mnesia_evil_coverage_test`, `mnesia_trans_access_test`, `mnesia_dirty_access_test`, `mnesia_atomicity_test`, `mnesia_consistency_test`, `mnesia_isolation_test`, `mnesia_durability_test`, `mnesia_recovery_test`, `mnesia_schema_recovery_test`, `mnesia_majority_test`, `mnesia_frag_test`, `mnesia_qlc_test`, `mnesia_nice_coverage_test`, `mnesia_evil_backup`, `mnesia_examples_test`, `mnesia_measure_test`, `mnesia_index_plugin_test`, `mnesia_external_backend_test`) — each exports `all/0`, `groups/0`, `init_per_testcase/2`, `end_per_testcase/2`; they have no `all/1` and cannot be run as top-level CT suites.
  They are pulled in either from another sub-suite's `all/0` (`mnesia_evil_coverage_test` includes `{mnesia_dirty_access_test, all}`, `{mnesia_trans_access_test, all}`, `{mnesia_evil_backup, all}`; `mnesia_recovery_test` includes `{mnesia_schema_recovery_test, all}`) or from `mnesia_SUITE:groups/0`.
- `mt.erl` — interactive driver: `mt:t/0,1,2,3`, `mt:loop/1,2,3`, `mt:struct/0,1`, `mt:doc/0,1`, `mt:read_config/0`, `mt:write_config/1`, `mt:ping/0`, `mt:start_nodes/0`, `mt:shutdown/0`, `mt:start_peers/1`. Aliases: `all|suite`→`mnesia_SUITE`, `light|medium|heavy`→groups, `atomicity`, `backup`→`mnesia_evil_backup`, `config`, `consistency`, `dirty`, `durability`, `evil`→`mnesia_evil_coverage_test`, `examples`, `frag`, `install`, `isolation`, `majority`, `measure`, `nice`, `qlc`, `trans`, `ixp`→`mnesia_index_plugin_test`, `external`, `recovery`; plain atoms are treated as `{Mod, all}`.
- `mnesia_test_lib.erl` + `mnesia_test_lib.hrl` — the harness engine (test driver, node preparation, result protocol) and its macros (`?match`, `?ignore`, `?error`, `?skip`, `?acquire_nodes`, `?is_debug_compiled`, `?needs_disc`, `?verify_mnesia`, `?BACKEND`).
- Support/plugin modules: `ext_test` + `ext_test_server` (external storage backend, `mnesia_backend_type` behaviour), `gen_tcp_blocking_dist` (custom `-proto_dist` used to simulate blocked nodes), `mnesia_config_backup` (impostor for the `mnesia_backup` callbacks), `mnesia_config_event` and `mnesia_inconsistent_database_test` (gen_event modules installed as `event_module` / for `inconsistent_database`), `mnesia_cost` and `mnesia_dbn_meters` (benchmark drivers invoked from `mnesia_measure_test`), `mnesia_frag_hash_test` (`test/0` only — manual comparison of old/new fragment-hash functions, not part of any suite).
- Run configuration: `mnesia.spec` (releasetest spec for `mnesia_SUITE` plus a long `skip_cases` list with reasons such as "Not yet implemented" / "Takes to long time"), `mnesia_bench.spec` (category `bench`), `mnesia.cover` (`{incl_app,mnesia,details}`), and `mt` (UNIX script that starts the main node plus two `peer` nodes in terminals and sets `-mnesia_test_verbose true`, optionally with `-proto_dist gen_tcp_blocking`).

## Test Strategy
Two ways to run:
1. Common Test (this is what `make test`/CI does): in this directory run `make test`; `../../make/test_target_script.sh` releases OTP if a test release is needed, then runs `ct_run -spec $MAKE_TEST_REL_DIR/mnesia[_<postfix>].spec` (the `_gh` variant is preferred when the file exists; CI sets `SPEC_POSTFIX=gh`). The spec runs `{suites,"../mnesia_test",[mnesia_SUITE]}` and applies its skip list. CI invokes `make mnesia_test` with `TEST_NEEDS_RELEASE=true`.
   `{Mod, all}` group entries are expanded by CT as sub-suites (`ct_framework:get_suite/2` → `Mod:all/0` + `Mod:groups/0`), so the sub-suites' own `init_per_testcase/2` (forwarding to `mnesia_test_lib`) run for their cases.
2. Interactive harness (`README`): start one distributed node with the code path covering `mnesia/examples`, `mnesia/src`, `mnesia/ebin` and this directory, then drive it with `mt:t(silly)` (a good starter) up to `mt:t(all)`. Extra nodes are started with `peer` on demand (`mt:start_nodes()`, `mt:ping()`), and `mt:t()` re-runs the case recorded in `mnesia_test_case_info` (configuration is persisted in `mnesia_test_case_config`).
Hierarchy semantics: a test case is `{Mod, Fun}`; the driver calls `Mod:Fun(suite)` to get a list of sub-cases (recursion) or `[]` for a leaf, and leaf cases are then run as `Mod:Fun(Config)`. `Fun(doc) -> [String]` clauses are the case documentation used by `mt:doc/0,1` to generate `index.html`.
Node requirements: the Erlang node must have distribution enabled (short names are fine). Cases call `?acquire_nodes(N, Config)` for N = 1..3 nodes; node names come from the `nodes` config (`default_config/0` derives up to three names from the local node, e.g. `a1`, `a2`, `a3`) or from the CT-provided `nodenames` config; down nodes are started via `peer` with `-mnesia debug <mnesia:system_info(debug)>` so instrumentation matches the local node.
`?acquire_nodes/2` also sets `schema_location=opt_disc` (unless `diskless`), kills mnesia on the selected nodes, deletes and recreates the schema, starts `ext_test_server` on every node, starts `[mnesia]` and syncs tables — test cases may assume a running disc-resident Mnesia cluster.
Result protocol: `?error` reports to the registered `mnesia_global_logger` / `mnesia_test_case_sup`; the case process exits `{test_case_ok, Res}`, `{skipped, Reason}` (from `?skip`) or crashes — any logged `?error` turns the case into `failed`.
Timeout: a watchdog kills a case after 5 minutes by default (`tc_timeout` in the config overrides it, scaled by the test framework timetrap factor); `erl -mnesia_test_timeout` disables the mechanism (useful when debugging). Killed cases log "Watchdog in test case timed out" and dump cores.
Debug instrumentation: cases guarded by `?is_debug_compiled` skip themselves unless mnesia was compiled with `-Ddebug` (`mnesia_lib:is_debug_compiled/0`); rebuild with `make clean && make ERLC=$(pwd)/../../bin/erlc ERL_COMPILE_FLAGS="-Ddebug"`.
Network blocking: when the node was started with `-proto_dist gen_tcp_blocking` (`?NETWORK_BLOCKER_DIST_OPTS`), `mnesia_test_lib` can block/unblock node pairs (`block_peer/2`, `block_pair/2`, `unblock_*`); tests needing it call `skip_if_no_network_blocker/1`. Pairs involving the local node are unblocked first during preparation.
Slow / long-running: `heavy` → `mnesia_measure_test` (meter, cost, dbn_meters, tpcb variants), `mnesia_bench_SUITE` (TPC-B, repeated twice), and everything in `mnesia.spec`'s skip list.

## Constraints
- Compilation: `erl -make` (per `README`) or the app's `make`; flags are `+nowarn_missing_spec_documented -Werror +nowarn_deprecated_catch` with `+deterministic` filtered out — a bad spec or doc comment fails the build.
- `EBIN = .`: the test modules are compiled into this directory, and the doc examples (`../doc/src/{company,company_o,bup}.erl`) plus `../examples/{mnesia_meter,mnesia_tpcb}.erl` are compiled here too. `release_tests_spec` installs the sorted `.erl`/`.hrl`/`.spec`/`.cover`/`mt` files to `$(RELEASE_PATH)/mnesia_test`.
- A new test module must be added to `MODULES` in `Makefile`; a new sub-suite must export `all/0`, `groups/0`, `init_per_testcase/2`, `end_per_testcase/2` and be referenced from a `groups/0` entry (`{Mod, all}`) or an `mt` alias, otherwise it is unreachable.
- Cases take a `Config` list and should keep the `(suite)`/`(doc)` clauses; they must use `?acquire_nodes/2` (not raw node handling) to get nodes.
- Tests must not assume any particular working directory contents: they create `Mnesia*` directories, `MnesiaCore*`, `tempfile*`, `bup*.BUP` in the current directory.

## Known Issues
- `mt:alias(recover)` maps to `mnesia_recover_test`, a module that does not exist in this directory; the working alias is `recovery`.
- `mnesia_trans_access_test`, `mnesia_dirty_access_test` and `mnesia_evil_backup` are only reached via `mnesia_evil_coverage_test:all/0` (so via `light`) or via the `trans`/`dirty`/`backup` aliases; `mnesia_schema_recovery_test` only via `mnesia_recovery_test`. `mnesia_frag_hash_test` (exports `test/0`) is manual-only and never invoked by the suite.
- `mnesia_external_backend_test` carries its own `suite/0` (`ts_install_cth` with `nodenames=1`) which is ignored when the module runs as a sub-suite of `mnesia_SUITE` (which uses 2 node names).
- Several cases are skipped by design: `mnesia.spec` skips most of `mnesia_measure_test` (too slow) and various not-yet-implemented cases, and `mnesia_external_backend_test:init_per_testcase/2` skips `backup_and_restore_should_work_with_external_backend`.
- The watchdog may kill long benchmarks; a "Watchdog in test case timed out" log entry is a timeout, not necessarily a Mnesia defect.

## Notes for Agents
- Read `README` before touching this directory; it documents the harness, node setup, timeouts and the `-Ddebug` rebuild recipe.
- For CI-equivalent runs prefer the spec route (`make test`), not `mt`, and consult `mnesia.spec` when a case seems to be missing from a run.
- The debug instrumentation entry points live in `../src/mnesia_lib.erl` (`activate_debug_fun/5`, `eval_debug_fun/4`), and `mnesia_frag_hash_test` relies on `mnesia_frag_hash:init_state/2`, `add_frag/1`, `key_to_frag_number/2`.
- `?init(N, Config)` macros (local to `mnesia_config_test`, `mnesia_examples_test`, `mnesia_measure_test`) are thin wrappers over `mnesia_test_lib:prepare_test_case/5` that skip starting Mnesia.

## See Also
- `../src/` — Mnesia implementation (instrumented for the debug tests); `../include/` — headers.
- `../examples/` — `mnesia_meter`, `mnesia_tpcb`, `company*`, `bup`, `bench/` (used by `mnesia_examples_test`, `mnesia_measure_test`, `mnesia_bench_SUITE`).
- `../doc/src/` — documentation example modules (`company`, `company_o`, `bup`) compiled into this directory and exercised by `mnesia_examples_test`.
- `../../common_test/` — Common Test and the legacy `test_server`/`ts` framework (`ts_install_cth`, spec/cover handling); `../../HOWTO/TESTING.md` and `../../make/test_target_script.sh` describe how `make test` drives these suites.
