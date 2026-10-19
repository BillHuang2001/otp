# Using the Rust ERTS (`beam_rt`)

How to build this runtime and use it to run Erlang and Elixir code, what works, and what does not.
Every command in this file was run on this tree (Erlang/OTP **29.1.1 / erts-17.1** on `PATH`,
`rustc` 1.98.1, 14-core x86-64 Linux) and the output shown is the real output.
Claims that were *not* re-run here are marked **documented** and name their source.
Measured performance lives in [`tools/bench/RESULTS.md`](tools/bench/RESULTS.md); the analysis of the
remaining gaps is in [`BOTTLENECKS.md`](BOTTLENECKS.md).

---

## 1. What this is

`erts_rust/` is a from-scratch, memory-safe Rust reimplementation of **ERTS** — the Erlang/BEAM
*runtime* (processes, scheduler, term representation, GC, ETS, distribution, ports, NIF/driver C
ABIs, `system_info`, CLI, boot) — targeting drop-in compatibility with Erlang/OTP `30.0-rc0`
(runtime `erts-17.1`), higher robustness, and better performance.
It is **not** a replacement for the OTP *libraries*: it runs `.beam` files produced by the real
`erlc`, and it loads the OTP applications/libraries (`.app`, `.beam`) that are installed on disk —
the stdlib/kernel/compiler code you run is the same code the system Erlang runs.
The workspace is 15 crates under `crates/` (term, alloc, codec, dist, ABI, loader, engine, gc,
process, message, bif, ets, io, rt, common) plus two tools under `tools/` (`differential`, `bench`).
Everything is **Apache-2.0**, matching OTP; no external dependency outside Apache-2.0/MIT.
The runtime binary is `beam_rt`; its CLI is the A.6 contract (`-eval`, `-s`, `-run`, `-pa`, `-boot`,
`run <file.beam> <m>:<f>/<a> args…`, the emulator switches, `ERL_*FLAGS`).

The compatibility contract is checked, not asserted: `tools/differential` runs a case under the real
`erl` and under `beam_rt` and compares observable results, and `gate.sh` exits non-zero on any
disagreement (see §6).

---

## 2. Prerequisites and build

### 2.1 What you need

| Need | Why | Version this tree is developed against |
|---|---|---|
| system Erlang/OTP | `erlc` produces the `.beam` inputs; `erl` is the differential oracle; the OTP apps on disk are what the runtime loads | **29.1.1 / erts-17.1** — the host toolchain and the CI pin (`.github/workflows/erts_rust.yml`, `otp-version: '29.1.1'`) |
| Rust toolchain | build `beam_rt` | **1.98.1**, pinned by `erts_rust/rust-toolchain.toml` |
| a C linker | links the Rust binary; the NIF/driver C-ABI tests compile C | `cc` + `lld` (default), `clang` + `mold` in the Nix devShell |
| Elixir (optional) | only to produce Elixir `.beam` inputs and to run the Elixir benchmark target | `elixirc` 1.20.4 here; nothing in the runtime needs it |

`beam_rt -version` reports `Erlang/OTP 30.0-rc0 (erts-17.1) [source] [64-bit]` — that is the
*release this runtime implements*, not the toolchain that built your code.

### 2.2 The Nix flake

`erts_rust/flake.nix` (`nix develop`) provides the pinned Rust toolchain, `clang` + `mold`, and the
oracle/target runtimes (`erlang`, `elixir`, `rebar3`, `dotnet-sdk_9`).
It exports `ERTS_ORACLE` (the oracle `erl`) and overrides the linker to `clang` + `mold` for
iteration speed; without Nix, `.cargo/config.toml` falls back to `cc` + `lld`.

### 2.3 Build — release (use this for anything you measure)

From the repository root (never `cd` into `erts_rust/`):

```sh
erts_rust/scripts/cargo.sh build --release --manifest-path erts_rust/Cargo.toml -p beam_rt
# → Finished `release` profile [optimized + debuginfo] target(s) in 47.63s   (cold tree, this host)
# binary: erts_rust/target/release/beam_rt
```

**Why the wrapper:** `$HOME` is read-only in this sandbox, so cargo's default `CARGO_HOME`
(`$HOME/.cargo`) is unwritable and a bare `cargo` fails.
`erts_rust/scripts/cargo.sh` picks the first writable `CARGO_HOME`
(`$HOME/.cache/cargo-home`, else `$TMPDIR/cargo-home`, else `/var/tmp/cargo-home`) and execs cargo.
Exporting `CARGO_HOME` yourself is equivalent; the cache is shared across worktrees, so a second
worktree does not re-download crates.
A bare `cargo` from the repository root also cannot resolve the workspace manifest — always pass
`--manifest-path erts_rust/Cargo.toml` (or invoke it through the wrapper as above).

Add `--offline` when you know the lockfile is vendored (the `bench` runbook does), and
`--all-targets` for the test/example surfaces.

### 2.4 Build — dev profile (fast iteration, **not** for timing)

```sh
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml -p beam_rt
# → Finished `dev` profile [optimized + debuginfo] target(s) in 1m 55s   (cold tree, this host)
# binary: erts_rust/target/debug/beam_rt
```

The dev profile is `opt-level = 1` with dependencies at `opt-level = 2` (workspace `Cargo.toml`), so
it is usable but ~2–6× slower than `--release` on the hot loops; the differential harness resolves
`target/{release,debug}/beam_rt` with **release first**.

Link time dominates incremental builds; `.cargo/config.toml` routes linking through `lld` and the
devShell through `mold`. If you get truncated or corrupt binaries after a killed build, delete the
partial object files and rebuild — a `cargo` invocation that is killed mid-link leaves a bad
`target/`.

---

## 3. Running Erlang code

### 3.1 The actions

`beam_rt` takes switches, then **one** action. The full parser surface is
[`crates/beam_rt/src/cli/CONTEXT.md`](crates/beam_rt/src/cli/CONTEXT.md); what a user needs:

| Switch / action | Meaning |
|---|---|
| `-version`, `--version` | print the version banner and exit (`Erlang/OTP 30.0-rc0 (erts-17.1) [source] [64-bit]`) |
| `-noshell` | head-less: no slogan, no shell prompt (the only mode in this milestone) |
| `-eval EXPR` | evaluate one Erlang expression; **prints nothing** by itself; one trailing `.` is tolerated |
| `-s Mod Func [Args…]` | `apply(Mod, Func, [atom Args…])`, result discarded |
| `-run Mod Func [Args…]` | same, but the arguments are **char lists** (`"2"`), result discarded |
| `-pa Dir` / `-pz Dir` | head / tail the code path (applied on every route, before the boot script) |
| `-boot File` | drive that boot script (default: this runtime's own `bootstrap/bin/start.boot`) |
| `run <file.beam> <mod>:<func>/<arity> [arg…]` | the batch runner: load the file, call the function, print the result |
| `+S`, `+A`, `+P`, `+Q`, `+K`, `+sbt`, `+hms`/`+hmbs`/`+hmax*`, `+JPperf*`, `-smp`, `-emu_flavor` | emulator switches (validated; most have no runtime-visible effect here yet) |
| `-name` / `-sname` | become a distributed node (A.5 phase 1) |
| `-env VAR VALUE`, `-config F`, `ERL_OTP30_FLAGS`/`ERL_AFLAGS`/`ERL_FLAGS`/`ERL_ZFLAGS` | environment/config surface, merged exactly as `erlexec` does |

There is **no interactive shell**: `-noshell` is the only mode, and a bare `beam_rt` prints the boot
slogan on stdout and exits.

### 3.2 The batch runner (`run`) — verified

`run` is the banner-free way to execute a compiled function. Integer, atom and list arguments work;
the exact arity is checked before the call.

```sh
$ erts_rust/target/release/beam_rt run erts_rust/crates/beam_loader/tests/fixtures/slice.beam slice:add/2 2 3
5
$ erts_rust/target/release/beam_rt run erts_rust/crates/beam_loader/tests/fixtures/slice.beam slice:fib/1 10
55
```

Compile your own module with the system `erlc` and run it the same way:

```sh
$ cat hello.erl
-module(hello).
-export([add/2, show/1, greet/1, bounce/0]).
add(A, B) -> A + B.
show(X) -> io:format("~p~n", [X]).
greet(Name) -> io:format("hello ~s~n", [Name]).
bounce() ->
    Self = self(),
    spawn(fun() -> Self ! {pong, 42} end),
    receive {pong, N} -> io:format("got ~p~n", [N]) end.

$ erlc -o /tmp/demo hello.erl
$ erts_rust/target/release/beam_rt run /tmp/demo/hello.beam hello:add/2 2 3
5
$ erts_rust/target/release/beam_rt run /tmp/demo/hello.beam hello:show/1 '[1,2,3]'
[1,2,3]
ok
$ erts_rust/target/release/beam_rt run /tmp/demo/hello.beam hello:add/2 -2 3
1
```

The argument grammar is deliberately small — **integer literals (including negative)**, **atoms**
(a bare word such as `world`), and **list literals** (`[1,2,3]`).
Two consequences a user will hit:

* **floats are rejected** — `… hello:add/2 2.5 3` exits **2** with
  `beam_rt: error "2.5" is not a valid run argument` plus the usage block;
* **there is no binary literal** — `… hello:kind/1 world` arrives as the **atom** `world`
  (`is_list=false is_binary=false`), and `… hello:greet/1 world` reaches a `~s` format as an atom.
  To exercise a function that takes a binary or a string, call it from a wrapper module (see §4) or
  through `-eval` with a literal.

Arity is checked: `run hello.beam hello:add/2 2` exits **2** with
`beam_rt: error hello:add/2 takes 2 argument(s), 1 given`.
An unknown module/function/arity exits **1** with `beam_rt: error undef` and prints nothing.

### 3.3 `-eval`, `-s`, `-run` — verified

```sh
$ beam_rt -noshell -eval 'io:format("hi~n")'
hi
$ beam_rt -noshell -eval '1+2'          # -eval itself prints nothing → no output, exit 0
$ beam_rt -eval 'io:format("hi~n")'     # without -noshell the booted kernel's user_drv adds the slogan
Erlang/OTP 30 [erts-17.1] [source] [64-bit]
hi
```

`-s` passes **atoms**, `-run` passes **char lists** (this is the OTP convention, reproduced):

```sh
$ beam_rt -noshell -pa /tmp/demo -s hello show abc
abc
$ beam_rt -noshell -pa /tmp/demo -run hello greet world
hello world
$ beam_rt -noshell -pa /tmp/demo -run hello add 2 3
beam_rt: badarith            # "2" + "3" — char lists, exactly as the oracle would fail
```

`-eval` is a **subset**, not the full `erl_eval`. It covers arithmetic, calls, list/map/tuple
literals, `fun`s, `spawn`, the send operator and a `receive` that a spawned sender satisfies:

```sh
$ beam_rt -noshell -pa /tmp/demo -eval 'hello:bounce()'
got 42
$ beam_rt -noshell -eval 'lists:map(fun(X) -> X end, [1,2])'   # exit 0, silent (result discarded)
```

Outside the subset you get a **typed** capability gap (exit **3**), never a wrong answer — e.g. a
`receive` with no sender and no finite timeout:

```sh
$ beam_rt -noshell -eval 'receive after infinity -> ok end'
beam_rt: unsupported: receive ... after infinity (no mailbox in -eval)     # exit 3
```

For anything non-trivial, compile a module and `run` it (or `-pa` it and call it from `-eval`).

### 3.4 Code path (`-pa` / `-pz`) — verified

```sh
$ beam_rt -noshell -pa /tmp/demo -eval 'io:format("~p~n",[hello:add(2,3)])'
5
```

`-pa` heads the path, `-pz` tails it, and both are in force *before* the boot script runs, so a `-pa`
directory can shadow an installed module for the boot script's very first `{primLoad, …}` batch.
`code:get_path/0` reports **36** entries on this tree — the release layout derived the way
`code_server.erl` `make_path` does it, matching the oracle element for element.

### 3.5 Boot script — verified

Every action except `-version` first drives a boot script (the `-boot` value, else this runtime's own
`bootstrap/bin/start.boot`), which loads the 22 preloaded modules and runs all 22 directives to
`{progress,started}`:

```sh
$ beam_rt -noshell -boot bootstrap/bin/start ; echo rc=$?
rc=0                            # empty stdout and stderr
$ beam_rt -noshell -eval 'io:format("x~n")'    # the default route boots too
x
```

The probe form is `-boot bootstrap/bin/start` — appending `.boot` doubles the suffix.

### 3.6 Output and exit codes (the stable external contract)

| Code | Meaning | stderr |
|---|---|---|
| `0` | success (also: a program's own `halt(N)` exits `N`, printing nothing) | — |
| `1` | a genuine failure: load/execute/BIF error, a rejected switch value, an unreadable `-config`, an unknown `+flag` | `beam_rt: <error>` (one line) |
| `2` | usage error: missing operand of an ordinary `-`-switch, bad `run` spec/arity | `beam_rt: <error>` **then** the `USAGE` block |
| `3` | **unsupported** — a capability gap of this milestone | exactly one line, `beam_rt: unsupported: <reason>`, no usage block |

Verified examples of each:

```sh
$ beam_rt run hello.beam hello:add/2 2 3 ; echo rc=$?     # rc=0, prints 5
$ beam_rt -noshell -eval 'nosuchmodule:f()' ; echo rc=$?  # beam_rt: error undef            rc=1
$ beam_rt -eval ; echo rc=$?                              # beam_rt: error switch -eval requires a value + usage   rc=2
$ beam_rt -noshell +zzz 1 -eval 'ok' ; echo rc=$?         # beam_rt: unknown flag -zzz      rc=1
$ beam_rt +Q 0 ; echo rc=$?                               # beam_rt: bad value "0" for switch +Q   rc=1
$ beam_rt -noshell -eval 'receive after infinity -> ok end' ; echo rc=$?
beam_rt: unsupported: receive ... after infinity (no mailbox in -eval)                         rc=3
```

**What exit 3 means for you:** the runtime reached something it does not implement and refused,
rather than guessing. It is **not catchable** from Erlang code — a `try … catch` around it still
exits 3, because it is reported out-of-band rather than raised as a term:

```sh
$ beam_rt -noshell -eval 'try (receive after infinity -> ok end) catch _:_ -> io:format("caught~n") end'
beam_rt: unsupported: receive ... after infinity (no mailbox in -eval)                         rc=3
```

Another reproducible exit-3 case: `beam_rt -noshell -eval 'io:format("~p~n",[erlang:fun_info(fun() -> ok end)])'`
→ `beam_rt: unsupported: bif` (a local fun's identity; recorded in the gate's coverage notes).

---

## 4. Running an Elixir project

Elixir is *ordinary* `.beam` code as far as this runtime is concerned: compile with the real
`elixirc`, then run the resulting `Elixir.*.beam` on `beam_rt`, putting Elixir's `ebin` on the path.

### 4.1 A worked example that succeeds — verified

```sh
$ cat demo.ex
defmodule Demo do
  def hello(name), do: IO.puts("hello " <> name)
  def sum(list), do: Enum.sum(list)
  def double(x), do: x * 2
end

$ elixirc -o /tmp/demo demo.ex          # real elixirc, Elixir 1.20.4 / OTP 29
$ EE=$(elixir -e 'IO.puts(Application.app_dir(:elixir, "ebin"))')

$ beam_rt -noshell -pa "$EE" run /tmp/demo/Elixir.Demo.beam "Elixir.Demo:double/1" 21
42
$ beam_rt -noshell -pa "$EE" run /tmp/demo/Elixir.Demo.beam "Elixir.Demo:sum/1" '[1,2,3,4]'
10
```

`sum/1` is the interesting one: it is a **protocol dispatch** (`Enumerable` → `Enum.sum/1`), i.e. real
Elixir library code running on this runtime, agreeing with the oracle (`10` on both).
Values returned here are real: `Elixir.Demo:double/1 21` → `42` on `beam_rt` **and** on the system
`erl` (`-eval "io:format(\"~p~n\",['Elixir.Demo':double(21)])"` → `42`).

IO works from a compiled Elixir module (Elixir's `IO.puts/1` and Erlang's `:io.format/2` both):

```sh
$ cat demo2.ex
defmodule Demo2 do
  def probe do
    try do
      IO.puts("x")
    catch
      k, r -> :io.format("caught ~p:~p~n", [k, r])
    end
  end
end
$ elixirc -o /tmp/demo demo2.ex
$ beam_rt -noshell -pa "$EE" run /tmp/demo/Elixir.Demo2.beam "Elixir.Demo2:probe/0"
x
ok
```

`GenServer` works end to end (both sides print `gen_server call -> 42`):

```erlang
%% gsmain.erl — an Erlang caller driving an Elixir GenServer
-module(gsmain). -export([go/0]).
go() ->
    {ok, Pid} = 'Elixir.GS':start_link(42),
    io:format("gen_server call -> ~p~n", [gen_server:call(Pid, get)]).
```

```sh
$ beam_rt -noshell -pa "$EE" -pa /tmp/demo -eval 'gsmain:go()'
gen_server call -> 42
```

### 4.2 What does not work, and how it fails

**(a) Elixir's `Agent`** — the `Agent` path does not work, in two distinct failure shapes. From a
**compiled module** the call never completes: it runs until the runtime's non-overridable 60 s
`RUN_DEADLINE` cuts it off (no output, exit 1):

```sh
$ cat ag.ex
defmodule AG do
  def go do
    {:ok, p} = Agent.start_link(fn -> 42 end)
    v = Agent.get(p, fn s -> s end)
    :io.format("agent -> ~p~n", [v])
  end
end
$ elixirc -o /tmp/demo ag.ex
$ beam_rt -noshell -pa "$EE" run /tmp/demo/Elixir.AG.beam "Elixir.AG:go/0"
beam_rt: error run: Elixir.AG:go/0 did not terminate within 60000 ms      # exit 1, after 60 s
$ erl -noshell -pa "$EE" -pa /tmp/demo -eval "'Elixir.AG':go(), erlang:halt(0)."
agent -> 42                                                              # the same .beam on the oracle
```

`Agent.get/3` is not what triggers it: calling only `Agent.start_link/1` from a compiled module has
the same outcome.

```sh
$ cat ag2.ex
defmodule AG2 do
  def go do
    r = Agent.start_link(fn -> 42 end)
    :io.format("start_link -> ~p~n", [r])
  end
end
$ elixirc -o /tmp/demo ag2.ex
$ beam_rt -noshell -pa "$EE" run /tmp/demo/Elixir.AG2.beam "Elixir.AG2:go/0"
beam_rt: error run: Elixir.AG2:go/0 did not terminate within 60000 ms      # exit 1, after 60 s
$ erl -noshell -pa "$EE" -pa /tmp/demo -eval "'Elixir.AG2':go(), erlang:halt(0)."
start_link -> {ok,<0.83.0>}
```

Called from `-eval` instead of from a compiled module the same expression fails **fast** (exit 1,
instantly) with a generic message, where the oracle returns `{ok,<0.83.0>}`:

```sh
$ beam_rt -noshell -pa "$EE" -eval "io:format(\"r=~p~n\",['Elixir.Agent':start_link(fun() -> 42 end)])"
beam_rt: term not found
$ erl -noshell -pa "$EE" -eval "io:format(\"r=~p~n\",['Elixir.Agent':start_link(fun() -> 42 end)]), erlang:halt(0)."
r={ok,<0.83.0>}
```

`term not found` is `BeamError::TermNotFound` (`crates/beam_common/src/error.rs:135`) — the generic
"the term/register slot I wanted is not there" error, not a diagnosis. Read it as *"this path is not
modelled yet"*, not as a bug in your program.

**(b) `mix`** — running Mix itself on this runtime fails while starting `Mix.Supervisor`; the
supervisor report names the mechanism, an exception-path gap in `build_stacktrace` (a frame naming a
module the running module cannot resolve):

```sh
$ ME=$(elixir -e 'IO.puts(Application.app_dir(:mix, "ebin"))')
$ beam_rt -pa "$EE" -pa "$ME" run "$ME/Elixir.Mix.CLI.beam" "Elixir.Mix.CLI:main/1" '["--version"]'
=SUPERVISOR REPORT==== 20-Oct-2026::00:48:36.780655 ===
    supervisor: {local,'Elixir.Mix.Supervisor'}
    errorContext: start_error
    reason: 'unsupported_opcode: build_stacktrace: supplied frame names a module atom the running module cannot resolve: Elixir.Agent.Server'
    offender: [{pid,undefined},
               {id,'Elixir.Mix.TasksServer'},
               {mfargs,{'Elixir.Mix.TasksServer',start_link,[[]]}}, …]
                                                                        # exit 1
$ mix --version                                                         # the oracle, for contrast
Erlang/OTP 29 [erts-17.1] [source] [64-bit] … Mix 1.20.4 (compiled with Erlang/OTP 29)
```

Mix fails on the *same module family*: `Mix.TasksServer` starts an `Agent.Server`, and the error text
names `Elixir.Agent.Server` verbatim. The mechanism is different, though — Mix dies in the
**exception** path (`build_stacktrace` cannot resolve a frame's module atom), not by hanging, so Mix
reports a supervisor `start_error` instead of running to the deadline.
Practical consequence: **use the real Elixir toolchain to build** (`elixirc`, `mix compile` with the
system Erlang), and treat `beam_rt` as the *runtime* you execute the resulting `.beam` files on.
Do not expect to *run the build tool* on it yet.

**(c) Other things to expect**, all **documented** in
[`crates/beam_rt/M1_GAPS.md`](crates/beam_rt/M1_GAPS.md):

* **NIF-based dependencies** work only inside the A.3 envelope: `load_nif/2` really `dlopen`s and
  runs C NIFs (176-slot `enif_*` host API), but there are **no dirty schedulers**
  (`enif_schedule_nif` → `badarg`), the C-variadic `enif_*` entries are absent
  (`enif_make_tuple` etc.; the `_from_array` variants cover building), and `upgrade` is not modelled.
  A NIF that needs any of those fails at load/call time.
* **`application:start/1,2` / `ensure_all_started/1`** do start real applications from their `.app`
  files (see §5) — but the `.app` `env` list is not read (`application:get_env/2` → `undefined`),
  and `logger` reports are accepted but reach no sink.
* **Distribution**: `-sname`/`-name` gives you a real node (`demo1@billmini`), ETF is byte-exact, and
  messages/links/monitors work with a real OTP peer; **`rpc:call/4,5`, `erpc` and
  `spawn(Node,M,F,A)` are capability gaps** (`DOP_SPAWN_REQUEST`/`REPLY` stay unsupported) and a
  remote `{Name,Node}` monitor gets no `'DOWN'`.
* **Hot code loading** is absent in the OTP sense: there is no old-code slot; `code:load_binary/3`
  unloads-and-loads where the oracle retires code.

---

## 5. Running an existing OTP-style project

### 5.1 The mechanics — verified

Put the project's `ebin` on the path with `-pa`; a real `.app` file loads and starts:

```sh
$ cat ebin/myapp.app
{application, myapp,
 [{description, "demo"}, {vsn, "1.0"}, {modules, [myapp]},
  {registered, []}, {applications, [kernel, stdlib]}, {env, [{greeting, "yo"}]},
  {mod, {myapp, []}}]}.

$ beam_rt -noshell -pa /tmp/demo/ebin \
    -eval 'io:format("load=~p~n",[application:load(myapp)]),
           io:format("start=~p~n",[application:start(myapp)]),
           io:format("env=~p~n",[application:get_env(myapp,greeting)])'
load=ok
myapp starting
start=ok
env=undefined

$ beam_rt -noshell -pa /tmp/demo/ebin -eval 'application:ensure_all_started(myapp), io:format("~s~n",[myapp:hello("there")])'
myapp starting
hi there

$ beam_rt -noshell -eval 'io:format("~p~n",[application:ensure_all_started(nosuchapp)])'
{error,{nosuchapp,{"no such file or directory","nosuchapp.app"}}}    # identical on the oracle
```

`application:which_applications/0` reports the real booted set, e.g.
`[{stdlib,"ERTS  CXC 138 10","8.1"},{kernel,"ERTS  CXC 138 10","11.0.4"}]`, and dependency
resolution works for real applications: `application:ensure_all_started(ssl)` →
`{ok,[crypto,asn1,public_key,ssl]}`.

**The gap to know about:** `application:get_env/2,3` does **not** read the `.app` `env` list —
it answers `undefined` (and `get_all_env/1` answers `[]`), where the oracle answers `{ok,true}` /
`{ok,"yo"}`. A library that reads its configuration through `application:get_env/2` will therefore
see `undefined` and must be configured another way.

### 5.2 The native floor (what a project's start-up actually leans on)

A small set of services is served natively, *before* the BIF registry, so a project can boot without
the real Erlang `code_server`/`file_server`/`prim_loader` chain:

| Service | Served |
|---|---|
| `code:*` | `which/1`, `get_object_code/1`, `is_loaded/1`, `load_file/1`, `load_binary/3`, `ensure_loaded/1`, `ensure_modules_loaded/1`, `get_mode/0`, `lib_dir/0,1,2`, `priv_dir/1`, `where_is_file/1`, `all_loaded/0`, `purge/1`, `soft_purge/1`, `add_patha/z/1,2`, `add_path/1,2`, `del_path/1` |
| `init:*` | the automatic `get_argument/1` flags (`root`/`home`/`progname`), `script_id/0`, `boot/1`, `stop/0,1`, `restart/0`, `get_arguments/0` |
| `application:*` | **read-only** queries plus real `load`/`start`/`stop`/`ensure_all_started` for apps found on the code path |
| `file_server_2` | a native service process answering `{write_file,…}`, write-mode `{open,…}`, `{get_until,…}`, `{get_line,…}`, `{get_chars,…}`, `getopts`/`setopts`, `{read_handle_info,_}`, `{list_dir,…}` — everything else is a typed `unsupported` |
| `net_kernel` | only on a `-name`/`-sname` node, and only `{is_auth,_}`; `gen:reply/2` alias routing |
| `io` | the stdio device floor (`put_chars`/`format`/`write`/`nl`/`fwrite`) |
| `prim_file`, `prim_tty`, `erl_ddll`, `erts_internal` | the served sets described in `crates/beam_rt/src/native/CONTEXT.md` |
| `erl_prim_loader` | a stand-in process only — **no`get_file`/`get_path` protocol** |

### 5.3 The honest list, by contract

Summarised from [`crates/beam_rt/M1_GAPS.md`](crates/beam_rt/M1_GAPS.md) (documented, not re-measured
here). Every gap below is a **typed** error/`unsupported`, or an honest own-state difference — the
runtime does not return silently wrong results.

* **A.1 language semantics** — real `erlc`-built code runs: the whole-OTP `.beam` census lowers with
  zero unmodelled opcodes, and the gate's verdict is `total=3218 agree=3217 mismatch=0
  unsupported=1 skipped=0` (§6). Gaps: a `receive … after T` whose delay is `infinity` or not a
  statically positive integer parks; `lists`/`maps` of a non-list raise `badarg` where the oracle
  raises `function_clause`/`{bad_generator,_}`; `math:erf/1`/`erfc/1` are approximate.
* **A.2 `.beam`/asm** — full chunk parse, loader, typed IR. Gap: there is **no old-code slot**, so
  `erlang:delete_module/1` really unloads (`check_old_code/1` stays `false`).
* **A.3 NIF C ABI** — `load_nif/2` + 176 `enif_*` slots, real C NIFs. Gaps: no dirty schedulers, no
  C-variadic `enif_*`, no `upgrade`, a `dtor`-time `enif_send` has no receiver.
* **A.4 driver C ABI** — real C drivers, 32 of 43 `DriverHost` hooks, `driver_async` pool,
  `{packet,N}`/`line` framing, the three static built-in driver names
  (`tcp_inet`/`udp_inet`/`ram_file_drv`). Gaps: a plain `port_command/2` on a **busy** port is a
  typed gap (the oracle suspends and re-issues); a `control` reply answered from the driver's own
  memory; `ignorefd`/`tcp_sendfile` refused.
* **A.5 distribution/ETF** — byte-exact ETF, live handshake, `epmd`, link/monitor/`DOP_SEND`
  families, `net_adm:ping/1` → `pong`. Gaps: `rpc`/`erpc`/`spawn(Node,…)`, `monitor_node/3`, no
  Erlang-level distribution subtree (`whereis(net_sup)` → `undefined`), foreign-pid formatting.
* **A.6 CLI/config** — the surface in §3.1, with `0/1/2/3` exit codes. Gaps: no shell, `-eval` is a
  subset, a few flag-driven `system_info` literals report defaults.
* **A.7 OTP app contracts** — 22 preloaded modules, `erts_internal:*`, `prim_file`/`prim_tty`,
  `erl_ddll`, and the floors above. Gaps: `prim_file`'s `write_file_info/2,3`, `advise/4`,
  `allocate/3`, `read_line/1`; `erl_prim_loader`/`prim_inet`/`prim_socket`/`prim_zip` fall through to
  preloaded bodies whose `-nifs()` entries are `erlang:nif_error(undef)`; `logger` reports reach no
  sink.
* **A.8 introspection/tooling** — `system_info/1` (88 keys + 2 tuple keys), `process_info/1,2`
  (≈28 items), `erlang:memory/0,1`, live process tracing. Gaps: the 27 oracle-accepted
  `system_info` keys naming facilities this runtime lacks raise the typed gap (e.g.
  `system_info(microstate_accounting)` → `beam_rt: error badarg`, exit 1 — verified);
  `statistics/0`, `scheduler_wall_time`, `process_info(P, backtrace|parent|label|…)`, the
  debugger/trace-session surface.
* **A.9 build/bootstrapping** — boot completes (22 directives, exit 0), and in-process compilation
  reproduces the oracle's `.beam` bytes for all **755** oracle-compilable `lib/*/src/*.erl` sources
  (772 swept, 17 fail on the oracle too). Gap: **no self-hosted build** — the runtime does not
  compile/run itself yet.

### 5.4 BIF surface (so you can judge a missing-function report)

`bif.tab` declares **556** entries and **all 556 are registered**; the registry has **667** keys
(556 declared + 111 stdlib fast paths). Of the 556, **369 are served unconditionally in-crate** and
**187 are seam-backed** (a typed `unsupported` until a host installs the hook); `crates/beam_rt`
installs the hooks, and its native floor answers *before* the registry, so a key can be shadowed.
The counts are pinned by `crates/beam_bifs/tests/roster_audit.rs` — re-derive them from there rather
than from prose. A missing function therefore shows up as either `beam_rt: error undef` (not on the
roster) or `beam_rt: unsupported: bif` (on the roster, seam unserved).

---

## 6. Testing and the compatibility gate

The gate is `tools/differential`. It runs a case **twice** — under the system `erl` (the oracle) and
under `beam_rt` — and compares the observable result (stdout, `term_to_binary` image of the value,
exception reason, `system_info`).

### 6.1 `gate.sh` — the CI contract

```sh
$ sh erts_rust/tools/differential/gate.sh --help
usage: gate.sh [--allowlist <file>]
  --allowlist <file>   known-gaps allowlist (default: <script_dir>/known-gaps.txt)

$ sh erts_rust/tools/differential/gate.sh          # builds beam_rt + beam_dist + differential, runs the corpus
```

It defaults the allowlist to `erts_rust/tools/differential/known-gaps.txt` and always passes
`--strict` to the binary. Its exit codes:

| Exit | Meaning |
|---|---|
| `0` | every executed case agreed, or every gap is recorded in the allowlist **and** the allowlist is fresh |
| `1` | a real **mismatch** (the oracle and `beam_rt` disagreed) |
| `2` | environment/usage error, or an unparsable report |
| `3` | no mismatch, but the **ratchet** must be updated — an unrecorded capability gap appeared, or a recorded one closed and went stale |

A **whole serial run is >100 minutes** (each CLI case boots a runtime). Do not run it casually.

### 6.2 The three verdicts

| Verdict | Meaning |
|---|---|
| `agree` | both sides produced the same observable result — the case is covered |
| `mismatch` | they disagreed — **always a defect**, never tolerated by the allowlist |
| `unsupported` | `beam_rt` exited **3** (typed capability gap) — tolerated only if recorded |

The corpus is **3218 cases** and the committed record is `agree=3217 mismatch=0 unsupported=1`.
The single recorded gap is `wire/new_fun_ext_encode` (A.5): an `erl_eval` lambda's closure identity
needs `erl_eval`/`erl_scan`/`erl_parse`, and it is deliberately left unclosable.

### 6.3 Running one case — verified

```sh
$ erts_rust/scripts/cargo.sh build --release --manifest-path erts_rust/Cargo.toml -p differential
$ erts_rust/target/release/differential list 2>/dev/null | wc -l
3218
$ erts_rust/target/release/differential run etf/binary_to_term_roundtrip
[AGREE] etf/binary_to_term_roundtrip (A.2)
    binary_to_term/1 inverts term_to_binary/1 for a mixed term
    oracle: exit 0 golden=ok stdout="ok {ok,[1,2,3],<<120>>}\n"
    rust  : exit 0 golden=not-asserted stdout="ok {ok,[1,2,3],<<120>>}\n"
                                                                        # exit 0, ~1.4 s
$ erts_rust/target/release/differential run wire/new_fun_ext_encode
[UNSUPPORTED] wire/new_fun_ext_encode (A.5)
    …
    rust  : unsupported: beam_rt: unsupported: local_fun                        # exit 0
```

Useful companions: `differential list --by-contract` (per-contract split), `differential compare
'EXPR'` (ad-hoc), `differential suite --strict --allowlist known-gaps.txt` (the binary's own suite).
`run` accepts `--out-file <path>`, which is how the fast route works: one `differential run`
per id at 16-way parallelism, then `differential merge --from <dir> --allowlist known-gaps.txt`
(seconds, no re-run). Two caveats from the harness docs: run the sweep with **cwd = repository
root** (the `selfhost/` expression cases compile a relative `lib/stdlib/src/*.erl`), and give each
case its own private `TMPDIR` and a `setsid -w` session.

The oracle is `$ERTS_ORACLE` if set, else `erl` on `PATH`; missing ⇒ a loud environment error
(exit 2). CI (`.github/workflows/erts_rust.yml`) runs this gate on any `erts_rust/**` change.

### 6.4 Running the workspace tests

```sh
erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml --workspace --no-fail-fast
```

The suite is large (~10 800 tests, tens of minutes on a loaded host) — run it per crate while
iterating (`-p beam_engine`, `-p beam_rt`, …). Oracle-backed tests **skip loudly** when `erl` is
unavailable rather than silently passing.

---

## 7. Troubleshooting

### 7.1 The two load-sensitive failure shapes

**(a) Boot deadline** — exit **1**, empty stdout, when the host is too loaded to finish the boot
script inside the `DRIVE_DEADLINE` (10 s, `crates/beam_rt/src/boot/drive.rs:85`):

```
beam_rt: error boot script: the scheduler was still runnable after 10000 ms
```

It is a *host* symptom, not a runtime defect: 30 CPU hogs on one core still finish (8.9 s, exit 0),
60 hogs do not (13.3 s, exit 1). Remedy: run big sweeps serially on an idle host, or accept the
retry.

**(b) Slice-boundary block in the interpreter** — exit **3**:

```
beam_rt: unsupported: opcode: run: process blocked (no message available)
```

A `pump_once` slice boundary (`crates/beam_rt/src/context/pump.rs`) — a scheduled round trip that
cannot be pumped inside one slice. Seen under heavy contention (and on an injected `charge()`
refusal). Remedy: re-run; it is contention, not the code path you are testing.

### 7.2 The deadlines (none is overridable from the CLI)

| Constant | Value | Where it bites |
|---|---|---|
| `RUN_DEADLINE` | **60 s** | the `run` action: `beam_rt: error run: <m>:<f>/<a> did not terminate within 60000 ms` (`crates/beam_rt/src/runtime/scheduler.rs:90`) |
| `DRIVE_DEADLINE` | 10 s | each boot-script `execute` pass (`crates/beam_rt/src/boot/drive.rs:85`) |
| `BLOCKING_PUMP_DEADLINE` | 900 s | nested blocking runs, e.g. an in-process `compile:file/2` (`crates/beam_rt/src/context/pump.rs:97`) |

A workload that legitimately needs longer than 60 s cannot be run through `run`; drive it from a
compiled module under `-eval` (or raise the loop count down until it fits).

### 7.3 Choosing the execution tier

The default is the native **baseline JIT** tier, compiled lazily per function at first entry.
`BEAM_RT_TIER=interpreter` is the only kill switch and is inherited by every child process:

```sh
$ BEAM_RT_TIER=interpreter erts_rust/target/release/beam_rt run foo.beam foo:f/1 1
```

The baseline tier is cheaper than the interpreter on every workload the bench can measure at its
default calibration; the interpreter is worth selecting when you want the simplest semantics (e.g.
to decide whether a failure is a JIT-tier issue) or a faster cold start for a one-shot call
(lazy per-function compilation costs ≈1.2–1.25× over the interpreter at start-up).
The Cranelift hot tier exists but is **default-OFF and not on `beam_rt`'s path** — it is not a
performance option (see [`BOTTLENECKS.md`](BOTTLENECKS.md) §4).

### 7.4 Common misreadings

* `-eval` prints nothing — use `io:format/2` inside the expression if you want output.
* Without `-noshell` you get the booted kernel's slogan (`Erlang/OTP 30 [erts-17.1] [source] [64-bit]`)
  on stdout before your own output; the `run` action sets `-noshell` for itself, so it is banner-free.
* `-s` passes **atoms**, `-run` passes **char lists** — `-run mod f 2 3` gives you `"2"` and `"3"`.
* A `run` argument is an integer, an atom or a list — **not** a float and **not** a binary.
* `beam_rt: error undef` after adding a directory usually means the module is not on the path: check
  `-pa`, or `code:which(Mod)`.

---

## 8. Where to go next

* [`CONTEXT.md`](CONTEXT.md) — the workspace contracts, crate routing table and current status.
* [`crates/CONTEXT.md`](crates/CONTEXT.md) — the crate-layer contracts and per-subsystem status.
* [`crates/beam_rt/M1_GAPS.md`](crates/beam_rt/M1_GAPS.md) — the full, itemised gap list.
* [`tools/differential/CONTEXT.md`](tools/differential/CONTEXT.md) — the gate, the corpus, the ratchet.
* [`tools/bench/RESULTS.md`](tools/bench/RESULTS.md) — the measured record; [`BOTTLENECKS.md`](BOTTLENECKS.md) —
  what is still slow, why, and what has already been tried.
