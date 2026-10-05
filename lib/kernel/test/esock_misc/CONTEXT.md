# esock misc test tools

## Intent
Hand-run, standalone socket ("esock") test tools that are NOT part of any Common Test suite — they are started manually from an Erlang shell to drive traffic, inspect `socket` options, or stress the esock statistics counters.
Two independent tool sets live here: the generic client/server pair (`socket_client` + `socket_server` + `socket_lib`) and the io-wait/counter-wrap stress pair (`esock_iow_client` + `esock_iow_server` + `esock_iow_lib`).
Originally lived in `erts/emulator/test/esock_misc/` and were moved here (with `../esock_ttest/`) because they belong to `kernel/test`.

## API Surface
- `socket_lib.erl` — shared helper module for the client/server pair: `sleep/1`, a tagged request/reply protocol (`request/3`, `reply/4`, messages `{Tag, Pid, Ref, Request}`), message framing (`enc_msg/3`, `dec_msg/1` = `<<Type:32, N:32, Data/binary>>` with `?REQ=0`/`?REP=1`), and timestamped `i/e` logging (uses the process dictionary key `sname`).
- `socket_client.erl` — traffic-generating client on top of the `socket` API; `start/1,2`, `start/5,6`, `start_tcp*`/`start_udp*` / `*4` / `*6` variants; spawns `Num` monitored client processes and waits for them all to die.
  Each client opens/binds (`any`), sets `socket timestamp`, `ip tos` (cycled 1..100 per message) and `ip recvtos`/`recvttl`, connects (stream) or records `dest` (dgram), sleeps 5 s, then loops sending 10 messages at a time.
  `Verbose` (true only for a single client) dumps socket/ip option values with `socket:getopt/3`; `msg = true` internal mode uses `sendmsg/recvmsg` with an `ip`/`tos` control message header.
- `socket_server.erl` — server for the same pair: manager process + 5 acceptors (`?NUM_ACCEPTORS`) + one handler per accepted connection; `start/0,5`, `start_tcp*` (`inet`/`inet6`, `*4`/`*6`), `start_udp*`, `start_sctp/0,1`.
  `start/5` takes `Domain, Type, Proto, UseMsg, Peek`: `UseMsg` selects `sendmsg`/`recvmsg` (with `ip`/`tos` control headers), `Peek` selects a `recv(... , [peek])` / `peek_recvfrom` peek-then-real-read path; dgram uses a single handler and no acceptors.
  Binds the first non-`lo` interface address with an ephemeral port (`socket:bind(Sock, SA)` returns the port, which is printed) and then loops until the manager dies.
- `esock_iow_lib.erl` — logging/format helpers used only by the `esock_iow_*` pair: `format_counters/1,2,3` (formatting `socket:info/1` traffic/listen counter maps), `iprint/eprint`, `f/2`, `fts/0,1`.
- `esock_iow_client.erl` — `start/1` (server port): opens TCP, `socket:setopt(S, otp, iow, true)`, binds/connect to a hardcoded local address, waits for the server's `<<"begin">>`, then sends a fixed 250-byte message in a loop, printing all counters every time a `{'$socket', S, counter_wrap, Cnt}` message arrives.
- `esock_iow_server.erl` — `start/0`: mirror of the client (open, `otp iow` true, bind, listen, accept, send `<<"begin">>`, recv loop with `flush/1` draining `counter_wrap` messages).
- Both `esock_iow_*` modules use `?LIMIT` 100000 messages between counter dumps.

## Notes for Agents
- These modules are NOT listed in `../Makefile` `MODULES`/`SOCKET_MODULES`/`ERL_FILES`, so they are neither compiled by `make` in `../` nor installed into `$(RELEASE_PATH)/kernel_test` — compile manually (e.g. `erlc -o ../ebin esock_misc/*.erl`) before use.
- No module has a `main/1` or escript entry point; they are started from an Erlang shell, e.g. `socket_server:start_tcp4()`, then `socket_client:start_tcp4(Port)` with the port the server printed.
- Nothing in the repository references these modules (no suite, script, or Makefile target) — they are purely manual tools; `../esock_ttest/` wrappers use the *different* `socket_test_ttest_*` framework in the parent directory, not these files.
- `esock_iow_*` exist specifically to exercise the `otp` level socket option `iow` ("Inform On Wrap" of statistics counters, see `lib/kernel/src/socket.erl`): the wrap message `{'$socket', Socket, counter_wrap, Counter}` is produced only from C (`erts/emulator/nifs/common/prim_socket_nif.c`), is not documented in the `socket` module docs nor present in `prim_socket.erl`, and its trigger threshold depends on the erts compile-time counter size (`ESOCK_COUNTER_SIZE`, 16/24/32/48/64 bits in `erts/emulator/nifs/common/prim_socket_int.h`).

## Known Issues
- `esock_iow_client.erl` and `esock_iow_server.erl` hardcode the local IPv4 address `{147,214,93,147}`; they only bind/connect successfully on a host that actually owns that address.
- `esock_iow_client:start/1` and `esock_iow_server:start/0` loop forever (the loop resets its counter instead of terminating) — they must be killed manually.
- `socket_client`/`socket_server` require a non-loopback interface address: their `which_addr/1` (and `which_addr/2`) pick the first non-`lo` interface with an address of the requested family and `throw(no_address)` on hosts that only have `lo` or only the other family.
- `socket_client.erl` still carries large commented-out blocks (old enc/dec message helpers, timestamp helpers); they are dead text, not live API.
- `socket_server`'s SCTP support is explicitly incomplete — `do_manager_init(..., seqpacket, sctp, ...)` only opens the socket, dumps `sctp` options, sets events and closes it.

## Constraints
- Keep these tools free of Common Test dependencies: they must stay runnable from a plain shell with only `kernel`/`stdlib` loaded.
- Do not add them to `../Makefile` `MODULES`/`SOCKET_MODULES` without checking that the release install step (`release_tests_spec` in `../Makefile` uses `$(INSTALL_DATA) $(ERL_FILES) ...` and only `*_SUITE_data` dirs) also ships them.

## Test Strategy
- No automated tests: these are developer-invoked tools, so they are not covered by `make test` in `../` and any regression in them will not be caught by CI.
- Typical use is two shells (or one shell plus a remote node): start the server, read the printed port, then run the client with that port.

## See Also
- `../` — the flat `kernel` Common Test directory (`Makefile`, `socket_test_lib`, `socket_*_SUITE`, and the automated `socket_test_ttest_*` traffic framework); these tools are not wired into it.
- `../esock_ttest/` — sibling escript/sh wrappers around the `socket_test_ttest_*` framework (read-only from here; escalate changes to the parent).
- `../../src/socket.erl` — the `socket` API these tools exercise (`otp`/`iow` option, `info/1` counters).
