# erts/emulator/nifs/win32/ — Windows OS-specific NIF backends

## Intent
Windows-only C implementations behind the cross-platform NIFs in `../common/`:
`win_socket_asyncio.c` implements the `esaio_*` asynchronous esock backend (I/O Completion Ports) and `win_prim_file.c` implements the `efile_*` file backend (Win32 `CreateFileW`/`ReadFile`/`WriteFile`). `ERLANG_OSTYPE=win32` selects this directory (objects listed explicitly in `../Makefile.in` as `OS_OBJS`/`win_socket_asyncio.o`+`win_prim_file.o`). No emulator-internal NIF module is defined here.

## API Surface
### `win_socket_asyncio.c` (12312 lines) — `esaio_*` IOCP backend
Implements `socket_asyncio.h` (`../common/socket_asyncio.h`), wired into `ESockIoBackend io_backend` under `#ifdef __WIN32__` in `../common/prim_socket_nif.c` (~line 18951).
- Backend entries: `esaio_init` (1186), `esaio_finish` (1503), `esaio_info` (1624), `esaio_command` (1694), `esaio_open_plain` (1739), `esaio_bind` (1856), `esaio_connect` (1909), `esaio_accept` (2279), `esaio_send` (2657), `esaio_sendto` (2772), `esaio_sendmsg` (2898), `esaio_sendv` (3358), `esaio_recv` (4212), `esaio_recvfrom` (4695), `esaio_recvmsg` (5045), `esaio_close` (5441), `esaio_fin_close` (5607), `esaio_cancel_connect` (5694), `esaio_cancel_accept` (5777), `esaio_cancel_send` (5872), `esaio_cancel_recv` (5956), `esaio_ioctl3` (6037), `esaio_ioctl2` (6616), `esaio_dtor` (11463), `esaio_stop` (11541), `esaio_down` (11623), `esaio_down_ctrl` (11805).
- Internal: `esaio_completion_main` (6746, the worker loop), `esaio_add_socket` (12242), `esaio_send_completion_msg` (12259), `mk_completion_msg` (12303), `do_stop` (5503).

### `win_prim_file.c` (1694 lines) — `efile_*` backend
Implements the `efile_*` contract in `../common/prim_file_nif.h`; `prim_file_nif.c` is the only caller.
`efile_marshal_path` (211), `efile_get_handle` (227), `efile_dup_handle` (238, returns `ENOTSUP`), `efile_open` (444), `efile_from_fd` (532), `efile_close` (564), `efile_readv` (683), `efile_writev` (689), `efile_preadv` (708), `efile_pwritev` (720), `efile_seek` (732), `efile_sync` (757), `efile_advise` (771, no-op), `efile_allocate` (783, `ENOTSUP`), `efile_truncate` (795), `efile_read_info` (929), `efile_read_handle_info` (1003), `efile_set_permissions` (1036), `efile_set_owner` (1056, no-op), `efile_set_time` (1064), `efile_read_link` (1144), `efile_list_dir` (1189), `efile_rename` (1243), `efile_make_hard_link` (1372), `efile_make_soft_link` (1383), `efile_make_dir` (1402), `efile_del_file` (1412), `efile_del_dir` (1437), `efile_set_cwd` (1453), `efile_get_device_cwd` (1502), `efile_get_cwd` (1532), `efile_altname` (1536), `windows_to_posix_errno` (1597).

## Design / How it works
### IOCP model (`win_socket_asyncio.c`)
- One global `static ESAIOControl ctrl` (1145). Completion port created in `esaio_init` with `CreateIoCompletionPort(INVALID_HANDLE_VALUE, NULL, 0, numThreads)` (1238-1239).
- Socket handles are associated via `CreateIoCompletionPort((HANDLE)sock, ctrl.cport, (ULONG_PTR)descP, 0)` in `esaio_add_socket` (12245). Called for every opened socket (`esaio_open_plain`) and for each accepted socket (`esaio_accept_accepted` 2575, `esaio_completion_accept_completed` 7667). The completion key is the `ESockDescriptor*`.
- Extension function pointers obtained in `esaio_init` via `WSAIoctl(ctrl.srvInit, SIO_GET_EXTENSION_FUNCTION_POINTER, ...)`: `WSAID_ACCEPTEX`→`ctrl.accept` (1279), `WSAID_CONNECTEX`→`ctrl.connect` (1303), `WSAID_WSASENDMSG`→`ctrl.sendmsg` (1327), `WSAID_WSARECVMSG`→`ctrl.recvmsg` (1352). GUIDs declared 1192-1195 on a throwaway socket from `init_srv_init_socket` (1458).
- **`GetAcceptExSockaddrs` is NOT obtained or used** — it appears only in the module comment (lines 40-42). Accepted-socket addresses come from `SO_UPDATE_ACCEPT_CONTEXT` (`ESAIO_UPDATE_ACCEPT_CONTEXT`, macro 170) plus later `getsockname`/`getpeername`.
- Async ops are issued with the `OVERLAPPED` as the first member of `ESAIOOperation` (struct 423, field `ol` 425). Wrappers: `sock_recv_O`=WSARecv (151/4279), `sock_send_O`=WSASend (157/2731), `sock_recvfrom_O`=WSARecvFrom (4766), `sock_sendto_O`=WSASendTo (2846), `sock_sendv_O`=WSASend vector (3551), `sock_accept_O`=AcceptEx via `ctrl.accept` (134/2399), `sock_connect_O`=ConnectEx via `ctrl.connect` (139/2076), sendmsg/recvmsg via `ctrl.sendmsg`/`ctrl.recvmsg` (3144/5138).

### Completion thread pool
- Thread count: `ioNumThreads = 2*scheduler_threads` (min 2) default, overridable by the `io_num_threads` load-info option (`../common/prim_socket_nif.c` 18938-18944); passed to `esaio_init(numThreads)` (1186). `ctrl.numThreads` set at 1220.
- Threads created in `esaio_init` loop 1393-1448 via `TCREATE` = `enif_thread_create` (macro in `../common/socket_int.h` 910), running `esaio_completion_main` (6746). Threads are ERTS NIF threads (`enif_thread_opts_create`/`TCREATE`), each with its own `ErlNifEnv` (`esock_alloc_env`, 6763).
- Worker loop: `GetQueuedCompletionStatus(ctrl.cport, &numBytes, &descP, &olP, INFINITE)` (6796), `CONTAINING_RECORD(olP, ESAIOOperation, ol)` (6835/6840), then `switch (opP->tag)` (6849) dispatching to `esaio_completion_{terminate,connect,accept,send,sendv,sendto,sendmsg,recv,recvfrom,recvmsg,unknown}` (6977-11392). `FREE(opP)` (6952); loop ends via `TEXIT` (6959).
- Teardown `esaio_finish` (1503): posts `ESAIO_OP_TERMINATE` per thread with `PostQueuedCompletionStatus` (1558), `TJOIN`s each (1588 = `enif_thread_join`), `FREE(ctrl.threads)` (1603). Note the `for (t=0, lastThread=-1, lastThread; ...)` at 1518 uses `lastThread` as the condition (always -1 → truthy).

### Notification back to the Erlang process
- No `enif_select` and no direct `enif_send` in this file. On a pending overlapped op the NIF entry returns `esock_atom_completion` (2500, 4625, 12130) after `esock_reader_push`/`esock_acceptor_push`/`esock_writer_push` records the `ErlNifPid`+`Ref`+env in `readersQ`/`acceptorsQ`/`writersQ` (e.g. 4621-4623).
- When the op completes the worker calls `esaio_send_completion_msg` (12259) → `esock_send_msg` (`../common/prim_socket_nif.c` 17428, wraps `enif_send`) sending `{'$socket', Socket, completion, {Ref, Status}}` (`mk_completion_msg` 12303, form documented at 62-67 and 12296-12299) to `opP->caller`/`opP->data.*.sockRef`. The Erlang side waits for that message.
- Synchronous "can complete immediately" path: e.g. `recv_check_ok` (4380) returns `{ok, Bin}`/`{more, Bin}`/`{error,closed}` directly without a message; a pending/incomplete result goes through `recv_check_pending` (4604).

### Concurrency / race handling
- Only one mutex type is used: `ctrl.cntMtx` (`MCREATE`= `enif_mutex_create`), for the debug counters (`esaio_info` 1633-1642, counter bumps 4576-4580, 4983-4987, 5383-5387, 7300-7304, 7801-7805). Per-socket `readMtx`/`writeMtx` are locked in the accept-completion path (2606-2630, 7699-7723, 7893-7898) and `esaio_dtor`. **No Windows `CRITICAL_SECTION`, no `CreateThread`, no separate thread pool API.**
- Close/stop: `esaio_close` (5441) marks `ESOCK_STATE_CLOSING` and calls `do_stop` (5503); `do_stop` issues `CancelIoEx(sock, NULL)` (5522) to abort all outstanding overlapped ops when `IS_SELECTED`, tolerating `ERROR_NOT_FOUND` (5537) as a benign race. The worker threads then observe `ERROR_OPERATION_ABORTED` via `get_*_ovl_result` (12177/12195/12209/12227 = `GetOverlappedResult`-style via `WSAGetOverlappedResult`) and run the `_aborted` completion paths.
- Cancellation: `esaio_cancel_connect/accept/send/recv` (5694/5777/5872/5956) call `CancelIoEx` on the specific `OVERLAPPED` (5729/5808/5903/5987).
- Process death: `esaio_down` (11623) matches the monitor against closer, ctrl owner, connector, then reader/acceptor/writer queues (`esaio_down_*` 11805-11923) to unqueue and/or close the socket. `ErlNifMonitor` (`MONP`/`DEMONP` = `esock_monitor`/`esock_demonitor`) tracks every waiting process.

### `win_prim_file.c` I/O model
- Handles are opened synchronously — `CreateFileW` is called **without** `FILE_FLAG_OVERLAPPED` (488, 835, 968, 1089, 1165). `OVERLAPPED` structs are used only to position `ReadFile`/`WriteFile` for `pre{read,write}v` and append (`shift_overlapped` 595, `internal_sync_io` 640, `io_op_t` typedef 638), so all file I/O is blocking and runs on dirty I/O schedulers (`ASSERT(enif_thread_type() == ERL_NIF_THR_DIRTY_IO_SCHEDULER)` at 573). No cancellation, no IOCP here.
- Long paths / WCHAR: paths are `efile_path_t` = UTF-16 `ErlNifBinary` (`WCHAR`). `has_invalid_null_termination` (110), `get_full_path` (130) uses `GetFullPathNameW` and always adds the `\\?\` (LP_FILE_PREFIX, 41) or `\\?\UNC\` (LP_UNC_PREFIX, 43) long-path prefix; `normalize_path_result` (248) strips it for Erlang space. `FILE_SHARE_FLAGS` = R|W|DELETE (37).

## Constraints
- Must compile only on win32; the `esaio_*`/`efile_*` symbols are referenced solely from `../common/` (Unix backends live in `../unix/`).
- Changing an `esaio_*`/`efile_*` signature requires updating `../common/socket_asyncio.h` / `../common/prim_file_nif.h` and the `io_backend` table in `../common/prim_socket_nif.c` (~18951).
- `win_prim_file.c` functions may only run on dirty I/O schedulers (asserted); do not assume a normal scheduler.

## Known Issues / Gotchas
- `GetAcceptExSockaddrs` documented in the file header (40-42) is never obtained/used.
- `efile_dup_handle` (238) and `efile_allocate` (783) are unimplemented (`ENOTSUP`); `efile_advise` (771) and `efile_set_owner` (1056) are deliberate no-ops.
- `esaio_command` (1694) always returns `esock_atom_ok` (1724) even on an unrecognized command, discarding `res`.
- `esaio_finish` loop condition at 1518 (`lastThread;`) is a constant-truthy expression, not `t < ...` — intended to be read as `for (t=0, lastThread=-1; ...)`. Verify carefully before touching.
- `ESAIO_OP_DEBUG` (431) is defined but not handled in the dispatch switch.
- Driver/NIF naming rule from the parent node applies to `../common/`, not here: these files build into `beam` via `OS_OBJS`, they are not static NIF modules.

## Dependencies
- `../common/socket_int.h` (macros `MALLOC`=enif_alloc, `FREE`, `MCREATE`/`MLOCK`, `TCREATE`=enif_thread_create, `TEXIT`, `MONP`, `CP_TERM`, `MKA`/`MKT2`, `ENO2T`), `socket_io.h`, `socket_asyncio.h`, `socket_util.h`, `socket_tarray.h`, `socket_dbg.h`; `../common/prim_file_nif.h`.
- `../../config.h` (`ESOCK_ENABLE`), `<ws2tcpip.h>`, `<mswsock.h>`, `<windows.h>`, `<winioctl.h>`, `<strsafe.h>`, `<wchar.h>`.

## Test Strategy
No suites in this directory. Exercised through `../../../lib/kernel/test/` (`socket_SUITE`, `socket_sctp_SUITE`, `prim_file_SUITE`, `net_SUITE`, `gen_tcp_socket_SUITE`) and `../test/` (`efile_SUITE`) on Windows CI.

## Routing Table
- `../common/` → cross-platform NIF modules and the `ESockIoBackend io_backend` table that references `esaio_*`; `socket_asyncio.h` (interface declared here)
- `../unix/` → the mirrored Unix backends (`unix_socket_syncio.c` `essio_*`, `unix_prim_file.c` `efile_*`)
- `../beam/erl_nif.h` → NIF API (`enif_thread_create`, `enif_send`, `ErlNifMonitor`, `enif_mutex_*`)
- `../../../lib/kernel/test/` → socket/file/net test suites (sibling, read-only)
