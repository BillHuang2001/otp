# erts/emulator/drivers/ — Built-in (static) emulator drivers

## Intent
Legacy `erl_driver`-style (port) drivers that are compiled *into* the `beam` executable, as opposed to loadable drivers or the NIFs in `../nifs/`.
Split by OS capability instead of by `#ifdef` walls: `common/` holds code that must compile on every platform, `win32/` holds platform-specific code.

## API Surface
A driver becomes available to Erlang only if its object file is listed in `DRV_OBJS` in `../Makefile.in` (win32 additionally adds `registry_drv.o`).
`make_driver_tab` (in `../utils/`) derives the exported symbol from the object file's base name (`inet_drv.o` → `inet_driver_entry`) and emits `$(TTF_DIR)/driver_tab.c`; `beam/io.c` then walks `driver_tab[]` at emulator init and registers every entry.
Convention: a linked driver must export `ErlDrvEntry <base>driver_entry` (base = file name with the trailing `drv` removed) whose `driver_name` string is what Erlang passes to `open_port`.
Non-static (loadable) drivers can also be appended to the same table via `--enable-static-drivers` / `STATIC_DRIVERS` (`../configure.ac`), which is why the table has both `de` pointers and `_driver_init()` calls.

Live drivers:
- `common/inet_drv.c` → symbol `inet_driver_entry`, driver name `"inet"`.
  Its `init` (run at emulator startup, thanks to `driver_tab`) registers `"tcp_inet"`, `"udp_inet"` and (when `HAVE_SCTP`) `"sctp_inet"`, then removes the placeholder `"inet"` entry; the win32-only `dummy_tcp/udp/sctp_driver_entry` structs (`inet_drv.c:2196`, `:2212`, `:2229`) are **never** `add_driver_entry`-ed — dead code, so an absent driver still fails as a missing name.
  Consumed by the preloaded module `../../preloaded/src/prim_inet.erl` (`protocol2drv/1` maps `tcp|mptcp`→`tcp_inet`, `udp`→`udp_inet`, `sctp`→`sctp_inet`) and thereby by `lib/kernel`'s `gen_tcp`/`gen_udp`/`gen_sctp`, `inet_tcp`/`inet_udp`/`inet_sctp` and the `local_tcp`/`local_udp` (AF_LOCAL) backends; the newer `socket` API is *not* served here (it is `../nifs/common/prim_socket_nif.c`).
  The driver's control commands are the private `prim_inet` protocol (`ctl_cmd`), and it replies with `{inet_reply, ...}` / `{inet_async, ...}` terms rather than via NIF returns.
- `common/ram_file_drv.c` → symbol `ram_file_driver_entry`, driver name `"ram_file_drv"`.
  An in-memory file implemented inside a port; consumed by `lib/kernel/src/ram_file.erl` (`open_port({spawn, "ram_file_drv"}, ...)`) and special-cased by `lib/runtime_tools/src/instrument.erl` (driver-level binary instrumentation).
- `win32/registry_drv.c` → symbol `registry_driver_entry`, driver name `"registry__drv__"` (win32 only).
  Consumed by `lib/stdlib/src/win32reg.erl`, which appends a `"r"`/`"w"` mode suffix to the port name.
- `unix/` holds no driver sources.
- `vxworks/` holds no driver sources.

## Constraints
- OS-specific code belongs in `unix/` (and `win32/`); `common/` must compile on all platforms and is compiled with `-Idrivers/common -Idrivers/$(ERLANG_OSTYPE)` so it may include headers from the matching OS directory.
- The public driver API is `../beam/erl_driver.h` (plus `../sys/$(ERLANG_OSTYPE)/driver_int.h`); these files include it as `"erl_driver.h"` and are the in-tree reference users of it.
- `ERLANG_OSTYPE` is only `unix` or `win32` (set in `../configure.ac`).
- Adding/removing a driver means editing `DRV_OBJS` in `../Makefile.in` and, if a new file name is used, exporting the matching `<base>driver_entry` symbol; the symbol name is derived mechanically, so renaming a file silently breaks the generated table link.
- Never edit the generated `$(TTF_DIR)/driver_tab.c`; regenerate via the `driver_tab.c:` rule in `../Makefile.in`.
- Do not copy code from here into `../nifs/` (or vice versa) without checking the `erl_driver` vs `erl_nif` semantics — the APIs differ in threading, scheduling (no dirty schedulers for ports) and process-exit signalling.

## Design Decisions
- Drivers, not NIFs, for these subsystems: they predate NIFs, they manage OS-level events through the emulator's I/O pollset (`driver_select`/`ready_input`/`ready_output`) rather than doing blocking or dirty-scheduler work, and they expose an Erlang-level control protocol over port commands.
- Compiled statically into the VM (rather than shipped as separate `.so` files) so `inet`/`ram_file`/registry support is always present, needs no install-time discovery, and cannot be swapped out by an unprivileged user (the table is also used for driver "taint" tracking).
- OS split by directory rather than by preprocessor conditionals, matching `../sys/` and `../nifs/`, so `common/` code stays readable and platform files stay small.
- `inet_drv.c` registers TCP/UDP/SCTP as three *separate* driver entries (sharing descriptor code via parameterised inits) instead of one driver with a protocol switch, so each gets its own `driver_name` and can be absent independently (e.g. no SCTP on win32).
- SCTP support is optional at runtime: with `HAVE_SCTP_BINDX` the symbols are linked in directly, otherwise the driver `dlopen`s `LIBSCTP` and resolves the symbols dynamically, falling back to "not supported".

## Known Issues / Gotchas
- A `.c` file here is NOT necessarily built: `DRV_OBJS` in `../Makefile.in` decides what is linked, not the `DRV_COMMON_SRC`/`DRV_OSTYPE_SRC` globs — check `DRV_OBJS` (or run `../beam/check_dead_sources.py`) before assuming a source is live.
- `inet_drv.c` is 15879 lines and by far the largest source of port/driver behaviour in the VM; it is the file to read for socket semantics, but avoid wholesale reformatting (huge conflict surface).
- `inet_drv.c` uses **no async threads**: the string `driver_async` occurs 0 times and every `ready_async` field is NULL. All I/O is non-blocking sockets driven by `driver_select` readiness plus an internal async-op queue (`INET_MAX_ASYNC = 1` ring at `inet_drv.c:1069`/`:1252` plus an unbounded multi-accept list). `driver_output_term` is also never used — all term replies go through `erl_drv_send_term`.
- Both link-time state (`driver_tab`, `inet_driver_entry`'s init) and runtime dynamism (`add_driver_entry`/`remove_driver_entry`) are involved, so a driver's presence at runtime is not visible from the table alone.
- Drivers are statically linked and therefore cannot be unloaded or reloaded at runtime; recompiling any of them requires exiting and restarting the emulator.
- Driver naming is the ABI used by Erlang code (`"tcp_inet"`, `"ram_file_drv"`, `"registry__drv__"` with its double-underscore suffix); changing a `driver_name` breaks the matching Erlang module silently at runtime, not at build time.
- `inet_drv.c` shares packet/header-parsing logic with the `decode_packet` BIF (`../beam/packet_parser.c` was split out of it), so packet-option changes may need to be mirrored there.

## Dependencies
- POSIX/Winsock sockets, `netdb`/`resolv`, and for SCTP either headers plus `-lsctp` (`LIBSCTP`, passed as `-DLIBSCTP=...` by the `drivers/common/%.c` rule) or runtime `dlopen` of the same library.
- `sys.h` and friends from `../sys/$(ERLANG_OSTYPE)`; for win32 `../etc/win32` is on the include path.
- `erts_sys_ddll_open_noext`/`erts_sys_ddll_vsym` from `../sys/` for the dynamic SCTP lookup; Windows registry APIs for `registry_drv.c`.
- The generated `config.h` in `$(TARGET)/` is required for the feature macros (`HAVE_SCTP`, `HAVE_UDP`, `HAVE_IN6`, ...) that select driver behaviour.

## Test Strategy
- `../test/driver_SUITE.erl` (data in `../test/driver_SUITE_data/*.c`) covers the general driver/port API using *external* example drivers, not the static ones here; it is the place for driver-interface semantics.
- `inet` drivers: `lib/kernel/test/{inet_SUITE,inet_sockopt_SUITE,inet_res_SUITE,gen_tcp_*,gen_udp_SUITE,gen_sctp_SUITE}.erl` plus the `socket_*_SUITE` for the NIF-based API; `../test/port_SUITE.erl` and `../test/trace_port_SUITE.erl` cover the port plumbing underneath.
- `ram_file_drv`: `lib/kernel/test/ram_file_SUITE.erl`; `lib/kernel/test/gen_tcp_misc_SUITE.erl` has an explicit `open_port({spawn_driver, "ram_file_drv"}, ...)` driver-registration check.
- `registry_drv`: win32-only, exercised indirectly via `lib/stdlib/test` `win32reg` coverage.
- `../test/efile_SUITE.erl` and `lib/kernel/test/prim_file_SUITE.erl` are the closest neighbours for file I/O but test the NIF path, not `ram_file_drv`.
- Run these through the `erts/emulator` test target (`make test` from `ERL_TOP`), i.e. the `emulator.spec` / `emulator_smoke.spec` specs, with a full release present.
- Memory-safety issues here surface under `TYPE=valgrind`/`asan`; `../valgrind/suppress.standard` contains `inet_drv`-specific suppressions.

## Notes for Agents
- Read `../Makefile.in` (`DRV_OBJS`, the `drivers/common/%.c` and `drivers/$(ERLANG_OSTYPE)/%.c` pattern rules) before assuming how anything in this directory is built.
- Erlang-side counterparts that must stay in sync with the driver names/messages: `../../preloaded/src/prim_inet.erl`, `lib/kernel/src/ram_file.erl`, `lib/stdlib/src/win32reg.erl`.

## See Also
- `../nifs/` → the NIF replacements for what used to be drivers here (`zlib_nif`, `prim_tty_nif`, `prim_file_nif`, `prim_socket_nif`, `zstd_nif`) — read-only from here, escalate changes to the emulator root
- `../beam/` → `erl_driver.h`, `io.c` (driver table registration, port I/O), `packet_parser.c` (code shared with `inet_drv.c`)
- `../sys/` → `erl_check_io`/`erl_poll` (pollset used by `driver_select`), `sys_$(ERLANG_OSTYPE)` helpers, `erts_sys_ddll_*`
- `../utils/make_driver_tab` → generates `driver_tab.c`; `../Makefile.in` → `DRV_OBJS`, `driver_tab.c` rule
- `../../test/` → erts-level port/launcher suites; `../../preloaded/src/prim_inet.erl` → the Erlang half of the inet driver

## Routing Table
- `common/` → platform-independent drivers: `inet_drv.c` (15879 lines; `inet`/`tcp_inet`/`udp_inet`/`sctp_inet`), `ram_file_drv.c` (~515 lines; `ram_file_drv`)
- `unix/` → unix-specific driver sources (holds no driver sources)
- `win32/` → win32-specific driver: `registry_drv.c` (~556 lines; `registry__drv__`), used by `lib/stdlib` `win32reg`
- `vxworks/` → holds no driver sources
- `../nifs/` → NIF counterparts and the newer `socket`/`prim_file`/`zlib` implementations (sibling — read-only, escalate writes to `../`)
- `../beam/` → driver/port core, `erl_driver.h` public API (sibling — read-only, escalate writes to `../`)
- `../sys/` → OS abstractions and pollset used by these drivers (sibling — read-only, escalate writes to `../`)
- `../utils/` → `make_driver_tab` codegen invoked for this directory (sibling — read-only, escalate writes to `../`)
- `../test/` → driver/port/efile test suites (sibling — read-only, escalate writes to `../`)
