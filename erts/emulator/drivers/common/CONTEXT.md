# erts/emulator/drivers/common/ — platform-independent built-in drivers

## Intent
The two live, statically-linked `erl_driver` (port) drivers that are OS-independent: `inet_drv.c` (the Erlang socket driver backing `gen_tcp`/`gen_udp`/`gen_sctp`, `inet_tcp`/`inet_udp`/`inet_sctp`, `local_tcp`/`local_udp`, all via the preloaded `prim_inet`) and `ram_file_drv.c` (in-memory file backing `lib/kernel`'s `ram_file`).
Both are compiled into `beam` via `DRV_OBJS` in `../Makefile.in` and registered at VM start through the generated `driver_tab[]`.
`inet_drv.c` is ~15.9k lines (the VM's largest single source of socket semantics); `ram_file_drv.c` is ~515 lines.

## API Surface
Public (to Erlang) surface is the drivers' registered names and the port-control / port-command wire protocol, not C symbols.

`inet_drv.c` registers up to four `erl_drv_entry` objects (`inet_drv.c:1348/1406/1446/1636`):
- `tcp_inet_driver_entry` (`:1348`), name `"tcp_inet"`; on unix ready_input=`tcp_inet_drv_input`, ready_output=`tcp_inet_drv_output`; on win32 ready_input=`tcp_inet_event`, ready_output=NULL.
- `udp_inet_driver_entry` (`:1406`, `#ifdef HAVE_UDP`), name `"udp_inet"`; ready_input=`packet_inet_drv_input`, ready_output=`packet_inet_drv_output` (win32: `packet_inet_event`/NULL).
- `sctp_inet_driver_entry` (`:1446`, `#ifdef HAVE_SCTP`), name `"sctp_inet"`; same callbacks as udp; `process_exit` is NULL.
- `inet_driver_entry` (`:1636`, non-static global, name `"inet"`) — placeholder whose only non-NULL field is `init=inet_init`; removed during `inet_init`.
All real entries set `extended_marker=ERL_DRV_EXTENDED_MARKER`, major/minor = `ERL_DRV_EXTENDED_{MAJOR,MINOR}_VERSION`.
`tcp_inet_driver_entry` flags = `ERL_DRV_FLAG_USE_PORT_LOCKING|ERL_DRV_FLAG_SOFT_BUSY`; udp/sctp flags = `ERL_DRV_FLAG_USE_PORT_LOCKING`; placeholder flags = 0.
`tcp_inet_driver_entry` callback set: init `tcp_inet_init` (`:11749`, returns 0), start `tcp_inet_start` (`:11785`), stop `tcp_inet_stop` (`:11895`), output `tcp_inet_command` (`:12629`), control `tcp_inet_ctl` (`:11952`), timeout `tcp_inet_timeout` (`:12547`), outputv `tcp_inet_commandv` (`:12657`), flush `tcp_inet_flush` (`:12708`), process_exit `tcp_inet_process_exit` (`:12732`), stop_select `inet_stop_select` (`:12773`), emergency_close `inet_emergency_close` (`:10862`); finish/handle/ready_async/call/event/handle2 are NULL.
udp/sctp entries use `packet_inet_init`/`packet_inet_stop`/`packet_inet_command`/`packet_inet_ctl`/`packet_inet_timeout` and have no outputv/flush/process_exit.
`ram_file_drv.c` registers `ram_file_driver_entry` (`ram_file_drv.c:106`), name `"ram_file_drv"`, flags 0; callbacks: init `rfile_init` (`:148`), start `rfile_start` (`:153`), stop `rfile_stop` (`:169`), output `rfile_command` (`:377`); everything else NULL (ready_input/ready_output/control/timeout/outputv/flush/call/event/process_exit/stop_select).

## Registration & driver table
`inet_init` (`inet_drv.c:4526`) runs at emulator start; it calls `add_driver_entry(&tcp_inet_driver_entry)` (`:4599`), `add_driver_entry(&udp_inet_driver_entry)` (`:4601`, `HAVE_UDP`), `add_driver_entry(&sctp_inet_driver_entry)` (`:4635`/`:4679`), then `remove_driver_entry(&inet_driver_entry)` (`:4688`) to drop the placeholder.
SCTP is added only after its symbols resolve: directly when `HAVE_SCTP_BINDX`, else via runtime `dlopen` of `LIBSCTP` (`erts_sys_ddll_open_noext`/`erts_sys_ddll_sym`/`..._vsym`, `:4643`-`:4683`), so the `"sctp_inet"` driver may be absent at runtime.
`../utils/make_driver_tab` (Perl) derives the exported symbol from the object base name (`inet_drv.o` → strips `drv` → `inet_` + `driver_entry` = `inet_driver_entry`) and emits `$(TTF_DIR)/driver_tab.c` containing `ErtsStaticDriver driver_tab[] = { {&inet_driver_entry, 0}, ... }` (rule `../Makefile.in:702`).
At startup `beam/io.c:3043` walks `driver_tab` and calls `erts_add_driver_entry(...)` → `init_driver` (`io.c:7672`) → `(*de->init)()`, i.e. `inet_init` runs then registers the tcp/udp/sctp entries through the same path (each entry's own `init` also fires, e.g. `tcp_inet_init`).
`add_driver_entry`/`remove_driver_entry` are declared in `../beam/erl_driver.h:447-448`.

## Wire protocol (Erlang ⇄ driver)
Control: Erlang `prim_inet:ctl_cmd/3` (`erts/preloaded/src/prim_inet.erl:2972`) issues `port_control(Port, Cmd+MAGIC, Args)` with `ERTS_INET_DRV_CONTROL_MAGIC_NUMBER = 0x03f1a300` (`../beam/sys.h:1466`); the driver's `control` callback subtracts the magic first (`tcp_inet_ctl:11958`, `packet_inet_ctl:14574`) and switches on `INET_REQ_*`/`TCP_REQ_*`/`SCTP_REQ_*` constants (`:805`-`:847`).
`tcp_inet_ctl` (`:11952`) owns OPEN(:11961), FDOPEN(:12003), LISTEN(:12042), CONNECT(:12064), ACCEPT(:12144), CLOSE(:12259), TCP_REQ_RECV(:12268), UNRECV(:12326), SHUTDOWN(:12339), SENDFILE(:12380); the `default:` branch (`:12447`) forwards all shared requests to `inet_ctl` (`:10988`), which handles BIND(:11409), GETOPTS(:11046)/SETOPTS(:11111), GETSTATUS/GETTYPE/GETSTAT, NAME/PEER + SETNAME/SETPEER, GETFD/IGNOREFD, GETHOSTNAME, GETIFLIST/IFGET/IFSET/GETIFADDRS, GETPADDRS/GETLADDRS, GETSERVBYNAME/GETSERVBYPORT, SUBSCRIBE.
`packet_inet_ctl` (`:14564`) owns OPEN/FDOPEN/CLOSE/CONNECT/LISTEN/SCTP_REQ_BINDX(:14873)/SCTP_REQ_PEELOFF(:14919)/PACKET_REQ_RECV(:14990) and forwards the rest to `inet_ctl` (`:15001`).
Control replies are a 1-byte result tag (`INET_REP_OK`=1, `INET_REP_ERROR`=0, `INET_REP`=2; `:869-871`) plus payload, built by `ctl_reply`/`ctl_reply_ok`/`ctl_error`/`ctl_xerror` (`:2260`/`:2280`/`:2300`/`:2307`).
Completed async control ops are delivered as terms: `{inet_reply, S, ok|{error,Reason}}` via `inet_reply_ok`(`:2689`)/`inet_reply_am`(`:2668`)/`inet_reply_error`(`:2745`)/`inet_reply_error_am`(`:2719`), and `{inet_async, S, Ref, ok|{error,Reason}|{ok,Port2}}` via `send_async_ok`(`:2494`)/`send_async_ok_port`(`:2515`)/`send_async_error`(`:2541`), all through `erl_drv_send_term` (never `driver_output_term`).
Replies are addressed to the caller captured at request time (`driver_caller(port)` into `desc->caller`/`inet_async_op.caller`; `init_caller`/`init_caller_iov` at `:1854`/`:1827`); an optional `caller_ref` tag is appended by `inet_reply_finish` (`:2617`).
Passive data is delivered as port data, not terms: `inet_port_data` (`:2753`) / `inet_port_binary_data` (`:2771`) use `driver_output`/`driver_output2`/`driver_output_binary`; `inet_async_data` (`:3162`) answers a pending `TCP_REQ_RECV` with `{inet_async,S,Ref,{ok,[Hsz|Data]}}`.
Port output (sends): `tcp_inet_command` (`:12629`) / `tcp_inet_commandv` (`:12657`) on TCP, `packet_inet_command` (`:15037`) on udp/sctp; each calls `init_caller` then either replies synchronously (`inet_reply_ok`/`inet_reply_error*`) or, on `EWOULDBLOCK` mid-send, keeps the port busy (`set_busy_port`) and defers the reply until `ready_output`.

## Concurrency / async model
No async threads: the file contains zero uses of `driver_async`/`driver_async_cancel`/`erl_drv_async`, and `ready_async` is NULL in every entry.
All I/O is non-blocking (`SET_NONBLOCKING(desc->s)` at `:5209`/`:5255`) driven by the emulator pollset through `driver_select`.
The `driver_select` wrapper is `inet_driver_select` (`:670`) → `driver_select(port, e, mode|(on?ERL_DRV_USE:0), on)`; `sock_select` (`:673`) additionally tracks `desc->event_mask` and passes `(ErlDrvEvent)desc->event`.
Event masks: `FD_READ=ERL_DRV_READ`, `FD_WRITE=ERL_DRV_WRITE`, `FD_CONNECT=ERL_DRV_WRITE`, `FD_ACCEPT=ERL_DRV_READ`, `FD_CLOSE=0` (`:627-631`).
The fd/event is created in `inet_ctl_open` (`:5201`, `sock_create_event`) and `inet_ctl_fdopen` (`:5253`); the socket fd is the event on unix. `desc->event` is torn down in `desc_close` (`:5067` `driver_select(...,ERL_DRV_USE,0)`, or `inet_stop_select` at `:5070`); win32 additionally hooks read via `driver_select(...,ERL_DRV_READ,1)` (`:5211`/`:5257`).
Because sockets are non-blocking, connect/accept/recv are asynchronous but execute on the emulator thread when the poll becomes ready, tracked by a fixed-size queue `INET_MAX_ASYNC = 1` (`:1069`) with `enq_async`/`enq_async_w_tmo` (`:2448`/`:2413`) and `deq_async`/`deq_async_w_tmo` (`:2486`/`:2453`); `inet_async_op` is defined at `:1194`.
- non-blocking connect: `sock_connect` returning `EINPROGRESS`/`ERRNO_BLOCK` → `sock_select(FD_CONNECT,1)`, optional `driver_set_timer`, `enq_async(INET_REQ_CONNECT)` (`:12091`-`:12114`); completion in `tcp_inet_output` when `INET_STATE_CONNECTING` (`:14242`) → `async_ok`.
- non-blocking accept: `sock_accept` `ERRNO_BLOCK` → `enq_async_w_tmo(INET_REQ_ACCEPT,...)`, `sock_select(FD_ACCEPT,1)` (`:12214`-`:12227`); completion in `tcp_inet_input` when `INET_STATE_ACCEPTING` (`:13469`), or immediately via `tcp_inet_copy` + `async_ok_port` (`:12236`-`:12252`).
- passive recv: `enq_async(..., TCP_REQ_RECV)` (`:12268`+) with `driver_set_timer`; completion in `tcp_inet_input`.
- UDP/SCTP passive recv: `enq_async(..., PACKET_REQ_RECV)` (`:14990`), completion in `packet_inet_input` (`:15361`).
Multi-accept uses an unbounded side list (`multi_first`/`multi_last`, `inet_async_multi_op` at `:1205`) plus `MultiTimerData` timers and `driver_monitor_process` for caller-death cleanup.
Timers: `driver_set_timer`/`driver_cancel_timer`/`driver_read_timer`; the `timeout` callback fires `tcp_inet_timeout` (`:12547`) or `packet_inet_timeout` (`:15011`) → `{error,timeout}`.
Caller liveness: `driver_monitor_process`/`driver_demonitor_process` on async connect/accept; `tcp_inet_process_exit` (`:12732`) cancels the pending op and restores listener state.

## Constraints
- Never edit the generated `$(TTF_DIR)/driver_tab.c`; regenerate via the `driver_tab.c:` rule in `../Makefile.in:702`.
- The exported entry symbol is mechanically derived from the object base name (`foo_drv.o` → `foo_driver_entry`); renaming a file here silently breaks the generated table link.
- Driver `driver_name` strings are the ABI used by Erlang (`"tcp_inet"`, `"udp_inet"`, `"sctp_inet"`, `"ram_file_drv"`); changing one breaks the matching Erlang module at runtime.
- `common/` must compile on every platform; OS differences belong in `../drivers/{unix,win32}/`, not `#ifdef` walls in these files.
- `inet_drv.c` shares header/packet parsing with the `decode_packet` BIF (`../beam/packet_parser.c`, included at `:698`); packet-option changes may need mirroring there.
- Avoid wholesale reformatting of `inet_drv.c` (huge conflict surface).

## Design Decisions
- TCP/UDP/SCTP are three separate driver entries (own `driver_name`, independently present) that share descriptor code, rather than one driver with a protocol switch.
- `inet_driver_entry` exists only so the name-derived `driver_tab` has an entry to register for `inet_drv.o`; its `init` registers the real drivers, then removes itself.
- The driver manages sockets through the emulator pollset (port locking on, soft-busy) rather than dirty schedulers; this is why there are no async threads here.
- `ram_file_drv` keeps the whole file in one resizable `ErlDrvBinary` (`RamFile` at `ram_file_drv.c:137`; grow by `BFILE_BLOCK` rounding in `ram_file_expand` `:290`), a deliberate simplification over a block vector.

## Known Issues / Gotchas
- The `dummy_tcp_driver_entry`/`dummy_udp_driver_entry`/`dummy_sctp_driver_entry` structs (`inet_drv.c:2196/2212/2229`, win32-only `#ifdef __WIN32__`) are defined but never referenced or `add_driver_entry`'d anywhere in the tree — they are dead code, not a runtime fallback.
- `ram_file_drv.c`'s `#ifdef LOADABLE` block (`:147`-`:166`, `driver_init`/`rfile_finish`) is dead: the driver is static-only.
- `ram_file_drv.c`'s `rfile_command` (`:397`) silently ignores unknown opcodes ("let the caller hang", `:531`) — no reply is sent, which is expected by `ram_file.erl` for unsupported ops.
- `inet_drv.c` uses the private `prim_inet` protocol as its control ABI (magic-number-offset cmd + packed binaries), not `erl_driver`'s higher-level helpers; the Erlang half must stay in sync (`erts/preloaded/src/prim_inet.erl`).
- The socket `driver_select` event is the OS fd on unix and a Winsock event handle on win32 (`inet_descriptor.event`, `:1225`), so event-based reasoning differs per platform.
- `INET_MAX_ASYNC` is 1 (one outstanding async op per descriptor); a descriptor running multi-accept keeps additional ops in a separate list.
- `driver_output_term` is never used; term replies all go through `erl_drv_send_term`.

## Dependencies
- POSIX/Winsock sockets, `netdb`/resolv; SCTP via headers+`-lsctp` (`LIBSCTP`) or runtime `dlopen`.
- `../sys/$(ERLANG_OSTYPE)` (`sys.h`, `erts_sys_ddll_*`), `../beam/erl_driver.h` (driver API), `../beam/packet_parser.h` (decode_packet parsing), generated `$(TARGET)/config.h` feature macros (`HAVE_SCTP`, `HAVE_UDP`, `HAVE_IN6`, `HAVE_SENDFILE`, `HAVE_SYS_UN_H`, `HAVE_SETNS`).
- Erlang-side counterparts that must stay in sync: `erts/preloaded/src/prim_inet.erl`, `lib/kernel/src/ram_file.erl`.

## Test Strategy
- Driver/port API semantics: `../test/driver_SUITE.erl` (uses external example drivers, not these static ones).
- `inet`: `lib/kernel/test/{inet_SUITE,inet_sockopt_SUITE,inet_res_SUITE,gen_tcp_*,gen_udp_SUITE,gen_sctp_SUITE}.erl`; port plumbing in `../test/port_SUITE.erl`, `../test/trace_port_SUITE.erl`.
- `ram_file_drv`: `lib/kernel/test/ram_file_SUITE.erl`; `lib/kernel/test/gen_tcp_misc_SUITE.erl` checks `open_port({spawn_driver,"ram_file_drv"},...)`.
- Run through the emulator test target (`make test` / `emulator.spec`); memory-safety via `TYPE=valgrind`/`asan` (`../valgrind/suppress.standard` has `inet_drv` suppressions).

## Routing Table
- (no child nodes) — this directory has no subdirectories.
- `../unix/`, `../win32/`, `../vxworks/` → platform-specific drivers (in `../`; `unix/` + `vxworks/` are legacy/non-linked)
- `../nifs/` → NIF counterparts incl. the newer `socket` API (`prim_socket_nif.c`), `prim_file`, `zlib`, `zstd`
- `../beam/` → `erl_driver.h` (driver API), `io.c` (driver_tab registration), `packet_parser.c`
- `../sys/` → pollset (`erl_check_io`), `sys_*` helpers, `erts_sys_ddll_*`
- `../utils/make_driver_tab` → generates `driver_tab.c`
