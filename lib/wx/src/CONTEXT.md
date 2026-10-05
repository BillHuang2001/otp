# wx — Erlang Sources

## Intent
Erlang side of the `wx` application: a 1:1 port of the wxWidgets GUI library to Erlang.
Holds the handcrafted runtime/binding modules plus `gen/`, the 236 machine-generated modules that mirror the wxWidgets/OpenGL C++ class APIs.
All of these are compiled into `../ebin`; the actual C++ wxWidgets code lives in `../c_src/` and the generator in `../api_gen/`.

## API Surface
### Handcrafted modules (edited by hand)
- `wx.erl` — public base-class module. `new/0,1`, `destroy/0`, `get_env/0`, `set_env/1`, `subscribe_events/0`, `batch/1`, `foreach/2`, `map/2`, `foldl/3`, `foldr/3`, `null/0`, `is_null/1`, `equal/2`, `getObjectType/1`, `typeCast/2`, `create_memory/1`, `get_memory_bin/1`, `retain_memory/1`, `release_memory/1`, `debug/1`, `demo/0`, `parent_class/1`.
- `wx_object.erl` — a `gen_server`-like behaviour for "sub-classing" a wx object: one process per object.
  Callbacks: `init/1`, `handle_event/2`, `handle_call/3`, `handle_cast/2`, `handle_info/2`, optional `handle_sync_event/3`, `terminate/2`, `code_change/3`.
  API: `start/3,4`, `start_link/3,4`, `stop/1,3`, `call/2,3`, `send_request/2`, `wait_response/1,2`, `check_response/2`, `cast/2`, `reply/2`, `get_pid/1`, `set_pid/2`.
- `wxe_server.erl` — internal `gen_server` (`-moduledoc false`) created by `wx:new/1`; owns the wx environment, tracks user processes (monitors them), holds the fun/pid callback registry, and dispatches driver event callbacks (`{'_wx_invoke_cb_', ...}`) to Erlang funs or to `wx_object` processes.
- `wxe_master.erl` — registered singleton `gen_server` named `wxe_master`; loads the NIF/driver, sets up constants, runs the debug-ping process and collects macOS app-level messages (`fetch_msgs/0`, `subscribe_events` subscribers).
- `wxe_util.erl` — NIF shim and low-level command layer. `init_nif/1` (loads `wxe_driver`), `priv_dir/2`, `queue_cmd/1..15` (the actual NIFs), `make_env/0`, `delete_env/1`, `get_const/1`, `get_name/2`, `setup_consts/0`, `color/1`, `rec/1` (receives `'_wxe_result_'`/`'_wxe_error_'`), `opt_error_log/3`.
- `wxe.hrl` — private internal header (not installed): `#wx_ref{}`, `#wx_env{}`, `#wx_mem{}`, `#evh{}` records, the `?get_env()`/`?CLASS()` macros and the driver opcode macros (`?BATCH_BEGIN`, `?WXE_CB_START`, `?WXE_REGISTER_OBJECT`, …).
  It ends with `-include("gen/wxe_funcs.hrl")`, which pulls in the generated per-class opcode macros.
- `wx.app.src` — application resource template; `%VSN%` ← `../vsn.mk`'s `VSN`, `%GEN_MODS%` ← the list of generated module names; handcrafted module list (`wx, wx_object, wxe_master, wxe_server, wxe_util`) is written literally.
  `{applications, [stdlib, kernel]}` — wx links against the driver, not against other OTP apps.
- `wx.appup.src` — release-upgrade spec: all versions `restart_application, wx`; `%VSN%` substituted by the Makefile.
- `Makefile` — compiles `ErlMods` (the 5 handcrafted files) and `gen/wx*.erl` + `gen/gl.erl` + `gen/glu.erl` into `../ebin`; adds `-DCAN_BUILD_DRIVER` when configure found wxWidgets; installs `src/`, `src/gen/` and `../include/*.hrl` on release.
  `TERTIARY_BOOTSTRAP` reduces the build target to `wx_object.beam` only.

## Constraints
- Never hand-edit anything under `gen/` (all files carry `%% This file is generated DO NOT EDIT`) or `gen/wxe_funcs.hrl`; regenerate with `../api_gen/` instead.
- Never edit `wx.app`/`wx.appup` in `ebin/` — they are sed-generated from `wx.app.src`/`wx.appup.src`.
- `wxe.hrl` is private; the public headers are `../include/wx.hrl`, `gl.hrl`, `glu.hrl` (installed, used via `-include_lib("wx/include/wx.hrl")`).
- The wx environment is per-process: code outside `wx:new/0` must carry `#wx_env{}` with `get_env/0`/`set_env/1`; helper macros in `wxe.hrl` fail with `{wx,unknown_env}` when unset.
- Adding/removing a handcrafted module requires updating `ErlMods` in `Makefile` **and** the `modules` list in `wx.app.src`.

## Design Decisions
- wxWidgets is object-oriented, so Erlang modules act as classes: `wxCLASS:new(...)`/`wxCLASS:member(Object, ...)`; inheritance is emulated by delegating to `parent_class/1` (`?CLASS`/`?CLASS_T` macros in `wxe.hrl`) and by generated modules re-exporting inherited functions.
- Three layers separate concerns: `wxe_master` (one OS/NIF-level environment and driver), `wxe_server` (Erlang-side session: users, callbacks, event routing) and per-object `wx_object` servers.
- Commands are synchronous NIF calls (`queue_cmd`) whose reply the caller picks up with `wxe_util:rec/1`; batching (`wx:batch/1`) stops the driver from processing events mid-sequence.
- Event callbacks come back *from* the driver as messages to `wxe_server`, which either spawns a fun or hands the event to the owning `wx_object` process (`handle_event/2` async, `handle_sync_event/3` sync).
- A NIF (not a port) is used for speed; `priv_dir/2` locates `wxe_driver.so`/`.dll` in `priv/` or in an arch-specific `priv/<arch>/` subdir.

## Notes for Agents
- `gen/wxe_funcs.hrl` is ~140 KB of generated opcode macros included by `wxe.hrl`; do not read it whole.
- `wxe_server` and `wxe_master` are internal (`-moduledoc false`) — public API is `wx`, `wx_object` and the generated `wx*` class modules.
- The NIF is loaded lazily by `wxe_master:init/1` (first `wx:new/0`), so most tests must start wx first.
- `-moduledoc false` + `ERL_COMPILE_FLAGS` (`+nowarn_missing_doc`, `-DCAN_BUILD_DRIVER`) mean the wx sources compile differently depending on whether the driver can be built.

## Dependencies
- Built only when wxWidgets is available (`config.mk`: `CAN_BUILD_DRIVER`); needs SMP (NIF + emulator), and OpenGL libraries for the `gl`/`glu` bindings.
- Runtime: NIF `wxe_driver` from `../c_src/`, plus `priv/` data files (e.g. `erlang-logo*.png`).

## Test Strategy
- `TEST_NEEDS_RELEASE=true`: suites in `../test/` run against an installed release.
- Suites: `wx_app_SUITE`, `wx_basic_SUITE`, `wx_class_SUITE`, `wx_event_SUITE`, `wx_opengl_SUITE`; `wx_obj_test.erl`/`wx_oc_object.erl` are helpers for `wx_object` tests.

## See Also
- `./gen/` — generated class modules (do not edit).
- `../api_gen/` — generator for `gen/` (sibling, read-only).
- `../c_src/` — the C++ driver/NIF implementation (sibling, read-only).
- `../include/` — public headers `wx.hrl`, `gl.hrl`, `glu.hrl`.
- `../test/`, `../examples/`, `../doc/` — tests, demo applications and documentation sources.

## Routing Table
- `gen/` → 236 generated wxWidgets/OpenGL class modules (do not hand-edit; regenerate via `../api_gen/`)
- `../api_gen/` → generator producing `gen/` (sibling — read-only, escalate writes to `lib/wx`)
- `../c_src/` → C++ wxWidgets driver/NIF (`wxe_driver`, OpenGL) (sibling — read-only)
- `../include/` → public Erlang headers (sibling — read-only)
- `../test/` → Common Test suites for the wx application (sibling — read-only)
