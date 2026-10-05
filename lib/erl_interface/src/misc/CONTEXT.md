# `erl_interface/src/misc/` — shared/support code of the `ei` C library

## Intent
"the rest of the library" (per `../README.internal`): the self-expanding buffer API (`ei_x_*`), term formatting/printing, portability shims (alloc/lock/errno/threads), the socket-I/O abstraction, and tracing/diagnostic helpers.
These sources are listed as `MISCSRC` in `../Makefile.in` (14 `.c` files) and are compiled four ways — ST / MT / MD / MDD object dirs — into `libei*.a` / `libei*.so`.
No header in this directory is installed: `HEADERS` in `../Makefile.in` is only `../include/ei.h`, `../include/ei_connect.h`, `../include/eicode.h`; `misc/*.[ch]` are shipped as source under `$(RELSYSDIR)/src/misc` only.
Everything here is internal glue around the public `ei` API — the encoder/decoder primitives themselves live in `../encode/` and `../decode/`.

## API Surface (file → responsibility → public API implemented)
| File | Responsibility |
| --- | --- |
| `ei_x_encode.c` | The `ei_x_buff` dynamic-buffer encoder. Public: `ei_x_new`, `ei_x_new_with_version`, `ei_x_free`, `ei_x_append`, `ei_x_append_buf`, and the whole `ei_x_encode_*` family (`string[_len]`, `binary`, `bitstring`, `long`, `ulong`, `char`, `boolean`, `double`, `list_header`, `empty_list`, `version`, `tuple_header`, `map_header`, `atom[_len][_as]`, `pid`, `fun`, `ref`, `port`, `trace`). Also defines the mutable global `int ei_x_extra = 100`. |
| `ei_x_encode.h` | Internal decl of `int x_fix_buff(ei_x_buff*, int szneeded)` (the buffer-growth primitive). |
| `ei_format.c` | `io_lib:format`-style `~` mini-format compiler (`eiformat`, `pformat`/`plist`/`ptuple`/`pmap`/`pquotedatom`/`pstring`/`pdigit`/`patom`, `read_args`). Public: `ei_x_format` (emits the version byte first), `ei_x_format_wo_ver`. |
| `ei_printterm.c` | Pretty-printer for binary-coded terms (`print_term`, `print_string`, `ei_big_to_str`, `xputc`/`xputs`/`xprintf` writing to either `FILE*` or `ei_x_buff*`). Public: `ei_print_term(FILE*, buf, index)`, `ei_s_print_term(char** s, buf, index)`. |
| `get_type.c` | Public `ei_get_type(buf, index, type, len)`: reads the tag at `*index` without advancing it, normalizes small variants (`ERL_SMALL_ATOM_*`→`ERL_ATOM_EXT`, `ERL_SMALL_TUPLE_EXT` kept, `ERL_NEW_PID_EXT`→`ERL_PID_EXT`, `ERL_*_PORT_EXT`→`ERL_PORT_EXT`, `ERL_NEWER_REFERENCE_EXT`→`ERL_NEW_REFERENCE_EXT`, both floats→`ERL_FLOAT_EXT`) and returns a length for length-carrying types. |
| `ei_decode_term.c` | Public `ei_decode_ei_term(buf, index, ei_term*)`: fills the `ei_term` union (still defined in `../../include/ei.h`), dispatching to `ei_decode_double/atom/ref/port/pid` in `../decode/`. Returns 1 = decoded (and only then advances `*index`), 0 = valid but not materialized (string/binary/bitstring/bignum/pass-through), -1 = error. |
| `ei_decode_term.h` | Internal re-declaration of `ei_decode_ei_term` (duplicates the public decl in `ei.h`). |
| `ei_cmp_nc.c` | Public ERTS-order comparison of node containers: `ei_cmp_refs`, `ei_cmp_pids`, `ei_cmp_ports` (ref/port compare node+creation before payload, pid compares serial/num before node; "nc" = node containers). |
| `ei_compat.c` | Public `ei_set_compat_rel(unsigned rel)`; stores into a file-static `compat_rel` (default `EI_COMPAT`/`UINT_MAX`) that nothing in the tree reads. |
| `ei_init.c` | Public `ei_init()` = `ei_init_connect()` then `ei_init_resolve()`, both implemented in `../connect/`. |
| `ei_trace.c` + `ei_trace.h` | Internal Lamport-clock token helper `ei_trace(int query, erlang_trace *token)`; query -1/0/1 = stop tracing / poll (returns token to embed in a message) / start with token. Function-static state. |
| `ei_internal.h` | Internal shared header: `EI_CONN_SAVE_ERRNO__` (honours `EI_HIDE_REAL_ERRNO`), the `EI_TRACE_ERR*/WARN*/CONN*/COMM*/TRACE*` level macros, `extern int ei_tracelevel`, `ei_trace_printf`, `ei_init_connect`, `ei_get_cbs_ctx__`, Win32 `MAXPATHLEN`. |
| `show_msg.c` + `show_msg.h` | Diagnostics. Defines `ei_trace_printf()` (the single output sink of every `EI_TRACE_*` macro; timestamped to `stderr`) plus `ei_show_recmsg(FILE*, erlang_msg*, char*)` and `ei_show_sendmsg(FILE*, header, msgbuf)`, called from `../connect/{eirecv,send,send_reg,send_exit}.c`. |
| `ei_portio.c` + `ei_portio.h` | Socket-I/O abstraction / USSI plumbing (see below), including the default TCP/IPv4 `ei_socket_callbacks` implementation and all `ei_*_ctx__` generic variants. |
| `eidef.h` | Mandatory first include for `ei` sources: pulls in generated `config.h` then `ei.h`; defines `EISMALLBUF 2048`, `socklen_t` fallback, `isfinite`/`finite` shims, `EI_FALLTHROUGH()`, the `uint8/uint16/uint32/int8/int16/int32` typedefs, `ASSERT` (only with `DEBUG`), `EI_UNDEF` (no-op under `VALGRIND`). |
| `eiext.h` | Legacy external-term-format header, now down to `ERL_VERSION_MAGIC 131`, `ERL_MAX`, `ERL_MIN`; the `ERL_*_EXT` tag bytes live in `../../include/ei.h`. |
| `putget.h` | `put8/put16le/put32le/put16be/put32be/put64be` and `get8/get16le/get32le/get16be/get32be/get64be` wire macros, the `FloatExt` union, and decls of `utf8_to_latin1`/`latin1_to_utf8`/`ei_internal_get_atom` (defined in `../decode/decode_atom.c`) and `ei_internal_put_atom` (defined in `../encode/encode_atom.c`), aliased to `get_atom`/`put_atom`. |
| `ei_malloc.c` + `ei_malloc.h` | `ei_malloc`/`ei_realloc`/`ei_free` thin wrappers over `malloc`/`realloc`/`free` (sizes typed `long`); used only by `ei_x_encode.c` and `ei_locking.c`. |
| `ei_locking.c` + `ei_locking.h` | `ei_mutex_t` + `ei_mutex_create/free/lock/unlock`; `ei_mutex_free` first locks (blocking when `nblock == 0`). Declares `extern ei_mutex_t* ei_sockets_lock` (defined in `../connect/ei_connect.c`). |
| `ei_pthreads.c` | Per-thread `erl_errno` emulation and the `ei_m_create/ei_m_destroy/ei_m_lock/ei_m_trylock/ei_m_unlock` primitives used by `ei_locking.c`. |
| `ei_format.h`, `ei_printterm.h` | Include-guard-only headers kept so the corresponding `.c` files have a matching header. |

## Socket I/O / USSI abstraction (`ei_portio.[ch]`)
`ei_socket_callbacks` (the user-supplied-socket-interface vtable) is defined in `../../include/ei.h`; the flags `EI_SCLBK_FLG_FULL_IMPL` and `EI_SCLBK_INF_TMO` are defined there too.
`ei_default_socket_callbacks` (defined here) is the plain TCP/IPv4 implementation: `tcp_socket/close/listen/accept/connect/write[v]/read`, `tcp_hs_packet_header_size` (2), `tcp_handshake_complete` (sets `TCP_NODELAY` + `SO_KEEPALIVE`), `tcp_get_fd`.
For the default callbacks the context pointer IS the fd: `EI_FD_AS_CTX__`/`EI_DFLT_CTX_TO_FD__` cast between `int` and `void*`, and `EI_DFLT_CTX_TO_FD__` maps a negative context to `EBADF`.
`ei_plugin_socket_impl__` (defined here) records that a non-default implementation was installed; `EI_HAVE_PLUGIN_SOCKET_IMPL__`/`EI_SET_HAVE_PLUGIN_SOCKET_IMPL__` read/write it as a plain int (ST), via `__atomic_load_n`/`__atomic_store_n` (MT with the needed `ETHR_HAVE___atomic_*`), or as the constant 1 when neither is available.
`EI_GET_CBS_CTX__` therefore either fast-paths (cbs = default, ctx = fd) or calls `ei_get_cbs_ctx__()` in `../connect/ei_connect.c`, which looks the fd up in the socket registry; `EI_GET_FD__` calls `cbs->get_fd` for non-default callbacks.
The `ei_*_ctx__` functions take callbacks + context; the fd-based `ei_socket__`, `ei_close__`, `ei_listen__`, `ei_accept_t__`, `ei_connect_t__`, `ei_read_fill[_t]__`, `ei_write_fill[_t]__` wrappers call them with the default callbacks.
The `*_t__` variants carry a millisecond timeout (`EI_SCLBK_INF_TMO` for infinite) and implement their own poll/select-style wait loop; the non-`t` variants delegate with an infinite timeout.
`ei_writev_fill_ctx_t__` and `ei_socket_callbacks_have_writev__` exist only when `struct iovec` (`EI_HAVE_STRUCT_IOVEC__`) and `HAVE_WRITEV` are available.
All errors are returned as `errno`-style ints; on Win32 the static `get_error()` converts `WSAGetLastError()` codes, and `MEANS_SOCKET_ERROR`/`IS_INVALID_SOCKET` abstract Win32 vs Unix error/validity checks.

## Portability & ST vs MT
`_REENTRANT` (thread support enabled by configure) is the single switch: in MT builds `ei_locking.c`'s entire body is compiled and Unix `erl_errno` becomes a `pthread_key_t`; in ST builds `ei_locking.c` compiles to nothing and `volatile int __erl_errno` is a plain global.
`erl_errno` is a macro in `../../include/ei.h`: `(*__erl_errno_place())` under `_REENTRANT`/`__WIN32__`, else the global `__erl_errno`; `__erl_errno_place()` is implemented here in `ei_pthreads.c`.
On Win32 `__erl_errno` is either a `__declspec(thread)` variable (`USE_DECLSPEC_THREAD`) or allocated per-thread from a `TlsAlloc()` index; the pthread path falls back to a static int (`fallback_errno`) if `pthread_key_create`/`malloc` fail, deliberately avoiding a fatal exit.
`ei_mutex_t` is a Win32 `HANDLE`, a `pthread_mutex_t*`, or an unused `void *dummy` when no pthread header exists; the documented caveat in `ei_locking.c` is that these locks are NOT recursive on Win32/Solaris, so re-entrant locking self-deadlocks.
`ei_x_buff` (see `../../include/ei.h`) is `{ char* buff; int buffsz; int index; }`; `ei_x_new` allocates `ei_x_extra` bytes, and `x_fix_buff(x, szneeded)` only reallocs when `szneeded + ei_x_extra > buffsz`, adding a further `ei_x_extra` of slack, and leaves `buff`/`buffsz` untouched if `ei_realloc` fails.
Every real `ei_x_encode_*` performs a size pass by calling the corresponding `ei_encode_*(NULL, &i, ...)` from `../encode/`, then `x_fix_buff`, then the same encoder against the real buffer.

## Constraints
- `eidef.h` must be the first include of every `ei` C source in this directory; the only exceptions are `ei_compat.c` and `ei_init.c`, which include `"ei.h"` (plus `ei_internal.h`) directly, and `../prog/ei_fake_prog.c`, which uses `config.h` directly.
- `eidef.h` typedefs `uint8/uint16/uint32/int8/int16/int32` unconditionally (marked "FIXME use configure"), so do not also include headers defining those names.
- `putget.h` needs `EI_ULONGLONG` and `erlang_char_encoding` from `ei.h`, so it must be included after `eidef.h`/`ei.h`.
- New external-term tag constants belong in `../../include/ei.h`; `eiext.h` is a legacy remnant kept only for its three macros.
- Track new source files by adding them to `MISCSRC` in `../Makefile.in` (the build also derives four object lists from it); the vpath build compiles all `.c` names flat, so duplicate basenames across directories are not allowed.
- Any new `.c` here is automatically compiled into all four library variants — it must therefore build with `-ansi`-ish settings on Win32, Unix ST/MT, and against both `--enable-threads` and `--disable-threads` (`_REENTRANT` absent).

## Known Issues / Gotchas
- `ei_set_compat_rel()` is effectively a no-op: the `compat_rel` it writes is never read anywhere in the tree.
- `ei_trace()` keeps state (`save_token`, `tracing`, `clock`) in function statics and carries a "FIXME problem for threaded?" comment — it is not thread-safe.
- `ei_trace_printf()` (in `show_msg.c`) uses `ctime()` + `sprintf`/`vsprintf` into a fixed 2048-byte stack buffer, so it is neither thread-safe nor safe for very long formatted output; it also writes to `stderr` unconditionally once the level check in `ei_internal.h` passes.
- `ei_tracelevel` itself is defined in `../connect/ei_connect.c` (with `ei_set_tracelevel`/`ei_get_tracelevel`), not here — `ei_internal.h` only declares it.
- `ei_x_extra` is a mutable, non-static global: reassigning it changes the initial allocation size *and* the realloc slack of every `ei_x_buff` in the process.
- `x_fix_buff` returns `int` used as a boolean, and on realloc failure leaves `x->buff`/`x->buffsz` intact so callers can return -1 without freeing — callers must check the return value, not `x->buff`.
- Because `ei_x_encode_*` encodes twice, the `../encode/` primitives it calls must remain side-effect free and must tolerate `buf == NULL` (size-only pass).
- `ei_s_print_term` reuses a caller-provided `*s` buffer by constructing a temporary `ei_x_buff` with `buffsz = BUFSIZ`, so a caller-supplied buffer smaller than `BUFSIZ` is over-reported and can be overflowed; the string is always NUL-terminated by an appended `""`.
- `ei_decode_ei_term` writes only `ei_type`/`arity`/`size` for strings, binaries, bitstrings and large bignums (returns 0 without advancing `*index`) — callers must handle the 0 case explicitly rather than assuming `term` is fully populated.
- `ei_term`/`ei_decode_ei_term` are still part of the public API in `ei.h`; the removed legacy types were `ETERM`/`erl_term`, not `ei_term`, so `ei_decode_term.c` is fully functional and does not depend on the old `erl_interface` term library.
- `ei_decode_term.c` pulls in `../decode/` (`ei_decode_double/atom/ref/port/pid`), so this "support" file is not standalone.
- `show_msg.c` duplicates term-walking logic (`show_term`, `ei_efprint`, `ei_decode_skip_bignum`, `printable_list_p`) instead of using `ei_printterm.c`, with an explicit FIXME noting the two should share an implementation.
- `ei_internal.h`/`ei_locking.h` reach for `ei_tracelevel`/`ei_sockets_lock` defined in `../connect/`, and `ei_portio.h` reaches for `ei_get_cbs_ctx__` there — this directory is not layered above `../connect/`.
- `../README.internal` is partly stale (it still mentions an `ei_trace_distribution` global and an `errno` global, and describes `ei_decode_term()` as working on `ETERM`); trust the code, not that file.

## Notes for Agents
- Build/compile check: `make` in `../` (i.e. `lib/erl_interface/src`) builds all four variants; `../prog/ei_fake_prog.c` (see `../Makefile.in`) is a deliberate compile+link completeness test over the documented public API — when you add or remove a public function, that file is the canary.
- Headers here are internal; changes to them do not break users, but changes to `../../include/ei.h` do (it is installed into `$(RELEASE_PATH)/usr/include`).
- `EI_NO_DEPR_WARN` is set by the build for all of these files.

## Test Strategy
The C code here is exercised through Common Test suites in `../../test/` whose helper programs link `libei`:
- `ei_format_SUITE` (`ei_format_SUITE_data/ei_format_test.c`) covers `ei_x_format`/`ei_x_format_wo_ver`/`ei_x_new` against `ei_format_test_cases.hrl`.
- `ei_print_SUITE` (`ei_print_SUITE_data/ei_print_test.c`) covers `ei_print_term`/`ei_s_print_term` plus `ei_get_type` against `ei_print_test_cases.hrl`.
- `ei_decode_encode_SUITE` (`ei_decode_encode_test.c`) round-trips most of the `ei_x_encode_*` family, including bitstring, fun, map, pid/port/ref/trace.
- `ei_decode_SUITE` (`ei_decode_test.c`, case `test_ei_cmp_nc`) is the only coverage of `ei_cmp_pids/ports/refs`, validating ordering against Erlang term ordering semantics.
- `ei_connect_SUITE` and `ei_tmo_SUITE` use `ei_get_type` and `ei_cmp_pids`; `all_SUITE`'s `ei_runner.c` exercises `ei_decode_ei_term`.
- Not directly unit-tested: `ei_trace.c`, `show_msg.c`'s `ei_trace_printf`/`ei_show_*msg` dumps, `ei_compat.c`'s `ei_set_compat_rel` (only called for its side-effect-free return in `ei_accept_SUITE_data/ei_accept_test.c`), and the portability fallbacks (`ei_pthreads.c` TLS paths, `eidef.h` shims) — these depend on platform/tracing configuration.

## See Also
- `../encode/` — the `ei_encode_*` primitives the `ei_x_encode_*` wrappers size/delegate to; owns `ei_internal_put_atom`.
- `../decode/` — `ei_decode_*` used by `ei_decode_term.c`; owns `utf8_to_latin1`/`latin1_to_utf8`/`ei_internal_get_atom` declared in `putget.h`.
- `../connect/` — defines `ei_tracelevel`, `ei_set_tracelevel`, `ei_get_tracelevel`, `ei_sockets_lock`, `ei_get_cbs_ctx__`; the only caller of `ei_trace()` and of `ei_show_*msg`.
- `../../include/ei.h` — public API plus the `ei_x_buff`, `ei_term`, `erlang_*` and `ei_socket_callbacks` types used throughout this directory.
- `../README.internal` — source-writing conventions and the global-variable policy ("avoid globals for thread support and VxWorks"); partly stale.
- `../prog/ei_fake_prog.c` — compile/link completeness check over the public API.
- `../../test/` — Common Test suites listed above (sibling directory; read-only from here, escalate write needs to the parent).
