# `beam_rt` test fixtures — the C driver and port fixtures

Every fixture in this doc has a compiled-at-test-time C half (a real `driver_*`-only shared object or a NIF), and the Erlang half is either generated at test time or committed as a path-free `.beam`.
None of the C sources is ever committed as an object: each embeds or is interpolated with the scratch directory the test builds in.
The general skip rule — a missing toolchain is a loud `SKIP`, a missing committed fixture **fails** — is [`README.md`](README.md)'s `## Skip-loudly rule`; each section below adds only its own suite's specifics.

| fixture | subject |
|---|---|
| `event_driver.c` / `event_driver.erl` | the driver `driver_select` + `driver_set_timer` + output-family modes |
| `reg.c` / `reg_recv.erl` | the port-less driver registry hooks |
| `env_nif.c` / `env_nif.erl` / `env_caller.erl` | `enif_alloc_env`/`enif_free_env` over a real NIF |
| `busy_driver.c` | the `port_command/2,3` busy/suspend/`[force]` table over a real busy driver |
| `port_failure_driver.c` + `port_failure.erl` | the `driver_failure*`/`driver_exit` port-closing family |

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
identical file). Its regeneration recipe lives in [`README.md`](README.md)'s `## Regeneration` section, like every other committed fixture.

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

## See Also

* [`README.md`](README.md) — the index, the committed `.beam` images, the `## Regeneration` recipe and the `## Oracle values` table.
* [`ERL_SOURCE_FIXTURES.md`](ERL_SOURCE_FIXTURES.md) — the `.erl` fixtures compiled at test time.
