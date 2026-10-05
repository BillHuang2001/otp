# snmp — Application Glue (`src/app`)

## Intent
Top-level OTP application glue for the `snmp` application: the public umbrella API (`snmp`), the `application` behaviour callback (`snmp_app`), the root supervisor (`snmp_app_sup`), the application resource / appup templates and an example node config.
One single application covers both the agent (`snmpa`) and manager (`snmpm`) sides; which side(s) run on a node is decided at start time from the application environment.

## API Surface
- `snmp.erl` — documented public umbrella API (`moduledoc` + `-export_type`s for `oid`, `varbind`, `pdu`, `verbosity`, …):
  - lifecycle: `start/0,1`, `stop/0`, `start_agent/0,1`, `start_manager/0,1`, `config/0`.
  - MIB / Audit-Trail-Log helpers: `read_mib/1`, `log_to_txt/5..8`, `log_to_io/4..7`, `change_log_size/2`.
  - version & time utilities: `versions1/0`, `versions2/0`, `print_version(s|_info)/*`, `date_and_time/0`, `universal_time_to_date_and_time/1`, `local_time_to_date_and_time_dst/1`, `date_and_time_to_universal_time_dst/1`, `validate_date_and_time/1,2`, `date_and_time_to_string/1,2`, `date_and_time_to_string2/1`, `sys_up_time/1`, `system_start_time/1`.
  - USM / misc: `passwd2localized_key/3`, `localize_key/3`, `octet_string_to_bits/1`, `bits_to_octet_string/1`, `str_apply/1` (`-doc false`, `erl -s snmp` entry).
  - tracing: `enable_trace/0`, `disable_trace/0`, `set_trace/1,2,3`, `reset_trace/1`.
- `snmp_app.erl` — `application` behaviour: `start/2`, `stop/0,1`, `config_change/3`, plus `start_agent/0,1,2` and `start_manager/0,1,2`. `start/2` starts `snmp_app_sup` then starts whichever of the agent/manager entities are configured.
- `snmp_app_sup.erl` — root `supervisor`: `start_link/0`, `start_agent/2`, `start_manager/2` (dynamic children `snmpa_supervisor` / `snmpm_supervisor`), `stop/0`; `init/1` declares **no** static children.
- `snmp_internal.hrl` — internal macros: `?APPLICATION`, a `rand`-seed helper, `?ip4/?ip6/?port` guard macros (copied from kernel's `inet_int.hrl`) and `?snmp_info/warning/error` logging macros. Compiled alongside `MODULES`.
- `snmp.app.src` — application resource template: `%VSN%` version placeholder, the full build-generated module list (compiler + app + agent + manager + misc), `registered = [snmp_app_sup]`, `mod = {snmp_app, []}`, `applications = [kernel, stdlib]`, and `runtime_dependencies` on asn1/stdlib/runtime_tools/mnesia/kernel/erts/crypto.
- `snmp.appup.src` — release-upgrade (and downgrade) instructions: `{restart_application, snmp}` for old 5.x/4.x versions.
- `snmp.config` — heavily commented example of the `{snmp, [snmp_app()]}` node config file: documents the full agent/manager option schema plus a sample configuration. Example/documentation only.
- `Makefile` / `modules.mk` / `depend.mk` — per-directory build: `MODULES = snmp snmp_app snmp_app_sup`, `HRLS = snmp_internal`.

## Constraints
- `snmp.app.src` / `snmp.appup.src` are templates: `%VSN%` is `sed`-substituted with `$(SNMP_VSN)` from `../../vsn.mk` into generated `snmp.app` / `snmp.appup` under `../../ebin` at build time — never hand-edit the generated `ebin/snmp.app*`.
- `snmp_internal.hrl` is internal (installed to `$(RELSYSDIR)/src/app`, not `include/`). The public headers `snmp_types.hrl` / `snmp_tables.hrl` live in `../../include/`.
- Modules compile with `-Werror` (unless `SNMP_WARNING_AS_ERROR=false`), `+warn_unused_vars`, `-Ddefault_verbosity=$(SNMP_DEFAULT_VERBOSITY)` (default `silence`), the `sys_pre_attributes` parse transform, `-I../misc`, `-Dversion="$(VSN)"`.
- `snmp.config` is not installed by `release_spec` (which installs `src/app` sources + headers, `ebin` beams/app/appup and `include/` headers).

## Design Decisions
- A single `snmp` application covers agent and manager: `snmp_app:entities/0` reads the `agent` and `manager` keys from the app env, so a node can run as agent, manager, or both.
- Backward compatibility: if no `agent`/`manager` env exists but the env list is non-empty, `snmp_app:entities/0` treats it as the old-style agent config and converts it via `snmpa_app:convert_config/1`; an empty env is a "skeleton start" with no entities.
- `snmp_app_sup` uses static `{one_for_one, 0, 1}` flags with no static children and adds `snmpa_supervisor` (default restart `permanent`) / `snmpm_supervisor` (default restart `transient`) dynamically, so a skeleton start simply yields an empty running supervisor.
- The umbrella `snmp` module contains no SNMP logic; each function delegates to the owning lower layer (`snmp_app`, `snmp_config`, `snmp_log`, `snmp_misc`) — it exists purely as the documented public interface.

## Notes for Agents
- `start_agent/1` and `start_manager/1` only work if the entity's env (`application:set_env(snmp, agent|manager, Conf)`) was set *after* application start — entity config cannot be part of the normal `sys.config`.
- `config_change/3` is deliberately a no-op: parameter changes are accepted but only take effect when the app is restarted.
- The application version is `SNMP_VSN` (defined in `../../vsn.mk`), injected into both the `.app` files and each module's `-Dversion`.
- The `snmp` app's `mod` is `{snmp_app, []}`; there is no per-directory `configure`.

## Routing Table
| File | Role |
| --- | --- |
| `snmp.erl` | Public umbrella API + documented types |
| `snmp_app.erl` | `application` callback; selects agent/manager entities |
| `snmp_app_sup.erl` | Root supervisor starting the agent/manager subtrees |
| `snmp_internal.hrl` | Internal macros (logging, IP/port guards, rand seed) |
| `snmp.app.src` / `snmp.appup.src` | Application resource & upgrade templates (`%VSN%`) |
| `snmp.config` | Example/documentation node config |
| `Makefile` / `modules.mk` / `depend.mk` | Per-directory build files |

- `../agent/` → agent implementation (`snmpa*`, started as the `snmpa_supervisor` child) — sibling, read-only
- `../manager/` → manager implementation (`snmpm*`, started as the `snmpm_supervisor` child) — sibling, read-only
- `../misc/` → shared helpers (`snmp_misc`, `snmp_config`, `snmp_log`, `snmp_pdus`, …) used by `snmp.erl` — sibling, read-only
- `../compile/` → MIB compiler modules (`snmpc*`) listed in `snmp.app.src` — sibling, read-only
- `../../include/` → public headers (`snmp_types.hrl`, `snmp_tables.hrl`)
- `../../vsn.mk` → `SNMP_VSN` / `APP_VSN` definitions
