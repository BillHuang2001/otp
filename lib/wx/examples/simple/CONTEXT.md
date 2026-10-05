# simple — Minimal wxerlang Example Programs

## Intent
Four tiny standalone sample programs demonstrating the smallest possible wxErlang apps — the "getting started" examples that accompany the `wx` documentation.
They are documentation artifacts, not part of the `wx` runtime; the normal OTP build only compiles and optionally installs them, and never runs them.

## API Surface
Each file is a single self-contained `-module`, exports only `start/0`, and is started manually from an Erlang shell (`hello:start().`) — there is no app/supervisor wrapper and no cross-file sharing.
| File | What it demonstrates |
| --- | --- |
| `hello.erl` | "Really minimal" app using the plain `wx` API: `wx:new/0`, `wx:batch/1`, `wxFrame:new` + status bar, a hand-written `receive` event loop matching `#wx{event=#wxClose{}}`, then `wx:destroy/0`. |
| `hello2.erl` | The same app re-implemented with the `wx_object` behaviour: `wx_object:start_link/3` plus `init/1`, `handle_event/2`, `handle_info/2`, `handle_call/3`, `code_change/3`, `terminate/2`; the frame is both the wx object and the state. |
| `minimal.erl` | A slightly larger plain-API app: frame icon, menubar with stock `?wxID_EXIT`/`?wxID_ABOUT`/`?wxID_HELP_CONTENTS` items, `command_menu_selected` handling, and a modal `wxMessageDialog` "About" box; the loop also has an `after 1000` timeout printing dots. |
| `menu.erl` | A large "Test of menus" harness (~580 lines): dozens of `?menuID_*` macros exercising `wxMenuBar`/`wxMenu`/`wxMenuItem` (append/insert/delete/toggle/enable/check/radio/item labels/find, submenus, popup menus, accelerators/mnemonics), a read-only `wxTextCtrl` log pane, and a `wxTaskBarIcon` with a dynamically created popup menu. |
| `Makefile` | Compiles all four in place with `erlc` (`TESTMODS = hello hello2 minimal menu`), provides `run:` (`erl -smp -detached -pa $(TOPDIR)/ebin -s hello`), `clean`, and `release_spec`. |

Makefile details: `ERL_COMPILE_FLAGS += +nowarn_missing_doc +nowarn_missing_spec_documented -I $(ERL_TOP)/lib -pa $(ERL_TOP)/lib/wx/ebin`; `TOPDIR = ../..` (`lib/wx`); includes `../../vsn.mk` and `../../config.mk`; `docs:`, `release_tests_spec:` and `release_docs_spec:` are empty.

## Constraints
- These are sample/documentation sources: the `Makefile` compiles them into the same directory (`BIN = .`), and `.beam` files are build artifacts (removed by `make clean`, git-ignored via the app's `ebin`/build rules).
- `release_spec` installs **only the `.erl` sources** (`$(INSTALL_DATA) $(TESTSRC) $(EXRELSYSDIR)`) into `$(RELSYSDIR)/doc/examples/simple` — unlike `../demo/` and `../sudoku/`, nothing is precompiled into the release, so an installed user must `erlc` them by hand.
- Each program is deliberately a flat single module with a manual event loop or `wx_object` callbacks; do not "modernise" them into a shared app — their purpose is to stay readable as documentation.
- All four require a working GUI/display and a usable `wx` NIF (`WXERL_CAN_BUILD_DRIVER`); they cannot run headless.

## Known Issues
- `minimal.erl` loads its frame icon from `filename:join(filename:dirname(code:which(?MODULE)), "sample.xpm")`, but no `sample.xpm` exists anywhere in the repository — the icon call fails if that file is not supplied next to the compiled module.
- `menu.erl` uses `wxTaskBarIcon` and the `?wxMB_DOCKABLE` menubar style, which are platform-dependent (taskbar/dock support); it is not portable to every wx port.
- The header comments of the sample files have drifted from reality: `hello2.erl` still says `File : hello.erl`, `menu.erl` says `File : menus.erl`, and `menu.erl`'s About dialog hard-codes the archaic version string `"Welcome to Wx 0.97.5.26!"`.

## Notes for Agents
- Nothing here is exercised by the OTP build, CI or any test suite — never assume `make` or `make test` covers these files.
- Icons/assets come from `code:priv_dir(wx)` (`menu.erl` uses `erlang-logo32.png`/`erlang-logo128.png`, see `../priv/`), so `menu.erl` only works when run against an installed/built `wx` app.
- Run manually with e.g. `erlc -I ../../include hello.erl && erl -pa ../../ebin -s hello`; the `Makefile`'s `run` target starts `hello` detached.
- `../Makefile` builds `demo sudoku simple` as `SUB_DIRECTORIES`; the three example dirs share the same `erlc`-in-place pattern but differ in what they install.

## Test Strategy
- There is no test suite in this directory: it contains no `*_SUITE.erl`, no `test/` subdirectory, and `release_tests_spec` is intentionally empty.
- Verification is manual only — compile with `make` (or `erlc`) and run each module's `start/0` against a display.

## See Also
- `../CONTEXT.md` — the three example directories and how they are built/installed.
- `../demo/`, `../sudoku/` — larger examples that precompile `.beam` files into the release (contrast with the source-only install here).
- `../../CONTEXT.md` — the `wx` app itself (runtime modules, generated bindings, `Test Strategy`).
