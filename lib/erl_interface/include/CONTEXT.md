# erl_interface/include

## Intent
The public, installed C API surface of the `ei` library — the header contract that external C programs, C-nodes and other OTP C code compile against.
Contains declarations only; every implementation lives in `../src/` (connect/, decode/, encode/, epmd/, global/, misc/).
There is no `erl_interface.h` and no legacy `erl_*` C API in this tree: `ei` is the whole interface.

## API Surface
Four headers, all `#ifndef`-guarded, all `extern "C"`-wrapped for C++ consumers:

- `ei.h` (~720 lines) — the complete interface; everything else is a stub or a subset.
- `ei_connect.h` — backwards-compatibility stub, only `#include "ei.h"` (contents marked "Dummy for now....").
- `eicode.h` — backwards-compatibility stub, only `#include "ei.h"` (contents marked "Dummy for now....").
- `ei_global.h` — `#include "ei.h"` plus re-declaration of the four global name-service functions `ei_global_names`, `ei_global_whereis`, `ei_global_register`, `ei_global_unregister` (already declared in `ei.h`).

`ei.h`, grouped by area:
- Compile-time flags: `EI_HAVE_TIMEOUT`, `USE_EI_UNDOCUMENTED` (both defined by the header itself); `EI_NO_DEPR_WARN` (user-settable, disables the deprecated attribute).
- Platform adaptation: `winsock2.h` / `windows.h` / `winbase.h` and `typedef LONG_PTR ssize_t` on `__WIN32__`, otherwise `<sys/types.h>`; `<stdio.h>` (for `FILE`), `<errno.h>`, and `<netdb.h>` on non-Windows.
- Deprecation mechanism: `EI_DEPRECATED_ATTR` / `EI_DEPRECATED_ATTR_NAME`, built from `__has_attribute(deprecated)` unless `EI_NO_DEPR_WARN` is set; currently applied to no declaration.
- Error/status codes: `ERL_ERROR`, `ERL_NO_DAEMON`, `ERL_NO_PORT`, `ERL_CONNECT_FAIL`, `ERL_TIMEOUT`, `ERL_NO_REMOTE`, plus `ERL_TICK` / `ERL_MSG` / `ERL_NO_TIMEOUT`, and backstop defines of `EMSGSIZE`, `ETIMEDOUT`, `EHOSTUNREACH` to `EIO` where the platform lacks them.
- Distribution control-message tags: `ERL_LINK`, `ERL_SEND`, `ERL_EXIT`, `ERL_UNLINK`, `ERL_NODE_LINK`, `ERL_REG_SEND`, `ERL_GROUP_LEADER`, `ERL_EXIT2`, `ERL_PASS_THROUGH`, and the trace variants `ERL_*_TT`, `ERL_MONITOR_P`, `ERL_DEMONITOR_P`, `ERL_MONITOR_P_EXIT`; `EI_RPC_FETCH_STDOUT` for `ei_xrpc_to`.
- External term format tags: `ERL_SMALL_INTEGER_EXT` … `ERL_V4_PORT_EXT`, plus `ERL_NEW_CACHE` / `ERL_CACHED_ATOM`.
- `erl_errno` facility: `volatile int* __erl_errno_place(void)` + `#define erl_errno (*__erl_errno_place())` under `_REENTRANT`/`__WIN32__`, otherwise `extern volatile int __erl_errno`.
- Types: `erlang_char_encoding` (ASCII/LATIN1/UTF8), `erlang_pid`, `erlang_port`, `erlang_ref`, `erlang_trace`, `erlang_msg`, `erlang_fun` (closure vs export), `erlang_big`, `ei_term`, `ErlConnect`, `ei_cnode`, `Erl_IpAddr` (a `struct in_addr *`), `ei_x_buff` (dynamic buffer with `buff`/`buffsz`/`index`).
- Versioned socket abstraction: `ei_socket_callbacks` (function pointers for socket/close/listen/accept/connect/writev/write/read/handshake/get_fd, end-of-"version 1" marker), with `EI_SCLBK_INF_TMO` and `EI_SCLBK_FLG_FULL_IMPL`; user-supplied socket implementation (USSI) is selected via `ei_connect_init_ussi` / `ei_connect_xinit_ussi`.
- Size constants: `EI_MAX_COOKIE_SIZE`, `MAXATOMLEN`, `MAXATOMLEN_UTF8`, `EI_MAXHOSTNAMELEN`, `EI_MAXALIVELEN`, `MAXNODELEN`, and `EI_LONGLONG`/`EI_ULONGLONG`.
- Connection/distribution: `ei_connect_init[_ussi]`, `ei_connect_xinit[_ussi]`, `ei_connect[_tmo]`, `ei_connect_host_port[_tmo]`, `ei_xconnect[_tmo]`, `ei_xconnect_host_port[_tmo]`.
- Messaging: `ei_receive[_tmo]`, `ei_receive_msg[_tmo]`, `ei_xreceive_msg[_tmo]`, `ei_send[_tmo]`, `ei_reg_send[_tmo]`, `ei_close_connection`.
- RPC: `ei_rpc`, `ei_xrpc_to`, `ei_rpc_to`, `ei_rpc_from`.
- Node/port lifecycle: `ei_publish[_tmo]`, `ei_listen`, `ei_xlisten`, `ei_accept[_tmo]`, `ei_unpublish[_tmo]`, `ei_thisnodename`, `ei_thishostname`, `ei_thisalivename`, `ei_self`, `ei_make_pid`, `ei_make_ref`.
- Settings/tracing: `ei_set_compat_rel`, `ei_set_tracelevel`, `ei_get_tracelevel`; resolver wrappers `ei_gethostbyname`, `ei_gethostbyaddr`, `ei_gethostbyname_r`, `ei_gethostbyaddr_r`.
- Encode/decode (−`ei_encode_*`/`ei_decode_*` on caller buffers, `ei_x_encode_*` on `ei_x_buff*`): version, long/ulong/longlong/ulonglong/double/boolean/char, atom (+`_as`/`_len`/`_len_as` variants with `erlang_char_encoding`), string(+`_len`), binary, bitstring, pid/port/ref/fun/trace, tuple/list/map headers, `ei_encode_empty_list` macro, `ei_get_type`, `ei_decode_ei_term`, `ei_decode_iodata`, `ei_skip_term`.
- Buffer/format helpers: `ei_x_new[_with_version]`, `ei_x_free`, `ei_x_append[_buf]`, `ei_x_format[_wo_ver]`, `ei_print_term`, `ei_s_print_term`, `free_fun`, `ei_cmp_refs`/`ei_cmp_pids`/`ei_cmp_ports`.
- `ei_init()` — one-time initialization, called automatically on first use.
- GNU MP interop: `ei_decode_bignum` / `ei_encode_bignum` / `ei_x_encode_bignum` declared only when the user included `<gmp.h>` with `__GNU_MP_VERSION == 4 && __GNU_MP_VERSION_MINOR >= 1`.
- Undocumented block inside `#ifdef USE_EI_UNDOCUMENTED` (always active, see Known Issues): `ei_decode_intlist`, `ei_receive_encoded[_tmo]`, `ei_send_encoded[_tmo]`, `ei_send_reg_encoded[_tmo]` plus the back-compat `ei_send_encoded_timeout` / `ei_send_reg_encoded_timeout` macros, and the `erlang_big` API (`ei_encode_big`, `ei_x_encode_big`, `ei_decode_big`, `ei_big_comp`, `ei_big_to_double`, `ei_small_to_big`, `ei_alloc_big`, `ei_free_big`).

## Constraints
- Must remain C **and** C++ compatible: everything is inside `extern "C" { }`, and `../src/prog/ei_fake_prog.c` is compiled with `-xc++` as a check.
- Must remain source- and ABI-compatible for existing external users; signatures, struct layouts and size macros are part of the contract.
- Term encoding must stay byte-compatible with the erts distribution/external term format.
- `ei_connect.h` and `eicode.h` exist only for backward compatibility — do not delete them, and do not reintroduce legacy `erl_*`/old-`ei` contents into them.
- New public functions must be declared here, implemented in `../src/`, documented in `../doc/references/`, and referenced from `../src/prog/ei_fake_prog.c` (rule stated in `../src/README.internal`).
- Headers are installed by the `release` target of `../src/Makefile.in` via its `HEADERS` variable into `$(RELEASEPATH)/lib/erl_interface-<vsn>/include` and `$(RELEASE_PATH)/usr/include`; a new header must be added to `HEADERS` to be installed.

## Dependencies
- Standard C/POSIX headers only: `sys/types.h`, `stdio.h`, `errno.h`, `netdb.h`; `winsock2.h`/`windows.h`/`winbase.h` on Windows.
- Optional system dependency: GNU MP (`gmp.h`) to get the `ei_*_bignum` declarations.
- No dependency on erts internal headers — consumers only need `-I$(ERL_TOP)/lib/erl_interface/include` (plus `-I.../include/$(TARGET)` as done by `lib/odbc/c_src/Makefile.in`).

## Known Issues
- `ei_global.h` is authored in this directory but NOT listed in `HEADERS` in `../src/Makefile.in`, so it is never installed into the release (`usr/include`); the `ei_global_*` functions are also declared in `ei.h`, and in-tree users include `ei.h`/`ei_connect.h` instead, so the file is effectively internal/uninstalled.
- `USE_EI_UNDOCUMENTED` is unconditionally `#define`d at the top of `ei.h`, so the whole "not documented" block (intlist decode, `ei_*_encoded*`, `erlang_big` API) is always visible to consumers despite the "want declarations for undocumented" wording.
- `EI_DEPRECATED_ATTR` exists but no declaration in `ei.h` currently carries it; the build defines `EI_NO_DEPR_WARN` (see `../src/Makefile.in` `WARNFLAGS`) so consumers can suppress such warnings if the attribute is ever applied.
- `erl_errno` is documented as belonging to the old `erl_interface` API but still lives in `ei.h` — flagged by an in-file comment.
- The `ERL_*_EXT` term tags are marked "we don't want to export these" but are public anyway.
- The `ei_encode_longlong`/`ei_x_encode_longlong`/`ei_decode_longlong` family sits under a "Function definitions not documented FIXME" heading.
- `erl_errno` (`EMSGSIZE`/`ETIMEDOUT`/`EHOSTUNREACH`) fall back to `EIO` on platforms where those codes are missing.
- Known limitation carried in `../doc/notes.md`: the encode/decode API cannot handle terms whose external representation exceeds 2 GB.

## Notes for Agents
- Verified: every function declared in these headers has an implementation or macro counterpart in `../src/` (`ei_get_type_internal` occurs only as a comment reference, and `ei_send_encoded_timeout` / `ei_send_reg_encoded_timeout` are macros mapping to the `*_tmo` functions) — no dangling declarations.
- `EI_64BIT` is NOT defined or referenced by `ei.h`; `lib/erl_interface/configure` adds `-DEI_64BIT` when `long` is 8 bytes, and it only affects the encode/decode implementations in `../src/`.
- `EI_ACCEPT_NAME` maps to `accept_ei` on AIX to work around AIX's `accept` namespace pollution; the struct field name differs per platform.
- `ei_get_type()` leaves `index` unchanged and reports size as characters for atoms/strings, bytes for binaries, arity for lists/tuples, 0 otherwise; `ei_decode_ei_term()` only advances `index` when it returns 1.
- `ei_cnode.this_ipaddr` is deliberately not stored (commented out as unused).
- The two stub headers plus `ei_connect.h`/`eicode.h` are still shipped, so treat them as frozen API surface.

## Test Strategy
- Header compile-coverage is exercised by `../src/prog/ei_fake_prog.c` (built by the `check` target in `../src/Makefile.in`, in `_st`/`_mt`/`_md`/`_mdd` and C++ variants) — it is the practical guard that these headers still compile and link.
- Common Test suites in `../test/` compile C helpers against these headers (e.g. `test/ei_global_SUITE_data/ei_global_test.c` includes `ei_connect.h`, `test/all_SUITE_data/my_ussi.c`, `test/ei_connect_SUITE_data/einode.c` include `ei.h`).

## Routing Table
- `ei.h` → the entire public `ei` API (types, macros, connection, encode/decode, undocumented block); the only header that carries real content.
- `ei_connect.h` → compatibility stub forwarding to `ei.h` (installed).
- `eicode.h` → compatibility stub forwarding to `ei.h` (installed).
- `ei_global.h` → global name-service declarations subset (not installed; see Known Issues).

## See Also
- `../src/` — all implementations (connect/, decode/, encode/, epmd/, global/, misc/), `.c` sources, `README.internal` conventions, and `prog/ei_fake_prog.c` header compile check.
- `../doc/references/ei.md`, `ei_connect.md`, `ei_global.md`, and `../doc/guides/ei_users_guide.md` — the API documentation that must stay in sync with these headers.
- `../test/` — Common Test suites with C helpers that compile against these headers.
- External consumers in-tree: `erts/etc/common/Makefile.in` (`EI_INCL`, builds `erl_call`), `erts/example/Makefile` (`EI_INCLUDE`, C-node examples), `lib/odbc/c_src/Makefile.in` (`EI_INCLUDE`), `system/doc/tutorial/cnode*.c` and `system/doc/tutorial/ei.c`.
