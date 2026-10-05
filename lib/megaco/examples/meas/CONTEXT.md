# megaco examples/meas — codec measurement & performance tooling

## Intent
Standalone megaco codec measurement/benchmark tools that ship as *examples* (`doc/examples/meas` in a release), not as runtime code of the megaco application.
Three runnable tools: `meas` (per-codec encode/decode timing), `mstone1` and `mstone2` (SMP "performance value" benchmarks), plus a shared helper library and a message-pack transform utility.
The tools are compiled into `../../ebin`, into this directory itself (for the test release) and into `megaco_test/examples/meas`, and are typically driven from a modified copy of one of the `*.sh.skel` scripts.

## API Surface
Erlang modules (all listed in `MODULES` in `modules.mk`):
- `megaco_codec_meas.erl` — the *meas* tool. `start/0,1,2` (also `start([MessagePackage])`, `start(Factor)`), `start(Factor, Opts)` where `Opts :: #{verbose := bool(), bench := bool()}`.
  Measures, per codec config, decode then encode of every message in a message package; warms up, then sizes the iteration count from a ~1 s (`?MEASURE_COUNT_TIME`) sample, runs each measurement in a monitored process (100 s timeout) and reports average size/encode/decode nanoseconds.
  Writes Excel-compatible text files `message_size.xls`, `decode_time.xls`, `encode_time.xls`, `total_time.xls` in the CWD; `bench => true` instead returns `{bench, [{ConfigName, {Size, EncodeAvg, DecodeAvg}}]}` for the test suite.
- `megaco_codec_mstone1.erl` — *mstone1*: one long-running runner process per codec config, all started together, loop encode/decode until the run time expires; the total number of messages processed is the mstone value.
  Entry points `start/0..4` (all codec configs) and `start_flex/…`, `start_only_drv/…`, `start_no_drv/…` which restrict the configs to flex/driver/no-driver variants; `#{bench := true}` returns `{bench, MStone}`. Reads `time_test` by default and runs 10 minutes (`?MSTONE_RUN_TIME`).
- `megaco_codec_mstone2.erl` — *mstone2*: same idea but each worker does exactly *one* pass over its messages and exits; a loader process respawns a replacement worker for the same config until a timer fires, then drains them.
  `start/0..5` with a `Mode` of `standard | flex | no_drv | only_drv`, `#{bench := true}` returns `{bench, MStone}`; reports per-config counts plus number of processes spawned.
- `megaco_codec_mstone_lib.erl` — shared helper for the mstone tools (also used by `meas`): `expanded_messages/2,3`, `expand_codecs` (per driver mode), `start_flex_scanner/0` / `stop_flex_scanner/1` (flex-scanner port handler process), `detect_version/3`, `display_os_info/0`, `display_system_info/0` (schedulers, bindings, cpu topology, …), `display_alloc_info/0`, `display_app_info/0`, `parse_runtime/1`.
- `megaco_codec_transform.erl` — message-package utility: `codecs/0`, `default_message_package/0` (`time_test`), `messages/0,1` and `export_messages/0,1`.
  Reads `{Codec, [{Name, binary()}]}` from `<MessagePackage>.msgs` (CWD first, then the directory of the module) and lazily transforms the base-codec messages into all other codecs, so a single pretty-encoded package serves every codec. `export_messages` writes the old on-disk layout (`<pkg>/pretty|compact|ber|per|erlang/<name>`) for external tools.

Build / run assets:
- `modules.mk` — `MODULES` (module list), `SCRIPT_SKELETON_SRC` (`meas.sh.skel.src`, `mstone1.sh.skel.src`, `mstone2.sh.skel.src`), `MESSAGE_PACKAGES = time_test.msgs`.
- `Makefile.in` — autoconf template; generates `Makefile` (via megaco `configure`). Compiles modules to `../../ebin` and to this dir, installs skeletons/`.erl`/messages as data under `$(RELEASE_PATH)/lib/megaco-$(VSN)/doc/examples/meas` and under `$(RELEASE_PATH)/megaco_test/examples/meas`.
- `meas.sh.skel.src`, `mstone1.sh.skel.src`, `mstone2.sh.skel.src` — shell-script templates transformed by `perl -p -e 's?%VSN%?$(VSN)?'` into `*.sh.skel` (gitignored) so a copy can be run straight from a release.
  - `meas.sh.skel.src` is a 3-line skeleton: sets `ERL_HOME` (must be edited) and runs `erl -noshell +sbt tnnps -pa $MEAS_HOME -s megaco_codec_meas start -s init stop` (a commented `-s megaco_codec_meas start time_test` variant exists).
  - `mstone1.sh.skel.src` parses `-mp -h -a -t -f/-s -d -sbt --` and loops re-invoking `erl -noshell … -s megaco_codec_mstone1 start|start_flex|start_no_drv|start_only_drv <mp> <rt> <factor>`, teeing each run to `mstone1-*.log`.
  - `mstone2.sh.skel.src` parses `-mp -h -a -t -s -d -sbt --` and runs `-s megaco_codec_mstone2 start <rt> <mode> <mp>`, teeing to `mstone2-<sched>.log`.
- `time_test.msgs` — the default message package: `{pretty, [{msg01a, <<…>>}, …]}` (~62 KB, ~148 named binary messages, a mix of version 1/2/3 MEGACO messages) used by all three tools; license tracked in `time_test.msgs.license`.

## Constraints
- `Makefile` is generated from `Makefile.in` by the megaco `configure` (`AC_CONFIG_FILES([examples/meas/Makefile:examples/meas/Makefile.in])` in `lib/megaco/configure.ac`) — never edit a generated `Makefile`.
- `*.sh.skel` are generated from `*.sh.skel.src` by `perl` with `%VSN%` substitution (make rules at the bottom of `Makefile.in`) and are gitignored — edit the `.src` templates only.
- Every new module must be added to `MODULES` in `modules.mk`; skeleton scripts go in `SCRIPT_SKELETON_SRC`.
- This directory is built as part of the megaco app build via `lib/megaco/subdirs.mk` (`SUB_DIRS = src examples/simple examples/meas`).
- Modules must stay out of the runtime application: they are examples/measurement code, compiled with `-pa $(ERL_TOP)/lib/megaco/ebin` and shipped under `doc/examples`, never loaded by megaco itself.
- Message packages must keep the `{Codec, [{Name, binary()}]}` structure and the base codec must be one of `pretty | compact | ber | per | erlang`.

## Notes for Agents
- `time_test.msgs` stores its messages *pretty-encoded*; the other codecs (compact, ber, per, erlang) are produced at run time by `megaco_codec_transform` — do not add a directory tree of per-codec message files.
- The benchmarks exercise the megaco codec modules directly (`megaco_pretty_text_encoder`, `megaco_compact_text_encoder`, `megaco_ber_encoder`, `megaco_per_encoder`, `megaco_erl_dist_encoder`) via `decode_message/3` / `encode_message/3` / `version_of/2` with a config that always includes `{version3, v3}`; encoding configs are `[]`, `[native]` (ber/per), `[flex_scanner]` (pretty/compact) and `[megaco_compressed]`/`[compressed]` (erlang). `flex_scanner` configs need a running `megaco_flex_scanner` port started by `megaco_codec_mstone_lib:start_flex_scanner/0`.
- The generated shell skeletons contain a literal placeholder `ERL_HOME=<path to otp top dir>` that must be edited before running them.
- Dead/vestigial knobs: `MEASURE_TIME` in `megaco_codec_meas.erl` is defined but unused, and `Makefile.in` still passes `-DMSTONE_TIME=...` / `-DVERBOSE_STATS` / `-DUSE_VERBOSE_STATS` compile flags that no current module consumes; `mstone1.sh.skel.src`'s usage text is copy-pasted from mstone2 (it describes `mstone 1`).
- `modules.mk` sets `README = MEAS_README`, but no such file exists and no makefile uses the variable — don't go looking for it.
- `Clean` target removes the locally compiled `.beam` files in this directory (`EBIN_TEST = .` is this directory), so `make clean` here also removes the test-release copies.

## Dependencies
- `perl` (required by `configure` and used to expand `%VSN%` in the `*.sh.skel` templates; exposed as `PERL` in the generated `Makefile`).
- A built megaco application (`lib/megaco/ebin`) on the code path, including the flex scanner driver for flex configs; running the shell skeletons also needs an installed OTP release at `$ERL_HOME` (they use `$ERL_HOME/lib/erlang/lib/megaco-<vsn>/examples/meas`).

## Test Strategy
- Covered by `../megaco_examples_SUITE.erl` (`meas`, `mstone1`, `mstone2` cases plus `bench_*` variants), which adds this directory to the code path as `examples/meas` of the installed test release, purges the example modules between cases, and calls `megaco_codec_meas:start(Factor, Opts)` / `megaco_codec_mstone1:start(Opts, "60s", Factor)` / `megaco_codec_mstone2:start(Opts, Factor, "10s", standard)` with `#{verbose => false}` or `#{bench => true}` on a worker node.
- `TEST_NEEDS_RELEASE=true` in `lib/megaco/Makefile`: the examples are only available after a release install, so the suite is run against an installed release (`$RELEASE_PATH/megaco_test/examples/meas`).
- There is a `dialyzer` target here (`DIA_PLT = megaco_example_meas.plt`, apps `erts et asn1 kernel stdlib compiler debugger runtime_tools crypto mnesia wx`); it is not part of the default build.

## See Also
- `../simple/` — the other megaco example subdirectory (sibling; read-only from here, escalate writes to `lib/megaco/`).
- `../../doc/guides/megaco_debug.md` — user-facing description of the meas/mstone tools, the message-package file format and the binary-codec `native` config.
- `../../test/megaco_examples_SUITE.erl` and `../../test/Makefile` — how these examples are tested and installed as `megaco_test/examples`.
- `../../src/` — the encoder modules the benchmarks drive (`megaco_pretty_text_encoder`, `megaco_compact_text_encoder`, `megaco_ber_encoder`, `megaco_per_encoder`, `megaco_erl_dist_encoder`, `megaco_flex_scanner`).
