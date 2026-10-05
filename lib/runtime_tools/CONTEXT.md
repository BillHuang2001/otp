# runtime_tools — OTP Application

## Intent
`runtime_tools` provides low-footprint tracing/debugging tools suitable for production systems, plus the node-side "backend" modules that GUI/CLI tools (`observer`, `etop`, `ttb`) call on the node being observed.
It is built by default (`ERLANG_APPLICATIONS` in `../Makefile`); version is `RUNTIME_TOOLS_VSN = 2.5` in `vsn.mk` (release-wide mapping: repo-root `otp_versions.table`).

## API Surface (public modules)
- `dbg` — text-based trace facility: front-end to `erlang:trace/*` BIFs and the tty tracer, with match-spec help (`fun2ms`), pattern save/load, sessions, and `trace_port(file|ip,…)`; it is also the loader/controller of the C trace drivers.
- `dyntrace` — interface to DTrace/SystemTap/LTTng user probes via the `dyntrace` NIF; every function is a no-op in a stock build.
- `instrument` — inspection of `erts_alloc` allocation/carrier statistics; explicitly unstable, "no compatibility guarantees".
- `msacc` — convenience layer over `erlang:statistics(microstate_accounting)`.
- `scheduler` — sampling and calculation of scheduler utilization on top of `erlang:statistics(scheduler_wall_time)`.
- `system_information` — gen_server that aggregates a system investigation report (`report/0`, `to_file/1`, `from_file/1`, `sanity_check/0`).
- `versions` — parsing/comparison helpers for the OTP version scheme, added in 2.5 (OTP 29.1).
- Internal backends (`-moduledoc false`): `observer_backend` (node backend for `observer`/`etop`/`ttb`, driven via `rpc`/`spawn_link`), `appmon_info` (observer's app-monitor information centre), `ttb_autostart` (resumes ttb tracing at startup), and the app/supervisor callbacks `runtime_tools` / `runtime_tools_sup`.
- `erts_alloc_config` — removed stub only (`-removed({'_','_',"… as of OTP 26.0 …"})`); kept so old releases referencing it still load.
- `include/observer_backend.hrl` — the only public header (`etop_info`/`etop_proc_info` records); re-exported by `../observer/include/etop.hrl`, so those records are a cross-app contract.
- `examples/` — `*.d` (DTrace) and `*.systemtap` probe scripts for dynamic tracing, installed to `doc/examples` in a release.

## Constraints
- The module list lives in `src/Makefile` (`MODULES`) and `src/runtime_tools.app.src` (`modules`); a new module must be added to **both**.
- `runtime_tools.app` / `runtime_tools.appup` are generated into `ebin/` by `sed` of `%VSN%` — never edit generated files.
- `applications` is only `[kernel, stdlib]`; `mnesia` appears only in `runtime_dependencies`, so it must be used conditionally (e.g. `observer_backend` guards on the mnesia `schema` table and returns `{error, "Mnesia is not running on: <node>"}`).
- Compilation is `-Werror`; modules deliberately using unsafe/deprecated functions (`binary_to_term/1`, `file:consult/1`, `os:cmd/1`, `erlang:exit/2`) must keep their `nowarn_possibly_unsafe_function` / `nowarn_deprecated_function` options.
- C code is built by the **erts** configure (there is no app-level `configure`/`configure.ac`); all dynamic-tracing C support is conditional on `--with-dynamic-trace=…` and is documented as experimental/unsupported by Erlang/OTP.
- `debug`/`valgrind` C builds add a `.debug`/`.valgrind` suffix to the library and object names.

## Dependencies
- Erlang: `kernel`, `stdlib` (runtime `applications`); `runtime_dependencies` = `stdlib-6.0`, `mnesia-4.12`, `kernel-10.0`, `erts-16.0`.
- C/build: erts internal headers (`erts/emulator/beam`, `sys.h`, `dtrace-wrapper.h`), plus optionally DTrace/SystemTap toolchain or `lttng-ust` ≥ 2.7 headers (only for `--with-dynamic-trace=dtrace|systemtap|lttng`).

## Build & Release
- `Makefile`: `SUB_DIRECTORIES = src c_src doc`, `VSN = $(RUNTIME_TOOLS_VSN)`, `DIA_PLT_APPS = mnesia`, `TEST_NEEDS_RELEASE = true`.
- `c_src/Makefile` is a short wrapper recursing (via `make/run_make.mk`) into the configure-generated `c_src/$(TARGET)/Makefile` derived from `c_src/Makefile.in`; libraries land in `priv/lib/$(TARGET)/` and are installed to `$(RELEASE_PATH)/lib/runtime_tools-$(VSN)/priv/lib`.

## Cross-App Consumers (compatibility contract)
- `../observer/src/observer_*_wx.erl`, `etop.erl`, `ttb.erl` → `observer_backend`; `observer_app_wx.erl` → `appmon_info`; `ttb.erl` → `ttb_autostart`.
- `../observer/include/etop.hrl` → `runtime_tools/include/observer_backend.hrl`.
- `msacc` has no in-OTP consumers besides tests elsewhere (`tools/test/tprof_SUITE`, `ssl/test/ssl_dist_bench_SUITE`).

## Notes for Agents
- `ebin/` and `priv/` are build output (only their `.gitignore` files are tracked); `priv/lib/` holds the C libraries, `priv/obj/` the objects.
- Probe/library name strings (`user_trace_i4s4`, `dyntrace`, `trace_file_drv`, `trace_ip_drv`) are hard-coded in `src/` and `c_src/` — grep both before renaming anything.

## Test Strategy
- See `test/` — `make test` in this directory requires a built **and installed** release (`TEST_NEEDS_RELEASE=true`); suites are `test/*_SUITE.erl` plus fixtures under `test/*_SUITE_data/`.
- `dyntrace_SUITE` / `dyntrace_lttng_SUITE` skip themselves unless the emulator was configured with the matching dynamic-trace backend.

## Routing Table
| Path | Contents |
| --- | --- |
| `src/` | The 13 Erlang modules, `runtime_tools.app.src`, `runtime_tools.appup.src` and `src/Makefile` |
| `c_src/` | C sources for the `dyntrace` NIF and the `trace_file_drv`/`trace_ip_drv` erlang drivers, plus `Makefile.in`/`Makefile` |
| `test/` | Common Test suites (`*_SUITE.erl`), fixtures, `runtime_tools.spec`, `runtime_tools.cover`, `test/Makefile` |
| `doc/` | `docs.exs`, `doc/Makefile`, `notes.md` (release notes), `runtime_tools_app.md`, `guides/` (`dbg_guide.md`, `lttng.md`, `dtrace.md`, `systemtap.md`) |
| `include/` | One public header, `observer_backend.hrl` (`etop_info`/`etop_proc_info` records) — no own CONTEXT.md |
| `examples/` | DTrace (`.d`) / SystemTap (`.systemtap`) tracing example scripts for dynamic tracing — no own CONTEXT.md |
| `priv/` | Build output only (`.gitignore`); receives `lib/$(TARGET)/` and `obj/$(TARGET)/` — no own CONTEXT.md |
| `ebin/` | Build output only (`.gitignore`): beams and generated `.app`/`.appup` — no own CONTEXT.md |
| `../observer/` | Sibling consumer of `observer_backend`/`appmon_info` (read-only — escalate changes to the parent) |

## See Also
- `../CONTEXT.md` — OTP library application conventions (app layout, `vsn.mk`, Common Test layout).
- `../observer/` — the main client of this app's internal backend modules.
- `../erts/` — home of the erts-allocation internals inspected by `instrument` and of the dynamic-tracing C wrappers.
