# erts/emulator/nifs/common/ — Cross-platform built-in (static) NIF sources

## Intent
The cross-platform C sources of the NIFs compiled **into the BEAM emulator**: `prim_socket`, `prim_net`, `prim_file`, `prim_buffer`, `prim_tty`, `zlib`, `zstd`, `erl_tracer`, plus the shared socket/net headers and helpers.
Everything here must compile on all targets (use `ESOCK_ENABLE`/`__WIN32__` guards); the OS-specific backends live in sibling `../unix/` and `../win32/`.
Registration, `STATIC_ERLANG_NIF` semantics, preloaded membership and the `NIF_OBJS` list are documented in `../CONTEXT.md`.

## API Surface — file map (line anchors verified against source)
- `prim_socket_nif.c` (19143 lines) — NIF module **`prim_socket`** (`ERL_NIF_INIT` at 19141; empty stub under `#ifndef ESOCK_ENABLE` at 50-66).
  - NIF function table `esock_funcs[]` = **lines 18625-18677, 40 entries** (28 distinct `nif_*` names declared by the `ESOCK_NIF_FUNCS` macro at 1127-1155). Dirty-annotated: `nif_sendfile` (18649-18651), `nif_ioctl` (18665-18667), `nif_finalize_close` (18676).
  - `on_load` declared 2170, defined **18714**; selects the backend and opens the resource.
  - Backend vtable type `ESockIoBackend` is **defined here at lines 1177-1245** (NOT in `socket_io.h`); the single instance is `static ESockIoBackend io_backend = {0}` at **2972**, driven by the `ESOCK_IO_*` macros at 2976-3100+.
  - Backend selection in `on_load`: `__WIN32__` → `esaio_*` (lines 18949-19005), else unix `essio_*` (19007-19085); `ESOCK_IO_INIT(ioNumThreads)` at 19090.
  - Resource: `ErlNifResourceType* esocks` + `ErlNifResourceTypeInit esockInit` (dtor/stop/down) at **2961-2966**; opened via `enif_open_resource_type_x(env, "sockets", &esockInit, ERL_NIF_RT_CREATE, NULL)` at **19098**; descriptors allocated by `esock_alloc_descriptor()` with `enif_alloc_resource(esocks, sizeof(ESockDescriptor))` at **16911**.
- `prim_net_nif.c` (5133 lines) — NIF module **`prim_net`** (`ERL_NIF_INIT` 5132; stub 42-55). Table `net_funcs[]` 4981-5006 (**15 entries**). Resource `net` opened at 5119. Does **not** use the socket `io_backend` (fully independent of prim_socket).
- `prim_file_nif.c` (1404 lines) — NIF module **`prim_file`** (`ERL_NIF_INIT` 221). Defines the `dyncall` resource callback (see dyncall note below).
- `prim_buffer_nif.c` (515 lines) — NIF module **`prim_buffer`** (`ERL_NIF_INIT` 98). Table `nif_funcs[]` 86-96 (**9 entries**). Resource `rtype_buffer` = `"gc_buffer"`, dtor `gc_buffer` (146), opened at 116. Payload `buffer_data_t` 58-70.
- `prim_tty_nif.c` (1340), `zlib_nif.c` (996), `zstd_nif.c` (1118), `erl_tracer_nif.c` (260) — one NIF module each, no per-module tables documented here.
- `socket_io.h` (283 lines) — the `ESockIO*` function-pointer typedefs (38-281) + `ESOCK_IO_OK`/`ESOCK_IO_ERR_UNSUPPORTED` (35-36). **The `ESockIoBackend` struct itself is NOT here** (see `prim_socket_nif.c`).
- `socket_syncio.h` (230 lines) — `essio_*` declarations (unix sync backend). `socket_asyncio.h` (203 lines) — `esaio_*` declarations (win32 async backend). Both are exactly the vtable member signatures.
- `prim_file_nif_dyncall.h` (48 lines) — the cross-NIF dup ABI (`enum` 29-31, `struct prim_file_nif_dyncall` 34-37, `struct prim_file_nif_dyncall_dup` 40-45).
- `prim_file_nif.h` (266 lines) — the `efile_*` C interface + `efile_data_t` (114-121), implemented by `../unix/unix_prim_file.c` / `../win32/win_prim_file.c`.
- `socket_int.h` (925 lines) — shared "utility" types usable by both socket and net: `ESockAddress` (closes at 156), `BOOLEAN_T` (160), `SOCKLEN_T`/`SOCKOPTLEN_T` (207-216).
- `prim_socket_int.h` (877 lines) — socket-global types: `ESockData` (342-383), `ESockDescriptor` (392-551), `ESockAssocId` (327/333), `ESockCounter` (127-163), `ESockMonitor`/`ESockRequestor`/`ESockMeta`, plus `esock_alloc_descriptor` decl (568) and the `ESOCK_OPERATOR_FUNCS_*` macros (717-750). Includes `socket_int.h` (line 34).
- Shared helpers: `socket_util.h`/`.c` (387/3751), `socket_tarray.h`/`.c` (49/149), `socket_dbg.h`/`.c` (55/186).

## Constraints
- File naming is load-bearing: `<module>_nif.c` + `ERL_NIF_INIT(<module>,...)`; the init symbol `<module>_nif_init` is derived from the object basename by `../utils/make_driver_tab`.
- New NIF objects must be added to the explicit `NIF_OBJS`/`ESOCK_RUN_OBJS`/`OS_OBJS` lists in `../Makefile.in`; the wildcard `NIF_COMMON_SRC` drives only dependency scanning / `compile_commands.json`.
- OS-specific code belongs in `../unix/` or `../win32/`, never here.

## Known Issues / Notes for Agents
- **`ESockIoBackend` is defined in `prim_socket_nif.c` (1177-1245), not in `socket_io.h`**; `socket_io.h` only declares the member typedefs. `prim_socket_int.h` is the socket-global header; `socket_int.h` is the shared socket+net header.
- **The `enif_dynamic_resource_call` that dups an efile handle is in `../unix/unix_socket_syncio.c:3594` (inside `essio_sendfile_start`), NOT in `prim_socket_nif.c`.** `prim_socket_nif.c` merely `#include`s `prim_file_nif_dyncall.h` at line 405 and never calls the dyncall API. The `prim_file` side registers the callback in `prim_file_nif.c`: `static ErlNifResourceDynCall dyncall` (227), `callbacks.dyncall = dyncall` (276), `dyncall_dup()` (426), dispatcher `dyncall()` (462).
- The same source file appears twice: the `#ifndef ESOCK_ENABLE` stub and the real `#else` body (e.g. `prim_socket_nif.c`, `prim_net_nif.c`). When `--disable-esock` the stub arrays are empty and `on_load` returns failure.
- `esock_funcs[]`/`net_funcs[]` are pop-in tables: several NIF names have multiple arities (e.g. `nif_open`/2 and /4; `nif_getopt`/3 and /5; three `nif_sendfile` arities).

## Routing Table
- `../unix/` → unix backends: `unix_prim_file.c` (`efile_*`), `unix_socket_syncio.c` (`essio_*` + the efile dyncall site)
- `../win32/` → win32 backends: `win_prim_file.c` (`efile_*`), `win_socket_asyncio.c` (`esaio_*`)
- `../beam/` → `erl_nif.c`/`erl_nif.h` (NIF API, `static_nifs_init`, `enif_dynamic_resource_call` impl at 3297) — read-only, another owner
- `../utils/make_driver_tab` → generates the NIF/driver registration table
- `../CONTEXT.md` → static-NIF registration, `NIF_OBJS`, preloaded boot order
