# edoc — Common Test Suites

## Intent
Common Test suites for the `edoc` application (EDoc, the Erlang documentation engine).
Two suites: `edoc_SUITE` (general EDoc end-to-end doc generation) and `eep48_SUITE` (EEP-48 doc-chunk generation validated through `shell_docs`).
All suites are pure Erlang/CT; there is no C code or OS-specific configure here.

## API Surface
- `edoc_SUITE.erl` — general EDoc tests. `all/0` = `[app,appup,build_std,build_map_module,otp_12008,build_app,otp_14285,infer_module_app_test,module_with_feature,module_with_nominal]`.
  - `app`/`appup` use `test_server:app_test/1` / `appup_test/1` to validate the `.app`/`.appup` metadata.
  - `build_std` drives `edoc:application/2,3` over the real `edoc` and `syntax_tools` apps into `priv_dir`, with `{preprocess,true}`, `{overview, ...}`, and `{def,{vsn,...}}` (both static value and a fun macro handler).
  - `build_map_module`, `otp_12008`, `otp_14285`, `module_with_feature`, `module_with_nominal` call `edoc:file/2` / `edoc:files/2` on `edoc_SUITE_data/*.erl`; most cases run both the default `epp_dodger` path and the `{preprocess,true}` (epp) path.
  - `build_app` calls `edoc:application(myapp, Src, ...)` on the nested-source tree in `edoc_SUITE_data/myapp/` and asserts `{subpackages,false}` hides nested-module pages.
  - `infer_module_app_test` walks `code:all_available()` and checks `edoc_lib:infer_module_app/1` returns `no_app` or an app whose ebin actually holds the BEAM.
- `eep48_SUITE.erl` — ~44 test cases in `all/0` validating EEP-48 `#docs_v1{}` chunks (`-include_lib("kernel/include/eep48.hrl")`).
  - `edoc_app_should_pass_shell_docs_validation` generates chunks for the whole `edoc` app and runs `shell_docs:validate/1` on every module.
  - Annotation cases (`module_anno`, `function_anno`, `type_anno`, `cb_anno`) check the `{file,Location}` anno of chunk entries.
  - Tag cases (`function_since_tag`, `function_deprecated_tag`, `type_*`, `cb_*`), `links`, `see_tags_grouped_together`, `equiv`.
  - Signature/spec cases (`f_sig_*`, `f_spec_*`, `f_redundant_spec`, `f_only_attr`, `f_only_tag`, `f_prefixed_spec`, `t_only_attr`, `t_only_tag`, `t_redundant`) compare the pretty-printed `signature` metadata against expected `-spec`/`-type` text.
  - `t_visibility` checks `{private,true}` / `{hidden,true}` option combinations.
  - Helpers at the bottom of the file: `get_chunk`/`get_docs` (generate chunks to `priv_dir` via `edoc:files` with `edoc_doclet_chunks`+`edoc_layout_chunks`, then read `<Mod>.chunk`), `get_sig`, `get_pp_spec`, `get_flat_doc` (uses `xmerl_lib`), `get_doc_link`, `lookup_entry`, `copy_application`.
- `Makefile` — builds the suites `MODULES= edoc_SUITE eep48_SUITE` into `.` (`EBIN = .`); adds `+nowarn_missing_spec_documented` and removes `+deterministic`; `RELSYSDIR = $(RELEASE_PATH)/edoc_test`; `release_tests_spec` installs the `.erl`s, `edoc.spec`, `edoc.cover` and untars `*_SUITE_data` into the release.
- `edoc.spec` — Common Test spec `{suites,"../edoc_test",all}` used when running from an installed release.
- `edoc.cover` — cover config `{incl_app,edoc,details}`.
- `test_libs.sh` — developer helper that runs `edoc [-chunks] -app <APP>` over every `$ERL_TOP/lib` app (skips Makefile), collecting `-I` include dirs from all `*.hrl` outside `test`/`ct_run`/`SUITE_data`; overridable via `EDOC`, `CHUNKS`, `LIB_DIR`, `APPS`, `VERBOSE`.

## Test Strategy
- Run via `make test` in `lib/edoc/` (delegates to `make/test_target_script.sh`), which builds this dir and runs Common Test over `lib/edoc/test`. Suites also runnable directly with `ct_run`; `edoc.spec` supports release-based runs from `$(RELEASE_PATH)/edoc_test`.
- Tests are file-system based, not unit-level: they generate docs into `priv_dir` and assert on generated HTML/chunk files or on EDoc return values.
- `build_std` and `edoc_app_should_pass_shell_docs_validation` need a full working `edoc` (and `syntax_tools`) build available via `code:lib_dir/1`; `edoc_SUITE` `build_std` additionally reads edoc/syntax_tools sources through `edoc:application/2`.
- `eep48_SUITE` requires `kernel`'s `eep48.hrl`, `shell_docs`, `xmerl` (`xmerl_lib` for `flatten_doc`) and `eunit` headers at runtime.
- `edoc_app_should_pass_shell_docs_validation` skips when cover-compiled (`init_per_testcase` checks `test_server:is_cover/0`) because cover instrumentation changes the copied app; it copies edoc's `ebin`/`include`/`src` into `priv_dir` and prepends that ebin via `code:add_patha/1`.
- Case ordering/behavior depends on `epp_dodger` vs `{preprocess,true}` (epp); several cases assert the `epp_dodger` path fails (`{'EXIT',error}`) on unparseable-with-dodger input.
- No explicit timers/ports; suites are fast and self-contained (no external services).

## Known Issues
- `module_with_maybe/1` is exported but **not listed in `edoc_SUITE:all/0`**, so it is never run; the `maybe`-expression fixture `edoc_SUITE_data/module_with_maybe.erl` is effectively dead test data.
- `not_supported/0` in `eep48_SUITE` lists `type_since_tag`, `type_deprecated_tag`, `cb_since_tag`, `cb_deprecated_tag`; `init_per_testcase` skips them with `"not supported"` because EDoc does not yet extract those tags (TODO).
- Several `f_spec_*` expectations acknowledge EDoc bugs in the asserted text (param-name inference across multi-clause specs); the expected values encode the current buggy output, so fixing EDoc will require updating these assertions.
- Some `edoc_SUITE_data/*.erl` fixtures contain non-UTF-8 / latin-1 bytes (`un1`/`un2`/`un3`/`un_atom*`); handle them as binary/`iconv` when reading.

## Notes for Agents
- `EBIN = .` in this dir: suite beams are compiled in place, and `edoc.cover`/`edoc.spec` are installed into the release `edoc_test` dir.
- Test data is split by suite: `edoc_SUITE_data/` and `eep48_SUITE_data/` (in the release these are untarred alongside the suite sources).
- `edoc_SUITE_data/overview.edoc.license` is the license sidecar for the non-`.erl` fixture `overview.edoc` (license scanner coverage), not test input.

## Constraints
- Fixtures must remain valid for the Erlang parser being tested; expected spec/signature strings in `eep48_SUITE` are exact `erl_pp` output and must match byte-for-byte.
- Adding a test case requires exporting it **and** listing it in `all/0` (see the `module_with_maybe` gotcha).
- Do not rename suite data dirs: `release_tests_spec` untars `*_SUITE_data` and suites resolve them via `?config(data_dir, ...)`.

## Routing Table
- `edoc_SUITE.erl` → end-to-end EDoc doc-generation tests (app/appup metadata, application- and file-level doc builds, module-app inference).
- `eep48_SUITE.erl` → EEP-48 chunk generation tests validated via `shell_docs`/`#docs_v1{}` (annotations, tags, links, specs, visibility).
- `edoc_SUITE_data/` → fixtures for `edoc_SUITE`: `un1/un2/un3.erl`, `un_atom1/un_atom2.erl`, `map_module.erl`, `module_with_feature.erl`, `module_with_maybe.erl`, `module_with_nominal.erl`, `overview.edoc`(+`.license`), `overview.syntax_tools`, and `myapp/` (nested-source sample app: `src/a.erl`, `src/src_1/b.erl`, `doc/.dummy`).
- `eep48_SUITE_data/` → EEP-48 fixtures: `eep48_meta.erl` (since/deprecated tags), `eep48_links.erl` (see*/equiv links), `eep48_sigs.erl` (signature inference), `eep48_specs.erl` (spec pretty-printing), `eep48_redundant.erl` (attr-vs-tag conflicts), `eep48_visibility.erl` (private/hidden/non-exported).
- `Makefile` → suite build + release_tests_spec install rules for `edoc_test`.
- `edoc.spec`, `edoc.cover` → CT spec file and cover config.
- `test_libs.sh` → helper script to batch-run `edoc` over all `lib/` apps (development aid, not part of `make test`).

## Dependencies
- Runtime: `common_test`, `kernel` (`eep48.hrl`), `stdlib` (`shell_docs`), `xmerl` (`xmerl_lib`), `eunit` (assert macros), plus the `edoc` and `syntax_tools` apps built and on the code path.

## See Also
- `../` (lib/edoc) — the application under test: `src/` (incl. `edoc_doclet_chunks`, `edoc_layout_chunks`), `doc/`, `include/`.
- `../../lib/syntax_tools/` — used as fixture input by `build_std` (`overview.syntax_tools`).
- `../../system/` and `lib/edoc/doc/` — documentation build that consumes EDoc output.
