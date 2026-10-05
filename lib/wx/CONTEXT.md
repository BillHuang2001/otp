# wx — Erlang/Elixir Bindings to wxWidgets

## Intent
OTP application `wx` (app version `WX_VSN = 2.7` in `vsn.mk`): a wrapper that exposes the C++ wxWidgets GUI toolkit to Erlang/Elixir as a near 1:1 port.
One Erlang module per wxWidgets class; the runtime is a NIF driver (`wxe_driver`) plus a separate OpenGL NIF (`erl_gl`).
Most of the source is **generated**: an `api_gen/` generator emits the `wx*` class modules, public headers and C++ glue from doxygen XML + a hand-maintained spec.
Built only when a system wxWidgets (with GL support) is available; the app is one of the "extra" apps built under `BUILD_ALL` (see `../Makefile`).

## API Surface
- Public Erlang API:
  - `wx` module (`src/wx.erl`) — base class + lifecycle (`new/0`, `destroy/0`, `get_env/0`, `set_env/1`, `batch/1`, memory and type helpers, `debug/1`).
  - `wx-*-` class families (`src/gen/wx*.erl`, 236 generated modules, plus `gl.erl`/`glu.erl`) — each `wxCLASS` module maps to a wxWidgets class (`wxFrame`, `wxButton`, `wxListCtrl`, ...); `wx_misc` holds global/classless functions.
  - `wx_object` behaviour (`src/wx_object.erl`) — gen_server-like "sub-classing" of wx objects, one process per object (`init/1`, `handle_event/2`, `handle_call/3`, ...).
- Public installed headers (`include/`): `wx.hrl` (the `#wx{}` event record + all event records/types), `gl.hrl`, `glu.hrl` — all generated, used via `-include_lib("wx/include/wx.hrl")`.
- App resources: `src/wx.app.src` (vsn `%VSN%`, module list `%GEN_MODS%` + handcrafted modules, `{applications,[stdlib,kernel]}`) and `src/wx.appup.src` (all versions `restart_application, wx`).
- Build entry points: `Makefile` (subdirs `src`, `c_src`, `examples`, `doc`), `configure`/`configure.ac` (own autoconf, generates `config.mk` and `c_src/Makefile`), `config.mk.in`.

## wxWidgets Version Handling
- The app's own version is `WX_VSN` (`vsn.mk`, currently `2.7`); the bound wxWidgets library version is whatever system wxWidgets `configure` finds (`wx-config`, `WX_MAJOR_VERSION` constants) — there is no single pinned upstream version.
- `configure.ac` accepts `--with-wxdir=PATH` and `--without-wx`; it sets `WXERL_CAN_BUILD_DRIVER` (`config.mk`), which gates whether `c_src` is built at all.
- Generated code records the upstream versions it came from: `src/gen/vendor.info` (wxWidgets/OpenGL doc licenses + commit SHA) and `src/gen/gl.erl`/`glu.erl` (`GL_GLEXT_VERSION`).

## Constraints
- **Never hand-edit generated files**: `src/gen/*`, `include/*.hrl`, `c_src/gen/*` are all checked in with `DO NOT EDIT` markers and produced by `api_gen/` — change `api_gen/wxapi.conf` / `glapi.conf` / `wx_extra/` and regenerate instead.
- `src/wxe.hrl` is a private internal header (not installed); the public headers live in `include/`.
- Requires a system wxWidgets build with GL support and OpenGL/GLU headers/libs to link `c_src`; the Erlang side compiles regardless, but the NIF is only usable when `CAN_BUILD_DRIVER` is true.
- The wx environment is per-process: code outside `wx:new/0` must be passed the `#wx_env{}` (`get_env/0`/`set_env/1`).
- Adding/removing a handcrafted module requires updating both `src/Makefile` (`ErlMods`) and the `modules` list in `src/wx.app.src`.

## Dependencies
- System: wxWidgets (with GL) + OpenGL/GLU; C++ compiler (and Objective-C compiler on Darwin); autoconf `configure` step run by the top-level build.
- OTP app deps (`wx.app.src`): `stdlib`, `kernel` only. Tests additionally need `common_test` and an installed release.
- Code generation (developer-only, not part of a normal build): doxygen 1.8.17, an Ericsson-patched wxWidgets checkout (`wx_api_gen` branch), OpenGL headers, Khronos OpenGL-Refpages.

## Design Decisions
- Erlang modules act as classes because wxWidgets is object-oriented: `wxCLASS:new(...)` / `wxCLASS:member(Object, ...)`; inheritance is emulated via `parent_class/1` and generated re-exports.
- Three runtime layers: `wxe_master` (registered singleton: one OS/NIF environment + driver), `wxe_server` (per-session: user processes, callback registry, event routing), and per-object `wx_object` servers.
- A NIF (not a port) is used for speed; commands are synchronous `queue_cmd` NIF calls with `wx:batch/1` to suppress mid-sequence event processing, and callbacks re-enter as messages to `wxe_server`.
- The full binding is **spec-driven code generation** from doxygen XML so the huge wxWidgets surface can be curated and kept diff-stable; two parallel suites (wx classes, OpenGL functions) share `gen_util`.
- The driver and the OpenGL NIF are split: the driver does not link GL; `erl_gl` supplies GL function pointers via `wxe_util:init_opengl/2`.

## Notes for Agents
- The `wx*` class modules and `include/*.hrl` are generated — do not edit them; see `api_gen/CONTEXT.md` for the regeneration pipeline.
- `src/gen/wxe_funcs.hrl` (~140 KB of opcode macros) is included by `src/wxe.hrl`; never read it whole.
- `api_gen` is listed only in `CLEANDIRS` of `Makefile` — the normal OTP build never runs generation.
- `prebuild.skip` marks `config.mk`, `c_src/Makefile` and `ebin/gl.beam` as non-prebuild artifacts during bootstrap.

## Test Strategy
- `TEST_NEEDS_RELEASE=true`: `make test` in `lib/wx` releases OTP and then runs Common Test (`ct_run`) against `$(RELEASE_PATH)/wx_test`; suites live in `test/`.
- Most suites need a GUI/display; `test/wx_app_SUITE` is the only display-independent suite. All GUI suites skip cleanly (not fail) without a display, and are always skipped on macOS.
- `wx_opengl_SUITE` additionally needs OpenGL and runs only with a `user` CT config key.
- Direct shell runs use the `test/wxt` driver (`wxt:t/...`, `wxt:user/...`).
- CI (`.github/`) only builds wx against wxWidgets — it does not run these suites.

## Routing Table
| Path | Description |
| --- | --- |
| `src/` | Handcrafted Erlang runtime/binding modules (`wx`, `wx_object`, `wxe_master`, `wxe_server`, `wxe_util`) + `wx.app.src`/`wx.appup.src`; contains `src/gen/` (236 generated class modules) |
| `api_gen/` | Developer-only code generator (doxygen XML + `.conf` spec → Erlang/C++/headers); `wx_extra/` holds hand-written overrides merged into generation |
| `c_src/` | Hand-written C/C++ NIF driver glue (`wxe_driver`, `erl_gl`); contains `c_src/gen/` (generated NIF glue) |
| `include/` | Public installed Erlang headers `wx.hrl`, `gl.hrl`, `glu.hrl` (all generated) |
| `test/` | Common Test suites, helper modules and the `wxt` standalone driver |
| `doc/` | Documentation build (`docs.exs`, `Makefile`), guide sources (`guides/chapter.md`), release notes (`notes.md`); `overview.edoc` is dead/unused |
| `examples/` | Demo/sample apps (`demo/`, `simple/`, `sudoku/`) built as part of the normal (non-bootstrap) build |
| `priv/` | Runtime assets — `erlang-logo{32,64,128}.png` (with `.license` sidecars) |

## See Also
- `../CONTEXT.md` — OTP application conventions (standard `lib/<app>` layout, build lists) and how `wx` fits the `ALL_ERLANG_APPLICATIONS` build.
- `../crypto/`, `../compiler/` — sibling apps with their own `configure`/`configure.ac`, like `wx`.
- `../../make/app_targets.mk`, `../../make/test_target_script.sh` — the `make`/`make test` driver referenced above.
