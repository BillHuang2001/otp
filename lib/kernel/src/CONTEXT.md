# kernel/src — Kernel Application Sources

## Intent
Sources of the `kernel` application: the lowest OTP layer above `erts` — application control, the code server, file/disk-log I/O, sockets & name resolution, distribution, `logger`, OS/terminal services.
It declares `{applications,[]}`: it starts before any other application, so it may only use erts plus the stdlib modules that are already available (a few modules such as `kernel`, `application` and `error_handler` must also work before most of `kernel` itself is up).
Anything that must be callable during early boot (before the code server, file server or logger exist) belongs here rather than in `stdlib`/`sasl`.
The directory is FLAT — there are no subdirectories — so this file is both the documentation and the module map.

## API Surface
121 tracked files: 104 `.erl` modules, 13 internal `.hrl`, `Makefile`, `kernel.app.src`, `kernel.appup.src`, `inet_dns_record_adts.pl`.
Module purpose is stated by a leading `-moduledoc` (public, emitted into the docs) or `-moduledoc false` (internal). Both sets are documented below because internal modules are still callable and used across apps.

### Public (documented) modules — 37
Application control / misc:
- `application` — application resource files and start/stop/ensure_all_started API.
- `error_handler` — default system error handler (undefined-function handling for the shell).
- `error_logger` — legacy event-based error logger, now a compatibility layer over `logger` (internal `gen_event` behaviour).
- `heart` — heartbeat monitoring / node restart interface.
- `os` — operating-system-specific functions (env, cmd, signals, monotonic time).
- `code` / `erl_ddll` / `erl_debugger` — code loading & dynamic driver loading; `erl_debugger` is EXPERIMENTAL (since OTP 28) low-level debugger support.
- `erl_boot_server` — boot server serving code to diskless nodes.
- `data_publisher` — behaviour for eventually-consistent replicated data stores with peer discovery and version translation (since OTP 29; `pg` is built on it).
Distribution:
- `net_kernel` — the net kernel process: distribution setup and connection supervision.
- `net` — the socket-like network interface API (connect/listen/monitors/addresses).
- `net_adm` — net administration (`ping/1`, `names/0`, `world/0`, hosts file handling).
- `auth` — cookie-based authentication server (module and most functions DEPRECATED).
- `erl_epmd` — client of the external `epmd` daemon.
- `erpc` — enhanced RPC with timeouts/monitoring/parallel calls.
- `rpc` — legacy RPC over the `rex` server (still the `rex` implementation).
- `global` / `global_group` — global name server and global node groups.
- `pg` — distributed named process groups (rewritten on `data_publisher`).
Sockets / DNS:
- `socket` — the new sockets API (wrapper over the `prim_socket` NIF; ~8k lines).
- `gen_tcp` / `gen_udp` / `gen_sctp` — classic inet_drv-based socket APIs.
- `inet` — address parsing and socket-option handling shared by all socket APIs.
- `inet_res` — DNS client.
Files / logging:
- `file` — the file API (dispatches to `prim_file` NIF or to the file server).
- `disk_log` — disk-based term logger (wrap/halt logs) and `wrap_log_reader` to read them.
- `logger`, `logger_handler`, `logger_std_h`, `logger_disk_log_h`, `logger_formatter`, `logger_filters` — the logging API, handler behaviour, the two standard handlers, the default formatter and the standard filter set.
Tracing:
- `seq_trace` — sequential tracing.
- `trace` — the Erlang trace interface (since OTP 27).

### Internal modules — 67 (grouped by functional area)
Application control & kernel supervision:
- `kernel` — `{mod,{kernel,[]}}` callback: builds the `kernel_sup` tree (see the ASCII tree in `kernel.erl`).
- `application_controller` — the `application_controller` server: application/process database.
- `application_master` — per-application process started by the controller, starts the top supervisor.
- `application_starter` — starts applications phase-by-phase per `{start_phases,...}`.
- `kernel_config` — gen_server for kernel config needing start-up synchronisation (wait for other nodes, `dist_auto_connect`).
- `kernel_refc` — reference counter for erts/kernel resources (e.g. `scheduler_wall_time`).
- `erl_reply` — replies to `erl_call`/`erl_interface` style remote requests.
- `erl_signal_handler` — default `gen_event` handler on `erl_signal_server` (sigusr1/sigquit/sigterm).
Code:
- `code_server` — the actual code server process behind `code`.
- `erl_compile_server` — serialises compilation through `erl_compile` (stdlib); started only when `-kernel start_compile_server true` is given (not set by default).
Sockets / distribution:
- `dist_util` — shared distribution handshake implementation.
- `inet_tcp_dist` — default `-proto_dist inet_tcp` distribution module; `inet6_tcp_dist` is the IPv6 variant.
- `inet_epmd_dist` / `inet_epmd_socket` — distribution over the socket-API/EPMD transport (`-proto_dist inet_epmd`; also used by `ssl` dist and `-epmd_module`).
- `erl_distribution` — the `net_sup` supervisor that starts `net_kernel`.
- `inet_tcp`, `inet6_tcp`, `inet_udp`, `inet6_udp`, `inet_sctp`, `inet6_sctp` — inet_drv-based protocol/option modules used by `gen_*` and the dist modules.
- `local_tcp`, `local_udp` — the `local` (Unix-domain) address-family variants of the same API.
- `gen_tcp_socket`, `gen_udp_socket` — re-implementations of the `gen_tcp`/`gen_udp` API on top of `socket`, selected with the `{inet_backend, socket}` option.
- `inet_db` — the `inet_db` process (inet configuration database), `inet_config` fills it from hosts/resolv.conf/services.
- `inet_dns` — DNS record encode/decode (uses the generated `inet_dns_record_adts.hrl`), `inet_dns_tsig` (TSIG signing/verification, RFC 8945).
- `inet_parse` — low-level parsing of addresses and tcp options used by `inet`/`inet_res`.
- `inet_hosts` — hosts-file (and `%ERL_INETRC%`) lookups, `inet_gethost_native` — bridge to the `inet_gethost` port program.
- `global_search` — helper implementing `global` name searches.
- `dist_ac` — distributed-application controller (takeover/failover), driven by `application`, `application_controller` and `kernel_config`.
File I/O:
- `file_server` — the `file_server_2` process, `file_io_server` — the I/O protocol server between it and `raw_file_io`.
- `raw_file_io` — raw-mode file operations (bypasses the file server), with layered filters: `raw_file_io_list`, `raw_file_io_delayed`, `raw_file_io_compressed`, `raw_file_io_deflate`, `raw_file_io_inflate`.
- `ram_file` — file-in-RAM device.
- `disk_log_1` — disk_log implementation, `disk_log_server` / `disk_log_sup` — its registry and supervisor.
Logger internals:
- `logger_server` / `logger_sup` — the logger process and its supervisor.
- `logger_backend` / `logger_handler` — handler behaviour dispatching and the handler process API.
- `logger_config` — ETS-based logger configuration store, `logger_proxy` — per-process filtering/forwarding front end.
- `logger_handler_watcher` — watches handler processes (restart/removal).
- `logger_h_common` / `logger_olp` — shared handler implementation and its overload-protection wrapper.
- `logger_simple_h` — minimal handler used before the full logger is available.
OS / terminal / processes:
- `user_drv` — terminal driver, job control (^G) and `-remsh` handling; spawns `group` processes.
- `group` — the group-leader process for user I/O (xterm and dumb modes).
- `group_history` — shell command history persisted via `disk_log`.
- `prim_tty` / `prim_tty_sighandler` — Erlang side of the `prim_tty` NIF plus its sigwinch/sigcont handler.
- `standard_error` — the `standard_error` process (stderr group leader).
- `user_sup` — supervisor bridge that starts `user_drv` with the right mode flags.
- `erl_erts_errors` / `erl_kernel_errors` — `format_error/2` implementations attached via `error_info` maps: the former is referenced by erts preloaded modules (`erlang`, `atomics`, `counters`, ...), the latter by kernel modules (`os`, `trace`, ...).
- `erts_debug` — wrappers around experimental erts debug BIFs (disassembly, internal state, lock counters); used by tests/tools.
- `pg2` — REMOVED-module stub only (`-removed`, "removed in OTP 24. Use pg instead"); no longer functional.

## Build & Module Registration
- `Makefile MODULES` and the `modules` list of `kernel.app.src` are currently identical (both 104 entries) — a new module must be added to both.
- `HRL_FILES` = headers installed into `$(RELSYSDIR)/include` (they live in `../include/`: `file.hrl`, `inet.hrl`, `inet_sctp.hrl`, `dist.hrl`, `dist_util.hrl`, `net_address.hrl`, `logger.hrl`, `eep48.hrl`).
- `INTERNAL_HRL_FILES` = headers kept in `src/` and installed to `$(RELSYSDIR)/src` (`application_master.hrl`, `disk_log.hrl`, `erl_epmd.hrl`, `file_int.hrl`, `inet_dns.hrl`, `inet_res.hrl`, `inet_boot.hrl`, `inet_config.hrl`, `inet_int.hrl`, `inet_dns_record_adts.hrl`, `logger_internal.hrl`, `logger_olp.hrl`, `logger_h_common.hrl`).
- `inet_dns_record_adts.hrl` is GENERATED by `perl inet_dns_record_adts.pl` into `$(ESRC)` (the src dir) and is not in git — regenerate by building; edit the `.pl` instead.
- `erl_epmd.beam` has its own rule: it is compiled with `EPMD_FLAGS` (`-Depmd_port_no`, `-Depmd_node_type`, `-Depmd_dist_low`, `-Depmd_dist_high`, `-Derlang_daemon_port`) read from `erts/vsn.mk` + `erts/epmd/epmd.mk`. Those `-D` values ARE the wire-protocol constants of the running node, so never "simplify" this rule.
- Compile flags: `-Werror +warn_deprecated_catch +warn_obsolete_bool_op -I../include`. Warnings are errors here.
- `kernel.app` / `kernel.appup` are generated with `sed -e 's;%VSN%;$(KERNEL_VSN);'` from the `.src` files — always edit the `.src`.
- `runtime_dependencies` in `kernel.app.src` uses `"stdlib-@OTP-20412@"`-style placeholders; `make/fixup_development_runtime_dependencies` substitutes the real version for development builds, and the release tooling resolves them at release time. Leave the markers alone.
- `BOOTSTRAP` build overrides `EGEN`/`EBIN` to point at `bootstrap/`.

## Constraints
- Every `.erl` in this directory must be listed in BOTH `Makefile MODULES` and `kernel.app.src` `modules` (verified identical at this revision).
- Keep `-moduledoc false` on internal modules: the doc build (`../doc/docs.exs` groups + ExDoc) publishes anything with a moduledoc, so a missing flag silently adds an internal module to the public docs.
- Module-level `-doc`/`-moduledoc` text is the documentation source (there is no `doc/src/*.xml`); `../doc/src/` only holds `logger_arch.dia`.
- New/changed internal `.hrl` stay in this directory; only shared headers referenced by other applications go to `../include/` and must be added to `HRL_FILES`.
- Do not add a dependency on an application above kernel: dependencies limited to `erts`, `stdlib`, and the declared `crypto`/`sasl` runtime dependencies (`crypto` is used only in `inet_db` and `inet_dns_tsig`, and must never be assumed available during early boot).
- Anything reachable during early boot (`kernel`, `application`, `error_handler`, `logger_simple_h`, `group`, `user_drv`, `code`) must keep minimal dependencies, since it may run before other kernel processes exist.

## Design Decisions
- The kernel supervisor tree is split into `kernel_sup` (one_for_all — a child dying halts the node) and `kernel_safe_sup` (one_for_one — restartable services). The full tree is drawn in the comment at the top of `kernel.erl`; put any new child in the correct supervisor.
- `-mode minimal` starts a much smaller tree (used by escript-like minimal nodes) — see the two branches of `kernel:start/2`.
- Two socket backends coexist: `inet_drv`/`prim_inet` (`gen_tcp`, `gen_udp`, `gen_sctp`, plus `inet_*`) and the new NIF-based `socket`/`prim_socket` stack (`gen_tcp_socket`, `gen_udp_socket`). `inet.erl` owns the shared option/address layer and must stay agnostic to the backend.
- `pg` was rewritten on top of the new `data_publisher` abstraction instead of `pg2`'s ad-hoc protocol; `pg2` is only a tombstone module.
- Pretty-printing of erts- and kernel-generated errors is delegated to `erl_erts_errors`/`erl_kernel_errors` (attached to raised errors through `error_info` maps) rather than being implemented inline — these live in kernel because the formatting code is plain Erlang that must not sit in erts.
- `error_handler` carries a strong warning in its moduledoc: it interacts with the code server's `init` process and the I/O mechanism, so small edits can deadlock the system.
- `prim_tty` (this module) is a thin wrapper over the C NIF in `erts/emulator/nifs/common/prim_tty_nif.c`; the process/terminal protocol lives in `user_drv` and `group`.

## File Server Wire Protocol (`file.erl` ↔ `file_server` ↔ `file_io_server`)
- `file.erl` addresses the process registered as `file_server_2` (`?FILE_SERVER`, `file.erl:251`) through `call/2` (`file.erl:3039`): `gen_server:call(file_server_2, list_to_tuple([Command|Args]), infinity)`. `check_and_call/2` (`file.erl:3046`) runs `check_args/1` first.
- Only `open` plus the stat/mutate ops (`read_file`, `write_file`, `set_cwd`, `delete`, `rename`, `make_dir`, `del_dir`, `list_dir(_all)`, `get_cwd`, `read_file_info`, `altname`, `write_file_info`, `read_link_info`, `read_link(_all)`, `make_link`, `make_symlink`, `copy`, `stop`) are sent to `file_server_2`. `open` sends `{open, FileName, ModeList}`; the server replies with the result of `file_io_server:start_link/3` = `{ok, Pid} | {error, Reason}` (`file_server.erl:101-103`) — NOT a `#file_descriptor{}`.
- The returned `Pid` is a `file_io_server`: a hand-written receive loop `server_loop/1`, NOT a `gen_server`. Reads/writes use the io protocol: `read/2` → `io:request(Pid, {get_chars, latin1, '', Sz})` (`file.erl:1634`), `write/2` → `{put_chars, latin1, Bin}` (`file.erl:1800`); messages are `{io_request, From, ReplyAs, Request}` ↔ `{io_reply, ReplyAs, Reply}` (`file_io_server.erl:215-239`, `io.erl:1669-1672`).
- `close/sync/position/advise/allocate/pread/pwrite/truncate/datasync` use the file protocol: `file_request(Pid, Req)` (`file.erl:3068`) sends `{file_request, From, Ref, Req}` ↔ `{file_reply, Ref, Reply}` (`file_io_server.erl:201-236`).
- `#file_descriptor{module=M, data=D}` is the 3-tuple `{file_descriptor, M, D}` (`../include/file.hrl:64-66`); it is returned only by the `raw`/`ram` handles (`raw_file_io:open` → `prim_file:open`, `prim_file.erl:137,151-152`), whereas the cooked file-server path returns a `Pid`.
- `path_open/3`/`path_open_first/4` (`file.erl:2601,2960`) call `open/2`; on `{error, enoent|enotdir}` they try the next path, on `{ok, Fd}` they return `{ok, Fd, FileName}`.

## Notes for Agents
- `prim_file`, `erl_prim_loader`, `prim_inet`, `prim_socket`, `prim_net`, `socket_registry`, `prim_buffer`, `prim_zip`, `zlib`, `init`, `erlang`, `erts_internal` are NOT here — they are preloaded modules in `erts/preloaded/src/`. `kernel` modules may assume they are available.
- The preloaded `init` module boots the system and starts applications through `application`; `application_controller` is already registered before `kernel:start/2` runs.
- The default distribution module is `inet_tcp_dist` (`net_kernel:proto_dist_argument/0` defaults to `"inet_tcp"`); `inet_epmd_dist` is opt-in.
- `erts_debug` and `erl_compile_server` are internal-looking but intentionally in the app's module list; `erts_debug` is part of the supported surface for erts tests/tools.
- Big files worth knowing before editing: `socket.erl` (8.1k lines), `inet.erl` (5k), `gen_tcp_socket.erl` (3.4k), `global.erl` (3.4k), `net_kernel.erl` (3.1k), `file.erl` (3.1k).

## Known Issues
- `erl_compile_server` is only started when `-kernel start_compile_server true` is set, and nothing in the tree sets it — the specs in `kernel:start_compile_server/0` are dormant by default.
- `inet_gethost_native` (port-program based name resolution) is legacy and only used for the `native` lookup order; the default is the inet_db/DNS path plus erts' own gethost.

## Dependencies
- Build: GNU make, `perl` (only for `inet_dns_record_adts.hrl`), the erlang compiler from an existing bootstrap.
- Runtime: `erts` (NIFs/drivers: `prim_file`, `prim_inet`, `prim_socket`, `prim_net`, `prim_tty`, `epmd`), external `epmd` daemon for distribution, system resolver configuration files under `/etc`.

## Test Strategy
- Suites live in the sibling `../test` (161 entries): `kernel_SUITE`, `application_SUITE`, `code_SUITE`, `file_SUITE`, `disk_log_SUITE`, `logger*_SUITE`, `inet_SUITE`/`inet_sockopt_SUITE`, `gen_tcp_*_SUITE`, `gen_udp_SUITE`, `gen_sctp_SUITE`, `socket*_SUITE` (with the shared `socket_test_lib`/`socket_suites` helpers), `global*_SUITE`, `erl_distribution*_SUITE`, `erpc_SUITE`, `rpc_SUITE`, `heart_SUITE`, `wrap_log_reader_SUITE`, `interactive_shell_SUITE`.
- `lib/kernel/Makefile` sets `TEST_NEEDS_RELEASE=true` and `DIA_PLT_APPS=crypto compiler` — tests must run against an installed release, and Dialyzer builds a PLT including crypto and compiler.
- Many tests are skipped/inert without `-proto_dist` support or a second node; distributed suites spawn peers via `?CT_PEER`.

## Routing Table
- Application control, boot, kernel supervision → `application*.erl`, `kernel.erl`, `kernel_config.erl`, `kernel_refc.erl`, `erl_reply.erl`, `erl_signal_handler.erl`, `error_handler.erl`, `error_logger.erl`, `heart.erl`
- Code loading / module management → `code.erl`, `code_server.erl`, `erl_boot_server.erl`, `erl_compile_server.erl`, `erl_ddll.erl`, `erl_debugger.erl`
- File I/O and disk logs → `file.erl`, `file_server.erl`, `file_io_server.erl`, `raw_file_io*.erl`, `ram_file.erl`, `disk_log*.erl`, `wrap_log_reader.erl`
- Logging (`logger` framework and handlers) → `logger*.erl`
- Distributed replicated data (peer discovery, versioned updates) → `data_publisher.erl` (used by `pg.erl`)
- Distribution, node connectivity, name registration → `net_kernel.erl`, `net.erl`, `net_adm.erl`, `auth.erl`, `erl_epmd.erl`, `erl_distribution.erl`, `dist_util.erl`, `dist_ac.erl`, `*_dist.erl`, `*_epmd_socket.erl`, `global*.erl`, `pg.erl`, `pg2.erl`, `rpc.erl`, `erpc.erl`
- Sockets, addresses and DNS → `socket.erl`, `gen_tcp*.erl`, `gen_udp*.erl`, `gen_sctp.erl`, `inet*.erl`, `local_*.erl`, `inet_dns*.erl`, `inet_res.erl`, `inet_hosts.erl`, `inet_parse.erl`, `inet_gethost_native.erl`
- OS, terminal/shell I/O, group leaders → `os.erl`, `user_drv.erl`, `user_sup.erl`, `group.erl`, `prim_tty*.erl`, `standard_error.erl`, `group_history.erl`
- Tracing and erts introspection → `trace.erl`, `seq_trace.erl`, `erts_debug.erl`, `erl_erts_errors.erl`, `erl_kernel_errors.erl`
- `../include/` → public headers installed for other applications (sibling — read-only, escalate writes to the parent node)
- `../test/` → Common Test suites and data (sibling — read-only, escalate writes to the parent node)
- `../doc/` → doc sources (`docs.exs` groups, guides, references) and `kernel_app.md` (sibling — read-only, escalate writes to the parent node)
- `../examples/` → example distribution/transport implementations (`erl_uds_dist`, `uds_dist`, `gen_tcp_dist`, `socket_sctp`)

## See Also
- `../../erts/preloaded/src/` — preloaded modules that kernel relies on (`prim_file`, `erl_prim_loader`, `init`, `prim_socket`, ...).
- `../../erts/emulator/nifs/common/` — C implementations of the kernel-facing NIFs (`prim_tty_nif.c`, `prim_file_nif.c`).
- `../../make/app_targets.mk` — how `<app>.app`/`.src` targets and test targets are generated.
- `../doc/docs.exs` — which kernel modules are grouped/published in the generated documentation.
