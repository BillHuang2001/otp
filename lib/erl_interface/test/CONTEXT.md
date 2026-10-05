# erl_interface Test Suite

## Intent
Verifies the `ei` C library (`libei`) and the `erl_call` program.
Each suite starts a small C program that links against the built library, and Common Test drives it from Erlang — over a command/data port, and (for the distribution suites) over real `epmd`/distribution connections between the emulator and the C node.
This is the only test directory in OTP that uses the `Makefile.first` ts mechanism (see HOWTO/TESTING.md).

## API Surface

### Test suites (`EI_MODULES` in `Makefile`)
| Suite | Covers |
| --- | --- |
| `ei_accept_SUITE` | `ei_publish`/`ei_accept`/`ei_receive` from a C node: pending and established connections, export funs, bitstrings, random term round-trips, threaded accept (`eiaccnode` binary), monitoring a C node process |
| `ei_connect_SUITE` | `ei_connect` from a C node: `ei_send`, `ei_reg_send` (incl. a 32 MB term), `ei_rpc`, `ei_make_ref`/`ei_make_pid`, `ei_link`/`ei_unlink`, `ei_set_tracelevel`, `ei_format_pid`; grouped into `default` and `ussi`; `ei_threaded_send` spawns the `einode` binary with 15 threads |
| `ei_decode_SUITE` | `ei_decode_*` for every external term type, incl. long/ulong/longlong/ulonglong, non-optimal encodings, UTF-8 atoms, iodata, huge foreign pids/ports/refs, `ei_cmp_nc` |
| `ei_encode_SUITE` | `ei_encode_*` for every external term type incl. failure paths (`test_ei_encode_fails`) and UTF-8 atoms + explicit length |
| `ei_decode_encode_SUITE` | Erlang→C→Erlang round trips of funs, pids, ports, refs, big integers, large pid/port/ref containers on foreign node names; the only suite in the smoke spec |
| `ei_format_SUITE` | `ei_x_format()` string→term construction (atoms, tuples, lists, maps, without version prefix) |
| `ei_print_SUITE` | `ei_print_term()`/`ei_s_print_term` term→string printing (atoms, tuples, lists, strings, maps, funs, binaries, bitstrings, integers) |
| `ei_tmo_SUITE` | Socket timeouts on accept/connect/recv/send, driven by manual partial distribution handshakes from Erlang; `ei_dflags` checks distribution flag negotiation |
| `ei_global_SUITE` | `ei_global_register`/`ei_global_unregister`/`ei_global_whereis`/`ei_global_names` against the local node |
| `erl_call_SUITE` | The `erl_call` binary: node start/lookup, `-address` (port/host:port), `-fetch_stdout`, `-no_result_term`, multi-io-request group leaders, unresolvable hostname, `-timeout` |
| `port_call_SUITE` | `erlang:port_call` into a dynamically loaded driver (`port_call_drv`) via `erl_ddll`; only case is `basic`, skipped outside unix/linux, unix/sunos and win32 |
| `runner` | Shared Erlang-side driver (not a suite): starts the C program on a `{packet,4}` port, sends the test case index, says terms, decodes replies |

### `runner` protocol (Erlang side)
- `runner:start/2,3` opens the C program as a port with `{packet,4}`+`exit_status` and sends the 2-byte test case index; `runner:test/2,3` runs a case that only reports success/failure.
- Commands to the C side: `$t` + `term_to_binary(Term)` (`send_term/2`), `$e` (`send_eot/1`); replies: `$t` term, `$b` bytes, `$f` fail(ure), `$N` NULL, `$e` eot, `$m` + message to log (`get_term/1,2`, `recv_eot/1`).
- `finish/1` sends eot, expects eot back and asserts exit status 0.
- `runner:init_per_testcase/3` prepends a `valgrind_cmd_fun` when `VALGRIND_LOG_DIR` is set (see Test Strategy).

### `all_SUITE_data/` (shared C harness, linked by every C test program)
- `ei_runner.c/.h` — the reusable C side of `runner`: `run_tests/3`, `TESTCASE(name)` macro, `report()/fail*()`, `read_packet()`, `send_buffer()`, `send_bin_term()`, `message()`; `ei_runner.h` includes the public `ei.h`.
- `init_tc.erl` — generator run by each suite's `Makefile.first`: scans `TESTCASE(Name)` in the suite's `*_test.c` and emits `<name>_cases.hrl` (`-define(Name, {<data_dir>/<prog>, Index})`) plus `<name>_decl.c` (the `main()` that calls `run_tests/3`).
- `gccifier.c` / `gccifier.sh` — wrapper used as `CC` in the data Makefiles: translates gcc-style arguments for MSVC, adjusts debug builds, and echoes the command line; `gccifier.sh` is substituted for the compiled wrapper on cross builds (see `all_SUITE_data/Makefile.src` `@IFEQ@ (@cross@, yes)`).
- `my_ussi.c/.h` — user-supplied socket implementation (`ei_socket_callbacks my_ussi`) used by the `ussi` groups and by `eiaccnode`.
- `Makefile.first` — compiles `init_tc.erl` with `erlc` so the per-suite generators can be run.
- `Makefile.src` — template that builds `runner.o`, `ei_runner.o`, `my_ussi.o` and `gccifier`; every `<suite>_data/Makefile.src` links `../all_SUITE_data/ei_runner.o` (plus `my_ussi.o` where used).

### Specs and coverage files
- `erl_interface.spec` — `{suites,"../erl_interface_test",all}` (the default spec); `erl_interface_smoke.spec` — runs only `ei_decode_encode_SUITE` (used via `SPEC_POSTFIX=smoke`).
- `erl_interface.cover` — `{incl_app,erl_interface,details}` for coverage runs; listed in `COVER_FILE`.
- `Makefile` `SPEC_FILES` installs only `erl_interface.spec` and `erl_interface_smoke.spec`.
- `erl_interface.dynspec` and `erl_interface.spec.vxworks` are NOT installed (not in `SPEC_FILES`) and are not picked up by the installed-test workflow.

## Build Mechanism for the C Test Programs
- No per-suite `Makefile` is committed; `Makefile.first` and `Makefile.src` are templates processed by the OTP test server (`ts_lib:make_non_erlang/2`): it runs `Makefile.first` with `make`, substitutes `@var@` placeholders in `Makefile.src` into `Makefile`, then runs `make`.
- Substituted variables come from `lib/common_test/test_server/variables.<TYPE>.<FLAVOR>` produced by `ts:install()`, extended by `ts_erl_config:erl_interface/2`: `erl_interface_libpath`(= `lib/erl_interface/obj[.debug]/<target>`), `erl_interface_eilib`, `erl_interface_include`, `erl_interface_sock_libs`, `erl_interface_threadlib`, `erl_interface_mk_include` (= `lib/erl_interface/src/<target>/eidefs.mk`, which supplies `EI_LDFLAGS`, `THR_DEFS`).
- `make test` (`make/test_target_script.sh`) installs the suites to `make_test_dir/erl_interface_test`, installs the ts test server, then calls `ts:compile_datadirs(".../variables.<TYPE>.<FLAVOR>", "*_SUITE_data")` to build all data dirs before `ct_run`.
- `ts_install_cth` (declared in `suite/0` of every suite except `erl_call_SUITE`) re-runs `ts_lib:make_non_erlang/2` for the suite's data dir at start, and skips it when the ts `variables` say `cross = "yes"`.

## Test Strategy
- Run from `lib/erl_interface`: `make test` (all suites via `erl_interface.spec`), `make test ARGS="-suite ei_decode_SUITE -case test_ei_decode_long"`, or `make test SPEC_POSTFIX=smoke`; also `make erl_interface_test` from `ERL_TOP`.
- Prerequisites: the whole OTP tree built enough for `ct_run`, and `libei` built — `lib/erl_interface/obj[.debug]/<target>/libei.a` plus `lib/erl_interface/src/<target>/eidefs.mk`; `erl_call_SUITE` additionally needs `lib/erl_interface/bin/<system_architecture>/erl_call`.
- Distribution is required for `ei_accept`, `ei_connect`, `ei_tmo`, `ei_global` and `ei_decode_encode`: they publish/connect C nodes, so a running `epmd` and a resolvable hostname are needed; ct_run is started with `-sname test_server`, and `ei_tmo_SUITE` registers fake node names with epmd itself over `127.0.0.1`.
- `ei_decode`, `ei_encode`, `ei_print`, `ei_format` need no distribution — they only exchange terms over the port.
- Slow areas: `ei_tmo_SUITE` (timetrap 1 minute; several 10–20 s waits on socket buffers), `ei_connect_SUITE` (timetrap 30 s; 32 MB term in `ei_reg_send_large`), the threaded cases (`einode`, `eiaccnode`).
- Valgrind: set `VALGRIND_LOG_DIR` (plus optional `VALGRIND_LOG_XML=yes`, `VALGRIND_MISC_FLAGS`) to run every C test program under valgrind (`runner:init_per_testcase/3`).
- Coverage gaps: `port_call_SUITE` only exercises basic driver load/`port_call`/unload (its own header notes missing reference-count and driver-unload-on-process-death cases); `ei_tmo_SUITE`'s handshake cases depend on the distribution protocol version assumed by the suite's `?EI_DIST_*`/`?COMPULSORY_DFLAGS` constants.

## Dependencies
- C compiler plus `make`; on unix the threaded `libei` needs pthreads (supplied by `@erl_interface_threadlib@`, `-lpthread`; `@erl_interface_sock_libs@` adds `-lsocket` where required).
- The test C files include only public headers (`ei.h`, `ei_connect.h`, `ei_runner.h`, `my_ussi.h`) — no `erts/` internal headers.
- `erl` and `erlc` must be on `PATH` while data dirs are prepared: per-suite `Makefile.first` runs `erl -noinput -pa ../all_SUITE_data -s init_tc run <prog>` from inside the data dir, and `all_SUITE_data/Makefile.first` runs `erlc init_tc.erl`.
- `erts_test_utils` is compiled into the test release from `$(ERL_TOP)/erts/emulator/test/erts_test_utils.erl` because `ei_decode_SUITE` and `ei_accept_SUITE` use `mk_ext_pid/3`, `mk_ext_port/2`, `mk_ext_ref/2`.
- `ei_decode_SUITE` / `ei_decode_encode_SUITE` hand a real port to the C side, so they invoke `/bin/true` (or `/usr/bin/true` on darwin, `cmd /q /c true` on win32) and `sort`.

## Known Issues / Notes for Agents
- `Makefile.src` in this directory (`lib/erl_interface/test/Makefile.src`) is dead legacy: its rules reference `eterm_test.c`, `ext_test.c`, `format_test.c`, `print_term.c`, `match_test.c`, which do not exist, and this directory is never processed as a `*_data` dir.
- `ei_global_SUITE_data/Makefile.src` still carries a leftover `LIBERL = @erl_interface_lib@`, but nothing consumes it: `@erl_interface_lib@` resolves via `ts_erl_config` to a `liberl_interface.a` path that the application no longer builds, and the only real prerequisites/link inputs are `$(LIBEI)` (`libei.a`), `ei_runner.o` and `my_ussi.o`.
- `README` describes the ClearCase-era workflow and is obsolete — do not follow it. To add a suite, copy a similar suite plus its `*_SUITE_data/` and rename the C file.
- `Makefile`'s `tests`/`clean`/`docs` targets are empty on purpose: tests are driven by `make test` → `make/test_target_script.sh`, not by this Makefile.
- Tests are installed to `$(RELEASE_PATH)/erl_interface_test` (not `lib/<app>-<vsn>/test`), which is why the specs use `{suites,"../erl_interface_test",all}`.
- `ei_connect_unreachable_tmo/0` returns `{require,test_host_not_reachable}`, so Common Test auto-skips it; the variable is commented out in `lib/common_test/test_server/ts.config` and only reachable test hosts define it.
- `ei_tmo_SUITE` (`ei_connect_tmo`, `ei_accept_tmo`, `ei_send_failure_tmo`, `ei_dflags`, plus the `normal_connect`/`normal_accept` helpers) performs distribution handshakes by hand with `gen_tcp` + `erl_epmd`, so it is sensitive to dist protocol details and to the suite's local `?EI_DIST_LOWEST_ASSUMED`/`?EI_DIST_HIGH`/`?COMPULSORY_DFLAGS` constants.
- `ei_accept_SUITE:init_per_suite/1` calls `erts_debug:set_internal_state(next_pid/next_port, 1 bsl 32)` on 64-bit to exercise large pids/ports; it also prints and depends on `rand` seeds, so failures are reproducible via `rand:seed`.
- The per-suite generated files (`<prog>_cases.hrl`, `<prog>_decl.c`) are created in the data dir at test time; a hand-compiled C program without running `Makefile.first` will fail to compile the suite.
- `erl_interface.dynspec` would skip `all_SUITE` for mismatched C compilers but references a suite that does not exist here; it and `erl_interface.spec.vxworks` are dormant in the standard install/test flow.

## Routing Table
- `all_SUITE_data/` → shared C harness and data-dir build scaffolding (`ei_runner`, `init_tc`, `gccifier`, `my_ussi`, `Makefile.first`, `Makefile.src`)
- `ei_accept_SUITE_data/` → `ei_accept_test.c` (accept/publish/receive cases) and `eiaccnode.c` (multi-threaded accepting C node spawned by the suite)
- `ei_connect_SUITE_data/` → `ei_connect_test.c` (send/reg_send/rpc/ref/pid/link cases) and `einode.c` (15-thread connecting C node used by `ei_threaded_send`)
- `ei_decode_SUITE_data/` → `ei_decode_test.c` (all `ei_decode_*` cases)
- `ei_encode_SUITE_data/` → `ei_encode_test.c` (all `ei_encode_*` cases incl. failures)
- `ei_decode_encode_SUITE_data/` → `ei_decode_encode_test.c` (single round-trip case used by the smoke spec)
- `ei_format_SUITE_data/` → `ei_format_test.c` (`ei_x_format()` cases)
- `ei_print_SUITE_data/` → `ei_print_test.c` (`ei_print_term()`/`ei_s_print_term` cases)
- `ei_tmo_SUITE_data/` → `ei_tmo_test.c` (timeout cases driven by Erlang handshakes)
- `ei_global_SUITE_data/` → `ei_global_test.c` (global register/whereis/names cases; its `Makefile.src` links `$(LIBEI)` = `libei.a` plus `../all_SUITE_data/ei_runner.o` and `my_ussi.o`)
- `port_call_SUITE_data/` → `port_call_drv.c`, a shared library (`@SHLIB_RULES@`) loaded with `erl_ddll` from the data dir
- `Makefile` → suite list (`EI_MODULES`), `ERTS_MODULES`, `SPEC_FILES`, `COVER_FILE`, `RELSYSDIR` install target (no `Makefile.first`); `Makefile.src` → dead legacy template (see Known Issues)
- `runner.erl` → Erlang-side driver for the C test programs
- `erl_interface.spec`, `erl_interface_smoke.spec`, `erl_interface.cover` → installed test spec + coverage config; `erl_interface.dynspec`, `erl_interface.spec.vxworks` → dormant legacy specs
- `README` → obsolete ClearCase-era notes

## See Also
- `../CONTEXT.md` — the `erl_interface` application (the `ei` library, `erl_call`, `vsn.mk`)
- `../../common_test/test_server/CONTEXT.md` — the `ts` framework (`ts_lib:make_non_erlang/2`, `ts_erl_config`, `ts_install_cth`) that builds these data dirs
- `../../../HOWTO/TESTING.md` — how `make test` and the `Makefile.first`/`Makefile.src` mechanism work
- `../../../erts/emulator/test/erts_test_utils.erl` — helper used by `ei_decode_SUITE` and `ei_accept_SUITE` to build large external pid/port/ref containers
