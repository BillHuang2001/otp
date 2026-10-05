# erts/emulator/test/ — BEAM emulator Common Test suites

## Intent
Common Test (`ct_run` / test_server `ts`) suites that validate the BEAM emulator itself (the thing built from `../emulator/`), i.e. the runtime behaviour observable from Erlang: terms and BIFs, bit syntax, BEAM instruction/literal handling, processes/signals/schedulers, GC and allocators, ETS/match specs, tracing, ports/drivers/NIFs, distribution and code loading.
This is the black-box/white-box complement of the C-level tests: the code under test lives in `../emulator/beam`, `../emulator/drivers`, `../emulator/nifs`, `../emulator/sys` and the vendored 3PP dirs, but the tests are ordinary Erlang suites compiled in this directory.
Sibling test levels (different scope, not duplicates): `../../test/` (erts install/upgrade/launcher, application `system`), `../../epmd/test/`, `../../emulator/test` (this dir, application `emulator`).

## API Surface
Content: 104 tracked `*_SUITE.erl` suites, 45 `*_SUITE_data/` support directories, a `property_test/` helper dir, 8 spec files, and ~20 non-suite helper modules.
Total tracked files: 370.

### Running a single suite (fast path)
- From `ERL_TOP`: `make emulator_test ARGS="-suite gc_SUITE"` and `make emulator_test ARGS="-suite binary_SUITE -case deep_bitstr_lists"` (the `emulator_test` root target also builds the emulator first).
- From `erts/emulator`: `make test` (all suites) or `make test ARGS="-suite gc_SUITE -case grow_heap"`.
- `ARGS` is passed verbatim to `ct_run`; when it is empty the spec file is used instead (see below).
- `TYPE=<debug|asan|valgrind|lcnt|gcout...>` is forwarded by `../../make/app_targets.mk` into `-emu_type`, and `FLAVOR=<emu|jit>` into `-emu_flavor`, so run e.g. `make test TYPE=debug` or `make emulator_test FLAVOR=jit ARGS="-suite jit_SUITE"`.
- `TEST_NEEDS_RELEASE` is **not** set at this level (only `erts/Makefile` sets it), so no full OTP release is required; `test_target_script.sh` still prepares `erts/emulator/make_test_dir/emulator_test` + `make_test_dir/ct_logs`.
- Test logs: `erts/emulator/make_test_dir/ct_logs/index.html`. Compile/install logs: `make_test_dir/{compile,install,release}_tests*_log`.
- `*_SUITE_data/Makefile.src` files are configured to `Makefile` and built by `ts:compile_datadirs` (`*_SUITE_data` glob) using the configured `make`; a suite reads its dir via `?config(data_dir, Config)`.

### Spec files (the CI/`ts` entry points, shipped by `release_tests_spec`)
`emulator.spec` is the default (selected when `ARGS` is empty and `SPEC_POSTFIX` is unset): all suites in `../emulator_test`, minus `hash_SUITE` group `phash2_benchmark`, with `{enable_builtin_hooks,false}`.
`emulator_gh.spec` — same as `emulator.spec` plus skip of `nif_SUITE:t_dynamic_resource_call` ("Unstable TC"); this is what GitHub Actions runs (`SPEC_POSTFIX=gh` in `.github/workflows/reusable-test.yaml` / `reusable-system-test.yaml`).
`emulator_smoke.spec` — fast subset: all of `smoke_test_SUITE`, all of `time_SUITE` minus the CET/timezone- and calendar-dependent cases, and single cases `crypto_SUITE:t_md5`, `float_SUITE:{fpe,cmp_integer}`, `erts_debug_SUITE:df`.
`emulator_bench.spec` — benchmark groups only: `estone_SUITE:estone_bench`, `binary_SUITE:iolist_size_benchmarks`, `erts_debug_SUITE:interpreter_size_bench`, `hash_SUITE:phash2_benchmark`, `map_SUITE:benchmarks`.
`emulator_trace.spec` — the 14 tracing-related suites only (`trace_session_SUITE`, `call_trace_SUITE`, `trace_bif_SUITE`, `trace_local_SUITE`, `trace_nif_SUITE`, `trace_SUITE`, `tracer_SUITE`, `port_trace_SUITE`, `trace_call_count_SUITE`, `trace_call_time_SUITE`, `trace_call_memory_SUITE`, `trace_meta_SUITE`, `trace_port_SUITE`, `match_spec_SUITE`).
`emulator_node_container_SUITE.spec` — only `node_container_SUITE`.
`emulator.spec.win` — legacy old-style spec (`{topcase,...}`/`{skip,...}`) shipped in `TEST_SPEC_FILES`; nothing in-tree selects it (`test_target_script.sh` looks for `<app>.spec` or `<app>_<SPEC_POSTFIX>.spec`).
Spec selection rule (`../../make/test_target_script.sh`): `$MAKE_TEST_REL_DIR/emulator_${SPEC_POSTFIX}.spec` if it exists, else `$MAKE_TEST_REL_DIR/emulator.spec`.

### Suites by theme
Terms, data types, BIFs and arithmetic: `a_SUITE` (misc, runs first: leaked processes/timers), `after_SUITE` (receive-after timing), `big_SUITE` (bignums, `.dat` vectors), `small_SUITE` (small-int boundaries/arith, group `p` parallel), `float_SUITE`, `tuple_SUITE`, `list_bif_SUITE`, `num_bif_SUITE`, `bif_SUITE` (largest BIF suite), `guard_SUITE`, `op_SUITE` (operator/instruction semantics), `exception_SUITE`, `binary_SUITE` (binary/bitstring + `term_to_binary` BIFs), `hash_SUITE` (`phash/phash2`, includes benchmark group), `hash_property_test_SUITE` (needs `proper`/`eqc`), `evil_SUITE` (heap frag + hostile external-term decoding), `nested_SUITE`, `receive_SUITE`, `fun_SUITE`, `ref_SUITE`, `unique_SUITE`, `pseudoknot_SUITE` (float-arith sanity + `statistics(runtime)`), `z_SUITE` (misc, runs last).
Bit syntax: `bs_bincomp_SUITE`, `bs_bit_binaries_SUITE`, `bs_construct_SUITE`, `bs_match_bin_SUITE`, `bs_match_int_SUITE`, `bs_match_misc_SUITE`, `bs_match_tail_SUITE`, `bs_utf_SUITE`.
BEAM instruction/literal level and JIT: `beam_SUITE` (opcode/register/apply semantics, hand-written `.S` in `beam_SUITE_data/`), `beam_literals_SUITE` (literal areas/type tests, `.S` generator in `beam_literals_SUITE_data/`), `jit_SUITE` (JIT-only: symbols/annotations/perf directory), `erts_debug_SUITE` (internal debug APIs: `df`, instructions, alloc_blocks_size, interpreter size), `module_info_SUITE`, `erl_debugger_SUITE`, `native_record_SUITE` (native/foreign records via `ext_records.erl`).
Processes, signals, scheduling: `process_SUITE`, `process_max_heap_size_SUITE`, `scheduler_SUITE`, `signal_SUITE`, `monitor_SUITE`, `erl_link_SUITE`, `hibernate_SUITE`, `message_queue_data_SUITE`, `register_SUITE`, `sensitive_SUITE` (`process_flag(sensitive,...)` + trace flags), `save_calls_SUITE`, `statistics_SUITE`, `system_info_SUITE`, `system_profile_SUITE`, `time_SUITE`, `timer_bif_SUITE`, `prim_eval_SUITE`.
GC, allocators, locks: `gc_SUITE` (40–80 min timetraps), `alloc_SUITE` (allocator unit tests + `*_SUITE_data/*.c` drivers), `nofrag_SUITE` (emulator built with no-fragile-load), `mtx_SUITE` (rwmutex/mutex stress), `atomics_SUITE`, `counters_SUITE`, `persistent_term_SUITE`.
Ports, drivers, NIFs and I/O: `port_SUITE`, `port_bif_SUITE`, `busy_port_SUITE`, `async_ports_SUITE`, `port_trace_SUITE`, `driver_SUITE` (32 data-dir C sources), `ddll_SUITE` (dynamic driver linker/loader), `erl_drv_thread_SUITE`, `nif_SUITE`, `dirty_nif_SUITE`, `dirty_bif_SUITE`, `send_term_SUITE`, `iovec_SUITE`, `hello_SUITE` (minimal driver), `decode_packet_SUITE`, `efile_SUITE`.
Tracing: `trace_SUITE`, `trace_bif_SUITE`, `trace_local_SUITE`, `trace_nif_SUITE`, `trace_port_SUITE`, `trace_meta_SUITE`, `trace_call_count_SUITE`, `trace_call_time_SUITE`, `trace_call_memory_SUITE`, `trace_session_SUITE`, `tracer_SUITE`, `call_trace_SUITE`, `match_spec_SUITE`.
Distribution and code loading: `distribution_SUITE` (largest; nodes/IPv6/`net_kernel`/timer/`message_latency` group), `node_container_SUITE` (node-container per-connection state, `pg`-style lockdown counters), `code_SUITE`, `code_parallel_load_SUITE`, `multi_load_SUITE`.
Platform/legacy/special builds: `os_signal_SUITE`, `dump_SUITE` (crash dumps/heart), `lttng_SUITE` (requires LTTng), `lcnt_SUITE` (requires `TYPE=lcnt`), `crypto_SUITE` (legacy `erlang:md5*/crc32/adler32` BIFs vs. reference impls), `estone_SUITE` (EStone benchmark; also the PGO profiling workload, see `../emulator/Makefile.in`), `smoke_test_SUITE` (boot combinations/atomics/jump table).

### Non-suite helper modules (all listed in `Makefile` `MODULES`)
`erts_test_utils.erl` (+ `erts_test_utils.hrl`) — shared helpers: external term constructors (`mk_ext_pid/port/ref`), `ept_check_leaked_nodes/1` (called from ~31 suites' `end_per_testcase`), `check_node_dist`, `assert_error_stack`, `available_internal_state/1`; its header states it is also used by other applications' test code, so changes are cross-application.
`tracer_test.erl`, `trace_sessions.erl` — tracer behaviour + trace-session parametrisation; `trace_SUITE` and `match_spec_SUITE` derive their `all()/groups()` from `trace_sessions`.
`literal_area_collector_test.erl` (used by `code_SUITE`, `dirty_nif_SUITE`, `signal_SUITE`, `z_SUITE`), `long_timers_test.erl` (`a_SUITE` starts it, `z_SUITE` checks the result), `ignore_cores.erl` (`port_SUITE` core-dump isolation), `erts_test_destructor.erl` (`persistent_term_SUITE`), `erts_test_sync_tracer.erl` (`code_SUITE`), `ext_records.erl` (`native_record_SUITE`), `crypto_reference.erl` + `random_iolist.erl` (`crypto_SUITE`), `dgawd_handler.erl` + `dummy.erl` (`driver_SUITE` and others).
`error_info_lib` (from `lib/stdlib/test/`) and `gen_tcp_dist` (from `lib/kernel/examples/gen_tcp_dist/src/`) are compiled in from outside this directory and used by `trace_session_SUITE` / `signal_SUITE` respectively.
`property_test/phash2_properties.erl` — PropEr/QuickCheck properties used by `hash_property_test_SUITE`.

### Generated suite variants (build artifacts, NOT in git)
`Makefile` derives extra suites with `sed` module renaming: `NO_OPT` = `bs_bincomp bs_bit_binaries bs_construct bs_match_bin bs_match_int bs_match_tail bs_match_misc bs_utf guard map` → `*_no_opt_SUITE` (compiled `+no_copt +no_postopt +no_ssa_opt +no_bsm_opt`), and `STRIPPED_TYPES` = the same list minus `bs_bit_binaries`, `guard`, `map` → `*_stripped_types_SUITE` (compiled `+strip_types`).
Because `emulator.spec` (and the `gh` spec) say `{suites,"../emulator_test",all}`, these generated suites are run too — i.e. the bit-syntax/`guard`/`map` suites are effectively exercised in three compilation modes.

## Constraints
- Suites are plain Common Test modules; a data directory must be named `<SUITE>_data` to be found via `?config(data_dir)` and to be configured/compiled by `ts:compile_datadirs`.
- Adding a suite requires adding its module name to `Makefile` `MODULES` (and the `.erl` must exist on disk); data dirs are picked up by the `*_SUITE_data` wildcard in `release_tests_spec`/`ts:compile_datadirs`, so no Makefile change is needed for them.
- Adding a suite to CI cost means adding it to the appropriate spec; specs partition suites by cost/flakiness, and `emulator.spec` (all suites) is the default.
- `Makefile` compiles suites into this directory itself (`EBIN = .`) with `+compressed`, `+nowarn_missing_spec_documented`, and with `+deterministic` filtered out.
- Do not write to sibling nodes: the C sources under `../emulator/beam`, `../emulator/drivers`, `../emulator/nifs`, `../emulator/sys` and the vendored 3PP dirs are owned by other nodes (read-only from here); a failing suite here usually means a bug there.
- Tests must run against the same `TYPE`/`FLAVOR` emulator they were invoked with; a debug/`asan`/`valgrind` binary changes timing and memory behaviour of several suites.

## Design Decisions
- Suites are grouped by *runtime subsystem* (bit syntax, tracing, ports/drivers, distribution, …) rather than by C source file, so a single suite often covers multiple `beam/*.c` files; the theme→suite mapping in the routing table is the practical entry point.
- Pure-Erlang `_data` dirs (`.dat` vectors, `.erl` fixtures, hand-written `.S`) exist so that generated code/large term data is checked in as data instead of being computed at test time — this keeps regressions in *decode/execute* paths visible.
- Several data dirs ship C drivers/NIFs plus `Makefile.src` (built at test time) because drivers/NIFs must be compiled with the same platform toolchain the emulator was built with.
- Specs partition suites by cost (smoke vs. full vs. bench) so CI can pick `SPEC_POSTFIX` without editing the suites; `emulator_gh.spec` exists to hold the CI-specific skip of a known-unstable test case.
- Suites gate on actual capabilities (`erlang:system_info(lock_counting|dynamic_trace|emu_flavor|emu_type|build_type)`, `test_server:memory_checker()`, `os:type()`, availability of `perf`/`taskset`) and return `{skip,...}` instead of failing, so one suite covers all build flavours.

## Known Issues / Gotchas
- `dist_init_unix_SUITE_data/` (hosts/nsswitch/resolv fixtures) has **no** matching `dist_init_unix_SUITE.erl` anywhere in the tree — it is an orphan data dir (last touched in R13B03); do not assume the suite exists.
- `dirty_bif_SUITE_data/` is an empty (untracked) placeholder directory.
- Slow suites: `gc_SUITE` (individual cases set 40–80 min timetraps), `port_SUITE` (30 min case), `distribution_SUITE`, `bif_SUITE`, `binary_SUITE`, `bs_construct_SUITE`, `call_trace_SUITE`, `code_SUITE`, `process_SUITE`, `scheduler_SUITE`, `map_SUITE`, `system_info_SUITE`. Never run the whole dir interactively without `ARGS`.
- `suite_release.exclude` lists `driver_SUITE`, `port_SUITE`, `fun_r7_SUITE` (no longer present) and `node_container_SUITE` plus their data dirs; nothing in-tree reads the file (external release tooling), so it can silently be stale.
- `hash_SUITE:phash2_benchmark` is skipped by both default specs ("Benchmark only"); `nif_SUITE:t_dynamic_resource_call` is skipped only in `emulator_gh.spec` ("Unstable TC").
- Some suites need a specific build to be meaningful, otherwise they skip or test nothing: `lcnt_SUITE` (TYPE=lcnt), `lttng_SUITE` (LTTng-enabled build), `nofrag_SUITE` (no-fragile-load emulator), `jit_SUITE` (JIT flavour, and its `perf` group needs `perf` in `PATH`), `jit_SUITE:perf`/`scheduler_SUITE` cases need `perf`/`taskset`.
- `scheduler_SUITE` and `z_SUITE` require >1 scheduler online; `distribution_SUITE:message_latency` skips when free memory < 2 GB; `dump_SUITE` skips without scheduler dump support; `efile_SUITE` skips the hard-limit case on win32; `alloc_SUITE` skips under valgrind/asan (its own allocators are meaningless there); `port_SUITE`/`port_bif_SUITE`/`node_container_SUITE` have unix-only cases.
- `erts/emulator/test` (`java_test`/`obsolete_SUITE` etc. referenced by `emulator.spec.win`) no longer exists — `emulator.spec.win` is stale.
- ~40 suites use `?CT_PEER()` (peer nodes), so they need local distribution/loopback to work: `distribution_SUITE` (69 uses), `process_SUITE`, `signal_SUITE`, `scheduler_SUITE`, `monitor_SUITE`, `erl_link_SUITE`, `bif_SUITE`, `dirty_nif_SUITE`, `node_container_SUITE`, …
- 32 suites poke emulator internals via `erts_debug:get/set_internal_state`; those values are only settable when the emulator exposes `available_internal_state` (toggled by `erts_test_utils:available_internal_state/1`).

## Notes for Agents
- Pick the suite by theme first (routing table below), then narrow with `-case`/`-group`; running one case is seconds-to-minutes, the whole dir is hours.
- The three compilation modes matter for bit syntax: a fix in the JIT/`ops.tab` or in `beam_ssa_bsm` should be validated with `bs_*_SUITE` **and** their `_no_opt`/`_stripped_types` variants (`make test ARGS="-suite bs_match_int_SUITE -suite bs_match_int_no_opt_SUITE -suite bs_match_int_stripped_types_SUITE"`).
- Anything touching the interpreter/JIT instruction set → `beam_SUITE`, `beam_literals_SUITE`, `op_SUITE`, `jit_SUITE`, plus `bs_*`/`guard_SUITE`; run both `FLAVOR=emu` and `FLAVOR=jit` because the two must agree.
- Anything touching GC, heaps or allocators → `gc_SUITE`, `alloc_SUITE`, `process_SUITE`, `binary_SUITE`, `nofrag_SUITE`, `mtx_SUITE`, `erts_debug_SUITE` (size/alloc reporting).
- Anything touching ports/drivers/NIFs → `driver_SUITE`, `port_SUITE`, `port_bif_SUITE`, `busy_port_SUITE`, `async_ports_SUITE`, `nif_SUITE`, `dirty_nif_SUITE`, `dirty_bif_SUITE`, `iovec_SUITE`, `erl_drv_thread_SUITE`, `hello_SUITE`, `ddll_SUITE`.
- Anything touching signals/process/monitor semantics → `signal_SUITE`, `process_SUITE`, `monitor_SUITE`, `erl_link_SUITE`, `receive_SUITE`, `after_SUITE`, `hibernate_SUITE`, `message_queue_data_SUITE`, `scheduler_SUITE`.
- Anything touching tracing → the `emulator_trace.spec` suite list; `trace_sessions.erl` is the shared parametrisation, so a change there affects `trace_SUITE` and `match_spec_SUITE` at once.
- Anything touching code loading/replacement (purge/delete/upgrade) → `code_SUITE`, `multi_load_SUITE`, `code_parallel_load_SUITE`, `module_info_SUITE`, `beam_literals_SUITE` (literal area collection).
- `estone_SUITE` is not just a benchmark: it is the PGO profiling workload compiled by `../emulator/Makefile.in`, so restructuring it affects profile-guided builds.
- Quick confidence run for small changes: `make emulator_test ARGS="-suite smoke_test_SUITE"` or use `emulator_smoke.spec` via `SPEC_POSTFIX=smoke`.

## Dependencies
- A configured **and built** OTP tree (`./configure && make emulator`, or `make emulator_test` which builds the emulator itself); test code alone is not enough since every suite drives a real `beam`.
- A working C compiler + `Makefile.src` configuration for the 30 data dirs that build drivers/NIFs/helper programs (`ts:compile_datadirs` fails the run otherwise).
- `lib/common_test/test_server` installed (`ts:install`), done automatically by `make/test_target_script.sh`; `ts_install_cth` is the CT hook most suites declare in `suite/0`.
- Platform tools used by individual suites: `perf` (`jit_SUITE:perf`), `taskset` (`scheduler_SUITE` cpu-binding), LTTng tools (`lttng_SUITE`), `proper`/`eqc` (`hash_property_test_SUITE`), valgrind/asan binaries for memory-checker runs.
- Network/loopback + working hostname resolution for the ~40 `?CT_PEER()` suites; `os_mon` is started by `distribution_SUITE`.
- `crypto`/`ssl` are *not* needed: `crypto_SUITE` only tests legacy `erlang` hash BIFs against checked-in reference values.

## Test Strategy
- Fast loop for one suite: `make emulator_test ARGS="-suite <name>_SUITE [-case <tc>] [-group <g>]"`; from `erts/emulator` use `make test ARGS=...`.
- Logs land in `erts/emulator/make_test_dir/ct_logs/index.html`; `ct_run`'s exit code is propagated, and skipped cases are only visible as `SKIPPED` in the logs (a "green" run may have skipped a lot).
- Re-run a released test set with `ts` from `erts/emulator/make_test_dir/test_server` when iterating without re-releasing (`ts:run(...)`), see `HOWTO/TESTING.md`.
- Flavour/type matrix: run a suite under `TYPE=debug` (assertions enabled) and, for scheduler/tracing/port code, also under `valgrind`/`asan`; `erts/emulator/valgrind/` and `../asan/` hold suppression/helper files.
- CI runs the emulator suite set with `SPEC_POSTFIX=gh` inside the project Docker image (`.github/workflows/reusable-test.yaml`, app `emulator` → `erts/emulator`); `emulator_smoke.spec`, `emulator_bench.spec`, `emulator_trace.spec`, `emulator_node_container_SUITE.spec` are the narrower alternatives.
- Coverage gaps: the `_no_opt`/`_stripped_types` variants exist only for the bit-syntax/`guard`/`map` suites; `hash_property_test_SUITE` silently skips unless PropEr/QuickCheck is installed; `dist_init_unix_SUITE_data` is dead weight.

## Routing Table (theme → suite files in this directory)
- terms/data types/arithmetic/BIFs → `a_SUITE`, `after_SUITE`, `big_SUITE`, `small_SUITE`, `float_SUITE`, `tuple_SUITE`, `binary_SUITE`, `list_bif_SUITE`, `num_bif_SUITE`, `bif_SUITE`, `guard_SUITE`, `op_SUITE`, `exception_SUITE`, `hash_SUITE`, `hash_property_test_SUITE`, `evil_SUITE`, `nested_SUITE`, `receive_SUITE`, `fun_SUITE`, `ref_SUITE`, `unique_SUITE`, `pseudoknot_SUITE`, `z_SUITE`
- bit syntax → `bs_construct_SUITE`, `bs_match_bin_SUITE`, `bs_match_int_SUITE`, `bs_match_misc_SUITE`, `bs_match_tail_SUITE`, `bs_bit_binaries_SUITE`, `bs_bincomp_SUITE`, `bs_utf_SUITE` (plus generated `*_no_opt_SUITE`/`*_stripped_types_SUITE`)
- BEAM instructions/literals/JIT/debug → `beam_SUITE`, `beam_literals_SUITE`, `jit_SUITE`, `erts_debug_SUITE`, `module_info_SUITE`, `erl_debugger_SUITE`, `native_record_SUITE`, `sensitive_SUITE`, `save_calls_SUITE`
- processes/signals/scheduling/info → `process_SUITE`, `process_max_heap_size_SUITE`, `scheduler_SUITE`, `signal_SUITE`, `monitor_SUITE`, `erl_link_SUITE`, `hibernate_SUITE`, `message_queue_data_SUITE`, `register_SUITE`, `statistics_SUITE`, `system_info_SUITE`, `system_profile_SUITE`, `time_SUITE`, `timer_bif_SUITE`, `prim_eval_SUITE`
- GC/allocators/locks → `gc_SUITE`, `alloc_SUITE`, `nofrag_SUITE`, `mtx_SUITE`, `atomics_SUITE`, `counters_SUITE`, `persistent_term_SUITE`
- ports/drivers/NIFs/IO → `port_SUITE`, `port_bif_SUITE`, `busy_port_SUITE`, `async_ports_SUITE`, `port_trace_SUITE`, `driver_SUITE`, `ddll_SUITE`, `erl_drv_thread_SUITE`, `nif_SUITE`, `dirty_nif_SUITE`, `dirty_bif_SUITE`, `send_term_SUITE`, `iovec_SUITE`, `hello_SUITE`, `decode_packet_SUITE`, `efile_SUITE`
- tracing/match specs → `trace_SUITE`, `trace_bif_SUITE`, `trace_local_SUITE`, `trace_nif_SUITE`, `trace_port_SUITE`, `trace_meta_SUITE`, `trace_call_count_SUITE`, `trace_call_time_SUITE`, `trace_call_memory_SUITE`, `trace_session_SUITE`, `tracer_SUITE`, `call_trace_SUITE`, `match_spec_SUITE`
- distribution → `distribution_SUITE`, `node_container_SUITE`
- code loading → `code_SUITE`, `code_parallel_load_SUITE`, `multi_load_SUITE`
- platform/legacy/special builds/bench/smoke → `os_signal_SUITE`, `dump_SUITE`, `lttng_SUITE`, `lcnt_SUITE`, `crypto_SUITE`, `estone_SUITE`, `smoke_test_SUITE`
- helper modules (not suites) → `erts_test_utils.erl`, `erts_test_utils.hrl`, `tracer_test.erl`, `trace_sessions.erl`, `literal_area_collector_test.erl`, `long_timers_test.erl`, `ignore_cores.erl`, `erts_test_destructor.erl`, `erts_test_sync_tracer.erl`, `ext_records.erl`, `crypto_reference.erl`, `random_iolist.erl`, `dgawd_handler.erl`, `dummy.erl`, `property_test/phash2_properties.erl`
- per-suite fixtures and test data → `<SUITE>_data/` (45 dirs; 30 contain `Makefile.src` and are compiled at test time)
- spec/CI entry points (not suites) → `emulator.spec`, `emulator_gh.spec`, `emulator_smoke.spec`, `emulator_bench.spec`, `emulator_trace.spec`, `emulator_node_container_SUITE.spec`, `emulator.spec.win`, `suite_release.exclude`
- code under test → `../emulator/` (beam/drivers/nifs/sys, own node); shared build harness → `../../make/app_targets.mk`, `../../make/test_target_script.sh`; run instructions → `../../../HOWTO/TESTING.md`
