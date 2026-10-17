# `beam_rt` test fixtures

`mailbox.beam`, `sched.beam`, `port_recv.beam`, `raise_structured.beam`,
`function_clause_fixture.beam`, `recv_after.beam`, `dist_echo.beam` and `pingpong.beam` are
committed oracle fixtures: real `.beam` images produced by the **system Erlang** toolchain
(OTP 29 / `erts-17.1`), which is also the differential oracle.
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
delivery-driven collection of the parked receiver used to corrupt its live scan options:

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

## GC fixtures (`gc_loop` · `gc_ints` · `gc_floats` · `heap_growth` · `gc_payload` · `pt_gc` · `churn`, committed)

These seven are committed oracle fixtures for the process-heap collector
(`tests/gc_collect.rs`, `tests/heap_growth.rs`, `tests/gc_strategy.rs`,
`tests/gc_immix_payload.rs`, `tests/persistent_term.rs` and `tests/rss_bound.rs`):
* `gc_loop.beam` — `loop/1` tail-recursively conses every iteration (`build/1`), so a
  scheduled process really allocates; the acceptance evidence that the collector runs at the
  engine's heap-admission point.
* `gc_ints.beam` — `l1/1` accumulates `+`/`rem` on small integers only: it allocates nothing,
  pinning that a non-allocating loop's young heap never moves.
* `gc_floats.beam` — `l1/1` accumulates `sqrt/1` in one IEEE-754 double: a module with a
  float-family instruction is never collected over (a live float register is outside the root
  set), and its result must stay exact.
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

All seven are path-free (`+deterministic`) and regenerate with the same scratch-dir recipe:

```sh
for f in gc_loop gc_ints gc_floats heap_growth gc_payload pt_gc churn; do    SCRATCH="$TMPDIR/beam_rt_${f}_fix"
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
