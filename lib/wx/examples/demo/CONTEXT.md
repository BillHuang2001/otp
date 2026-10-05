# wx demo — sample wxWidgets/Erlang application

## Intent
Sample / documentation code for the `wx` app: an interactive browser of the wxWidgets widget surface.
`demo` is a running application (not a library) whose 3-pane UI lists every example, shows the example's Erlang source with syntax highlighting, and hosts the live widget in an embedded pane.
It is compiled and installed as release documentation — never part of the wx runtime.

## API Surface
All modules are flat in this directory (no subdirectories). 29 modules = `demo` + `demo_html_tagger` + 27 `ex_*` examples.

- `demo.erl` — launcher and top-level window; a `wx_object` server.
  - `start/0`, `start/1` (debug opts), `start_link/0|1`; `demo:format/3` appends to the shared log `wxTextCtrl` (used by every example for log output).
  - Builds a 3-way `wxSplitterWindow` layout: left = example list (`wxListBox`) + source pane (`wxStyledTextCtrl` with `?wxSTC_LEX_ERLANG`), right = live demo pane, bottom = events/log `wxTextCtrl`.
  - Enumerates examples at runtime via `filelib:wildcard("ex_*.beam")` in the **current working directory**, so it must be started with cwd = this directory.
  - Loads each example by reading `ex_<Name>.erl` from cwd (for the source pane) and calling `<Mod>:start([{parent,Panel},{demo_sz,Sz},{log,Ctrl}])`.
- `demo_html_tagger.erl` — helper (not a `wx_object`): `erl2htmltext/1` parses an Erlang source file with `epp_dodger`/`erl_scan`/`erl_syntax` and returns a colourised HTML string; `erl2htmlfile/1` writes it to `./html/<base>.html`.
  Used by `demo.erl`'s "Print code" menu (`wxHtmlEasyPrinting:previewText/2`) and is the sole external caller of `erl_syntax`.
- Assets (loaded by examples **relative to cwd**): `image.jpg` (`ex_canvas`, `ex_gl`, `ex_static`), `erlang.png` (`ex_gl`), `ex_htmlWindow.html` (`ex_htmlWindow`).
- `Makefile` — `TESTMODS` list, `erlc` compile (`+nowarn_missing_doc +nowarn_missing_spec_documented +nowarn_deprecated_catch -I $(ERL_TOP)/lib -pa $(ERL_TOP)/lib/wx/ebin`), `run` (`erl -smp -detached -pa ../../ebin -s demo`), `clean`, and `release_spec` (installs the `.erl` sources, the `.beam` files and the three assets into `$(RELSYSDIR)/doc/examples/demo`).

## Example Modules (`ex_*.erl`)
Each is a `-behaviour(wx_object)` implementing `start/1` (+ `init`/`handle_*`/`terminate`/`code_change`); standard shape `wx_object:start_link(?MODULE, Config, [])`, reading `parent` (and logging via `demo:format/3`) from `Config`.
- Sizers & layout: `ex_sizers` (sizer zoo — largest example), `ex_aui` (wxAuiManager/Notebook/dock art), `ex_notebook`, `ex_splitterWindow`, `ex_sashWindow`.
- Basic controls: `ex_button`, `ex_choices` (choice/combo/spin/listbox), `ex_radioBox` (radio/checkbox), `ex_slider`, `ex_gauge`, `ex_static`, `ex_textCtrl`.
- Data views: `ex_listCtrl` (report mode + image list + art provider), `ex_treeCtrl`, `ex_grid`.
- Menus/frames/dialogs: `ex_popupMenu`, `ex_frame_utils` (toolbar/statusbar/mini-frame/art), `ex_pickers` (file/dir/font/colour/date pickers), `ex_dialogs`.
- Drawing/DC: `ex_canvas` (DC overlay), `ex_canvas_paint` (print preview), `ex_graphicsContext`, `ex_cursor`.
- Web/media: `ex_htmlWindow` (loads `ex_htmlWindow.html`), `ex_webview` (needs wxWidgets webview support; falls back to a "Support is missing" label when the `wxWebView` module is absent).
- Notification: `ex_notificationMessage` (guards its event-based variant behind wxWidgets ≥ 3.1).
- OpenGL: `ex_gl` — needs OpenGL/GLU at runtime; uses `gl:`/`wxGLCanvas` and includes `gl.hrl`/`glu.hrl` in addition to `wx.hrl`.

## Constraints
- Sample/documentation code: the normal OTP build only compiles these modules (and optionally installs them) — nothing here is exercised by the build.
- Running any example requires a GUI/display; `ex_gl` additionally needs OpenGL/GLU, `ex_webview` needs webview support in wxWidgets.
- Every example is a `wx_object` and must be started through `demo.erl` (it supplies `parent`/`log`); the `ex_*` modules are not meant to be started standalone.
- Do not depend on these modules from `lib/wx/src` — `demo` is only referenced from `wx:demo/0`.

## Known Issues
- `demo.erl` resolves examples and sources relative to the process cwd; it works only when started from this directory (`wx:demo/0` sets cwd for you).
- `ex_aui.erl` and `ex_htmlWindow.erl` use the relative `-include("../../include/wx.hrl")` instead of the `-include_lib(...)` form used by all other modules; the relative form only resolves inside the build tree and breaks recompilation of the installed `doc/examples/demo` sources.

## Notes for Agents
- These files are examples/tutorials, not shipped runtime code — avoid "fixing" them into a library.
- Header comments are mostly boilerplate (the "widgets and usage of Wx" blurb); the `-module`/`-export`/wx-module usage is the real signal of what each example demonstrates.
- `wx:demo/0` (`lib/wx/src/wx.erl`) sets cwd to the source-tree `../examples/demo` when present, else the installed `../doc/examples/demo`, then calls `demo:start/0`; the `.beam` files must already be compiled.
- Assets must be loaded with cwd = this directory (see Known Issues).

## Dependencies
- Requires the built/installed `wx` app (`wx`, `wx_object`, `wx*` class modules) plus `stdlib`/`kernel`.
- `demo_html_tagger` requires `syntax_tools` (parsing) at runtime.
- `ex_gl` requires OpenGL/GLU; `ex_webview` requires wxWidgets built with webview support; GUI display required for all.

## Test Strategy
- There is **no test suite** in this directory — no Common Test suites, no unit tests; it is manually exercised through `wx:demo/0` or `make run`.
- CI does not run the demo; verification is manual on a machine with a display and a wx-enabled build.

## See Also
- `..//CONTEXT.md` (parent, `lib/wx/examples`) and `../CONTEXT.md` (the `wx` app) for build/install wiring.
- `../../src/wx.erl` — `wx:demo/0`, the supported entry point that launches this demo.
