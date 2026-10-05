# reltool — Release Management Tool

## Intent
Reltool analyses an installed Erlang/OTP system (or an `escript`) and computes the application/module dependency graph, then lets you customize and generate a *target system* (rel/script/boot files plus a filtered file tree).
It ships both a batch/API backend (`reltool_server` process) and a wx GUI frontend that visualizes the graph.
Current app version `RELTOOL_VSN = 1.1` (`vsn.mk`).

## API Surface
- `src/reltool.erl` — the only public module; its `-moduledoc` **is** the reference manual (rendered by ExDoc).
  API: `start/0,1`, `start_link/1`, `debug/0` (GUI); `start_server/1`, `get_server/1`, `get_status/1`, `stop/1`; `get_config/1,3`, `get_rel/2`, `get_script/2`; `create_target/2`, `get_target_spec/1`, `eval_target_spec/3`, `install/2`.
- `src/reltool_server.erl` (2247 lines) — backend server process holding the `#sys{}` state; `start_link/0,1`, `get_config`, `load_config`, `save_config`, `reset_config`, `undo_config`, `get_app/get_apps/set_app/set_apps`, `get_sys/set_sys`, `get_mod`, `get_rel/get_script`, `gen_rel_files/gen_target/gen_spec`.
- `src/reltool_target.erl` — filesystem side: `gen_config/gen_app/gen_rel/gen_rel_files/gen_boot/gen_script/gen_spec`, `eval_spec/3`, `gen_target/2`, `install/2` (generates `.rel`/`.script`/`.boot` itself — it does **not** delegate to `systools`).
- `src/reltool_utils.erl` — shared helpers: dir/normalize utilities, `prim_consult`, `split_app_name`, regexp decoding, wx item helpers, `escript_foldl` (uses `escript:extract/2`), `call/cast/reply` used to talk to the server.
- `src/reltool.hrl` — the *data model*: option types (`sys()`, `app()`, `mod()`, `escript()`, `config()`, `options()`, `target_spec()`) and records `#sys{}`, `#app{}`, `#app_info{}`, `#mod{}`, `#rel{}`, `#rel_app{}`, `#regexp{}`; also all `DEFAULT_*` / profile filter (`EMBEDDED_*`, `STANDALONE_*`) macros.
- `src/reltool_fgraph.erl` — generic keyed property graph (`new/add/set/del/is_defined/get/size/foreach/map/foldl/mapfoldl`); `src/reltool_fgraph.hrl` holds its node/edge records and force-directed-layout physics constants (`fg_*`).
- `src/reltool_fgraph_win.erl` — wx rendering of that graph (force-directed layout, double-click handling).
- `src/reltool_sys_win.erl` / `reltool_app_win.erl` / `reltool_mod_win.erl` — the three level GUI tree windows (sys → app → mod), each a `gen_server`-style loop process.
- `bin/reltool.escript` — CLI frontend; actions `--window`, `--create_config`, `--create_rel`, `--create_script`, `--create_target`, `--create_target_spec`, `--eval_target_spec`.

## Constraints
- Configuration is a `{sys, [sys()]}` tuple (or a file containing one); every option is read at sys/app/mod level with app overriding sys, mod overriding app (`app_file`, `incl_cond`, `mod_cond`, filters, `debug_info`).
- Inputs are existing `.app`/`.rel`/`.script` files and `ebin/` dirs; an `AppDir` is only recognized as an application if `AppDir/ebin` exists.
- Version sorting requires version ids of integers and dots only (`1`, `2.0`, `3.17.1`) — non-standard vsn ids sort unpredictably.
- GUI modules require `wx`; `reltool` must remain usable with only the `systools`/batch path when `wx`/`tools`/`sasl` are absent (they are declared `optional_applications`).
- `ebin/` is generated output — do not edit; `reltool.app`/`reltool.appup` are produced from `*.app.src`/`*.appup.src` with `%VSN%` substituted by `src/Makefile`.
- Adding a module requires updating the `MODULES` list in `src/files.mk` (`src/Makefile` derives `ERL_FILES` from it).

## Dependencies
- From `src/reltool.app.src`: `applications: [stdlib, kernel, wx, tools, sasl]`, `optional_applications: [wx, tools, sasl]`, `runtime_dependencies: ["wx-2.3","tools-2.6.14","stdlib-5.0","sasl-4.2.1","kernel-9.0","erts-15.0"]`.
- No C code / no `configure` — pure Erlang application (unlike `crypto`, `wx`, …).

## Test Strategy
- Common Test suites live in `test/`; `test/Makefile` compiles `MODULES` with `-pa $(ERL_TOP)/lib/reltool/ebin` and installs them (plus `*_SUITE_data`) to `$(RELEASE_PATH)/reltool_test`.
- The app is built with `TEST_NEEDS_RELEASE=true` (set in the app `Makefile`), so `make test` runs against an installed release.
- `reltool_server_SUITE` (2896 lines) is the main suite: it synthesizes fake OTP systems under `reltool_server_SUITE_data/<case>/` via `Makefile.src` templates and drives the batch API (create_release/script/target, profiles, escript, unicode, vsn selection, OT regression cases).
- `reltool_app_SUITE` verifies the app's own `.app` file (fields, modules list, app deps, exports, appup) — it is not about reltool's functionality.
- `reltool_wx_SUITE` boots all GUI windows (`start/1`, `reltool_sys_win:open_app`, `reltool_app_win:open_mod`) and only asserts that nothing crashes.
- `reltool_manual_gui_SUITE` requires manual interaction; it is excluded from automated runs via `test/reltool.spec` (`skip_suites`).
- `test/reltool_test_lib.erl` (+ `.hrl`) provides suite helpers and the `?m`/`?msym`/`?ignore`/`?log` macros; `test/rtt.erl` + the `test/rtt` shell script exist for interactive runs in a separate xterm.

## Routing Table
- `src/` → Erlang sources (`reltool.erl` API, `reltool_server` backend, `reltool_target` filesystem generation, GUI windows, `reltool_fgraph*`), headers `reltool.hrl`/`reltool_fgraph.hrl`, `reltool.app.src`/`reltool.appup.src`, `files.mk`, `Makefile`
- `test/` → Common Test suites + `reltool_test_lib`, interactive `rtt`, `reltool.spec`/`reltool.cover`, and per-suite `*_SUITE_data/` fixture systems
- `doc/` → `docs.exs` (ExDoc config, `extras` = the guides), `guides/reltool_intro.md|reltool_usage.md|reltool_examples.md`, `notes.md` (published release notes), `Makefile` (includes `make/doc.mk`)
- `examples/` → example `escript`s `display_args` and `mnesia_core_dump_viewer`; installed into `$(RELSYSDIR)/doc/examples`
- `bin/` → `reltool.escript` CLI frontend (installed as the `reltool` command)
- `ebin/` → build output (only `.gitignore` tracked; ignored)

## Notes for Agents
- `reltool_fgraph.erl` is a *generic* keyed graph utility, not reltool-specific logic — the `fg_*` constants/records in `reltool_fgraph.hrl` are for the GUI's force-directed layout.
- The reference manual seen on the web is generated from `reltool.erl`'s `-moduledoc`; edit docs there, not in `doc/`.
- `reltool_target` reimplements `.rel`/`.script`/`.boot` generation (see its comment about unaddressed `systools` tickets) rather than calling `systools`.
- The GUI is optional at runtime; batch/API paths must keep working without `wx`.

## See Also
- `../sasl/` — release handling / `systools` (complementary release tooling).
- `../wx/` — the GUI toolkit reltool's frontend depends on.
- `../syntax_tools/`, `../tools/` — `tools` is an optional reltool dependency.
- `../CONTEXT.md` — OTP library application layout and build conventions; repo-root `CONTEXT.md` for the top-level build.
