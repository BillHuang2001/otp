# erts/emulator/nifs/common/ — Cross-platform NIF modules

## Intent
The cross-platform (OS-independent) C sources of the emulator's static NIFs — all compiled into the BEAM binary by `../Makefile.in` (`NIF_OBJS`/`ESOCK_RUN_OBJS`). One `.c` per NIF module (`<module>_nif.c`, `#define STATIC_ERLANG_NIF 1`, `ERL_NIF_INIT(<module>, ...)`); OS-specific backends live in the sibling `../unix/` and `../win32/` directories, selected by `ERLANG_OSTYPE`.

## API Surface (files)
- NIF modules: `prim_socket_nif.c` (`prim_socket`, esock), `prim_net_nif.c`, `prim_file_nif.c`, `prim_buffer_nif.c`, `prim_tty_nif.c`, `zlib_nif.c`, `zstd_nif.c`, `erl_tracer_nif.c`.
- Shared contracts (headers used by more than one module / by the backends):
  - `socket_io.h` — `ESockIoBackend` vtable (the per-OS I/O backend interface).
  - `socket_syncio.h` (`essio_*`, unix), `socket_asyncio.h` (`esaio_*`, win32).
  - `socket_int.h`, `prim_socket_int.h` — `ESockData`, `ESockDescriptor`, term/conversion macros, `TCREATE`/`TJOIN` = `enif_thread_*` wrappers, `ESOCK_GET_RESOURCE`.
  - `socket_util.{h,c}`, `socket_tarray.{h,c}`, `socket_dbg.{h,c}` — address/option conversion, term arrays, debug printing.
  - `prim_file_nif.h` (`efile_*` contract + `efile_data_t`), `prim_file_nif_dyncall.h` (socket→prim_file resource dyncall ABI).

## prim_socket_nif.c — architecture (see also `../CONTEXT.md`)
- ~19.2k lines, wrapped in `#ifdef ESOCK_ENABLE`; when esock is disabled (`#ifndef ESOCK_ENABLE`) the whole file reduces to an empty `esock_funcs[]` stub and an `on_load` that returns `1` (module load fails), so `socket` falls back to `notsup`.
- Module-level split: `nif_<op>` functions do argument validation and delegate to `io_backend.<op>` (a per-OS `ESockIoBackend`). `io_backend` is set in `on_load` under `#ifdef __WIN32__` (`esaio_*`) vs `#else` (`essio_*`).
- **Exactly ONE NIF resource type** is opened: the socket resource `esocks` (Erlang name `"sockets"`), via `enif_open_resource_type_x` in `on_load`, with `ErlNifResourceTypeInit { esock_dtor, esock_stop, esock_down }`. Its payload is a single `ESockDescriptor` (`enif_alloc_resource(esocks, sizeof(ESockDescriptor))`). There is NO separate select- or completion-resource type: readiness/completion handles are ordinary Erlang references supplied by the caller as NIF arguments.
- Erlang-visible socket value = `{'$socket', SockRef}`; delivered side-messages are `{'$socket', Socket, Tag, Info}` (`esock_mk_socket_msg`).
- Readiness model: `enif_select`-based. Wrappers `esock_select_read`/`esock_select_write` (→ `enif_select_read`/`enif_select_write`) send `{'$socket', Socket, select, SelectRef}` when the fd is ready; `esock_select_stop` uses `ERL_NIF_SELECT_STOP`, `esock_select_cancel` uses `ERL_NIF_SELECT_CANCEL|mode`. `SelectRef` is the caller-supplied op reference (e.g. `nif_recv(Sock, Len, Flags, RecvRef)`).
- Monitoring: `enif_monitor_process` per requestor/controlling process; the resource down-callback `esock_down` (→ `io_backend.down` / `ESOCK_IO_DOWN`) drives cancellation/abort; the down-monitor term is exposed via `enif_make_monitor_term`.
- No NIF-level helper thread in this file and none in the unix backend: `essio_init` ignores its `numThreads` argument (`VOID(numThreads)`) and the unix backend arms `enif_select` instead of spawning threads. The win32 backend (`../win32/win_socket_asyncio.c`) DOES create a pool of `numThreads` `enif_thread_create` threads running `esaio_completion_main` (IOCP, `GetQueuedCompletionStatus`) and reports results with `{'$socket', Socket, completion, {Ref, CompletionStatus}}`. `numThreads` defaults to `2*scheduler_threads` in `on_load`.
- Completion/notification messages sent to Erlang processes (all via `enif_send`): `select`, `abort` (per-request `{Ref,Reason}` or simple `Info`), `close`, `completion` (win32), counter-wrap, and socket-registry `add`/`del`.
- Entry: `ERL_NIF_INIT(prim_socket, esock_funcs, on_load, NULL, NULL, NULL)`; a few entries are `ERL_NIF_DIRTY_JOB_IO_BOUND` (`nif_sendfile`, `nif_ioctl`, `nif_finalize_close`).

## Constraints
- A NIF/backend `.c` must name-match its Erlang module and its `ERL_NIF_INIT` NAME; the static-init symbol (`<mod>_nif_init`) is derived from the object basename (see `../CONTEXT.md`).
- `common/` code must compile on every target — use `__WIN32__`/`ESOCK_ENABLE`/feature-macro guards, never a new OS subdirectory.
- Blocking work must be declared dirty (or shipped to the backend), never done on an ordinary scheduler.
- Object lists are explicit in `../Makefile.in` (`NIF_OBJS`, `ESOCK_RUN_OBJS`, `OS_OBJS`); adding a file here requires editing that list.

## Notes for Agents
- `prim_socket_nif.c` is by far the largest file here (~19k lines) and mixes NIF argument handling, the requestor/monitor bookkeeping, and the select wrappers. The per-OS "does the work" code is out of this directory — grep `essio_*` in `../unix/unix_socket_syncio.c` and `esaio_*` in `../win32/win_socket_asyncio.c`.
- `ESOCK_DISPLAY_ON_LOAD_DETAILS` / `ESOCK_DISPLAY_SOCKADDR_SIZES` (commented out at the bottom of `prim_socket_nif.c`) and `socket_dbg.h`'s `ESOCK_PRINTF`/`esock_dbg_printf` are the in-tree debug switches.

## Routing Table
- No own subdirectories; the OS backends are siblings: `../unix/` (`essio_*`, `efile_*`), `../win32/` (`esaio_*`, `efile_*`).

## See Also
- `../CONTEXT.md` → the NIF set as a whole, `NIF_OBJS`/`OS_OBJS`, `driver_tab.c` registration, boot order.
- `../unix/unix_socket_syncio.c` → unix `essio_*` backend (select-based, no thread pool).
- `../win32/win_socket_asyncio.c` → win32 `esaio_*` backend (IOCP + `esaio_completion_main` threads).
- `../../beam/erl_nif.{c,h}` → NIF API (`enif_select`, monitors, resource types).
