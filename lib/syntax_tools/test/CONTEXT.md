# syntax_tools — Test Suites

## Intent
Common Test suites and fixtures for the `syntax_tools` OTP application (sources in `../src`, public header `../include/merl.hrl`).
Covers `erl_syntax`/`erl_syntax_lib` node wrapping and round-trip (`revert`), `epp_dodger` macro-tolerant parsing, `erl_comment_scan`/`erl_recomment`, `erl_prettypr`/`prettypr` printing, and `merl`/`merl_transform`.

## API Surface
| Artifact | Role |
| --- | --- |
| `syntax_tools_SUITE.erl` | Main CT suite, 22 cases (see table below). |
| `merl_SUITE.erl` | CT suite for `merl`/`merl_transform`, 4 cases. |
| `syntax_tools.spec` | ct_run spec `{suites,"../syntax_tools_test",all}` — path is relative to the released test tree, not this dir. |
| `syntax_tools.cover` | Cover spec (`{incl_app,syntax_tools,details}`) for the coverage run. |
| `Makefile` | Builds only `MODULES = syntax_tools_SUITE merl_SUITE` via `make_emakefile` + `erl -make`, `EBIN = .`. |
| `syntax_tools_SUITE_data/` | Input fixtures parsed/printed by the suites. |

### `syntax_tools_SUITE` cases
| Case | Intent |
| --- | --- |
| `app_test` | `test_server:app_test(syntax_tools)` — app file / module list sanity (pedantic mode). |
| `appup_test` | `test_server:appup_test(syntax_tools)` — appup consistency. |
| `smoke_test` | `epp_dodger:parse_file/1` over every `lib/*/src/*.erl` in the tree — no error_markers allowed (12 min timetrap). |
| `revert` | `epp:parse_file` + `erl_syntax` wrap/map → `revert` → `compile:forms([strong_validation])` over stdlib sources. |
| `revert_map` | Reverting `map_field_assoc` trees built from internal `{tree,...}` representation. |
| `revert_map_type` | Reverting `map_field_exact`/`map_field_assoc` inside a typed record field type. |
| `revert_preserve_pos_changes` | A `pos` change (`erl_anno:set_generated/2`) applied via `set_pos/2` must survive `revert/1` when the node carries a user annotation. |
| `wrapped_subtrees` | `erl_syntax:subtrees/1` must work on every `erl_parse` node wrapped with `set_pos/2`. |
| `t_type` | `erl_syntax` type constructors ↔ `erl_parse` type AST equality (`tuple_type`, `map_type`, `user_type_application`, `function_type`, ranges, bitstring types, …). |
| `t_abstract_type` | `erl_syntax:abstract/1` → type/`is_literal`/`revert`/`concrete` for literals (incl. nested maps, binaries, strings). |
| `t_erl_parse_type` | `erl_syntax:type/1` and `is_leaf/1` for a wide list of parsed expressions/comprehensions/records (incl. comprehensions, zip and strict generators, native records `#_`). |
| `t_epp_dodger` | All four `epp_dodger` entry points (`parse_file`, `quick_parse_file`, `parse`, `quick_parse`) on the generic fixtures must produce no error markers. |
| `t_epp_dodger_clever` | `clever` mode on `epp_dodger_clever.erl`. |
| `gh11155` | Regression: `erl_syntax_lib:fold/3` and `map/2` must not crash on a macro-named native record (`macro_native_record.erl`). |
| `t_comment_scan` | `erl_comment_scan:file/1` + `erl_recomment:recomment_forms/2` + pretty-print of the generic fixtures. |
| `t_prettypr` | `erl_prettypr:format/2` ({paper,120},{ribbon,110}) output must re-parse with `epp:parse_file` without errors. |
| `test_named_fun_bind_ann` | `erl_syntax_lib:annotate_bindings/2` env/bound/free annotations for named funs (bug #4733). |
| `test_maybe_expr_ann` | Annotation of `maybe_expr`/`maybe_match_expr`/`else_expr`, with and without `else` (PR #8811). |
| `test_mc_ann` | Annotation of a map-comprehension (`mc`) with a generator. |
| `test_zip_ann` | Annotation of zipped generators (`zip`, `generate`, `generate_strict`). |
| `is_literal` | `erl_syntax:is_literal/1` recognises utf8 binaries, sigils and triple-quoted strings. |
| `test_erl_recomment` | `erl_recomment:recomment_forms/2` on a non-formlist (regression) and on a formlist, asserting exact internal `{tree,...}`/`{attr,...}` shapes. |

### `merl_SUITE` cases
| Case | Intent |
| --- | --- |
| `merl_smoke_test` | `?Q`/`merl:quote`/`term`/`tree`/`subst`/`match`/`switch`, comment preservation, patterns with `_@`/`'@'`/`~"..."`. |
| `transform_parse_error_test` | `merl_transform:parse_transform/2` rewrites `merl:quote`/`qquote` calls even for unparsable quoted text. |
| `otp_15291` | Regression (OTP-15291/ERL-719): `merl:quote` of `() -> ok`, `(_,_) -> ok`, `(_) -> ok`. |
| `compile_and_load_with_comments` | `merl:compile_and_load/1` on quoted modules carrying comments. |

### `syntax_tools_SUITE_data/` fixtures
| File | Used by |
| --- | --- |
| `syntax_tools_SUITE_test_module.erl` | Generic fixture list: `epp_dodger`, comments, pretty-print, revert, smoke test; exercises `@doc`/`-type`/`-spec`, EEP-49 `maybe`, EEP-58 map comprehensions, strict generators, EEP-73 zip generators, EEP-78 multi-comprehensions and native records. |
| `syntax_tools_test.erl` | Generic fixture list; heavy macro usage (incl. macros as function names, in patterns, `-?attrib(...)`), records, `-include_lib`. |
| `type_specs.erl` | Generic fixture list; `-type`/`-opaque`/`-callback` forms, `_` in types, bitstring types with size expressions, multi-clause `-spec`. |
| `specs_and_funs.erl` | Generic fixture list; multi-clause `-spec` with `when` constraints (OTP-15519/ERL-815). |
| `epp_dodger_clever.erl` | Only `t_epp_dodger_clever`; deliberately has no terminating full stop (clever mode must cope). |
| `macro_native_record.erl` | Only `gh11155`; `-record(?state, {a})` with a macro record name. |

## Constraints
- Both suites must keep `suite() -> [{ct_hooks,[ts_install_cth]}]` — required by the `ts`/test_server based runner.
- Regression cases are named after their issue ids and must not be renamed: `gh11155`, `otp_15291`; `test_maybe_expr_ann`, `test_zip_ann`, `test_mc_ann`, `test_named_fun_bind_ann`, `is_literal` are regression tests too.
- `MODULES` in the Makefile and the `*_SUITE` names are referenced by `syntax_tools.spec` and by `ts:run(syntax_tools, ...)`; renaming/adding a suite requires updating the Makefile.
- Cases are expected to be added to `all/0` (no groups are used; `groups/0` returns `[]`).
- The `Makefile` sets `ERL_COMPILE_FLAGS += +nowarn_missing_spec_documented` and filters `+deterministic` out; `EBIN = .` means suite beams land next to the sources.
- Fixture files are data, not tests: some are intentionally not compilable standalone (`epp_dodger_clever.erl`, `macro_native_record.erl` uses a macro record name) — do not "fix" them.
- Do not add non-suite modules here; the bulk of merl unit tests lives in `../src/merl_tests.erl`.

## Known Issues / Gotchas
- `smoke_test`, `revert` and `wrapped_subtrees` walk the live OTP tree via `code:lib_dir()` (`all lib/*/src/*.erl` for smoke, stdlib sources for the others): they are slow (1–12 min timetraps), spawn one worker per scheduler, and need a source checkout with `lib/*/src` present — a beams-only install is not enough.
- `revert_file/2` accepts compile errors originating from `epp` `moduledoc`/`doc` attributes (internal OTP doc forms) but nothing else.
- Several cases assert exact internal representations (`{tree,...}`, `{attr,Pos,[],none}`, annotation lists in a fixed order) — changing `erl_syntax` annotation layout or the `erl_syntax_lib` env/bound/free annotation order breaks them.
- Parsing here goes through `{feature, maybe_expr, enable}` and `erl_features:init_parse_state/2` (`res_word_option/0`): new language features must be enabled in these helpers and covered in the fixtures/`revert` path.
- `t_erl_parse_type`/`t_type` depend directly on `erl_parse`/`erl_scan` behaviour and on compiler-accepted syntax; a parser change can require updating the expectation tables.
- `merl_SUITE` includes `eunit/include/eunit.hrl` only for `?assert*` macros — these are still Common Test cases, not eunit tests.
- `merl_tests.erl` in `../src` is an EUnit module that is *not* in `ERL_FILES`/the app `modules` list, so it is not built by the normal make; run it separately (e.g. `eunit:test(merl_tests)`) when touching `merl`/`merl_transform`.

## Test Strategy
- Primary: `(cd lib/syntax_tools && make test)` or `make ARGS="-suite syntax_tools_SUITE -case revert" test`; `ERL_TOP` must point to the source root and ERL must be built.
- `make/test_target_script.sh` releases the tests into `lib/syntax_tools/test/make_test_dir/syntax_tools_test` (via `release_tests_spec`, which also untars `*_SUITE_data`), installs the common_test test_server, runs `ts:compile_datadirs/2` over `*_SUITE_data`, then invokes `$ERL_TOP/bin/ct_run` with the released `syntax_tools.spec`.
- `TEST_NEEDS_RELEASE` is not set, so no full release is built and `$ERL_TOP/bin/ct_run` is used; a working `erl` from `$ERL_TOP/bin` is required.
- Alternative (released tree): start `erl` in `$TESTROOT/test_server` and run `ts:run(syntax_tools, [batch])`; coverage via the `syntax_tools.cover` spec.
- Suite code is built in place by `make tests`/`make test` through a generated `Emakefile` — no `configure` is involved (pure Erlang application).

## Dependencies
- `common_test` (CT + `test_server`: `app_test/1`, `appup_test/1`, `timetrap/1`, and the `ts_install_cth` ct_hook from `lib/common_test/test_server/`).
- `eunit` (assert macros only), `compiler` (`compile:forms/2` with `strong_validation`, `merl:compile_and_load/1`), `stdlib`, `kernel`, `erts` (see `runtime_dependencies` in `../src/syntax_tools.app.src`).

## See Also
- `../CONTEXT.md` — application overview and module list.
- `../src/` — modules under test; `../src/merl_tests.erl` — EUnit merl tests (sibling, read-only).
- `../include/merl.hrl` — include needed to compile `merl_SUITE`.
- `../../common_test/test_server/ts_install_cth.erl` and `../../make/test_target_script.sh` — runner/hook behaviour.
