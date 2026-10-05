# wx examples — Sample wxWidgets/Erlang Programs

## Intent
Sample/documentation programs for the `wx` app (Erlang bindings to wxWidgets): a full widget browser (`demo/`), minimal getting-started programs (`simple/`) and a complete small game (`sudoku/`).
They are documentation artifacts, not part of the wx runtime — the OTP build only compiles them in place, and `make install` copies them into the release as `$(RELSYSDIR)/doc/examples/<name>`.

## API Surface
- `Makefile` — includes `../vsn.mk`, `../config.mk` and sets `SUBDIRS = demo sudoku simple` (`SUB_DIRECTORIES=$(SUBDIRS)`, recursed via `$(ERL_TOP)/make/otp_subdir.mk`).
- `../Makefile` adds `examples` to `SUBDIRS` only in a normal build (after `src`, and `c_src` when `CAN_BUILD_DRIVER` is true); a `BOOTSTRAP` build builds `src` only, so examples are skipped entirely.
- Each subdirectory has its own `Makefile` with the same pattern: a `TESTMODS` list compiled in place with `erlc` (flags `+nowarn_missing_doc +nowarn_missing_spec_documented [+nowarn_deprecated_catch] -I $(ERL_TOP)/lib -pa $(ERL_TOP)/lib/wx/ebin`), `clean`, a `run:` target (`erl -smp -detached -pa ../../ebin -s <module>`), empty `docs`/`release_tests_spec`/`release_docs_spec`, and a `release_spec` that installs into `$(RELSYSDIR)/doc/examples/<name>`.
- Install payload differs per directory: `demo/` installs `.erl` + `.beam` + `image.jpg`, `erlang.png`, `ex_htmlWindow.html`; `sudoku/` installs `.erl` + `.beam` + `sudoku.hrl`; `simple/` installs `.erl` sources **only** (no `.beam`).
- Runtime entry point: `wx:demo/0` in `../src/wx.erl` sets cwd to the source-tree `../examples/demo` (preferred) or the installed `../doc/examples/demo`, then calls `demo:start/0`.
- No Erlang/API surface is exported to other applications; each sample is standalone and started from a shell or `make run`.

## Constraints
- Samples are compiled in place (`BIN = .`); `.beam` files here are untracked build artifacts and are not installed for `simple/`.
- Nothing in `examples/` may be depended on by `../src/`: `wx:demo/0` is the only reference from the runtime.
- Keep each sample flat, self-contained and readable — its value is pedagogical; do not refactor them into a shared library/app.
- Adding or renaming an example in `demo/` requires both the `ex_` prefix (the launcher discovers `ex_*.beam` by wildcard) and an entry in `demo/Makefile`'s `TESTMODS`.
- Every sample needs a GUI/display and a usable wx NIF; `demo/ex_gl` also needs OpenGL/GLU, `demo/ex_webview` needs wxWidgets webview support, `demo/ex_notificationMessage` needs wxWidgets ≥ 3.1 for its event variant.

## Known Issues
- `demo/` resolves its example modules, sources and assets relative to the process cwd, so it only runs when started from that directory (`wx:demo/0` handles this by changing cwd globally).
- Include style is inconsistent: `demo/ex_aui.erl`, `demo/ex_htmlWindow.erl` and `sudoku/sudoku.hrl` use the build-tree-only relative `-include("../../include/wx.hrl")`, while all other modules use `-include_lib("wx/include/wx.hrl")`; the installed `doc/examples/**` sources therefore cannot be recompiled as-is (only their installed `.beam` files work).

## Notes for Agents
- Nothing here is exercised by `make`, `make test` or CI; changes can only be verified manually on a machine with a display and a wx-enabled build.
- `make run` in a subdirectory compiles the samples first; `wx:demo/0` requires `demo/` to be already compiled.
- See each subdirectory's `CONTEXT.md` for module roles, per-example module notes and remaining quirks.

## Test Strategy
- There is no test suite anywhere under `examples/` (no `test/` dirs, no `*_SUITE.erl`, empty test/doc make targets); verification is manual via `make run` or `wx:demo/0`.

## Routing Table
| Path | Description |
| --- | --- |
| `demo/` | Interactive widget browser: `demo.erl` launcher + `demo_html_tagger.erl` + 27 `ex_*.erl` per-widget examples + image/HTML assets; installed with its `.beam` files |
| `simple/` | Four minimal standalone programs (`hello`, `hello2`, `minimal`, `menu`); installed as sources only |
| `sudoku/` | Complete small game: `sudoku.erl` entry, `sudoku_gui`/`sudoku_board` (`wx_object`), `sudoku_game` engine, `sudoku.hrl`; installed with its `.beam` files |

## See Also
- `../CONTEXT.md` — the `wx` app (runtime modules, generated bindings, `wx:demo/0`).
- `../src/wx.erl` — `demo/0`, the supported entry point that launches `demo/`.
- `../../CONTEXT.md` — OTP application conventions and how `wx` (and its examples) fit the `BUILD_ALL` app list.
