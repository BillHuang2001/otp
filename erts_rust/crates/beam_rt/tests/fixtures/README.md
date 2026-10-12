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
  `src/runtime.rs` and `src/runner/`): `after20/0` and `after2000/0` park on an empty mailbox
  and time out, `afterzero/0` (`after 0`) never suspends, `self_send/0` matches the message
  already queued and never arms a deadline, and `wait_forever/0` (`after infinity`) parks. It
  pins the five opcode shapes the runtime deadline path is read from (`wait_timeout` +
  `timeout`, the compiler-folded `after 0`, a matched queued message, a message that **cancels**
  an armed deadline, and a bare `wait`).

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
Any byte change in a fixture invalidates the expected values in the tests that load it;
re-derive them from the system `erl`, do **not** guess.

## GC fixtures (`gc_loop` · `gc_ints` · `gc_floats` · `heap_growth` · `gc_payload` · `pt_gc`, committed)

These six are committed oracle fixtures for the process-heap collector
(`tests/gc_collect.rs`, `tests/heap_growth.rs`, `tests/gc_strategy.rs`,
`tests/gc_immix_payload.rs` and `tests/persistent_term.rs`):

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

All six are path-free (`+deterministic`) and regenerate with the same scratch-dir recipe:

```sh
for f in gc_loop gc_ints gc_floats heap_growth gc_payload pt_gc; do
    SCRATCH="$TMPDIR/beam_rt_${f}_fix"
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
