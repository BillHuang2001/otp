# et — Event Tracer

## Intent
`et` is the Event Tracer application: it collects built-in Erlang trace events (`erlang:trace/*`, `dbg`) and presents them as a graphical sequence chart (message/action flow between actors).
Events can be reported explicitly by user code (via `et:trace_me`/`phone_home`/`report_event`) or collected from Erlang trace ports or event files.
The application is layered: an API module (`et`), a distributed event store (`et_collector`), a trace-pattern/filter helper (`et_selector`) and wx-based viewers (`et_viewer` + private wx modules).

## API Surface
- `src/et.erl` — main user API: `trace_me/4,5`, `phone_home/4,5`, `report_event/4,5` (thin wrappers around `et_collector:report*`; `/4` takes a combined `FromTo` actor, `/5` separate `From`/`To`). Types `actor()`, `level()` (0..100 detail level).
- `src/et_collector.erl` — `gen_server` plus the event store; the app's only registered process (name `et_collector`). Public API includes `start_link/1`, `stop/1`, `report/2`, `report_event/5,6`, `iterate/3,5`, `lookup/2`, `start_trace_client/3`, `start_trace_port/1`, `save_event_file/3`, `clear_table/1`, `get_global_pid/0`, `get_table_size/1`, `change_pattern/2`, `make_key/2`, and ETS-dict helpers (`dict_insert/3`, `dict_delete/2`, `dict_lookup/2`, `dict_match/2`, `multicast/2`). Internal export `monitor_trace_port/2`. Rich `option/0` set (e.g. `trace_global`, `trace_port`, `trace_max_queue`, `event_order`, `parent_pid`, `file`, `dict`).
- `src/et_selector.erl` — defines event transforms and trace patterns: `make_pattern/1` (builds an `erlang:trace_pattern/3` match spec from a detail level `min|max|0..100`), `change_pattern/1`, `parse_event/2`.
- `src/et_viewer.erl` — viewer entry point used by the GUI/observer: `start/0,1,2`, `start_link/1,2`, `file/1`, `open_event/2`, `stop/1`, `get_collector_pid/1`. Alias `collector_pid` options include `default`, `GUI`, `extended_pid()`.
- `src/et_wx_viewer.erl` — the wxWidgets sequence-chart window (`gen_server`, `moduledoc false`); large module holding the GUI, menus, filter menu and event layout.
- `src/et_wx_contents_viewer.erl` — a `wx_object` window showing the contents of a single event (`start_link/1`, `stop/1`); private.
- `include/et.hrl` — public `#event{}` record (`detail_level`, `trace_ts`, `event_ts`, `from`, `to`, `label`, `contents`); exported API header.
- `src/et.app.src` → `et.app` (built), `src/et.appup.src` → `et.appup` (`restart_application et` for every version).
- `vsn.mk` — `ET_VSN = 1.8`.
- `Makefile` / `subdirs.mk` — top build; `SUB_DIRECTORIES = src examples doc`; `DIA_PLT_APPS = runtime_tools wx`.

## Constraints
- `et_wx_viewer` / `et_wx_contents_viewer` require the `wx` application and a display; without wx or a GUI the viewer cannot start (module still compiles).
- Event collection of built-in traces relies on `runtime_tools` (`dbg`) and the `erlang:trace*` BIFs; `et` is a thin layer over them.
- `et_collector` is globally startable: with `{trace_global, true}` it registers as `{global, et_collector}` for distributed collection; `get_global_pid/0` returns that process.
- Events are stored in two `ordered_set` public ETS tables created by the collector (`et_events`, `et_dict`) — ordering and key layout are load-bearing for `iterate`/`lookup`/`make_key`.
- `src/ebin` is `../ebin` and the release installs to `$(RELEASE_PATH)/lib/et-$(ET_VSN)`; build artifacts under `ebin/` are never edited by hand.
- `runtime_dependencies` pin `wx-1.2`, `stdlib-3.4`, `runtime_tools-1.10`, `kernel-5.3`, `erts-9.0`.

## Dependencies
- Required applications (`et.app.src` `applications`): `stdlib`, `kernel`, `runtime_tools`. Optional: `wx`, `runtime_tools`.
- `wx` is needed at runtime for the GUI viewers; `runtime_tools` for trace collection of remote/global traces.

## Test Strategy
- Common Test suites in `test/`: `et_SUITE` (`app`, `appup` — verifies the `.app`/`.appup` files), `et_wx_SUITE` (`start_all_windows` — starts viewer + opens event windows and checks processes stay alive).
- GUI tests self-skip: `et_test_lib:wx_init_per_suite/1` skips on macOS, when no wx is compiled for the platform, or when `wx:start` fails (uses `DISPLAY`); a real display is required to actually run them.
- `test/et_test_lib.erl` + `et_test_lib.hrl` — shared helpers/macros (`?msym`, `?m`) for the suites.
- `test/ett.erl` + `test/ett` shell script — manual Erlang-shell test runner (`ett:t()`, `ett:t(all, [{user,true}])` for the interactive tests); reads `test/README`.
- `test/et.cover` / `test/et.spec` — coverage config and in-house test spec; `test/Makefile` installs the test system to `$(RELEASE_PATH)/et_test`.

## Notes for Agents
- `et_wx_viewer.erl` (~2.1k lines) is by far the largest module and is where GUI behaviour lives; `et_wx_contents_viewer` is the per-event popup.
- `doc/notes.md` exists but is not referenced by `doc/docs.exs` (which only lists the `guides/*.md` extras) — treat it as legacy/unused.
- `doc/guides/et_intro.md` states the app needs R13BB+ (the wx GUI); the old GS-based GUI is historical only.

## Routing Table
- `src/` → Erlang sources: API (`et`), collector (`et_collector`), selector (`et_selector`), viewers (`et_viewer`, `et_wx_viewer`, `et_wx_contents_viewer`), `et.app.src`/`et.appup.src`, `modules.mk`
- `include/` → public `et.hrl` (`#event{}` record); internal `et_internal.hrl` lives in `src/`
- `test/` → Common Test suites (`et_SUITE`, `et_wx_SUITE`), `et_test_lib`, manual `ett` runner, `et.cover`/`et.spec`
- `doc/` → ExDoc build (`docs.exs`, `Makefile`) + user guides (`guides/*.md`) + `assets/` images + `notes.md`
- `examples/` → sample programs (`et_demo`, `et_display_demo`, `et_trace_demo`), installed to `$(RELSYSDIR)/doc/examples`
- `priv/` → runtime data dir (currently empty, only `.gitignore`)
- `ebin/` → build output (git-ignored)

## See Also
- `../runtime_tools/` — `dbg`/trace helpers used by the collector.
- `../wx/` — GUI toolkit the viewers depend on.
- `../observer/` — GUI system observer that can launch `et_viewer`.
- `../CONTEXT.md` — app-level conventions for all `lib/<app>/` directories.
