# observer — GUI System Observation Tool

## Intent
The `observer` OTP application: a wxWidgets-based GUI for observing an Erlang system (system info, application supervisor trees, processes, ETS/Mnesia tables, ports, sockets, allocators) plus a front end for Erlang tracing.
It also ships three related tools: `ttb` (Trace Tool Builder, a library for building trace tools), `etop` (a UNIX-`top`-like text tool for process info) and `crashdump_viewer` (a wxWidgets browser for Erlang crash dumps).
Current version: `OBSERVER_VSN = 2.19` (`vsn.mk`).

## API Surface
- `observer` — the documented public API: `start/0`, `start/1` (connect to a node), `start_and_wait/0`, `start_and_wait/1`, `stop/0`. Delegates to `observer_wx`.
- `ttb` — documented Trace Tool Builder API (`tracer/0..2`, `tp/tpl/tpe`, `ctp/ctpl/ctpg/ctpe`, `write_config/run_config/list_config`, `format`, `seq_trigger_ms`, `stop`, …). Works with `dbg`/`runtime_tools` trace facilities.
- `etop` — documented; started via the `priv/bin/etop` script (hidden node, `-s etop`); `etop_txt` formats the text output, `etop_tr` handles distributed tracing.
- `crashdump_viewer` — documented; started via the `priv/bin/cdv` script; GUI modules are the `cdv_*` family.
- `multitrace` — example tool (an `EXAMPLE_FILES` entry, NOT in the app's `modules`); installed to `doc/examples`.
- Observer GUI panels (all `wx_object` behaviours, internal): `observer_wx` (main frame), `observer_app_wx` (app/supervisor tree), `observer_sys_wx` (system info), `observer_perf_wx`, `observer_alloc_wx`, `observer_pro_wx` (process list), `observer_procinfo`, `observer_port_wx`, `observer_sock_wx`, `observer_trace_wx` + `observer_traceoptions_wx` (trace front end), `observer_tv_wx` + `observer_tv_table` (table viewer).
- `observer_lib` / `observer_html_lib` — shared GUI helper modules (dialogs, menus, progress, HTML windows).
- Crashdump viewer modules: `crashdump_viewer` (server/parser) + `cdv_wx` (main GUI) + `cdv_*_cb` (per-tag detail callbacks) + `cdv_detail_wx`, `cdv_html_wx`, `cdv_multi_wx`, `cdv_table_wx`, `cdv_virtual_list_wx`, `cdv_info_wx`.
- `ttb_et` — bridge from `ttb` traces to the `et` event tracer (its `et.hrl` header comes from `../../et/include`).

## Dependencies
- `observer.app.src`: applications `[kernel, stdlib, runtime_tools, et]`; optional applications `[wx, runtime_tools, et]`.
- runtime deps: `wx-2.3`, `stdlib-5.0`, `runtime_tools-2.1`, `kernel-10.0`, `et-1.5`, `erts-15.0`.
- The GUI needs a display (X11 `DISPLAY` on Unix) and a build with `wx`; `runtime_tools` provides the `observer_backend` server-side data collection (re-exported via `include/etop.hrl`).

## Constraints
- The GUI works only where `wx` is compiled and a display/server is available; on headless or `wx`-less builds only the library parts (`ttb`, `etop` text mode) are usable.
- `src/Makefile` is the authoritative module list; adding/removing a module requires editing `MODULES` there and `modules` in `observer.app.src`.
- `observer.app` and `observer.appup` in `ebin/` are generated from the `*.src` templates (`%VSN%` substituted with `OBSERVER_VSN`).
- Internal headers (`observer_tv.hrl`, `observer_defs.hrl`, `crashdump_viewer.hrl`, `etop_defs.hrl`) live in `src/`; only `etop.hrl` is public (`include/`).
- `ERL_COMPILE_FLAGS` include `+nowarn_deprecated_catch` and `-I ../../et/include`; the app is compiled against `lib/wx/ebin`.

## Test Strategy
- Common Test suites: `observer_SUITE`, `crashdump_viewer_SUITE`, `etop_SUITE`, `ttb_SUITE`; helper modules `client`, `server`, `ttb_helper`, `crashdump_helper`, `crashdump_helper_unicode`, `observer_socket_test`.
- GUI tests (`observer_SUITE` `gui` group, `crashdump_viewer_SUITE`) require a `DISPLAY`/wx; they `init_per_group`-skip with a reason when `wx` is missing or `wx:new/0` fails.
- `crashdump_viewer_SUITE` further skips on MacOSX and SunOS, and exits when `ERL_CRASH_DUMP_PUBLIC_KEY` is set (no encrypted-crashdump support), using `?failed_file` for known-fail tracking.
- `etop_SUITE`/`ttb_SUITE` are node/trace based (start hidden nodes, capture output) and need no display.
- `observer.spec` (`{suites,"../observer_test",all}.`) and `observer.cover` select suites/coverage; `MODULES` in `test/Makefile` also pulls in `kernel/test/shell_test_lib`.
- `test/crashdump_viewer_SUITE_data/` holds sample dumps (e.g. `old_format.dump`).

## Routing Table
| Path | Contents |
| --- | --- |
| `src/` | All Erlang sources, internal `.hrl` headers, `observer.app.src`/`observer.appup.src`, and the authoritative `MODULES`/build flags list (`src/Makefile`) |
| `include/` | Public header — only `etop.hrl` (re-exports `runtime_tools`' `observer_backend.hrl`) |
| `test/` | Common Test suites, helpers, `observer.spec`/`observer.cover`, `crashdump_viewer_SUITE_data/` (sample dumps) |
| `doc/` | Documentation: `docs.exs` (ExDoc config), `guides/` (introduction, observer, ttb, etop, crashdump user guides), `references/cdv_cmd.md`, `observer_app.md`, `notes.md`, `assets/` |
| `priv/` | Runtime assets: `bin/` launch scripts (`etop`, `etop.bat`, `cdv`, `cdv.bat`) and `erlang_observer.png` (+ `.license`) |
| `ebin/` | Build output (only `.gitignore` tracked) — do not edit |
| `vsn.mk` | Defines `OBSERVER_VSN`; `Makefile`/`src/Makefile`/`doc/Makefile` include it |
| `Makefile` | App driver; `SUB_DIRECTORIES = src doc`, `DIA_PLT_APPS = runtime_tools et wx` |

## See Also
- `../wx/` — wxWidgets bindings the GUI is built on; `../runtime_tools/` — `observer_backend` data-collection helpers; `../et/` — event tracer used by `ttb_et`; `../tools/` — related dev tooling.
- `../CONTEXT.md` — the enclosing OTP library applications overview.
