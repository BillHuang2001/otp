# tools — Common Test Suites

## Intent
Common Test suites (`*_SUITE.erl`) covering the OTP `tools` application: `cover`, `xref`, `make`, `tags`, the profilers (`fprof`, `eprof`, `cprof`, `tprof`, `lcnt`), the Emacs mode and crash-dump decryption.
Built and released as `$(RELEASE_PATH)/tools_test`; run against an installed release (the app Makefile sets `TEST_NEEDS_RELEASE=true`), never against the in-tree `lib/tools/ebin`.

## API Surface
- `<suite>_SUITE.erl` — one Common Test suite per covered tool; `suite/0`+`all/0` (+`groups/0` where used) are the CT entry points.
- `<suite>_SUITE_data/` — per-suite fixtures, copied next to the suite and used via `data_dir`.
- `ignore_cores.erl` — core-dump-redirecting helper (cwd/PWD/path juggling, moves `/cores/core.*`); listed in `MODULES` and compiled with the suites, but no suite in this directory currently references it (its copy in `lib/kernel/test/` is the one in use by `heart_SUITE`).
- `Makefile` — compiles `MODULES` in place: generates `Emakefile` via `$(ERL_TOP)/make/make_emakefile` and runs `erl -make`; `EBIN = .`; `RELSYSDIR = $(RELEASE_PATH)/tools_test`.
- `Makefile` `MODULES` (the compile/release list) = `cover_SUITE eprof_SUITE tprof_SUITE emacs_SUITE fprof_SUITE cprof_SUITE lcnt_SUITE make_SUITE tools_SUITE xref_SUITE prof_bench_SUITE crashdump_SUITE tags_SUITE ignore_cores`.
- `tools.spec` — `{suites,"../tools_test",all}`; used by `make test` (default spec) after Erlang is released.
- `tools_bench.spec` — `{suites,"../tools_test",[prof_bench_SUITE]}`; run `prof_bench_SUITE` alone.
- `tools.spec.win` — `{topcase,{dir,"../tools_test"}}` Windows variant (handled by the ct runner).
- `tools.cover` — `{incl_app,tools,details}`; cover config so the suites themselves and the `tools` app are instrumented under CT.

## How to run
- Whole app: `make test` in `lib/tools` — releases Erlang (`TEST_NEEDS_RELEASE=true`), compiles the test server, copies `tools_test` + `*_SUITE_data`, then runs `ct_run -spec ../tools_test/tools.spec`.
- Single suite: compile in this dir (`make` → `erl -make`) then `ct_run -dir . -suite <suite>_SUITE -pa .` (or pass `ARGS=-suite ...` to `make test`).
- Benchmark only: `ct_run -spec tools_bench.spec` (see `prof_bench_SUITE`) — deliberately excluded from `tools.spec`.
- `ts:compile_datadirs(..., "*_SUITE_data")` is run by the release test script, so data dirs that hold `.erl` modules get compiled before the suites run.
- The suites are compiled in place with `+nowarn_missing_spec_documented`; the Makefile strips `+deterministic` from `ERL_COMPILE_FLAGS`.

## Suites
- `tools_SUITE` — sanity check of the app: `app_test` (`test_server:app_test(tools, tolerant)`) and `appup_test`; timetrap 1 min.
- `cover_SUITE` — the largest suite (~2150 lines); exercises `cover` end-to-end: start/stop, `compile*`/`analyse`/`export`/`import`, distribution + reconnect cases, and many named regression cases (`otp_NNNN`, `gh_NNNN`, `eep37`, `eep49`); `all/0` dynamically trims the case list when a `cover_server` is already running, and `coverage`/`coverage_analysis` are the self-coverage cases; timetrap 2 min. Suites call `cover:compile_beam(?MODULE)` so the suite module must be compilable from its beam.
- `xref_SUITE` — the `xref` API driven in four groups (`xref`, `files`, `analyses`, `misc`); ~3000 lines and the main exercise of the private `xref_*` modules (reader/scanner/compiler/library), via `xref:start/add_application/analyze/...`; timetrap 2 min.
- `fprof_SUITE` — `fprof` tracing/analysis (stack/tail recursion, spawn, unicode, `parsify_maps`); writes `*.trace`/`*.analysis` into `priv_dir`; timetrap 240 s.
- `eprof_SUITE` — `eprof` rootset/profiling and pattern profiling (`basic`, `basic_option`, `tiny`, `eed`); compiles `eprof_test`/`eed` from the data dir into `priv_dir` and cds there; timetrap 60 s.
- `tprof_SUITE` — `tprof` call-count/call-time/call-memory, live tracing, patterns, sessions; `all/0` puts the two non-parallelisable cases first and groups the rest under a `parallel()` group that is only `[parallel]` when `schedulers_online < 64`; timetrap 60 s.
- `cprof_SUITE` — `cprof` basic/on-load/multi-module profiling; also compilable `-define(STANDALONE,1)` to run without Common Test; timetrap 30 s.
- `lcnt_SUITE` — lock-counting profiling: loads `.lcnt` files from the data dir and runs conflict/location/swap-key/implicit-start/crash-before-collect plus a `smoke_lcnt` trace; timetrap 4 min.
- `make_SUITE` — `make:all/1` and `make:files/2`, include-change recompilation, `emake_opts`, and the `otp_6057` group; `netload` uses a `peer` node for `netload` compilation.
- `tags_SUITE` — `tags:file/2` and `tags:files/2`: writes `TAGS` into the (priv) data dir and asserts content; no `suite/0`, so no `ts_install_cth` hook.
- `crashdump_SUITE` — `crashdump:pem_decrypt` on purposefully truncated RSA and ML-KEM crash dumps plus their `*_key.pem`; timetrap 4 min.
- `emacs_SUITE` — checks `lib/tools/emacs/erlang.el`: BIF highlighting, interpreted/compiled load, indentation and fontification fixtures; `init_per_testcase` skips everything except `bif_highlight` unless an Emacs >= 27.1 is on `PATH`, and skips all cases if `erlang.el` is missing.
- `prof_bench_SUITE` — single `overhead` benchmark comparing `fprof`/`eprof`/`cprof`/`cover` overhead; timetrap 30 min; skips if a `cover_server` is running or if `priv_dir` has < 16 GB free (it writes ~4 GB of trace); not part of `tools.spec`.

## Fixtures (routing table)
| Path | Description |
| --- | --- |
| `cover_SUITE_data/` | Standalone/module fixtures for cover, incl. issue-named dirs (`compile_beam/`, `include_eunit_hrl/`, `included_functions/`, `otp_6115/`, `otp_11439/`, `otp_16476/`) |
| `xref_SUITE_data/` | Module/release fixtures incl. `rel2/lib/app*-*/ebin` fake release tree, `lib_test/`, `md/`, `update/` |
| `crashdump_SUITE_data/` | `rsa_*`/`ml-kem_*` encrypted+decrypted dumps and key PEMs |
| `emacs_SUITE_data/` | `erlang.el` fontification/indent fixtures (`highlight`, `indent`-style terms, `records`, `macros`, `type_specs`, …) |
| `eprof_SUITE_data/` | `eprof_test.erl`, `eed.erl`, `ed.script` |
| `fprof_SUITE_data/` | `foo.erl`, `fprof_unicode.erl` |
| `cprof_SUITE_data/` | `cprof_SUITE_test.erl` |
| `lcnt_SUITE_data/` | Recorded `.lcnt` lock-count logs (`big_bang_40`, `ehb_3_3_hist`) |
| `make_SUITE_data/` | `Emakefile` + `test[1-5].erl`, `test_incl*.erl`, `incl_src/` |
| `tags_SUITE_data/` | `m1.erl`/`m2.erl`/`valid.erl`/`bad.erl` |
| `prof_bench_SUITE_data/` | `sofs_copy.erl` benchmark target |
| `../emacs/` | `erlang.el`/`erlang.elc` sources the Emacs suite drives (sibling — read-only, escalate writes to parent) |
| `../src/` | The tools modules under test (sibling — read-only, escalate writes to parent) |
| `../../common_test/test_server/ts_install_cth.erl` | Shared `ts_install_cth` hook referenced by `suite/0` in most suites (sibling — read-only) |

## Constraints
- Fixture dirs `*_SUITE_data/` are inputs; suites that write (e.g. `tags_SUITE`) must write into `priv_dir`/the copied data dir, never back into the tree.
- `data_dir` is a per-run copy of the fixture dir — do not rely on writes persisting between runs.
- Adding/removing a suite requires updating `MODULES`, the `SPEC_FILES`/release lists where relevant, and `tools.spec`/`tools_bench.spec` coverage.
- Tests must pass against an installed release (`TEST_NEEDS_RELEASE=true`), so suites must use the released `ern`/`ct_run`, not in-tree paths.

## Known Issues
- `prof_bench_SUITE` needs ~4 GB of free disk and no running `cover_server`; it silently skips otherwise, so a "pass" may not have measured anything.
- `emacs_SUITE` silently skips when Emacs is absent or < 27.1 — on CI images without Emacs it provides no coverage.
- `cover_SUITE`'s case list changes depending on whether `cover_server` is already running (reduced `StartStop` set), so the number of executed cases is not fixed.
- `tprof_SUITE` disables `[parallel]` when `schedulers_online >= 64`, changing runtime characteristics on large machines.

## Dependencies
- Common Test (`ct_run`) from a released OTP; the test server is compiled once into `lib/common_test/test_server/variables.${TYPE}.${FLAVOR}`.
- `emacs_SUITE`: an `emacs` binary >= 27.1 on `PATH` (optional, skips).
- `prof_bench_SUITE`: `os_mon` (`disksup`) to check free disk space.
- `make_SUITE` `netload`: distributed Erlang (`peer`).

## Notes for Agents
- `ignore_cores.erl` is identical to the copy in `lib/kernel/test/`; it is not a suite despite being in `MODULES`.
- `EPROF`/`FPROF` suites compile fixture modules at runtime into `priv_dir` — compile errors surface as runtime failures, not build failures.
- `xref_SUITE` covers the private `xref_*` modules only indirectly; there are no direct unit tests for the private API.
- `+deterministic` is intentionally filtered out of `ERL_COMPILE_FLAGS` here.

## See Also
- `../CONTEXT.md` (parent, `lib/tools`) — describes the modules these suites exercise (`cover`, `xref`, `make`, `tags`, profilers, `emacs/`).
- `../../common_test/test_server/` — the test server and `ts_install_cth` hook.
- `../../make/test_target_script.sh` and `../../make/app_targets.mk` — implement the `test` target used here.
