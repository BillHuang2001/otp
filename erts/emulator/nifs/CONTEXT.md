# erts/emulator/nifs/ — Built-in (static) NIFs of the BEAM emulator

## Intent
C sources of the NIFs that are compiled **into the emulator binary** and therefore always available, even in a bootstrap/preloaded-only system: file I/O, buffered reads, tty handling, tracing, zlib/zstd compression, and the `socket`/`net` (esock) API.
They are the C half of the preloaded Erlang modules in `../../preloaded/src/` plus `lib/kernel/src/prim_tty.erl` and `lib/stdlib/src/zstd.erl`.
Layout follows the emulator-wide convention: `common/` holds the cross-platform NIF modules and shared headers, `unix/` and `win32/` hold the OS-specific backend implementations (`ERLANG_OSTYPE` selects the directory in the Makefile).

## API Surface
One NIF module per `.c` in `common/`, each ending in `ERL_NIF_INIT(<mod>, ...)`. Every file starts with `#define STATIC_ERLANG_NIF 1`, so the entry symbol is `<mod>_nif_init` (not `nif_init`) and the `enif_*` API is called directly instead of through the Windows dyn-call table.

| common/*.c | NIF module | ~#funcs | Erlang consumer |
|---|---|---|---|
| `prim_socket_nif.c` | `prim_socket` | 28 names (40 table rows) | `../../preloaded/src/prim_socket.erl`, `lib/kernel/src/socket.erl` (+ `socket_registry.erl`) |
| `prim_net_nif.c` | `prim_net` | 15 | `../../preloaded/src/prim_net.erl` (loads it as NIF for module `net`), `lib/kernel/src/net.erl`, `inet` |
| `prim_file_nif.c` | `prim_file` | 33 | `../../preloaded/src/prim_file.erl`, `file.erl`, `erl_prim_loader` |
| `prim_buffer_nif.c` | `prim_buffer` | 9 | `prim_file.erl`, `prim_inet.erl` |
| `prim_tty_nif.c` | `prim_tty` | 20 | `lib/kernel/src/prim_tty.erl` (shell/escript) |
| `zlib_nif.c` | `zlib` | 19 | `../../preloaded/src/zlib.erl`, `zip`/`prim_zip` |
| `zstd_nif.c` | `zstd` | 24 | `lib/stdlib/src/zstd.erl` |
| `erl_tracer_nif.c` | `erl_tracer` | 2 | `../../preloaded/src/erl_tracer.erl` (tracer module callbacks) |

Shared headers (not part of the NIF API, but the internal contracts):
- `prim_file_nif.h` — the `efile_*` C interface + `efile_data_t` (also holds the `efile` NIF resource payload). Implemented by `unix/unix_prim_file.c` and `win32/win_prim_file.c`; `prim_file_nif.c` is the only caller.
- `socket_io.h` — the function-pointer typedefs (`ESockIOSend`, `ESockIOGetopt`, …) that make up the `ESockIoBackend` vtable, plus I/O vector helpers. The `ESockIoBackend` struct itself is declared in `common/prim_socket_nif.c` (not here).
- `socket_syncio.h` / `socket_asyncio.h` — the two backend interfaces: `essio_*` (unix, non-blocking + `enif_select`) and `esaio_*` (win32, overlapped I/O / IOCP).
- `socket_int.h`, `prim_socket_int.h` — shared types/macros (`ESockData`, `ESockDescriptor`, term conversions) used by `prim_socket_nif.c`, `prim_net_nif.c` and both backends.
- `socket_util.h`/`socket_util.c`, `socket_tarray.h`/`socket_tarray.c`, `socket_dbg.h`/`socket_dbg.c` — shared helpers for socket/net NIFs (address+option conversion, variable-length Erlang term arrays, debug printing).
- `prim_file_nif_dyncall.h` — ABI structs for the cross-NIF resource call that lets the socket code `dup` a `prim_file` handle via `enif_dynamic_resource_call`; the caller is `unix/unix_socket_syncio.c` (sendfile path) and the handler is `common/prim_file_nif.c` (`dyncall()`/`dyncall_dup()`).

Public NIF API used by all of them: `../beam/erl_nif.h` (installed copy `../../include/erl_nif.h`). `../../include/internal/erl_internal_test.h`-style VM internals are not part of this API.

## Registration (how these become available in the VM)
- `../Makefile.in` compiles them via `$(OBJDIR)/%.o: nifs/common/%.c` and `nifs/$(ERLANG_OSTYPE)/%.c` (includes `-Inifs/common -Inifs/$(ERLANG_OSTYPE)`), with `ESOCK_CFLAGS=-DESOCK_NON_EMPTY_IOV` and `-DLIBSCTP=$(LIBSCTP)`. The object lists are explicit: `NIF_OBJS` (`prim_tty_nif.o`, `erl_tracer_nif.o`, `prim_buffer_nif.o`, `prim_file_nif.o`, `zlib_nif.o`, `zstd_nif.o`, `ESOCK_NIF_OBJS`), `ESOCK_RUN_OBJS` (`socket_dbg.o`, `socket_tarray.o`, `socket_util.o`), and the OS backend objects in `OS_OBJS` (`unix_prim_file.o`/`unix_socket_syncio.o` resp. `win_prim_file.o`/`win_socket_asyncio.o`).
- `../Makefile.in` generates `$(TTF_DIR)/driver_tab.c` with `utils/make_driver_tab -nifs $(NIF_OBJS) $(STATIC_NIF_SYMBOLS) -drivers ...`. The generator derives `<name>_nif_init` from each object's basename (stripping `_nif.*`) and emits `ErtsStaticNif erts_static_nif_tab[]`. **Never edit the generated file.**
- `../beam/erl_nif.c`: `static_nifs_init()` (called from `erl_nif_init()` at VM start) invokes each entry init once, maps `entry->name` to a module atom and marks it untainted. When the Erlang module's `on_load` calls `erlang:load_nif/2`, `erts_load_nif()` finds the entry via `is_static_nif_module(caller_module)` and skips `dlopen`; the filename argument is then ignored (hence `prim_net.erl` passing `"net"` and `zstd.erl` passing `"zstd"` still works).
- Boot order: `../../preloaded/src/erl_init.erl` explicitly loads `zlib`, `erl_tracer`, `prim_buffer`, `prim_file`, and (only if the module exists) `prim_socket` + `prim_net`, before `init` starts.
- `preloaded` membership: `zlib`, `prim_buffer`, `prim_file`, `erl_tracer`, `prim_socket`, `prim_net` are preloaded (`../../preloaded/src/Makefile`); `prim_tty` (lib/kernel) and `zstd` (lib/stdlib) are ordinary modules that load the same static NIFs.

## Constraints
- OS-specific C code belongs in `unix/` or `win32/`; everything in `common/` must compile on all targets (use `ESOCK_ENABLE` / `__WIN32__` guards, not new subdirectories).
- File/object naming is load-bearing: the `.c` must be `<module>_nif.c` and `ERL_NIF_INIT`'s NAME must equal the Erlang module, because the init symbol is derived from the object basename.
- Adding a new NIF object requires touching the explicit `NIF_OBJS`/`OS_OBJS` list in `../Makefile.in` — `NIF_COMMON_SRC` (wildcard) is used only for dependency generation (`-MM` → `nif.depend.mk`) and `compile_commands.json`, it does not drive compilation.
- Blocking work must be declared dirty or avoided: `prim_file_nif.c` marks its I/O functions `ERL_NIF_DIRTY_JOB_IO_BOUND`/`CPU_BOUND` (~32 annotated functions) and drives reads/writes through a `EFILE_STATE_IDLE|BUSY|CLOSED` state machine guarded by `ErlNifMonitor`, so the resource can be closed/cancelled while a dirty job is in flight. `zstd_nif.c` marks dictionary loading CPU-bound. Long-running socket work is NOT offloaded to a worker thread pool on unix — sockets are non-blocking and the NIF returns an `enif_select` term so the VM pollset drives completion (there is no `pthread_*`/`enif_thread_create` in `unix/unix_socket_syncio.c`). On win32 it uses overlapped I/O, dispatched by the backend's own IOCP completion thread pool (sized `2*schedulers`, floor 2, computed in `on_load`).
- The `socket_*.c` files and both socket backends are wrapped in `#ifdef ESOCK_ENABLE` and compile to empty objects when esock is disabled; `prim_socket_nif.c` then emits an empty `esock_funcs[]` stub so the Erlang fallback can raise `notsup`.
- SCTP: no separate source here, the socket NIF uses `-DLIBSCTP=$(LIBSCTP)`/`HAVE_SCTP_H` from `../../configure.ac` (`--enable-sctp[=lib]`).

## Design Decisions
- **NIFs rather than port drivers** for files, sockets and compression: socket/file handles become NIF *resources* with monitors (close/dtor callbacks, `enif_dynamic_resource_call`), calls can be dispatched to dirty schedulers, and no extra port process or copying through a driver is needed. The legacy equivalents (`inet_drv`, `ram_file_drv`, `registry_drv`) live in `../drivers/` and are only used by `inet`/`file` legacy paths.
- **Static-only, by design.** No in-tree Makefile builds `nifs/**/*.c` into a shared library, and nothing is installed under `lib/*/priv/lib` from here. The unconditional `#define STATIC_ERLANG_NIF 1` is what makes the sources usable only as emulator-internal NIFs (symbol `<mod>_nif_init`, direct `enif_*` calls); `erl_nif.h` also supports the mirror-image case of *dynamically written but statically linked* NIFs (`-DSTATIC_ERLANG_NIF`, `-DSTATIC_ERLANG_NIF_LIBNAME=<lib>`) which is how `lib/*` NIFs are linked in under `--enable-static-nifs` (`STATIC_NIFS` in `../Makefile.in`: `lib/asn1/.../asn1rt_nif.a`, `lib/crypto/.../crypto.a`). Those externally linked entries get `taint=1` in `erts_static_nif_tab`, the entries from this directory get `taint=0`.
- **Common code + backend vtable.** `prim_socket_nif.c` keeps all argument validation (`nif_*` functions) and delegates the real work to a per-OS backend selected in `on_load` into the static `ESockIoBackend io_backend`: `esaio_*` on win32 (I/O completion ports, `WSARecv`/`AcceptEx`/`ConnectEx` via `WSAIoctl` extension pointers, own completion thread pool), `essio_*` on unix (non-blocking sockets — blocking `recv`/`send`/`accept`/`connect` that would return EAGAIN become `enif_select` waits on the VM pollset; no worker threads). Same pattern for `prim_file`: the `prim_file` NIF module implements the module's semantics, the OS files implement `efile_*`.
- **Result delivery differs by OS.** On unix the backend arms the VM pollset through `esock_select_read`/`esock_select_write`/`esock_select_stop`/`esock_select_cancel` (`prim_socket_nif.c:17635-17700`, all `#ifndef __WIN32__`) which wrap `enif_select*` → erts check_io (epoll/kqueue/poll). On win32 there is NO select path: whole ops are issued as overlapped I/O and their results are delivered as Erlang messages by the IOCP completion threads via `esaio_send_completion_msg` (`win_socket_asyncio.c:12259`) → `mk_completion_msg` (`:12303`) → `esock_send_msg` → `enif_send` (`prim_socket_nif.c:17433`), as `{'$socket', Socket, completion, {Ref, Status}}`. So the Windows esock path bypasses check_io entirely.
- **`prim_file` and `prim_socket` interoperate through a dyncall**: on unix the socket code needs a file descriptor duplicated out of an `efile` resource, done from `unix/unix_socket_syncio.c` with `enif_dynamic_resource_call(env, <prim_file efile resource>, prim_file_nif_dyncall_dup, ...)` (handled by `dyncall()`/`dyncall_dup()` in `common/prim_file_nif.c`) using `prim_file_nif_dyncall.h` as the shared ABI.
- **Compression NIFs statically use the vendored/embedded libraries** (`../zlib/`, `../zstd/`), consistent with the emulator's "3PP can be replaced by system libs" model (`--enable-builtin-zlib` etc.); no separate compression shared object is shipped.

## Known Issues / Gotchas
- `$(TTF_DIR)/driver_tab.c` is generated at build time and lives outside this directory; the `erts_static_nif_tab` in it is the only place where the `<mod>_nif_init` symbols of this directory are referenced. Grepping the tree for these symbols shows them only there.
- Renaming or duplicating a NIF module means renaming the file *and* the `ERL_NIF_INIT` NAME, or the emulator fails to link (undefined `<basename>_nif_init`).
- `../Makefile.in` line ~1437 excludes `nifs/common/socket_*.c` from `NIF_COMMON_SRC` **when TARGET=win32** ("the nif-functions for socket and net are basically stubbed with badarg in the win32 case"). That variable feeds dependency scanning/compdb only; the objects still come from `NIF_OBJS`/`ESOCK_RUN_OBJS`/`OS_OBJS`, and win32 does ship a real socket backend (`win32/win_socket_asyncio.c`), so the comment no longer describes the code.
- `erts`/`prim_tty`: the NIF source is here but the module is not preloaded — it is loaded on demand by the shell/escript path (`lib/kernel/src/prim_tty.erl`), so breakage in `prim_tty_nif.c` surfaces as shell/`escript` failures, not as a VM start failure.
- `zlib_nif.c` vs `prim_zip.erl`: the NIF provides only the zlib primitives; the Erlang `zip` archive logic is separate.
- SCTP support in `prim_socket_nif.c`/`unix_socket_syncio.c` is compiled only when the platform has `netinet/sctp.h`; SCTP test cases then still require kernel SCTP + ESOCK_SCTP_* config.
- On Solaris the socket sources need `_XPG4_2`/`__EXTENSIONS__` for SCTP (in-file workaround).

## Dependencies
- `../../config.h` (from `../../configure.ac`) for `ESOCK_ENABLE`, `HAVE_SCTP_H`, `HAVE_SENDFILE`, `ESOCK_*` knobs; `../../include/internal/*` for `sys.h`, `SysIOVec`.
- OS socket headers: `sys/socket.h`, `netinet/*`, `ifaddrs.h`, `netpacket/packet.h` (unix); `ws2tcpip.h`, `mswsock.h`, `windows.h` (win32); `netinet/sctp.h` optional.
- ncurses/termcap for `prim_tty_nif.c`; zlib and libzstd (or the embedded copies in `../zlib`, `../zstd`) for the compression NIFs.
- `../beam/erl_nif.h` (public NIF API) and the `erts_atomic*`/`ErlNifMonitor` primitives it exposes.

## Test Strategy
No test suites live in this directory.
- `lib/kernel/test/` (sibling, read-only): `socket_SUITE`, `socket_api_SUITE`, `socket_sctp_SUITE`, `socket_sctp_example_SUITE`, `socket_ttest_SUITE`, `socket_traffic_SUITE` + `socket_test_lib`/`socket_test_evaluator`/`socket_test_profile`, `prim_net_SUITE`, `prim_file_SUITE`, `net_SUITE`, `inet_SUITE`, `gen_tcp_socket_SUITE`, `interactive_shell_SUITE` (prim_tty). The socket suites `skip` when the esock NIF is not loaded (`--disable-esock`).
- `../test/` (erts/emulator/test): `efile_SUITE` (prim_file/efile behaviour), `nif_SUITE`/`dirty_nif_SUITE`/`trace_nif_SUITE` (NIF API in general, not these NIFs), `tracer_SUITE` (erl_tracer interface), `module_info_SUITE`.
- `../../../lib/stdlib/test/`: `zstd_SUITE`, `zip_SUITE` (zlib), `shell_SUITE`.
- `../../test/` (erts/test): `otp_SUITE` (sanity checks that preloaded NIF modules load).
- Debug flavors for VM C code apply here too (`TYPE=debug|valgrind|asan`), and `ESOCK_DISPLAY_ON_LOAD_DETAILS`/`socket_dbg.h` macros (`ESOCK_PRINTF`, `esock_dbg_printf`) can be enabled during development.

## Routing Table
- `common/` → cross-platform NIF modules (prim_socket, prim_net, prim_file, prim_buffer, prim_tty, zlib, zstd, erl_tracer) + shared socket headers/helpers (`socket_int.h`, `socket_util.c`, `socket_tarray.c`, `socket_dbg.c`, `socket_io.h`) + the `efile_*` and dyncall contracts (`prim_file_nif.h`, `prim_file_nif_dyncall.h`) — see `common/CONTEXT.md`
- `unix/` → unix backends: `unix_prim_file.c` (`efile_*` on POSIX), `unix_socket_syncio.c` (`essio_*` non-blocking socket backend using `enif_select`) — see `unix/CONTEXT.md`
- `win32/` → win32 backends: `win_prim_file.c` (`efile_*` on Windows, WCHAR/long-path handling), `win_socket_asyncio.c` (`esaio_*` IOCP/overlapped backend) — see `win32/CONTEXT.md`
- `../beam/` → NIF/driver API implementation (`erl_nif.c`, `erl_nif.h`), `static_nifs_init()` — read-only, owned by another agent
- `../drivers/` → port-driver equivalents (`inet_drv`, `ram_file_drv`, `registry_drv`) — read-only, owned by another agent
- `../sys/` → OS abstraction layer (`sys.h`, `erts_sys_*`) used by the unix/win32 backends — read-only, owned by another agent
- `../utils/` → `make_driver_tab` (generates the NIF/driver registration table) and other build tools
- `../test/` → VM test suites (`efile_SUITE`, `nif_SUITE`, `tracer_SUITE`, …)
- `../../preloaded/src/` → Erlang side of these NIFs (`prim_socket.erl`, `prim_net.erl`, `prim_file.erl`, `prim_buffer.erl`, `zlib.erl`, `erl_tracer.erl`, `erl_init.erl` boot loading)
- `../../../lib/kernel/test/` → socket/prim_net/prim_file/prim_tty test suites (sibling — read-only, escalate writes to the parent)
- `../../../lib/stdlib/test/` → `zstd_SUITE`, `zip_SUITE` (sibling — read-only)

## See Also
- `../CONTEXT.md` → emulator build knobs (`FLAVOR`, `TYPE`), object lists, generated `driver_tab.c`
- `../../CONTEXT.md` → erts build/configure layout, preloaded modules, test levels
- `../internal_doc/` → VM internals notes (scheduler/tracing) relevant when touching dirty-scheduling behaviour here
