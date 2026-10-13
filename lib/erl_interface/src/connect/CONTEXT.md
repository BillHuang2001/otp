# erl_interface — `src/connect/` (distribution connection layer of `ei`)

## Intent
Implements the C-side of the Erlang distribution protocol for the `ei` API: node identity/cookie setup, the EPMD-based connect/handshake, the listen/accept handshake, and the send/receive of distribution control messages over a connected socket.
This directory is where socket I/O is bound to the `ei_socket_callbacks` indirection, so all I/O here goes through the `misc/ei_portio.c` helpers, never through raw `read()`/`write()` on a fd.

## API Surface (public `ei.h` functions, by owning file)
`ei_connect.c` (the core, ~2900 lines) owns node/connection lifecycle and most user-facing APIs:
- Init: `ei_init_connect`, `ei_connect_init`, `ei_connect_init_ussi`, `ei_connect_xinit`, `ei_connect_xinit_ussi`.
- Connect: `ei_connect`, `ei_connect_tmo`, `ei_connect_host_port[_tmo]`, `ei_xconnect[_tmo]`, `ei_xconnect_host_port[_tmo]`.
- Listen/accept: `ei_listen`, `ei_xlisten`, `ei_accept`, `ei_accept_tmo`, `ei_close_connection`.
- Raw receive/send: `ei_receive[_tmo]`, `ei_send[_tmo]`, `ei_reg_send[_tmo]`, `ei_do_receive_msg`, `ei_receive_msg[_tmo]`, `ei_xreceive_msg[_tmo]`.
- RPC: `ei_xrpc_to`, `ei_rpc_to`, `ei_rpc_from`, `ei_rpc`.
- Identity/ids: `ei_make_pid`, `ei_make_ref`, `ei_self`, `ei_thisnodename`, `ei_thishostname`, `ei_thisalivename`, `ei_thiscookie`, `check_initialized_node` (static).
- Introspection/internal: `ei_fd_to_cnode`, `ei_distversion`, `ei_getfdcookie`, `ei_get_cbs_ctx__`, `ei_set_tracelevel`, `ei_get_tracelevel`.
- Statics: the whole handshake (`send_name`/`recv_name`, `send_status`/`recv_status`, `send_challenge`/`recv_challenge`, `send_challenge_reply`/`recv_challenge_reply`, `recv_complement`, `send_challenge_ack`/`recv_challenge_ack`, `read_hs_package`), `gen_challenge`/`gen_digest`/`md_32`/`hex`, `get_cookie`/`get_home`, `dyn_gethostbyname_r`, `ip_address_from_hostname`, `close_connection`/`abort_connection`, `preferred_flags`.
`eirecv.c` owns the shared receive engine:
- `ei_recv_internal` (internal, also used by `../global/`), `ei_receive_encoded`, `ei_receive_encoded_tmo`; static `send_unlink_id_ack`.
`send.c`, `send_reg.c`, `send_exit.c` own the encoded-write primitives:
- `send.c`: `ei_send_encoded`, `ei_send_encoded_tmo`; `send_reg.c`: `ei_send_reg_encoded`, `ei_send_reg_encoded_tmo`; `send_exit.c`: `ei_send_exit`, `ei_send_exit_tmo`.
`ei_resolve.c` owns name resolution: `ei_gethostbyname`, `ei_gethostbyaddr`, `ei_gethostbyname_r`, `ei_gethostbyaddr_r`, `ei_init_resolve`.

## Internal Headers
- `ei_connect_int.h` — the distribution capability flags (`DFLAG_*`), `DistFlags`, `ERL_UNLINK_ID[_ACK]` tags, `DEFBUF_SIZ`/`MAX_RECEIVE_BUF`, and the intra-library prototypes: `ei_fd_to_cnode`, `ei_distversion`, `ei_getfdcookie`, `ei_thiscookie`, `ei_do_receive_msg`.
- `eirecv.h` — only `ei_recv_internal`.
- `eisend.h` — `ei_send_exit[_tmo]`; `ei_send_link`/`ei_send_unlink[_tmo]` are `#if 0`'d out and must not be re-enabled without protocol work.
- `ei_resolve.h` — only `ei_init_resolve` (called from `../misc/ei_init.c`).
None of these are installed; the installed public header set is only `../include/ei.h`, `../include/ei_connect.h` and `../include/eicode.h` (`../include/ei_global.h` is not in `HEADERS` and is not installed — see the Known Issue in `../global/CONTEXT.md`).

## Socket Callbacks / USSI Indirection
`ei_cnode.cbs` + `setup_context` hold the socket implementation; the `_ussi` variants of init let a user supply their own `ei_socket_callbacks` (version 1 layout, `EI_SOCKET_CALLBACKS_SZ_V1` = up to `get_fd`, size-checked in `ei_connect_xinit_ussi`, too small ⇒ `ERL_ERROR`).
- When `cbs != &ei_default_socket_callbacks`, `EI_SET_HAVE_PLUGIN_SOCKET_IMPL__` is set (it affects `misc/ei_portio.c` behavior).
- Every function here resolves `(cbs, ctx)` from the fd via `EI_GET_CBS_CTX__` (`misc/ei_portio.h`) and then uses `ei_socket_ctx__`, `ei_connect_ctx_t__`, `ei_listen_ctx__`, `ei_accept_ctx_t__`, `ei_close_ctx__`, `ei_read_fill_ctx_t__`, `ei_write_fill_ctx_t__`, `ei_writev_fill_ctx_t__`.
- Optional callbacks `handshake_packet_header_size` (2 or 4 byte packet header), `connect_handshake_complete`, `accept_handshake_complete` are honoured; `ei_accept_tmo` tolerates a listen fd created outside `ei_listen` by falling back to `&ei_default_socket_callbacks` with `ctx = EI_FD_AS_CTX__(lfd)`, but then requires `ec->cbs == cbs`.

## Per-fd State (the `ei_socket_info` table)
`struct ei_socket_info { int socket; ei_socket_callbacks *cbs; void *ctx; int dist_version; ei_cnode cnode; char cookie[...]; }` maps fd → {dist version, copy of the `ei_cnode`, cbs/ctx}.
- Two mutually exclusive implementations selected by `EI_DISABLE_SEQ_SOCKET_INFO`:
  - Lock-free, fd-indexed segmented table (`EI_SOCKET_INFO_SEG_BITS = 5`) with `__atomic_*` ops; `socket_info_data` is allocated lazily by `init_socket_info()` sized by `sysconf(_SC_OPEN_MAX)` (fallback 1024).
  - Legacy linear array `ei_sockets`/`ei_n_sockets`/`ei_sz_sockets` guarded by `ei_sockets_lock` (`ei_mutex_t`), used on Win32 and when `_REENTRANT` is set but 64/32-bit atomics are unavailable.
- Other file/module-static state: `ei_connect_initialized`, `ei_tracelevel` (env `EI_TRACELEVEL` or `ERL_DEBUG_DIST`), `pid_mtx`/`ref_mtx` + `ref_count` (or the atomic `ec->pidsn`/`ref_count` equivalents), `ei_gethost_sem`/`ei_resolve_initialized` in `ei_resolve.c`, `null_cookie` (empty string).
- The table is the only place where `ei` uses locks; `ei_locking.h` mutexes are used here (and in `ei_resolve.c` for the non-reentrant `gethostbyname` fallback), not by `ei`'s encoding layer.

## Distribution Version / Flags Handling
- `ei_distversion(fd)` returns the stored `dist_version`: `-1` for an unknown fd, `0` for a listen socket registered by `ei_xlisten`, and `EI_DIST_6` (hardcoded) for sockets established by `ei_connect_helper`/`ei_accept_tmo`.
- `EI_DIST_5`/`EI_DIST_6`/`EI_DIST_LOW`/`EI_DIST_HIGH` live in `../epmd/ei_epmd.h`; `EI_DIST_LOW = EI_DIST_HIGH = EI_DIST_6`, so `ei_connect_helper` rejects a peer whose EPMD-negotiated version is below 6 (i.e. pre-OTP-23 nodes cannot be connected to).
- `preferred_flags()` announces `DFLAG_MANDATORY_25_DIGEST | DFLAG_DIST_MANDATORY | DFLAG_DIST_MONITOR | DFLAG_SMALL_ATOM_TAGS` (plus `DFLAG_NAME_ME` when the local node has no alive name); the peer must support `DFLAG_DIST_MANDATORY | DFLAG_HANDSHAKE_23` or the handshake fails.
- Handshake tags (plain `char` literals, no macros): `send_name`/`send_challenge` write `'N'`; `recv_name` accepts `'n'` (dist 5) or `'N'`; `send_status`/`recv_status` use `'s'`; `send_challenge_reply`/`recv_challenge_reply` use `'r'`; `send_challenge_ack`/`recv_challenge_ack` use `'a'`; `recv_complement` reads `'c'`. `ei`'s own connector always sends `'N'` and never `'n'` or `'c'`. The same characters are also external-term-format tags in `../include/ei.h` (`'a'`=`ERL_SMALL_INTEGER_EXT`, `'s'`=`ERL_SMALL_ATOM_EXT`, `'n'`=`ERL_SMALL_BIG_EXT`, `'r'`=`ERL_NEW_REFERENCE_EXT`, `'c'`=`ERL_FLOAT_EXT`, `'e'`=`ERL_REFERENCE_EXT`, `'p'`=`ERL_NEW_FUN_EXT`) — different context, don't confuse.
- Auth: `gen_challenge()` (MD5 over time/pid/uid/hostid/etc.) and `gen_digest()` = `MD5(cookie ++ decimal_challenge_string)`, using `erts_md5` (`erl_md5.h` from `erts/emulator/beam/`); digest mismatch ⇒ authorization failure.
- Dynamic (USSI) node name: if `thisalivename` is NULL the local name is left empty, `send_name` announces `DFLAG_NAME_ME`, and `recv_status` parses the `snamed:<len><nodename><creation>` reply to fill in `thisnodename`/`thisalivename`/`creation`/`self`.

## Wire-Format Conventions Here
- Control packets: 4-byte big-endian length, `ERL_PASS_THROUGH` (`'p'`), external-term-encoded control tuple, then the payload (`ei_send_encoded*`) or the reason string (`ei_send_exit*`).
- Receiving: `ei_recv_internal` decodes `{ SEND, Cookie, ToPid }`, `{ REG_SEND, From, Cookie, ToName }`, `LINK`/`UNLINK`/`GROUP_LEADER`, `EXIT`/`EXIT2`, the `_TT` trace variants, and `ERL_UNLINK_ID` (surfaced to the caller as `ERL_UNLINK` after replying with `UNLINK_ID_ACK`); a zero length field is a tick, answered with zeroes and returned as `ERL_TICK`.
- Trace tokens: tracing is switched off at each receive (`ei_trace(-1,NULL)`); `_TT` tags are turned on and then reported to the caller with `msg->msgtype -= 10`; the `send*` functions add the `+10` tag only when `ei_distversion(fd) > 0` and `ei_trace(0,NULL)` yields a token.
- Return/errno convention: `ERL_ERROR`(-1)/`ERL_MSG`(1)/`ERL_TICK`(0)/`ERL_TIMEOUT`, `ERL_CONNECT_FAIL`, `ERL_NO_PORT`; the global `erl_errno` is set via `EI_CONN_SAVE_ERRNO__` (`EAGAIN` on tick, `EMSGSIZE`, `ETIMEDOUT`, `EBADF`, `EBADMSG`, `EHOSTUNREACH`).

## Constraints
- Include `"eidef.h"` first in every `.c` here — it pulls in the generated `config.h`; then `eiext.h`, `ei_connect_int.h`, `ei_portio.h`, `ei_internal.h`, `ei_locking.h` as needed.
- Never touch a socket directly: resolve cbs/ctx and use the `ei_*_ctx_t__` / `ei_*_fill_ctx_t__` helpers so user-supplied socket implementations keep working.
- Keep `DFLAG_*` and the mandatory-flag sets in sync with `erts/emulator/beam/dist.h`; `ERL_*` control tags come from `include/ei.h` (`ERL_UNLINK_ID`/`_ACK` are local to `ei_connect_int.h`).
- Wire data must use `uint8/uint16/uint32` and the `put*be`/`get*be` macros from `putget.h`.
- Statics in `ei_connect.c` are filesystem/path dependent (`get_cookie` reads `$HOME/.erlang.cookie`, else `./.erlang.cookie`); do not add new global mutable state — the fd table plus `erl_errno` is the extent of it.

## Known Issues
- `(ms == 0)` means "infinite" for every `_tmo` variant (`tmo = ms == 0 ? EI_SCLBK_INF_TMO : ms`); a zero (poll) timeout cannot be expressed, and `ei_rpc_from` maps any negative timeout to infinite.
- The per-fd cookie stored by the table is always `null_cookie` ("") because all three `put_ei_socket_info` call sites pass it, so `ei_getfdcookie()` returns `""`; the real cookie is only used from `ec->ei_connect_cookie` during the handshake.
- `ei_accept_tmo` ignores failures from `EI_GET_FD__` and from `cbs->handshake_packet_header_size` (it saves `erl_errno` and continues instead of `goto error`), so `fd`/`pkt_sz` may be used uninitialized.
- In the segmented fd-table variant, `get_ei_socket_info`/`put_ei_socket_info` dereference `socket_info_data` without a NULL check, so any fd-based `ei_` call made before an `ei_*init*()` has run can crash instead of failing with `EBADF`.
- `ei_make_pid`/`ei_make_ref` print to stderr and call `exit(1)` (not `ERL_ERROR`) when `ei_connect_initialized` is false; on a too-long/unset node name they return an error after writing invalid UTF-8 into `pid->node`/`ref->node` so that later encoding fails.
- `ei_receive_tmo` returns `EMSGSIZE` without draining an oversized message (`FIXME` in the code), unlike `ei_recv_internal` with `staticbufp` set, which flushes and discards the rest of the packet; `ei_recv_internal` reallocs the caller's buffer when `staticbufp == 0`.
- `ei_do_receive_msg`'s `staticbuffer_p && msglen > x->buffsz` check is unreachable: `ei_recv_internal` already returns `ERL_ERROR` with `EMSGSIZE` in that case.
- `ei_thiscookie` is declared internal but looks like public API (marked `FIXME` in the source).
- `ei_fd_to_cnode` returns a pointer into the fd table; it is invalidated by `ei_close_connection`.

## Build
- Sources are listed as `CONNECTSRC` (`connect/ei_connect.c`, `ei_resolve.c`, `eirecv.c`, `send.c`, `send_exit.c`, `send_reg.c`) in `../Makefile.in`; `vpath %.c connect:encode:decode:misc:epmd:global:erts` plus `-Iconnect` in `INCFLAGS` allow plain `#include "ei_connect_int.h"`.
- Output goes into `libei*.a`/`libei*.so` in `../../obj.*/$(TARGET)` and `../../lib/$(TARGET)`.

## Test Strategy
- Exercised from `../../test/`: `ei_connect_SUITE` (`groups default|ussi`; cases `ei_send`, `ei_reg_send`, `ei_reg_send_large`, `ei_format_pid`, `ei_send_funs`, `ei_threaded_send`, `ei_connect_host_port_test`, `ei_rpc`/`rpc_test`, `ei_set_get_tracelevel`, `ei_make_ref`, `ei_make_pid`, `ei_link_unlink`), `ei_accept_SUITE` (`ei_accept`, `hopeful_random`, `ei_threaded_accept`, `monitor_ei_process`) and `ei_tmo_SUITE` (`framework_check`, `ei_connect_unreachable_tmo`, `ei_send_failure_tmo`, plus `default|ussi` groups `ei_recv_tmo`, `ei_accept_tmo`, `ei_connect_tmo`, `ei_send_tmo`, `ei_dflags`).
- Each suite has a `*_SUITE_data/` dir with C programs (`ei_connect_SUITE_data/ei_connect_test.c`, `einode.c`, `ei_tmo_SUITE_data/ei_tmo_test.c`) built against the just-built `libei` and driven as external cnode processes by `test/runner.erl`; shared harness code (`ei_runner.c`, `gccifier`) lives in `test/all_SUITE_data/`.
- Run with `make test` in `lib/erl_interface` (or a single suite via `ct_run`) after building the library; the `ussi` groups are the only coverage of the user-supplied socket-callback path.

## See Also
- `../encode/`, `../decode/` — term encoding used for the control tuples built here; `../epmd/` — port lookup (`ei_epmd_port_tmo`) and the `EI_DIST_*` constants; `../misc/` — `ei_portio.c` (socket-callback I/O), `ei_locking.h`, `ei_internal.h` (`erl_errno`), `ei_trace.*`, `putget.h`, `eidef.h` (generated `config.h`); `../global/` — calls `ei_recv_internal`/`ei_send_reg_encoded`; `../prog/` — `ei_fake_prog.c` stubs.
- `../../include/ei.h` — public declarations and `ei_socket_callbacks`/`ei_cnode`; `../../include/ei_connect.h` is only a wrapper that includes `ei.h`.
- `../../test/`, and `../../../../erts/emulator/beam/` (`dist.h`, `erl_md5.h`) for the authoritative distribution protocol and MD5 implementation.
