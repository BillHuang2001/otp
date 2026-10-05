# os_mon — Erlang Sources

## Intent
Erlang sources of the `os_mon` application: the OS monitoring processes that observe CPU load/utilization, memory usage, disk space and OS system messages.
`os_mon` is an application (started as `os_mon_sup`) whose child monitors are gen_servers, most of which talk to a C or shell helper port program built from `../c_src/`.

## API Surface (modules)
Public monitor API (called cross-node via `os_mon:call/3`):
- `cpu_sup` — CPU load & utilization supervisor (Unix). `start_link/0`, `start/0`, `stop/0`, `nprocs/0`, `avg1/0`, `avg5/0`, `avg15/0`, `util/0`, `util/1`.
- `memsup` — memory supervisor (Unix + Windows). `get_memory_data/0`, `get_system_memory_data/0`, `get_os_wordsize/0`, watermark/interval/helper-timeout getters+setters (`get_check_interval/0`, `set_check_interval/1`, `get_procmem_high_watermark/0`, …, `get_helper_timeout/0`).
- `disksup` — disk-space supervisor (Unix + Windows). `get_disk_data/0`, `get_disk_info/0`, `get_disk_info/1`, `get_check_interval/0`, `set_check_interval/1`, `get_almost_full_threshold/0`, `set_almost_full_threshold/1`.
- `os_sup` — OS system-message (syslog/eventlog) interface. `start_link/1`, `start/0`, `stop/0`, `error_report/2`, `enable/0`, `enable/2`, `disable/0`, `disable/2`.
- `nteventlog` — Windows event-log backend used by `os_sup`; also usable standalone. `start_link/2`, `start/2`, `stop/0`.
- `os_mon_sysinfo` — gen_server wrapping the `win32sysinfo.exe` port (Windows only). `get_disk_info/0`, `get_disk_info/1`, `get_mem_info/0`.
Application/supervision plumbing (not a monitor):
- `os_mon` — the `application` **and** `supervisor` behaviour module.
- `os_mon_mib` — removed module (OTP 22.0), retained only for its `-removed/1` warning attribute; contains no code.

Internal/other exports:
- `format_status/1` — the OTP gen_server status callback (map-based), exported by `memsup` and `disksup`.
- `ping/0` (`cpu_sup`), `parse_df/2` (`disksup`) — exported for tests only.

## Contract: `dummy_reply/1`, `param_type/2`, `param_default/1`
Every monitor module that a caller may reach via `os_mon:call/3` must export `dummy_reply/1`.
- `os_mon:call(Service, Request, Timeout)` calls `gen_server:call(server_name(Service), …)`; on `exit:{noproc,_}` it will try `startp/1` once, and if the service is genuinely unavailable it logs an `error_logger` warning and returns `Service:dummy_reply(Request)` instead of crashing (default timeout 5000 ms).
- Configured monitors (`memsup`, `disksup`, `os_sup`) also export `param_type/2` and `param_default/1`.
- `os_mon:get_env(Service, Param)` reads `application:get_env(os_mon, Param)`, validates with `Service:param_type(Param, Value)`, and falls back to `Service:param_default(Param)` (with a warning) when the value is invalid or unset.
- `cpu_sup` has no configuration parameters, so it exports only `dummy_reply/1`.
- `param_default/1` values match the defaults documented in each module's `-moduledoc` (e.g. `memsup` `memory_check_interval=1`, `system_memory_high_watermark=0.80`, `process_memory_high_watermark=0.05`, `memsup_helper_timeout=30`, `memsup_system_only=false`; `disksup` `disk_space_check_interval=30`, `disk_almost_full_threshold=0.80`, `disksup_posix_only=false`; `os_sup` `os_sup_errortag=std_error`, `os_sup_own="/etc"`, `os_sup_syslogconf="/etc/syslog.conf"`, `os_sup_enable=true`, `os_sup_mfa={os_sup,error_report,[std_error]}`).

## Port-program protocol (Erlang ⇄ helpers in `../c_src/`)
`os_mon:open_port(Name, Opts)` finds the helper at `priv/bin/<Name>`, falling back to `priv/bin/<Arch>/<Name>` (Arch = `win32` or `system_info(system_architecture)`).
- `cpu_sup` — `open_port("cpu_sup", [stream])`; single-byte commands: `n` (nprocs), `1`/`5`/`f` (avg1/5/15), `q` (quit), `p` (ping), `u` (util). Replies are big-endian uint32 (`?INT32`); `util` replies with (num_cpus, entries_per_cpu, then tagged value pairs keyed by `?cu_*`: cpu_id, user, nice_user, kernel, io_wait, idle, hard_irq, soft_irq, steal); `ping` must return 4711 or init fails.
- `memsup` — `open_port("memsup", [{packet,1}])`; request/response bytes are defined in `../include/memsup.hrl` (`?SHOW_MEM`, `?SHOW_SYSTEM_MEM`, `?EXIT`, `?SHOW_SYSTEM_MEM_END` and the `?MEM_*`/`?SWAP_*` extended-stat tags). Basic memory replies are hex strings decoded via `list_to_integer(_, 16)`; extended stats are streamed as one tag byte + value until the `?SHOW_SYSTEM_MEM_END` terminator. `?MEM_*` tags are mapped to public atoms through a `persistent_term` table built in `init/1`.
- `os_mon_sysinfo` — `open_port("win32sysinfo.exe", [{packet,1}])`; handshake expects `?OK` (`$o`) within 5 s; commands are `?DISK_INFO` (`$d`) and `?MEM_INFO` (`$m`); data is read until a trailing `?OK`.
- `nteventlog` — `open_port({spawn,"\"…/bin/nteventlog.exe\" <Identifier>"}, [{packet,2}])`; each received log record is applied to the callback MFA and acknowledged by sending `"A"`. Records are Hollerith-encoded strings decoded by `parse_log/1` into `{Time,Category,Facility,Severity,Message}`.
- `os_sup` (Solaris) — `open_port({spawn,"\"…/bin/ferrule\" <OwnPath>"}, [{packet,2}])`; `terminate/2` sends `"only_stdin"` and calls `disable/2`. `enable/2`/`disable/2` shell out via `os:cmd` to `priv/bin/mod_syslog otp|nootp Dir Conf` (= the `mod_syslog` helper) and expect the literal `"0"`.
- `disksup` — no C helper on Unix: spawns a shell `open_port({spawn, "sh -s disksup 2>&1"}, [stream])` and runs per-OS `df` command lines; on Windows it has no port (`not_used`) and instead calls `os_mon_sysinfo:get_disk_info/0,1`.

## Platform gating
`os_mon:init/1` builds child specs from `startp/1`, which combines `services(os:type())` with the `start_cpu_sup`/`start_disksup`/`start_memsup`/`start_os_sup` env flags.
- `services({unix,sunos})` → `[cpu_sup,disksup,memsup,os_sup]`; `services({unix,_})` → `[cpu_sup,disksup,memsup]`; `services({win32,_})` → `[disksup,memsup,os_sup,sysinfo]`.
- Supervisor flags: `one_for_one` with `{5,3600}` on win32, `{4,3600}` otherwise.
- `cpu_sup`: Unix only. Utilization (`util/0,1`) only on sunos/linux/freebsd/openbsd/darwin; the port-backed measurement server runs on sunos/linux/darwin/freebsd/dragonfly/openbsd; on irix/irix64 the server is `not_used` and load is read from `uptime` + `/proc/pinfo`; other OS → `exit({unsupported_os,OS})`. On Linux load/nprocs prefer `/proc/loadavg`, falling back to the port.
- `memsup`: Unix + Windows. Port mode on darwin/linux/openbsd/netbsd/irix/irix64/sunos; non-port mode on freebsd/dragonfly (via `sysctl`/`/proc`) and win32 (`os_mon_sysinfo:get_mem_info/0`); other OS → `exit({unsupported_os,OS})`.
- `disksup`: Unix port mode on sunos4/solaris/freebsd/dragonfly/darwin/linux/posix/openbsd/netbsd/irix; win32 via `os_mon_sysinfo`; other OS → `exit({unsupported_os,OS})`. `disksup_posix_only=true` collapses any non-sunos Unix to `{unix,posix}` (plain `df -k -P`) for stripped-down systems.
- `os_sup`: Solaris only (`init/1` stops with `{unsupported_os,OS}` otherwise); on win32 `start_link/1` instead starts `nteventlog` registered as `os_sup_server`.
- `os_mon_sysinfo` and `nteventlog`: win32 only (`require` a win32 `os:type()` or they exit).
- `os_mon:server_name/1` maps the logical service to its registered name: `os_sup → os_sup_server`, `sysinfo → os_mon_sysinfo`, others to themselves.

## Constraints
- `memsup.erl` includes `"memsup.hrl"` (found via `-I../include`); `../include/memsup.hrl` must stay manually consistent with `../c_src/memsup.h` (value/tag numbers) — a mismatch corrupts the port protocol silently.
- All monitor gen_servers set `process_flag(trap_exit,true)` and `process_flag(priority,low)`; port death is reported as a child exit so the supervisor restarts the monitor.
- Built with `ERL_COMPILE_FLAGS += -I../include -Werror`; `MODULES = disksup memsup cpu_sup os_mon os_mon_mib os_sup os_mon_sysinfo nteventlog`.
- `ebin/os_mon.app` and `ebin/os_mon.appup` are generated by `sed` substitution of `%VSN%` from `os_mon.app.src` / `os_mon.appup.src` (which are hand-edited); `os_mon.appup` restarts the whole application for any version change.

## Notes for Agents
- `os_mon_mib.erl` is intentionally empty (removed OTP 22.0); do not "restore" it.
- `cpu_sup` keeps per-caller utilization state inside a dedicated spawned `measurement_server` process (monitors each client; runs `port_server_loop` over the port), not in the gen_server state — it is killed and respawned on measurement error.
- The helper executables (`cpu_sup`, `memsup`, `win32sysinfo.exe`, `nteventlog.exe`, `ferrule`, `mod_syslog`) are built from `../c_src/` into `priv/bin`; the fallback path is `priv/bin/<system_architecture>/<Name>`.
- `os_sup` shell commands (`enable`/`disable`) require root privileges and must be run outside the emulator when `os_sup_enable=false`.

## Dependencies
- Unix: system `df` (and `uptime` on irix) plus Linux `/proc/loadavg`, `/proc/meminfo`, `/proc/pinfo`.
- Windows: `win32sysinfo.exe` and `nteventlog.exe` helper binaries.
- Solaris `os_sup`: `syslogd`, a named pipe under `os_sup_own` (default `/etc`), and the `mod_syslog`/`ferrule` helpers.

## Test Strategy
- Suites live in `../test/`: `cpu_sup_SUITE`, `disksup_SUITE`, `memsup_SUITE`, `os_mon_SUITE`, `os_sup_SUITE`.
- `disksup:parse_df/2` and `cpu_sup:ping/0` are exported specifically to be driven by those suites.

## See Also
- `../include/` — `memsup.hrl` (shared port protocol constants); `../c_src/` — helper C programs; `../test/` (read-only); `../doc/` — documentation sources; parent `../CONTEXT.md`.
