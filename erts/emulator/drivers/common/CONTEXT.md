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
`INET_REQ_SETOPTS` is owned by `inet_ctl` (`:11111-11150`) → `inet_set_opts` (`:6871`, SCTP delegates to `sctp_set_opts` at `:6904-6905`); the return code selects the follow-up action, not the reply: `-1`→`ctl_error(EINVAL)`, `0`→`ctl_reply_ok`, `1`→`tcp_deliver`+`ctl_reply_ok`, default→`tcp_recv`+`ctl_reply_ok`. `{active,true|once|N}` therefore always answers with the 1-byte `INET_REP_OK` (rlen==1), which `prim_inet:ctl_cmd` renders as `{ok,[]}` and `prim_inet:setopts/2` as `ok`.
`{active,once}`/`{active,N}` are encoded as an extended option (2 extra bytes for N) in `prim_inet:enc_opt_val/1` (`:2406-2410`) — the only options with a non-5-byte arg encoding.
Control replies are a 1-byte result tag (`INET_REP_OK`=1, `INET_REP_ERROR`=0, `INET_REP`=2; `:869-871`) plus payload, built by `ctl_reply`/`ctl_reply_ok`/`ctl_error`/`ctl_xerror` (`:2260`/`:2280`/`:2300`/`:2307`); `ctl_reply` writes the tag at `rbuf[0]` (`*ptr++ = rep`, `:2271`) and returns `len+1`.
`rbuf`/`rlen` semantics: the driver returns a raw BYTE buffer, and the VM turns it into an Erlang **LIST of small integers**, not a binary.
`rbuf` is the VM's stack buffer (`char resp_buf[ERL_ONHEAP_BINARY_LIMIT]`, `beam/io.c:4365`/`:4186`) unless the reply is larger, in which case `ctl_reply` `ALLOC`s (`:2265-2268`) and the VM `driver_free`s it.
The list conversion is `write_port_control_result` → `buf_to_intlist(hpp, resp_bufp, resp_size, NIL)` (`beam/io.c:4171`, builder in `beam/utils.c:3040`); the binary branch (`beam/io.c:4136`) is taken only when `prt->control_flags & PORT_CONTROL_FLAG_BINARY`, i.e. only for drivers that call `set_port_control_flags` — `inet_drv.c` and `ram_file_drv.c` never do, so `erlang:port_control/3` on these ports can never return a binary.
Erlang side: `prim_inet:ctl_cmd/3` (`erts/preloaded/src/prim_inet.erl:2972-2979`) pattern-matches the list: `[?INET_REP_OK|Reply] -> {ok,Reply}; [?INET_REP] -> inet_reply; [?INET_REP_ERROR|Err] -> {error,list_to_atom(Err)}`, so a 1-byte `INET_REP_OK` reply yields `{ok, []}`.
The `INET_REP` (2) tag is the "answer arrives later as `{inet_reply,S,Res}`" case — e.g. SCTP getopts/subscribe (`inet_subscribe` sends the full term itself and returns the 1-byte tag, `:10721-10723`).
Completed async control ops are delivered as terms: `{inet_reply, S, ok|{error,Reason}}` via `inet_reply_ok`(`:2689`)/`inet_reply_am`(`:2668`)/`inet_reply_error`(`:2745`)/`inet_reply_error_am`(`:2719`), and `{inet_async, S, Ref, ok|{error,Reason}|{ok,Port2}}` via `send_async_ok`(`:2494`)/`send_async_ok_port`(`:2515`)/`send_async_error`(`:2541`), all through `erl_drv_send_term` (never `driver_output_term`).
Replies are addressed to the caller captured at request time (`driver_caller(port)` into `desc->caller`/`inet_async_op.caller`; `init_caller`/`init_caller_iov` at `:1854`/`:1827`); an optional `caller_ref` tag is appended by `inet_reply_finish` (`:2617`).
Passive data is delivered as port data, not terms: `inet_port_data` (`:2753`) / `inet_port_binary_data` (`:2771`) use `driver_output`/`driver_output2`/`driver_output_binary`; `inet_async_data` (`:3162`) answers a pending `TCP_REQ_RECV` with `{inet_async,S,Ref,{ok,[Hsz|Data]}}`.
Passive `recv` control replies carry ONLY the async id: `TCP_REQ_RECV` (`:12268-12322`) and `PACKET_REQ_RECV` (`:14968-14999`) declare `char tbuf[2]`, `enq_async(desc, tbuf, REQ)` writes the 16-bit id (`put_int16(id, buf)`, `:2443-2444`), and the reply is `ctl_reply(INET_REP_OK, tbuf, 2, rbuf, rsize)` → Erlang `{ok,[R1,R0]}` → `Ref = ?u16(R1,R0)` (`prim_inet.erl:822`, `:855-857`). The datagram itself never travels in the control reply.
The datagram arrives later as a term built in C and sent with `erl_drv_send_term` **to the captured caller** (`opp->caller = driver_caller(desc->port)`, `:2427`), not to the port owner: `inet_async_binary_data` (`:3869-3981`) emits `{inet_async, S, Ref, {ok, [AddrBytes... | Data]}}` (`LOAD_TUPLE 4` at `:3977`), with `{ok,{[F|AddrData],AncData}}` (an extra `LOAD_TUPLE 2` at `:3968`) when `recvmsg` ancillary data is present; TCP uses `inet_async_data` (`:3162-3203`) with the same 4-tuple.
The address prefix in that reply is an Erlang LIST of small ints, not a binary: `ERL_DRV_STRING` maps to `buf_to_intlist` (`beam/io.c:6032-6038`) and `ERL_DRV_STRING_CONS` conses onto a list (`:6042-6045`); only the payload tail is a binary (`LOAD_BINARY` / `LOAD_BUF2BINARY`, i.e. `[H1..Hsz | Binary]`, `:3957-3960`). The driver-side macros are `LOAD_STRING`/`LOAD_STRING_CONS` (`:1766-1778`).
Where the address gets prepended to the payload: `packet_inet_input` formats the peer address with `inet_get_address(abuf, &other, &len)` (`:4890`, layout `Family(1B), Port(2B), IP(4|16B)`) into the SAME binary buffer just before the data (`sys_memcpy(ptr, abuf, len)`, `:15535-15536`), so `[addr|data]` is contiguous; `packet_reply_binary_data(desc, len, udesc->i_buf, sizeof(other)-len, nsz, mp)` (`:15561`) passes that as `hsz`/`offs`, and `inet_async_binary_data` splits it again via `hsz = desc->hsz + phsz` (`:3874`) and `bin->orig_bytes+offs` (`:3953`/`:3960`).
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
Active-mode delivery is entirely driver-driven — **no Erlang code participates**: the pollset's `ready_input` (`packet_inet_drv_input` for udp/sctp `:1416`/`:1456`, `tcp_inet_drv_input` for tcp) reads the socket and calls `packet_binary_message` (`:4106`) / `tcp_message` (`:3987`) / `tcp_binary_message` (`:4026`) → `erl_drv_output_term(desc->dport, ...)` straight into the port owner's mailbox. Shapes: `{udp, S, IP, Port, Data}` (`LOAD_TUPLE 5`, `:4158`; 6-tuple `{udp,S,IP,Port,AncData,Data}` when `recvmsg` ancillary data is present, `:4155`), `{sctp, S, IP, Port, {[AncData],Data_or_Event}}` (`:4200`), `{tcp, S, Data}` (`:4002`/`:4014`), `{tcp_closed,S}` (`:4073`), `{tcp_error,S,Atom}` (`:4090`), `{udp_passive,S}`/`{sctp_passive,S}` (`packet_passive_message`, `:4215`). Address and headers are again `LOAD_STRING`/`load_address` (`:1916`) i.e. lists of small ints, and the payload is a binary in `{mode,binary}` (`LOAD_BINARY`/`LOAD_BUF2BINARY`).
Dispatch point: `packet_reply_binary_data` (`:4335`) picks passive (`inet_async_binary_data` → `erl_drv_send_term` to caller) vs active (`INET_DELIVER_PORT` → `inet_port_binary_data` `:2771` raw port data, else `packet_binary_message` → `erl_drv_output_term` to owner); the TCP equivalent is `tcp_reply_data` (`:4279`) / `tcp_reply_binary_data` (`:4306`). `{deliver,port}` is what distribution uses (`:4272-4273`).
`{active,N}` countdown lives in C too: `INET_MULTI`=3 (`:785-791`), `active_count` (`:1256`), `INET_CHECK_ACTIVE_TO_PASSIVE` (`:1554`) fires `packet_passive_message`, and going to `N==0` in `inet_set_opts` (`:6995-6998`) flips `desc->active` to `INET_PASSIVE` and emits `{*_passive,S}` from C.
Caller liveness: `driver_monitor_process`/`driver_demonitor_process` on async connect/accept; `tcp_inet_process_exit` (`:12732`) cancels the pending op and restores listener state.

## Read/delivery data path (line anchors in `inet_drv.c`)
- `desc->hsz` (the application-level "header" option `INET_LOPT_HEADER`) is written in only two places: init to `0` (`:10922`) and the setopts arm (`:6936`) — it is 0 unless `{header,N}` is set, i.e. TCP does NOT always pass hsz=0.
- The *packet-framing* header (1/2/4-byte length, line mode, FCGI padding…) is stripped BEFORE the reply functions, by `packet_get_body()` (`../beam/packet_parser.h:152-164`) called from `tcp_reply_data` (`:4285`) and `tcp_reply_binary_data` (`:4314`); `[H1..Hsz | Data]` is therefore the first `hsz` bytes of the *body*, not the framing bytes.
- `tcp_inet_input` (`:13461`) only dispatches accept / `tcp_recv(desc,0)` (`:13599`) and never touches `hsz`; `tcp_deliver` (`:13042`) loops packet-by-packet calling `tcp_reply_binary_data` (`:13069`) or `tcp_reply_data` (`:13094`).
- The `(mode == INET_MODE_LIST) || (hsz > len)` short-packet/list arm is duplicated verbatim in `inet_port_data` (`:2760`), `inet_port_binary_data` (`:2779`), `inet_async_data` (`:3182`), `inet_async_binary_data` (`:3951`), `tcp_message` (`:4000`), `tcp_binary_message` (`:4038`), `packet_binary_message` (`:4142`).
- UDP/SCTP: `packet_inet_input` (`:15361`) reads at most `udesc->read_packets` datagrams per call (`:15367` + `while(packet_count--)` `:15378`; default `INET_PACKET_POLL`=5 `:1072`, set at `:14508`, option `INET_LOPT_UDP_READ_PACKETS` `:7138`) and returns after the first packet in passive mode (`:15570-15574`) ⇒ one datagram per call.
- Packed address header for IPv4 = 7 bytes (`addrlen` `:1156-1160` = 1 family + 2 port + 4 addr); `inet_get_address` (`:4890`) writes the family byte, then the *host-order* port via `put_int16` (MSB first = big-endian, `../beam/sys.h:1314`), then the raw `sin_addr` bytes in network order. `load_address` (`:1916`) decodes the same layout with `get_int16`.
- `INET_CHECK_ACTIVE_TO_PASSIVE` (`:1552`) fires **per delivered packet** (call sites `:4302`, `:4329`, `:4352`); `{active,once}` flips to passive silently, `{active,N}` emits `{tcp|udp|sctp}_passive` only when its counter hits 0.
- Readiness arming = "`active > 0`" (`:7966` setopts, `:12126`/`:14286` tcp connect, `:14621`/`:14682` udp open/fdopen, `:9030` sctp setopts) **or** "an async recv op is queued and hit `EWOULDBLOCK`" (tcp `:12317`, only when `timeout != 0`; udp `:15496` inside `packet_inet_input`).

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
- `ram_file_drv` keeps the whole file in one resizable `ErlDrvBinary` (`RamFile` at `ram_file_drv.c:137`; grow by `BFILE_BLOCK` rounding in `ram_file_expand` `:270`), a deliberate simplification over a block vector.

## Known Issues / Gotchas
- The `dummy_tcp_driver_entry`/`dummy_udp_driver_entry`/`dummy_sctp_driver_entry` structs (`inet_drv.c:2196/2212/2229`, win32-only `#ifdef __WIN32__`) are defined but never referenced or `add_driver_entry`'d anywhere in the tree — they are dead code, not a runtime fallback.
- `ram_file_drv.c`'s `rfile_command` (`:377`) silently ignores unknown opcodes ("let the caller hang", `:512`) — no reply is sent, which is expected by `ram_file.erl` for unsupported ops.
- `inet_drv.c` uses the private `prim_inet` protocol as its control ABI (magic-number-offset cmd + packed binaries), not `erl_driver`'s higher-level helpers; the Erlang half must stay in sync (`erts/preloaded/src/prim_inet.erl`).
- The socket `driver_select` event is the OS fd on unix and a Winsock event handle on win32 (`inet_descriptor.event`, `:1225`), so event-based reasoning differs per platform.
- `INET_MAX_ASYNC` is 1 (one outstanding async op per descriptor); a descriptor running multi-accept keeps additional ops in a separate list.
- The deprecated `driver_output_term` is never used; term emission splits into exactly two APIs: `erl_drv_send_term` (16 uses) for control/async **replies sent to the captured caller** and `erl_drv_output_term` (16 uses) for **active-mode messages sent to the port owner** (`desc->dport`).
- `inet_drv.c`'s interface-constant block (`:752` "This section must be \"identical\" to the corresponding inet_int.hrl", `INET_REP_*` at `:869-871`, `INET_LOPT_*` at `:896-900`, `INET_REQ_*`/`TCP_REQ_*`/`PACKET_REQ_*`) is duplicated by hand in `lib/kernel/src/inet_int.hrl` (`INET_REP_OK` at `:126`, `TCP_REQ_RECV` at `:110`, `PACKET_REQ_RECV` at `:116`). Changing an opcode/option number requires editing BOTH; nothing enforces the sync at build time.
- `{active,N}` with an overflowing/underflowing sum returns `-1` (⇒ `EINVAL` to Erlang) at `:6985` *after* `desc->active` has already been set to `INET_MULTI` at `:6974`, so the descriptor is left in active mode with the old counter. A negative-but-representable result is instead clamped to 0 (`:6993`) and immediately flips to passive with a passive message (`:6995-6998`). `{active,true}`/`{active,false}` zero the counter (`:7000`).
- A UDP `{active,false}` → `{active,true}` setopts arms read (`:7966`) but delivers **nothing** during that call: `inet_set_opts` returns 1/2 only for `SOCK_STREAM` (`:7950`/`:7970-7985`), so `inet_ctl` (`:11130`) just replies ok. Already-buffered datagrams are delivered by the next poll-driven `packet_inet_drv_input` (`:15352`), not inside setopts — the two `UDP sockets could also trigger immediate read here NIY` comments (`:7949`, `:7969`) mark this gap. TCP, by contrast, does deliver synchronously inside the setopts ctl (`tcp_deliver` `:11137`, `tcp_recv`+re-arm `:11145-11146`).

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
- `../win32/` → win32-specific driver (`registry_drv.c`)
- `../nifs/` → NIF counterparts incl. the newer `socket` API (`prim_socket_nif.c`), `prim_file`, `zlib`, `zstd`
- `../beam/` → `erl_driver.h` (driver API), `io.c` (driver_tab registration), `packet_parser.c`
- `../sys/` → pollset (`erl_check_io`), `sys_*` helpers, `erts_sys_ddll_*`
- `../utils/make_driver_tab` → generates `driver_tab.c`
