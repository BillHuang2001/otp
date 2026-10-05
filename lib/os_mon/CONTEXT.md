# os_mon — OS Monitoring Application

## Intent
OS_Mon: OS-level monitoring for Erlang/OTP — supervises CPU load/utilization, memory usage, disk space and OS system messages (syslog / Windows event log).
It is a regular OTP application (`{mod, {os_mon, []}}`, supervisor `os_mon_sup`) that is OS-heavy and platform-conditional: several of its services exist only on some operating systems.
Its native code lives in `c_src/` (port programs spawned by the Erlang monitors); the app builds with `include`, `src`, `c_src`, `doc` as its subdirectories (`Makefile`: `SUB_DIRECTORIES = src c_src doc`).

## API Surface
- Public API modules (one per monitored resource, all reachable cross-node via `os_mon:call/3`): `m:cpu_sup`, `m:memsup`, `m:disksup`, `m:os_sup`, plus `m:nteventlog` (Windows event log, also usable standalone) and the internal `os_mon_sysinfo`.
- Application configuration (`os_mon` app env in `src/os_mon.app.src`): `start_cpu_sup = bool()` (default `true`), `start_disksup = bool()` (default `true`), `start_memsup = bool()` (default `true`), `start_os_sup = bool()` (default `false`).
- Per-service configuration (validated by each module's `param_type/2` + `param_default/1`): `memsup` `memory_check_interval`, `system_memory_high_watermark`, `process_memory_high_watermark`, `memsup_helper_timeout`, `memsup_system_only`; `disksup` `disk_space_check_interval`, `disk_almost_full_threshold`, `disksup_posix_only`; `os_sup` `os_sup_mfa`, `os_sup_errortag`, `os_sup_enable`, `os_sup_own`, `os_sup_syslogconf`.
- Alarms are reported through the SASL alarm handler: `{system_memory_high_watermark, []}`, `{process_memory_high_watermark, Pid}` (memsup) and `{{disk_almost_full, MountedOn}, []}` (disksup).
- `src/os_mon.app.src` lists modules `os_mon, os_mon_mib, os_sup, disksup, memsup, cpu_sup, os_mon_sysinfo, nteventlog`; `os_mon_mib` is a removed shim (OTP 22.0) kept only for its `-removed/1` attribute.
- `os_mon.appup.src` restarts the whole application for any version change; `os_mon.app`/`os_mon.appup` are generated into `ebin/` by `src/Makefile` substituting `%VSN%`.
- Version: `OS_MON_VSN` in `vsn.mk` (2.12); repo-root `otp_versions.table` maps OTP releases to app versions.

## Constraints
- Platform availability is authoritative in `src/os_mon.erl:services/1`: `{unix,sunos}` → cpu_sup, disksup, memsup, os_sup; other Unix → cpu_sup, disksup, memsup; `{win32,_}` → disksup, memsup, os_sup, sysinfo.
- Unavailable services must not crash callers: `os_mon:call/3` catches `noproc`, logs an `error_logger` warning and returns `Service:dummy_reply(Request)` — so every monitor module must export `dummy_reply/1` (and `param_type/2`/`param_default/1` if configurable).
- Each monitor is a gen_server with `trap_exit` + low priority; its helper port program lives in `priv/bin` (built from `c_src/`).
- The port-protocol constants in `include/memsup.hrl` must be kept manually in sync with `c_src/memsup.h` — there is no code generation and a mismatch silently corrupts the protocol.
- `lib/os_mon` has no `configure`/`configure.ac` of its own; the per-arch `c_src/$(TARGET)/Makefile` is generated from `c_src/Makefile.in` by the **erts** configure (`erts/configure.ac`). Never edit generated files.
- Edit `*.app.src`/`*.appup.src` and `vsn.mk`, never the generated `ebin/*.app*`.

## Dependencies
- Erlang apps: `kernel`, `stdlib`, `sasl` (`src/os_mon.app.src`); `runtime_dependencies` pin `erts`/`kernel`/`stdlib`/`sasl`.
- Unix: system tools `df` (and `uptime` on irix), Linux `/proc` (`loadavg`, `meminfo`, `pinfo`), libs `kstat` (Solaris) / `kvm` (BSD) detected by erts configure for `cpu_sup`; Solaris `os_sup` additionally needs `syslogd` and a named pipe under `os_sup_own`.
- Windows: `win32sysinfo.exe` and `nteventlog.exe` port programs.
- Build-time: C compiler configured by the top-level/erts configure (`@CC@`, `@LIBS@`, `@CPU_SUP_LIBS@`, `@os_mon_programs@`).

## Test Strategy
- Common Test suites in `test/` (`cpu_sup_SUITE`, `disksup_SUITE`, `memsup_SUITE`, `os_sup_SUITE`, `os_mon_SUITE`) run via `make test` / `make release_tests` conventions; `make test` in the app dir builds and runs them in place.
- Suites gate their case lists on `os:type()`, so a skipped or missing case usually means "unsupported platform", not a bug; `os_sup` cases and `os_mon_SUITE:config` effectively only run on Solaris, and `os_sup_SUITE:config` cannot run without root.
- The C port programs have no direct unit tests — they are exercised indirectly by the suites.

## Notes for Agents
- `ebin/` is build output (only `.gitignore` tracked); `priv/mibs/`, `priv/bin/` and `priv/obj/` contain only `.gitignore` in git — helper binaries and objects are produced by the build, and `priv/mibs` is a leftover placeholder from the removed os_mon SNMP MIB support.
- `c_src/win32/`, `priv/bin/win32/` and `priv/obj/win32/` are git-ignored build/legacy dirs (repo-root `.gitignore`).
- On Windows the build directory is `win32`; on Unix it is the autoconf host triplet (e.g. `x86_64-pc-linux-gnu`).
- `src/os_mon_mib.erl` is intentionally empty (removed in OTP 22.0) — do not "restore" it.

## Routing Table
- `src/` → Erlang sources: the monitor modules (`cpu_sup`, `memsup`, `disksup`, `os_sup`, `nteventlog`, `os_mon_sysinfo`), app/supervisor plumbing (`os_mon`), the removed `os_mon_mib` shim, `os_mon.app.src`/`os_mon.appup.src`, and `src/Makefile`
- `c_src/` → OS-specific C port programs (`cpu_sup.c`, `memsup.c`, `ferrule.c`, `mod_syslog.c`, `win32sysinfo.c`) and their per-arch build (`Makefile` wrapper + `Makefile.in`); `c_src/nteventlog/` → Windows event-log program sources
- `test/` → Common Test suites, `os_mon_conf.erl` helper and `os_mon.spec`/`os_mon_smoke.spec`/`os_mon.cover` (read-only for other areas; escalate writes here)
- `include/` → `memsup.hrl`, the public constants of the memsup port protocol (must match `c_src/memsup.h`)
- `doc/` → ExDoc sources: `os_mon_app.md` (application overview page), `notes.md` (release notes), `docs.exs`, `doc/Makefile`
- `priv/` → runtime data: `priv/bin/` (helper executables), `priv/obj/` (C objects), `priv/mibs/` (empty placeholder); all build outputs
- `ebin/` → build output (generated beams and `os_mon.app`/`os_mon.appup`) — never edited by hand
- `vsn.mk`, `Makefile`, `AUTHORS` → app version, build coordination, authors

## See Also
- `../CONTEXT.md` for the OTP application list and build conventions; `../../erts/configure.ac` for the `os_mon_programs`/`CPU_SUP_LIBS` logic that generates the C build.
