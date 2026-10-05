# debugger/src — Erlang Debugger & Interpreter Sources

## Intent
All 24 Erlang modules of the `debugger` OTP application plus its `.app.src`/`.appup.src` templates and the `Makefile`.
The application has two largely independent layers that share one state store (`dbg_iserver`):
- **Interpreter core** (`int`, `i`, `dbg_*`): stepwise execution of Erlang code by interpreting BEAM *abstract code*, with line breakpoints, step/next/finish, binding inspection and stacktraces. Usable from the shell without any GUI.
- **wx GUI** (`debugger`, `dbg_wx_*`): the graphical Debugger (Monitor window, Attach Process window, break dialogs), built on `wx`.
The interpreter is the lower layer: the GUI is just one possible *attached process* (the CLI `i` module plus the shell are another).

## API Surface

### Top-level interfaces
- `debugger.erl` — GUI entry module. `start/0,1,2`, `stop/0`, `quick/3` (interpret + auto-attach + apply), `auto_attach/1`.
  `which_gui/0` always returns `wx` (the only GUI backend); `stop/0` and `start/2,3` delegate to `dbg_wx_mon`.
- `int.erl` (1123 lines) — the public interpreter API (documented, `-moduledoc`). Exports, grouped as in the source:
  - Interpretation: `i/1,2`, `ni/1,2` (all known nodes), `n/1`, `nn/1`, `interpreted/0`, `file/1`, `interpretable/1`.
  - Options/status: `auto_attach/0,1,2`, `stack_trace/0,1`, `snapshot/0`, `clear/0`.
  - Breakpoints: `break/2`, `delete_break/2`, `break_in/3`, `del_break_in/3`, `no_break/0,1`, `disable_break/2`, `enable_break/2`, `action_at_break/3`, `test_at_break/3`, `get_binding/2`, `all_breaks/0,1`.
  - Process control: `continue/1,3`, `step/1`, `next/1`, `finish/1`, `attach/2`, `start/0`, `stop/0`, `subscribe/0`.
  - **Attached-process API** (only for a process that attaches): `attached/1`, `meta/2,3`, `contents/2`, `functions/1`.
  - **`eval/3`** — entry point called only by Kernel's `error_handler` breakpoint handler.
- `i.erl` (483 lines) — shell short forms, backwards-compatible interface to `int` plus display helpers. Exports: `ia/1,2,3,4`, `iaa/1,2`, `ib/2,3,4`, `ibd/2`, `ibe/2`, `iba/3`, `ibc/3`, `ic/0`, `ii/1,2`, `il/0`, `im/0`, `ini/1,2`, `inq/1`, `ip/0`, `ipb/0,1`, `iq/1`, `ir/0,1,2,3`, `iv/0`, `ist/1`, `help/0`.
  These are callable without the `i:` prefix because `lib/stdlib/src/shell_default.erl` re-exports them for the shell.

### Interpreter core internals
- `dbg_iserver.erl` — gen_server that is the single source of truth: registered **globally** (`global:register_name/2`, lookup via `global:whereis_name/1` in `find/0`), name `dbg_iserver`.
  Holds `#state{}` with the ETS module database (`db`), a list of `#proc{}` records (debugged process ↔ meta process ↔ attached process, status, exit info), breakpoints, auto-attach and stack-trace settings, and subscribers.
  Wraps the gen_server with `call/1,2`, `cast/1,2`, `safe_call/1`, `safe_cast/1` (the `safe_*` variants auto-start the server).
- `dbg_iload.erl` (1264 lines) — turns a module's BEAM abstract code into the interpreter's internal representation and stores it in the ETS db. Exports only `load_mod/4` (Mod, File, Binary, Db).
  Loading is synchronous: it spawns a linked fun and waits for its `EXIT`, so no other process can interpret a half-loaded module.
- `dbg_ieval.erl` (2493 lines) — the interpreter itself: evaluates the abstract code received from the db. Exports `eval/3` (spawns/returns the meta process), `eval_expr/3`, `exit_info/5`, `check_exit_msg/3`, `exception/4`.
- `dbg_icmd.erl` — command/message handler for the meta process: decides whether to break (`cmd/3` before each expression), implements `step/1`, `next/1`, `continue/1`, `finish/1`, `skip/1`, `timeout/1`, `eval/2`, `set/3`, `get/3`, `handle_msg/4`, `tell_attached/1`, `get_binding/2`.
- `dbg_debugged.erl` — message loop for the *debugged* process. Single export `eval/3`, called from `int:eval/3` via `error_handler`.
- `dbg_istk.erl` — call-stack bookkeeping kept in the meta process's **process dictionary** (`?STACK = dbg_istk`). `init/0`, `push/3`, `pop/0,1`, `stack_level/0`, `delayed_stacktrace/0,2`, `bindings/1`, `stack_frame/2`, `backtrace/2`, `in_use_p/2`, plus `delayed_to_external/0` / `from_external/1` which convert the internal stack to/from a `{stack, term_to_binary(...)}` form that can be shipped to the debugged process.
- `dbg_idb.erl` — 58-line ETS wrapper (`insert/3`, `lookup/2`, `match_object/2`) that transparently dispatches to `rpc:block_call/4` when the table reference is `{Node, ModDb}`, idem for remote nodes.

### wx GUI layer
Convention: each area is split into a logic module `dbg_wx_<x>` (gen_server / wx_object-ish logic, name registered) and a `dbg_wx_<x>_win` module holding the wx widget code.
- `dbg_wx_mon.erl` / `dbg_wx_mon_win.erl` — Monitor window, the main Debugger window (interpreted modules, debugged processes, breakpoints); `dbg_wx_mon` is registered and is the app's start/stop entry point.
- `dbg_wx_trace.erl` / `dbg_wx_trace_win.erl` — Attach Process window (stepping buttons, evaluator area, bindings area, backtrace); this is the attached process passed to `int:auto_attach/2`.
- `dbg_wx_break.erl` / `dbg_wx_break_win.erl` — Break dialog (line / conditional / function breakpoints).
- `dbg_wx_interpret.erl` — module-selection dialog (uses `dbg_wx_filedialog_win`).
- `dbg_wx_settings.erl` — save/load Debugger settings (uses `dbg_wx_filedialog_win`).
- `dbg_wx_view.erl` — read-only view of the source of interpreted modules (uses `dbg_wx_src_view`).
- `dbg_wx_code.erl` — syntax-highlighting code editor area with breakpoint markers (`wxStyledTextCtrl`), used by the trace/monitor windows.
- `dbg_wx_src_view.erl` — minimal read-only code area (no breakpoint markers), used by `dbg_wx_view`.
- `dbg_wx_winman.erl` — gen_server window manager (registered `dbg_wx_winman`), tracks open windows and Debugger processes, drives the "Windows" menu.
- `dbg_wx_win.erl` — shared wx helpers (icons, colours, break button/marker handling, `to_string/1..2` wrappers that swallow wx/error output formatting).
- `dbg_wx_filedialog_win.erl` — `wx_object` file dialog with path completion; logic-only counterpart is absent (it is a self-contained widget).

### Headers and build files
- `dbg_ieval.hrl` — internal header: the `#ieval{}` record (call level, current line, module/function/arguments, error info, `top` flag) and the `?DB_REF_KEY(Mod)` macro; included by `dbg_icmd`, `dbg_istk`, `dbg_ieval`.
- `dbg_wx_filedialog_win.hrl` — large literal `?ERL_SRC`/`?ERL_HRL`/`?ERL_BIN` string lists of Erlang keywords/BIFs used for file-dialog syntax highlighting.
- `Makefile` — sets `MODULES` (compiled list) and both headers via `INTERNAL_HRL_FILES`; `ERL_COMPILE_FLAGS += -Werror -I $(ERL_TOP)/lib -pa $(ERL_TOP)/lib/wx/ebin +nowarn_deprecated_catch`.
- `debugger.app.src` — `{applications,[kernel,stdlib,compiler]}`, `{optional_applications,[compiler,wx]}`, `{registered,[dbg_iserver, dbg_wx_mon, dbg_wx_winman]}`.
- `debugger.appup.src` — always `{restart_application, debugger}` in both directions for `<<".*">>`.

## Constraints
- Only modules compiled with `debug_info` can be interpreted — the interpreter needs the abstract code embedded in the BEAM file.
- `int`/`debugger` depend on Kernel, STDLIB and wx; those applications' modules must not be interpreted (deadlock / emulator-crash risk). `int:check_application/1` actively rejects modules whose source path shows a `kernel-*`, `stdlib-*`, `erts-*`, `gs-*` or `debugger-*` application directory, returning `{error,{app,App}}`.
- The interpreter needs both source and object code (`no_src` / `no_beam` / `no_debug_info` / `badarg` are the `interpretable/1` failure reasons); sticky modules/dirs fail only later, at load time.
- Compilation is `-Werror`: warnings in any module break the app build.
- The GUI modules require the `wx` application and a display; `wx` is only an optional application, so the interpreter modules must remain usable without it.
- Breakpoints are line-based and a breakpoint line must contain an executable expression.

## Design Decisions
- **Meta process split.** When a process is debugged, the interpreter runs in a separate *meta process*; the debugged process waits in the `dbg_debugged:msg_loop/2` receive loop and replies to `{sys, Meta, {command, Cmd}}` requests.
  This is required because some operations (BIFs such as `link/1`/`spawn_link/1`, `receive`, and calls into non-interpreted code) must execute in the debugged process itself, while interpretation continues elsewhere.
  `dbg_debugged:msg_loop` handles exactly these four cases: result ready (`{ready, Val}`), uncaught exception, `receive` matching, and generic `{command,...}` (apply/eval) requests.
  Non-interpreted function calls are returned to the debugged process as `{dbg_apply,M,F,A}`.
  The stack is passed between meta and debugged process as a serialized `{stack, binary}` term (`dbg_istk:delayed_to_external/0`/`from_external/1`), so an Attach Process window can inspect a stack that lives in another process.
- **Single central state.** All bookkeeping (module db, processes, breakpoints, settings, subscribers) lives in `dbg_iserver` so that the GUI, `int`, and the meta processes always see consistent state; `safe_call/safe_cast` hide the lazily-started server from callers.
- **`dbg_idb` indirection.** The module database may be on a remote node (global `dbg_iserver`), so all ETS access is funnelled through `dbg_idb` to keep `dbg_ieval`/`dbg_icmd` free of node-awareness.
- **Logic/`_win` split.** Each GUI window is two modules so the wx-specific code (widgets, events, layouts) is isolated from the logic that talks to `int`/`dbg_iserver`; this dates from when the GUI backend was GS and is retained for wx.
- `i` is kept as a thin, separate module purely for backwards compatibility with the old shell short forms and status printers.

## Known Issues / Notes for Agents
- `ebin/` is a build output directory (git-ignored); compiled beams are not in the repository — do not look for them or edit them.
- `debugger.app` and `debugger.appup` are generated from their `.src` files by `sed -e 's;%VSN%;$(VSN);'`; always edit the `.src` (there is a `%VSN%` placeholder in both).
- `%VSN%` is not the only generated content: `debugger.app.src` also has a hand-written `modules` list that must be kept in sync with `MODULES` in the `Makefile` when adding/removing a module.
- The `dbg_wx_*_win` modules contain hard-coded wx menu/button/control IDs in `-define`s; these must not collide across windows.
- Comments still refer to "GS" (the pre-wx graphics system) in places and `i.erl`/`int.erl` still mention `gs` among the non-interpretable applications — historical naming, not a live backend.
- `dbg_ieval.erl`, `dbg_iload.erl` and `dbg_istk.erl` use `binary_to_term/1` (and `list_to_atom/1`) on interpreter data; those calls are explicitly silenced with `nowarn_possibly_unsafe_function` compile options, so `-Werror` does not flag them.
- The meta process keeps most of its state in the **process dictionary** (`dbg_istk:?STACK`, plus keys like `int`, `attached`, `breakpoints`, `self`, `exit_info`, `next_break`, `trace_stack`); state is not passed functionally through `dbg_iev*`.

## Dependencies
- Built-in apps: Kernel, STDLIB, Compiler (required); wx (optional, only for the GUI layer).
- `include_lib("wx/include/wx.hrl")` and `include_lib("kernel/include/file.hrl")` are used; the `-pa $(ERL_TOP)/lib/wx/ebin` compile flag exists so `include_lib` can resolve `wx/include`.
- Runtime peer: Kernel's `error_handler` module calls `int:eval/3` to hand a call over to the interpreter when a breakpoint is hit.

## Test Strategy
- Tests live in the sibling `../test/` directory (Common Test), not here; they are extensive (~40 `*_SUITE.erl`, notably `debugger_SUITE`, `int_SUITE`, `int_break_SUITE`, `int_eval_SUITE`, `line_number_SUITE`, `exception_SUITE`, the `bs_*` bit-syntax suites).
- `../test/dbg_ui_SUITE.erl` covers the wx GUI (needs `wx`); `../test/debugger_test.erl` and `../test/test_lib.erl` are shared helpers.
- Module `int` is the main testable surface without a GUI; GUI code is mostly exercised indirectly.

## Routing Table
| Area | Files |
| --- | --- |
| Public interpreter API | `int.erl` |
| Shell short forms / status display | `i.erl` |
| GUI application entry | `debugger.erl` |
| Interpreter state / ETS db server | `dbg_iserver.erl` |
| Module loading (abstract code → db) | `dbg_iload.erl` |
| Expression evaluator (meta process) | `dbg_ieval.erl` |
| Meta-process command & breakpoint handling | `dbg_icmd.erl` |
| Debugged-process message loop (`error_handler` hook) | `dbg_debugged.erl` |
| Call stack / stacktrace conversion | `dbg_istk.erl` |
| Remote-transparent ETS access | `dbg_idb.erl` |
| Internal `#ieval{}` record + `DB_REF_KEY` | `dbg_ieval.hrl` |
| Monitor window (main) | `dbg_wx_mon.erl`, `dbg_wx_mon_win.erl` |
| Attach Process window | `dbg_wx_trace.erl`, `dbg_wx_trace_win.erl` |
| Break dialog | `dbg_wx_break.erl`, `dbg_wx_break_win.erl` |
| Interpret-modules dialog | `dbg_wx_interpret.erl` |
| Settings save/load | `dbg_wx_settings.erl` |
| View interpreted module source | `dbg_wx_view.erl` |
| Code editor with break markers | `dbg_wx_code.erl` |
| Read-only source area | `dbg_wx_src_view.erl` |
| Window manager (gen_server) | `dbg_wx_winman.erl` |
| Shared GUI helpers | `dbg_wx_win.erl` |
| File dialog widget (+ `dbg_wx_filedialog_win.hrl` syntax tables) | `dbg_wx_filedialog_win.erl`, `dbg_wx_filedialog_win.hrl` |
| Build / app resource templates | `Makefile`, `debugger.app.src`, `debugger.appup.src` |
| Sibling test suites (read-only, escalate writes to parent) | `../test/` |
| Sibling docs (`../doc/`, `../priv/`, `../vsn.mk`, `../ebin/`) | outside this directory |
