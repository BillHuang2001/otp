# erl_interface — `global` name-server client

## Intent
Client side of the Erlang `global` name server for C nodes: register/unregister a name, look a name up, and list all registered names.
There is no C implementation of `global` — every call is an RPC to the `global` module on a *remote Erlang node* over a connection the caller already opened with `ei_connect`.
One C file per public function; no subdirectories, no shared header of its own.

## API Surface (public functions → file)
- `char **ei_global_names(ei_cnode *ec, int fd, int *count)` → `global_names.c`
- `int ei_global_register(int fd, const char *name, erlang_pid *self)` → `global_register.c`
- `int ei_global_unregister(ei_cnode *ec, int fd, const char *name)` → `global_unregister.c`
- `int ei_global_whereis(ei_cnode *ec, int fd, const char *name, erlang_pid *pid, char *node)` → `global_whereis.c`
- Prototypes live in `../../include/ei_global.h` and are duplicated verbatim in `../../include/ei.h` (section "The ei_global functions", ~lines 631–636).
- `ec` is only used to obtain the cnode pid via `ei_self(ec)`; `ei_global_register` takes that pid directly (`erlang_pid *self`), an ABI asymmetry inherited from the historical separate implementations.
- Return convention: `0` on success / `ERL_ERROR` (`-1`) on failure for the three `int` functions; `ei_global_names` returns a `char **` or `NULL`.
- Failure sets the thread-local `erl_errno` via `EI_CONN_SAVE_ERRNO__` (`EINVAL` on encode failure, `EBADMSG` on bad reply, `ENOMEM`, or `EMSGSIZE` from `ei_recv_internal`).

## Wire Mechanism (verified)
- All four hand-roll the Erlang `rex` RPC protocol instead of calling `ei_rpc*`: encode `{PidFrom, {call, global, Fun, Args, user}}` and send it with `ei_send_reg_encoded(fd, self, "rex", buf, index)` (registered process name `rex`; `connect/send_reg.c`).
- Remote calls: `global:registered_names()` (args `[]`), `global:register_name_external(Name, Pid, {global, cnode})`, `global:unregister_name_external(Name)`, `global:whereis_name(Name)`.
- Replies are received with `ei_recv_internal(fd, &bufp, &index, &msg, &msglen, staticbufp, 0)` and expected as `{rex, Reply}`: `[Name,...]` for names, the atom `yes` for register, the atom `ok` for unregister, a pid for whereis.
- `register`/`unregister` additionally consume the `LINK`/`UNLINK` (and monitor) control messages that `global_name_server` sets up between itself and the registering cnode pid; `register` requires link *and* monitor to have been seen only in the sense that a repeated one is an error, not that they must precede the reply.
- Receive loops retry on `ERL_TICK` (`i == 0`) and use a zero timeout (`ms = 0` → infinite), so every call blocks forever if the remote node stops answering — unlike `ei_rpc` there is no `_tmo` variant.
- `ei_recv_internal` is called with `staticbufp = 1` (stack buffer, no `realloc`), so an unexpectedly large reply is discarded and reported as `EMSGSIZE`.

## Buffer / Ownership Semantics
- `ei_global_names`: the returned array is one `malloc` block holding `arity+1` pointers followed by the NUL-terminated name strings; it is `NULL`-terminated and `free(array)` is the only cleanup needed (also stated in `../../doc/references/ei_global.md`).
- `*count` is written only on the success path (set to 0 then incremented per decoded name); on failure `*count` is untouched.
- Size of the `ei_global_names` reply buffer is `GLOBALNAMEBUF` = 16 KB on the stack; the other three use `EISMALLBUF` = 2048 from `../misc/eidef.h`.
- Because the encode calls are unbounded, `name` longer than roughly the stack buffer size overflows the request buffer (`ei_encode_atom` does not bound-check); keep names well under 2 KB.

## Constraints
- `"eidef.h"` must be included first (it centralizes the generated `config.h` and pulls in `ei.h`); the remaining internal headers are `eiext.h`, `eisend.h`, `eirecv.h`, `ei_connect_int.h`, `ei_internal.h`, `ei_connect.h`.
- No locks, no mutable static state: `README.internal` requires `ei` to be reentrant/async-safe; each function only touches the caller-supplied `fd`, `ec` and stack buffers.
- The caller-supplied `fd` must be dedicated to the global operation (documented in `../../doc/references/ei_global.md`) — no other traffic may be interleaved, or the reply decoding sees foreign control messages.
- Built as part of `libei*.a`/`libei*.so`: files are listed in `GLOBALSOURCES` in `../Makefile.in`, compiled via `vpath %.c connect:encode:decode:misc:epmd:global:erts` so they are referred to by basename.
- `PROG_CFLAGS` in `../Makefile.in` adds `-Iglobal`; `../prog/ei_fake_prog.c` relies on that include path for its compile-only prototype checks.
- Interoperate with the `global` module's wire protocol — changing the encoded atoms/tuple shape breaks the cnode↔Erlang contract.

## Known Issues
- `../../include/ei_global.h` is **not** in the `HEADERS` list of `../Makefile.in` (only `ei.h`, `ei_connect.h`, `eicode.h`), so it is not installed into `$(RELSYSDIR)/include` nor `$(RELEASE_PATH)/usr/include`; the functions remain reachable because `ei.h` duplicates the prototypes. The `.c` sources themselves *are* installed to `$(RELSYSDIR)/src/global`.
- The switch labels `case ERL_MONITOR_P-10:` (`global_register.c`) and `case ERL_DEMONITOR_P-10:` (`global_unregister.c`) are intentional, not typos: `ei_recv_internal` returns `msg->msgtype - 10` for any `msgtype > 10`, so monitor/demonitor messages surface as 9/10.
- `ei_global_whereis` returns `ERL_ERROR` *without* saving `erl_errno` when the received control message is not `ERL_SEND` (the `if (i != ERL_SEND) return ERL_ERROR;` path) — the caller sees a stale `erl_errno`.
- `ei_global_whereis` copies the node name with `strcpy(node, pid.node)` and performs no bounds check; the caller's `node` buffer must hold a full node name (see the documented `char node[256]` usage).
- An unregistered name is not a distinct return value: `global:whereis_name/1` replies `undefined`, `ei_decode_pid` fails and the function reports `-1`/`EBADMSG`.
- `ei_global_register`'s resolve method `{global, cnode}` refers to a function that does not exist in the current `lib/kernel/src/global.erl`; `global:register_name_external/3` does not normalize `{M,F}` methods (no `allow_tuple_fun` call) and `resolve_it/4` wraps the call in `catch`, so a name clash involving a cnode-registered name ends in the "unexpected return value" branch and the conflicting name is deleted rather than resolved in the cnode's favour.
- `register`/`unregister` treat any control message other than `LINK`/`UNLINK`, monitor/demonitor and `SEND` as `EBADMSG`; they do not tolerate `EXIT`.

## Test Strategy
- Common Test suite `../../test/ei_global_SUITE.erl`, cases `ei_global_registration`, `ei_global_whereis`, `ei_global_names`; run with `make test` in `lib/erl_interface` (or `ct_run -suite ei_global_SUITE`).
- The suite drives the C program `../../test/ei_global_SUITE_data/ei_global_test.c` through `../../test/runner.erl`, which `open_port({spawn, ...})`s the executable and exchanges external-format terms over stdio.
- `ei_global_test_cases.hrl` and `ei_global_test_decl.c` are generated by the `init_tc` step in `../../test/ei_global_SUITE_data/Makefile.first` from the `TESTCASE(...)` macros — do not edit them.
- The C side keeps a file-static `ei_cnode ec` (from `cmd_ei_connect_init`) and connects to the CT node with `ei_connect`, copying the pid from `ei_self(&ec)` where `ei_global_register` needs it.
- Covered behaviour: register then unregister succeed, `whereis` returns the pid registered by the Erlang side, `names` contains the name while registered and not after unregistering; `cmd_ei_global_names` also exercises the single-`free` ownership contract.
- Not covered: failure paths (`EBADMSG`/`EMSGSIZE`, unknown name, oversized name list), `erl_errno` values, `whereis` node-name out-parameter correctness, and multi-node conflicts.

## See Also
- `../connect/send_reg.c` — `ei_send_reg_encoded*` used for every request.
- `../connect/eirecv.c` — `ei_recv_internal` (the `msgtype - 10` normalization and `staticbufp` semantics).
- `../connect/ei_connect.c` — `ei_rpc_to`/`ei_rpc_from`/`ei_rpc`, which build the same `rex` envelope with a dynamic (`ei_x_buff`) buffer and do not handle link/monitor traffic.
- `../../include/ei_global.h`, `../../include/ei.h` — public prototypes; `../../doc/references/ei_global.md`, `../../doc/guides/ei_users_guide.md` ("Using Global Names") — reference and guide text kept in sync with these functions.
- `../../../../lib/kernel/src/global.erl` — the Erlang side (`registered_names/0`, `whereis_name/1`, `register_name_external/3`, `unregister_name_external/1`, `resolve_it/4`).
