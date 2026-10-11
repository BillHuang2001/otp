# stdlib/src — Erlang Standard Library Sources

## Intent
Flat source directory of the `stdlib` OTP application: 96 tracked `.erl` modules plus two build-time-generated ones (`erl_parse.erl`, `unicode_util.erl`) — 98 modules in total.
There are **no subdirectories**: the modules themselves are the public API surface, and module names are effectively part of the Erlang language.
Also holds the app resource templates (`stdlib.app.src`, `stdlib.appup.src`), the Erlang grammar (`erl_parse.yrl`) and the internal (non-exported) headers.
Documentation is written as `-moduledoc`/`-doc` attributes inside these files.

## API Surface
Build/release metadata:
- `Makefile` — `MODULES` (98 entries), `HRL_FILES` (the 6 public headers in `../include/`), `INTERNAL_HRL_FILES` (`dets.hrl erl_tar.hrl otp_internal.hrl json.hrl swar_ascii.hrl`), `VSN = $(STDLIB_VSN)` from `../vsn.mk`, `primary_bootstrap_compiler` target.
- `stdlib.app.src` — `{vsn,"%VSN%"}` and `{modules,[...]}` (identical set to `MODULES`); `{registered,[timer_server,rsh_starter,take_over_monitor,pool_master,dets]}`; `{applications,[kernel]}`; `runtime_dependencies` = `sasl-3.0 kernel-11.0 erts-16.0.3 crypto-4.5 compiler-5.0 syntax_tools-3.2.1`.
- `stdlib.appup.src` — release-upgrade instruction template, also `%VSN%`-substituted.
- `../examples/erl_id_trans.erl` is compiled into `$(EBIN)` by the `opt` target (source lives outside this dir); it is documented (`erl_id_trans` is in the CODE doc group).

Module groups (membership of the published docs groups is defined in `../doc/docs.exs` `groups_for_modules`; files marked *(h)* carry `-moduledoc false` and are internal):

**BASIC DATA TYPES** — core collection APIs:
- `lists`, `maps`, `binary`, `records` (native-record runtime API, OTP 29+; debugging/tools-oriented despite being public).

**HIGH-LEVEL DATA STRUCTURES**:
- `array`, `dict`, `gb_sets`, `gb_trees`, `graph`, `digraph`, `digraph_utils`, `orddict`, `ordsets`, `proplists`, `queue`, `sets`, `sofs`.

**STRINGS / TEXT / UNICODE**:
- `string`, `unicode`, `re`, `base64`, `uri_string`, `json` (EEP-68 JSON codec, OTP 27+), `unicode_util` *(generated, h)*.

**PROCESSES / OTP BEHAVIOURS**:
- `gen_server`, `gen_statem`, `gen_event`, `gen_fsm` (deprecated), `supervisor`, `supervisor_bridge`, `proc_lib`, `sys`, `pool`, `log_mf_h`, `gen` *(h — behaviour call-dispatch helpers)*.

**IO / FILES**:
- `io`, `io_ansi`, `io_lib`, `filelib`, `filename`, `file_sorter`; internal helpers `io_lib_format`, `io_lib_fread`, `io_lib_pretty`, `erl_posix_msg` *(all h)*.

**TERM STORAGE**:
- `ets`, `dets`, `qlc`; internal helpers `dets_server`, `dets_sup`, `dets_utils`, `dets_v9`, `qlc_pt` *(all h)*.

**CODE — scan/parse/lint/eval/compile support**:
- `erl_scan`, `erl_parse` *(generated from `erl_parse.yrl`)*, `erl_lint`, `erl_pp`, `erl_eval`, `epp`, `erl_expand_records`, `erl_features`, `erl_anno`, `erl_error`, `erl_internal`, `ms_transform`, `beam_lib`; internal helpers `erl_bits`, `eval_bits`, `erl_abstract_code`, `erl_compile`, `erl_stdlib_errors` *(all h)*.

**COMPRESSION / ARCHIVES**:
- `zip`, `erl_tar`, `zstd` (`zstd.erl` is a thin wrapper over the erts NIF; the codec lives in `erts/`).

**SHELL / INTERACTIVE TOOLING**:
- `shell`, `shell_default`, `c` (shell convenience commands), `shell_docs`, `edlin`, `edlin_expand`; internal helpers `edlin_key`, `edlin_context`, `edlin_type_suggestion`, `shell_docs_markdown`, `man_docs` (man-page generation) *(all h)*.

**MISC / RUNTIME & OS — documented outside the doc groups**:
- `argparse`, `calendar`, `math`, `rand`, `random` (deprecated → `rand`), `timer`, `peer` (→ replaces `slave`), `slave` (deprecated), `escript`, `win32reg` (Windows registry port driver), `error_logger_file_h`, `error_logger_tty_h` (legacy logger handlers, *h*).

**GENERATED, internal**:
- `otp_internal` *(h)* — auto-generated central table of deprecated/removed functions, types and callbacks.

## Constraints
- **Generated files — never edit by hand**: `erl_parse.erl` (yecc from `erl_parse.yrl`, via `$(ERLC) ... erl_parse.yrl`), `unicode_util.erl` (from `../uc_spec/gen_unicode_mod.escript`), and `otp_internal.erl` (from `-deprecated`/`-removed`/`-deprecated_type`/… attributes across the whole OTP tree; regenerate with `./otp_build update_deprecations`, implemented by `../scripts/update_deprecations`).
- `erl_parse.erl` and `unicode_util.erl` are git-ignored and absent from a fresh checkout — they appear only after a build.
- Adding/removing a module requires editing **both** `MODULES` in `src/Makefile` and `{modules,[...]}` in `stdlib.app.src` (kept in the same order; no automatic sync).
- Compile flags are strict: `-Werror +warn_deprecated_catch +warn_obsolete_bool_op -I../include -I../../kernel/include` — any new warning fails the build.
- Dependency rule: `{applications,[kernel]}` only; use of `crypto`, `compiler`, `syntax_tools`, `sasl` must stay optional/late-bound even though they appear in `runtime_dependencies`.
- Public headers belong in `../include/` and must be added to `HRL_FILES` for release packaging; headers here are internal to stdlib.
- Public documentation lives in `-moduledoc`/`-doc` attributes (not in `../doc/`); internal modules must say `-moduledoc false`.

## Notes for Agents
- **Bootstrap ordering**: the `primary_bootstrap_compiler` target compiles `epp`, `erl_anno`, `erl_error`, `erl_features`, `erl_scan`, `erl_parse`, `erl_lint`, `graph`, `io`, `otp_internal`, `erl_internal` against the bootstrap compiler into `$(BOOTSTRAP_COMPILER)/ebin` first — nothing else in OTP can be compiled until these exist. Changes to this set are extremely sensitive.
- `erl_parse.yrl` is the tracked grammar source; `erl_parse.beam` for bootstrap is generated into `$(BOOTSTRAP_COMPILER)/egen` + `ebin`.
- 23 of the 96 `.erl` files are `-moduledoc false` internals: `dets_server dets_sup dets_utils dets_v9 edlin_context edlin_key edlin_type_suggestion erl_abstract_code erl_bits erl_compile erl_posix_msg erl_stdlib_errors error_logger_file_h error_logger_tty_h eval_bits gen io_lib_format io_lib_fread io_lib_pretty man_docs otp_internal qlc_pt shell_docs_markdown`.
- Deprecated-but-present: modules `gen_fsm` (→`gen_statem`), `slave` (→`peer`), `random` (→`rand`); deprecated functions in `calendar`, `lists`, `queue`, `sys`. Their entries are mirrored (generated) in `otp_internal.erl` — add the `-deprecated` attribute in the module, then regenerate.
- `swar_ascii.hrl` is an internal SWAR/ASCII helper header used only by `string.erl`.
- **`epp` host-runtime surface** (everything a host BEAM must provide to run `epp` in-process for `compile:file/2`): a plain `spawn`ed server process (NOT linked — monitored; `epp.erl` `open/1`); a `file:open/2` `[read]` io-device usable with BOTH `file:*` (`position`/`read`/`close`) and `io:*` requests; `io:scan_erl_form` → `{get_until,unicode,Prompt,erl_scan,tokens,[Loc,Opts]}` (`io.erl` `scan_erl_form/4`); `io:getopts`/`io:setopts` with `binary` and `{encoding,latin1|utf8}` (`epp.erl` `read_encoding_from_file/2`, `set_encoding/2`); `file:path_open/3` + `code:lib_dir/1` (for `-include_lib`), `filename:*`, `os:getenv/1`; and `erl_scan`/`erl_parse`/`erl_anno`/`erl_features`/`unicode`/`string`/`io_lib`. No `ets`/`persistent_term`/`register`ed names (only `group_leader` via `io:fwrite` on the unknown-request error path). Its `receive`s are untimed except an `after 0` flush in `wait_epp_reply/2`.
- `io_ansi` implements the terminal/virtual-terminal-sequence layer used by `shell`/`edlin`.
- Per-app Dialyzer PLT apps (`DIA_PLT_APPS=compiler crypto`) are set in `../Makefile`, not here.

## Test Strategy
- No tests live in this directory: the Common Test suites are in `../test/` (`lists_SUITE`, `maps_SUITE`, `erl_parse_SUITE`, `beam_lib_SUITE`, `dets_SUITE`, …) plus property suites backed by `../test/property_test/`.
- `make test` from `../` runs them against an installed release (`TEST_NEEDS_RELEASE=true`) with the `maybe_expr` feature enabled (`ERL_ARGS="-enable-feature maybe_expr"`).

## Routing Table
This node is flat — no child directories. Use the module groups above as a navigation index; every module is a sibling file in this directory.
- Term/aggregate data structures → `lists.erl`, `maps.erl`, `array.erl`, `sets.erl`, `gb_trees.erl`, `orddict.erl`, `proplists.erl`, `sofs.erl`.
- Text/Unicode/encoding → `string.erl`, `unicode.erl`, `re.erl`, `base64.erl`, `uri_string.erl`, `json.erl`.
- OTP behaviours → `gen_server.erl`, `gen_statem.erl`, `gen_event.erl`, `supervisor.erl`, `proc_lib.erl`, `sys.erl`.
- Erlang front end (scan/parse/lint/pp/eval) → `erl_scan.erl`, `erl_parse.yrl`, `erl_lint.erl`, `erl_pp.erl`, `erl_eval.erl`, `epp.erl`.
- Storage/archives → `ets.erl`, `dets.erl`, `qlc.erl`, `zip.erl`, `erl_tar.erl`, `zstd.erl`.
- Shell → `shell.erl`, `shell_default.erl`, `edlin.erl`, `edlin_expand.erl`, `shell_docs.erl`, `c.erl`.
- Node/OS/util → `peer.erl`, `timer.erl`, `calendar.erl`, `argparse.erl`, `rand.erl`, `filename.erl`, `filelib.erl`.

## See Also
- `../include/` — the 6 public headers exported to other applications (`assert.hrl`, `erl_compile.hrl`, `erl_bits.hrl`, `ms_transform.hrl`, `qlc.hrl`, `zip.hrl`).
- `../doc/` — ExDoc config (`docs.exs` doc groups) and the external `.md` doc sources for `ms_transform`, `peer`, `qlc`, `re`, `shell`, `sofs`.
- `../test/` — Common Test / property suites for these modules.
- `../uc_spec/` — Unicode Character Database inputs + `gen_unicode_mod.escript` producing `unicode_util.erl`.
- `../scripts/update_deprecations` — regenerates `otp_internal.erl`.
- `../vsn.mk` — `STDLIB_VSN`, substituted for `%VSN%`.
- `../../erts/` — BIFs/NIFs backing `ets`, `binary`, `records`, `zstd`, and the preloaded modules.
- `../kernel/` — the only runtime dependency; `../compiler/` consumes `erl_scan`/`erl_parse`/`erl_lint`/`erl_pp`.
