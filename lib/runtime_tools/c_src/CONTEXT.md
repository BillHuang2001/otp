# runtime_tools C sources (`lib/runtime_tools/c_src`)

## Intent
Native code shipped by the `runtime_tools` application: one NIF library (dynamic tracing) and two erlang drivers (trace output sinks).
All three are built into `lib/runtime_tools/priv/` (build output) and consumed by Erlang modules in `../src/`.

## API Surface
Each `*.c` here is a self-contained shared library; there is no cross-file linking between them (`TRACE_LIBNAME = dyntrace trace_file_drv trace_ip_drv`, each compiled and linked independently).

| Source | Artifact (`../priv/lib/$(TARGET)/`) | Load mechanism | Erlang consumer |
| --- | --- | --- | --- |
| `dyntrace.c` (739 ln) | `dyntrace.so` / `.dll` | NIF — `ERL_NIF_INIT(dyntrace, nif_funcs, load, …)`; loaded in `on_load/0` via `erlang:load_nif(priv/lib/dyntrace, 0)` | `../src/dyntrace.erl` (NIF stubs `available/0`, `user_trace_s1/1`, `user_trace_i4s4/9`, `user_trace_n/10`, plus lttng-only `trace_*`/`enabled_*` NIFs) |
| `trace_file_drv.c` (709 ln) | `trace_file_drv.so` / `.dll` | erlang driver — `DRIVER_INIT(trace_file_drv)`, driver name `"trace_file_drv"` | `../src/dbg.erl` (`trace_port1/3`, ~line 1346): `erl_ddll:load_driver(priv/lib, "trace_file_drv")` then `open_port({spawn, "trace_file_drv <opts> n <AbsName>"}, [eof])` |
| `trace_ip_drv.c` (936 ln) | `trace_ip_drv.so` / `.dll` | erlang driver — `DRIVER_INIT(trace_ip_drv)`, driver name `"trace_ip_drv"` | `../src/dbg.erl` (`trace_port(ip, {Portno,Qsiz})`, ~line 1321): same `erl_ddll:load_driver` + `open_port({spawn, "trace_ip_drv <Portno> <Qsiz> 2"}, [eof])` |
| `dyntrace_lttng.h` (369 ln) | — (header only) | lttng `TRACEPOINT_EVENT` definitions for provider `org_erlang_dyntrace` | compiled only into `dyntrace.c`; runtime consumer is `lttng` tooling (`../test/dyntrace_lttng_SUITE.erl`) |
| `dtrace_user.d` (36 ln) | — (not built) | DTrace provider stub | not referenced by any build rule; kept only as a placeholder (the probe set moved to `erts/emulator/beam/erlang_dtrace.d`) |

Both drivers speak a fixed byte protocol with Erlang (documented in the file headers): driver→Erlang `'\0'` = ok, `'\1' ++ String` = `{error, Atom}`; trace records are `Op(1 byte) || Size(NBO 32-bit) || external term`.
`trace_file_drv` additionally handles port control message `'f'` (flush) and supports wrapping multi-file logs (`w <WrapSize> <WrapCnt> <TailIndex> ... n <Filename>` open-string form).

## Build & configure relationship
- `Makefile` in this directory is a small wrapper: it `include`s `$(ERL_TOP)/make/run_make.mk`, which recurses into `$(TARGET)/Makefile` (e.g. `x86_64-pc-linux-gnu/Makefile`) with `TYPE=opt|debug|valgrind`.
- `Makefile.in` is the autoconf input; the generated file is `c_src/$(TARGET)/Makefile` — **not** `c_src/Makefile`.
- `lib/runtime_tools/` has **no** `configure`/`configure.ac` of its own; `erts/configure.ac` contains `AC_CONFIG_FILES([../lib/runtime_tools/c_src/$host/Makefile:../lib/runtime_tools/c_src/Makefile.in])` (explicitly marked "should be moved to their respective lib").
  So `@LIBS@`, `@DEFS@`, `@DEBUG_FLAGS@`, `@ERTS_CONFIG_H_IDIR@` are substituted by the **erts** configure, and this directory can only be built after `erts/` (and `make/`, which generates `make/$(TARGET)/otp_ded.mk`) have been configured.
- Uses the "direct emulator dependency" macro set from `make/$(TARGET)/otp_ded.mk` (`DED_CC`, `DED_CFLAGS`, `DED_LD`, `DED_LIBS`, `DED_EXT`, `DED_SYS_INCLUDE`) rather than the ordinary app C flags — same pattern as `lib/public_key/c_src`, `lib/asn1/c_src`, `lib/wx/c_src`.
- Include path (`ALL_CFLAGS`) = `@DEFS@ @ERTS_CONFIG_H_IDIR@` (`erts/$host`, i.e. `config.h`) `+ $(DED_SYS_INCLUDE)` (`erts/emulator/beam`, `erts/include`, `erts/include/*`, `erts/emulator/sys/*`) `+ -I$(OBJDIR) -I$(ERL_TOP)/erts/emulator/$(TARGET)`.
- Compile rule declares `dyntrace_lttng.h` as a prerequisite of **every** object, including the two drivers that never include it (blanket rebuild trigger).
- `TYPE` variants produce a suffix on the file name and on objects: `TYPE=` → none, `TYPE=debug` → `.debug` (adds `-DDEBUG @DEBUG_FLAGS@`), `TYPE=valgrind` → `.valgrind` (adds `-DVALGRIND`); both strip `-O2`/`-O3` from `CFLAGS`.
- Object dir `../priv/obj/$(TARGET)/`; library dir `../priv/lib/$(TARGET)/`; both created at makefile-parse time via `_create_dirs := $(shell mkdir -p …)`.
- `release_spec: opt` installs the trace libs flat into `$(RELEASE_PATH)/lib/runtime_tools-$(VSN)/priv/lib` (no `$(TARGET)` subdir) — matching the loaders' fallback lookup `priv/lib/<name>` then `priv/lib/<system_architecture>`.

## Constraints
- Never edit a generated `$(TARGET)/Makefile`; edit `Makefile.in`.
- `../priv/` is pure build output (only an empty `.gitignore` is tracked) — do not add source files there.
- Do not reorder or rename `TRACE_LIBNAME` entries; the Erlang side loads them by literal file/driver name `dyntrace`, `trace_file_drv`, `trace_ip_drv`.
- Keep `dtrace_user.d`'s provider declared as `erlang` and in sync with `erts/emulator/beam/erlang_dtrace.d` if it is ever revived; today it is inert.

## Dependencies (external, beyond the compiler)
- `dyntrace.c` reads `config.h` from `erts/$host` and branches on `USE_DYNAMIC_TRACE`, `USE_DTRACE`, `USE_SYSTEMTAP`, `USE_LTTNG` — i.e. on `--with-dynamic-trace=dtrace|systemtap|lttng` (default `no`).
  - Without one of those flags the NIF **still builds**, but as a stub: `available/0` returns `false` and `user_trace_*` return `error`; all lttng NIFs are compiled out.
  - DTrace/SystemTap variants call `erl_nif_user_trace_*` from `erts/emulator/beam/dtrace-wrapper.c` internals.
  - LTTng variant requires `lttng/tracepoint.h` + `lttng/tracepoint-event.h` and lttng-ust >= 2.7 (checked at configure time); `LIBS` picks up `-llttng-ust -ldl` (or `-ldtrace` / `-lelf` for dtrace) from `@LIBS@`.
- Reaches into erts internals: `sys.h` (both `dyntrace.c` and `trace_ip_drv.c` for `SET_NONBLOCKING`), `dtrace-wrapper.h`, plus `erl_nif.h` / `erl_driver.h` from `erts/emulator/beam`.
- `trace_ip_drv.c` uses Winsock2 on `__WIN32__` and POSIX sockets elsewhere; `trace_file_drv.c`-family code maps `_write`/`_close`/`_unlink` on Windows.
- `HAVE_CONFIG_H` must be defined (comes from `@DEFS@`) for `trace_file_drv.c` / `trace_ip_drv.c` to pick up `config.h`.

## Known Issues
- `dtrace_user.d` is dead weight: no makefile rule references it, and no probe is generated from it (the comment inside points at `erts/emulator/beam/erlang_dtrace.d`).
- `trace_file_drv`/`trace_ip_drv` derive `MAXPATHLEN` by an `#ifdef` cascade ending in `#error` — a platform lacking all of `PATH_MAX`/`_POSIX_PATH_MAX`/`MAXPATH`/`MAX_PATH`/`_MAX_PATH` will not compile.

## Test Strategy
- `../test/dyntrace_SUITE.erl` exercises the DTrace/SystemTap NIF (skipped unless the runtime was built with that framework) and `../test/dyntrace_lttng_SUITE.erl` the LTTng tracepoints; both are listed in `../test/Makefile`'s `MODULES`.
- `../test/dbg_SUITE.erl` covers the file/IP trace drivers through `dbg:trace_port/2`.
- `TEST_NEEDS_RELEASE=true` in `../Makefile`, so these suites run against an installed release (where the libs live in `priv/lib`).

## Notes for Agents
- The C sources here are versioned by `../vsn.mk` (`RUNTIME_TOOLS_VSN`), read by `Makefile.in` as `VSN`.
- The lttng tracepoint names exposed to users are `org_erlang_dyntrace:<event>` (dotted/underscored), see `dyntrace_lttng.h`.
- `../src/dyntrace.erl` has a `-nifs()` attribute and matching stub functions for every NIF in `nif_funcs` — keep both in sync when touching `dyntrace.c`.

## See Also
- `../src/` — Erlang consumers (`dbg.erl`, `dyntrace.erl`) and the app resource.
- `../test/` — `dyntrace_SUITE`, `dyntrace_lttng_SUITE`, `dbg_SUITE`.
- `../../erts/emulator/beam/dtrace-wrapper.h`, `erlang_dtrace.d`, `sys.h` — emulator side of the same tracing feature.
- `../../make/otp_ded.mk.in` and `../../make/autoconf/otp.m4` — the `DED_*` variables used to build these libs.
