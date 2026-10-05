# epmd Client (erl_interface `ei`)

## Intent
Client side of the epmd (Erlang Port Mapper Daemon) protocol for the `ei` C API: publish a C-node's listen port/alive name, and look up the port of a remote node.
Part of `libei`; not an Erlang application and not a standalone program.
All traffic is TCP to the external `epmd` daemon (default port 4369) speaking the "epmd r4" protocol (ALIVE2/PORT2/STOP).

## API Surface
- `ei_epmd.h` — internal header, NOT in the installed public header set (`HEADERS` = `../include/ei.h`, `ei_connect.h`, `eicode.h`); it is only copied to `$(RELSYSDIR)/src/epmd` by the `release` target of `../Makefile.in`.
  - Wire constants: `EI_DIST_5` 5, `EI_DIST_6` 6, `EI_DIST_HIGH`/`EI_DIST_LOW` both aliased to `EI_DIST_6` (OTP 23+ handshake), `EPMD_PORT` 4369, `EPMDBUF` 512, `EI_MYPROTO` 0 (tcp/ip), `INADDR_LOOPBACK` fallback.
  - Opcodes: `EI_EPMD_ALIVE2_REQ` 120, `EI_EPMD_ALIVE2_RESP` 121, `EI_EPMD_ALIVE2_X_RESP` 118, `EI_EPMD_PORT2_REQ` 122, `EI_EPMD_PORT2_RESP` 119, `EI_EPMD_STOP_REQ` `'s'`.
  - Internal prototypes: `ei_epmd_connect_tmo`, `ei_epmd_publish`, `ei_epmd_publish_tmo`, `ei_epmd_port`, `ei_epmd_port_tmo` (declares `struct in_addr*`, so socket headers must precede it).
- `epmd_port.c` — `ei_epmd_connect_tmo()` (connect a new socket to epmd on a given IPv4 address, or `INADDR_LOOPBACK` when `addr == NULL`) and the lookup path: static `ei_epmd_r4_port()`, wrapped by `ei_epmd_port()` / `ei_epmd_port_tmo()`.
- `epmd_publish.c` — static `ei_epmd_r4_publish()` plus the public `ei_publish()` / `ei_publish_tmo()` (return the epmd connection fd).
- `epmd_unpublish.c` — public `ei_unpublish_tmo()` and `ei_unpublish()` (send STOP for the node).
- Public `ei_publish*`/`ei_unpublish*` are declared in `../include/ei.h`, not in `../include/ei_connect.h`; there is no `ei_unpublish_node`.

## Protocol Implemented (all big-endian / network order, 2-byte length prefix on requests)
- Connect: `ei_epmd_connect_tmo()` opens `AF_INET`/`sockaddr_in` to `ERL_EPMD_PORT` (env, read and cached in a function-`static` on first use) or 4369; `ms == 0` is mapped to `EI_SCLBK_INF_TMO` (infinite) and all I/O goes through `ei_write_fill_t__`/`ei_read_fill_t__`.
- Lookup (PORT2_REQ): `len = 1 + strlen(alive)`, opcode 122, then the alive name (not NUL-terminated on the wire); reply is read as 2 bytes (opcode 119 + result byte, non-zero = failure) then exactly 8 bytes: port(16), nodetype(8), protocol(8), dist_high(16), dist_low(16).
  - The name/extra fields the epmd reply also carries are ignored, and the connection is closed.
  - Lookup fails with `erl_errno = EIO` unless `EI_MYPROTO == proto` and `EI_DIST_HIGH >= dist_low && EI_DIST_LOW <= dist_high`; on success `*dist = min(dist_high, EI_DIST_HIGH)`.
  - `ei_epmd_r4_port()` returns `-2` when reading the response header fails, `-1` otherwise; callers (`ei_xconnect_tmo`) treat any negative as `ERL_NO_PORT`.
- Publish (ALIVE2_REQ): `len = 13 + strlen(alive)`, opcode 120, listen port(16), nodetype hard-coded `'h'` (hidden), protocol `EI_MYPROTO`, dist_high, dist_low, namelen(16), alive name, extralen = 0 (no extra string).
  - Reply: `ALIVE2_RESP` 121 with 4 bytes (opcode, result byte, creation(16)) or, because `EI_DIST_HIGH` is 6, `ALIVE2_X_RESP` 118 with 6 bytes (opcode, result, creation(32)) since epmd 23+ answers X_RESP for `highvsn >= 6`.
  - On success the socket fd is returned and deliberately left OPEN: epmd keeps the node registered for the lifetime of that connection, so closing that fd is how a node unregisters.
  - The creation number is parsed for tracing only and discarded (the creation passed to `ei_connect_init` is what is used).
- Unpublish (STOP_REQ): `len = 1 + strlen(alive)`, opcode `'s'`, alive name; epmd replies with 7 raw bytes (no length prefix), which are compared to `"STOPPED"` (returns 0) or `"NOEXIST"` (`erl_errno = EIO`, returns -1).
- Error codes set here: `ERANGE` for over-long alive names (`EPMDBUF` 512 limits), `EIO` for short writes/reads, unknown response opcodes, protocol/dist mismatch, NOEXIST, and `"STOPPED"`-unknown replies.

## Callers
- `../connect/ei_connect.c` — `ei_xconnect_tmo()` is the only in-tree caller of `ei_epmd_port_tmo()`, passing the negotiated dist version on as `epmd_says_version` to `ei_connect_helper()`.
- `../prog/ei_fake_prog.c` references `ei_publish`/`ei_unpublish` (link/compile coverage via the fake-prog test executables).

## Constraints
- Include `"eidef.h"` first (it pulls the generated `config.h` and `ei.h`) as `epmd_publish.c`/`epmd_port.c` do; `epmd_unpublish.c` includes the socket headers first — keep the ei includes plus `ei_epmd.h`, `ei_portio.h`, `putget.h` ordering.
- Use the `uint8/uint16/uint32` types and the `putget.h` macros (`put8`, `put16be`, `put32be`, `get8`, `get16be`, `get32be`); never byte-swap manually.
- The byte layouts and opcodes are a wire contract with the ERTS epmd server (`../../../erts/epmd/`) and with every other node implementation — change them only together with the server.
- Never assume a length prefix on replies: PORT2_RESP and STOP replies are read with fixed byte counts.
- Keep using the `ei_portio` timeout wrappers so the `*_tmo` variants honour their timeouts.
- No locks are used (reentrant); do not add global mutable state beyond the existing cached `epmd_port`.

## Known Issues
- IPv4 only (`struct in_addr`, `AF_INET`); the code carries a FIXME noting it excludes IPv6.
- The publish path cannot send a non-hidden node type and always sends an empty extra string, so epmd cannot learn extra node info (e.g. TLS/ALPN data) from a C-node.
- `ei_epmd_port()` rejects any remote node whose dist version range does not include 6, i.e. it cannot look up nodes older than OTP 23.
- `ei_unpublish()`/`ei_unpublish_tmo()` are documented as deprecated and normally fail: epmd only handles STOP_REQ from a local peer when started with `-relaxed_command_check`, and without it epmd sends no reply, so `ei_unpublish()` (ms 0 → infinite timeout) blocks until the connection closes.
- `ei_unpublish_tmo()` returns the raw (negative) value from `ei_epmd_connect_tmo()` on connect failure instead of a clean -1 (see the FIXME in the file).

## Test Strategy
- No dedicated suite for this directory; it is exercised indirectly.
- `../../test/ei_accept_SUITE.erl` drives `ei_publish` through the fake-prog command protocol (`send_command(P, ei_publish, [Port])`) and verifies registration through `erl_epmd:names()`.
- `../../test/ei_connect_SUITE.erl` (and `ei_xconnect*`) exercise `ei_epmd_port` via `ei_connect`/`ei_xconnect`.
- `../../test/ei_tmo_SUITE.erl` contains an Erlang-side fake epmd responder (`epmd_register/3`, `get_epmd_port/0`) with `ei_tmo_SUITE_data/ei_tmo_test.c` to test behaviour against epmd that does not answer.
- `ei_unpublish`/`ei_unpublish_tmo` are not called by any suite — only link-referenced by `../prog/ei_fake_prog.c`, so the STOP path has no runtime coverage.

## Dependencies
- No dependencies beyond system sockets and the generated `config.h` produced by `../configure`.

## See Also
- `../connect/ei_connect.c` — the only caller of `ei_epmd_port_tmo` (and the dist handshake that consumes the negotiated version).
- `../misc/ei_portio.h` — `ei_socket__`, `ei_connect_t__`, `ei_write_fill_t__`, `ei_read_fill_t__`, `EI_SCLBK_INF_TMO`.
- `../misc/putget.h` — the put/get wire macros; `../misc/eidef.h` — `config.h` + `ei.h` include chain.
- `../../../erts/epmd/src/epmd_srv.c` — server side of this protocol (request validation, X_RESP/STOP semantics); keep in sync.
- `../include/ei.h` — public declarations of `ei_publish`, `ei_publish_tmo`, `ei_unpublish`, `ei_unpublish_tmo`.
- `../Makefile.in` — `EPMDSRC` and the `vpath %.c connect:encode:decode:misc:epmd:...` build wiring into `libei*.a`/`libei*.so`.
