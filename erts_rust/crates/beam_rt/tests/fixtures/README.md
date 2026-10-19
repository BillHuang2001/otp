# `beam_rt` test fixtures

`mailbox.beam`, `sched.beam`, `port_recv.beam`, `raise_structured.beam`,
`function_clause_fixture.beam`, `recv_after.beam`, `dist_echo.beam`, `port_exit.beam` and
`pingpong.beam` are committed oracle fixtures: real `.beam` images produced by the **system
Erlang** toolchain (OTP 29 / `erts-17.1`), which is also the differential oracle.
* `pingpong.beam` is the **two-process ping-pong** image for the `run` route's sleep
  accounting (`tests/run_route_sleep.rs`): `go/1` spawns one fun-bodied worker and runs `N`
  synchronous round trips (`Worker ! {self(), N}` / a bound-variable `receive {_, N}`, with
  an `after 5000` guard that raises `{pingpong_timeout, N}` instead of hanging). Every
  iteration parks the parent on an empty mailbox until the reply arrives, so the run queue
  empties and the pass boundary is reached repeatedly — the shape whose per-pass poll sleep
  the syscall assertion rules out.* `mailbox.beam` is the receive/message end-to-end image — `tests/mailbox.rs` (the
  runtime slice) and the receive-seam unit tests in `src/context.rs` run it through the
  real `RuntimeContext`.
* `sched.beam` is the **sender** image for the cross-process scheduler test
  (`tests/scheduler.rs`): it sends the non-matching head `42` and then `{bar, N + 7}`,
  the message `recv:scan/0` (from `../beam_engine/tests/fixtures/recv.beam`) matches —
  something `recv:bomb/1` (a constant `42` only) cannot do.
* `port_recv.beam` is the **port-wake** image (`tests/port_wake.rs`): `recv/0` opens a
  spawn-driver port (`{spawn, "echo hi"}`) and then blocks in a compiled `receive` on its
  stream, so the only thing that can wake it is a `{Port, {data, Data}}` message the
  runtime's poller delivers into its mailbox — never a `send` from another process.
* `raise_structured.beam` is the **structured-raise** image (`tests/structured_errors.rs`):
  `badkey/0` and `badmap/0` raise the structured reasons `{badkey, nokey}` and
  `{badmap, not_a_map}` from compiled code, so a spawned process that raises one exposes the
  **real term** through `Runtime::process_exit`.
* `function_clause_fixture.beam` is the **failed-head-clause** image
  (`tests/function_clause.rs`): `f/1` has the single clause `f(a) -> ok.`, so `f(b)` matches
  no clause and jumps to the function's `func_info` label. Its own atom table holds only
  `function_clause_fixture`, `f`, `a`, `ok`, `module_info`, `erlang` and `get_module_info`
  (`beam_lib:chunks/2`), so the reason atom `function_clause` can only come from the
  runtime's boot seed — which is exactly the property the test pins.
* `recv_after.beam` is the **finite `receive … after T`** image (the receive-deadline tests in
  `src/runtime.rs` and `src/runner/`): `after20/0` and `after2000/0` park on an empty mailbox
  and time out, `afterzero/0` (`after 0`) never suspends, `self_send/0` matches the message
  already queued and never arms a deadline, and `wait_forever/0` (`after infinity`) parks. It
  pins the five opcode shapes the runtime deadline path is read from (`wait_timeout` +
  `timeout`, the compiler-folded `after 0`, a matched queued message, a message that **cancels**
  an armed deadline, and a bare `wait`).
* `dist_echo.beam` is the **inbound-distribution** image (`tests/live_inbound.rs`): `wait/0`
  registers the name `echo` and blocks in a compiled `receive`, so a real system `erl` peer
  that resolves this runtime's `-sname` listener through `epmd` and sends
  `{echo, Node} ! {hello, node()}` makes the message arrive as a `DOP_REG_SEND` in the
  running process's mailbox — the value `wait/0` returns *is* the delivered payload.
* `port_exit.beam` is the **port `'EXIT'`/`'DOWN'` on close** image (`tests/port_exit.rs`, contract
  A.4): `trap_owner/0`, `trap_owner_mon/0`, `notrap_owner_mon/0` and `watcher_only/0` each close a
  real `{spawn, "true"}` port and return the messages that arrived, **normalized** (`#Port<…>` /
  `#Ref<…>` differ between two runtimes, so each is replaced by `is_port/1` / `is_reference/1`), and
  `connected/0` does the same for a `erlang:port_connect/2`ed second process while also measuring
  the port's link count before and after the connect (`{true,1,2,[{exit,true,normal}]}`). It pins
  both delivery routes: the `-eval` `receive` route and the scheduler `run` route.
* `bifent.beam` / `tmr.beam` are the **first-entry BIF dispatch** images
  (`tests/bif_entry_spawn.rs`): `bifent:fire/0` is `spawn(erlang, send, [self(), done])` followed
  by a bounded receive (`bifent:queued/0` is the regression guard — `self() ! done` then the same
  receive), and `tmr:fire/0` arms `timer:apply_after(50, erlang, send, [self(), done])`, whose
  `timer_server` runs the MFA as a **spawned process** (`spawn(erlang, send, …)`). A spawned
  entry naming a host-served MFA must be dispatched to the BIF; entering `erlang.beam`'s stub
  instead raises the bare atom `undefined`, so both answer `timeout` where the oracle answers
  `done`.

## The scheduled-route `epp` pair (`epp_forms.erl` / `epp_input.erl`, compiled at test time)

`tests/compile_file.rs::a_scheduled_process_parses_a_source_file_like_the_oracle` drives the
`epp:parse_file/3` read path from a **scheduled `run`** process, which is the route on which a
delivery-driven collection of the parked receiver is declined by the runner's park-window
relocation exemption instead of moving the receiver's live scan options:

* `epp_forms.erl` is the **driver**: `go/1` takes the input source file as a char list, runs
  `epp:parse_file(File, [], [])` and answers `{length(Forms), [element(1, F) || F <- Forms]}` — a
  pure `~w`-printing value, so `beam_rt run …` stdout and the oracle's
  `io:format("~w~n", [epp_forms:go(File)])` are directly comparable.
* `epp_input.erl` is the **input**: `-module(epp_input). -export([f/0]). f() -> 1.` — legacy
  syntax only (no maps, no binaries), so the scan options stay on the plain cons cell.
* The driver is compiled with the system `erlc` into a scratch directory (both files'
  **contents** are `include_str!`-embedded by the suite, so nothing is read relative to the
  test's CWD; the scratch dir is the only `-pa` given to either runtime).

Oracle (the same artifacts under the real system `erl`): `epp` prepends its own
`{attribute, _, file, _}` form, so the answer is the three source forms plus that attribute and
the trailing `eof`:

```sh
erl -noshell -pa "$DIR" -eval 'io:format("~w~n",[epp_forms:go("'$DIR'/epp_input.erl")]), halt(0).'
# {5,[attribute,attribute,attribute,function,eof]}
```

## The `{active, …}` socket-input fixture (`inet_active.erl`, compiled at test time)
`tests/inet_active.rs` pins the driver's **active-input channel** end to end (contract A.4): a
socket in `{active, true | once | N}` really delivers `{tcp, S, Data}` / `{udp, S, IP, Port, Data}`
and `{tcp_passive, S}` / `{udp_passive, S}` to its owner's mailbox, and a compiled `receive` gets
it. The single committed source `inet_active.erl` (module `inet_active`, exports `t1/0 … t7/0`) is
`include_str!`-embedded and compiled by the suite into a **per-case scratch directory** with the
system `erlc`, and only that directory is ever named on a `-pa` (`tests/fixtures/` holds a
`lists.beam` that would shadow stdlib).

### The transport is the driver itself — exactly what `prim_inet` does
Every case opens its socket with `erlang:open_port({spawn_driver, "tcp_inet" | "udp_inet"},
[binary])` and drives it with **numeric** `erlang:port_control/3` requests, each offset by
`ERTS_INET_DRV_CONTROL_MAGIC_NUMBER` (`16#03f1a300`): `INET_REQ_OPEN` 1, `INET_REQ_CONNECT` 3,
`INET_REQ_BIND` 6, `INET_REQ_SETOPTS` 7, `INET_REQ_ACCEPT` 26, `INET_REQ_LISTEN` 27. No
`gen_tcp`/`gen_udp`/`prim_inet` is involved, so the identical source runs under both runtimes with
nothing library-level that could differ. The helpers mirror the driver's own encodings:
* an option record is `[code, value_be32]` (`{mode,list}` = `[24,0,0,0,0]`, `{mode,binary}` =
  `[24,0,0,0,1]`, `{packet,2}` = `[23,0,0,0,2]`), with a trailing **16-bit counter only for
  `{active, N}`** (`{active,true}` = `[22,0,0,0,1]`, `{active,once}` = `[22,0,0,0,2]`,
  `{active,2}` = `[22,0,0,0,3,0,2]` — `prim_inet:enc_opt_val/2`'s exact split);
* `bind` answers `[1] ++ <<Port:16/big>>` (the kernel-assigned port, bound on `127.0.0.1` with
  port `0` — the address record `[4]` is `INET_AF_LOOPBACK`);
* `connect` normally completes inside the call (a bare `[1]`) and otherwise answers
  `[1] ++ <<Id:16/big>>` plus `{inet_async, P, Id, ok}`; `accept` is always the async form, ending
  in `{inet_async, L, Id, {ok, Port2}}`;
* a send is `prim_inet:send/3`'s wire shape — `[<<Size:16, TagBin/binary>>, Data]` where `TagBin` is
  `term_to_binary(Ref, [local])`, the driver echoing the decoded tag as the fourth element of
  `{inet_reply, S, ok, Tag}`; a datagram's destination record is
  `[INET_AF_INET, Port:16, A, B, C, D, 0:32]` (a zero ancillary-data length).

### Every case is self-contained in one VM
The fixture binds a listener, reads the assigned port out of the `bind` reply and opens its **own
peer** socket in the same run — so no external peer is needed and the bytes the owner receives are
bytes a real loopback socket carried. The last line of every case is `ok` (the `run` action's
printed result on this runtime's side, the fixture's own `~w`-printed return value on the oracle's).

### What each case pins, and the oracle's rendering of it (OTP 29.1.1 / erts-17.1)
`'$port'` stands for the port term (a per-runtime table slot) and `'$source'` for a UDP source port
(a per-run value); **everything else is compared byte for byte**.

| case | configuration | pinned shape | oracle output |
|---|---|---|---|
| `t1` | `{active,true}` `{mode,binary}`, peer writes `<<"hello">>` then `<<"again">>` | every write becomes a `{tcp, S, Data}` | `t1_first={tcp,'$port',<<104,101,108,108,111>>}`, `t1_socket_is_port=true`, `t1_second={tcp,'$port',<<97,103,97,105,110>>}` |
| `t2` | `{active,2}` | two deliveries, exactly one `{tcp_passive, S}`, silence, then a re-armed delivery | `t2_one={tcp,'$port',<<111,110,101>>}`, `t2_two={tcp,'$port',<<116,119,111>>}`, `t2_passive={tcp_passive,'$port'}`, `t2_silent=timeout`, `t2_rearmed={tcp,'$port',<<116,104,114,101,101>>}` |
| `t3` | `{active,once}` | one delivery, silence while the grant is spent, a re-arm, one more delivery | `t3_first={tcp,'$port',<<104,101,108,108,111>>}`, `t3_silent=timeout`, `t3_second={tcp,'$port',<<98,98,98,98>>}`, `t3_silent_again=timeout` |
| `t4` | `{mode,binary}` vs `{mode,list}`, same `<<"hello">>` | binary vs charlist for one payload | `t4_binary={tcp,'$port',<<104,101,108,108,111>>}`, `t4_list={tcp,'$port',[104,101,108,108,111]}`, `t4_list_is_hello=true` |
| `t5` | `{packet,2}`, peer writes `<<5:16,"hello">>` | the framing header is stripped before delivery | `t5_stripped={tcp,'$port',<<104,101,108,108,111>>}`, `t5_body_is_hello=true` |
| `t6` | UDP `{active,true}`, one datagram | `{udp, S, {127,0,0,1}, SourcePort, Data}` with the peer's real bound port | `t6_datagram={udp,'$port',{127,0,0,1},'$source',<<104,105>>}`, `t6_source_is_peer=true`, `t6_source_is_real=true`, `t6_address={127,0,0,1}` |
| `t7` | listener `{packet,2}` + `{mode,list}` + `{active,2}`, one accepted connection | the listener's options reach the accepted port (charlist, header stripped), and its own re-armed `{active,2}` reports `{tcp_passive, S}` | `t7_accepted_is_port=true`, `t7_first={tcp,'$port',[104,101,108,108,111]}`, `t7_second={tcp,'$port',[119,111,114,108,100]}`, `t7_passive={tcp_passive,'$port'}` |

The two content rules the table encodes: `{mode,binary}` must be **asked for** (the driver's
effective default for a `[binary]` port is a *list* — measured, which is why `prim_inet:open/5`'s
`[binary]` alone is not enough), and `{active, …}` is the one option block an accepted socket does
**not** inherit (the C copies everything else), so `t7` re-arms the accepted port itself.

### The one measured difference the fixture avoids on purpose
This runtime harvests a passive window's bytes out of the socket when it emits `{tcp_passive, S}`
and never delivers them, while the oracle keeps them queued and hands them over after the next
re-arm (a peer write made while the grant is spent shows up as `<<"threethree">>` there and
`<<"three">>` here — the note recorded in `src/port/active_input_e2e_tests.rs`). `t2` therefore
writes **nothing** during its passive window and `t3` never reads back its spent-grant write, so
every line agrees; making the window byte-faithful is the port provider's work, not the fixture's.

### Skip-loudly rule
Each test skips **loudly** (`beam_rt inet_active SKIPPED` plus a return, never a silent pass) when
`erlc` or `erl` is missing — a **toolchain** absence the host may legitimately have. A fixture that
fails to compile, a non-zero exit, a run that outlives the harness's 30 s probe bound
(`support::PROBE_TIMEOUT`) or any line that differs from the oracle's is a **failure**: the fixture
is part of this repository.

## Driver fixtures (never committed — compiled at test time)

`event_driver.c` and `event_driver.erl` are the **driver event/timer** pair for
`tests/driver.rs::a_driver_select_and_a_driver_timer_both_reach_the_owner` (contract A.4):

* `event_driver.c` is a real driver shared object: `start` arms `driver_select` on the read end of
  an internal `pipe(2)` and a `driver_set_timer` deadline, `output` writes one byte to the write end
  (making the selected descriptor readable), `ready_input` drains that byte and answers
  `driver_output("R")`, and `timeout` answers `driver_output("T")`. Its entry's `driver_name` is
  `event`, so the compiled object's stem must be `event` (the name `{spawn_driver, Path}` derives
  from the path — see `beam_rt::port::driver_lookup`).
* `event_driver.erl` (`recv/0`) opens that port with `{spawn_driver, "{path}"}`, `port_command`s it
  one byte, and collects the two `{Port, {data, Data}}` messages — each `receive` bounded by
  `after 5000`, so a missing byte shows up as the atom `timeout` in its position.

The C is compiled in one of four **modes** (`-DEVENT_DRIVER_MODE=N`, defaulting `EVENT_DRIVER_NAME`),
so one fixture drives every A.4 output path; each mode's `.so` is opened under its own `driver_name`:

| Mode | `driver_name` | Entry shape | `event_driver.erl` | Pins |
|---|---|---|---|---|
| 0 (default) | `event` | `output` + `driver_select` + `driver_set_timer` (the event/timer round) | `recv/0` | `a_driver_select_and_a_driver_timer_both_reach_the_owner` |
| 1 | `eventv` | **`outputv` only** (`.output = NULL`) | `recv_v/0` | `an_outputv_only_entry_receives_port_command` |
| 2 | `eventt` | `output` emits a term via `erl_drv_output_term` (an `ERL_DRV_EXT2TERM` operand) to the owner | `recv_term/0` | `a_driver_output_term_reaches_the_owner_as_a_bare_term` |
| 3 | `eventp` | `output` emits a term via `erl_drv_send_term` to the named pid | `recv_term/0` | `a_driver_send_term_reaches_the_connected_process` |

`recv_v/0` `port_command`s one byte and returns the bytes the `outputv` vector produced; `recv_term/0`
returns the bare term the driver delivered (the port's owner and the explicit pid are the same process in
the test, so the two destination paths are observationally identical). Modes 2/3 emit an **atom-free**
image (`<<131,97,9>>` = the integer `9`), so delivery is asserted without depending on a driver-interned
atom landing in the live table — see the atom-commit gap in `src/port/CONTEXT.md`.

Neither is committed in compiled form, because **both** embed the scratch directory the test builds
in: the test replaces the `.erl`'s `{path}` placeholder with the compiled `.so`'s path (known only
then) and compiles the result. The C side is compiled with `$CC` (or `cc`) against the host
`erl_driver.h`; the Erlang side with the system `erlc`. Both halves are skipped **loudly** (and the
test returns) when a tool is missing, so a tree without the host toolchain never silently passes.

The Erlang half must never be *committed* as a `.beam` with an absolute path baked in; the test-time
artifact lives under `$TMPDIR` and can be checked with:

```sh
strings "$TMPDIR"/beam_rt_driver_events_*/event_driver.beam | rg 'workers|/home/'
```

(no output = nothing about the checkout leaked). The `.so` and `.beam` the test compiles are removed
again by the test itself (`Fixture::cleanup`).

Oracle values (the fixture is valid against the real system, verified with the same `.so`):

```sh
INC=$(erl -noshell -eval 'io:format("~s",[code:lib_dir(erts,include)])' -s init stop)
cc -shared -fPIC -I"$INC" -o "$TMPDIR/event.so" event_driver.c
erl -noshell -eval 'ok = erl_ddll:load_driver(os:getenv("TMPDIR"), "event"),
    P = open_port({spawn_driver, "event"}, [binary]),
    true = port_command(P, <<"go">>),
    A = receive {P,{data,D1}} -> D1 after 5000 -> timeout end,
    B = receive {P,{data,D2}} -> D2 after 5000 -> timeout end,
    io:format("~p ~p~n",[A,B]), halt(0).'
# <<"R">> <<"T">>
```

so `ready_input`'s byte is `<<"R">>`, `timeout`'s is `<<"T">>`, and the runtime's own answer is
asserted against exactly that pair.

## Execution-position fixtures (`recv.erl` / `pos_self.erl`, never committed — compiled at test time)

`tests/position.rs` (contract A.8, `erlang:process_info/1,2`'s execution-position items: the
in-machine BIF door) drives two shapes through these sources. Both are compiled at test time by the
system `erlc` **with the scratch directory as cwd and the bare file names as arguments**, so the
name recorded in each `.beam`'s `Line` chunk is exactly `"recv.erl"` / `"pos_self.erl"` — which is
what the oracle reports in `current_location`/`current_stacktrace` (an absolute or repo-relative
argument would record *that* string instead and the two would disagree):

* `recv.erl` is the **probed-process** fixture. `wait/0`'s clause head sits on **line 12** so a
  process parked in its `receive` reports the oracle's `{line,12}`; `probe_b/1` reads the four
  items of a parked process (`current_function`, `initial_call`, `current_location`,
  `current_stacktrace`) in that order; `main/0` is `probe_b(self())` — the **self** query, asking
  for the position of the process *running it* (the shape a scheduler pass cannot answer from its
  runner state, because the executing process's machine is taken out of it for the whole slice —
  hence the in-machine door).
* `pos_self.erl` is the **self-query driver**: `go/1` runs `recv:main/0` inside the *scheduled*
  process and sends `{result, R}` to the parent it is given; `sink/0` is the built runtime's parent
  — it receives that report and returns it, so the test reads it back through
  `beam_rt::Runtime::process_result`. (A spawned export process's return value is unobservable in
  Erlang, so message passing is the only way to observe it; the oracle's parent is the `erl`
  probe's own root process, which receives the same message.)

Oracle values (verified against `erl -noshell`, erts-17.1 / OTP 29.1.1):

```sh
D=$(mktemp -d); cp recv.erl pos_self.erl "$D"; (cd "$D" && erlc recv.erl pos_self.erl)

# a process parked in recv:wait/0, probed from a second process
erl -noshell -pa "$D" -eval 'P = spawn(recv, wait, []), timer:sleep(50),
    io:format("~p~n", [process_info(P, [current_function, initial_call,
        current_location, current_stacktrace])]), halt(0).'
# [{current_function,{recv,wait,0}},
#  {initial_call,{recv,wait,0}},
#  {current_location,{recv,wait,0,[{file,"recv.erl"},{line,12}]}},
#  {current_stacktrace,[{recv,wait,0,[{file,"recv.erl"},{line,12}]}]}]

# recv:main/0's **self** query, from compiled code
erl -noshell -pa "$D" -eval 'Self = self(), _ = spawn(pos_self, go, [Self]),
    R = receive {result, X} -> X end, io:format("~p~n", [R]), halt(0).'
# [{current_function,{recv,probe_b,1}},
#  {initial_call,{pos_self,go,1}},
#  {current_location,{recv,probe_b,1,[{file,"recv.erl"},{line,18}]}},
#  {current_stacktrace,[{recv,probe_b,1,[{file,"recv.erl"},{line,18}]},
#                       {pos_self,go,1,[{file,"pos_self.erl"},{line,24}]}]}]
```

(The `recv:main/0` frame is elided by its tail call into `probe_b/1`, exactly as the oracle elides
it; the test compares the frame **count** and the **head** frame of the stacktrace, not the second
frame.)

Documented divergences of the self-query, recorded in `tests/position.rs` and `M1_GAPS.md`: the
machine's `initial_call` is the process's **entry MFA**, where the oracle answers
`{erlang,apply,2}` for some spawn shapes (the scheduled-export shape above agrees); and a **nested**
fun-applied run started inside the published window reads the **outer** machine's position for the
same pid, because the published slot is runtime-wide rather than per-`RuntimeContext`.

* `rescan.beam` is the **mailbox scan-position / re-scan** image (`tests/mailbox_rescan.rs`):
  `parked/0` is `spawn(fun() -> Self ! 42 end)` followed by a plain `receive` (the receiver
  parks on an **empty** mailbox and takes the next message), `marked/0` is the same shape with
  a `recv_marker_reserve`/`_bind`/`_use` prologue (it matches on a reference created just
  before the receive), `junk_then_ref/0` queues a non-matching message **before** the
  reference so the marker anchor sits behind it, `two_receives/0` shares one reference across
  two receives (so the second `recv_marker_use` runs against a queue the first receive shrank),
  `timed/0` (`after 20`, nothing matches) fires the timeout clause, and `timedhit/0` (same
  deadline, a message arrives first) must **cancel** it.

## Regeneration

`mailbox`, `sched`, `port_recv`, `raise_structured` and `function_clause_fixture` are compiled
with `+deterministic`, which strips the embedded source path from the `CInf` (compile info) and
`Dbgi` (debug info) chunks — the committed `.beam` is therefore **path-free**. Compile from a
neutral scratch dir under `$TMPDIR` so nothing about the local checkout is recorded:

```sh
# mailbox.beam (740 bytes)
SCRATCH="$TMPDIR/beam_rt_mailbox_fix"
mkdir -p "$SCRATCH/mailboxgen"
cp mailbox.erl "$SCRATCH/mailboxgen/"
( cd "$SCRATCH/mailboxgen" && erlc +deterministic mailbox.erl )
cp "$SCRATCH/mailboxgen/mailbox.beam" mailbox.beam

# sched.beam (592 bytes)
SCRATCH="$TMPDIR/beam_rt_sched_fix"
mkdir -p "$SCRATCH/schedgen"
cp sched.erl "$SCRATCH/schedgen/"
( cd "$SCRATCH/schedgen" && erlc +deterministic sched.erl )
cp "$SCRATCH/schedgen/sched.beam" sched.beam

# port_recv.beam (680 bytes)
SCRATCH="$TMPDIR/beam_rt_port_recv_fix"
mkdir -p "$SCRATCH/port_recvgen"
cp port_recv.erl "$SCRATCH/port_recvgen/"
( cd "$SCRATCH/port_recvgen" && erlc +deterministic port_recv.erl )
cp "$SCRATCH/port_recvgen/port_recv.beam" port_recv.beam

# raise_structured.beam (644 bytes)
SCRATCH="$TMPDIR/beam_rt_raise_fix"
mkdir -p "$SCRATCH/raisegen"
cp raise_structured.erl "$SCRATCH/raisegen/"
( cd "$SCRATCH/raisegen" && erlc +deterministic raise_structured.erl )
cp "$SCRATCH/raisegen/raise_structured.beam" raise_structured.beam

# function_clause_fixture.beam (548 bytes)
SCRATCH="$TMPDIR/beam_rt_function_clause_fix"
mkdir -p "$SCRATCH/fcgen"
cp function_clause_fixture.erl "$SCRATCH/fcgen/"
( cd "$SCRATCH/fcgen" && erlc +deterministic function_clause_fixture.erl )
cp "$SCRATCH/fcgen/function_clause_fixture.beam" function_clause_fixture.beam

# rescan.beam (1660 bytes) — same deterministic scratch-dir recipe
SCRATCH="$TMPDIR/beam_rt_rescan_fix"
rm -rf "$SCRATCH"; mkdir -p "$SCRATCH/gen"
cp rescan.erl "$SCRATCH/gen/"
( cd "$SCRATCH/gen" && erlc +deterministic rescan.erl )
cp "$SCRATCH/gen/rescan.beam" rescan.beam
# recv_after.beam (824 bytes) — same deterministic scratch-dir recipe
SCRATCH="$TMPDIR/beam_rt_recv_after_fix"
rm -rf "$SCRATCH"; mkdir -p "$SCRATCH/gen"
cp recv_after.erl "$SCRATCH/gen/"
( cd "$SCRATCH/gen" && erlc +deterministic recv_after.erl )
cp "$SCRATCH/gen/recv_after.beam" recv_after.beam

# dist_echo.beam (632 bytes) — same deterministic scratch-dir recipe
SCRATCH="$TMPDIR/beam_rt_dist_echo_fix"
rm -rf "$SCRATCH"; mkdir -p "$SCRATCH/gen"
cp dist_echo.erl "$SCRATCH/gen/"
( cd "$SCRATCH/gen" && erlc +deterministic dist_echo.erl )
cp "$SCRATCH/gen/dist_echo.beam" dist_echo.beam

# remote_send.beam (984 bytes) — same deterministic scratch-dir recipe
SCRATCH="$TMPDIR/beam_rt_remote_send_fix"
rm -rf "$SCRATCH"; mkdir -p "$SCRATCH/gen"
cp remote_send.erl "$SCRATCH/gen/"
( cd "$SCRATCH/gen" && erlc +deterministic remote_send.erl )
cp "$SCRATCH/gen/remote_send.beam" remote_send.beam

# bifent.beam (720 bytes) + tmr.beam (660 bytes) — same deterministic scratch-dir recipe
SCRATCH="$TMPDIR/beam_rt_bif_entry_fix"
rm -rf "$SCRATCH"; mkdir -p "$SCRATCH/gen"
cp bifent.erl tmr.erl "$SCRATCH/gen/"
( cd "$SCRATCH/gen" && erlc +deterministic bifent.erl tmr.erl )
cp "$SCRATCH/gen/bifent.beam" "$SCRATCH/gen/tmr.beam" .
# pingpong.beam (1004 bytes) — same deterministic scratch-dir recipe
SCRATCH="$TMPDIR/beam_rt_pingpong_fix"
rm -rf "$SCRATCH"; mkdir -p "$SCRATCH/gen"
cp pingpong.erl "$SCRATCH/gen/"
( cd "$SCRATCH/gen" && erlc +deterministic pingpong.erl )
cp "$SCRATCH/gen/pingpong.beam" pingpong.beam

# port_exit.beam (2512 bytes) — same deterministic scratch-dir recipe
SCRATCH="$TMPDIR/beam_rt_port_exit_fix"
rm -rf "$SCRATCH"; mkdir -p "$SCRATCH/gen"
cp port_exit.erl "$SCRATCH/gen/"
( cd "$SCRATCH/gen" && erlc +deterministic port_exit.erl )
cp "$SCRATCH/gen/port_exit.beam" port_exit.beam
```
Every fixture is built with `+deterministic` in a `$TMPDIR` scratch dir, so no
absolute checkout path is embedded in any committed `.beam`.

The repo-root `.gitignore` ignores `*.beam` globally, so a (re)built fixture must be
**force-added**: `git add -f sched.beam`.

`sched.erl` (the input, also committed):

```erlang
-module(sched).
-export([send2/2]).

send2(Pid, N) ->
    Pid ! 42,
    Pid ! {bar, N + 7},
    ok.
```

`port_recv.erl` (the input, also committed):

```erlang
-module(port_recv).
-export([recv/0]).

recv() ->
    Port = open_port({spawn, "echo hi"}, [binary]),
    receive
        {Port, {data, Data}} -> Data
    end.
```

`raise_structured.erl` (the input, also committed):

```erlang
-module(raise_structured).
-export([badkey/0, badmap/0]).

badkey() -> maps:get(nokey, #{}).
badmap() -> map_get(a, not_a_map).
```

`recv_after.erl` (the input, also committed):

```erlang
-module(recv_after).
-export([after20/0, after2000/0, afterzero/0, self_send/0, wait_forever/0]).

after20()     -> receive X -> X after 20     -> timeout end.
after2000()   -> receive X -> X after 2000   -> timeout end.
afterzero()   -> receive X -> X after 0      -> timeout end.
self_send()   -> self() ! hello,
                 receive X -> X after 2000   -> timeout end.
wait_forever()-> receive X -> X after infinity -> timeout end.
```

`dist_echo.erl` (the input, also committed):

```erlang
-module(dist_echo).
-export([wait/0]).
wait() -> register(echo, self()), loop().
loop() -> receive M -> M after 20 -> loop() end.
```

The `after 20` is **load-bearing** (the same rule `recv_after.beam` pins and
`tests/dist.rs`'s keep-alive fixture relies on): a bare `receive` with no deadline leaves the
scheduler quiescent (an empty run queue, nothing to poll, no pending timer), so
`Runtime::run_scheduled` would stop with its honest `blocked` diagnostic *before* the peer's
message can arrive. The per-iteration deadline keeps a pass stepping, so the distribution half
(`pump_dist`) keeps draining the accepted link until the delivery arrives.

`bifent.erl` / `tmr.erl` (the inputs, also committed):

```erlang
-module(bifent).
-export([fire/0, queued/0]).

fire()   -> spawn(erlang, send, [self(), done]),
            receive M -> M after 1500 -> timeout end.
queued() -> self() ! done,
            receive M -> M after 1500 -> timeout end.
```

```erlang
-module(tmr).
-export([fire/0]).

fire() -> timer:apply_after(50, erlang, send, [self(), done]),
          receive M -> M after 1500 -> timeout end.
```
Any byte change in a fixture invalidates the expected values in the tests that load it;
re-derive them from the system `erl`, do **not** guess.

`pingpong.erl` (the input, also committed):

```erlang
-module(pingpong).
-export([go/1]).

go(N) when is_integer(N), N >= 0 ->
    Worker = spawn(fun() -> worker() end),
    loop(N, Worker),
    Worker ! stop,
    N.

loop(0, _Worker) ->
    ok;
loop(N, Worker) ->
    Worker ! {self(), N},
    case receive {_, N} -> ok after 5000 -> timeout end of
        ok -> loop(N - 1, Worker);
        timeout -> erlang:error({pingpong_timeout, N})
    end.

worker() ->
    receive
        {From, N} ->
            From ! {self(), N},
            worker();
        stop ->
            ok
    end.
```
Oracle: `pingpong:go(N)` answers `N` for every `N` (verified against `erl -noshell`).

## GC fixtures (`gc_loop` · `gc_ints` · `gc_floats` · `heap_growth` · `gc_payload` · `pt_gc` · `churn` · `vp`, committed)

These eight are committed oracle fixtures for the process-heap collector
(`tests/gc_collect.rs`, `tests/heap_growth.rs`, `tests/gc_strategy.rs`,
`tests/gc_immix_payload.rs`, `tests/persistent_term.rs`, `tests/rss_bound.rs` and
`tests/foreign_root_loop.rs`):
* `gc_loop.beam` — `loop/1` tail-recursively conses every iteration (`build/1`), so a
  scheduled process really allocates; the acceptance evidence that the collector runs at the
  engine's heap-admission point.
* `gc_ints.beam` — `l1/1` accumulates `+`/`rem` on small integers only: it allocates nothing,
  pinning that a non-allocating loop's young heap never moves.
* `gc_floats.beam` — `l1/1` accumulates `sqrt/1` in one IEEE-754 double: a module with a
  float-family instruction is collected over safely (the engine reaches only the full door,
  which publishes the machine's whole root set — the FR file included — so a live float
  register is relocated like any other root), and its result must stay exact.
* `heap_growth.beam` — `sprawl/1` builds a 400-element tuple of a *runtime* value in one block
  (the compiler emits a single `test_heap 1 401`, far above the 233-word fresh nursery) and
  `run/1` repeats it, discarding each tuple, so a runtime that cannot grow the nursery must
  fall back to the pointer-stable arena on every iteration. Pins the growable-heap seam
  (`Process::ensure_capacity` behind `ensure_heap`).
* `gc_payload.beam` — `run(N)` builds a **long-lived payload** of `payload_length()` (64)
  tuples `{K, K + 1, [K, K + 2, K + 3]}` (~768 words), then churns `N` rounds of short-lived
  garbage (~576 words per round, above the fresh nursery), reading `erlang:length(Payload)`
  *after* each round's allocation so the payload stays a live root across every heap-admission
  point, and finally **returns the payload** (a structurally checkable term: element `i` is
  keyed `64 - i`). The witness that a collection on the mature strategy did not relocate the
  heap out from under a live term.
* `pt_gc.beam` — `run(N)` builds the **same 64-element payload**, stores it in the node-global
  persistent-term table (`persistent_term:put(pt_gc_key, Payload)`, so the store's raw word
  names the process's own **young heap**), churns `N` rounds of the same short-lived garbage and
  finally returns `persistent_term:get(pt_gc_key)` — the value read back **out of** the store.
  The witness for `tests/persistent_term.rs::a_stored_value_survives_a_process_heap_collection`:
  a store that kept a raw young-heap word without stabilizing it hands back a dangling/recycled
  term (observed: a 2-element list instead of 64 elements).
* `churn.beam` — `run(N)` is the allocate-and-abandon churn loop mirrored from the `gc_bound`
  benchmark workload's embedded source (`../../tools/bench/src/workloads/gc_bound.rs`): every
  iteration builds a 256-element list (interpreted recursion), a 256-byte binary and a
  256-element tuple, discards all three, checks the two O(1)-checkable ones and adds 1, so
  `run(N)` returns exactly `N`. The **RSS-bound** witness for `tests/rss_bound.rs`: the peak
  `VmHWM` must be flat across a 10× `N` step (the pre-fix curve in `tools/bench/RESULTS.md`
  §6.7 grew roughly linearly), and the deterministic companion asserts
  `Runtime::gc_stats().collections > 0` for a scheduled run.

* `vp.beam` — the **copy-out fill-law** witness (`tests/foreign_root_loop.rs`): `hold/1`
  keeps the 3-tuple `erlang:statistics(garbage_collection)` returns — a registered BIF's
  result, which the copy-out puts on the process's own **scanned** young heap, so a collection
  relocates it like any other heap object — as its own accumulator for a whole loop, and
  `control/1` is the same loop reading only `element(1, ...)` (a small integer, so no container
  is ever a root). Both throw away a fresh runtime-valued `junk/1` term per iteration, which is
  what gives the loop a heap-admission point at all. Heap use is **17 words per iteration** — 13
  for `junk/1` plus 4 for the `{Collections, WordsReclaimed, 0}` tuple (1 header + 3 small
  integers) — so one 233-word nursery space holds **13** iterations and the first collection
  lands at **iteration 8**. The law is the exact floor `(N + 4) / 13` with a measured surplus of
  at most **1** (601 consecutive rows `N = 0 … 600`, one fresh runtime each, plus a wide ladder to
  `N = 1 000 000`); `major` sits in a measured **32…37** minors-per-major band because the held
  tuple is promoted on every minor. `heap_size` stays on the collector's geometry plateau and
  `hold` is otherwise indistinguishable from `control`. `vp.erl`'s comment-only rewrite
  regenerates a **byte-identical** `vp.beam` (md5 `9802066cd17933629b1279484cf7a66c`).
All eight are path-free (`+deterministic`) and regenerate with the same scratch-dir recipe:

```sh
for f in gc_loop gc_ints gc_floats heap_growth gc_payload pt_gc churn vp; do    SCRATCH="$TMPDIR/beam_rt_${f}_fix"
    rm -rf "$SCRATCH"; mkdir -p "$SCRATCH/gen"
    cp "$f.erl" "$SCRATCH/gen/"
    ( cd "$SCRATCH/gen" && erlc +deterministic "$f.erl" )
    cp "$SCRATCH/gen/$f.beam" "$f.beam"
done
```

Each rebuilt `.beam` must be **force-added** (`git add -f gc_loop.beam`), like the images above.

## Oracle values (verified against `erl -noshell`)

| export | oracle expression | value |
|---|---|---|
| `mailbox:selfsend/0` | `mailbox:selfsend()` | `hello` |
| `mailbox:scan/0` | `mailbox:scan()` | `7` |
| `mailbox:wait/0` | `mailbox:wait()` | blocks (empty mailbox) |
| `rescan:parked/0` | `rescan:parked()` | `42` |
| `rescan:marked/0` | `rescan:marked()` | `done` |
| `rescan:junk_then_ref/0` | `rescan:junk_then_ref()` | `7` |
| `rescan:two_receives/0` | `rescan:two_receives()` | `3` |
| `rescan:timed/0` | `rescan:timed()` | `timeout` (after ≈ 20 ms) |
| `rescan:timedhit/0` | `rescan:timedhit()` | `done` (the deadline is cancelled) |
| `sched:send2/2` | a receiver matching `{bar, X}` after `sched:send2(R, 0)` | `7` |
| `sched:send2/2` | `sched:send2(R, N)` | returns `ok` |
| `port_recv:recv/0` | `port_recv:recv()` | `<<"hi\n">>` |
| `raise_structured:badkey/0` | `try raise_structured:badkey() catch C:R -> {C,R} end` | `{error,{badkey,nokey}}` |
| `raise_structured:badmap/0` | `try raise_structured:badmap() catch C:R -> {C,R} end` | `{error,{badmap,not_a_map}}` |
| `function_clause_fixture:f/1` | `function_clause_fixture:f(a)` | `ok` |
| `function_clause_fixture:f/1` | `try function_clause_fixture:f(b) catch C:R -> {C,R} end` | `{error,function_clause}` |
| `churn:run/1` | `churn:run(N)` | `N` (each churn checks `byte_size`/`tuple_size` and adds 1) || `recv_after:after20/0` | `recv_after:after20()` | `timeout` (after ≈ 20 ms) |
| `recv_after:afterzero/0` | `recv_after:afterzero()` | `timeout` (no suspend) |
| `recv_after:self_send/0` | `recv_after:self_send()` | `hello` |
| `recv_after:after2000/0` | a receiver matching nothing, then a `hello` send | `hello` (the send **cancels** the deadline); with no send it is `timeout` |
| `port_exit:trap_owner/0` | `port_exit:trap_owner()` | `[{exit,true,normal}]` |
| `port_exit:trap_owner_mon/0` | `port_exit:trap_owner_mon()` | `[{exit,true,normal},{down,true,port,true,normal}]` (`'EXIT'` first, once each) |
| `port_exit:notrap_owner_mon/0` | `port_exit:notrap_owner_mon()` | `[{down,true,port,true,normal}]` (nothing for `normal`; it survives) |
| `port_exit:watcher_only/0` | `port_exit:watcher_only()` | `[{down,true,port,true,normal}]` |
| `port_exit:connected/0` | `port_exit:connected()` | `{true,1,2,[{exit,true,normal}]}` (links 1 → 2 after `port_connect/2`) |
| `recv_after:wait_forever/0` | `spawn(recv_after, wait_forever, [])` | parks indefinitely (`after infinity` ⇒ bare `wait`) |`selfsend/0` compiles to `bif self` + `send` + `loop_rec` + `remove_message` (with a
`wait` fail branch); `scan/0` additionally emits `loop_rec_end`, because `42` fails the
`{bar, X}` test and the scan must advance to the second message. `wait/0` is a bare
`loop_rec`/`wait` loop and therefore suspends on an empty mailbox. `send2/2` emits two
`send`s and builds `{bar, N + 7}` from the **run-time** argument `N` (an ordinary
`copy_struct` across two processes, not a folded literal).

`recv_after`'s exports pin the finite-`after` shapes: `after20/0`/`after2000/0` compile to a
`wait_timeout` carrying the literal `{integer,T}` followed by `timeout` (the shape
`src/runner/reason.rs::parked_receive_timeout` recovers the deadline from), `afterzero/0` folds the
`after 0` away so an empty mailbox falls straight to `timeout`, `self_send/0` matches the
queued message and never arms a deadline, and `wait_forever/0` (`after infinity`) is a bare
`wait`. Only the two literal-timeout exports are read here; a **variable** `after T` compiles
to the same `wait_timeout` with a Y-register operand, which the runner cannot read today (see
`CONTEXT.md`'s Known Issues). The fixture is driven by the receive-deadline tests in
`src/runtime.rs` (the scheduler path) and `src/runner/` (the parked-timeout bookkeeping).

`marked/0`-style exports that would drive the `recv_marker_*` prologue are deliberately
**not** in either fixture: the prologue is emitted for a receive that matches on a
reference created before the send, which needs `erlang:make_ref/0` — a `beam_bifs` BIF
that Milestone 1 does not register. The marker seam is covered directly instead (the
unit tests in `src/mailbox.rs` and `src/context.rs`).

## Registry-hook fixtures (`reg.c` / `reg_recv.erl`, never committed — compiled at test time)

`tests/driver.rs::driver_lock_driver_and_the_entry_registry_agree_with_the_oracle` compiles a second
driver fixture at test time (contract A.4):

* `reg.c` is a real driver shared object whose `start` callback calls the four port-less registry
  slots — `driver_lock_driver`, `add_driver_entry`, and `remove_driver_entry` twice (the helper entry
  it just added, then the port's **own** entry) — and whose `output` prints the four codes as
  `lock,add,remove,remove_own`. Its entry's `driver_name` is `reg`, so the compiled object's stem is
  `reg` (the name `{spawn_driver, Path}` derives from the path — see `beam_rt::port::driver_lookup`).
* `reg_recv.erl` (`probe/0`) opens that port with `{spawn_driver, "{path}"}`, `port_command`s it one
  byte and returns the bytes its owner receives.

Neither is committed: the C is compiled with `$CC` (or `cc`) against the host `erl_driver.h` and the
Erlang with the system `erlc`, both at test time, and both are skipped **loudly** (the test returns)
when a tool is missing.

Oracle values (the same `.so`, verified against the real system `erl`):

```sh
INC=$(erl -noshell -eval 'io:format("~s",[code:lib_dir(erts,include)])' -s init stop)
cc -shared -fPIC -I"$INC" -o "$TMPDIR/reg.so" reg.c
erl -noshell -eval 'ok = erl_ddll:load_driver(os:getenv("TMPDIR"), "reg"),
    P = open_port({spawn_driver, "reg"}, [binary]),
    true = port_command(P, <<"go">>),
    R = receive {P,{data,D}} -> D after 3000 -> <<"TIMEOUT">> end,
    io:format("~s~n",[R]), halt(0).'
# 0,1,0,-1
```

| hook | Oracle code | meaning |
|---|---|---|
| `driver_lock_driver` | `0` | the port's own driver was `dlopen`ed (`io.c:7337`) |
| `add_driver_entry` + `remove_driver_entry` | `1` | the helper entry was recorded, then found and removed |
| `remove_driver_entry` (again) | `0` | the entry is already gone (no such entry) |
| `remove_driver_entry` (the port's own entry) | `-1` | dynamically loaded, refused (`io.c:7709-7713`) |

## `enif_alloc_env` fixtures (`env_nif.c` / `env_nif.erl` / `env_caller.erl`, never committed)

`tests/nif.rs::enif_alloc_env_returns_a_live_env` compiles a NIF fixture at test time (contract A.3)
that is **separate** from the shared `beam_abi` fixture `test_nif.c` (which this crate may not edit),
because the shared fixture does not exercise the `alloc_env`/`free_env` host hooks:

* `env_nif.c` is a real NIF shared object whose `alloc_env_ok/0` calls `enif_alloc_env()`, frees the
  environment again and returns the atom `live` (a non-NULL env) or `null` (a NULL env).
* `env_nif.erl` (`init/1`, `alloc_env_ok/0`) is the module whose stub the library replaces;
  `env_caller.erl` (`go/1`) reaches `alloc_env_ok/0` through an **external** call (a local call inside
  the NIF's own module would never reach the runtime's dispatch).

Neither is committed; both are compiled at test time and skipped **loudly** when `cc`/`erlc`/the
header is missing. Oracle (the same artifacts under the real system `erl`):

```sh
erl -noshell -pa "$TMPDIR" -eval 'io:format("~w~n",[env_caller:go(os:getenv("TMPDIR") ++ "/env_nif")]), halt(0).'
# live
```

the C's `enif_alloc_env` never returns `NULL` (an allocation failure aborts the emulator), so `live`
is the reference answer.

## Busy-port driver (`busy_driver.c` — never committed, compiled at test time)

The fixture behind `tests/port_busy.rs` (contract A.4): the `erlang:port_command/2,3` behaviour
table against a **real, busy C driver**. It is a plain `driver_*`-only driver (like
`beam_io/tests/fixtures/host_driver.c`), so `dlopen(RTLD_NOW)` resolves every symbol from the
loading process's dynamic symbol table — no driver may reference an unimplemented `driver_*` slot.

### What the callbacks do

* `start` allocates the private state, emits the **start-up data message** with `driver_output`
  (the C `command` tail when the port was opened with one, else the literal `"started"` — so a port
  *always* reports one, and a test can prove `start` ran before it sends anything), and then calls
  `set_busy_port(port, 1)`. **A port therefore starts busy**: every `port_command` is issued against
  a busy port unless the test clears it first.
* `control` serves two verbs, both through `set_busy_port`, and answers the C's `0`:
  * `$B` (`'B'`, 66) sets the busy bit;
  * `$N` (`'N'`, 78) clears it — which on the oracle also **resumes** any caller the C had suspended
    on the busy port (`erts_port_resume_procs`, `io.c:5261-5262`).

  Because it answers `0`, `port_control/3` yields the **empty list** `[]` (never `ok`).
* `output` **echoes** the bytes it is handed back through `driver_output`, so the command really
  reaching the driver (rather than being dropped by a busy/no-`force` arm) is observable at the
  port's owner.
* `stop` frees the private state.

### The compile-time variant (`-D`, one source → two objects)

A driver may only be opened under the `driver_name` its entry declares — and the entry's
`driver_flags` decides whether `[force]` is accepted at all — so the fixture is compiled **twice**
from one source into one scratch directory:

```sh
ERL=$(readlink -f "$(command -v erl)"); INC="$(dirname "$(dirname "$ERL")")/include"
cc -shared -fPIC -I"$INC"                                    -o "$DIR/busy.so"     "$DIR/busy_driver.c"
cc -shared -fPIC -I"$INC" -DBUSY_DRIVER_SOFT=1               -o "$DIR/busysoft.so" "$DIR/busy_driver.c"
```

| macro | `driver_name` | `driver_flags` |
|---|---|---|
| `BUSY_DRIVER_SOFT=0` (the committed default, no `-D` needed) | `"busy"` | `0` |
| `BUSY_DRIVER_SOFT=1` | `"busysoft"` | `ERL_DRV_FLAG_SOFT_BUSY` |

The Erlang half (`busy_probe.erl`) is generated and compiled at test time too (`erlc -o "$DIR"`),
because it interpolates the object's path — which only exists then. It opens the port the way **each
runtime** requires: the oracle first loads the object (`erl_ddll:load_driver/2`, which returns `ok`)
and opens `{spawn_driver, Name}`; this runtime has no such BIF and opens `{spawn_driver, Path}`
(`beam_rt::port::driver_lookup`). A `try`/`catch` around the load doubles as the selector.

> `code:lib_dir(erts,include)` names a path that does **not** exist in the Nix store layout, so the
> include directory is resolved from the **canonicalized `erl` sibling**
> (`…/erts-<vsn>/include`), with a `code:root_dir()` probe and the standard install roots as
> fallbacks. Every `erl` probe is **bounded** (`tests/support/mod.rs`) and carries `-s init stop`.

### Oracle values (`erl -noshell`, erts-17.1 / OTP 29.1.1)

Six cases per variant, each a fresh port (start-up message drained, then the state set with
`port_control`) — `{Tag, {CommandResult, Echo}}`:

```none
%% driver_flags = 0 ("busy")
[{idle_none,{ok,echoed}},{idle_empty,{ok,echoed}},{idle_nosuspend,{ok,echoed}},
 {idle_force,{{other,{error,notsup}},no_data}},{busy_nosuspend,{dropped,no_data}},
 {busy_force,{{other,{error,notsup}},no_data}}]

%% driver_flags = ERL_DRV_FLAG_SOFT_BUSY ("busysoft")
[{idle_none,{ok,echoed}},{idle_empty,{ok,echoed}},{idle_nosuspend,{ok,echoed}},
 {idle_force,{ok,echoed}},{busy_nosuspend,{dropped,no_data}},{busy_force,{ok,echoed}}]
```

(The probe's `try … catch` renders a raise as `{Class, Reason}`, so the printed
`{{other,{error,notsup}},no_data}}` is the echoed `error:notsup`; `true` → `ok`, `false` →
`dropped`, and a missing echo → `no_data`.)

| port state | options | entry | result |
|---|---|---|---|
| idle | none / `[]` / `[nosuspend]` | either | `true`, command delivered (`echoed`) |
| idle | `[force]` | without `ERL_DRV_FLAG_SOFT_BUSY` | `error:notsup`, nothing written |
| idle | `[force]` | with `ERL_DRV_FLAG_SOFT_BUSY` | `true`, command delivered |
| busy | `[nosuspend]` | either | `false` (`dropped`), nothing written |
| busy | `[force]` | without `ERL_DRV_FLAG_SOFT_BUSY` | `error:notsup`, nothing written |
| busy | `[force]` | with `ERL_DRV_FLAG_SOFT_BUSY` | `true`, command delivered |
| busy | none | either | caller **suspends** until the port is idle, then `true`, command delivered |

The last row is the one **documented divergence**: this runtime's BIF seam has no
"suspend the caller and re-run me" shape (`src/native/port.rs`), so a bare `port_command` on a busy
port raises the typed capability gap (exit 3) rather than fabricating a `true`.

> **Never run the busy + no-options case on the oracle without something to clear the busy bit.** It
> blocks for ever (the C's `erts_suspend` + `ERTS_BIF_YIELD3`). `tests/port_busy.rs` asserts the
> runtime's typed gap on that call, and evidences the oracle's suspend **only** through
> `busy_plain_resumed/1`, whose helper clears the bit 200 ms in and yields the oracle's
> `{ok,cleared,echoed}`.

### Skip-loudly rule

The suite needs the C compiler, the host `erl_driver.h`, `erlc` and the oracle `erl`. A missing one
prints a loud `SKIP` banner and returns — it is never a silent pass.

## Driver-failure fixtures (`port_failure_driver.c` + `port_failure.erl` → committed `port_failure.beam`; the `.so` compiled at test time)

The pair behind `tests/port_failure.rs` (contract A.4's `driver_failure*`/`driver_exit` family): a
driver that closes its **own** port from inside its `output` callback, and the consumer that reports
what the port's owner is left with. It is a plain `driver_*`-only object (like
`beam_io/tests/fixtures/host_driver.c` and `busy_driver.c`), so `dlopen(RTLD_NOW)` resolves every
symbol from the loading process's dynamic symbol table — no driver may reference an unimplemented
`driver_*` slot.

### What each half does

* `port_failure_driver.c` — a real driver shared object whose entry declares `driver_name`
  `port_failure_driver` (also the compiled object's **stem**, since the name
  `erl_ddll:try_load/3` registers is what `{spawn_driver, Name}` opens), `driver_flags` `0`, and only
  `start`/`stop`/`output` populated — no `init`, no `control`, no `ready_input`/`ready_output`, no
  `timeout`, and it never calls `driver_select`. `start` allocates the private state that holds the
  `ErlDrvPort` handle its own `driver_output`/`driver_failure*` calls need (`ERL_DRV_ERROR_GENERAL` if
  the allocation fails), `stop` frees it, and `output` dispatches on the command's first byte to one
  row of the C's port-closing family:

  | command | the C calls | the row |
  |---|---|---|
  | `1` | `driver_output(port, "hi", 2)` **then** `driver_failure(port, 9)` | a read, then exit `9` — the **ordering** row |
  | `2` | `driver_failure_eof(port)` | exit `normal` |
  | `3` | `driver_failure_atom(port, "boom")` | exit `boom` |
  | `4` | `driver_failure_posix(port, EACCES)` | exit `eacces` |
  | `5` | `driver_failure(port, 7)` | exit the **bare integer** `7` |
  | `6` | `driver_failure(port, 0)` | exit `normal` |
  | `7` | `driver_exit(port, 0)` | **no** exit signal at all |
  | `8` | `driver_exit(port, EACCES)` | **no** exit signal at all |

  Rows 7/8 are the C's `signals_owner == false` arm — `driver_exit` unlinks the owner first, so a
  linked (even trapping) owner is told nothing while a **monitor** is still told.

* `port_failure.erl` — the probing consumer. Its four exports (`trapping/3`, `trapping_monitored/3`,
  `trapping_list_monitored/3`, `non_trapping/3`) all take the object's directory, its registered name
  and the row's command byte, so one compiled module serves every row. `open/2` is
  `{ok, _} = erl_ddll:try_load(Dir, Name, [])` followed by
  `open_port({spawn_driver, Name}, [binary])` (`open_list/2` passes `[]`, for the list-stream row) —
  the **same** expression on both runtimes, unlike the busy fixture's per-runtime open. The three
  trapping rows set `process_flag(trap_exit, true)`, drive one command with
  `port_command(P, <<Op>>)`, and drain what they receive **in order** — `drain/2` collects at most
  four messages, each `receive` bounded by `after 300` — and then report
  `{erlang:port_info(P), try erlang:port_close(P) catch C:R -> {C,R} end}`. `trapping_monitored_with/4`
  additionally takes `Rf = erlang:monitor(port, P)` after opening and returns `is_reference(Rf)` as a
  third element. Every message is **normalized** (`shape/1` → `{exit,true,Reason}` /
  `{down,true,true,Reason}` / `{data,Data}` / `{misc,Other}`), because a `#Port<…>` and a `#Ref<…>`
  cannot be equal across two runtimes or two OS processes; the *shape* and the **order** are what the
  two sides must agree on. `non_trapping/3` is the one row that needs a second process: it
  `spawn_monitor`s a helper that opens the port, reports `{opened, P}` **before** commanding it (a
  non-`normal` reason kills the owner from inside `port_command/2`, so a send after it would race the
  kill), then sleeps 400 ms and sends `alive`; the collector drains three messages with `after 700`
  (`n_shape/1` → `opened_port` / `alive` / `{helper_down,true,Reason}`).

### The artifacts

Two artifacts, with two different lifetimes: the driver **`.so`** is compiled at test time, the
probe **`.beam`** is committed. `tests/port_failure.rs` stages both into one per-test scratch
directory — that directory is both the runtime's `-pa` and the path `erl_ddll:try_load/3` searches
— by copying the committed image in and running `$CC` beside it.

**Committed: `port_failure.beam` (2 656 bytes)** — `port_failure.erl` compiled with
`+deterministic`, so the image is path-free and byte-stable (md5
`1044dd92b5f9472f169213ba0eab206e`; regenerating in two different scratch directories yields the
identical file). Compile it in a neutral `$TMPDIR` scratch dir, like every other committed fixture:

```sh
SCRATCH="$TMPDIR/beam_rt_port_failure_fix"
rm -rf "$SCRATCH"; mkdir -p "$SCRATCH/gen"
cp port_failure.erl "$SCRATCH/gen/"
( cd "$SCRATCH/gen" && erlc +deterministic port_failure.erl )
cp "$SCRATCH/gen/port_failure.beam" port_failure.beam    # 2 656 bytes
git add -f port_failure.beam
```

The repo-root `.gitignore` drops `*.beam` globally, so the image must be **force-added** like the
other committed fixtures. It is a **prerequisite of the suite, not a toolchain question**: its
absence is a repository defect and fails (`require_probe_beam` / the dedicated
`the_probe_image_is_committed` test), never a skip — otherwise all nine rows would report green
without one oracle comparison.

**Compiled at test time: `port_failure_driver.so`** — the C side, built with `$CC` (or `cc`)
against the host `erl_driver.h`, whose directory is resolved from the
**canonicalized `erl` sibling** (`…/erts-<vsn>/include`, i.e. `erts-<vsn>` is the grandparent of the
real `erl` binary), then `$ERL_TOP`, then the standard install roots (`/usr/lib/erlang`,
`/usr/local/lib/erlang`, `/usr/lib64/erlang`, `/opt/erlang`) — each tried directly as
`<root>/include` and `<root>/lib/erlang/include`, and then as the first `erts-*/include` naming the
header. The suite does not consult `code:lib_dir(erts,include)`, which names a path that does
**not** exist in the Nix store layout. The equivalent hand-built recipe:

```sh
D="$TMPDIR/beam_rt_port_failure"
ERL=$(readlink -f "$(command -v erl)"); INC="$(dirname "$(dirname "$ERL")")/include"
cc -shared -fPIC -I"$INC" -o "$D/port_failure_driver.so" port_failure_driver.c
```

### Oracle values (`erl -noshell`, erts-17.1 / OTP 29.1.1)

One fresh port per row, printed with `~w` (`<<104,105>>` is `<<"hi">>`); a `—` is a combination the
suite does not drive, so nothing pins it:

| row | `trapping/3` | `trapping_monitored/3` | `trapping_list_monitored/3` | `non_trapping/3` |
|---|---|---|---|---|
| `1` | `[{data,<<"hi">>},{exit,true,9}]` | `[{data,<<"hi">>},{exit,true,9},{down,true,true,9}]` | `[{data,[104,105]},{exit,true,9},{down,true,true,9}]` | — |
| `2` | `[{exit,true,normal}]` | — | — | `[opened_port,alive,{helper_down,true,normal}]` (survives) |
| `3` | `[{exit,true,boom}]` | — | — | `[opened_port,{helper_down,true,boom}]` (terminated) |
| `4` | `[{exit,true,eacces}]` | — | — | `[opened_port,{helper_down,true,eacces}]` (terminated) |
| `5` | `[{exit,true,7}]` | — | — | `[opened_port,{helper_down,true,7}]` (terminated) |
| `6` | `[{exit,true,normal}]` | — | — | `[opened_port,alive,{helper_down,true,normal}]` (survives) |
| `7` | `[]` | `[{down,true,true,normal}]` | — | `[opened_port,alive,{helper_down,true,normal}]` (survives) |
| `8` | `[]` | `[{down,true,true,eacces}]` | — | `[opened_port,alive,{helper_down,true,normal}]` (survives) |

Every trapping row's answer is `{Messages, Post}` and every monitored row's `{Messages, Post, true}`,
where `Post` is the same after every row: `{undefined,{error,badarg}}` — `erlang:port_info/1` answers
`undefined` and the second `erlang:port_close/1` raises `badarg`. The full printed shapes:

```none
trapping/3              1  {[{data,<<104,105>>},{exit,true,9}],{undefined,{error,badarg}}}
                        2  {[{exit,true,normal}],{undefined,{error,badarg}}}
                        3  {[{exit,true,boom}],{undefined,{error,badarg}}}
                        4  {[{exit,true,eacces}],{undefined,{error,badarg}}}
                        5  {[{exit,true,7}],{undefined,{error,badarg}}}
                        6  {[{exit,true,normal}],{undefined,{error,badarg}}}
                        7  {[],{undefined,{error,badarg}}}
                        8  {[],{undefined,{error,badarg}}}
trapping_monitored/3    1  {[{data,<<104,105>>},{exit,true,9},{down,true,true,9}],
                            {undefined,{error,badarg}},true}
                        7  {[{down,true,true,normal}],{undefined,{error,badarg}},true}
                        8  {[{down,true,true,eacces}],{undefined,{error,badarg}},true}
trapping_list_monitored/3 1 {[{data,[104,105]},{exit,true,9},{down,true,true,9}],
                            {undefined,{error,badarg}},true}
non_trapping/3          2  [opened_port,alive,{helper_down,true,normal}]
                        6  [opened_port,alive,{helper_down,true,normal}]
                        7  [opened_port,alive,{helper_down,true,normal}]
                        8  [opened_port,alive,{helper_down,true,normal}]
                        3  [opened_port,{helper_down,true,boom}]
                        4  [opened_port,{helper_down,true,eacces}]
                        5  [opened_port,{helper_down,true,7}]
```

The two rules the table encodes: a **trapping** owner gets `{'EXIT',Port,Reason}` for every failure
reason (a numeric one — `driver_failure(port, 7)` — is the bare integer `7`, never an atom), while a
**non-trapping** owner is terminated by a non-`normal` reason (its own `'DOWN'` carries that reason)
and survives `normal`; and `driver_exit` never produces an `'EXIT'` at all, yet its monitor still
gets a `'DOWN'`.

### What the suite pins, and on which route

Both delivery routes are covered, because they are separate code paths over one policy — the
**`-eval` route** returns the messages to the evaluator's own `receive`
(`RuntimeContext::poll_port_messages`) and the **scheduler `run` route** puts them in the recipient's
mailbox (`Runtime::deliver_port_signals`), so a receiver parked in a compiled `receive` is woken by
them. Each row is one probe pair: the same expression prints under the oracle and under `beam_rt`,
and the two stdouts must be equal. The `run` route passes the directory as a double-quoted `run`
argument (a charlist), the driver name as a bare atom and the row as an integer
(`beam_rt run port_failure.beam port_failure:<function>/3 "<dir>" port_failure_driver <row>`).

| test | rows |
|---|---|
| `a_read_written_before_the_failure_is_delivered_first` | `trapping/3` 1 (the read precedes the exit signal) |
| `the_run_route_delivers_the_read_before_the_exit` | `trapping/3` 1, on the scheduler route |
| `a_list_stream_port_renders_the_surviving_read_as_a_charlist` | `trapping_list_monitored/3` 1 |
| `a_trapping_owner_gets_the_exit_with_the_failures_reason` | `trapping/3` 2–6 |
| `driver_exit_tells_the_linked_owner_nothing` | `trapping/3` 7, 8 |
| `the_owner_that_is_only_linked_never_hears_a_driver_exit` | `trapping/3` 8, once |
| `a_monitor_gets_the_down_in_both_signal_arms` | `trapping_monitored/3` 1, 7, 8 |
| `a_non_trapping_owner_is_terminated_by_a_non_normal_reason` | `non_trapping/3` 3, 4, 5 |
| `a_non_trapping_owner_survives_a_normal_reason` | `non_trapping/3` 2, 6, 7 |

### Skip-loudly rule

Each test skips **loudly** (a `beam_rt port_failure SKIPPED` banner and a return, never a silent
pass) when the oracle `erl` is absent, when no `erl_driver.h` include directory is found, or when
the C compiler is missing or fails — a **toolchain** absence, which the host may legitimately have.

A missing **committed** probe image is not a skip: `tests/fixtures/port_failure.beam` is a
repository artifact, so `require_probe_beam` panics with the regeneration recipe (and
`the_probe_image_is_committed` fails alongside it), because skipping there would report nine green
rows that compared nothing.
### `zlib_stream_levels_fixture.erl` — compiled at test time, never committed as a `.beam`
`zlib_stream_levels_fixture.erl` is the **`zlib` per-level / flush / init-form** image for
`tests/zlib_stream_levels.rs`, driven through the scheduler `run` route
(`beam_rt run <scratch>/zlib_stream_levels_fixture.beam zlib_stream_levels_fixture:<f>/0`). Like
`zlib_stream.erl` it is **source only**: the suite compiles it into a per-fixture scratch directory
holding only this module (a safe single `-pa` for the oracle) with the system `erlc` at test time.
Six exported functions cover the six surfaces — `multi_sync/0` (the four-`sync`-then-`finish`
transcript per compression level 0–9 with its per-call byte-count vector, total and md5),
`flush_modes/0` (the whole payload with each flush), `init_forms/0` (`deflateInit/1,2,6`),
`reset_mid_stream/0` (`deflateReset/1`), `deflate_params/0` (`deflateParams/3`) and `round_trip/0`
(deflate → `inflate/2`).
Every function **prints** its transcript with `io:format/2` and returns `ok`, never a binary: the
`run` route renders its result with `~w`, and this runtime's binary printing is `<<55,56,…>>` where
the oracle prints `<<"AB">>` for printable content (a pre-existing printer limitation, unrelated to
`zlib`). Sizes are integers and digests are `binary:encode_hex/1` charlists, and the payload's own
`bytes`/`md5` line pins it to the same 1 024 bytes the `-eval` probes build. A missing `erlc` skips
the `run`-route half **loudly** (the `-eval` half still runs).
### `zlib_stream.erl` — compiled at test time, never committed as a `.beam`
`zlib_stream.erl` is the **`zlib` stream-API** image for `tests/zlib_stream_dict.rs`, driven through
the scheduler `run` route (`beam_rt run <scratch>/zlib_stream.beam zlib_stream:<f>/0`). Unlike the
committed images above it is **source only**: the suite copies it into a scratch directory and
compiles it there with the system `erlc` at test time (the file is the source of truth, and a
committed `.beam` would age independently of it). Five exported functions cover the five cases:
`params_stash/0` (`deflateParams/3`'s stash path), `dict_protocol/0` (the `need_dictionary`
protocol), `safe_inflate_chunks/0` (`safeInflate/2` chunking), `reset_and_dictionary/0`
(`inflateReset/1`, `inflateGetDictionary/1`, `deflateReset/1`) and `truncated_stream/0` (a stream
missing its Adler trailer is only caught by `inflateEnd/1`).

Every function returns a term, **never a binary**: the `run` route prints its result with `~w`, and
this runtime's binary printing is `<<55,56,…>>` where the oracle prints `<<"AB">>` for printable
content (a pre-existing printer limitation, unrelated to `zlib`). Hex digests therefore travel as
`binary_to_list(binary:encode_hex/1)` integer lists and payloads as sizes plus `=:=` booleans; the
scratch directory holds **only** this module, so it is also a safe single `-pa` for the oracle.
A missing `erlc` skips the `run`-route half **loudly** (the `-eval` half still runs).
### `zlib_one_shot_fixture.erl` — compiled at test time, never committed as a `.beam`
`zlib_one_shot_fixture.erl` is the **one-shot `zlib`** image (the six convenience functions
`compress/1`, `uncompress/1`, `zip/1`, `unzip/1`, `gzip/1`, `gunzip/1`) for
`tests/zlib_one_shot.rs`, driven through the scheduler `run` route
(`beam_rt run <scratch>/zlib_one_shot_fixture.beam zlib_one_shot_fixture:<f>/0`). Like
`zlib_stream.erl` it is **source only**: the suite compiles it into a scratch directory with the
system `erlc` at test time. Five exported functions cover the five surfaces — `goldens/0` (the
three named hex goldens), `fast_paths/0` (each function's bytes plus the three round trips over
the six-payload table), `wrapper/0` (`deflateInit/6` + `deflate(…, finish)` + `deflateEnd` +
`close`, at window bits 15 / -15 / 31, against the one-shot image), `libz/0` (the whole
`zlib:compress/1` image — A.9 libz byte exactness) and `concat/0` (`gunzip/1` over concatenated
gzip members).
Every function **prints** its transcript with `io:format/2` and returns `ok`, **never a binary**:
the `run` route renders its result with `~w`, and this runtime's binary printing is `<<55,56,…>>`
where the oracle prints `<<"AB">>` for printable content (a pre-existing printer limitation,
unrelated to `zlib`). Sizes are integers and digests/hex images are
`binary_to_list(binary:encode_hex/1)` charlists. The payload table (`payload/1`) is
byte-identical to the `-eval` expressions' `PAYLOADS_ERL` list, and the transcript's own
`payload <label> bytes <N> md5 <HEX>` line is what pins the two constructions together. The
scratch directory holds **only** this module, so it is also a safe single `-pa` for the oracle; a
missing `erlc` skips the `run`-route half **loudly** (the `-eval` half still runs).

### `zlib_guards.erl` / `zlib_guards_unnamed.erl` — compiled at test time, never committed as `.beam`
The **`zlib` guard-class / controlling-process** images for `tests/zlib_guards.rs`. Both are source
only and are compiled by the suite with the system `erlc` into a scratch directory holding **only
that module** (a safe single `-pa` for the oracle).
`zlib_guards.erl` exports `guard_atoms/0`, `classes/0` (the `data_error` / `stream_error` /
`not_initialized` / `already_initialized` / `badarg` families and the `ok` boundaries around them),
`controlling/0` (`not_on_controlling_process` and the `set_controlling_process/2` transfer both
ways, plus a dead pid and a non-pid) and `closed_controller/0` (`set_controlling_process/2` across
the fresh / ended / closed states, which all answer `ok` on both runtimes — the C's
`set_controller_nif` imposes no state precondition). `guard_atoms/0` is **documentation** of the
class set: it names the six guard atoms so they enter the module's atom table, but nothing depends
on that — a raise whose reason atom is absent from the raising module's atom table is catchable
anyway, because the runtime names a BIF raise's reason atom in its one shared table.
`zlib_guards_unnamed.erl` is that probe shape *without* the atom reference (`deflate_end_twice/0`,
`deflate_init_twice/0`, `after_finish/0`); the suite pins that it answers the **same** caught
`{exception,error,<atom>}` lines as the system `erl` oracle and as the named fixture. Nothing in it
may spell a guard atom — not even as a function name, since an exported name *is* an atom in the
table (measured).
Both fixtures print their transcript with `io:format/2` and return `ok`, so the compared observable
is the printed transcript and never the `run` route's `~w` result line; no binary is ever printed
(probes answer atoms, integers, booleans and small tuples).
