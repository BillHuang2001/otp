# stdlib — Test Directory (Common Test)

## Intent
Common Test suites, fixtures and data for the `stdlib` OTP application — the full regression/behaviour test surface of the standard library (`lists`, `ets`, `erl_*` parser family, `gen_*`, `io`, `re`, `unicode`, `escript`, `shell`, archives, …).
1351 tracked entries: 90 `*_SUITE.erl` suites, 23 helper modules, 23 tracked `*_SUITE_data/` dirs, one shared `property_test/` dir, 4 spec files and 2 cover specs.
All suites run against an **installed OTP release**, not against the build tree (`TEST_NEEDS_RELEASE=true` in `../Makefile`).

## API Surface
- `*_SUITE.erl` — one Common Test suite per tested module/area; standard CT callbacks (`all/0`, `groups/0`, `init_per_suite/1`, `suite/0`, …); 73 of them declare `{ct_hooks,[ts_install_cth]}`.
- other `.erl` (23 files) — helper/fixture modules called by suites; they only end up in the released test dir if listed in `MODULES` in `Makefile` (22 of the 23 are; `copyright_generator` deliberately is not).
- `*_SUITE_data/` — data dirs reached from suites via `data_dir/1`; 23 are tracked, 4 more are created by `make release_tests_spec` (see Build wiring).
- `property_test/` — shared, property-tool-driven modules (see Property tests).
- `Makefile` — `MODULES`, `ERL_FILES`, `EXTRA_FILES`, `RELSYSDIR`, cover spec list; the authoritative list of what is compiled/installed.
- Test specs: `stdlib.spec`, `stdlib_bench.spec`, `stdlib_gh.spec`, `error_info.spec` (contents below).
- Cover specs: `stdlib.cover` (`{incl_app,stdlib,details}`) and `tty.cover` (`prim_tty`, `prim_tty_sighandler`, `user_drv`, `group`, `shell`, `edlin`, `edlin_expand`, `io_lib`).
- `suite_release.exclude` — legacy exclusion list (last touched in R13B03); **nothing in-tree reads it** and `release_tests_spec` does not install it, so it has no current effect.
- `rand_Xoroshiro928ss_dev.txt` — design notes for the `exro928ss` PRNG; reference documentation only, not read by tests and not installed.

## Build / run wiring (`Makefile`)
- `MODULES` drives `make_emakefile` → `Emakefile` → `make tests`, and is what `release_tests_spec` installs; `EBIN=.` so test beams are compiled next to the sources.
- Cross-app helpers are pulled from other apps' test dirs via `ERL_FILES`: `erts/emulator/test/erts_test_utils.erl`, `sasl/test/otp_vsns.erl`, `kernel/test/{rtnode,shell_test_lib}.erl` + `shell_test_lib.hrl`.
- `EXTRA_FILES=../../../otp_versions.table` (repo root) is installed flat into the test dir; `stdlib_SUITE:appup_test` reads it through `otp_vsns:read_state/0` (relative to the beam location or `../priv/`).
- `ERL_COMPILE_FLAGS` adds `-I../../../lib/kernel/include -I../../../lib/stdlib/include +nowarn_missing_spec_documented` and filters out `+deterministic`.
- `RELSYSDIR = $(RELEASE_PATH)/stdlib_test`; `release_tests_spec` installs the 4 specs, `Emakefile`, `ERL_FILES`, both cover files and `otp_versions.table` flat, then tars in every `*_SUITE_data` and `property_test`.
- It then creates four data dirs that do **not** exist in git, from sources elsewhere in the tree: `stdlib_SUITE_data` (`make/otp_version_tickets`), `unicode_SUITE_data` (`../uc_spec/PropList.txt`, `DerivedCoreProperties.txt`), `uri_string_SUITE_data` (`../doc/guides/uri_string_usage.md`), `records_SUITE_data` (`../../../system/doc/reference_manual/ref_man_native_records.md`) — the last three are doctest inputs.
- Before each run `ts:compile_datadirs/2` processes all `*_SUITE_data`: any `Makefile.src` there (e.g. `supervisor_SUITE_data/Makefile.src`) is turned into a Makefile and built, so data dirs may contain compilable Erlang.
- `make tests` (no release) compiles the suites locally via the generated `Emakefile`.

## Property tests
- All tool-driven properties live in `property_test/`: `array_prop`, `array_eqc`, `base64_prop`, `binary_prop`, `calendar_prop`, `ets_prop`, `gb_sets_prop`, `json_prop`, `lists_prop`, `queue_prop`, `sets_prop`, `shell_docs_prop`, `uri_string_recompose`.
- Suites pick them up with `ct_property_test:init_per_suite/1`, which compiles the whole `test/property_test/` dir (sibling of the suite's data dir) and **skips the suite** when no QuickCheck/PropEr/Triq is installed — so these suites silently pass as "skipped" in a plain OTP checkout.
- Direct callers: `array_SUITE` (`array_eqc`/`array_prop`), `calendar_prop_SUITE`, `json_SUITE` (`json_prop`), `shell_docs_SUITE` (`shell_docs_prop`, group `prop`).
- Dedicated property suites: `base64_property_test_SUITE`, `binary_property_test_SUITE`, `ets_property_test_SUITE`, `gb_sets_property_test_SUITE`, `lists_property_test_SUITE`, `queue_property_test_SUITE`, `sets_property_test_SUITE`, `uri_string_property_test_SUITE`.
- `property_test/README` marks the directory experimental and QuickCheck-licensed, intended to be run manually.

## Test specs (what each selects)
- `stdlib.spec` — `{suites,"../stdlib_test",all}` minus benchmark groups (`stdlib_bench_SUITE` groups `binary,base64,gen_server,gen_statem,unicode`; `ets_SUITE` group `benchmark`); this is the spec picked up by a plain `make test`.
- `stdlib_bench.spec` — runs only `stdlib_bench_SUITE` plus `ets_SUITE` group `benchmark`, and lists `unicode_util_SUITE`/`string_SUITE` cases purely so their data gets compiled; all other suites are skipped.
- `stdlib_gh.spec` — `stdlib.spec` plus `{skip_cases,gen_server_SUITE,[multicall_remote_old1]}` and CI ct-hook / event-handler / log-redirect settings; selected in GitHub CI via `SPEC_POSTFIX=gh`.
- `error_info.spec` — only the `error_info` group of `ets`, `io`, `lists`, `binary_module`, `maps`, `math`, `re` and `unicode` suites; assertions live in `error_info_lib:test_error_info/2,3`.
- Spec suite paths are relative to the *installed* test dir (e.g. `"../stdlib_test"`), so a spec edit needs a re-release or an explicit `ARGS="-spec <path>"`.

## Notable suites
- `stdlib_SUITE` — app-level: `test_server:app_test(stdlib)`, `appup` up/down one-major-version consistency (uses `otp_versions.table` + `otp_vsns`), `assert.hrl` checks; skips `appup_test` on development versions.
- `stdlib_bench_SUITE` (+ `stdlib_bench_SUITE_data/` generic server/statem/fib fixtures) — benchmarks for unicode/base64/binary/io/gen_server/gen_statem plus comparison groups; skipped by default specs.
- `ets_SUITE` (10.4k lines, largest) — ETS semantics, `benchmark` and `error_info` groups; `ets_tough_SUITE` holds the heavy/slow cases.
- `qlc_SUITE` (8.1k) — query-compiler semantics incl. `qlc_SUITE_data/join_info_compat.erl`; `dets_SUITE` (+ pre-OTP file-format fixtures) and `sofs_SUITE` cover the other "database-ish" stdlib modules.
- `erl_lint_SUITE` (6.2k) + `erl_lint_SUITE_data/` (bad behaviours, callbacks, predef names) — the lint/error-diagnostic suite; siblings `erl_scan_SUITE`, `erl_parse`-touching `erl_pp_SUITE`, `erl_eval_SUITE`, `erl_expand_records_SUITE`, `epp_SUITE` (+ data), `erl_anno_SUITE`, `erl_error_SUITE`, `erl_internal_SUITE`.
- `supervisor_SUITE` (4.1k) with fixtures `supervisor_1..4`, `supervisor_deadlock`, `naughty_child`, and `supervisor_SUITE_data/app_faulty` (built via `Makefile.src`); `supervisor_bridge_SUITE` alongside.
- `gen_server_SUITE`/`gen_statem_SUITE`/`gen_fsm_SUITE`/`gen_event_SUITE` — behaviour state machines; fixtures `dummy_via`, `dummy_h`, `dummy1_h`, `error_logger_forwarder`, plus `*_SUITE_data/oc_*.erl` and `format_status_*` modules; `proc_lib_SUITE`, `sys_SUITE` (fixtures `sys_sp1`, `sys_sp2`), `pool_SUITE`.
- `shell_SUITE`, `shell_docs_SUITE` (+ 715 golden files), `shell_docs_markdown_SUITE`, `edlin_expand_SUITE`/`edlin_context_SUITE` (shell completion/rendering).
- `re_SUITE` — PCRE2-based `re`; `pcre2`/`old_pcre1` cases replay upstream PCRE test outputs from `re_SUITE_data/` via `run_pcre_tests`/`run_old_pcre1_tests`; `replace_autogen`/`split_autogen` use the generated `re_testoutput1_replacement_test`/`re_testoutput1_split_test` modules.
- `unicode_SUITE`, `unicode_util_SUITE` (+ `unicode_util_SUITE_data/`: UCD `GraphemeBreakTest`, `LineBreakTest`, `NormalizationTest`, `unicode_table.bin`), `string_SUITE`; random-input helpers `random_unicode_list`, `random_iolist`.
- `json_SUITE` (+ `json_SUITE_data/`: the 323-file JSONTestSuite corpus), `sigils_SUITE`, `argparse_SUITE`, `records_SUITE` (native records + doctest of the reference manual), `maps_SUITE`, `lists_SUITE`, `string_SUITE`, `ms_transform_SUITE`, `id_transform_SUITE` (+ data).
- `escript_SUITE` (+ 37 `escript_SUITE_data/` scripts/archives) — shebang, emulator flags, archives, unicode, error handling; `c_SUITE` (+ `c_SUITE_data/m.erl`) tests the `c` command module.
- `io_SUITE`/`io_ansi_SUITE`/`io_proto_SUITE` (+ per-encoding `testdata_*.dat`), `format_SUITE`, `file_sorter_SUITE`, `filelib_SUITE`, `filename_SUITE` — io and file/path handling.
- Archives/compression: `tar_SUITE` (+ tarball fixtures), `zip_SUITE` (+ zip fixtures), `zstd_SUITE` (+ `zstd_SUITE_data/dict`).
- `peer_SUITE` (distributed `peer` node controller; groups `dist`, `dist_seq`, `tcp`, `standard_io`, `compatibility`, `remote`) and `slave_SUITE` (deprecated `slave` module via rsh).
- `error_logger_h_SUITE`, `log_mf_h_SUITE`, `error_info_lib` — logger/SASL log-file handling and error-info/stacktrace formatting.
- `beam_lib_SUITE` (+ `binref` fixture), `timer_SUITE`/`timer_simple_SUITE`, `rand_SUITE`/`random_SUITE`, `calendar_SUITE`/`calendar_prop_SUITE`, `dict_SUITE` (+ `dict_test_lib`), `sets_SUITE` (+ `sets_test_lib`), `gb_sets`/`digraph`/`digraph_utils`/`graph`/`queue`/`proplists`/`fixtable`/`select`/`math`/`base64_SUITE`, `binary_module_SUITE` (`binref`, error-info cases, sub-binary edge cases).
- `win32reg_SUITE` — Windows registry only (skips on Unix); `y2k_SUITE` — calendar/year-boundary cases.
- `zzz_SUITE` — by convention the last suite (`lc_graph` for the lock-order checker); `timer_simple_SUITE` deliberately asserts the opaque internal `tref/0` shapes.

## How to run
- `make test` from `lib/stdlib` — the only supported entry point: it releases OTP to `make_test_dir/Erlang ∅⊤℞`, installs the test dir into `make_test_dir/stdlib_test` and runs `ct_run` there (`TEST_NEEDS_RELEASE=true`).
- Single suite/case: `make ARGS="-suite lists_SUITE -case reverse" test`; `ARGS` must be on the outer `make` line and is passed straight through to `ct_run` (`-group`, `-spec`, `-case` all work).
- `../Makefile` overrides the `test` target to inject `ERL_ARGS="-enable-feature maybe_expr"`, so `maybe_expr` is enabled for stdlib tests (and only there).
- GitHub CI uses the same flow in docker with `SPEC_POSTFIX=gh`, `TEST_NEEDS_RELEASE=true`, `CTRUN_TIMEOUT=90` and `EXTRA_ARGS="-ct_hooks cth_surefire [{path,...}]"`.
- `ct_run` is invoked with `-sname test_server`, `-rsh ssh`, `-config .../test_server/ts.config` — suites may legitimately assume an `rsh`/ssh channel and short node names.

## Known Issues / Notes for Agents
- `MODULES` must list every helper module a suite calls; a helper missing from it compiles locally (`make tests`) but is absent from the released test dir, so the suite fails only under `make test`.
- System/network-dependent cases are known to fail in CI-like environments: `ets_SUITE:partly_bound` ("System dependent"), `gen_server_SUITE:multicall_down` ("Network dependent"), `gen_server_SUITE:multicall_remote_old1` ("Broken in docker") — the last is skipped by `stdlib_gh.spec`.
- `peer_SUITE` needs distribution and, for group `compatibility:old_release`, an installed OTP two releases back (skips if absent); its `remote:ssh` group needs a working ssh; `init_per_suite` sets `ERL_FLAGS=+S1 +SDio 1`.
- `suite_release.exclude` looks like a skip/installation list but no in-tree makefile or script reads it — do not rely on it.
- Property-test suites report as skipped unless QuickCheck/PropEr/Triq is on the code path, so "green" runs can hide untested properties.
- `shell_docs_SUITE_data` is generated: regenerate with `shell_docs_SUITE:update_render/0` or `update_render_modules/0`, which `git rm` and rewrite files under `$ERL_TOP/lib/stdlib/test/shell_docs_SUITE_data`; any doc/render change must be followed by that regeneration.
- `zstd_SUITE:generate_dict/0` (not in `all/0`) writes into `$ERL_TOP/lib/stdlib/test/zstd_SUITE_data` and shells out to `zstd --train`; the checked-in `zstd_SUITE_data/dict` is its output.
- `copyright_generator:erlang/0` is called only by `run_pcre_tests`' test-module generators, which is why it is intentionally absent from `MODULES`.
- `stdlib_SUITE:appup_test` depends on `otp_versions.table` being reachable from the released `otp_vsns` beam (or `../priv/`) and logs/skips on a development version — it is not a reliable indicator of a real bug in a source checkout.
- Some suites assume `ERL_TOP` and the source tree are present at runtime (`zstd_SUITE`, `shell_docs_SUITE`, `io_proto_SUITE` via `TERM`), so they can behave differently when only the release dir is kept.
- Runtime is dominated by the large suites (`ets_SUITE` 10.4k lines, `qlc_SUITE` 8.1k, `erl_lint_SUITE` 6.2k, `supervisor_SUITE` 4.1k, `dets_SUITE` 4.0k, `shell_SUITE` 3.7k); CI caps the whole run at 90 minutes.

## Routing Table
`test/` is flat — suites and helpers live directly here; the only subdirectories are data dirs and one shared property dir.
- `property_test/` → shared property modules compiled on demand by `ct_property_test:init_per_suite/1` (read-only for non-property work).
- Data dirs used by a single suite (23 tracked): `c_`, `dets_`, `edlin_expand_`, `epp_`, `erl_lint_`, `escript_`, `ets_`, `gen_event_`, `gen_fsm_`, `gen_server_`, `gen_statem_`, `id_transform_`, `io_proto_`, `json_`, `qlc_`, `re_`, `shell_docs_` (715 golden render files), `stdlib_bench_`, `supervisor_`, `tar_`, `unicode_util_`, `zip_`, `zstd_` — all suffixed `SUITE_data/`.
- Data dirs created at release time (not in git): `stdlib_SUITE_data/`, `unicode_SUITE_data/`, `uri_string_SUITE_data/`, `records_SUITE_data/`.
- `../src/` → the modules under test; `../Makefile` → `TEST_NEEDS_RELEASE` and the `test` override.

## See Also
- `../src/` — the `stdlib` modules these suites exercise; `../include/` — headers used by suites (`assert.hrl`).
- `../Makefile` — sets `TEST_NEEDS_RELEASE=true` and injects `-enable-feature maybe_expr`; `../../make/app_targets.mk` + `../../make/test_target_script.sh` — the generic `make test` driver.
- `../../common_test/` — Common Test framework, `ct_property_test`, and `test_server/ts*` that compiles data dirs.
- `../../kernel/test/` (`rtnode`, `shell_test_lib`), `../../sasl/test/otp_vsns.erl`, `../../../erts/emulator/test/erts_test_utils.erl` — helpers pulled into the released test dir.
- `../../uc_spec/`, `../../doc/guides/uri_string_usage.md`, `../../../system/doc/reference_manual/ref_man_native_records.md` — doctest/data inputs installed at release time.
