# `beam_rt` test fixtures

This directory holds every fixture the `beam_rt` integration suites (`../`) use: committed `.beam` images produced by the system Erlang toolchain, `.erl` sources compiled at test time, and the C driver/port fixtures.
The documentation is split by subject across three files in this directory:

| doc | subject |
|---|---|
| `README.md` (this file) | the index, the committed `.beam` images (name list, descriptions, byte sizes), the `## Regeneration` recipe for every committed image, the GC images and the `## Oracle values` table |
| [`ERL_SOURCE_FIXTURES.md`](ERL_SOURCE_FIXTURES.md) | the `.erl` fixtures compiled at test time (the `epp` pair, `inet_active.erl`, the execution-position pair and the `zlib_*` sources) |
| [`PORT_AND_DRIVER_FIXTURES.md`](PORT_AND_DRIVER_FIXTURES.md) | the C/`.c` fixtures and the driver-port fixtures (`event_driver`, `reg`, `env_nif`, `busy_driver`, `port_failure`) |

## Skip-loudly rule (this rule binds all three docs)

A missing **toolchain** — `erlc`, `erl`, a C compiler or `erl_driver.h` — is always a loud `SKIP` banner plus a `return`, never a silent pass, because the host may legitimately lack it.
A missing **committed** fixture is a different class: it is a repository defect and **fails**, because a skip there would report green rows that compared nothing.
The per-doc sections add only their suite's own specifics (which tools, the 30 s `support::PROBE_TIMEOUT` bound, and the `port_failure.beam` non-skip rule).

## Committed `.beam` images

All 27 committed images, by name (byte sizes are the committed files' own):

| image | bytes | described under |
|---|---|---|
| `mailbox.beam` | 740 | `## Committed oracle fixtures` |
| `rescan.beam` | 1 660 | `## Committed oracle fixtures` |
| `sched.beam` | 592 | `## Committed oracle fixtures` |
| `port_recv.beam` | 680 | `## Committed oracle fixtures` |
| `raise_structured.beam` | 644 | `## Committed oracle fixtures` |
| `function_clause_fixture.beam` | 548 | `## Committed oracle fixtures` |
| `recv_after.beam` | 824 | `## Committed oracle fixtures` |
| `dist_echo.beam` | 632 | `## Committed oracle fixtures` |
| `port_exit.beam` | 2 512 | `## Committed oracle fixtures` |
| `port_suspend.beam` | 1 048 | `## Committed oracle fixtures` |
| `bifent.beam` | 720 | `## Committed oracle fixtures` |
| `tmr.beam` | 660 | `## Committed oracle fixtures` |
| `pingpong.beam` | 1 004 | `## Committed oracle fixtures` |
| `remote_send.beam` | 984 | `## Regeneration` |
| `port_failure.beam` | 2 656 | [`PORT_AND_DRIVER_FIXTURES.md`](PORT_AND_DRIVER_FIXTURES.md) |
| `maxheap.beam` | 6 088 | `../max_heap_enforce.rs` |
| `preempt.beam` | 748 | `../preempt.rs` |
| `reds_boundary.beam` | 928 | `src/runner/tests/reds_door.rs` |
| `native_reds.beam` | 796 | `src/runner/tests/slice_charging.rs` |
| `gc_loop.beam` | 784 | `## GC fixtures` |
| `gc_ints.beam` | 664 | `## GC fixtures` |
| `gc_floats.beam` | 740 | `## GC fixtures` |
| `heap_growth.beam` | 1 088 | `## GC fixtures` |
| `gc_payload.beam` | 1 052 | `## GC fixtures` |
| `pt_gc.beam` | 1 076 | `## GC fixtures` |
| `churn.beam` | 1 000 | `## GC fixtures` |
| `vp.beam` | 1 104 | `## GC fixtures` |

## Committed oracle fixtures

`mailbox.beam`, `sched.beam`, `port_recv.beam`, `raise_structured.beam`,
`function_clause_fixture.beam`, `recv_after.beam`, `dist_echo.beam`, `port_exit.beam`,
`port_suspend.beam` and `pingpong.beam` are committed oracle fixtures: real `.beam` images produced
by the **system Erlang** toolchain (OTP 29 / `erts-17.1`), which is also the differential oracle.
* `port_suspend.beam` (`port_suspend.erl`, compiled with the plain system `erlc`) is the **busy-port
  BIF-suspend** image for `src/port/busy_suspend_tests.rs`: `park/1` opens a busy driver port and
  calls `port_command/2` on it (the shape that parks), and `park_with_clearing_helper/1` additionally
  spawns a helper that blocks in `receive after 50` and then calls `port_control(Port, $N, <<>>)`
  (the fixture's `set_busy_port(port, 0)`), so one `run_scheduled` proves park → pump → resume and
  that the driver was written to **exactly once**.
* `pingpong.beam` is the **two-process ping-pong** image for the `run` route's sleep
  accounting (`tests/run_route_sleep.rs`): `go/1` spawns one fun-bodied worker and runs `N`
  synchronous round trips (`Worker ! {self(), N}` / a bound-variable `receive {_, N}`, with
  an `after 5000` guard that raises `{pingpong_timeout, N}` instead of hanging). Every
  iteration parks the parent on an empty mailbox until the reply arrives, so the run queue
  empties and the pass boundary is reached repeatedly — the shape whose per-pass poll sleep
  the syscall assertion rules out.
* `mailbox.beam` is the receive/message end-to-end image — `tests/mailbox.rs` (the
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

# port_failure.beam (2 656 bytes, md5 1044dd92b5f9472f169213ba0eab206e)
SCRATCH="$TMPDIR/beam_rt_port_failure_fix"
rm -rf "$SCRATCH"; mkdir -p "$SCRATCH/gen"
cp port_failure.erl "$SCRATCH/gen/"
( cd "$SCRATCH/gen" && erlc +deterministic port_failure.erl )
cp "$SCRATCH/gen/port_failure.beam" port_failure.beam    # 2 656 bytes
git add -f port_failure.beam
```
Every fixture in this section is built with `+deterministic` in a `$TMPDIR` scratch dir, which strips the embedded source path.
Two committed images are **not** path-free — `strings reds_boundary.beam port_suspend.beam | rg '/home/'` reports a worktree path in each:
`reds_boundary.beam` and `port_suspend.beam` were compiled without `+deterministic`.
Neither suite compares its image bytes, so nothing is flaky; recompiling either one with the deterministic recipe above removes the leak.

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
| `churn:run/1` | `churn:run(N)` | `N` (each churn checks `byte_size`/`tuple_size` and adds 1) |
| `recv_after:after20/0` | `recv_after:after20()` | `timeout` (after ≈ 20 ms) |
| `recv_after:afterzero/0` | `recv_after:afterzero()` | `timeout` (no suspend) |
| `recv_after:self_send/0` | `recv_after:self_send()` | `hello` |
| `recv_after:after2000/0` | a receiver matching nothing, then a `hello` send | `hello` (the send **cancels** the deadline); with no send it is `timeout` |
| `port_exit:trap_owner/0` | `port_exit:trap_owner()` | `[{exit,true,normal}]` |
| `port_exit:trap_owner_mon/0` | `port_exit:trap_owner_mon()` | `[{exit,true,normal},{down,true,port,true,normal}]` (`'EXIT'` first, once each) |
| `port_exit:notrap_owner_mon/0` | `port_exit:notrap_owner_mon()` | `[{down,true,port,true,normal}]` (nothing for `normal`; it survives) |
| `port_exit:watcher_only/0` | `port_exit:watcher_only()` | `[{down,true,port,true,normal}]` |
| `port_exit:connected/0` | `port_exit:connected()` | `{true,1,2,[{exit,true,normal}]}` (links 1 → 2 after `port_connect/2`) |
| `recv_after:wait_forever/0` | `spawn(recv_after, wait_forever, [])` | parks indefinitely (`after infinity` ⇒ bare `wait`) |

`selfsend/0` compiles to `bif self` + `send` + `loop_rec` + `remove_message` (with a
`wait` fail branch); `scan/0` additionally emits `loop_rec_end`, because `42` fails the
`{bar, X}` test and the scan must advance to the second message. `wait/0` is a bare
`loop_rec`/`wait` loop and therefore suspends on an empty mailbox. `send2/2` emits two
`send`s and builds `{bar, N + 7}` from the **run-time** argument `N` (an ordinary
`copy_struct` across two processes, not a folded literal).

`recv_after`'s exports pin the finite-`after` shapes: `after20/0`/`after2000/0` compile to a
`wait_timeout` carrying the literal `{integer,T}` followed by `timeout` (the shape
`Machine::wait_deadline` recovers the deadline from), `afterzero/0` folds the
`after 0` away so an empty mailbox falls straight to `timeout`, `self_send/0` matches the
queued message and never arms a deadline, and `wait_forever/0` (`after infinity`) is a bare
`wait`. Only the two literal-timeout exports are read here; a **variable** `after T` compiles
to the same `wait_timeout` with a Y-register operand, which the runner cannot read today (see
`CONTEXT.md`'s Known Issues). The fixture is driven by the receive-deadline tests in
`src/runtime.rs` (the scheduler path) and `src/runner/` (the parked-timeout bookkeeping).

`rescan:marked/0` drives the `recv_marker_*` prologue (a receive matching on a reference created before the send), and `junk_then_ref/0` and `two_receives/0` pin the anchor and the rescan positions.

## Every other fixture file in this directory

The files below are consumed by the named suite; `ERL_SOURCE_FIXTURES.md` and `PORT_AND_DRIVER_FIXTURES.md` describe the ones whose detail is load-bearing, and this table is the index for the rest (all compiled at test time unless noted).

| file | suite | what it is |
|---|---|---|
| `action_identity.erl` | `../action_identity.rs` | `ident/0` prints the `-s`/`-run` action process's own identity (registered name, `whereis(init)`, liveness, node) on one line |
| `funrecv.erl` / `funrecvw.erl` | `../fun_entry_receive.rs` | the fun-bodied process entry: a `make_fun3` closure entered at its own lambda label, with the blocking `receive` inline and one `call_ext` away |
| `inet_completion_parity.erl` / `inet_completion_udp.erl` | `../inet_completion.rs` | the passive-completion parity pair (`cpl_parity:go/0`, `cpl_udp:recv/0`) over real loopback sockets |
| `maxheap.erl` (compiled image committed as `maxheap.beam`) | `../max_heap_enforce.rs` | the `max_heap_size` kill/survive matrix |
| `preempt.erl` (compiled image committed as `preempt.beam`) | `../preempt.rs` | the allocation and spin loops the reduction pre-emption budget is measured on |
| `pi_binary.erl` | `../process_info_binary.rs` | `process_info(self(), binary)` over a self-built payload, a held payload and a held list |
| `plstart.erl` / `plwait.erl` | `../proc_lib_start.rs` | the `proc_lib:start_link` shapes and the `plwait` receive that waits on the ack/`'DOWN'` pair |
| `port_watch.erl` | `../port_monitor_cleanup.rs` | `watch/1` monitors a port and parks; `kill/1` removes the watcher |
| `sched_compile_driver.erl` / `sched_compile_input.erl` | `../compile_file.rs` | the scheduled-route `compile:file/2` driver and its one-function input |
| `sigself.erl` | `../signal_self_delivery.rs` | a process signalling itself out of its own slice (`killed/0`, `natural/0`, `ran_before_kill/0`) |
| `spawnmon.erl` | `../spawn_monitor.rs` | `spawn_monitor/1` incl. the local-node, bad-arg and remote shapes |
| `spawnslice.erl` | `../spawn_from_slice.rs` | a bare and a nested spawn issued from inside a running slice |
| `nif_monitor.c` / `nif_monitor.erl` / `nif_monitor_caller.erl` | `../nif_monitor.rs` | the `enif_monitor_process`/`enif_demonitor_process` NIF, its stub module and the external caller |
| `nif_resource_gc.c` | `../nif_resource_gc.rs`, `../nif_resource_nested.rs` | the resource-type destructor fixture the reachability prune is measured against |
| `reg.c` / `reg_recv.erl` | `../driver.rs` | **inline in `tests/driver.rs`** (`REG_DRIVER_C` and the `reg_recv` source), not files in this directory |
| `env_nif.c` / `env_nif.erl` / `env_caller.erl` | `../nif.rs` | **inline in `tests/nif.rs`** (`ENV_NIF_ERL`/`ENV_CALLER_ERL` and the C), not files in this directory |

## See Also

* [`ERL_SOURCE_FIXTURES.md`](ERL_SOURCE_FIXTURES.md) — the `.erl` fixtures compiled at test time.
* [`PORT_AND_DRIVER_FIXTURES.md`](PORT_AND_DRIVER_FIXTURES.md) — the C driver and port fixtures.
* `../CONTEXT.md` — the `beam_rt` test-suite routing table (which suite loads which fixture).
