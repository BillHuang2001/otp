# A.3 NIF host gaps — implementation handoff (`beam_abi`)

Three A.3 (NIF C ABI) capability gaps remain in `../beam_abi`, each a typed refusal today:

1. `enif_schedule_nif` never runs the continuation it is asked to schedule.
2. `enif_alloc_env()` answers `NULL` off the NIF-call path, so a NIF-created thread cannot build a
   message env and `enif_send` from it.
3. A resource type's `stop` callback is never invoked, so `ERL_NIF_SELECT_STOP_CALLED` is answered
   without the callback running and `ERL_NIF_SELECT_STOP_SCHEDULED` is unobservable.

**Why this document lives in `beam_bifs`:** every implementation site is in `crates/beam_abi`
(`src/nif/enif/{process,envs,select,resources}.rs`, `src/nif/host.rs`) and in the host
`crates/beam_rt` (`src/nif.rs`, `src/nif_select.rs`), i.e. sibling nodes the authoring agent has no
write authority over. What this node *can* own — and does — is the ground truth: three **real C NIF
fixtures**, each compiled with `cc` and **run against the live oracle** (OTP 29.1.1 / erts-17.1), plus
`tests/nif_a3_oracle_replay.rs` (which replays the measured oracle transcript) and
`tests/nif_a3_abi_assertions.rs` (which asserts the same three contracts in-process). All three gaps are
now **closed** in `beam_abi`; the two suites are the regression gate. The measurements below are
recorded facts from those runs, not predictions: several of
them contradict the obvious port of the C, so read them before writing code.

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

### Why the current seam cannot express it
`beam_abi`'s `enif_schedule_nif` (`src/nif/enif/process.rs`) forwards to
`NifHost::schedule_nif(name, flags, fptr, argc)` — **no `argv`** — and returns the hook's term as its
own result. Both halves are wrong against the measurements: (a) the hook cannot run the continuation
because it never receives the arguments, and (b) the C's return is `THE_NON_VALUE` (a trap marker),
*not* the continuation's value — the value reaches the caller only after the emulator re-runs the
trap (`erl_nif.c:367-400` `schedule`, `:3506-3558` `execute_nif`, `:3584-3596`).

### Design (resolved — implement this shape)
1. Keep every existing refusal, in this order (`erl_nif.c:3569-3603`): `argc > MAX_ARG` (255,
   `erl_vm.h:48`, checked **first**), `argc < 0`, a `flags` bit outside
   `ERL_NIF_DIRTY_JOB_CPU_BOUND|ERL_NIF_DIRTY_JOB_IO_BOUND`, null/non-UTF-8 `fun_name`, null `fptr`.
   Note the observed convention: a refusal is *raised* (`enif_make_badarg` →
   `enif_raise_exception` → `BIF_ERROR`, `erl_nif.c:1791-1801` + `bif.h:227-230`), so on the wire it
   is also `THE_NON_VALUE`; `beam_abi` returns the `badarg` term today and the host turns it into a
   raise — keep that, it is the same observable.
2. Copy nothing: the arguments already live on the caller's env heap and stay valid for the whole
   trampoline. Validate `argv` is readable for `argc` (a null `argv` with `argc > 0` is `badarg`).
3. Model the C's **three-way** outcome instead of a term sentinel. In C both "rescheduled" and
   "raised" are `THE_NON_VALUE`, distinguished only by `proc->freason` (`execute_nif`,
   `erl_nif.c:3527-3541`); a Rust trampoline needs an explicit
   `enum Outcome { Value(Eterm), Reschedule(Continuation), Raised }`.
4. The trampoline must be **iterative with constant Rust stack** (the oracle runs a 10 000 000-entry
   chain; a per-reschedule recursion blows the native stack). Shape: a thread-local pending-continuation
   slot plus a nesting counter; a nested `enif_schedule_nif` (i.e. the continuation scheduling again)
   *records* the continuation and reports `Reschedule`; the outermost call loops until `Value`/`Raised`.
5. Between iterations the host must be given a *yield point* so the process can be preempted: the
   oracle is fair — with `+S 1:1`, an unrelated process ran 3 000 033 `receive … after 0` iterations
   *while* a 3 000 000-entry chain ran. Additive seam, e.g.
   `NifHost::nif_yield_point(&mut self, flags: c_int)` (defaulted no-op), called once per iteration.
   Without it a chain holds the emulator thread; document that if you skip it.
6. While the chain runs, `process_info(P, current_function)` is `{Module, fun_name, argc}` from the
   schedule (measured `{a3_schedule,cont_loop,1}`, `status = running`) — contract A.8.
7. A **raise inside a continuation is attributed to the scheduling NIF**, with the *original* MFA and
   the *original* argv, never the continuation's: measured `{a3_schedule,sched_raise,[],[]}` and, for
   a continuation that rescheduled with a different argv, still `{a3_schedule,sched_reargv,[4242],[]}`
   (`execute_nif` sets `EXF_RESTORE_NFUNC`, `erl_nif.c:3534-3541`, restored by `handle_error` from
   `nep->argv`/`nep->mfa` saved at the first schedule — `erl_nfunc_sched.c:106-134`). Synthesising a
   stacktrace head from the continuation's identity would be wrong.
8. Dirty flags: no dirty-scheduler pool exists in this runtime (`beam_rt` reports
   `dirty_scheduler_support: 0`), so run the continuation on the calling thread and document the
   difference: the value/args/env semantics are identical, the thread identity is not
   (`enif_thread_type()` would report `1` where the oracle reports `2`).
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
  exactly why this crate's `NULL` appears.
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

### Design (resolved)
1. Add a process-global, `Send + Sync` factory seam in `src/nif/host.rs` so a thread that is not
   inside a NIF call can still obtain a real host:
   `pub trait ThreadEnvHostFactory: Send + Sync + 'static { fn open(&self) -> Option<Box<dyn NifHost>>; }`
   + `pub fn install_thread_env_factory(f: Box<dyn ThreadEnvHostFactory>) -> bool`, stored in a
   `Mutex<Option<…>>` (last-install-wins, poison ⇒ `None`, never a panic), with a thread-local cache
   of the host `open()` returned (a `NifHost` is `!Send`, so each thread opens its own — sharing goes
   through the factory's own `Send + Sync` handles).
2. `enif_alloc_env` reachability order, documented as such: (1) the current NIF call's host on this
   thread — today's behaviour, unchanged; (2) else the factory-opened host cached on this thread;
   (3) else `NULL`. **`NULL` must stay the answer when no host is reachable** — a crate-owned env with
   a private atom table would hand a NIF a *wrong atom index*, which is worse than an honest refusal
   (that reasoning is already in `src/nif/enif/envs.rs`'s module doc and survives).
3. `enif_send`: resolve the host from `env` when it is a live env, **else from `msg_env`** (the
   `enif_send(NULL, &pid, msg_env, msg)` idiom). Nothing else changes: null `to_pid` ⇒ `0`,
   unsupported hook ⇒ `0`.
4. **Host half (`beam_rt`, not this node):** `NifHostImpl` must be opened by a factory whose
   `intern_atom`/`atom_name` reach the runtime's **live** `AtomTable` (the `AtomInterner` seam —
   the measured atom round-trip proves a snapshot is not enough, `enif_make_atom` can *create* an
   atom) and whose `send` hook injects into the process table thread-safely (a `Send + Sync`
   channel drained by the scheduler, mirroring `beam_abi::driver::async_pool::AsyncPool`). Until it
   is installed, `enif_alloc_env()` off the call path stays `NULL` — an honest typed gap.

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

### Design (resolved)
1. `src/nif/enif/resources.rs` (additive, next to the existing `resource_type_has_down` /
   `deliver_resource_down_id`):
   - `pub fn resource_type_has_stop(obj: *mut c_void) -> bool`
   - `pub fn call_resource_stop(obj: *mut c_void, event: ErlNifEvent, is_direct_call: bool) -> bool`
     — `true` iff this crate allocated the object, its type has a `stop`, and the callback ran.
     Invoke it on a nooproc message env exactly as the existing `dtor` invocation does (same file,
     `erl_nif.c:3016-3021`). A foreign/stale object, a null pointer or a type without `stop` answers
     `false` — never a fabricated success.
2. `src/nif/enif/select.rs`'s STOP arm, *after* `NifHost::select` answers (the host owns the poller and
   decides which verdict applies):
   - `code & STOP_CALLED` ⇒ `call_resource_stop(obj, e, true)`.
   - `code & STOP_SCHEDULED` ⇒ record a pending stop for `(obj, e)`, taking the C's
     `enif_keep_resource` reference (`:1509`), and expose `pub fn run_pending_stops() -> usize` which
     calls `call_resource_stop(obj, e, false)` and then releases the kept reference (`:1533`). A second
     STOP for the same `(obj, e)` must steal, not double-arm (`steal_pending_stop_nif`, `:1334`).
   - anything else (a plain `0`, the `*_CANCELLED` bits, a negative refusal) ⇒ no `stop` call.
   - A type without `stop`: answer the host's code, skip the call, document it.
3. **Reference accounting is the subtle part** and must be stated per verdict: fast-track `CALL_STOP`
   ⇒ the caller still owns its reference; `CALL_STOP_AND_RELEASE` ⇒ the runtime already released;
   `STOP_SCHEDULED` ⇒ the drain releases. The fixture's balanced 8 objects → 8 dtors across all four
   verdicts is the pin.
4. **Host half (`beam_rt`):** its select store de-registers immediately, so `STOP_CALLED` is the
   *faithful* answer there (and, per the measurement above, the only one Linux/epoll can produce for a
   real fd) — so §3 works in the runtime with **no** `beam_rt` change once `beam_abi` calls the
   callback. `STOP_SCHEDULED` becomes reachable only for a host that really defers; document that.

---

## §4 What this node already provides
- The three fixtures + `.erl` drivers, `cc -Wall -Wextra` clean, oracle-measured (tables above).
- `tests/nif_a3_oracle_replay.rs` — 3 tests, one per fixture: compile → `erlc` → `erl`, asserting the
  measured oracle transcript (skip loudly without the toolchain, never a silent pass).
- `tests/nif_a3_abi_assertions.rs` — 3 tests, one per gap path, asserting the **closed** contract
  in-process through `beam_abi`'s public `enif_*` entry points (no `dlopen`, no `.so`, no oracle, no
  toolchain): the continuation trampoline (`enif_schedule_nif` runs the continuation exactly once with
  the caller's own uncopied `argv` and answers its value; a nested schedule answers `THE_NON_VALUE`
  while the outermost call yields the chain's final value; one `nif_yield_point` per trampoline
  iteration; a raise ends the chain; dirty flags show through `enif_thread_type`; `argc > 255` and a
  bad flags bit are `badarg` and run nothing); `enif_alloc_env` off the call path (`NULL` / no host /
  `enif_send` `0` with no factory; with one, a real env, an atom round trip, an
  `enif_send(NULL, &pid, msg_env, msg)` with the C's `undefined` sender, and a free that lands back on
  the allocating host); the `enif_select` STOP verdict matrix (`1` and `17` synchronous and direct;
  `18` deferred, run exactly once by `run_pending_stops()` with `is_direct_call = 0` and a second drain
  running nothing; `16`, `0` and a negative code running nothing), with the reference accounting
  checked per case (the parked stop's keep is taken and the drain gives it back).
- One documented difference the schedule assertions encode: for a raise the trampoline answers the
  **pending reason term** — the very word `enif_raise_exception` returned — not the C's raw `0`, because
  the emulator's trap re-run is folded into the outermost call (`beam_abi`'s `nif::enif::schedule`
  module documents that substitution); a nested schedule still answers `0`.
- A measured-transcript record inside the replay test's doc comments (stable fields only; the
  deferred-stop latency/`CallsAtReturn` slots vary with host load, and READ-on-pending-stop is a genuine
  race the test resolves with a bounded retry).

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

## §6 Test placement (decided)
The three assertions stayed in `beam_bifs/tests/nif_a3_abi_assertions.rs` (plus its
`tests/nif_a3_abi_assertions/{schedule,thread_env,select_stop}.rs` modules) rather than moving to
`crates/beam_abi/tests/`: they exercise the frozen ABI only through its public `enif_*` entry points,
they need no fixture and no toolchain, and keeping them beside the oracle replay keeps the ground truth
and the contract in one node (and the file split keeps each file inside the workspace's ~1000-line
rule). The fixtures + oracle replay stay here (this node's ground-truth artifacts). Add the
`beam_rt`-level
end-to-end cases to `crates/beam_rt/tests/` (the runtime's own NIF tests already own the load/call
harness), and the corpus cases per §5.
