# sudoku — Sample Sudoku Game (wx example)

## Intent
A small but complete Sudoku game used as a non-trivial demonstration of the `wx` binding (menus, buttons, custom-drawn panel, dialogs, printing, a worker process).
It is a documentation/example program, not part of the `wx` runtime: the normal OTP build only compiles it (and optionally installs it into `doc/examples/sudoku`), never runs it.
Built as a subdir of `../Makefile` (`SUBDIRS = demo sudoku simple`) via `make/otp_subdir.mk`; installed under `$(RELSYSDIR)/doc/examples/sudoku`.

## API Surface
- `sudoku.erl` — entry point; `start/0` (halts the VM on quit) and `go/0` (keeps VM alive); both spawn a linked process that creates the GUI, waits for its `{gfx, GFX}` message, then enters the game engine (`sudoku_game:init/1`).
- `sudoku_gui.erl` — the `wx_object` GUI: owns the frame/menubar/buttons and hosts the board widget; `new/1` calls `wx:new/0` + `wx_object:start_link`; `dialog/2` dispatches the NEW/EMPTY/CLEAR/HINT/OPEN/SAVE/RULES/ABOUT/SHOW_ERROR/PRINT/level commands.
- `sudoku_board.erl` — the `wx_object` board widget (a custom-drawn `wxPanel`); public API `new/1`, `setup_board/2`, `clear_board/1`, `left/1`, `get_board_data/1`, `set_board_data/2`, `set_butt/3`, `butt_correct/3`, `get_state/1`, `redraw/3` (also reused to render into a printer DC from the gui).
- `sudoku_game.erl` — the pure game-engine process: puzzle generation/validation/solving; exports `init/1`, `loop/1`, `indx/1`, `rcm/1`, `level/1`; owns the `#s{}` state and spawns a low-priority background generator (`create_games/2`).
- `sudoku.hrl` — shared header: `-record(s, ...)` game state, command-id macros (`?NEW`/`?EMPTY`/`?HINT`/`?OPEN`/`?SAVE`/`?CLEAR`/`?PRINT*`/levels/`?QUIT`/`?ABOUT`) and the `?TC(Cmd)` timing macro (prints elapsed time per call).
- `Makefile` — compiles `TESTMODS` (`sudoku sudoku_board sudoku_game sudoku_gui`) with `erlc`; `run` target launches via `erl -smp -detached -pa ../../ebin -s sudoku`.

## Message Flow
- `sudoku_gui` ⇄ `sudoku_game`: gui→game `{op,?NEW,Level}`, `{op,?EMPTY}`, `{loaded,Game}`, `{solve,All}`, `{validate,Butt,Val}`, `{get_game,Pid}`, `quit`; game→gui `{init,Game}`, `{busy,start|stop}`, `{correct,{R,C}}`, `{wrong,{R,C}}`, `{set_val,Ind,Val}`, `{working,Percent}`.
- `sudoku_board` → parent gui pid: raw user input `{set_val,Id,Val}` (key/text) and popup-menu requests; the gui forwards to the game and updates the board via the `sudoku_board` call API.
- The background generator process talks only to the engine: sends `{game,{Level,Game}}` (engine queues it in `#s.games`) and `{working,N}` status to the gui; `gen_game` commands wake it.
- Startup handshake: gui sends `{gfx, self()}` to the creator process, which then hands that pid to `sudoku_game:init/1`.

## Constraints
- Never hand-edit generated files — none here, but the game must stay in sync with `../../include/wx.hrl` (the generated public header).
- GUI code must run inside a wx environment: `sudoku_gui:new/1` is the only place calling `wx:new/0`; all other processes only send messages.
- Keep the four modules' roles separate: GUI/board never contain game logic; `sudoku_game` never touches wx.

## Known Issues
- `sudoku.hrl:22` uses the RELATIVE include `-include("../../include/wx.hrl")`, which resolves correctly inside the build tree (`lib/wx/include/wx.hrl`) but breaks if the installed `doc/examples/sudoku` sources are recompiled (installed headers live under `$(RELEASE_PATH)/lib/wx-<vsn>/include`).
- `sudoku_gui` calls `file:consult/1` to load a saved board and `file:open(...,[write])`/`io:format` to save one; the save file is an untrusted Erlang term, hence the `-compile([{nowarn_possibly_unsafe_function, {file, consult, 1}}])` waiver at the top of the module.

## Notes for Agents
- This directory has no subdirectories and no routing table — all four modules plus the header and Makefile are flat.
- `?TC/1` wraps several calls with `timer:tc/3` and prints timing to stdout on every invocation; harmless but noisy in test runs.
- The Makefile sets `ERL_COMPILE_FLAGS += +nowarn_missing_doc +nowarn_missing_spec_documented +nowarn_deprecated_catch -I $(ERL_TOP)/lib -pa $(ERL_TOP)/lib/wx/ebin`; no release/test/doc targets beyond install.

## Test Strategy
- There is NO test suite in this directory: no `test/`, no `*_SUITE.erl`, and the Makefile's `docs`/`release_tests_spec`/`release_docs_spec` targets are empty.
- Verification is manual: `make run` (or `erl -pa ../../ebin -s sudoku`) needs a working wxWidgets GUI/display and a built `wx` NIF; it cannot be exercised headlessly.

## See Also
- `../CONTEXT.md` — the wx example set (`demo`, `simple`, `sudoku`) and how it is built.
- `../../CONTEXT.md` — wx app conventions, the generated `include/wx.hrl` header, and `wx_object`.
