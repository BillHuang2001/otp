# Megaco Flex Scanner (optional C text scanner)

## Intent
Optional accelerator for the *scanning* (tokenizing) phase of Megaco/H.248 **text** decoding, implemented as a flex(1)-generated linked-in driver in C.
It is one of the two implementations of the Megaco text scan phase; the pure-Erlang one is `../text/megaco_text_scanner.erl` (used when the flex scanner is not selected).
Whether the C scanner is built is decided at configure/build time (requires a real `flex`); whether it is *used* is decided at runtime by the `megaco` app env `scanner` (see Runtime Selection).
Token output is handed to the Erlang text parsers (`../text/megaco_text_parser_*`), so the token vocabulary in `megaco_flex_scanner_drv.flex.src` must stay in sync with the Erlang scanner/parsers.

## API Surface
Erlang side:
- `megaco_flex_scanner.erl` — public interface (`-moduledoc` published): `start/0,1`, `stop/1`, `scan/2`, `is_enabled/0`, `is_reentrant_enabled/0`, `is_scanner_port/2`; type `megaco_ports() :: port() | tuple()`.
- `megaco_flex_scanner_handler.erl` — `-moduledoc false` gen_server that owns the driver port(s); `start_link/0,1` returns `{ok, Pid, {flex, PortOrPorts}}`, which the caller stores as the `text_config` encoding config.
Native side (dynamically loaded linked-in drivers, `ErlDrvEntry` / `erl_driver.h` — **not** NIFs):
- `megaco_flex_scanner_drv` (single-threaded / non-reentrant variant) and `megaco_flex_scanner_drv_mt` (the `_mt` variant, reentrant when built that way).
Driver protocol: Erlang calls `erlang:port_control(Port, $s, Bin)`; on success the driver sends `{tokens, TokenList, LatestLine}` to the process that issued the `port_control` (`erl_drv_send_term` to `driver_caller`), which is why `scan/2` does the receive itself (5 s timeout); on scan failure `port_control` returns the error string. `megaco_flex_scanner:version/1` derives the protocol version from the first `SafeChars` token (99 = "let the parser decide").
`drv_name/0` picks the `_mt` driver whenever `erlang:system_info(threads)` is true — so `megaco_flex_scanner_drv` is only loaded on a non-threaded emulator.

## Files
- `megaco_flex_scanner_drv.flex.src` — the only hand-edited native source; it is *not* valid flex input (contains the placeholders `%FLEX_VERSION%`, `%MEGACO_YY_LINENO_OPTION%`, `%MEGACO_YY_REENTRANT_OPTION%`, `%MEGACO_DUMMY_DECL_YY_LINENO%` substituted by perl).
- `Makefile` (tracked, hand-written wrapper) — includes `$(ERL_TOP)/make/run_make.mk`, which recurses into `$(TARGET)/Makefile`; adds `conf`/`dconf`/`econf` (re-run the app configure with `FLEX_SCANNER_REENTRANT=disable|enable`), `setup` and `info` targets.
- `Makefile.in` (tracked autoconf template) — processed by the megaco `configure` into `src/flex/$(TARGET)/Makefile` (`AC_CONFIG_FILES([src/flex/$host/Makefile:src/flex/Makefile.in])` in `lib/megaco/configure.ac`).
- `modules.mk` — `MODULES` (the two Erlang modules), `FLEX_MODULES = megaco_flex_scanner_drv`, `STD_DRV`, `MT_DRV`.
- `prebuild.skip` (content `.`) — marks this directory as one whose prebuilt copies must be replaced from source during bootstrap/tar creation, i.e. the flex modules are always recompiled.

## Build
Generated chain in `src/flex/`: `megaco_flex_scanner_drv.flex.src` --perl--> `{drv,drv_mt}.flex` --`$(LEX) -P$* -o$@`--> `{drv,drv_mt}.c` --cc--> `../../priv/obj/$(TARGET)/{drv,drv_mt}.o` --ld--> `../../priv/lib/$(TARGET)/{drv,drv_mt}.so`.
Both `.so` files are always built when the scanner is enabled; `$(OBJDIR)`/`$(LIBDIR)` are created by a shell `mkdir -p` at makefile-parse time (`_create_dirs`).
`release_spec` installs the `.so` files as programs into `$(RELSYSDIR)/priv/lib` and copies the generated `.flex`/`.c` plus the `.erl` files into `$(RELSYSDIR)/src/flex`.
Relevant make variables / defines:
- `ENABLE_MEGACO_FLEX_SCANNER` (erlc: `-DENABLE_MEGACO_FLEX_SCANNER=<val>`) — normally substituted by configure, overridable on the make command line; forced `false` on win32.
- `ENABLE_REENTRANT_MEGACO_FLEX_SCANNER` (`-DMEGACO_REENTRANT_FLEX_SCANNER=true|false`, C flag `-DMEGACO_REENTRANT_FLEX_SCANNER`, flex `-R`) — reentrant scanner creates one port per scheduler.
- `ENABLE_MEGACO_FLEX_SCANNER_LINENO` (C: `-DMEGACO_LINENO` + flex `-Ca`, else `-DMEGACO_TOKENCNT` + flex `-Cfe`) — line-number reporting vs token-count reporting (~5-10% faster).
- Debug knobs: `MFS_DEBUG=true`, `MFS_FLEX_PERF=true` (flex `-p`), `MFS_FLEX_DEBUG=true` (flex `-d`); `TYPE=valgrind` strips `-O2` and adds `-DVALGRIND`.
- `FLEX_VSN = $(shell flex --version)` is embedded as the C `flex_version[]` string, but the `-DFLEX_VERSION` define is commented out, so nothing reads it.

## Runtime Selection
The pure-Erlang scanner is the default: with megaco app env `scanner` unset, `text_config` is `[]` and `megaco_text_scanner` is used.
With `{scanner, flex}` the stack starts `megaco_flex_scanner_handler` as a permanent worker (`megaco_config:init_scanner/0`); the encoding config is retrieved via `megaco:system_info(text_config)` and stored in `megaco_receive_handle.encoding_config` as `[{flex, port()}]` or `[{flex, ports()}]`.
There is **no graceful runtime fallback**: if the driver cannot be loaded or started, handler init returns `{stop, {failed_starting_scanner, Reason, []}}` and megaco start fails with `{scanner_start_failed, ...}`.
`../text/megaco_pretty_text_encoder.erl` and `../text/megaco_compact_text_encoder.erl` dispatch on the config (`[{flex, Port}]` / `[{version3,v3},{flex, Port}]` → `megaco_flex_scanner:scan/2`, otherwise `megaco_text_scanner:scan/1`).

## Constraints
- Never hand-edit generated files: `megaco_flex_scanner_drv.flex`, `megaco_flex_scanner_drv_mt.flex`, the corresponding `.c` files, or `$(TARGET)/Makefile` — they are git-ignored build outputs; edit `megaco_flex_scanner_drv.flex.src` / `Makefile.in` instead.
- The `drv_name()` used by `open_port` must match the driver name compiled into the driver (`MEGACO_DRV_NAME`) and the installed file name under `priv/lib`.
- The driver is loaded from `code:priv_dir(megaco) ++ "/lib"`, i.e. the `.so` must be reachable at `<megaco>/priv/lib/...` for both the source tree and an installed release.
- The Erlang modules reference the pure-Erlang scanner's token/version expectations; token changes here (and in `../text/`) must stay consistent between the two scanners.
- Win32 never builds or uses the scanner (no `SOLIBS`, no C targets); `megaco_flex_scanner.erl` carries `-dialyzer({nowarn_function, ...})` suppressions for that case.

## Known Issues / Gotchas
- `make setup` is required before the flex scanner can be used from an un-installed source tree: it creates `../../priv/lib/{megaco_flex_scanner_drv,megaco_flex_scanner_drv_mt}.so` symlinks pointing at `$(TARGET)/...so`. `SPECIAL_TARGETS = setup` in `../Makefile` is not consumed by anything in `make/`, so `setup` must be invoked explicitly (it propagates `make setup` from `lib/megaco` down through `src/` to here).
- Changing reentrancy or lineno requires re-running configure (`make dconf` / `make econf` in this directory, or passing the option from `lib/megaco`); it is not a make-time toggle of an existing build.
- Non-reentrant builds keep a single `static MfsErlDrvData mfs_drv_data` in C and therefore open exactly one port; reentrant builds compile with `ERL_DRV_FLAG_USE_PORT_LOCKING` and open one port per scheduler. Note that reentrancy is a property of the *build*, while the `_mt`/non-`_mt` file choice is a property of the *emulator build* (`system_info(threads)`), so with reentrancy disabled the driver actually loaded on an SMP emulator is still the non-reentrant `_mt` one.
- `is_enabled/0` and `is_reentrant_enabled/0` report compile-time constants (`ENABLE_MEGACO_FLEX_SCANNER`, `MEGACO_REENTRANT_FLEX_SCANNER`); they say nothing about whether a `.so` exists or a port is running.
- The erlc flags for these constants are passed by the flex Makefile, but the doc-build/specs path does not pass them, so `megaco_flex_scanner.erl` defaults both macros to `true` when undefined.
- Error text from the scanner is returned through a fixed 512-byte `error_msg` buffer in the driver data — long scanner error messages are truncated.

## Notes for Agents
- Do not expect `--enable/--disable-megaco-flex-scanner` to exist: `configure` enables the scanner iff `$LEX = flex` (real flex only, `lex` disables it); the only scanner-specific options are `--enable-megaco-flex-scanner-lineno` and `--enable-megaco-reentrant-flex-scanner`.
- To inspect the actual configuration of the current tree run `make info` here (delegates to `$(TARGET)/Makefile info`, printing all `ENABLE_*`, `CFLAGS`, `STD_LEX_FLAGS`, `MT_LEX_FLAGS`), or read the generated `src/flex/$(TARGET)/Makefile`.
- To tell which scanner is in use at runtime, ask the app: `megaco:system_info(text_config)` returning `[]` means the Erlang scanner, `[{flex, _}]` means this driver.
- `megaco_flex_scanner_drv.flex.src` is ~2000 lines of mostly generated-looking token term-spec building; only edit it when changing scanning semantics, and re-check `MEGACO_LINENO` vs `MEGACO_TOKENCNT` and the reentrant/non-reentrant macro pairs which appear throughout.

## Dependencies
- `flex` (the real GNU flex; `lex` is explicitly rejected) and `perl` (configure fails with "Perl is required to build the flex scanner!" if absent).
- A C compiler and `erts/` headers (`erl_driver.h`); the driver links against nothing else (no `-lfl` — `%option noyywrap`).

## Test Strategy
- `../test/megaco_flex_SUITE.erl` exercises scanner start/stop/scan; its `init_per_suite` skips with `?SKIP(flex_scanner_not_enabled)` when `megaco_flex_scanner:is_enabled()` is false, so it is a no-op in builds without flex.
- `../test/megaco_codec_flex_lib.erl` is the helper the codec suites use to start the scanner and skip gracefully when the driver cannot be loaded; `../test/Makefile` has `*_flex_prof*` profiling targets.
- Flex paths are also exercised from `../../examples/meas/` (`megaco_codec_meas.erl`, `megaco_codec_mstone_lib.erl`).
- All of these require the shared library to be built **and** `make setup` to have been run.

## See Also
- `../text/` — the pure-Erlang scanner (`megaco_text_scanner.erl`) and the parsers consuming the token lists.
- `../engine/megaco_config.erl` — `init_scanner/0` decides between Erlang and flex scanner and stores the encoding config.
- `../../Makefile` / `../../configure.ac` — app-level `CONFIGURE_OPTS`, `FLEX_SCANNER_REENTRANT`/`FLEX_SCANNER_LINENO`, and the flex detection macros.
- `../../doc/guides/megaco_encode.md` ("Configuration of text encoding module(s)") — user-facing description of the two scanners and the encoding config.
