# debugger — Erlang Debugger & Interpreter

## Intent
The `debugger` OTP application (version `7.0`, `DEBUGGER_VSN` in `vsn.mk`): a source-level GUI debugger for Erlang plus the low-level Erlang interpreter that powers it.
It lets you interpret modules, set line breakpoints, step through execution, and inspect bindings/stacks — either through the wx GUI or headlessly from the shell.
The application is two layers over one shared state store: the **interpreter core** (`int`, `i`, `dbg_*`, usable without any GUI) and the **wx GUI** (`debugger`, `dbg_wx_*`), where the GUI is just one possible *attached process* to the interpreter.
Built in place like every OTP app; `lib/Makefile` builds it among `ALL_ERLANG_APPLICATIONS` (only under `BUILD_ALL`).

## API Surface
- `debugger` — GUI entry module: `start/0,1,2`, `stop/0`, `quick/3` (interpret + auto-attach + apply), `auto_attach/1`; the only GUI backend is `wx` (`which_gui/0` always returns `wx`).
- `int` — public interpreter API: interpret (`i/1,2`, `ni/1,2`, `n/1`, `nn/1`, `interpreted/0`, `file/1`, `interpretable/1`), breakpoints (`break/2`, `delete_break/2`, `break_in/3`, `no_break/0,1`, `disable_break/2`, `enable_break/2`, `action_at_break/3`, `test_at_break/3`, `get_binding/2`, `all_breaks/0,1`), process control (`attach/2`, `step/1`, `next/1`, `finish/1`, `continue/1,3`, `auto_attach/0,1,2`, `stack_trace/0,1`, `snapshot/0`, `clear/0`, `start/0`, `stop/0`, `subscribe/0`), the attached-process API (`attached/1`, `meta/2,3`, `contents/2`, `functions/1`) and `eval/3` (called only by Kernel's `error_handler`).
- `i` — backwards-compatible shell short forms over `int` plus status/display helpers (`ia*`, `ib*`, `ii`, `iq`, `ip`, `ipb`, `ist`, `im`, `iv`, `help`, …); re-exported into the shell by `lib/stdlib/src/shell_default.erl`, so callable without the `i:` prefix.
- Full module-by-module API surface, internal design and routing table: see `src/CONTEXT.md`.
- Each open GUI window is a pair `dbg_wx_<x>` (logic) + `dbg_wx_<x>_win` (wx widgets).

## Application metadata
- `src/debugger.app.src` — `vsn` from `%VSN%`; `applications` `[kernel, stdlib, compiler]`; `optional_applications` `[compiler, wx]`; `registered` `[dbg_iserver, dbg_wx_mon, dbg_wx_winman]`; `modules` is a hand-written list that must stay in sync with `MODULES` in `src/Makefile`.
- `src/debugger.appup.src` — always `{restart_application, debugger}` in both directions (no release-handler upgrade procedure possible for the interpreter).
- `Makefile` — `SUB_DIRECTORIES = src priv doc`; `DIA_PLT_APPS=wx` (Dialyzer runs with the wx PLT).
- `AUTHORS`, `LICENSE`-header SPDX = Apache-2.0.

## Constraints
- Only modules compiled with `debug_info` (abstract code) can be interpreted; `interpretable/1` fails with `no_src`/`no_beam`/`no_debug_info`/`badarg`.
- Modules belonging to Kernel, STDLIB, `erts`, `gs` or the `debugger` app itself must not be interpreted (deadlock / emulator-crash risk); `int` actively rejects them with `{error,{app,App}}`.
- The GUI layer requires the `wx` application and a display; `wx` is optional, so the interpreter core must stay usable without it.
- `src` compiles with `-Werror`: a warning in any module breaks the build (unsafe-call warnings are silenced per-module with `nowarn_possibly_unsafe_function`).
- Never edit generated files: `ebin/*.beam`, `ebin/debugger.app`, `ebin/debugger.appup` are produced from the `.src` templates by `sed` substituting `%VSN%`.

## Dependencies
- Required OTP apps: Kernel, STDLIB, Compiler.
- Optional: wx (GUI only). `include_lib("wx/include/wx.hrl")` needs `-pa $(ERL_TOP)/lib/wx/ebin` at compile time (set in `src/Makefile`).
- Runtime peer: Kernel's `error_handler` calls `int:eval/3` to hand a call to the interpreter when a breakpoint is hit.

## Test Strategy
- Common Test suites live in `test/` (see `test/CONTEXT.md`); `make test` in this dir installs them as `$(RELEASE_PATH)/debugger_test` and runs `ct_run -spec .../debugger.spec`.
- Headless coverage comes from the interpreter-API suites (`int_SUITE`, `int_break_SUITE`, `int_eval_SUITE`) and the language-feature suites (each interprets *itself* via `test_lib:interpret/1`), plus app-level checks in `debugger_SUITE`.
- There is no automated GUI coverage: `dbg_ui_SUITE`'s `dbg_ui/1` always returns `{skipped,...}`, and its `manual_tests` group is human-driven via a hand-maintained `manual_results.erl`.
- Interpreted test data must be built with `debug_info` (e.g. `int_SUITE_data/Emakefile`).

## Notes for Agents
- `ebin/` is a git-ignored build-output directory (only `.gitignore` tracked); compiled beams are not in the repository.
- The interpreter needs `+debug_info`-compiled code, so debugging code of the debugger's own dependency apps is not possible by design.
- Legacy references to `gs` (the pre-wx graphics system) remain in comments and in the non-interpretable-app guard; historical naming, not a live backend.
- The debugger was formerly part of the `tools` app; stale `/clearcase/...` paths still appear in some manual-test text.

## Routing Table
- `src/` → all 24 Erlang modules (`int`/`i` interpreter core + `dbg_*` engine + `dbg_wx_*` GUI) and `debugger.app.src`/`.appup.src`; the interpreter-vs-GUI architecture, module API surface and design decisions are documented in `src/CONTEXT.md`.
- `test/` → Common Test suites and `*_SUITE_data/` dirs, plus `debugger.spec`/`debugger.cover`; suite families and gaps are documented in `test/CONTEXT.md`.
- `doc/` → the ExDoc build (`docs.exs`), user-facing guides (`guides/introduction.md`, `guides/debugger_chapter.md`), release notes (`notes.md`) and the `assets/` screenshots; built via `make/doc.mk`.
- `priv/` → runtime data: the `erlang_bug.png` window icon (installed to `lib/debugger-$(VSN)/priv`), plus its `Makefile` and `.license`.
- `vsn.mk` → defines `DEBUGGER_VSN` (used by `src/`, `priv/`, `doc/` Makefiles); `Makefile`/`AUTHORS` → app build driver and author list.

## See Also
- `../Makefile` and `../CONTEXT.md` — how `lib/` builds and lists the debugger app.
- `../wx/` — the GUI framework backend; `../stdlib/src/shell_default.erl` — re-exports `i`'s functions into the shell; `../kernel/src/error_handler.erl` — calls `int:eval/3`.
- `../../HOWTO/TESTING.md` — how to build/run the debugger test set.
