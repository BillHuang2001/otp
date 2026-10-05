# runtime_tools/src — Runtime Tools Application Sources

## Intent
Erlang sources of the `runtime_tools` OTP application (app version `RUNTIME_TOOLS_VSN = 2.5`, from `../vsn.mk`).
Collects the "runtime inspection / tracing" tools that sit on top of runtime BIFs (`erlang:trace/*`, `erlang:statistics/*`, `erlang:system_info/*`, `erts_alloc` internals) and the backends that GUI/front-end tools in other applications (`observer`) drive over RPC.
Most modules are public, documented API; the `*_backend`/`*_info`/plumbing modules are internal (`-moduledoc false`) and consumed only from other OTP applications.

## API Surface
Build inputs/outputs:
- `runtime_tools.app.src` → `../ebin/runtime_tools.app` (Makefile `sed`-substitutes `%VSN%`); declares 13 modules, `registered = [runtime_tools_sup]`, `applications = [kernel, stdlib]`, `mod = {runtime_tools, []}` and `runtime_dependencies = ["stdlib-6.0","mnesia-4.12","kernel-10.0","erts-16.0"]`.
- `runtime_tools.appup.src` → `../ebin/runtime_tools.appup`; instructs `{restart_application, runtime_tools}` for every `<<".*">>` upgrade/downgrade, i.e. the app is always fully restarted.
- `Makefile` — compiles `MODULES` into `$(EBIN)` (= `../ebin`) and generates the `.app`/`.appup`; installs `src/`, `../include/observer_backend.hrl` and `../examples/*` on release.

User-facing tracing / measurement tools (public, documented):
- `dbg` — "Text Based Trace Facility": front-end to `erlang:trace/3,4`, `trace_pattern/2,3` and `trace_port`; also the loader of the `trace_file_drv`/`trace_ip_drv` erlang drivers and the tty tracer (`dhandler/2`); exports `tp/tpl/tpe/ctp/ltp/dtp`-family, `trace_port/2`, `trace_client/2,3`, `fun2ms/1`, and opaque `session()` (isolated sessions via `session/2`).
- `dyntrace` — interface to dynamic (DTrace/systemtap/LTTng) user probes via the `dyntrace` NIF (`../c_src/dyntrace.c`); no-ops unless the emulator was built with `--with-dynamic-trace=...`; `available/0`, `p/0..8`, `pn/1..9`, `put_tag/1`/`get_tag/0`/`spread_tag/1`.
- `instrument` — allocation/carrier inspection of `erts_alloc` internals (`allocations/0,1`, `carriers/0,1`); explicitly carries NO compatibility guarantee across OTP versions (inspects runtime internals).
- `msacc` — convenience layer over `erlang:statistics(microstate_accounting)` (`start/0,1`, `stop/0`, `reset/0`, `stats/0,2`, `print/0..3`, `to_file/1`, `from_file/1`).
- `scheduler` — wrapper over `erlang:statistics(scheduler_wall_time)` (`sample/0`, `get_sample/0`, `sample_all/0`, `get_sample_all/0`, `utilization/1,2`); returns an opaque `sched_sample()`.
- `system_information` — gen_server that aggregates a system investigation report (`report/0`, `to_file/1`, `from_file/1`, `load_report/0,2`, `applications/0,1`, `application/1,2`, `environment/0,1`, `module/1,2`, `sanity_check/0`, `start/0`, `stop/0`).
- `versions` — helpers for the OTP version scheme (branch/compare/check/convert), types `vsn_string()`, `vsn_list()`, `vsn_branch_string()`; documented API, no internal consumers inside OTP.

GUI / etop backends (internal, `-moduledoc false`):
- `observer_backend` — backend for the `observer` GUI, the `etop` script and `ttb`, driven by `rpc`/`spawn_link` from the caller's node; `vsn/0` (used by `etop`/`ttb` for version checks), `sys_info/0`, `procs_info/1`, `fetch_stats/2`, `get_port_list/0`, `get_socket_list/0`, `socket_info/0`, `get_table_list/2`, `get_table/3`, `etop_collect/1`, and the `ttb_*` meta-tracer helpers (`ttb_init_node/3`, `ttb_write_trace_info/3`, `ttb_write_binary/2`, `ttb_stop/1`, `ttb_fetch/2,3`, `ttb_resume_trace/0`, `ttb_get_filenames/1`); includes `observer_backend.hrl`.
- `appmon_info` — legacy "appmon" information centre, still used by `observer` as its application/progress backend (`start_link/3`, `start_link2/3`, `app/4`, `app_ctrl/4`, `pinfo/4`, `load/4`, `status/0`); one gen_server per monitored node, designed to be started on the remote node.
- `erts_alloc_config` — stub module retained with `-removed({'_','_',"this module has as of OTP 26.0 been removed"})` so that calls fail with a clear error; still listed in `MODULES`/`.app.src`.
- `ttb_autostart` — gen_server (child of `runtime_tools_sup`) that reads the `ttb_autostart.bin` config on startup and lets ttb resume tracing; it is what `observer_backend`'s `autostart_module()`/`write_config` machinery talks to (customizable via the `runtime_tools` env `ttb_autostart_module`).

Application plumbing:
- `runtime_tools` — `application` callback (`start/2` links `runtime_tools_sup`, `stop/1`).
- `runtime_tools_sup` — top supervisor; single `temporary` child `ttb_autostart` (`one_for_one`, 0 restarts in 3600 s).

## Module → Role
| Module | Role | Audience |
| --- | --- | --- |
| `dbg` | text-based trace facility, trace drivers, tty tracer | public |
| `dyntrace` | dynamic-probe (DTrace/systemtap/LTTng) interface via NIF | public |
| `instrument` | erts_alloc allocation/carrier inspection | public, unstable |
| `msacc` | microstate-accounting convenience layer | public |
| `scheduler` | scheduler utilization wrapper | public |
| `system_information` | aggregated system investigation report gen_server | public |
| `versions` | OTP version-scheme helpers | public |
| `observer_backend` | backend for observer GUI / etop / ttb | internal |
| `appmon_info` | legacy appmon info centre used by observer | internal |
| `ttb_autostart` | resumes ttb tracing at app start | internal plumbing |
| `runtime_tools` / `runtime_tools_sup` | app + supervisor callbacks | internal plumbing |
| `erts_alloc_config` | removed stub (`-removed`) | none |

## External Consumers (who to keep compatible)
- `lib/observer/src/observer_*_wx.erl`, `observer_tv_table.erl`, `etop.erl`, `ttb.erl` → `observer_backend` (mostly via `rpc:call`/`spawn_link` on remote nodes; the module is fetched over the network, so its API must stay backward compatible).
- `lib/observer/src/observer_app_wx.erl` → `appmon_info:start_link/3`, `app/4`, `app_ctrl/4`.
- `lib/observer/include/etop.hrl` does `-include_lib("runtime_tools/include/observer_backend.hrl")`, so the `etop_info`/`etop_proc_info` records in `../include/` are a cross-app contract with `etop`/`observer`.
- `lib/observer/src/ttb.erl` also references the `ttb_autostart` module name/`ttb_autostart.bin` file documented for autostart resume.

## Constraints
- Do not hand-edit `../ebin/runtime_tools.app` or `.appup` — they are generated by the Makefile (`sed` of `%VSN%` from `../vsn.mk`).
- `../ebin/` is build output (only `ebin/.gitignore` is tracked).
- New modules must be added to both `MODULES` in `Makefile` and the `modules` list in `runtime_tools.app.src`.
- All modules compile with `-Werror +nowarn_deprecated_catch` and `-I../include`; `-Werror` makes warnings fatal, so `nowarn_*` compile attributes must be kept when using unsafe/deprecated functions.
- `mnesia` is NOT in `applications` (only `runtime_dependencies`); any use of `mnesia:*` must be tolerant of mnesia not being loaded/running.

## Known Issues & Gotchas
- Compile options deliberately allow "unsafe" calls, which reviewers/new code must keep aligned:
  - `dbg`: `nowarn_possibly_unsafe_function` for `erlang:binary_to_term/1` and `file:consult/1`, plus `nowarn_deprecated_function` for `erlang:exit/2`.
  - `system_information`: `nowarn_possibly_unsafe_function` for `erlang:list_to_atom/1` (converts app names), `file:consult/1`, and `nowarn_unsafe_function` for `os:cmd/1` (runs `otool -L`/`ldd` on loaded BEAM files).
  - `msacc`: `nowarn_possibly_unsafe_function` for `file:consult/1`.
  - `ttb_autostart`: `nowarn_possibly_unsafe_function` for `erlang:binary_to_term/1` (reads `ttb_autostart.bin`).
- `observer_backend` is uploaded/run on remote (possibly older/newer) nodes: keep existing exports/arity and result shapes; several observer call sites still fall back to "old version" behavior when a call is `undef`.
- `observer_backend` calls into `mnesia` (`table_info`, `select`, `system_info`) for mnesia tables; `get_table_list(mnesia, _)` throws `{error, "Mnesia is not running on: <node>"}` when the `schema` ETS table has no owner.
- `observer_backend` and `appmon_info` are large, security-sensitive modules that build atoms from remote names and inspect ETS/mnesia tables — they are loaded on the traced node and run with that node's full privileges.
- `dbg` trace drivers live in `code:priv_dir(runtime_tools)/lib` (with an arch subdir `win32`/`$TARGET` fallback) and are C erlang drivers in `../c_src/`; `trace_port/2` only supports the `file` (incl. wrap files) and `ip` drivers.
- `dyntrace` exports return "not available" no-ops in a stock build (no dynamic tracing) — never assume probes are active.
- `instrument` inspects emulator-internal structures (`GATHER_AHIST_FLAG_*` mirrors `erl_alloc_util.c`) — it may be wrong/silently degraded across OTP releases.
- `scheduler_wall_time` is auto-disabled when the process that enabled it dies, so `scheduler:utilization/2` needs the enabling process kept alive.
- `erts_alloc_config` is intentionally a stub that only raises a `removed` error; don't implement it here.

## Test Strategy
- Common Test suites live in `../test/` (`dbg_SUITE`, `dyntrace_SUITE`, `dyntrace_lttng_SUITE`, `instrument_SUITE`, `msacc_SUITE`, `observer_backend_SUITE`, `scheduler_SUITE`, `system_information_SUITE`, `versions_SUITE`, `runtime_tools_SUITE`, `zzz_SUITE`) with data dirs `dbg_SUITE_data/`, `system_information_SUITE_data/`, `versions_SUITE_data/`; run with `make test` in `../` (TEST_NEEDS_RELEASE — suites exercise remote nodes/dist).
- User-facing guides for `dbg`/`dyntrace` live in `../doc/guides/` (`dbg_guide.md`, `dtrace.md`, `systemtap.md`, `lttng.md`); module docs reference them via `e:runtime_tools:*`.

## See Also
- `../` (`lib/runtime_tools/`) — app Makefile, `vsn.mk`, `include/`, `c_src/` (trace drivers, `dyntrace.c`), `examples/` (`.d`/`.systemtap` probe scripts), `doc/`, `test/`.
- `../../observer/src/` — main consumer of `observer_backend`/`appmon_info` and home of `etop`/`ttb`.
- `../../erts/` — `erts_alloc`, trace BIFs and the dynamic-trace NIF/`efile_drv` machinery these modules wrap.
