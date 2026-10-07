# `beam_rt` test fixtures

`mailbox.beam`, `sched.beam`, `port_recv.beam`, `raise_structured.beam`,
`function_clause_fixture.beam` and `recv_after.beam` are committed oracle fixtures: real
`.beam` images produced by the **system Erlang** toolchain (OTP 29 / `erts-17.1`), which is
also the differential oracle.
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
  `src/runtime.rs` and `src/runner.rs`): `after20/0` and `after2000/0` park on an empty mailbox
  and time out, `afterzero/0` (`after 0`) never suspends, `self_send/0` matches the message
  already queued and never arms a deadline, and `wait_forever/0` (`after infinity`) parks. It
  pins the five opcode shapes the runtime deadline path is read from (`wait_timeout` +
  `timeout`, the compiler-folded `after 0`, a matched queued message, a message that **cancels**
  an armed deadline, and a bare `wait`).

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

# recv_after.beam (1064 bytes) — compiled in place with the system erlc
erlc -o tests/fixtures tests/fixtures/recv_after.erl
```
`recv_after.beam` is the one exception to the `+deterministic` recipe above: it is compiled
**in place** (run from the crate root), so its `CInf.source` and `Dbgi` chunks record the
`.erl` path rather than being stripped. It is otherwise a plain system-`erlc` image and
nothing in the tests depends on the embedded path.

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
Any byte change in a fixture invalidates the expected values in the tests that load it;
re-derive them from the system `erl`, do **not** guess.

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
| `recv_after:after20/0` | `recv_after:after20()` | `timeout` (after ≈ 20 ms) |
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
`src/runner.rs::parked_receive_timeout` recovers the deadline from), `afterzero/0` folds the
`after 0` away so an empty mailbox falls straight to `timeout`, `self_send/0` matches the
queued message and never arms a deadline, and `wait_forever/0` (`after infinity`) is a bare
`wait`. Only the two literal-timeout exports are read here; a **variable** `after T` compiles
to the same `wait_timeout` with a Y-register operand, which the runner cannot read today (see
`CONTEXT.md`'s Known Issues). The fixture is driven by the receive-deadline tests in
`src/runtime.rs` (the scheduler path) and `src/runner.rs` (the parked-timeout bookkeeping).

`marked/0`-style exports that would drive the `recv_marker_*` prologue are deliberately
**not** in either fixture: the prologue is emitted for a receive that matches on a
reference created before the send, which needs `erlang:make_ref/0` — a `beam_bifs` BIF
that Milestone 1 does not register. The marker seam is covered directly instead (the
unit tests in `src/mailbox.rs` and `src/context.rs`).
