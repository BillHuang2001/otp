# A.3 NIF host surface — measured oracle ground truth and the shipped host surface

The three A.3 (NIF C ABI) host-surface paths this document covers:

1. `enif_schedule_nif`'s continuation.
2. `enif_alloc_env()` off the NIF-call path, and `enif_send` from a NIF-created thread.
3. A resource type's `stop` callback and the `enif_select` STOP verdicts.

Each section records the **oracle ground truth** — measured from a real C NIF fixture against OTP 29.1.1 / erts-17.1 — then what the Rust runtime does, naming the implementing symbol and, where it substitutes or refuses, the symbol and verdict that limit it.
Nothing here is a work item: a section's last subsection lists only gaps that are live in the code.

The **ABI half** is `../beam_abi`: `src/nif/enif/schedule.rs` (trampoline), `src/nif/thread_env.rs` (factory seam), `src/nif/enif/{resources_stop,stop}.rs` (the `stop` call + the parking lot) dispatched from `src/nif/enif/select.rs`.
The **runtime host half** is `../beam_rt`: `src/nif/thread_env.rs` + `src/runtime.rs::drain_nif_thread_sends`, and the select store `src/nif_select.rs`.

**Why this document lives in `beam_bifs`:** the implementation sites are in `beam_abi`/`beam_rt`, but the *ground truth* is this node's — three **real C NIF fixtures** compiled with `cc` and run against the live oracle, plus the two test files that replay the measured transcripts and assert the ABI against them.
The measurements are recorded facts, not predictions: several contradict the obvious port of the C.

Fixtures (reuse them; do not rewrite them):

| fixture | probes | driver |
|---|---|---|
| `tests/fixtures/a3_schedule.c` | §1 continuation/argv/raise/refusal/thread-type | `.erl` beside it |
| `tests/fixtures/a3_thread_env.c` | §2 cross-thread env + send + atom round-trip | `.erl` beside it |
| `tests/fixtures/a3_resource_stop.c` | §3 stop callback + every select-stop verdict | `.erl` beside it |

Build/run (the Nix *include path* trap: `code:lib_dir(erts,include)` answers a directory that does **not** exist on this host; the real headers are at `<erts_root>/include`, equivalently `<erts_root>/usr/include` — `tests/nif_a3_oracle_replay.rs::erl_include_dir` walks that order and accepts a directory only when `erl_nif.h` is in it):

```
INC=$(erl -noshell -eval 'io:format("~s",[code:lib_dir(erts,include)]),halt().')
# ...only if "$INC/erl_nif.h" exists; else $(dirname $(dirname $(readlink -f $(which erl))))/include
cc -fPIC -shared -O2 -Wall -Wextra -I"$INC" -o "$TMPDIR/a3_schedule.so" tests/fixtures/a3_schedule.c
cp tests/fixtures/a3_schedule.erl "$TMPDIR" && erlc -o "$TMPDIR" "$TMPDIR/a3_schedule.erl"
erl -noshell -pa "$TMPDIR" -eval 'a3_schedule:...' -s init stop
```

---

## §1 `enif_schedule_nif` — the success path

### What the oracle does (measured, `a3_schedule`)
The transcript asserted by `tests/nif_a3_oracle_replay.rs::the_a3_schedule_fixture_replays_the_measured_oracle_transcript` (one probe per line, a raise captured as `{Class,Reason}`):

```text
sum={ok,{scheduled,5}}
ret={ok,0}
loop0={ok,{done,1}}          loop1={ok,{done,2}}        loop3={ok,{done,4}}
loop100000={ok,{done,100001}}
stack_const=true             (process_info(self(),stack_size) unchanged: 35 -> 35)
raise={raised,error,boom}    (class `error`, reason `boom`)
badflags={raised,error,badarg}
toomany={raised,error,badarg}
dirty={ok,{dirty,7}}
ttype0={ok,{threads,1,1,0}}  ttype1={ok,{threads,1,2,1}}  ttype2={ok,{threads,1,2,2}}
ghost={ok,{ghost,99}}
```

Readings: a continuation's own value IS the call's result (`sum`); the raw word a scheduling call receives is `0` — `THE_NON_VALUE` (`erl_nif.c:399`), on *every* path including the refusals; a 100 000-entry self-rescheduling chain completes at constant stack; `sched_thread_types/1`'s slot 2 is the *continuation's* `enif_thread_type()` — `1` (normal) for `flags = 0`, `2` (dirty CPU) for `flags = 1`/`2` — while slot 1 (the caller) is always `1`; a `fun_name` naming nothing still works (`ghost`: interned, never resolved).

Two further measured facts: `process_info(P, current_function)` is `{Module, fun_name, argc}` from the schedule while the chain runs (measured `{a3_schedule,cont_loop,1}`, `status = running` — contract A.8); and a raise inside a continuation is attributed to the **scheduling NIF** with the original MFA and argv (measured `{a3_schedule,sched_raise,[],[]}`, and `{a3_schedule,sched_reargv,[4242],[]}` for a continuation that rescheduled with a different argv — `execute_nif` sets `EXF_RESTORE_NFUNC`, `erl_nif.c:3534-3541`, restored by `handle_error` from `nep->argv`/`nep->mfa`, `erl_nfunc_sched.c:106-134`).

### How it is implemented
`src/nif/enif/schedule.rs` (`Continuation`, `Outcome`, `run`) owns a scheduled call's whole lifetime: it runs the continuation, follows a self-rescheduling chain to its end, and hands the final value back to the NIF call that started it.
`enif_schedule_nif` (`src/nif/enif/process.rs`) validates in the C's order — `argc > MAX_ARG` (255, `erl_vm.h:48`) first (`erl_nif.c:3569`), then `argc < 0`, then a `flags` bit outside `ERL_NIF_DIRTY_JOB_CPU_BOUND|ERL_NIF_DIRTY_JOB_IO_BOUND` (`erl_nif.c:3601-3603`), then a null/non-UTF-8 `fun_name`, a null `fptr`, a null `argv` with `argc > 0`, a null `env` — and then calls `schedule::run(env, flags, fptr, argc, argv)`.

* **Iterative, constant Rust stack.** A thread-local pending-continuation slot plus a nesting counter (`PENDING`/`DEPTH`): the **outermost** `run` owns the loop and never recurses; a **nested** `enif_schedule_nif` records its `Continuation` and answers the C's `THE_NON_VALUE` (`0`). Rust stack depth is one continuation call however long the chain is — which keeps `sched_loop(100000)`'s `stack_size` unchanged.
* **`argv` uncopied outermost, copied when nested.** The outermost schedule carries the calling NIF's `argv` pointer (alive for the whole chain); a nested one names a local array of the continuation's dying frame, so `Continuation::own_args` takes the C's copy (`erl_nfunc_sched.c:126-130`).
* **Three-way outcome, not a term sentinel.** In C "rescheduled" and "raised" are both `THE_NON_VALUE`, told apart only by `proc->freason` (`execute_nif`, `erl_nif.c:3527-3541`); here `enum Outcome { Value, Reschedule(Continuation), Raised }`.
* `NifHost::nif_yield_point(flags)` is called once per trampoline iteration (see the fairness gap); the `NifHost::schedule_nif` hook is kept only for source compatibility and consulted by nothing (`src/nif/host.rs`).
* **A raise ends the chain, leaves the reason on the env, fabricates no stacktrace head** — the host's error path attributes it to the NIF call in progress. A refusal is a raise, not a return code: the C raises (`enif_make_badarg` → `enif_raise_exception` → `BIF_ERROR`, `erl_nif.c:1791-1801` + `bif.h:227-230`), so `beam_abi` returns the `badarg` term and the host raises it — the same observable.
* **`fun_name` is interned-and-ignored** (measured, `ghost`). The loader-side constraint the fixture hit is separate and real: every `ErlNifFunc` row, continuations included, must name an **exported** function of the `.erl` module (`erl_nif.c:4874-4878`, `{error,{bad_lib,"Function not found M:F/A"}}`).

### What is not reproduced
* **The outermost schedule's raw return word.** The emulator's trap re-run is folded into that call, so it answers the chain's final value (a raise: the pending reason term; `non_value_term()` is `0`) where the C always answers `THE_NON_VALUE`. Only a raw-word probe sees it: `sched_last_ret()` answers `0` on the oracle, and the fixture's `last_ret_is_the_non_value` flag is `false` here. A nested schedule answers the C's `0` exactly; the Erlang-visible result is identical on every path the fixtures drive.
* **Fairness.** `NifHost::nif_yield_point` is a documented no-op in `beam_rt` (`crates/beam_rt/src/nif.rs`): a chain keeps the engine thread until it ends, where the oracle preempts it (with `+S 1:1`, an unrelated process ran 3 000 033 `receive … after 0` iterations while a 3 000 000-entry chain ran). Three reasons it cannot close at this seam: the chain runs inside one `call_ext` dispatch and must hand a term back to the instruction that made the call; `NifHost::consume_timeslice` is itself a documented default (the slice's `BudgetProbe` owns the reduction budget); a chain's env and `argv` point into the C call frames it runs in, so there is no "resume later" to re-enter.
* **Dirty-scheduler thread identity.** No dirty pool exists (`system_info` reports `dirty_scheduler_support: 0`, `crates/beam_rt/src/nif.rs`), so a dirty-flagged continuation runs on the calling thread. Value, `argv` and env semantics are the oracle's; what *is* reproduced is the observable the fixture probes — `enif_thread_type()` answers `2` (`ERL_NIF_THR_DIRTY_CPU_SCHEDULER`) / `3` (`_IO_`) for that continuation's duration, restored afterwards, both bits set reporting the CPU type.
* **`enif_thread_type()` of the calling thread** is `0` (`ERL_NIF_THR_UNDEFINED`) here vs `1` (`…_NORMAL_SCHEDULER`) on the oracle (scheduler thread vs this runtime's own driver thread); likewise a **non-dirty** continuation, entered inline on that driver thread (`ttype0`). The dirty continuation agrees (`ttype1`) because its type is synthesized.

---

## §2 `enif_alloc_env` off the NIF-call path (+ `enif_send` from a NIF-created thread)

### What the oracle does (measured, `a3_thread_env`, identical across 3 runs)
The transcript asserted by `tests/nif_a3_oracle_replay.rs::the_a3_thread_env_fixture_replays_the_measured_oracle_transcript`:

```text
thread_send={{0,1,0},0}          {EnvIsNull, SendCode, ThreadType} ++ [ThreadCreated]
thread_send_queue=1              the send landed before the NIF call returned
{thread_msg,{from_thread,42,<<"abc">>}}
worker_atoms={0,1,1,1,0}         {EnvIsNull, AtomLenOk, AtomTextOk, SendCode, ThreadType}
{worker_atom_msg,{worker_atom_check,worker_atom}}
main_env={0,1}                   {EnvIsNull, SendCode}
{call_path_msg,{from_call,42}}
null_env={0,1,1}                 {EnvIsNull, SendCode, ThreadType} with a NULL `env` argument
{null_env_msg,{from_call_null,42}}
parked_status={status,waiting}    the receiver really is parked before the send
thread_send_to={{0,1,0},0}
{parked_msg,{parked_got,{from_thread_to_target,42}}}
```

Readings: on an `enif_thread_create`d thread `enif_alloc_env()` answers a **non-NULL** env (`EnvIsNull = 0`) whose atoms live in the ONE global atom table (`worker_atoms` slots 1/2 are both `1`); `enif_send(NULL, …)` answers `1` from there; `enif_thread_type()` there is `0` (`ERL_NIF_THR_UNDEFINED`) where the call-path control reports the scheduler thread's `1`; and that send even wakes a receiver parked in `receive` (`parked_msg`).

### What the C guarantees
* `enif_alloc_env` touches **no** thread state (`erts_alloc_fnf` + `setup_nif_env(msg_env, NULL, NULL)`, `erl_nif.c:636-680`): a process-independent env whose heap *and* the one VM-global atom table are the runtime's. A thread-local "am I inside a call?" lookup is the wrong shape for it.
* The message env's heap is **transferred, not copied** (`erts_create_message_from_nif_env` splices the fragment chain and off-heap list into the `ErtsMessage` and nulls the env's, `erl_nif.c:812-841`, used at `:960`): a `msg_env` send is zero-copy, the term is unusable after the send, one env per message is the idiom.
* Delivery is **synchronous inside `enif_send`** and **wakes a parked receiver** (`erl_nif.c:1006`, `:876-877`) — through the receiver's real queue + resume-on-send path, never a deferred flush.
* With no calling process the sender is `undefined`, not a fabricated pid (`erl_nif.c:996`).

### How it is implemented
`beam_abi`'s `src/nif/thread_env.rs` owns the reachability seam — `pub trait ThreadEnvHostFactory: Send + Sync + 'static { fn open(&self) -> Option<Box<dyn NifHost>>; }`, `install_thread_env_factory(…) -> bool`, `clear_thread_env_factory`, `with_thread_env_host` — in a `Mutex<Option<…>>` (last install wins; a poisoned lock answers `false`/`None`, never a panic) with a global `GENERATION` stamp and a per-thread cache (`NifHost` is `!Send`, so each thread opens its own).
`src/nif/enif/envs.rs::enif_alloc_env` walks it in order: (1) the host of the NIF call this thread is inside (published by `NifEnv::new`, withdrawn by its `Drop`) — the call-path answer; (2) else the host the installed factory opened for **this thread**, stamped with the generation so a host from a superseded factory is dropped rather than reused; (3) else **`NULL`** — deliberately kept, because a crate-owned env with a private atom table would hand a NIF a wrong atom *index*, i.e. a wrong term delivered to another process.
`enif_send` resolves its host from `env` when it is a live env, **else from `msg_env`** (the measured `enif_send(NULL, &pid, msg_env, msg)` idiom); `enif_free_env` frees through the env's own host, needing no factory step.

The runtime half is `../beam_rt/src/nif/thread_env.rs`: `ThreadEnvFactory` (installed from `cli::run`, cleared on the one graceful exit) opens one `ForeignHost` per thread and owns `ForeignAtomHub` (a private append-only name→index space shared by every host it opens) plus `ForeignSends = Arc<Mutex<Vec<ForeignSend>>>`.
`ForeignHost::send` encodes the message as a complete ETF image **by atom name** against the hub's scratch table, pushes `ForeignSend { target, from, image }`, and answers the C's `1`.
`Runtime::drain_nif_thread_sends` runs at the next pump (`context/calls.rs::try_call_nif` after the call's services drop, and `runtime/scheduler.rs`'s scheduler pass), decodes each image against the live table (installing the same local-node identity, so a message carrying one of this node's pids/ports/references still decodes to the internal id), enqueues it in the target's mailbox through the runtime's own send seam, and wakes a parked receiver.
The atom space is private because reserving indices inside the live space re-anchors when the live table grows around an outstanding handle, so an index a term already carried can end up naming a different atom; an ETF image crosses the boundary by name instead.

### What is not reproduced
* **A queued image, not a synchronous delivery.** `enif_send` answers the C's `1` at call time, but the message reaches the mailbox (and wakes a parked receiver) only at the runtime's next pump — `ForeignHost::send` + `Runtime::drain_nif_thread_sends`. The observable ordering `thread_send_queue=1` is therefore not reproduced, and no term is shared across hosts: the receiver's heap, mailbox and process table belong to a thread this host does not run on.
* **`ForeignHost` typed gaps** — each a typed `None`/`false`, never a wrong term (module doc of `crates/beam_rt/src/nif/thread_env.rs`):
  * `NifHost::self_pid` is `None` (no calling process — the C's `phy_proc` with `ERTS_INVALID_PID`), so `enif_self`/`enif_is_current_process_alive` answer their failure codes and `from: None` means the C's `am_undefined`.
  * `NifHost::atom_table` stays at the trait default `None`, so env-less `enif_compare`/`enif_is_identical`/`enif_hash` order a *nested* atom by **raw index** (`beam_term::compare`'s documented fallback); `enif_make_existing_atom` answers within the private space only.
  * `select`, monitors, liveness, `whereis_pid`, `make_ref`, `unique_integer`, `system_info`, `term_to_binary`/`binary_to_term`, `port_command`, `priv_data`, `dynamic_resource_call`/`consume_timeslice`/`nif_schedule` keep their trait defaults — each needs the process table, poller, reference space or code server this host does not hold.
  * A **remote** destination answers the C's failure code `0` where the C delivers and answers `1`: `NifHost::send` resolves through `ProcessId::from_eterm`, which accepts only an internal local pid, and no `beam_dist` route is on the host.
  * A message carrying a local (`make_fun3`) closure is unencodable (its `NEW_FUN_EXT` identity lives in the loaded module), so the send is the ABI's `0` rather than an image with an invented identity — same for any other tag the codec refuses.
  * The thread heap is a fixed-capacity bump arena **never rewound** (`THREAD_ENV_HEAP_WORDS = 1 << 20` words): recycling waits on `enif_free_env`'s lifetime rule, so a thread that builds messages forever exhausts its heap and gets typed allocation failures from the `enif_make_*` family.
  * A NIF must not keep an env (or a term built on it) across a factory reinstall — the generation stamp drops the superseded host on that thread's next use.
* **`enif_thread_type()` of the calling thread** in the `null_env` probe is `0` (`undefined`) here vs `1` (`normal`) on the oracle — the §1 driver-thread substitution; `src/nif/enif/threads.rs` folds `ERL_NIF_THR_OTHER` into `UNDEFINED`.

---

## §3 The resource `stop` callback and the select-stop verdicts

### Ground truth
`erts_resource_stop(resource, e, is_direct_call)` (`erl_nif.c:3032-3040`) builds a **nooproc message env** and calls `type->fn.stop(&env, resource->data, e, is_direct_call)`.
It is called from `enif_select`'s STOP arm (`erts/emulator/sys/common/erl_check_io.c:1531`, `is_direct_call = 1`) or from the poller thread (`:2241`, `is_direct_call = 0`).
The returned code's low bits are the verdict: `ERL_NIF_SELECT_STOP_CALLED` = 1 << 0, `ERL_NIF_SELECT_STOP_SCHEDULED` = 1 << 1.

### What the oracle does (measured, `a3_resource_stop`; stable slots of 3 runs)
Asserted by `tests/nif_a3_oracle_replay.rs::the_a3_resource_stop_fixture_replays_the_measured_oracle_verdicts` (`Code` bit 0 = `STOP_CALLED`, bit 1 = `STOP_SCHEDULED`, bit 4 = `READ_CANCELLED`; `Direct` = the callback's `is_direct_call`, `Thr` = `enif_thread_type()` inside it):

```text
stop_unknown_fd   {1,1,1,17,1}     Code 1 = STOP_CALLED, 1 call, direct 1, the pipe fd, thread 1
arm_then_stop     {0,17,1,1,1,2}   Stop 17 = STOP_CALLED|READ_CANCELLED, 1 call, direct 1, thread 1
double_stop       {0,17,1,2,1,1,3} the SECOND STOP answers 1 and runs the callback a 2nd time
cancel_then_stop  {0,16,1,1,1,1,4} Cancel 16 = READ_CANCELLED, then Stop 1 = STOP_CALLED
arm_then_stop_fallback  {0,18,…} or {0,2,…}  the STOP carries STOP_SCHEDULED (2); at return the
                   callback may (2) or may not (18, plus READ_CANCELLED = 16) have run
stop_fallback_wait_ms(1000)  {0,18,0,1,0,0,8,901,…}  code 18, callback deferred to the poller thread:
                   after the wait it ran once with direct = 0 and thread type 0 (ERL_NIF_THR_UNDEFINED)
read_on_pending_stop  ReadOnPending = 2 (STOP_SCHEDULED) — an EXPLICIT race, see below
```

Replay literals: `unknown_fd={1,1,1,1}`, `arm_then_stop={0,17,1,1,1,2}`, `double_stop={0,17,1,2,1,1,3}`, `cancel_then_stop={0,16,1,1,1,1,4}`, `fallback_arm={0,true,true}` (bit 1 set, bit 0 clear), `fallback_wait={0,true,true,1,0,0,8,901,true,true}` (arm, scheduled, not-called, 1 call after the wait, direct 0, thread 0, payload tag 8, fd 901, waited, latency observed).

Readings: a STOP on an fd never armed takes the C's **fast track** (`:1278-1285`) — `CALL_STOP`, code `1`, synchronous with `is_direct_call = 1` on a scheduler thread, and the **caller keeps** its reference; an armed READ + STOP on an epoll-able fd is `CALL_STOP_AND_RELEASE` (17) with the runtime releasing the select's own reference; a STOP on an fd whose kernel poll refused it (`/dev/null`, a regular file, a directory: `epoll_ctl(ADD)` → `EPERM`) is `STOP_SCHEDULED|READ_CANCELLED` (18), the callback running **later on the poller thread** with `is_direct_call = 0` and thread type `0` (measured latency 2.4 µs – 1.6 ms), the poller releasing the reference; a repeated STOP runs the callback a **second** time (the first STOP cleared the event state, so the second takes the unknown-fd fast track); a READ on an fd whose stop is still pending is steered through `steal_pending_stop_nif` and then unconditionally answered `ERL_NIF_SELECT_STOP_SCHEDULED` (`:1331-1337`), polluting even a READ's return.

Two structural facts behind those verdicts. `STOP_SCHEDULED` needs `wake_poller` set, which the epoll path's concurrent update never does (`concurrent_update_pollset` takes no `do_wake` out-parameter, `erts/emulator/sys/common/erl_poll.c:760-870`, under `ERTS_POLL_USE_CONCURRENT_UPDATE`), while the non-concurrent `poll_control` — the select/poll **fallback** driver — sets it on every op (`:1388-1412`, waking through `wake_poller`, `:429`). So a pipe's or socket's STOP is always synchronous, exactly as measured, and only an fd that landed in the fallback pollset defers. And a type **without** a `stop` callback crashes the oracle (SIGSEGV, exit 139, 3/3 measured runs): `ASSERT(resource->type->fn.stop)` (`:1279`) is compiled out in a release build and `erts_resource_stop` calls a NULL pointer, so the fixture's `probe_stop_nostop_type()` is **never** run by either test.

### How it is implemented
`src/nif/enif/resources_stop.rs` holds the invocation seam — `resource_type_has_stop(obj)` and `call_resource_stop(obj, event, is_direct_call)` — both answering `false`, never a fabricated success, for a null handle, an object this crate did not allocate, or a type opened without `stop`.
`src/nif/enif/select.rs::service_stop_verdict` dispatches the host's answer **after** `NifHost::select` returns, leaving the code itself answered unchanged:

* `code & ERL_NIF_SELECT_STOP_CALLED` ⇒ `call_resource_stop(obj, e, true)`, synchronous with `is_direct_call = 1`; this crate takes and releases no reference on that verdict (the caller keeps its own on the fast track, the runtime already released on `CALL_STOP_AND_RELEASE`).
* `code & ERL_NIF_SELECT_STOP_SCHEDULED` ⇒ `record_pending_stop(obj, e)` (`src/nif/enif/stop.rs`): one `enif_keep_resource` reference (`:1509`) plus an insertion-ordered entry in a `Mutex<PendingStops>`, drained by `run_pending_stops()`, which calls the callback with `is_direct_call = 0` (`:2241`) and then releases the kept reference (`:2242`). A second STOP for the same `(obj, e)` **steals** the entry rather than double-arming, the displaced reference released outside the lock (`steal_pending_stop_nif`, `:1334`).
* anything else — a plain `0`, the `*_CANCELLED` bits, a negative refusal ⇒ nothing runs, nothing is kept, nothing is released.
* a `stop`-less type stays a **deliberate typed no-op** (the host's code is answered, the call skipped) because the oracle SIGSEGVs there; `resource_type_has_stop` is the query.

Reference accounting is per verdict and is what the fixture pins: `CALL_STOP` ⇒ the caller still owns its reference; `CALL_STOP_AND_RELEASE` ⇒ the runtime already released; `STOP_SCHEDULED` ⇒ the drain releases — the fixture's balanced 8 objects → 8 dtors across all four verdicts.

### What is not reproduced
* **`STOP_SCHEDULED` is unreachable from `beam_rt`.** `PortRegistry::stop_nif_select` de-registers the descriptor immediately (`crates/beam_rt/src/nif_select.rs::select`), so every STOP the runtime observes is `ERL_NIF_SELECT_STOP_CALLED | <cancelled bits>` — which, per the measurement above, is also the only verdict Linux/epoll can produce for an epoll-able fd. `beam_abi`'s deferred arm therefore has no caller there: `run_pending_stops()` has nothing to run and `pending_stop_count()` stays `0`. It is exercised only by a host that really defers (and by `tests/nif_a3_abi_assertions/select_stop.rs`, which drives a host answering each verdict in turn).
* **A parked stop's thread identity.** A parked stop runs on whichever thread calls the drain (a host with a real reactor calls it from its own poll loop, where the C's poller thread sits): the identity can differ, the operands never do.
* **`enif_thread_type()` inside the `stop` callback** is `0` (`ERL_NIF_THR_UNDEFINED`) here vs `1` (`…_NORMAL_SCHEDULER`) on the oracle; the callback still runs synchronously, which the `is_direct_call = 1` slot proves. Same for the callback's thread type in every other verdict the `beam_rt` cross-check drives.

---

## §4 What this node provides

- The three fixtures + `.erl` drivers, oracle-measured (transcripts above) — the ground truth both test files compare against.
- `tests/nif_a3_oracle_replay.rs` — **3 oracle replays**, one per fixture: compile the `.c` with `cc -fPIC -shared -O2 -Wall -Wextra -I<erts include>`, `erlc` the `.erl`, run under the live system `erl` (`$ERTS_ORACLE`, else `erl` on `$PATH`), assert the recorded OTP 29.1.1 / erts-17.1 transcript. Each `#[test]` skips independently with a loud `SKIP:` banner naming the missing piece (`cc`/`erlc`/`erl`/`erl_nif.h`) — never a silent pass.
- `tests/nif_a3_abi_assertions.rs` — one test binary, three `#[path]` modules (`tests/nif_a3_abi_assertions/{schedule,thread_env,select_stop}.rs`) to respect the ~1000-line rule. `beam_abi`'s `enif_*` entry points are plain `#[no_mangle] pub unsafe extern "C" fn`s, so the ABI contract is asserted with **no** `dlopen`, `.so` or oracle:
  - `enif_schedule_nif_runs_its_continuation_to_completion` (§1) — runs once with the caller's uncopied `argv`, its value is the call's result, a nested schedule answers `THE_NON_VALUE`, a long chain stays constant-stack, one yield point per iteration, a raise ends the chain with the reason left on the env, dirty flags pick the reported thread type, the C's refusals run nothing.
  - `enif_alloc_env_off_the_call_path_needs_a_factory_and_then_works` (§2) — `NULL` with no call host and no factory, a real env once a factory is installed, the per-thread generation-stamped cache, and `enif_send` taking its host from `env` else `msg_env`, with `enif_free_env` handing the env back to its own host.
  - `enif_select_stop_verdicts_run_the_right_callback` (§3) — `STOP_CALLED` runs `stop` synchronously with `is_direct_call = 1`, `STOP_SCHEDULED` parks and `run_pending_stops()` runs it with `is_direct_call = 0` after releasing the keep, anything else runs nothing, a `stop`-less type is a no-op, a second STOP steals rather than double-arms.
- The replay test's doc comments carry the transcript record itself (stable fields only: the deferred-stop latency and `CallsAtReturn` slots vary with host load, and READ-on-pending-stop is a genuine race resolved by a bounded retry over fresh VMs — every observation must be `0` or `2`, and at least one must be `2`).

## §5 End-to-end cross-checks and corpus coverage

`../beam_rt/tests/nif_a3_{schedule,thread_env,resource_stop}.rs` drive the **same** `.so` and `.beam` under both runtimes and compare observation sets slot by slot (harness `tests/support/nif_a3.rs`): **35 observations — 25 agree exactly, 10 are documented divergences**, each row either asserting agreement or reproducing its recorded divergence, so neither half drifts silently.
The divergences are exactly the substitutions named above: `last_ret_is_the_non_value` (the raw word), `ttype0` (a non-dirty continuation entered inline), and `ttype0_caller_thread` / `ttype1_caller_thread` / `null_env_caller_thread` / `unknown_fd_callback_thread` / `arm_then_stop_callback_thread` / `report_after_arm_callback_thread` / `double_stop_callback_thread` / `cancel_then_stop_callback_thread` (all `enif_thread_type()` = `undefined` on a driver thread vs `normal` on a scheduler thread).

`../tools/differential` carries **no** `nif_a3/*` case ids; these three paths are pinned by the suites above.
Each `.erl` driver already prints one line per probe, so such a case could compare that line verbatim through a single CLI process (no distribution needed) — but it needs `cc` + `erl_nif.h` and would have to skip loudly without the toolchain, as the existing NIF cases do.

## §6 Test placement
The two test files **stay in this crate**: they are cross-crate assertions at the `beam_abi` ABI boundary, and the fixtures they rest on are this node's ground-truth artifacts. `nif_a3_oracle_replay.rs` owns the compile-and-run oracle replays; `nif_a3_abi_assertions.rs` is the in-process binary whose three per-section modules are the assertions, split only for the file-size rule (a single file would run ~1500 lines). Any `beam_abi`-internal tests are that node's call; the end-to-end cases live in `crates/beam_rt/tests/` (§5), where the runtime's own NIF tests already own the load/call harness.
