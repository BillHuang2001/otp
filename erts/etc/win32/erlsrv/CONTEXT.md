# erts/etc/win32/erlsrv/ — Windows service wrapper (`erlsrv.exe`)

## Intent
C sources of **`erlsrv.exe`**, the Windows-only tool that runs an Erlang node as a native Windows (NT) service and doubles as the command-line manager for registering, configuring, and controlling such services.
It is the Windows counterpart of `../common/` (which holds the portable `erl`/`erlc`/`escript`/`heart`/`inet_gethost` sources); both trees are compiled by `../common/Makefile.in`.
Compiled/installed to `$(ERL_TOP)/bin/$(TARGET)/erlsrv.exe` and shipped (on Windows) as `erts-$(VSN)/bin/erlsrv.exe`.

## API Surface

### Process model (dispatch in `erlsrv_main.c` `wmain`)
- **No arguments** (how the SCM launches a service) → `service_main()` → `StartServiceCtrlDispatcherW` → `service_main_loop()` (service mode).
- **Any argument** → `interactive_main()` → the command-line manager below.

### Command-line manager (`erlsrv_interactive.c`)
Commands (`interactive_main`):
- `add <servicename> [options]` / `set <servicename> [options]` — register (add) or reconfigure (set) a service.
- `start <name>` / `start_disabled <name>` — start; `start_disabled` starts then disables so it does not auto-start next boot (used by `release_handler` upgrade).
- `stop <name>`, `enable <name>`, `disable <name>`, `remove <name>`, `rename <old> <new>`, `list [<name>]`, `help` (`?`, `/?`, `-?`, `h`, `H`).
- `readargs` — internal: read further args line-by-line from stdin (used to avoid cmd-line length limits).
Options for `add`/`set` (short forms in brackets, parsed by `lookup_arg`, case-insensitive, `-` or `/` prefix): `-stopaction|-st`, `-onfail|-on {reboot|restart|restart_always}`, `-machine|-m`, `-env|-e`, `-workdir|-w`, `-priority|-p {low|high|realtime}`, `-sname|-sn`, `-name|-n`, `-args|-ar`, `-debugtype|-d {new|reuse|console}`, `-comment|-c`, and internal `-internalservicename|-i`.
Full text: `erts/doc/references/erlsrv_cmd.md`.

### Windows Registry (config store) — `erlsrv_registry.c`
- Per-service config: `HKLM\SOFTWARE\Ericsson\Erlang\ErlSrv\1.1\<servicename>` (legacy `\1.0` also read/removed). Values (`reg_entries[]`, in index order): `StopAction` (REG_SZ), `OnFail` (REG_DWORD), `Machine` (REG_EXPAND_SZ), `Env` (REG_MULTI_SZ), `WorkDir` (REG_EXPAND_SZ), `Priority` (REG_DWORD), `SName` (REG_SZ), `Name` (REG_SZ), `Args` (REG_EXPAND_SZ), `DebugType` (REG_DWORD), `InternalServiceName` (REG_SZ), `Comment` (REG_SZ, non-mandatory).
- Event-log source registration: `HKLM\SYSTEM\CurrentControlSet\Services\EventLog\System\ErlSrv` with `EventMessageFile` and `TypesSupported` (`register_logkeys`).
- `OnFail` values: `ON_FAIL_IGNORE`=0, `ON_FAIL_RESTART`=1, `ON_FAIL_REBOOT`=2, `ON_FAIL_RESTART_ALWAYS`=3; `DebugType`: no-debug=0, new=1, reuse=2, console=3 (both in `erlsrv_registry.h`).

### Windows SCM integration (`erlsrv_interactive.c`, `erlsrv_service.c`)
- Service registered via `CreateServiceW`; **display name = user's servicename**, **internal service name = `<servicename><16-hex-timestamp>`** (`generate_real_service_name`) to avoid collisions and permit rename; the internal name is remembered in the `InternalServiceName` registry value. Service depends on `LanmanWorkstation`.
- Service status reported via `RegisterServiceCtrlHandlerW`/`SetServiceStatus` (START_PENDING/RUNNING/STOP_PENDING/STOPPED); accepts STOP|SHUTDOWN.
- A machine-global named semaphore `{468d6954-e355-415f-968f-d257cb0feef4}` (`ERLSRV_INTERACTIVE_GLOBAL_SEMAPHORE`) serialises concurrent interactive invocations against the shared registry.

### Runtime contract with the launched node
- The wrapper launches the emulator named by `Machine` (default `erl.exe`; see `ERLANG_MACHINE` in `erlsrv_global.h`) as a child process with `-nohup` plus `-sname <SName>` or `-name <Name>` (+ `-keep_window` for console debug).
- Env vars set for the child: `ERLSRV_SERVICE_NAME` (`SERVICE_ENV`), `ERLSRV_EXECUTABLE` (`EXECUTABLE_ENV`, quoted exe path). Consumed by `lib/sasl/src/erlsrv.erl`, `lib/sasl/src/release_handler.erl`, `../common/heart.c`, `../win32/port_entry.c`.
- Named Win32 event `ErlSrv_<servicename>` is passed to the emulator as `-service_event <event>`, handled by `erts/emulator/sys/win32/sys.c` (+ `erl_poll.c`); used to ask the emulator to terminate.
- If `StopAction` is set (or debug is on) the child's stdin is a pipe so erlsrv can feed an Erlang shell command to shut the node down cleanly; otherwise the node's stdhandles go to `NUL` (or a `.debug` file).
- The child is placed in a Job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK` so it dies with erlsrv.

### Logging
`erlsrv_logmess.mc` is a Windows message-catalog source defining `MSG_WARNING`=0x10, `MSG_ERROR`=0x11, `MSG_INFO`=0x12 (all `"<service>: <message>"`), compiled by `mc` to produce the event-log message DLL/resource; `log_warning`/`log_error`/`log_info` (`erlsrv_util.c`) call `ReportEventW` against source `APP_NAME` = `"ErlSrv"`.

## Source file roles
- `erlsrv_main.c` — `wmain` dispatch (service vs interactive).
- `erlsrv_global.h` — constants: `APP_NAME L"ErlSrv"`, `ERLANG_MACHINE L"erl.exe"`, env-var names, debug macros.
- `erlsrv_interactive.c` / `.h` — CLI/SCM manager (largest file, ~1330 lines): arg parsing, install/remove/rename/list/start/stop/enable/disable, semaphore lock, UTF-8 stdio.
- `erlsrv_service.c` / `.h` — service mode: SCM status loop, launching/supervising the child emulator, StopAction/kill, OnFail restart/reboot, ACL + Job-object setup.
- `erlsrv_registry.c` / `.h` — registry get/set/enumerate/remove of per-service keys and event-log source registration.
- `erlsrv_util.c` / `.h` — event-log wrappers and environment-block ↔ argv conversion helpers (`envdup`, `env_to_arg`, `arg_to_env`), plus the globals `service_name`/`real_service_name`.
- `erlsrv_logmess.mc` — event-log message catalog.

## Constraints
- **No Makefile, headers-only build inputs**: these `.c` files are compiled by `../common/Makefile.in` (objects `erlsrv_{main,util,registry,service,interactive}.o` + `erlsrv_logmess.res|.o`); `../win32/Makefile` only installs the already-built exe. Do not add a Makefile here.
- Must stay buildable by both MSVC (`cl`) and mingw (the two object/rc rule variants in `../common/Makefile.in`).
- Unicode/Wide-char throughout (`wchar_t`, `wmain`, `*W` Win32 APIs); interactive mode forces stdio to UTF-8 via `_setmode(_O_U8TEXT)`.
- `add`/`remove`/`start`/`stop`/`enable`/`disable`/`rename` require **administrator** privileges (HKLM writes and SCM control).
- The `mc`/`rc` steps are deliberately serialized on Windows (see `../common/Makefile.in`).

## Design Decisions
- **Separate wrapper process rather than running `erl.exe` as the service**: a Windows service binary must itself call `StartServiceCtrlDispatcher`/`RegisterServiceCtrlHandler` and report status; `erl.exe` is a console app. `erlsrv.exe` is the registered service binary and supervises `erl.exe` as a child, relaying stop/kill and implementing OnFail policies.
- **Registry as the configuration store**: keeps per-service settings independent of the SCM DB, easy to enumerate (`list`), and survives rename.
- **Decoupled display vs internal service name**: lets users rename a service and avoids SCM name collisions.
- **Message catalog (`.mc`) separate from code**: produces `EventMessageFile`-registered, localisation-ready event-log messages instead of inline strings.
- **Job object + named kill event**: robustly guarantees the child emulator dies with the service even across forced `erlsrv.exe` termination.

## Known Issues / Gotchas
- `OnFail=reboot` calls `InitiateSystemShutdown` (requires `SE_SHUTDOWN_NAME`, via `enable_privilege`) — a crash-looping/misconfigured service can reboot the host.
- `OnFail=restart` suppresses restart if the node dies within `CYCLIC_RESTART_LIMIT` (10 s) of start (interpreted as a distribution-name conflict); `restart_always` overrides this.
- Child must exit within ~30–45 s of `StopAction` or it is force-killed with `TerminateProcess`.
- `DEBUG_ENV` (`ERLSRV_DEBUG`) is defined in `erlsrv_global.h` but is not referenced anywhere in the tree — dead definition.
- The global semaphore is taken for the whole `interactive_main` run, so interactive invocations block each other.
- `erlsrv.exe` is Windows-only and cannot be exercised on Unix builds.

## Notes for Agents
- Documentation lives outside this directory: `erts/doc/references/erlsrv_cmd.md` (rendered as `lib/erts/doc`/`erts` man page, generated for Windows only).
- The Erlang-side counterpart is `lib/sasl/src/erlsrv.erl` (release_handler integration); `lib/sasl/src/release_handler.erl` is the other consumer of `ERLSRV_SERVICE_NAME`. Keep the env-var names in `erlsrv_global.h` in sync with those.
- The emulator side of the `-service_event`/named-event contract is `erts/emulator/sys/win32/sys.c` and `erl_poll.c`.
- Sole automated coverage is `erts/test/nt_SUITE.erl` (Windows-only), which drives the CLI and registry via `erlsrv`.

## Dependencies
- Windows SDK / Win32 APIs: `advapi32` (registry, SCM, event log, tokens/ACLs — `RegOpenKeyExW`, `CreateServiceW`, `ReportEventW`, `OpenProcessToken`, `AllocateAndInitializeSid`, `AdjustTokenPrivileges`), `kernel32` (processes, Job objects, events, pipes, `env` APIs).
- `mc.exe` (message compiler) and `rc.exe`, wrapped by the shim scripts in `../cygwin_tools/`, `../msys_tools/`, `../wsl_tools/`.
- MSVC or mingw `cl`/`gcc` toolchain.

## Test Strategy
- No tests in this directory. Only `erts/test/nt_SUITE.erl` (runs only on Windows) exercises the CLI, SCM registration, and registry round-trips. Unix CI gives this code no coverage.
- Symptom-level debugging is via the Windows Event Log (source `ErlSrv`) and the per-service `.debug` file (`-debugtype new|reuse`); `console` debug attaches an interactive console window to the node.

## See Also
- `../common/` → portable launchers and the Makefile that builds `erlsrv.exe`
- `../win32/` → other Windows launcher programs (`erlsrv` install rules in `Makefile`, `port_entry.c`, `erlsrv`-adjacent `run_erl`/`heart` Windows bits)
- `../../emulator/sys/win32/` → emulator side of the `-service_event` protocol
- `../../../lib/sasl/src/erlsrv.erl` → Erlang/release_handler integration
