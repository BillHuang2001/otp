# wx — Common Test Suites

## Intent
Common Test suites, helper modules and a standalone GUI test driver for the `wx` application (Erlang bindings to wxWidgets).
Testing GUI apps automatically is hard, so the suites mostly assert that the wx API is *consistent* and that calls/events do not crash (see `Readme`), rather than deep functional behaviour.

## API Surface

### Test suites
All suites declare `suite() -> [{ct_hooks,[ts_install_cth]}, {timetrap,{minutes,N}}]` (`wx_app_SUITE` uses 5 minutes, the rest 2).
- `wx_app_SUITE.erl` — application-metadata checks on the installed `wx.app` (`all/0` = `fields`, `modules`, `exportall`, `app_depend`, `undef_funcs`, `appup`).
  It is the only GUI-independent suite: it does not use `wx_test_lib:init_per_suite/1` but its own `is_app/1`, and `init_per_testcase/2` puts the parsed app file into the Config.
- `wx_basic_SUITE.erl` — baseline behaviour of the `wx`/`wx_object` API.
  `all/0` = `silent_start, create_window, several_apps, wx_api, wx_misc, data_types, enums, memory, wx_object, {group,undef_callbacks}, undef_in_handle_info, undef_in_terminate`; group `undef_callbacks` = `undef_handle_event, undef_handle_call, undef_handle_cast, undef_handle_info, undef_code_change, undef_terminate1, undef_terminate2` (missing-optional-callback error paths).
  `data_types/1` covers argument/return type conversion, `memory/1` retain/release of `wx:create_memory/1`, `enums/1` delegates to `wx_test_enums:test()`.
- `wx_class_SUITE.erl` — per-widget/class usage tests; `all/0` = `calendarCtrl, treeCtrl, notebook, staticBoxSizer, clipboard, helpFrame, htmlWindow, image, listCtrlSort, listCtrlVirtual, radioBox, systemSettings, taskBarIcon, toolbar, popup, modal, textCtrl, locale`.
- `wx_event_SUITE.erl` — event-handling semantics (connect/disconnect options, message vs callback delivery, re-entrancy); `all/0` = `connect, disconnect, disconnect_cb, connect_msg_20, connect_cb_20, mouse_on_grid, spin_event, connect_in_callback, recursive, dialog, char_events, mouse_events, callback_clean, handler_clean`.
- `wx_xtra_SUITE.erl` — regression cases for app/window lifetime and API misuse (extra/edge cases, not extra classes).
  `all/0` = `destroy_app, destroy_app2, multiple_add_in_sizer, app_dies, menu_item_debug`.
- `wx_opengl_SUITE.erl` — OpenGL basics through the `gl` module over `wxGLCanvas`; includes `wx/include/gl.hrl`.
  `all/0` = `canvas, glu_tesselation, debugMessage`; `init_per_suite/1` chains `wx_test_lib:init_per_suite/1` and skips unless `wx_test_lib:user_available/1` is true.

### Support modules
- `wx_test_lib.erl` + `wx_test_lib.hrl` — shared test library: suite/testcase init+end hooks, `wx_test_lib:run_test/2,3` (a small standalone test server used by `wxt`), `user_available/1`, `wx_destroy/2`, `wx_close/2`, `wait_for_close/0`, `flush/0`, `pick_msg/0`.
  `wx_test_lib.hrl` includes `wx/include/wx.hrl` (so `?wx*` macros are available) and defines the assertion macros `?m(Expected,Expr)`, `?mt(Expected,Expr)` (matches result element 3), `?mr(Expected,Expr)` (element 1), `?m_receive/1`, `?m_multi_receive/1`, plus `?log/2`, `?warning/2`, `?error/2`, `?fatal/2`, `?skip/2`.
- `wx_obj_test.erl` — generic `wx_object` test object; forwards everything to its parent as `{event,...}`, `{sync_event,...}`, `{call,...}`, `{cast,...}`, `{info,...}`, `{terminate,...}`, `{code_change,...}`; used by `wx_basic_SUITE` and `wx_event_SUITE`.
- `wx_oc_object.erl` — minimal `wx_object` implementing only `init/1`, used by `wx_basic_SUITE` to verify the error path for absent optional callbacks.
- `wx_test_enums.erl` — 4479-line mechanically-maintained module with a single `test/0` that asserts every `?wxXXX` macro equals `wxe_util:get_const(wxXXX)`; reached only via `wx_basic_SUITE:enums/1`.
- `wxt.erl` — standalone driver usable from a plain erl shell: `t/0,1,2` (suite aliases: `basic` → `wx_basic_SUITE`; `all` = every `wx_*_SUITE.erl` except `wx_app_SUITE`), `user/0,1,2` = same tests with `{user,step}`.
- `wxt` — `/bin/sh` wrapper that opens an xterm running `erl -sname test_server -pa ../ebin -wx_test_verbose true -wx_test_timeout` and tails `test_log_*` (needs `$DISPLAY` and `xterm`).

### Test configuration
- `wx.spec` — spec for the OTP in-house `ts` test tool: `{suites,"../wx_test",all}`.
- `wx.cover` — coverage spec: `{incl_app,wx,details}`.
- `Makefile` — compiles `Mods` in place (`EBIN = .`, `-pa ../ebin`); `release_tests_spec` installs sources, beams, `wx.spec`, `wx.cover`, `wx_test_lib.hrl` and the `wxt` script into `$(RELEASE_PATH)/wx_test`.
- `Readme` — the three supported run modes (direct erl shell, Common Test, in-house `ts` tool) and the `wxt` user modes.

## Constraints
- Tests run against an *installed release*: `../Makefile` sets `TEST_NEEDS_RELEASE=true`, so `make test` first releases OTP and then uses `$(ERL_TOP)/release/bin/ct_run` against the released `wx_test` dir.
- Every suite except `wx_app_SUITE` needs a working GUI/display (X11/Wayland/Quartz); `wx_test_lib:init_per_suite/1` calls `wx:new()` and skips the whole suite when that fails.
- On macOS the suites are always skipped: `wx_test_lib:init_per_suite/1` exits with `"Can not test on MacOSX"`, which its own `catch` turns into `{skipped, ...}`; wx must be verified manually there.
- `wx_opengl_SUITE` additionally needs OpenGL and only runs when the CT config contains `user` (any value) so that `wx_test_lib:user_available/1` is true.
- The active display can be overridden with CT config `{xserver, Host}` (`ct:get_config(xserver, none)` in `init_per_suite/1`); `{user,true}` enables interaction-tolerant timings, `{user,step}` waits for the user to close each window.
- `wx_app_SUITE` is deliberately excluded from `wxt:t(all, ...)` because it checks the app metadata rather than driving wx.

## Test Strategy
- `make test` from `lib/wx` (releases OTP, then `ct_run`); restrict with standard `ct_run` args passed via `ARGS=`.
- Direct shell mode: `./wxt` and then `wxt:t()`, `wxt:t(basic)`, `wxt:t(basic, create_window)`, `wxt:t(all, [{user,true}])` (also run the manual tests) or `wxt:t(all, [{user,step}])` (step through every window by hand).
- Third mode: the in-house `ts` tool using `wx.spec`.
- Headless/CI expectation: all GUI suites skip with `{skipped,"Start wx failed: ..."}`; only `wx_app_SUITE` can pass without a display. The GitHub Actions workflows only build wx against wxWidgets — they do not run these suites.
- Assertions go through the `?m`-family macros; a mismatch is reported via `wx_test_lib:error/4` (global logger process `wx_global_logger`, error funnel `wx_test_case_sup`) and recorded, so one test case can report several deviations before failing.

## Known Issues
- Timing-sensitive: many cases use fixed `timer:sleep/1` and `receive ... after` waits plus `wx_test_lib:flush/0`, so they are flaky on slow or heavily loaded machines.
- `wx_xtra_SUITE:app_dies/1` is documented in-code as hanging when run automatically on Windows, so it is ordered last.
- `wx_xtra_SUITE:multiple_add_in_sizer/1` intentionally reproduces a user error (adding the same window to a sizer twice) that used to crash the BEAM at destroy time.
- `wx_test_lib:init_per_suite/1` reads `DISPLAY` and the `xserver` CT config, but individual suites `wx:new()` again; a broken/half-working display can make isolated cases fail instead of skipping cleanly.
- Suites create top-level windows, so they must not be run concurrently against the same display.

## Notes for Agents
- `wx_test_enums.erl` is huge but checked in as normal source (listed in the Makefile `Mods`); no generator in `lib/wx/api_gen/` produces it — edit it minimally, never reformat wholesale.
- `wxt` runs `erl` with `-pa ../ebin`, so it must be invoked from the test directory next to `wx/ebin`; the released copy runs from `$(RELEASE_PATH)/wx_test`.
- `wxt:t/1,2` writes `wx_test_case_info` in the CWD to remember the last test case — a leftover file changes what a later `wxt:t/0` runs.
- Compiled `*.beam` land in this directory (`EBIN = .`) but are ignored by the root `.gitignore` (`*.beam`); `test_log_*` and `wx_test_case_info` are ignored by `../.gitignore`.

## Routing Table
This directory has no subdirectories; the whole node is leaf-level.
- `wx_app_SUITE.erl` → application-metadata checks; the only display-independent (and CI-friendly) suite
- `wx_basic_SUITE.erl` → baseline `wx`/`wx_object` API behaviour, incl. enum and type-conversion checks
- `wx_class_SUITE.erl` → per-widget/class usage tests
- `wx_event_SUITE.erl` → event connect/disconnect/callback semantics
- `wx_xtra_SUITE.erl` → lifetime/misuse regression cases
- `wx_opengl_SUITE.erl` → OpenGL (`gl` module) checks; needs OpenGL + `{user,_}` config
- `wx_test_lib.erl` / `wx_test_lib.hrl` → shared hooks, assertion macros and the standalone test server
- `wxt.erl` / `wxt` → standalone shell driver for running suites directly
- `wx_obj_test.erl`, `wx_oc_object.erl` → helper `wx_object` implementations used by the basic/event suites
- `wx_test_enums.erl` → `?wxXXX` macro ↔ `wxe_util:get_const/1` assertions
- `Makefile`, `wx.spec`, `wx.cover`, `Readme` → build/release of the tests, `ts` spec, coverage spec and usage documentation

## See Also
- `../CONTEXT.md` — the `wx` application itself (sources, `api_gen/`, `c_src/`); `../Makefile` sets `TEST_NEEDS_RELEASE=true`.
- `../../common_test/test_server/` — the in-house `ts` tool and the `ts_install_cth` hook used by every suite here.
- `../../HOWTO/TESTING.md` — general OTP testing/releasing workflow.
- `../../make/app_targets.mk` + `../../make/test_target_script.sh` — the `make test` driver (release-then-`ct_run` when `TEST_NEEDS_RELEASE=true`).
