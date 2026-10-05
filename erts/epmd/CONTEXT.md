# erts/epmd/ — Erlang Port Mapper Daemon

## Intent
`epmd` is the standalone OS-level port mapper daemon: a small self-contained C program (no Erlang VM, no OTP application, no `.app` file) that maps the `Name` part of an Erlang node name (`Name@Host`) to the TCP port on which that node listens for distribution connections.
Every distributed node registers its `Name` + port with the `epmd` running on its own host (`ALIVE2_REQ`) and queries that daemon to find other nodes on the same host (`PORT2_REQ`).
The same executable doubles as a CLI (`epmd -names|-dump|-kill|-stop`) that talks to the already-running daemon over loopback TCP, and can detach itself (`epmd -daemon`); in practice it is launched automatically by `erl` when a node starts distributed (see `../etc/common/erlexec.c`: `-start_epmd`, `start_epmd_daemon()`, and the `-epmd_port` it forwards).
Sibling areas that speak this protocol (complete external clients, not users of this C code): `../../lib/kernel/src/erl_epmd.erl` (+ `inet_epmd_dist.erl`, `erl_epmd.hrl`), `../../lib/erl_interface/src/epmd/` (C client), `../../lib/jinterface/` (Java client).

## API Surface
What this directory produces and exposes:

- **The `epmd` executable** — built to `$ERL_TOP/bin/$(TARGET)/epmd` (`epmd.exe` on win32) and installed by `src/Makefile.in` `release_spec` into `$(RELEASE_PATH)/erts-$(VSN)/bin`; `../etc/unix/Install.src` symlinks `$ERL_TOP/bin/epmd` to it. Sub-targets `tests`/`opt`/`debug`/`clean`/`release_spec` come from the standard OTP make includes.
- **Server mode** (default) — arguments from `src/epmd.c` parsing:
  - `-port No` / `ERL_EPMD_PORT` — port to listen on (default `EPMD_PORT_NO`, 4369).
  - `-address List` / `ERL_EPMD_ADDRESS` — comma- or space-separated IPs to listen on (IPv4 and IPv6 literals).
  - `-daemon` — detach (double fork + `setsid`, `chdir("/")`, stdio to `/dev/null`, syslog logging).
  - `-relaxed_command_check` / `ERL_EPMD_RELAXED_COMMAND_CHECK` — allow `-kill` with live nodes and allow `-stop`.
  - `-systemd` (only with `HAVE_SYSTEMD_DAEMON`) — take listening sockets from systemd socket activation (`sd_listen_fds`, `sd_notify`).
  - `-d` / `-debug` (repeatable) — raise debug level; also shrinks the remembered unregistered-name list.
  - DbgExtra, test-only: `-packet_timeout Secs` (default `CLOSE_TIMEOUT` 60), `-delay_accept Secs`, `-delay_write Secs`.
- **Client mode (CLI, `src/epmd_cli.c`)** — `-names`, `-dump`, `-kill`, `-stop Name`, plus `-started` (like `-names` but silent, used to probe liveness). Each connects to loopback (`EPMD6` IPv6 loopback first, then IPv4 loopback) at `g->port` and speaks the interactive request codes.
- **Distribution-protocol messages answered** (codes in `src/epmd.h`) — `'x'` `EPMD_ALIVE2_REQ` → `'y'` `EPMD_ALIVE2_RESP` (16-bit creation) or `'v'` `EPMD_ALIVE2_X_RESP` (32-bit creation, dist v6+); `'z'` `EPMD_PORT2_REQ` → `'w'` `EPMD_PORT2_RESP`; `'n'` names, `'d'` dump, `'k'` kill, `'s'` stop. Every request/response is a 2-byte big-endian length prefix followed by the payload.
- **Source files** — `src/epmd.c` (`main`/arg parsing, daemonizing, `usage()`, `dbg_printf`/`dbg_tty_printf`/`dbg_perror`, `epmd_cleanup_exit`, env-var lookups); `src/epmd_srv.c` (server: `run()` select loop, accept/read/`do_request` dispatch, node registry, listen-socket setup); `src/epmd_cli.c` (CLI client side); `src/epmd.h` (wire codes only); `src/epmd_int.h` (internal types + platform shims).
- **User documentation** lives outside this directory: `../doc/references/epmd_cmd.md` (CLI + env vars) and `../doc/guides/erl_dist_protocol.md` (wire protocol). `doc/` here is an empty placeholder containing only `.gitignore`.

## Constraints
- Plain portable C with no Erlang/VM dependency: it must build and run with no BEAM, no OTP release and no `lib/*` application present. Its only link-time dependency from the tree is the static `erts_internal` library from `../lib_src` (for `erl_printf.h` `erts_snprintf`/`erts_vsnprintf`); `src/Makefile.in` builds that on demand via the `$(ERTS_LIB)` rule.
- All OS differences are hidden behind `src/epmd_int.h` shims (`__WIN32__` `close`/`read`/`write`/`sleep`/`ioctl` macros, `NO_DAEMON`, `NO_SYSCONF`, `NO_FCNTL`, `USE_BCOPY`, `EPMD6`) plus `config.h` `HAVE_*` macros. Keep new code inside these shims rather than sprinkling `#ifdef`s.
- Wire format is frozen by external readers: `NAMES_RESP`/`DUMP_RESP` text is parsed by `erl_epmd.erl` and `net_adm:names/0` (the source comments say so explicitly) — never reformat it.
- `src/Makefile` is generated from `src/Makefile.in` (`erts/configure.ac` `AC_CONFIG_FILES` → `epmd/src/$host/Makefile`). Edit the `.in` file and run `./otp_build update_configure` from `ERL_TOP`; the tracked `erts/epmd/Makefile`, `src/Makefile` and `test/Makefile` are thin wrappers around `$(ERL_TOP)/make/*.mk` and must not gain logic.
- Port/version constants are shared with the Erlang side: `epmd.mk` defines `EPMD_PORT_NO`, `EPMD_NODE_TYPE`, `EPMD_DIST_LOW/HIGH` and is also included by `lib/kernel/src/Makefile`. `EPMD_PORT_NO` is passed to the compiler as `-DEPMD_PORT_NO=$(EPMD_PORT_NO)`; `../vsn.mk` mirrors the same value as `ERLANG_DAEMON_PORT`.
- Fixed limits in `epmd_int.h`: `MAX_LISTEN_SOCKETS` 16, `MAXSYMLEN` 255*4 bytes, `INBUF_SIZE`/`OUTBUF_SIZE` = 3*`MAXSYMLEN`, `MAX_UNREG_COUNT` 1000. Buffer sizes are derived from the largest legal request/response — do not grow fields without re-checking them.
- `TYPE=debug` is supported (adds `-DDEBUG @DEBUG_FLAGS@` and a `.debug` object marker); every other `TYPE` is forced to `opt`.
- Keep `epmd.h` (wire constants, historically `epmd_r4.h`) free of internal types; internal structures belong in `epmd_int.h`.

## Design Decisions
- **Separate OS process, not part of the VM.** One daemon per host serves every node on that host (and non-Erlang clients written in C/Java), must outlive individual VM restarts, must be startable without a VM at all, and must be usable by a node that is just coming up before it has a working distribution stack.
- **`epmd_cli.c` is not a second binary.** CLI mode reuses the same executable and simply connects to the running daemon over loopback, so there is exactly one implementation of the protocol and no separate client tool to keep in sync.
- **Registration liveness = the ALIVE2 TCP connection.** `do_request` sets `s->keep` on successful registration; `do_read` treats EOF/reset/extra data on that socket as "node gone" and calls `node_unreg_sock`, which is why epmd keeps the socket open instead of replying-and-closing. There are no heartbeats.
- **`creation` counter for stale-instance detection.** `bump_creation`/`get_creation`: the counter is seeded from the current time, never below 4, and returned as a raw 32-bit value for `highvsn >= 6` (`'v'` response) or folded to 1..3 (`cr_counter % 3 + 1`) for older nodes. Unregistered names are remembered in a second list (capped at `MAX_UNREG_COUNT`) precisely so the counter keeps increasing across re-registration.
- **Separate connection and node data structures** (`Connection` vs `Node`) so a request cannot occupy the name slot it is about to reuse — see the comment above `do_request`.
- **IPv4 + IPv6 by default.** `EPMD6` (guarded by `HAVE_IN6 && AF_INET6 && HAVE_INET_PTON`) adds an IPv6 listener with `IPV6_V6ONLY`, and the code prefers the IPv6 loopback when connecting. Loopback addresses are always added implicitly to `-address`; those implicit sockets are "nonfatal" (a missing protocol stack is logged and skipped), while explicitly requested addresses are fatal.
- **Single-threaded `select()` loop.** No threads, no fork-per-connection; `max_conn` comes from `sysconf(_SC_OPEN_MAX)` clamped to `FD_SETSIZE` (`FD_SETSIZE` is forced to 1024 on win32 in `epmd_int.h`). An idle close-out timer (`IDLE_TIMEOUT` 5 s wakeups, `CLOSE_TIMEOUT`/`-packet_timeout` 60 s) reaps hung connections; `s->mod_time` is only refreshed when a complete packet has been read, so slow/partial senders are timed out too.
- **Security posture:** `ALIVE2_REQ`, `DUMP_REQ`, `KILL_REQ` and `STOP_REQ` all require `local_peer` (the peer address must be loopback, checked in `conn_local_peer_check`); `PORT2_REQ`/`NAMES_REQ` are answered to anyone. `KILL_REQ` is refused while nodes are registered and `STOP_REQ` is refused outright, unless relaxed mode was enabled at daemon startup.
- **`DONT_USE_MAIN`** replaces `main()` with `epmd(argc,argv)`/`epmd_dbg(level,port)` so the daemon can be embedded (legacy VxWorks path); `EpmdVars` is passed explicitly everywhere to keep the code reentrant.

## Known Issues / Gotchas
- A registered name lives only as long as its ALIVE2 socket. If the connection dies silently (firewall/NAT drop), the name stays registered until the socket errors or `packet_timeout` (60 s default) expires.
- The client CLI only ever connects to loopback, and it tries IPv6 loopback before IPv4 — a daemon bound only to a specific non-loopback IPv4 address still works, but a port mismatch between daemon and client is a silent "Cannot connect to local epmd".
- `bind()` failing with `EADDRINUSE` is treated as success: a second `epmd` on the same port prints "there is already a epmd running" and exits 0. A stale `epmd` from another OTP installation therefore silently keeps serving requests.
- `-d`/`-debug` reduces the remembered unregistered-name list to `DEBUG_MAX_UNREG_COUNT` (5), which changes `creation` behaviour between debug and normal runs.
- Names are compared byte-wise with a hand-rolled `is_same_str` (no case folding); names containing an embedded NUL are rejected, and both `ALIVE2_REQ` and `PORT2_REQ` names must be valid UTF-8 of at most 255 characters (up to 255*4 bytes).
- `NAMED`/`DUMP_RESP` includes `fd` numbers and the "old/unused name" list — diagnostic output, not part of the intended protocol surface, but still consumed as text by Erlang code.
- Windows build: `max_conn` is bounded by `FD_SETSIZE` (1024) but `../doc/references/epmd_cmd.md` documents a lower practical limit of 60 nodes per epmd instance on Windows.
- `erts/epmd/doc/` is intentionally empty (only a `.gitignore`); `erts/doc/man1/` is likewise a build-output directory, so the rendered `epmd(1)`/`epmd_cmd` documentation is generated elsewhere.

## Notes for Agents
- `epmd` is one of the few components that can be built and run without the whole release: `cd erts/epmd/src && make` (after `erts/` is configured, since `epmd_int.h` needs `config.h`) produces `$ERL_TOP/bin/$(TARGET)/epmd`, and `epmd -port 2243 -d` gives a usable test instance. It still requires the configured `erts` tree for `config.h` and the `erts_internal` library.
- Two different `Makefile`s are in play: `src/Makefile.in` (real build rules; compiled objects land in `erts/obj$(TYPEMARKER)/$(TARGET)/`) and `Makefile`/`test/Makefile` (OTP make includes only).
- `HAVE_SYSTEMD_DAEMON` comes from `erts/configure.ac` `--enable-systemd`; without it `-systemd` is not a valid option and attempting to use it yields the generic usage error.
- When debugging protocol issues, `-d`/`-d -d` makes epmd log every request and `dbg_print_buf` hex-dumps payloads; the leading `epmd: <timestamp>: ` prefix means the message came from `dbg_gen_printf`.
- Before changing wire behaviour, read `../doc/guides/erl_dist_protocol.md`; before changing CLI/env-var behaviour, read `../doc/references/epmd_cmd.md` — those two files are the user-visible contract.

## Dependencies
- Configure-provided `config.h` with `HAVE_SYSLOG_H`, `HAVE_SYSTEMD_DAEMON`, `HAVE_IN6`/`HAVE_INET_PTON`/`HAVE_DECL_IN6ADDR_*`, `HAVE_SYS_TIME_H`, `HAVE_UNISTD_H`, and the `NO_DAEMON`/`NO_SYSCONF`/`NO_FCNTL`/`USE_BCOPY` knobs.
- Sockets/`select()` (Winsock on Windows), `syslog` on unix; `libsystemd` (`sd-daemon.h`, `@SYSTEMD_DAEMON_LIBS@`) only with `--enable-systemd`.
- The `erts_internal` static library from `../lib_src` (built automatically by `src/Makefile.in`).

## Test Strategy
- Suite: `test/epmd_SUITE.erl` (~33 cases plus the `buffer_overrun` group). `test/epmd.spec` is `{suites,"../epmd_test",all}` — i.e. it runs every suite in the released `epmd_test` directory; adding a suite there needs no spec change but does need `release_tests_spec` to install it (`test/Makefile` installs `epmd.spec`, the `.erl` files and the generated `Emakefile` into `$(RELEASE_PATH)/epmd_test`; compile flags come from `epmd.mk`).
- Run it with `make epmd_test` from `ERL_TOP`, or `make test` inside `erts/epmd` (both end in `make/test_target_script.sh`, which builds a test release via `release_tests_spec` and then runs `ct_run -spec .../epmd_test/epmd.spec`). Root `Makefile.in` also lists `erts/epmd/test` in `TEST_DIRS`, and `scripts/otp_build_check` maps the pseudo-application name `epmd` to `$ERL_TOP/erts/epmd/test`.
- The suite does not start a node as epmd: it uses `os:find_executable(epmd)` from `PATH` and spawns `epmd -packet_timeout 1 -port 2243`, so a release with `$ERL_TOP/bin` on `PATH` is a prerequisite (which is why the tests are driven through the release test script).
- `init_per_testcase`/`end_per_testcase` call `cleanup()`, which connects to port 2243 and sends `EPMD_KILL_REQ` expecting `OK` — tests assume a fresh, single-daemon-per-run environment on that port.
- Cases `no_nonlocal_register`/`no_nonlocal_kill` simulate a remote peer with an `ssh -L` tunnel (`ssh_proxy/2`, needs an SSH-accessible unix host with `read`), so they cannot run in a plain sandbox. `register_name_ipv6` skips when the host has no IPv6 loopback.

## Routing Table
- `src/` → the daemon itself: `epmd.c` (main/arg parsing/daemonize/logging), `epmd_srv.c` (server loop + protocol + node registry), `epmd_cli.c` (CLI client over loopback), `epmd.h` (wire codes), `epmd_int.h` (internal types, platform shims, limits), `Makefile.in` (build rules; `Makefile` is generated via configure)
- `test/` → `epmd_SUITE.erl` (protocol/registration/CLI test suite), `epmd.spec`, `Makefile` (release_tests_spec packaging)
- `doc/` → empty placeholder (`.gitignore` only); the real docs are `../doc/references/epmd_cmd.md` and `../doc/guides/erl_dist_protocol.md`
- `epmd.mk` → shared constants (`EPMD_PORT_NO`, `EPMD_NODE_TYPE`, `EPMD_DIST_LOW/HIGH`); also included by `../../lib/kernel/src/Makefile` (sibling — read-only from here, escalate changes to `../../lib/kernel/`)

## See Also
- `../../lib/kernel/src/erl_epmd.erl` → the Erlang client of this daemon, and the parser of its `NAMES`/`DUMP` text output (sibling — read-only)
- `../../lib/erl_interface/src/epmd/` and `../../lib/jinterface/` → the C and Java clients of the same protocol (siblings — read-only)
- `../etc/common/erlexec.c` → `-start_epmd`/`-epmd`/`-epmd_port` handling that spawns `epmd -daemon`
- `../doc/references/epmd_cmd.md`, `../doc/guides/erl_dist_protocol.md`, `../doc/references/erl_cmd.md` → user-facing CLI, wire-protocol and `erl` interaction documentation (source of truth for `ERL_EPMD_ADDRESS`/`ERL_EPMD_PORT`/`ERL_EPMD_RELAXED_COMMAND_CHECK`)
- `../CONTEXT.md`, `../epmd/CONTEXT.md` (this node's parent), `../test/CONTEXT.md` → erts-level build/test conventions (this directory's Makefiles are thin wrappers over `../../make/*.mk`)
- `../../HOWTO/TESTING.md` → general CT/release-test workflow used by `make epmd_test`
