# os_mon — C Helper / Port Programs

## Intent
C programs that the `os_mon` Erlang modules spawn as Erlang ports to gather OS data (CPU load, memory, disk) or to talk to OS logging facilities.
They are the only native code of the application; the Erlang side is `../src/`.
Programs are arch/OS-specific: only the ones relevant to the current host are built.

## API Surface (tracked sources)
- `Makefile` — 22-line wrapper; only does `include $(ERL_TOP)/make/run_make.mk` (forwards build targets to `$(TARGET)/Makefile`).
- `Makefile.in` — the real build description; configured into `$(TARGET)/Makefile` (see Constraints).
- `cpu_sup.c` — CPU load / utilization port program (`cpu_sup`, non-Windows), talks to `../src/cpu_sup.erl`.
- `memsup.c` + `memsup.h` — memory-usage port program (`memsup`), talks to `../src/memsup.erl`; `memsup.h` defines the shared request/response protocol.
- `ferrule.c` — Solaris-only helper (`ferrule OwnPath`) that reads the syslog FIFO `<OwnPath>/syslog.otp` and forwards each line, length-prefixed, to stdout (Erlang).
- `mod_syslog.c` — Solaris-only helper (`mod_syslog otp|nootp OwnPath SyslogConf`) that creates/removes the `syslog.otp` FIFO, installs the OTP or original `syslog.conf` from `<OwnPath>`, HUP-restarts syslogd and prints an exit code to stdout.
- `win32sysinfo.c` — Windows-only port program (`win32sysinfo.exe`) for memory and disk info; talks to `../src/os_mon_sysinfo.erl` (used by `disksup`).
- `nteventlog/` — Windows-only event-log port program (`nteventlog.exe`); see `nteventlog/CONTEXT.md`.

## Port Protocols
- `cpu_sup.c` ↔ `cpu_sup.erl`: port opened with `[stream]`; Erlang sends 1-byte command chars, C replies with 4-byte big-endian unsigned ints.
  Commands: `p` ping (returns 4711), `n` nprocs, `1`/`5`/`f` loadavg 1/5/15 min, `u` per-CPU utilization vector, `q` quit.
  Utilization reply: header `[no_of_cpus, values_per_cpu]` followed by `(id, value)` pairs (value ids `CU_*`, mirrored by the `?cu_*` macros in `cpu_sup.erl`).
- `memsup.c` ↔ `memsup.erl`: port opened with `[{packet,1}]`; Erlang sends `[SHOW_MEM]` or `[SHOW_SYSTEM_MEM]`, C replies with `{packet,1}`-framed hex strings.
  `SHOW_MEM`: two packets, used then total (hex). `SHOW_SYSTEM_MEM`: alternating one-byte tag packet + value packet, terminated by tag `SHOW_SYSTEM_MEM_END`; unknown tags are ignored by the C side and rejected by the Erlang side.
- `win32sysinfo.c` ↔ `os_mon_sysinfo.erl`: port opened with `[{packet,1}]`; Erlang sends `'m'` (memory) or `'d'` (all drives) or `'d'+DriveRoot+0`, C answers line-oriented text terminated by tag `"o"` (`?OK`). Startup sends an initial `"o"` ACK.
- `ferrule.c`: no command input — it streams FIFO lines to Erlang; accepts `die` and `only_stdin` commands on stdin (2-byte length-prefixed).
- `mod_syslog.c`: no port; run synchronously by `os_sup.erl`, exit status printed to stdout as a decimal code.

## Build / Generation (important)
- `lib/os_mon` has **no** `configure`/`configure.ac` of its own. The per-arch build directory `c_src/$(TARGET)/Makefile` is generated from `c_src/Makefile.in` by the **erts** autoconf (`erts/configure.ac`: `AC_CONFIG_FILES([../lib/os_mon/c_src/$host/Makefile:../lib/os_mon/c_src/Makefile.in])`, near the `os_mon_programs` / `CPU_SUP_LIBS` `AC_SUBST` block, which also checks for `kstat`/`kvm`).
- `PROGRAMS` selection: on `win32` targets → `win32sysinfo.exe` and `nteventlog.exe` (linked with `$(ERL_TOP)/erts/obj/$(TARGET)/port_entry.o` and `-entry:erl_port_entry`); otherwise → `memsup @os_mon_programs@`, where erts' configure sets `os_mon_programs` to `ferrule mod_syslog` on `solaris2*` and `cpu_sup` on darwin/linux/freebsd/openbsd.
- Outputs: binaries to `../priv/bin/$(TARGET)`, objects to `../priv/obj/$(TARGET)`. `release_spec` copies `C_FILES` to `$(RELSYSDIR)/src` and the binaries flattened into `$(RELSYSDIR)/priv/bin` (no per-arch subdir in an installed release).
- `../src/os_mon.erl:open_port/2` resolves the executable as `priv/bin/<Name>` first, else `priv/bin/<arch>/<Name>` — this is why the relocatable release must be flattened.

## Constraints
- Keep `memsup.h` in sync **manually** with `../include/memsup.hrl` (the `.hrl` carries an explicit "keep consistent manually" comment); there is no code generation for this protocol.
- Never edit generated `$(TARGET)/Makefile`; edit `Makefile.in` (or the erts configure variables) and re-run the top-level configure.
- Only one `main()` per program; `memsup.c` defines `MAIN` as `main` (a porting hook), `cpu_sup.c` uses `FD_IN`/`FD_OUT` (=0/1).
- Do not close stdio fds 0/1/2 explicitly at exit.

## Known Issues
- `memsup.h` defines `MEM_LARGEST_FREE` (4) and `MEM_NUMBER_OF_FREE` (5) which have no counterpart in `../include/memsup.hrl` (they are unused by the Linux/BSD/Darwin paths).
- Platform support in `cpu_sup.c`/`memsup.c` is selected by ad-hoc `#if defined(__sun__)` / `__linux__` / `__APPLE__+__MACH__` / `__FreeBSD__` / `__OpenBSD__` / `__DragonFly__` / `sgi` / `Android` guards rather than by configure capabilities; `memsup.c` falls back to faked values when no memory source is available.
- `cpu_sup.c`/`memsup.c` assume 32-bit ints in their wire encoders.

## Notes for Agents
- `c_src/win32/`, `../priv/bin/win32/` and `../priv/obj/win32/` are git-ignored build-output/legacy dirs (repo-root `.gitignore`); there is no tracked `win32/Makefile`.
- On Windows the build dir is named `win32` but on Unix it is the autoconf host triplet (e.g. `x86_64-pc-linux-gnu`).
- `ferrule`/`mod_syslog` exist only for Solaris `os_sup` syslog support; `cpu_sup`/`memsup` cover the other Unixes.

## Test Strategy
- No unit tests target this C code directly; it is exercised indirectly through `../test/` Common Test suites (e.g. `cpu_sup`/`memsup` suites) which spawn the compiled programs.
- `make opt` in `lib/os_mon/c_src/` builds only the programs relevant to the configured `$(TARGET)`.

## See Also
- `../src/` for the Erlang counterparts (`cpu_sup.erl`, `memsup.erl`, `os_mon_sysinfo.erl`, `nteventlog.erl`, `os_sup.erl`).
- `../include/memsup.hrl` for the Erlang side of the `memsup` protocol.
- `../../erts/configure.ac` for the `os_mon_programs` / `CPU_SUP_LIBS` generation logic.

## Routing Table
- `nteventlog/` → Windows event-log port program (`nteventlog.exe`) sources
