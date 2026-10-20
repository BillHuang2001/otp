# A.3 NIF host surface — oracle ground truth and resolved design (`beam_abi` / `beam_rt`)

The three A.3 (NIF C ABI) host-surface paths this document covers are:

1. `enif_schedule_nif`'s continuation.
2. `enif_alloc_env()` off the NIF-call path, and `enif_send` from a NIF-created thread.
3. A resource type's `stop` callback and the `enif_select` STOP verdicts.

What follows is the **oracle ground truth** for all three — measured from three real C NIF fixtures
against OTP 29.1.1 / erts-17.1 — and the design that was resolved from it. The **ABI half is
implemented** in `../beam_abi`: the continuation trampoline in `src/nif/enif/schedule.rs`, the
thread-env host factory in `src/nif/thread_env.rs`, and the `stop` callback + deferred-stop parking lot
in `src/nif/enif/{resources_stop,stop}.rs`, dispatched from `src/nif/enif/select.rs`. The **runtime
host half** — a factory host over the live `AtomTable` and a thread-safe process-table send (§2 item 4)
— is `../beam_rt`'s.

**Why this document lives in `beam_bifs`:** the implementation sites are in `crates/beam_abi` and
`crates/beam_rt`, but the *ground truth* is this node's: three **real C NIF fixtures**, each compiled
with `cc` and **run against the live oracle** (OTP 29.1.1 / erts-17.1), plus the two test files that
replay the measured transcripts and assert the ABI against them. The measurements below are recorded
facts from those runs, not predictions: several of them contradict the obvious port of the C, so read
them before changing code.

Fixtures (reuse them; do not rewrite them):

| fixture | probes | driver |
|---|---|---|
| `tests/fixtures/a3_schedule.c` | §1 continuation/argv/raise/refusal/thread-type | `.erl` beside it |
| `tests/fixtures/a3_thread_env.c` | §2 cross-thread env + send + atom round-trip | `.erl` beside it |
| `tests/fixtures/a3_resource_stop.c` | §3 stop callback + every select-stop verdict | `.erl` beside it |

Build/run (the Nix *include path* trap: `code:lib_dir(erts,include)` answers a directory that does
**not** exist on this host; the real headers are at `<erts_root>/include`, equivalently
`<erts_root>/usr/include`):

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
| probe | answer |
|---|---|
| `sched_sum(2,3)` — schedules `cont_sum`, which returns `{scheduled, N+M}` from `argv` | `{scheduled,5}` |
| `sched_last_ret()` — the raw word `enif_schedule_nif` returned | **`0`** — `THE_NON_VALUE`, on *every* path, including the refusals |
| `sched_loop(0/1/3)` / `sched_loop(100000)` — self-rescheduling chain | `{done,1}` / `{done,2}` / `{done,4}` / `{done,100001}`, **`stack_size` unchanged** (52→52) |
| `sched_raise()` — a continuation that calls `enif_raise_exception(boom)` | `error:boom` |
| `sched_bad_flags()` / `sched_too_many_args()`, and an `enif_make_badarg` inside a continuation | `error:badarg` |
| `sched_dirty(7)` (`ERL_NIF_DIRTY_JOB_CPU_BOUND`) | `{dirty,7}`; the continuation's `enif_thread_type()` is `2` (dirty CPU), vs `1` (normal) for `flags == 0` |
| `sched_ghost_name(99)` — `fun_name` naming nothing | `{ghost,99}` — the name is interned only, never resolved |

### How it is implemented
`src/nif/enif/schedule.rs` owns the whole lifetime of a scheduled call: it *runs* the continuation,
follows a self-rescheduling chain to its end, and hands the chain's final value back to the NIF call
that started it. `enif_schedule_nif` (`src/nif/enif/process.rs`) validates the arguments and then calls
`schedule::run(env, flags, fptr, argc, argv)`; the earlier `NifHost::schedule_nif` hook — which never
received `argv` — is kept only for source compatibility.

* **Iterative, constant Rust stack.** A thread-local pending-continuation slot plus a nesting counter
  (`PENDING` / `DEPTH`): the **outermost** `run` owns the loop and never recurses, while a **nested**
  `enif_schedule_nif` (a continuation scheduling the next one) *records* its `Continuation` and
  answers the C's `THE_NON_VALUE` (`0`), exactly as the C does. The Rust stack depth is therefore one
  continuation call however long the chain is — which is what keeps `sched_loop(100000)`'s
  `stack_size` unchanged.
* **`argv` uncopied for the outermost schedule, copied for a nested one.** The outermost schedule
  carries the calling NIF's `argv` *pointer*: that vector is alive for the whole chain (the trampoline
  runs inside that call), so the words the continuation reads are the words the scheduling call passed
  in. A nested schedule names a **local** array of the continuation's own frame, gone by the time the
  next continuation is entered, so `Continuation::own_args` takes the copy the C takes
  (`erl_nfunc_sched.c:126-130`) into a buffer the continuation owns.
* **One yield point per iteration.** `NifHost::nif_yield_point(flags)` (defaulted no-op) is called once
  per trampoline iteration, so a host can preempt a chain instead of losing the thread for its whole
  length.
* **Dirty flags are observable, the pool is not.** No dirty-scheduler pool exists here (`beam_rt`
  reports `dirty_scheduler_support: 0`), so a dirty-flagged continuation runs on the calling thread;
  what *is* reproduced is the observable the fixture probes — `enif_thread_type` reports `2`
  (`ERL_NIF_THR_DIRTY_CPU_SCHEDULER`) / `3` (`_IO_`) for the duration of such a continuation and is
  restored afterwards (both bits set reports the CPU type, deterministically).
* **A raise ends the chain and leaves the reason on the env.** The trampoline never clears an env's
  pending exception and never fabricates a stacktrace head: the host's error path attributes the raise
  to the NIF call in progress, which *is* the scheduling NIF.
* **The one substitution**, stated in `schedule.rs`'s module doc: the emulator's trap re-run is folded
  into the *outermost* `enif_schedule_nif` call, so that call returns the chain's **value** where the C
  returns `0` (visible only to a probe of the raw word, e.g. `sched_last_ret()`); a nested schedule
  answers the C's `0` exactly.

### Design (landed, item by item)
1. Keep every existing refusal, in this order (`erl_nif.c:3569-3603`): `argc > MAX_ARG` (255,
   `erl_vm.h:48`, checked **first**), `argc < 0`, a `flags` bit outside
   `ERL_NIF_DIRTY_JOB_CPU_BOUND|ERL_NIF_DIRTY_JOB_IO_BOUND`, null/non-UTF-8 `fun_name`, null `fptr`.
   Note the observed convention: a refusal is *raised* (`enif_make_badarg` →
   `enif_raise_exception` → `BIF_ERROR`, `erl_nif.c:1791-1801` + `bif.h:227-230`), so on the wire it
   is also `THE_NON_VALUE`; `beam_abi` returns the `badarg` term and the host turns it into a
   raise — the same observable.
2. Copy nothing on the outermost schedule: the arguments already live on the caller's env heap and stay
   valid for the whole trampoline. `argv` must be readable for `argc` (a null `argv` with `argc > 0`
   is `badarg`); a nested schedule copies, as item 2 of "How it is implemented" describes.
3. Model the C's **three-way** outcome instead of a term sentinel. In C both "rescheduled" and
   "raised" are `THE_NON_VALUE`, distinguished only by `proc->freason` (`execute_nif`,
   `erl_nif.c:3527-3541`); the Rust trampoline uses an explicit
   `enum Outcome { Value(Eterm), Reschedule(Continuation), Raised }`.
4. The trampoline must be **iterative with constant Rust stack** (the oracle runs a 10 000 000-entry
   chain; a per-reschedule recursion blows the native stack). Shape: a thread-local pending-continuation
   slot plus a nesting counter; a nested `enif_schedule_nif` (i.e. the continuation scheduling again)
   *records* the continuation and reports `Reschedule`; the outermost call loops until `Value`/`Raised`.
5. Between iterations the host must be given a *yield point* so the process can be preempted: the
   oracle is fair — with `+S 1:1`, an unrelated process ran 3 000 033 `receive … after 0` iterations
   *while* a 3 000 000-entry chain ran. The additive
   `NifHost::nif_yield_point(&mut self, flags: c_int)` (defaulted no-op) is called once per iteration.
   Without it a chain holds the emulator thread.
6. While the chain runs, `process_info(P, current_function)` is `{Module, fun_name, argc}` from the
   schedule (measured `{a3_schedule,cont_loop,1}`, `status = running`) — contract A.8.
7. A **raise inside a continuation is attributed to the scheduling NIF**, with the *original* MFA and
   the *original* argv, never the continuation's: measured `{a3_schedule,sched_raise,[],[]}` and, for
   a continuation that rescheduled with a different argv, still `{a3_schedule,sched_reargv,[4242],[]}`
   (`execute_nif` sets `EXF_RESTORE_NFUNC`, `erl_nif.c:3534-3541`, restored by `handle_error` from
   `nep->argv`/`nep->mfa` saved at the first schedule — `erl_nfunc_sched.c:106-134`). Synthesising a
   stacktrace head from the continuation's identity would be wrong; the engine owns that head.
8. Dirty flags: no dirty-scheduler pool exists in this runtime (`beam_rt` reports
   `dirty_scheduler_support: 0`), so the continuation runs on the calling thread. The value, `argv` and
   env semantics are identical to the oracle's; the *thread identity* is not. What is reproduced is
   the observable: `enif_thread_type()` answers the C's dirty type for the continuation's duration.
9. `fun_name` is interned-and-ignored (never resolved — measured). Do not use it to look anything up.
   Do note the loader-side constraint the fixture hit: every `ErlNifFunc` row (continuations
   included) must name an **exported** function of the `.erl` module (`erl_nif.c:4874-4878`,
   `{error,{bad_lib,"Function not found M:F/A"}}`).

---

## §2 `enif_alloc_env` off the NIF-call path (+ `enif_send` from a NIF-created thread)

### What the oracle does (measured, `a3_thread_env`, identical across 3 runs)
| probe | answer |
|---|---|
| `probe_thread_send()` — an `enif_thread_create`d worker: `enif_alloc_env` → build `{from_thread,42,<<"abc">>}` → `enif_send(NULL, &pid, msg_env, msg)` → `enif_free_env` | env **not** NULL, send code **`1`**, message arrives: `{from_thread,42,<<"abc">>}`; the worker's `enif_thread_type()` is `0` (`ERL_NIF_THR_UNDEFINED`) |
| `probe_main_env()` — the same idiom on the call path | `{0,1}`, `{from_call,42}` |
| `probe_null_env()` — `enif_send(NULL, …)` **from a scheduler thread** | `{0,1,1}` (works: `env == NULL` is legal unconditionally, not only off-scheduler) |
| `probe_worker_atoms()` — `enif_make_atom`/`enif_get_atom*` on the worker's env | round-trips (`enif_get_atom` answers `len+1`); the atom prints and compares as `worker_atom` |
| `probe_thread_send_to(Pid)` — the target is parked in `receive` | delivered and the receiver **woken** |

### What the C guarantees that a Rust port must not lose
- `enif_alloc_env` touches **no** thread state (`erts_alloc_fnf` + `setup_nif_env(msg_env, NULL,
  NULL)`, `erl_nif.c:636-680`): a process-independent env whose heap *and* the one VM-global atom
  table are the runtime's. A thread-local "am I inside a call?" lookup is the wrong shape — which is
  exactly why this crate used to answer `NULL`.
- The message env's heap is **transferred, not copied**: `erts_create_message_from_nif_env` splices
  the env's fragment chain and off-heap list into the `ErtsMessage` and nulls the env's
  (`erl_nif.c:812-841`, used at `:960`). So a `msg_env` send is zero-copy, the term cannot be reused
  after the send, and one env per message is the idiom. A Rust host may keep the copy semantics but
  must say so.
- Delivery is **synchronous inside `enif_send`** (the message is in the mailbox before it returns)
  and it must **wake a parked receiver** — i.e. it has to go through `beam_process`'s real queue +
  resume-on-send path, never a deferred flush.
- The sender identity with no calling process is `undefined`, not a fabricated pid
  (`erl_nif.c:996`).

### How it is implemented
`src/nif/thread_env.rs` owns the reachability seam, and `src/nif/enif/envs.rs::enif_alloc_env` walks it
in this order:

1. the host of the NIF call this thread is inside (published by `NifEnv::new`, withdrawn by its
   `Drop`) — the call-path answer, unchanged;
2. else the host the installed `ThreadEnvHostFactory` opened for **this thread** (`with_thread_env_host`,
   cached per thread and stamped with a global generation so a host opened by a superseded factory is
   dropped rather than reused);
3. else **`NULL`** — and `NULL` is deliberately kept as the answer when nothing is reachable: a
   crate-owned env with a private atom table would hand a NIF an atom term carrying a **wrong atom
   index** — a wrong *term*, delivered to another process — which is strictly worse than an honest
   refusal.

The factory is `Send + Sync + 'static` while the host it opens is not, because each thread opens its
own host over the same runtime state (an `Arc<…>` captured by the factory is the documented idiom);
last install wins, a poisoned lock answers `false`/`None` instead of panicking, and every install or
clear bumps the generation so no cached host outlives its factory. `enif_send` resolves its host from
`env` when it is a live env, **else from `msg_env`** (the measured `enif_send(NULL, &pid, msg_env, msg)`
idiom). Ownership is unchanged by the seam: the env belongs to the host that allocated it, so
`enif_free_env` needs no factory step.

### Design (landed, item by item)
1. A process-global, `Send + Sync` factory seam in `src/nif/thread_env.rs`:
   `pub trait ThreadEnvHostFactory: Send + Sync + 'static { fn open(&self) -> Option<Box<dyn NifHost>>; }`
   + `pub fn install_thread_env_factory(f: Box<dyn ThreadEnvHostFactory>) -> bool`, stored in a
   `Mutex<Option<…>>` (last-install-wins, poison ⇒ `None`, never a panic), with a thread-local cache
   of the host `open()` returned (a `NifHost` is `!Send`, so each thread opens its own — sharing goes
   through the factory's own `Send + Sync` handles).
2. `enif_alloc_env` reachability order, documented as such: (1) the current NIF call's host on this
   thread — the call-path behaviour, unchanged; (2) else the factory-opened host cached on this thread;
   (3) else `NULL`. **`NULL` stays the answer when no host is reachable** — a crate-owned env with a
   private atom table would hand a NIF a *wrong atom index*, which is worse than an honest refusal
   (that reasoning is in `src/nif/enif/envs.rs`'s module doc and survives).
3. `enif_send`: resolve the host from `env` when it is a live env, **else from `msg_env`** (the
   `enif_send(NULL, &pid, msg_env, msg)` idiom). Nothing else changes: null `to_pid` ⇒ `0`,
   unsupported hook ⇒ `0`.
4. **Host half (`beam_rt`, still owed):** a factory whose `intern_atom`/`atom_name` reach the
   runtime's **live** `AtomTable` (the `AtomInterner` seam — the measured atom round-trip proves a
   snapshot is not enough, `enif_make_atom` can *create* an atom) and whose `send` hook injects into
   the process table thread-safely (a `Send + Sync` channel drained by the scheduler, mirroring
   `beam_abi::driver::async_pool::AsyncPool`), so a send **wakes** a parked receiver. Until
   `beam_rt` installs one, `enif_alloc_env()` off the call path answers `NULL` — an honest refusal,
   never a wrong term.

---

## §3 The resource `stop` callback and the select-stop verdicts

### Ground truth
`erts_resource_stop(resource, e, is_direct_call)` (`erl_nif.c:3032-3040`) builds a **nooproc message
env** and calls `type->fn.stop(&env, resource->data, e, is_direct_call)`. It is called from
`enif_select`'s STOP arm (`erl_check_io.c:1531`, `is_direct_call = 1`) or from the poller thread
(`:2241`, `is_direct_call = 0`). The returned code's low bits are the verdict:
`ERL_NIF_SELECT_STOP_CALLED` = 1 << 0, `ERL_NIF_SELECT_STOP_SCHEDULED` = 1 << 1.

### What the oracle does (measured, `a3_resource_stop`; stable slots of 3 runs)
| probe | code | when `stop` runs |
|---|---|---|
| STOP on an fd never armed (fast track, `:1278-1285`) | `1` (`STOP_CALLED`) | synchronously inside `enif_select`, `is_direct_call = 1`, scheduler thread; the **caller keeps** its reference (C `CALL_STOP`) |
| arm READ then STOP, epoll-able fd (pipe) | `17` (`STOP_CALLED \| READ_CANCELLED`) | synchronously, `is_direct_call = 1`; `CALL_STOP_AND_RELEASE` — the runtime releases the select's own reference |
| arm READ then STOP, fd the kernel poll refuses (`/dev/null`, a regular file, a directory: `epoll_ctl(ADD)` → `EPERM`) | `18` (`STOP_SCHEDULED \| READ_CANCELLED`) | **later, on the poller thread**, `is_direct_call = 0`, `enif_thread_type() = 0`, measured latency 2.4 µs – 1.6 ms; the poller releases the reference |
| STOP twice | `17` then `1` | the callback runs a **second** time (the first STOP cleared the fd's event state, so the second takes the unknown-fd fast track) |
| CANCEL then STOP | `16` then `1` | once, `is_direct_call = 1` |
| a **READ** on an fd whose stop is still pending | `2` (`STOP_SCHEDULED`) | nothing yet — `steal_pending_stop_nif` then unconditionally `ret = ERL_NIF_SELECT_STOP_SCHEDULED` (`:1331-1337`), polluting even a READ's return |

Two further measured facts: **`STOP_SCHEDULED` is unreachable on Linux/epoll for an epoll-able fd**
(`wake_poller` is only ever set by the select/poll *fallback* driver, `erl_poll.c:761-877`), so a pipe
or socket STOP is *always* synchronous; and a resource type **without** a `stop` callback crashes the
oracle (SIGSEGV, 3/3) because `ASSERT(resource->type->fn.stop)` (`:1279`) is compiled out in a release
build and `erts_resource_stop` calls a NULL pointer — this crate must make that a deliberate typed
decision, not a silent one, and the fixture's `probe_stop_nostop_type()` is therefore **never** run by
the test.

### How it is implemented
`src/nif/enif/resources_stop.rs` holds the invocation seam
(`resource_type_has_stop(obj)` / `call_resource_stop(obj, event, is_direct_call)`, both answering
`false` — never a fabricated success — for a null handle, an object this crate did not allocate, or a
type opened without `stop`), and `src/nif/enif/stop.rs` holds the parking lot.
`src/nif/enif/select.rs::service_stop_verdict` dispatches the host's answer **after**
`NifHost::select` returns, leaving the code itself answered unchanged:

* `code & ERL_NIF_SELECT_STOP_CALLED` ⇒ `call_resource_stop(obj, e, true)` — synchronous,
  `is_direct_call = 1`. This crate takes and releases no reference on that verdict (the caller keeps
  its own on the fast track; the runtime already released on `CALL_STOP_AND_RELEASE`).
* `code & ERL_NIF_SELECT_STOP_SCHEDULED` ⇒ `record_pending_stop(obj, e)`: one `enif_keep_resource`
  reference (`:1509`) and an insertion-ordered entry, drained later by `run_pending_stops()`, which
  calls the callback with `is_direct_call = 0` (`:2241`) and then **releases the kept reference**
  (`:2242`). A second STOP for the same `(obj, e)` **steals** the entry rather than double-arming, and
  the displaced reference is released outside the lock (`steal_pending_stop_nif`, `:1334`).
* anything else — a plain `0`, the `*_CANCELLED` bits, a negative refusal ⇒ nothing runs, nothing is
  kept, nothing is released.
* a `stop`-less type stays a **deliberate typed no-op**: the host's code is answered, the call is
  skipped, because the oracle SIGSEGVs there (above). `resource_type_has_stop` is the query.

The one deviation, stated once: a parked stop runs on whichever thread calls the drain (a host with a
real reactor calls it from its own poll loop, where the C's poller thread sits), so only the thread's
identity can differ, never the operand.

### Design (landed, item by item)
1. `src/nif/enif/resources_stop.rs` (the sibling of `resources.rs`, next to the existing
   `resource_type_has_down` / `deliver_resource_down_id`):
   - `pub fn resource_type_has_stop(obj: *mut c_void) -> bool`
   - `pub fn call_resource_stop(obj: *mut c_void, event: ErlNifEvent, is_direct_call: bool) -> bool`
     — `true` iff this crate allocated the object, its type has a `stop`, and the callback ran.
     Invoke it on a nooproc message env exactly as the existing `dtor` invocation does (`erl_nif.c:3016-3021`).
     A foreign/stale object, a null pointer or a type without `stop` answers `false` — never a
     fabricated success.
2. `src/nif/enif/select.rs`'s STOP arm, *after* `NifHost::select` answers (the host owns the poller and
   decides which verdict applies) — `service_stop_verdict`:
   - `code & STOP_CALLED` ⇒ `call_resource_stop(obj, e, true)`.
   - `code & STOP_SCHEDULED` ⇒ `record_pending_stop(obj, e)` (`src/nif/enif/stop.rs`), taking the C's
     `enif_keep_resource` reference (`:1509`), with `pub fn run_pending_stops() -> usize` calling
     `call_resource_stop(obj, e, false)` and then releasing the kept reference (`:1533`). A second
     STOP for the same `(obj, e)` must steal, not double-arm (`steal_pending_stop_nif`, `:1334`).
   - anything else (a plain `0`, the `*_CANCELLED` bits, a negative refusal) ⇒ no `stop` call.
   - A type without `stop`: answer the host's code, skip the call, document it.
3. **Reference accounting is the subtle part** and must be stated per verdict: fast-track `CALL_STOP`
   ⇒ the caller still owns its reference; `CALL_STOP_AND_RELEASE` ⇒ the runtime already released;
   `STOP_SCHEDULED` ⇒ the drain releases. The fixture's balanced 8 objects → 8 dtors across all four
   verdicts is the pin.
4. **Host half (`beam_rt`):** its select store de-registers immediately, so `STOP_CALLED` is the
   *faithful* answer there (and, per the measurement above, the only one Linux/epoll can produce for a
   real fd) — so §3 works in the runtime with **no** `beam_rt` change. `STOP_SCHEDULED` becomes
   reachable only for a host that really defers.

---

## §4 What this node provides

- The three fixtures + `.erl` drivers, `cc -Wall -Wextra` clean, oracle-measured (tables above) — the
  ground truth both test files compare against.
- `tests/nif_a3_oracle_replay.rs` — **3 oracle replays**, one per fixture: compile the `.c` with `cc`,
  `erlc` the `.erl`, run it under the live system `erl`, and assert the recorded OTP 29.1.1 /
  erts-17.1 transcript; each skips loudly without the toolchain.
- `tests/nif_a3_abi_assertions.rs` — **3 in-process ABI assertions** (no `dlopen`, no oracle), each
  pinning one section's verdict matrix at the `beam_abi` C-ABI boundary. One test binary over three
  `#[path]` modules, one per section (`tests/nif_a3_abi_assertions/{schedule,thread_env,select_stop}.rs`),
  so each stays inside the ~1000-line file rule:
  - `enif_schedule_nif_runs_its_continuation_to_completion` (§1) — the continuation runs and its value
    is the call's value, a nested schedule answers the C's `THE_NON_VALUE`, a long chain stays
    constant-stack, a raise ends it with the reason left on the env, dirty flags pick the reported
    thread type.
  - `enif_alloc_env_off_the_call_path_needs_a_factory_and_then_works` (§2) — `NULL` with no call host
    and no factory, a real env once a factory is installed, the per-thread generation-stamped cache,
    and `enif_send` taking its host from `env` else `msg_env`.
  - `enif_select_stop_verdicts_run_the_right_callback` (§3) — `STOP_CALLED` runs `stop` synchronously
    with `is_direct_call = 1`, `STOP_SCHEDULED` parks and `run_pending_stops()` runs it with
    `is_direct_call = 0` after releasing the keep, anything else runs nothing, a `stop`-less type is a
    no-op, and a second STOP steals rather than double-arms.
- A measured-transcript record inside the replay test's doc comments (stable fields only; the
  deferred-stop latency/`CallsAtReturn` slots vary with host load, and READ-on-pending-stop is a
  genuine race the test resolves with a bounded retry).

**Still owed, and where:**

- (a) **`../beam_rt`** — the runtime host halves: the §2 item 4 factory host over the live `AtomTable`
  plus a thread-safe send that wakes a parked receiver, and the `beam_rt`-level end-to-end cases.
- (b) **`../tools/differential`** (sibling node) — the corpus cases of §5.
- (c) Nothing in `beam_abi`'s ABI surface is outstanding for these three paths.

## §5 Differential-corpus cases owed (`tools/differential`, sibling node)
- `nif_a3/schedule_normal_continuation_value`, `nif_a3/schedule_chain_of_100000` (constant stack),
  `nif_a3/schedule_continuation_raises_blames_the_scheduling_nif`, `nif_a3/schedule_bad_flags_is_badarg`,
  `nif_a3/schedule_argc_over_max_arg_is_badarg`, `nif_a3/schedule_dirty_flag_thread_type`.
- `nif_a3/alloc_env_on_a_nif_thread_sends`, `nif_a3/enif_send_null_env_delivers_and_wakes`.
- `nif_a3/resource_stop_called_on_select_stop`, `nif_a3/resource_stop_double_stop_calls_twice`,
  `nif_a3/resource_read_on_pending_stop_answers_stop_scheduled`.

Each is expressible through a single CLI process (no distribution needed): the `.erl` drivers already
print one line per probe, so a case can compare that line verbatim. Note the gate's `--strict` mode and
that the drivers need `cc` + `erl_nif.h`; skip loudly when absent, as the existing NIF cases do.

## §6 Test placement
The two test files **stay in this crate**: they are cross-crate assertions at the `beam_abi` ABI
boundary, and the fixtures they rest on are this node's ground-truth artifacts. `nif_a3_oracle_replay.rs`
owns the compile-and-run oracle replays; `nif_a3_abi_assertions.rs` is the in-process binary and its
three per-section modules are the assertions, split only to respect the file-size rule (a single file
would run ~1500 lines). Any further `beam_abi`-internal tests are that node's call. The `beam_rt`-level
end-to-end cases belong in `crates/beam_rt/tests/` (the runtime's own NIF tests already own the
load/call harness), and the corpus cases per §5.
