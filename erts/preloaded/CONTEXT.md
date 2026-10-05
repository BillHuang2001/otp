# erts/preloaded/ — Modules Compiled Into the BEAM Emulator

## Intent
Holds the Erlang modules that are compiled into the emulator binary and available before any code loader/path exists.
During boot the VM has no way to load code from disk until `erl_prim_loader` works, so this small set of modules is linked into the executable (as `beams.o` on Unix or a `.rc` resource on Windows) and preloaded into the code area at startup.
They are the minimum needed to bring up the loader, the code server/purger, the `init` boot-script interpreter and the built-in NIF/driver libraries (`prim_*`, `zlib`, `socket`), plus a stub `erlang` module whose bodies live in C.
Because these beams are checked into git and compiled into the VM, changing any of them requires the special `./otp_build update_preloaded` flow (see Constraints) and an emulator rebuild.
See `../../HOWTO/BOOTSTRAP.md` and `../../HOWTO/DEVELOPMENT.md#preloaded-and-primary-bootstrap` for the authoritative description.

## API Surface
Two-level split, both listed in `src/Makefile` (`PRE_LOADED_MODULES`) and mirrored by `../../erts/emulator/Makefile.in`:
- `src/` — sources for every preloaded module, the app resource `erts.app.src` and the `add_abstract_code` helper.
- `ebin/` — the compiled `.beam` files that are **tracked in git** (this is derived code committed on purpose; `otp_build update_preloaded` recompiles and commits them, e.g. "Update preloaded modules" commits). `ebin/erts.app` is generated and NOT tracked (see `.gitignore`).

### Preloaded modules and their role
Language / BIF stubs:
- `erlang.erl` — the BIF module. Only specs, docs and `erlang:nif_error(undefined)`/`erlang:nif_error(stub)` stub bodies; the real implementations are C functions registered from `../emulator/beam/bif.tab` via `bif.c`. Defines the auto-imported BIFs and predefined types; `moduledoc` is the source of the erlang module docs.
- `erts_internal.erl` — undocumented internal helpers backing ERTS functionality; apps must never call it.

Boot / code loading / code management (system processes):
- `erl_init.erl` — the very first Erlang process; loads the static NIF modules (`zlib`, `erl_tracer`, `prim_buffer`, `prim_file`, and `prim_socket`/`prim_net` if already loaded) then hands over to the boot module.
- `erl_prim_loader.erl` — primitive code loader: reads the boot script and loads modules via `efile` (local) or `inet` (distributed boot), before `kernel`'s `code_server` exists.
- `init.erl` — the boot-script interpreter/system controller (`-boot`, `-mode interactive|embedded`, `-pa`, `-run`, `-s`, ...), start/stop/restart of the node.
- `erts_code_purger.erl` — system process handling code purging (`purge/1`, `soft_purge/1`).
- `erts_literal_area_collector.erl` — helps GC copy literals out of literal areas to allow an area to be released.
- `erts_trace_cleaner.erl` — system process that cleans up tracing state when processes/modules terminate.
- `erts_dirty_process_signal_handler.erl` — system process that handles signals directed at dirty processes.

Native BIF/driver backends (thin Erlang front-ends over NIFs):
- `atomics.erl` — atomic arrays (mutable, lock-free 64-bit integers).
- `counters.erl` — shared mutable counter arrays.
- `persistent_term.erl` — constantly-readable persistent term storage.
- `prim_buffer.erl` — mutable binary buffer NIF, factored out of drivers/NIFs; thread-unsafe.
- `prim_file.erl` — NIF file primitives used by `file`/loader.
- `prim_inet.erl` — `inet` driver primitives.
- `prim_zip.erl`, `zlib.erl` — NIF bindings to the vendored zlib and the zip helper (shared via `zip_internal.hrl`).
- `erl_tracer.erl` — NIF module used by the tracing machinery (must be `on_load`ed early).
- `prim_eval.erl`/`prim_eval.S` — `prim_eval:'receive'/2` implemented in raw BEAM assembly so `erl_eval` can evaluate `receive`; `prim_eval.erl` is only a Dialyzer stub whose abstract code is spliced into the assembled beam by `add_abstract_code`.

Socket/NIF-module set (ESOCK — only built/preloaded when the emulator is configured for the new socket API):
- `prim_socket.erl` — NIF-based socket API (`socket` module backend).
- `prim_net.erl` — primitives shared by the socket/`net` implementation.
- `socket_registry.erl` — registry process that the socket NIF notifies of created/deleted sockets; also used for socket monitors.
These three are `PRE_LOADED_ERL_ESOCK_MODULES` and are separately linked (`ESOCK_PRELOAD_BEAM`) so they can be excluded; `erslt`'s `erl_init` conditionally loads them via `if_loaded(prim_socket, ...)`.

### Application resource
- `src/erts.app.src` → generated `ebin/erts.app`: application `erts`, description "ERTS CXC 138 10", `vsn` = `%VSN%` (substituted from `../vsn.mk`), listing the preloaded modules (socket modules injected at `%ESOCK_MODS%`), no registered processes, no applications, and `runtime_dependencies` on stdlib/kernel/sasl.

### Contract with the emulator build
- `../emulator/Makefile.in` `PRELOAD_BEAM` = the `ebin/*.beam` files (comment: "This list must be consistent with PRE_LOADED_MODULES in erts/preloaded/src/Makefile"); `ESOCK_PRELOAD_BEAM` holds the socket-module trio and is only added when ESOCK is enabled.
- `../emulator/utils/make_preload` (Perl) converts the beams into C (`preload.c`, linked as `preload.o` → `PRELOAD_OBJ`) on Unix, or a windres resource (`beams.rc`) on win32; `PRELOAD_OBJ` is linked into `${EMULATOR_LIB}`/the `beam` executables.
- `make_preload` emits `preloaded_<module>[]` arrays + a module table that the VM uses to preload `ebin/erts.app`-listed modules at startup.

## Constraints
- **Must bootstrap**: these modules are compiled by the *already-built* (bootstrap/primary) compiler, so they may only use language features the committed primary bootstrap understands. `./otp_build update_primary` is the escape hatch when new language features are needed.
- **Minimal dependencies**: they may not depend on `lib/*` beyond kernel/stdlib primitives; `src/Makefile` only adds include paths for `lib/kernel` and `lib/stdlib` (`+nowarn_deprecated_catch`, `+deterministic` when `ERL_DETERMINISTIC=yes`).
- **Generated / not tracked**: `ebin/erts.app` (from `erts.app.src` by `sed` on `%VSN%`/`%ESOCK_MODS%`) and `src/prim_eval.abstr` are generated and git-ignored.
- **Tracked derived code**: `ebin/*.beam` ARE tracked — do not "clean" them away and do not hand-edit them; regenerate with `./otp_build update_preloaded [--no-commit]` (requires a working `erlc` in PATH) and then rebuild the system. PRs to upstream are expected NOT to include preloaded/primary-bootstrap binary updates (unreviewable binaries).
- `src/Makefile` is NOT run in the normal build in the sense of producing the tracked beams; the normal tree provides them, and the emulator consumes `ebin/*.beam`. The `../../ets/ebin` referenced from `src/Makefile` is its scratch/`EBIN` dir, and the `copy` target strips+commits into `../ebin`.
- One line per module keeps the diff of `.beam` updates and `PRELOAD_BEAM` edits reviewable; keep `src/Makefile` `PRE_LOADED_MODULES` and `emulator/Makefile.in` `PRELOAD_BEAM` in sync when adding a module.

## Design Decisions
- **Preload rather than load**: the loader itself is Erlang code, so a bootstrap paradox is avoided by embedding a fixed, self-contained module set in the VM.
- **`erlang.erl` is a stub** because the BIF implementations are C; keeping the module as source lets docs/specs/BIF tables live in one place while the code stays native.
- **`prim_eval.S`** exists because `receive`-as-a-function (needed by `erl_eval`) cannot be expressed in ordinary Erlang; a raw BEAM assembly module is hand-written and Dialyzer's abstract-code stub is spliced in by `add_abstract_code`.
- **`prim_*` front-ends** factor the Erlang-level glue of NIFs/drivers (`prim_buffer`, `prim_file`, `prim_inet`, `prim_socket`/`prim_net`, `prim_zip`, `zlib`) out of C and into tested Erlang, reducing the C surface.
- **`erts_*` system processes** (`erts_code_purger`, `erts_trace_cleaner`, `erts_dirty_process_signal_handler`, `erts_literal_area_collector`) must exist before `kernel` starts, so they live here rather than in an application.

## Known Issues / Gotchas
- **Editing a preloaded source has no effect** on the running VM until the corresponding `ebin/*.beam` is regenerated (`otp_build update_preloaded`) and the emulator is rebuilt/reinstalled. A changed `.erl` with a stale `.beam` silently ships old behavior.
- `src/Makefile` in a fresh checkout builds into `../../ets/ebin` (a scratch dir) when driven directly; the tracked artifacts live in `ebin/`. Don't confuse the two.
- The ESOCK trio is conditionally preloaded; code that assumes `prim_socket`/`prim_net`/`socket_registry` are always available must not rely on it (see `erl_init`'s `if_loaded`).
- `erlang.erl` is huge (≈470 KB); it is generated/maintained as the single source of BIF specs+docs, not meant to be split.

## Notes for Agents
- Do not add a new preloaded module without updating BOTH `src/Makefile` `PRE_LOADED_MODULES` and `../emulator/Makefile.in` `PRELOAD_BEAM`, and adding it to `erts.app.src`.
- `ebin/*.beam` are binary and committed; `git log -p` shows them as `Bin` diffs — use the accompanying `.erl` change / commit message to understand them.
- `src/` is flat; there is no per-module subdirectory to navigate.

## Dependencies
- A working Erlang compiler (the primary bootstrap in `../../bootstrap/`) to regenerate the beams; `erlc`, `escript` (for `add_abstract_code`).
- Includes from `lib/kernel` (src + include) and `lib/stdlib` (include).
- The emulator build that consumes them needs Perl (`emulator/utils/make_preload`) and, on Windows, `windres`.

## Test Strategy
- There is no dedicated `erts/preloaded/test` suite. The preloaded modules are exercised through the general suites: `../emulator/test/bif_SUITE.erl` (BIF vs. `erl_bif_types` cross-check; locates `preloaded/src` when running uninstalled), `../emulator/test/code_SUITE`, `../test/otp_SUITE.erl` (installed layout), and the various `prim_*`/`zlib`/`socket` suites in `../emulator/test`.
- `src/Makefile` has `dialyzer`/`dialyzer_plt` targets (`erts-preloaded.plt`) and the root `erts`/`preloaded` make targets compile these with `ERL_COMPILE_WARNINGS_AS_ERRORS=yes`, so Dialyzer + warnings-as-errors are the main static check.

## See Also
- `../emulator/Makefile.in` → `PRELOAD_BEAM`/`PRELOAD_OBJ` and how `beams.o` is linked into the VM
- `../emulator/utils/make_preload` → converter from `.beam` to C/Win resource
- `../emulator/beam/bif.tab`, `../emulator/beam/bif.c` → C implementations behind `erlang.erl` stubs
- `../../lib/kernel/` → `code_server`, `file`, `inet`, `socket`/`net` Erlang-level counterparts
- `../../bootstrap/` → primary bootstrap compiler used to rebuild these beams; `../../otp_build update_preloaded|update_primary`
- `../../HOWTO/BOOTSTRAP.md`, `../../HOWTO/DEVELOPMENT.md` → authoritative bootstrap/preloaded workflow

## Status
Complete for this node. `src/` is a flat terminal directory; no child CONTEXT.md files.
