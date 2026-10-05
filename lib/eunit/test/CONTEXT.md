# EUnit Test Suite (`lib/eunit/test`)

## Intent
Common Test suite that tests EUnit itself: `eunit:test/1,2` semantics, the test macros, all test *primitives*, fixtures/setup behaviour, `{node, ...}` spawning, surefire XML reporting and command-line parsing.
Everything here is a *consumer* of EUnit's public API — this is the self-test/regression suite, not the EUnit library (`../src/`).

## API Surface
- `eunit_SUITE.erl` — the only Common Test suite (`all/0` has 15 cases, `groups/0` is empty, `suite/0` installs the `ts_install_cth` hook).
  - `app_test`, `appup_test` — `test_server:app_test/1` / `appup_test/1` on `eunit`.
  - `eunit_test` — `file:set_cwd(code:lib_dir(eunit))` then `eunit:test(eunit)`, i.e. runs EUnit's own embedded self-tests.
  - `eunit_exact_test` / `primitive_test` — the same list of primitives (`[eunit, eunit_tests]`, `{application, stdlib}`, `{file, ...}`, `{dir, ...}`, `{generator, fun|MFA}`, `{with, V, Fs}`) run with `{exact_execution, true}` resp. `false`, checking listener-reported pass/fail/skip/cancel counts via `{report, {eunit_test_listener, [self()]}}`.
  - `primitive_test` pre-compiles `../examples/fib.erl` into the examples dir so `{dir, ...}` finds 8 tests.
  - `fixture_test` — plain `{setup, S, I}`, `{setup, S, [{module, M}]}`, `{foreach, ...}`, `{foreachx, ...}`.
  - `node_test` — `{node, ...}` with an explicit test list, with an *instantiator* fun (asserts it receives the node name as an atom), with extra peer args (`"+S 1"`), and with `net_kernel` auto-start during system init: it writes a helper module into `priv_dir` and re-runs `erl` with `-eval`/`-s`/`-run`/`-S` through `run_erl_cmd/2` (30 s guard).
  - `surefire_utf8_test` / `surefire_latin_test` / `surefire_c0_test` / `stacktrace_at_timeout_test` — `check_surefire/1` runs a module with `{report, {eunit_surefire, [{dir, "."}]}}`, decodes the emitted `TEST-<mod>.xml` as unicode, validates it with `xmerl_scan:file/1`; `tc0` asserts the control chars `\n`, `\r`, `\t` survive; `stacktrace_at_timeout_test` asserts a timed-out test's XML contains `"in call from"`.
  - `surefire_ensure_dir_test` — passes a non-existent `dir` in `priv_dir` ("c1") and verifies `eunit_surefire` creates it (then removes it).
  - `scale_timeouts_test` — asserts `{scale_timeouts, N}` actually scales the `{timeout, 1, F}` generator timeout (×2 ⇒ ≥2000 ms, ×0.25 ⇒ ≥250 ms) and the default 5 s test timeout (×0.01), by timing `times_out_test_/0` / `times_out_default_test/0` with `erlang:monotonic_time/1`.
  - `report_failed_setup_inparallel_test` — under `{inparallel, [...]}` a `setup` that exits must be reported as `cancel => 1` (not silently dropped) while the sibling test still runs (`pass => 1`), `skip => 0`, `fail => 0`.
  - `parse_commandline_test` — table-driven test of `eunit_data:parse_command_line/2` (whitespace, double/single quotes, escapes, unbalanced quotes, literal backslash).
- `eunit_test_listener.erl` — a custom `eunit_listener` behaviour implementation; `start(Options)` is given `[TestProcess]`, and on terminate it sends `{test_report, #{pass=>,fail=>,skip=>,cancel=>}}` to that process and replies `ok`/`error` on the listener `stop` protocol; `handle_begin/end/cancel` are no-ops.
- `tc0.erl`, `tutf8.erl`, `tlatin.erl` — dummy EUnit test modules with deliberately hostile names/descriptions: `tc0` uses character codes 0..31 (raw control chars in test names and descriptions), `tutf8` is `%% coding: utf-8`, `tlatin` is `%% coding: latin-1` (both compile with `nowarn_latin1_binary`); they are inputs to the surefire cases, not suites.
- `ttimesout.erl` — one `{timeout, 1, _}` generator that sleeps 20 s; input to `stacktrace_at_timeout_test`.
- `Makefile` — compiles `MODULES = eunit_test_listener eunit_SUITE tc0 tlatin tutf8 ttimesout` in place (`EBIN = .`) via a generated `Emakefile` (`+nowarn_missing_spec_documented`; `+deterministic` is filtered out of `ERL_COMPILE_FLAGS`); new helper modules must be added to `MODULES`.
  `release_tests_spec` installs the sources, `eunit.spec`, `eunit.cover`, the `Emakefile` and a copy of `../examples` into `$(RELEASE_PATH)/eunit_test`.
- `eunit.spec` — `{suites,"../eunit_test",all}`; the suites path is relative to the spec file's own directory in the released `eunit_test` tree.
- `eunit.cover` — coverage spec: `{incl_app,eunit,details}` (whole app instrumented) with `{excl_mods,eunit,[eunit_test]}`.

## Constraints
- Never edit the expected pass/fail/skip/cancel maps to match observed output — they are assertions about EUnit's documented counting behaviour.
- The hard-coded counts are coupled to EUnit's own self-tests: `../src/eunit_tests.erl` (7 tests: `?UNDER_EUNIT`, `?LET`, `?IF`, `?MATCHES`, `?capturedOutput`) and `../examples/fib.erl` (8 assertions); changing those requires updating `eunit_exact_test`/`primitive_test`.
- `exact_execution => false` counts differ from `true`: non-exact mode also pulls in the `<Module>_tests` module (`eunit_data:extract_module_tests/2`), so `[eunit, eunit_tests]` yields 14 resp. 7 and `{file, "./ebin/eunit.beam"}` yields 7 resp. 0.
- Empty `groups/0` and the mutation of the process-wide cwd mean the suite is not designed to run cases in parallel.
- Do not change the encoding declarations in `tc0`/`tutf8`/`tlatin` — they are the point of the surefire encoding cases.

## Test Strategy
- `make test` in `lib/eunit` → `make/test_target_script.sh`: builds & `release_tests_spec`-installs into `lib/eunit/make_test_dir/eunit_test` (plus `examples/`), installs the CT test server, then runs `ct_run -spec .../eunit_test/eunit.spec` with `-logdir make_test_dir/ct_logs`.
- Single case: `make ARGS="-suite eunit_SUITE -case surefire_utf8_test" test`.
- Full OTP test environment: run `ts:run(eunit, eunit, [batch])` from `release/tests/test_server` (uses `eunit.spec`/`eunit.cover`).
- Requires a built/installed OTP where `code:lib_dir(eunit)` resolves to the build tree (`eunit_test`, `eunit_exact_test`, `primitive_test` read `./ebin/*.beam` and `./examples` from there).
- `node_test` needs distribution/`peer` (it auto-starts `net_kernel`) and an `erl` executable reachable by `os:find_executable/1` (it is invoked via `ct:get_progname()` from an `open_port`).
- `check_surefire/1` uses `xmerl` (XML validation) and `unicode` (decoding the XML file).

## Known Issues
- Cases mutate the cwd and never restore it: `file:set_cwd(code:lib_dir(eunit))` (`eunit_test`, `eunit_exact_test`, `primitive_test`) and `file:set_cwd(priv_dir)` (surefire cases) — cwd leaks into later cases.
- `check_surefire/1` writes `TEST-<Module>.xml` into the *current* directory (deleting any previous file first) and deliberately ignores the EUnit result, since some of `tc0`/`tutf8`/`tlatin` tests fail on purpose.
- `scale_timeouts_test` asserts wall-clock windows (2000–5000 ms, 250–1000 ms) and is therefore timing-sensitive on slow/loaded machines.
- `check_test_results/2` waits only `?TIMEOUT` (1000 ms) for the listener report, so a slow listener path fails with `no_test_report_not_received`.

## Notes for Agents
- `tc0`, `tutf8`, `tlatin`, `ttimesout` are *EUnit* test modules consumed by `eunit:test/2` inside the CT cases — they are not Common Test suites despite living in `test/`.
- `eunit_test_listener` exists to observe EUnit's internal result protocol, so most assertions are on aggregate counts, not on stdout.
- Do not add a `MODULES`-list omission: any new helper module must be added to `test/Makefile` `MODULES` to be compiled and released.

## See Also
- `../src/` — the EUnit implementation under test (`eunit`, `eunit_data`, `eunit_proc`, `eunit_surefire`, `eunit_tests`, `eunit_test`).
- `../examples/` — `fib.erl` (8 EUnit assertions) and `tests.txt`, copied into the released test dir; `{dir, <examples>}` in the primitive cases.
- `../include/eunit.hrl` — the test macros exercised by the helper modules.
- `../doc/` — EUnit user documentation.
