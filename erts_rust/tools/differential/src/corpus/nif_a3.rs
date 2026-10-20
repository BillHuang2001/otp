// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%
//! Contract **A.3** — the three `enif_*` host seams that are now **live** in the Rust runtime.
//!
//! [`super::nif_thread`] pins the threading / blocking / cross-process surface that always worked,
//! [`super::nif_state`] the stateful families and [`super::nif_abi`] / [`super::nif_abi_extra`] the
//! frozen structs. This group pins the three seams that used to be **capability gaps** and are now
//! implemented, end to end through a real C library:
//!
//! * **`enif_schedule_nif`'s rescheduling trampoline** — a NIF re-schedules itself and the runtime
//!   hands the **resumed entry's value** back to the Erlang caller, on the same scheduler
//!   (`flags = 0`) and on a dirty CPU scheduler (`ERL_NIF_DIRTY_JOB_CPU_BOUND`).
//! * **The NIF thread environment** — a thread created by `enif_thread_create` calls
//!   `enif_alloc_env()`, builds a term and `enif_send(NULL, &pid, msg_env, msg)`; the message
//!   really reaches that process's mailbox (the receiver's bounded `receive` sees it).
//! * **The resource `stop` / `dtor` callbacks** — a resource type's `stop` callback runs once per
//!   `enif_select(…, ERL_NIF_SELECT_STOP, …)` and its `dtor` runs when the last reference is
//!   released.
//!
//! # The cases
//!
//! | case | the observation |
//! |---|---|
//! | `schedule_nif_resumes_its_continuation_value` | `sched_sum(2,3)` is `{scheduled,5}` and `sched_sum(20,3)` is `{scheduled,23}`: the continuation's own value — computed from **its** argv — is the call's result |
//! | `schedule_nif_chain_completes_at_a_constant_stack` | a 100 000-step self-rescheduling chain answers `{done,100001}`, and a 1-step chain moves the stack by exactly as much (constant-stack trampoline) |
//! | `schedule_nif_raise_and_refusals_reach_the_caller` | a raising continuation stops the chain and the CALLER sees `error:boom`; `flags = 0x1000` and `argc = 300` both raise `badarg` |
//! | `dirty_schedule_nif_resumes_on_a_dirty_cpu_scheduler` | `sched_dirty(7)` is `{dirty,7}` **and** the continuation's `enif_thread_type()` is `ERL_NIF_THR_DIRTY_CPU` (`2`) — it really ran on a dirty CPU scheduler |
//! | `nif_thread_env_send_reaches_the_owning_process` | a NIF-created thread's `enif_alloc_env` + `enif_send(NULL, …)` answers `{{0,1,0},0}` and the caller receives exactly `{from_thread,42,<<"abc">>}` |
//! | `nif_thread_env_atom_identity_crosses_by_name` | the worker's atom reads back by length **and** text (`{0,1,1,1,0}`) and the receiver matches it against its **own** literal (`worker_atom`) — atom identity, not a printed name |
//! | `nif_thread_env_send_wakes_a_parked_receiver` | the same send to a receiver verified `{status,waiting}` first; the message `{parked_got,{from_thread_to_target,42}}` arrives |
//! | `resource_stop_runs_once_with_the_descriptor` | arm READ then STOP answers `17` (`STOP_CALLED \| READ_CANCELLED`), the callback ran **once**, inline (`is_direct_call = 1`) and saw the stopped object |
//! | `resource_stop_on_an_unarmed_descriptor_takes_the_fast_track` | a STOP on a pipe descriptor nothing ever armed answers `1` and still runs the callback once, inline, with a real descriptor |
//! | `resource_stop_twice_runs_the_callback_twice` | arm READ, STOP, STOP: the first STOP is `17`, the second is the fast-track `1`, and the callback has run **twice** |
//! | `resource_cancel_then_stop_cancels_the_read` | arm READ, CANCEL → `16` (`READ_CANCELLED`), then STOP → the plain `1` (there is no read interest left to cancel) |
//! | `resource_dtor_runs_when_the_last_reference_is_released` | an object's only reference dropped in a NIF reaches `{1,5}` (one destructor call, the right object) — reached through a **bounded** wait, because ERTS runs a destructor as aux work |
//!
//! # Mechanics (identical to [`super::nif_thread`])
//!
//! One embedded C library (`nif_a3`) is compiled by the harness inside the case's own temp dir
//! against the host `erl_nif.h`, and the embedded module `erlang:load_nif/2`s it from
//! `filename:dirname(code:which(?MODULE))`, so both runtimes load *the same* `.so` and run *the
//! same* compiled module; only stdout (the single integer line `classify/1` returns) and the exit
//! status are compared. Two rules the fixture honours and a future case must keep honouring:
//!
//! * every call into the library is **external** (`nif_a3:sched_sum/2`), because the Rust runtime
//!   intercepts a NIF at external dispatch — a *local* call reaches the `erlang:nif_error/1` stub
//!   and raises `not_loaded` on the Rust side only (a guaranteed false mismatch);
//! * every `ErlNifFunc` row has an exported stub of the same arity in the module — including the
//!   `cont_*` continuations, which are entered **by pointer** but are still validated against the
//!   module's own function table at load time — or the oracle refuses the load with `bad_lib`.
//!
//! # Bounded waits, no possible hang
//!
//! Every wait is bounded and every created thread is joined: the module's waits are
//! `receive … after 5000` (the delivered message), `await_waiting/2` (a bounded poll of
//! `process_info(Pid, status)` before the parked-receiver send) and `await_dtor/1` (a bounded poll
//! for the destructor). The C contains **no** sleep and no unbounded loop at all: its only native
//! wait is `enif_thread_join`, and the worker it joins does nothing but send one message and return.
//!
//! # Measured and deliberately NOT pinned
//!
//! * **`enif_thread_type()` of the *calling* thread** — the recorded divergence: the oracle runs a
//!   NIF on a scheduler thread (`1`), the Rust runtime on its own driver thread (`0`,
//!   `ERL_NIF_THR_UNDEFINED`). Every probe here that reports it is destructured into a `_Thread`
//!   slot no case reads. The type of a thread the **NIF itself created** (`0`) and of a **dirty**
//!   continuation (`2`) *do* agree and are pinned.
//! * **`enif_schedule_nif`'s raw return word** — the oracle answers `THE_NON_VALUE` (`0`,
//!   `erl_nif.c:399`) and the Rust runtime answers the continuation's own tagged term word (its
//!   trampoline *is* the resumed entry, so no "no value" word is ever produced; see
//!   `beam_abi`'s `nif/enif/schedule.rs`). The word is address-dependent, so the C has no
//!   `last_ret` probe: only the resumed value is pinned.
//! * **`enif_thread_type()` inside a NON-dirty continuation** — `1` in the oracle (a scheduler
//!   thread), `0` in the Rust runtime (entered inline on its driver thread).
//! * **The deferred STOP arm** (`ERL_NIF_SELECT_STOP_SCHEDULED`) — the Rust runtime answers
//!   `ERL_NIF_SELECT_STOP_CALLED` for every STOP by construction (the callback is run synchronously
//!   through `beam_abi`'s `service_stop_verdict`, `is_direct_call = true`), while the oracle's own
//!   answers for the fallback-pollset probes are timing-dependent. Those probes are not in the
//!   library at all, and neither is the probe that STOPS on a resource type **without** a `stop`
//!   callback: the C only `ASSERT`s that the callback exists (`erl_check_io.c:1279`), `ASSERT` is
//!   compiled out of a release build, so the oracle calls a NULL function pointer and dies with
//!   SIGSEGV — a test that kills the oracle is not a test.
//! * **The destructor's *timing*** — ERTS runs a resource destructor as **aux work**
//!   (`nif_resource_dtor_prologue`, `erl_nif.c:2961-2972`, "Schedule user resource destructor as aux
//!   work"), so it is never inline: the report printed immediately after a release still sees `0`
//!   (measured, both fixtures). Only its **eventual** count and payload are pinned
//!   (`resource_dtor_runs_when_the_last_reference_is_released`, through `await_dtor/1`), and the
//!   report that follows a STOP deliberately ignores the destructor slots.
//! * **Every per-VM value** — a pid, a ref, an fd number, a pointer, a thread id, a thread name or
//!   a timing figure. The C builds no pid/ref term that reaches a returned tuple (the one
//!   `enif_make_ref` per select probe is handed to `enif_select` and dropped), and the module
//!   prints nothing of its own: the golden is always one integer bit aggregate.
//! * **Already pinned elsewhere, not duplicated here**: the STOP *codes* for an armed/cancelled
//!   descriptor ([`super::nif_state`]'s `select_stop_releases_an_armed_read`), `enif_send`'s
//!   in-call forms ([`super::nif_so`]'s `nif_rt` block), the schedule refusals' *shape*
//!   ([`super::nif_abi`]'s `schedule_nif_rejects_*`), and the threading/mutex/binary families
//!   ([`super::nif_thread`]).
use super::{Case, CaseKind, Contract, Golden, ModuleRun};
use crate::module_build::NifFixture;

/// The embedded module: one stub per `ErlNifFunc` row plus the `classify/1` selector table.
///
/// Every call into the library is **external** and every wait is bounded. A case's answer is a
/// **bit aggregate** (one integer, each bit a documented check), so a single integer line carries
/// every observation of that case and the golden stays stable.
const NIF_A3_MODULE_SOURCE: &str = r#"
-module(nif_a3).
-export([classify/1,
         sched_sum/2, sched_loop/1, sched_raise/0, sched_bad_flags/0, sched_too_many_args/0,
         sched_dirty/1, sched_dirty_type/1,
         cont_sum/2, cont_loop/1, cont_raise/0, cont_dirty/1, cont_ttype/1,
         thread_send/0, worker_atoms/0, thread_send_to/1,
         stop_reset/0, stop_report/0, stop_unknown_fd/0, arm_then_stop/0, double_stop/0,
         cancel_then_stop/0, dtor_report/0, dtor_release/0]).

%% One stub per ErlNifFunc row: `erlang:load_nif/2` refuses a library whose rows are not real,
%% exported functions of the loading module (the continuations are entered by POINTER, but each
%% still needs its row). Every call into the library is EXTERNAL (`nif_a3:...`), because the Rust
%% runtime intercepts a NIF at external dispatch only.
sched_sum(_N, _M) -> erlang:nif_error(not_loaded).
sched_loop(_N) -> erlang:nif_error(not_loaded).
sched_raise() -> erlang:nif_error(not_loaded).
sched_bad_flags() -> erlang:nif_error(not_loaded).
sched_too_many_args() -> erlang:nif_error(not_loaded).
sched_dirty(_N) -> erlang:nif_error(not_loaded).
sched_dirty_type(_N) -> erlang:nif_error(not_loaded).
cont_sum(_N, _M) -> erlang:nif_error(not_loaded).
cont_loop(_N) -> erlang:nif_error(not_loaded).
cont_raise() -> erlang:nif_error(not_loaded).
cont_dirty(_N) -> erlang:nif_error(not_loaded).
cont_ttype(_N) -> erlang:nif_error(not_loaded).
thread_send() -> erlang:nif_error(not_loaded).
worker_atoms() -> erlang:nif_error(not_loaded).
thread_send_to(_Pid) -> erlang:nif_error(not_loaded).
stop_reset() -> erlang:nif_error(not_loaded).
stop_report() -> erlang:nif_error(not_loaded).
stop_unknown_fd() -> erlang:nif_error(not_loaded).
arm_then_stop() -> erlang:nif_error(not_loaded).
double_stop() -> erlang:nif_error(not_loaded).
cancel_then_stop() -> erlang:nif_error(not_loaded).
dtor_report() -> erlang:nif_error(not_loaded).
dtor_release() -> erlang:nif_error(not_loaded).

classify(N) ->
    Dir = filename:dirname(code:which(?MODULE)),
    case erlang:load_nif(Dir ++ "/nif_a3", 0) of
        ok -> run(N);
        {error, {Reason, _Text}} -> reason_code(Reason)
    end.

%% 1: the resumed continuation's value IS the call's result, and the arguments the scheduling call
%%    passed reach the continuation.
run(1) ->
    bit(nif_a3:sched_sum(2, 3) =:= {scheduled, 5}, 1)
  + bit(nif_a3:sched_sum(20, 3) =:= {scheduled, 23}, 2);
%% 2: a 100 000-step self-rescheduling chain completes, and a 1-step chain moves the stack by
%%    exactly as much as the long one (the chain is constant-stack).
run(2) ->
    Big = guarded(fun() -> nif_a3:sched_loop(100000) end),
    D1 = delta(fun() -> nif_a3:sched_loop(1) end),
    D2 = delta(fun() -> nif_a3:sched_loop(100000) end),
    bit(Big =:= {ok, {done, 100001}}, 1) + bit(D1 =:= D2, 2);
%% 3: a raising continuation stops the chain and the CALLER sees the raise; both refusals raise.
run(3) ->
    bit(guarded(fun() -> nif_a3:sched_raise() end) =:= {raised, error, boom}, 1)
  + bit(guarded(fun() -> nif_a3:sched_bad_flags() end) =:= {raised, error, badarg}, 2)
  + bit(guarded(fun() -> nif_a3:sched_too_many_args() end) =:= {raised, error, badarg}, 4);
%% 4: a dirty-flagged schedule delivers its value, and the continuation really ran on a dirty CPU
%%    scheduler (ERL_NIF_THR_DIRTY_CPU = 2) with the argument it was handed.
run(4) ->
    Cont = case nif_a3:sched_dirty_type(1) of
               {threads, _Caller, 2, 1} -> 2;
               _ -> 0
           end,
    bit(guarded(fun() -> nif_a3:sched_dirty(7) end) =:= {ok, {dirty, 7}}, 1) + Cont;
%% 5: a NIF-created thread's message env send reaches the calling process, byte for byte.
run(5) ->
    R = nif_a3:thread_send(),
    M = recv(),
    bit(R =:= {{0, 1, 0}, 0}, 1) + bit(M =:= {from_thread, 42, <<"abc">>}, 2);
%% 6: an atom a foreign thread interned on its message env is the SAME atom here (a match, not a
%%    printed name).
run(6) ->
    R = nif_a3:worker_atoms(),
    M = recv(),
    bit(R =:= {0, 1, 1, 1, 0}, 1)
  + bit(case M of
            {worker_atom_check, worker_atom} -> true;
            _ -> false
        end, 2);
%% 7: the same send to a receiver that was verified PARKED in `receive` first.
run(7) ->
    Parent = self(),
    Pid = spawn(fun() -> receive Msg -> Parent ! {parked_got, Msg} end end),
    Ready = await_waiting(Pid, 4000),
    R = nif_a3:thread_send_to(Pid),
    M = recv(),
    bit(Ready =:= {status, waiting}, 1)
  + bit(R =:= {{0, 1, 0}, 0}, 2)
  + bit(M =:= {parked_got, {from_thread_to_target, 42}}, 4);
%% 8: arm READ then STOP: 17 = STOP_CALLED | READ_CANCELLED, the callback ran ONCE inline and saw
%%    the stopped object. The destructor is NOT read here: ERTS runs it as aux work, so its count
%%    is timing-dependent (case 12 is the deterministic dtor case).
run(8) ->
    ok = nif_a3:stop_reset(),
    {Arm, Stop, Calls, Direct, _Thread, Tag, Fd} = nif_a3:arm_then_stop(),
    {RCalls, RDirect, REvent, RTag, _RThread, _Dtors, _Dtag} = nif_a3:stop_report(),
    bit(Arm =:= 0 andalso Stop =:= 17, 1)
  + bit(Calls =:= 1 andalso Direct =:= 1 andalso Tag =:= 2, 2)
  + bit(RCalls =:= 1 andalso RDirect =:= 1 andalso RTag =:= 2
        andalso is_integer(REvent) andalso is_integer(Fd), 4);
%% 9: a STOP on a descriptor nothing ever armed takes the fast track (1) and still runs the
%%    callback once, inline, on a real descriptor.
run(9) ->
    ok = nif_a3:stop_reset(),
    {Code, Calls, Direct, Event, _Thread} = nif_a3:stop_unknown_fd(),
    bit(Code =:= 1, 1) + bit(Calls =:= 1, 2) + bit(Direct =:= 1, 4) + bit(is_integer(Event), 8);
%% 10: a second STOP runs the callback a SECOND time.
run(10) ->
    ok = nif_a3:stop_reset(),
    {Arm, S1, S2, Calls, Direct, _Thread, Tag} = nif_a3:double_stop(),
    bit(Arm =:= 0 andalso S1 =:= 17, 1)
  + bit(S2 =:= 1 andalso Calls =:= 2, 2)
  + bit(Direct =:= 1 andalso Tag =:= 3, 4);
%% 11: arm READ, CANCEL the read, then STOP: 16 = READ_CANCELLED, after which the STOP is the plain
%%     1 (there is no read interest left to cancel).
run(11) ->
    ok = nif_a3:stop_reset(),
    {Arm, Cancel, Stop, Calls, Direct, _Thread, Tag} = nif_a3:cancel_then_stop(),
    bit(Arm =:= 0 andalso Cancel =:= 16, 1)
  + bit(Stop =:= 1 andalso Calls =:= 1, 2)
  + bit(Direct =:= 1 andalso Tag =:= 4, 4);
%% 12: the destructor runs when the last reference is released — reached through a BOUNDED wait,
%%     because ERTS schedules it as aux work (`erl_nif.c:2961-2972`), never inline.
run(12) ->
    ok = nif_a3:stop_reset(),
    Before = nif_a3:dtor_report(),
    ok = nif_a3:dtor_release(),
    Waited = await_dtor(2000),
    After = nif_a3:dtor_report(),
    bit(Before =:= {0, -1}, 1)
  + bit(Waited =:= seen andalso After =:= {1, 5}, 2).

bit(true, B) -> B;
bit(false, _) -> 0.

guarded(F) ->
    try F() of
        V -> {ok, V}
    catch
        Class:Reason -> {raised, Class, Reason}
    end.

%% The stack movement across a scheduling chain: the ABSOLUTE size is runtime-specific, so only the
%% MOVEMENT is ever compared.
delta(F) ->
    Before = element(2, process_info(self(), stack_size)),
    _ = F(),
    After = element(2, process_info(self(), stack_size)),
    After - Before.

recv() -> receive Msg -> Msg after 5000 -> timeout end.

%% A bounded poll for the parked receiver: the precondition is OBSERVED, never assumed.
await_waiting(_Pid, 0) -> {status, unknown};
await_waiting(Pid, N) ->
    case process_info(Pid, status) of
        {status, waiting} -> {status, waiting};
        _ -> receive after 1 -> ok end, await_waiting(Pid, N - 1)
    end.

%% A bounded poll for the destructor: `seen` once it has run, `timeout` if it never does.
await_dtor(0) -> timeout;
await_dtor(N) ->
    case element(1, nif_a3:dtor_report()) of
        0 -> receive after 1 -> ok end, await_dtor(N - 1);
        _ -> seen
    end.

%% Only the reason TAG leaves this function: the Text of a load failure embeds the absolute temp-dir
%% path, which would make the golden drift on every run.
reason_code(ok) -> 0;
reason_code(load_failed) -> 1;
reason_code(bad_lib) -> 2;
reason_code(reload) -> 3;
reason_code(_) -> 9.
"#;
/// The library every case loads: one C file carrying the three seams.
///
/// It creates a single resource type in its `load` callback (with both a `stop` and a `dtor`
/// callback), spawns one thread per NIF-thread probe (each joined on every path) and keeps all of
/// its state in C statics — the probes are driven one at a time from one process. Nothing
/// C-variadic is used and every `enif_*` referenced here is a symbol the `beam_rt` release binary
/// exports.
const NIF_A3_C: &str = r#"
/*
 * One NIF library carrying the three A.3 host seams that are live in the Rust runtime:
 *
 *   1. `enif_schedule_nif`'s rescheduling trampoline — a NIF that re-schedules itself and the
 *      runtime hands the RESUMED entry's value back to the Erlang caller, both on the same
 *      scheduler (`flags = 0`) and on a dirty CPU scheduler (`ERL_NIF_DIRTY_JOB_CPU_BOUND`);
 *   2. the NIF-thread environment — a thread the NIF created calls `enif_alloc_env()`, builds a
 *      term and `enif_send(NULL, &pid, msg_env, msg)`, and the message reaches that mailbox;
 *   3. a resource type's `stop` / `dtor` callbacks — `stop` runs once per
 *      `enif_select(..., ERL_NIF_SELECT_STOP, ...)` and the `dtor` once when the object is freed.
 *
 * Every probe returns integers and atoms only (the select reference never leaves the call), so the
 * single integer line the module prints is a stable observable.
 */
#include <erl_nif.h>
#include <errno.h>
#include <string.h>
#include <unistd.h>

/* A flag word outside `ERL_NIF_DIRTY_JOB_CPU_BOUND | ERL_NIF_DIRTY_JOB_IO_BOUND`
   (`erl_nif.c:3584-3598`); `erl_nif.h` has no name for such a bit. */
#define BAD_FLAGS 0x1000
/* `argc` past `MAX_ARG` (255, `erl_vm.h:48`). */
#define TOO_MANY_ARGS 300
/* A code no real `enif_send` can answer, meaning "this step never ran". */
#define NOT_RUN (-1)

static ERL_NIF_TERM pair(ErlNifEnv* env, ERL_NIF_TERM a, ERL_NIF_TERM b)
{
    ERL_NIF_TERM arr[2];
    arr[0] = a;
    arr[1] = b;
    return enif_make_tuple_from_array(env, arr, 2);
}

static ERL_NIF_TERM triple(ErlNifEnv* env, ERL_NIF_TERM a, ERL_NIF_TERM b, ERL_NIF_TERM c)
{
    ERL_NIF_TERM arr[3];
    arr[0] = a;
    arr[1] = b;
    arr[2] = c;
    return enif_make_tuple_from_array(env, arr, 3);
}

/* ============================================================================================
 * 1. `enif_schedule_nif` — the rescheduling trampoline
 * ========================================================================================== */

/* Continuations entered in the current `sched_loop/1` chain (reset by the scheduling entry). */
static int loop_yields;
/* `enif_thread_type()` inside the SCHEDULING call of `sched_dirty_type/1`. */
static int caller_thread_type;

/* `{scheduled, N+M}` built from the arguments the CONTINUATION receives: they must be the ones the
   scheduling call passed to `enif_schedule_nif`. */
static ERL_NIF_TERM cont_sum(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    int n = 0;
    int m = 0;
    if (argc != 2)
        return enif_make_badarg(env);
    if (!enif_get_int(env, argv[0], &n) || !enif_get_int(env, argv[1], &m))
        return enif_make_badarg(env);
    return pair(env, enif_make_atom(env, "scheduled"), enif_make_int(env, n + m));
}

/* `flags = 0` is the header's spelling of "same scheduler" (the C only tests `flags == 0`,
   `erl_nif.c:3584`). The C returns whatever `enif_schedule_nif` answers; the value the CALLER sees
   comes from the continuation. */
static ERL_NIF_TERM sched_sum(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    return enif_schedule_nif(env, "cont_sum", 0, cont_sum, argc, argv);
}

/* Decrement argv[0] and reschedule until it reaches 0: a constant-stack trampoline chain. */
static ERL_NIF_TERM cont_loop(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    int n = 0;
    ERL_NIF_TERM next[1];
    if (argc != 1)
        return enif_make_badarg(env);
    if (!enif_get_int(env, argv[0], &n))
        return enif_make_badarg(env);
    loop_yields++;
    if (n <= 0)
        return pair(env, enif_make_atom(env, "done"), enif_make_int(env, loop_yields));
    next[0] = enif_make_int(env, n - 1);
    return enif_schedule_nif(env, "cont_loop", 0, cont_loop, 1, next);
}

static ERL_NIF_TERM sched_loop(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    loop_yields = 0;
    return enif_schedule_nif(env, "cont_loop", 0, cont_loop, argc, argv);
}

/* A raising continuation: `enif_raise_exception` answers THE_NON_VALUE too, so the chain stops and
   the CALLER sees the raise. */
static ERL_NIF_TERM cont_raise(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    (void)argc;
    (void)argv;
    return enif_raise_exception(env, enif_make_atom(env, "boom"));
}

static ERL_NIF_TERM sched_raise(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    return enif_schedule_nif(env, "cont_raise", 0, cont_raise, argc, argv);
}

/* The two refusals: an out-of-range flag word and an `argc` past MAX_ARG both RAISE `badarg`
   (`erl_nif.c:3569`, `:3584-3598`). */
static ERL_NIF_TERM sched_bad_flags(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    (void)argc;
    return enif_schedule_nif(env, "cont_sum", BAD_FLAGS, cont_sum, 0, argv);
}

static ERL_NIF_TERM sched_too_many_args(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM big[TOO_MANY_ARGS];
    int i;
    (void)argc;
    (void)argv;
    for (i = 0; i < TOO_MANY_ARGS; i++)
        big[i] = enif_make_int(env, i);
    return enif_schedule_nif(env, "cont_sum", 0, cont_sum, TOO_MANY_ARGS, big);
}

static ERL_NIF_TERM cont_dirty(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    int n = 0;
    if (argc != 1)
        return enif_make_badarg(env);
    if (!enif_get_int(env, argv[0], &n))
        return enif_make_badarg(env);
    return pair(env, enif_make_atom(env, "dirty"), enif_make_int(env, n));
}

/* `ERL_NIF_DIRTY_JOB_CPU_BOUND` makes the continuation the trampoline's direct function, so it runs
   ON a dirty CPU scheduler instead of the ordinary one. */
static ERL_NIF_TERM sched_dirty(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    return enif_schedule_nif(env, "cont_dirty", ERL_NIF_DIRTY_JOB_CPU_BOUND, cont_dirty, argc, argv);
}

/* `{threads, CallerType, ContType, Arg}` — `enif_thread_type()` in the scheduling call and in the
   continuation, plus the argument the continuation was handed. */
static ERL_NIF_TERM cont_ttype(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM arr[4];
    int arg = 0;
    if (argc != 1)
        return enif_make_badarg(env);
    if (!enif_get_int(env, argv[0], &arg))
        return enif_make_badarg(env);
    arr[0] = enif_make_atom(env, "threads");
    arr[1] = enif_make_int(env, caller_thread_type);
    arr[2] = enif_make_int(env, enif_thread_type());
    arr[3] = enif_make_int(env, arg);
    return enif_make_tuple_from_array(env, arr, 4);
}

static ERL_NIF_TERM sched_dirty_type(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    if (argc != 1)
        return enif_make_badarg(env);
    caller_thread_type = enif_thread_type();
    return enif_schedule_nif(env, "cont_ttype", ERL_NIF_DIRTY_JOB_CPU_BOUND, cont_ttype, argc, argv);
}

/* ============================================================================================
 * 2. The NIF-thread message environment
 *
 * `enif_alloc_env()` off the NIF call path hands out a standalone MESSAGE env (its phony proc has
 * `common.id == ERTS_INVALID_PID`, `erl_nif.c:636-680`), so the idiom
 *
 *     env = enif_alloc_env(); ... enif_send(NULL, &pid, env, term); enif_free_env(env);
 *
 * works verbatim from a thread the NIF itself created. Every worker below is joined from the call
 * path before the statics are read, so the reported values belong to a worker that has exited.
 * ========================================================================================== */

/* ---- thread_send/0 ---------------------------------------------------------------------- */

static ErlNifPid thread_send_caller;
static struct {
    volatile int done;
    volatile int env_is_null;
    volatile int send_code;
    volatile int thread_type;
} ts;

static void* thread_send_worker(void* arg)
{
    ERL_NIF_TERM msg, bin, arr[3];
    unsigned char* bytes;
    ErlNifEnv* msg_env;
    (void)arg;

    ts.env_is_null = 1;
    ts.send_code = NOT_RUN;
    ts.thread_type = enif_thread_type();

    msg_env = enif_alloc_env();
    if (msg_env != NULL) {
        ts.env_is_null = 0;
        bytes = enif_make_new_binary(msg_env, 3, &bin);
        if (bytes != NULL) {
            bytes[0] = 'a';
            bytes[1] = 'b';
            bytes[2] = 'c';
        }
        arr[0] = enif_make_atom(msg_env, "from_thread");
        arr[1] = enif_make_int(msg_env, 42);
        arr[2] = bin;
        msg = enif_make_tuple_from_array(msg_env, arr, 3);
        ts.send_code = enif_send(NULL, &thread_send_caller, msg_env, msg);
        enif_free_env(msg_env);
    }

    ts.done = 1;
    return NULL;
}

/* `{{EnvIsNull, SendCode, ThreadType}, ThreadCreated}` */
static ERL_NIF_TERM thread_send(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ErlNifTid tid;
    int created;
    (void)argc;
    (void)argv;

    (void)enif_self(env, &thread_send_caller);

    ts.done = 0;
    ts.env_is_null = 1;
    ts.send_code = NOT_RUN;
    ts.thread_type = NOT_RUN;

    created = enif_thread_create("nif_a3_send", &tid, thread_send_worker, NULL, NULL);
    if (created != 0)
        return enif_make_badarg(env);
    (void)enif_thread_join(tid, NULL);
    if (ts.done != 1)
        return enif_make_badarg(env);

    return pair(env,
                triple(env,
                       enif_make_int(env, ts.env_is_null),
                       enif_make_int(env, ts.send_code),
                       enif_make_int(env, ts.thread_type)),
                enif_make_int(env, created));
}

/* ---- worker_atoms/0 --------------------------------------------------------------------- */

/* The name the worker interns on its own message env and reads back. */
static const char worker_atom_name[] = "worker_atom";
#define WORKER_ATOM_LEN (sizeof(worker_atom_name) - 1)

static ErlNifPid worker_atoms_caller;
static struct {
    volatile int done;
    volatile int env_is_null;
    volatile int atom_len_ok;
    volatile int atom_text_ok;
    volatile int send_code;
    volatile int thread_type;
} wa;

static void* worker_atoms_worker(void* arg)
{
    ERL_NIF_TERM atom, msg, arr[2];
    unsigned len = 0;
    char buf[32];
    int get_ret = 0;
    ErlNifEnv* msg_env;
    (void)arg;

    wa.env_is_null = 1;
    wa.atom_len_ok = -1;
    wa.atom_text_ok = -1;
    wa.send_code = NOT_RUN;
    wa.thread_type = enif_thread_type();

    msg_env = enif_alloc_env();
    if (msg_env != NULL) {
        wa.env_is_null = 0;
        /* A name a foreign thread interns: it must land in the ONE global atom table. */
        atom = enif_make_atom(msg_env, worker_atom_name);
        wa.atom_len_ok = enif_get_atom_length(msg_env, atom, &len, ERL_NIF_LATIN1);
        memset(buf, 0, sizeof(buf));
        get_ret = enif_get_atom(msg_env, atom, buf, (unsigned)sizeof(buf), ERL_NIF_LATIN1);
        wa.atom_text_ok = (get_ret != 0
                           && len == (unsigned)WORKER_ATOM_LEN
                           && memcmp(buf, worker_atom_name, WORKER_ATOM_LEN) == 0);

        arr[0] = enif_make_atom(msg_env, "worker_atom_check");
        arr[1] = atom;
        msg = enif_make_tuple_from_array(msg_env, arr, 2);
        wa.send_code = enif_send(NULL, &worker_atoms_caller, msg_env, msg);
        enif_free_env(msg_env);
    }

    wa.done = 1;
    return NULL;
}

/* `{EnvIsNull, AtomLenOk, AtomTextOk, SendCode, ThreadType}` */
static ERL_NIF_TERM worker_atoms(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM arr[5];
    ErlNifTid tid;
    int created;
    (void)argc;
    (void)argv;

    (void)enif_self(env, &worker_atoms_caller);

    wa.done = 0;
    wa.env_is_null = 1;
    wa.atom_len_ok = -1;
    wa.atom_text_ok = -1;
    wa.send_code = NOT_RUN;
    wa.thread_type = NOT_RUN;

    created = enif_thread_create("nif_a3_atoms", &tid, worker_atoms_worker, NULL, NULL);
    if (created != 0)
        return enif_make_badarg(env);
    (void)enif_thread_join(tid, NULL);
    if (wa.done != 1)
        return enif_make_badarg(env);

    arr[0] = enif_make_int(env, wa.env_is_null);
    arr[1] = enif_make_int(env, wa.atom_len_ok);
    arr[2] = enif_make_int(env, wa.atom_text_ok);
    arr[3] = enif_make_int(env, wa.send_code);
    arr[4] = enif_make_int(env, wa.thread_type);
    return enif_make_tuple_from_array(env, arr, 5);
}

/* ---- thread_send_to/1 ------------------------------------------------------------------- */

/* The same worker as `thread_send/0`, but the receiver is the PID argument (so the caller can be a
   process parked in `receive`) and the message is a distinguishable one. */
static ErlNifPid to_target;
static struct {
    volatile int done;
    volatile int env_is_null;
    volatile int send_code;
    volatile int thread_type;
} tt;

static void* thread_send_to_worker(void* arg)
{
    ERL_NIF_TERM msg, arr[2];
    ErlNifEnv* msg_env;
    (void)arg;

    tt.env_is_null = 1;
    tt.send_code = NOT_RUN;
    tt.thread_type = enif_thread_type();

    msg_env = enif_alloc_env();
    if (msg_env != NULL) {
        tt.env_is_null = 0;
        arr[0] = enif_make_atom(msg_env, "from_thread_to_target");
        arr[1] = enif_make_int(msg_env, 42);
        msg = enif_make_tuple_from_array(msg_env, arr, 2);
        tt.send_code = enif_send(NULL, &to_target, msg_env, msg);
        enif_free_env(msg_env);
    }

    tt.done = 1;
    return NULL;
}

/* `{{EnvIsNull, SendCode, ThreadType}, ThreadCreated}` to the pid the caller passed in. */
static ERL_NIF_TERM thread_send_to(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ErlNifTid tid;
    int created;
    (void)argc;

    if (argc != 1 || !enif_get_local_pid(env, argv[0], &to_target))
        return enif_make_badarg(env);

    tt.done = 0;
    tt.env_is_null = 1;
    tt.send_code = NOT_RUN;
    tt.thread_type = NOT_RUN;

    created = enif_thread_create("nif_a3_to", &tid, thread_send_to_worker, NULL, NULL);
    if (created != 0)
        return enif_make_badarg(env);
    (void)enif_thread_join(tid, NULL);
    if (tt.done != 1)
        return enif_make_badarg(env);

    return pair(env,
                triple(env,
                       enif_make_int(env, tt.env_is_null),
                       enif_make_int(env, tt.send_code),
                       enif_make_int(env, tt.thread_type)),
                enif_make_int(env, created));
}

/* ============================================================================================
 * 3. The resource `stop` / `dtor` callbacks
 *
 * The `stop` callback records into plain C statics: the call count, `is_direct_call`, the
 * `ErlNifEvent`, the object's payload tag and `enif_thread_type()` of the thread that ran it. The
 * `dtor` records its own count and the tag it saw. `stop_report/0` reads them back, `stop_reset/0`
 * zeroes them between probes (the statics are module-global and the probes are driven one at a
 * time from one process).
 * ========================================================================================== */

static unsigned long stop_calls;
static int last_is_direct;      /* -1 = the callback never ran */
static long last_event;         /* -1 = the callback never ran */
static int last_tag;
static int last_thread_type;
static unsigned long dtor_calls;
static int dtor_last_tag;

static ErlNifResourceType* res_stop;

typedef struct {
    int tag; /* identifies which probe's object a callback belongs to */
} probe_obj;

static void stop_cb(ErlNifEnv* env, void* obj, ErlNifEvent event, int is_direct_call)
{
    (void)env;
    stop_calls++;
    last_is_direct = is_direct_call;
    last_event = (long)event;
    last_tag = ((probe_obj*)obj)->tag;
    last_thread_type = enif_thread_type();
}

static void dtor_cb(ErlNifEnv* env, void* obj)
{
    (void)env;
    dtor_calls++;
    dtor_last_tag = ((probe_obj*)obj)->tag;
}

static int nif_load(ErlNifEnv* env, void** priv_data, ERL_NIF_TERM load_info)
{
    ErlNifResourceTypeInit init;
    (void)priv_data;
    (void)load_info;

    memset(&init, 0, sizeof(init));
    init.dtor = dtor_cb;
    init.stop = stop_cb;
    init.down = NULL;
    init.members = 3;
    init.dyncall = NULL;

    res_stop = enif_init_resource_type(env, "nif_a3_stop", &init,
                                      (ErlNifResourceFlags)(ERL_NIF_RT_CREATE
                                                            | ERL_NIF_RT_TAKEOVER),
                                      NULL);
    if (res_stop == NULL)
        return 1;
    return 0;
}

static probe_obj* alloc_obj(int tag)
{
    probe_obj* obj = (probe_obj*)enif_alloc_resource(res_stop, sizeof(probe_obj));
    if (obj != NULL)
        obj->tag = tag;
    return obj;
}

/* A pipe the probe owns; returns 0 on success, errno on failure. */
static int make_pipe(int fds[2])
{
    if (pipe(fds) != 0)
        return errno;
    return 0;
}

static ERL_NIF_TERM pipe_error(ErlNifEnv* env, int err)
{
    return pair(env, enif_make_atom(env, "pipe_failed"), enif_make_int(env, err));
}

static ERL_NIF_TERM alloc_error(ErlNifEnv* env)
{
    return enif_make_atom(env, "alloc_resource_failed");
}

/* Zero every recorded static between probes (answers `ok`). */
static ERL_NIF_TERM stop_reset(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    (void)argc;
    (void)argv;
    stop_calls = 0;
    last_is_direct = -1;
    last_event = -1;
    last_tag = -1;
    last_thread_type = -1;
    dtor_calls = 0;
    dtor_last_tag = -1;
    return enif_make_atom(env, "ok");
}

/* `{StopCalls, LastIsDirect, LastEvent, LastTag, LastThreadType, DtorCalls, DtorLastTag}` */
static ERL_NIF_TERM stop_report(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM a[7];
    (void)argc;
    (void)argv;

    a[0] = enif_make_ulong(env, stop_calls);
    a[1] = enif_make_int(env, last_is_direct);
    a[2] = enif_make_long(env, last_event);
    a[3] = enif_make_int(env, last_tag);
    a[4] = enif_make_int(env, last_thread_type);
    a[5] = enif_make_ulong(env, dtor_calls);
    a[6] = enif_make_int(env, dtor_last_tag);
    return enif_make_tuple_from_array(env, a, 7);
}

/* STOP on a pipe descriptor nothing ever armed: the C's fast track (`erl_check_io.c:1278-1285`)
   answers STOP_CALLED and calls the callback inline (`is_direct_call = 1`).
   `{Code, StopCalls, LastIsDirect, LastEvent, LastThreadType}` */
static ERL_NIF_TERM stop_unknown_fd(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM a[5];
    ERL_NIF_TERM ref = enif_make_ref(env);
    probe_obj* obj;
    int fds[2];
    int code;
    (void)argc;
    (void)argv;

    if (make_pipe(fds) != 0)
        return pipe_error(env, errno);
    obj = alloc_obj(1);
    if (obj == NULL) {
        close(fds[0]);
        close(fds[1]);
        return alloc_error(env);
    }
    code = enif_select(env, fds[0], ERL_NIF_SELECT_STOP, obj, NULL, ref);

    a[0] = enif_make_int(env, code);
    a[1] = enif_make_ulong(env, stop_calls);
    a[2] = enif_make_int(env, last_is_direct);
    a[3] = enif_make_long(env, last_event);
    a[4] = enif_make_int(env, last_thread_type);

    close(fds[0]);
    close(fds[1]);
    enif_release_resource(obj);
    return enif_make_tuple_from_array(env, a, 5);
}

/* arm READ, then STOP on the same descriptor: the STOP answers
   `ERL_NIF_SELECT_STOP_CALLED | ERL_NIF_SELECT_READ_CANCELLED` (17) and runs the callback once,
   synchronously.
   `{ArmCode, StopCode, StopCalls, LastIsDirect, LastThreadType, LastTag, Fd}` */
static ERL_NIF_TERM arm_then_stop(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM a[7];
    ERL_NIF_TERM ref = enif_make_ref(env);
    probe_obj* obj;
    int fds[2];
    int arm_code;
    int stop_code;
    (void)argc;
    (void)argv;

    if (make_pipe(fds) != 0)
        return pipe_error(env, errno);
    obj = alloc_obj(2);
    if (obj == NULL) {
        close(fds[0]);
        close(fds[1]);
        return alloc_error(env);
    }

    arm_code = enif_select(env, fds[0], ERL_NIF_SELECT_READ, obj, NULL, ref);
    stop_code = enif_select(env, fds[0], ERL_NIF_SELECT_STOP, obj, NULL, ref);

    a[0] = enif_make_int(env, arm_code);
    a[1] = enif_make_int(env, stop_code);
    a[2] = enif_make_ulong(env, stop_calls);
    a[3] = enif_make_int(env, last_is_direct);
    a[4] = enif_make_int(env, last_thread_type);
    a[5] = enif_make_int(env, last_tag);
    a[6] = enif_make_int(env, fds[0]);

    close(fds[0]);
    close(fds[1]);
    enif_release_resource(obj);
    return enif_make_tuple_from_array(env, a, 7);
}

/* arm READ, STOP, STOP: the first STOP completes, so the second takes the fast track again and the
   callback runs a SECOND time.
   `{ArmCode, StopCode1, StopCode2, StopCalls, LastIsDirect, LastThreadType, LastTag}` */
static ERL_NIF_TERM double_stop(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM a[7];
    ERL_NIF_TERM ref = enif_make_ref(env);
    probe_obj* obj;
    int fds[2];
    int arm_code;
    int first_code;
    int second_code;
    (void)argc;
    (void)argv;

    if (make_pipe(fds) != 0)
        return pipe_error(env, errno);
    obj = alloc_obj(3);
    if (obj == NULL) {
        close(fds[0]);
        close(fds[1]);
        return alloc_error(env);
    }

    arm_code = enif_select(env, fds[0], ERL_NIF_SELECT_READ, obj, NULL, ref);
    first_code = enif_select(env, fds[0], ERL_NIF_SELECT_STOP, obj, NULL, ref);
    second_code = enif_select(env, fds[0], ERL_NIF_SELECT_STOP, obj, NULL, ref);

    a[0] = enif_make_int(env, arm_code);
    a[1] = enif_make_int(env, first_code);
    a[2] = enif_make_int(env, second_code);
    a[3] = enif_make_ulong(env, stop_calls);
    a[4] = enif_make_int(env, last_is_direct);
    a[5] = enif_make_int(env, last_thread_type);
    a[6] = enif_make_int(env, last_tag);

    close(fds[0]);
    close(fds[1]);
    enif_release_resource(obj);
    return enif_make_tuple_from_array(env, a, 7);
}

/* arm READ, CANCEL the read, then STOP: the CANCEL answers `ERL_NIF_SELECT_READ_CANCELLED` (16),
   after which the STOP is the plain `1` (there is no read interest left to cancel).
   `{ArmCode, CancelCode, StopCode, StopCalls, LastIsDirect, LastThreadType, LastTag}` */
static ERL_NIF_TERM cancel_then_stop(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM a[7];
    ERL_NIF_TERM ref = enif_make_ref(env);
    probe_obj* obj;
    int fds[2];
    int arm_code;
    int cancel_code;
    int stop_code;
    (void)argc;
    (void)argv;

    if (make_pipe(fds) != 0)
        return pipe_error(env, errno);
    obj = alloc_obj(4);
    if (obj == NULL) {
        close(fds[0]);
        close(fds[1]);
        return alloc_error(env);
    }

    arm_code = enif_select(env, fds[0], ERL_NIF_SELECT_READ, obj, NULL, ref);
    cancel_code = enif_select(env, fds[0],
                              (enum ErlNifSelectFlags)(ERL_NIF_SELECT_READ
                                                       | ERL_NIF_SELECT_CANCEL),
                              obj, NULL, ref);
    stop_code = enif_select(env, fds[0], ERL_NIF_SELECT_STOP, obj, NULL, ref);

    a[0] = enif_make_int(env, arm_code);
    a[1] = enif_make_int(env, cancel_code);
    a[2] = enif_make_int(env, stop_code);
    a[3] = enif_make_ulong(env, stop_calls);
    a[4] = enif_make_int(env, last_is_direct);
    a[5] = enif_make_int(env, last_thread_type);
    a[6] = enif_make_int(env, last_tag);

    close(fds[0]);
    close(fds[1]);
    enif_release_resource(obj);
    return enif_make_tuple_from_array(env, a, 7);
}

/* `{DtorCalls, DtorLastTag}` — the destructor's own statics. */
static ERL_NIF_TERM dtor_report(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    (void)argc;
    (void)argv;
    return pair(env, enif_make_ulong(env, dtor_calls), enif_make_int(env, dtor_last_tag));
}

/* Allocate one object and drop the ONLY reference there is (no `enif_select` involved), so the
   destructor is the single thing that can account for it. Answers `ok`: ERTS runs a resource
   destructor as AUX WORK (`nif_resource_dtor_prologue`, `erl_nif.c:2961-2972`), never inline, so
   *when* it runs is the module's bounded wait, not this call. */
static ERL_NIF_TERM dtor_release(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    probe_obj* obj;
    (void)argc;
    (void)argv;

    obj = alloc_obj(5);
    if (obj == NULL)
        return alloc_error(env);
    enif_release_resource(obj);
    return enif_make_atom(env, "ok");
}

static ErlNifFunc nif_funcs[] = {
    /* the rescheduling trampoline */
    {"sched_sum", 2, sched_sum, 0},
    {"sched_loop", 1, sched_loop, 0},
    {"sched_raise", 0, sched_raise, 0},
    {"sched_bad_flags", 0, sched_bad_flags, 0},
    {"sched_too_many_args", 0, sched_too_many_args, 0},
    {"sched_dirty", 1, sched_dirty, 0},
    {"sched_dirty_type", 1, sched_dirty_type, 0},
    /* The continuations are entered by POINTER, but each still needs a row here: the load refuses
       a row that is not a real function of the module, which is why the module exports a stub for
       each of them. */
    {"cont_sum", 2, cont_sum, 0},
    {"cont_loop", 1, cont_loop, 0},
    {"cont_raise", 0, cont_raise, 0},
    {"cont_dirty", 1, cont_dirty, 0},
    {"cont_ttype", 1, cont_ttype, 0},
    /* the NIF-thread message environment */
    {"thread_send", 0, thread_send, 0},
    {"worker_atoms", 0, worker_atoms, 0},
    {"thread_send_to", 1, thread_send_to, 0},
    /* the resource stop / dtor callbacks */
    {"stop_reset", 0, stop_reset, 0},
    {"stop_report", 0, stop_report, 0},
    {"stop_unknown_fd", 0, stop_unknown_fd, 0},
    {"arm_then_stop", 0, arm_then_stop, 0},
    {"double_stop", 0, double_stop, 0},
    {"cancel_then_stop", 0, cancel_then_stop, 0},
    {"dtor_report", 0, dtor_report, 0},
    {"dtor_release", 0, dtor_release, 0}
};

ERL_NIF_INIT(nif_a3, nif_funcs, nif_load, NULL, NULL, NULL)
"#;

/// The library every case loads.
const NIF_A3: NifFixture = NifFixture::CompiledLibrary {
    name: "nif_a3",
    c_source: NIF_A3_C,
};

/// Build one module-run case over [`NIF_A3_MODULE_SOURCE`].
fn case(
    id: &'static str,
    description: &'static str,
    selector: &'static [i64],
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts: &[Contract::NifAbi, Contract::BeamAsm],
        kind: CaseKind::Module(ModuleRun {
            source: NIF_A3_MODULE_SOURCE,
            module: "nif_a3",
            function: "classify",
            arity: 1,
            args: selector,
            nif: Some(NIF_A3),
        }),
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// The three closed A.3 host seams. Each golden is the integer the **oracle** prints (measured with
/// the system `erl` over the same `.so` and the same module, re-verified per case with
/// `differential run <id>`).
pub(crate) fn group() -> Vec<Case> {
    vec![
        case(
            "nif_a3/schedule_nif_resumes_its_continuation_value",
            "enif_schedule_nif with flags = 0 resumes the continuation and its own value — computed from the argv the scheduling call passed — is what the Erlang caller receives",
            &[1],
            // 1 (sched_sum(2,3) = {scheduled,5}) + 2 (sched_sum(20,3) = {scheduled,23}).
            "3",
        ),
        case(
            "nif_a3/schedule_nif_chain_completes_at_a_constant_stack",
            "a 100 000-step self-rescheduling chain answers {done,100001} and a 1-step chain moves the stack by exactly as much, so the trampoline is constant-stack",
            &[2],
            // 1 (the long chain's value) + 2 (the 1-step and 100 000-step stack deltas are equal).
            "3",
        ),
        case(
            "nif_a3/schedule_nif_raise_and_refusals_reach_the_caller",
            "a raising continuation stops the chain and the caller sees error:boom, and both refusals (flags = 0x1000, argc = 300) raise badarg",
            &[3],
            // 1 (error:boom) + 2 (bad flags raise) + 4 (too many args raise).
            "7",
        ),
        case(
            "nif_a3/dirty_schedule_nif_resumes_on_a_dirty_cpu_scheduler",
            "an ERL_NIF_DIRTY_JOB_CPU_BOUND schedule delivers {dirty,7} and the continuation's enif_thread_type() is ERL_NIF_THR_DIRTY_CPU, so it really ran on a dirty CPU scheduler",
            &[4],
            // 1 (the value) + 2 (the continuation's thread type is 2 and it saw argv[0] = 1).
            "3",
        ),
        case(
            "nif_a3/nif_thread_env_send_reaches_the_owning_process",
            "a NIF-created thread allocates a message env, builds a term and enif_send(NULL, …)s it to the calling pid: the send answers 1 and the caller receives exactly {from_thread,42,<<\"abc\">>}",
            &[5],
            // 1 ({{0,1,0},0}: a non-NULL env, enif_send 1, the created thread's kind 0, created) +
            // 2 (the message arrived byte for byte).
            "3",
        ),
        case(
            "nif_a3/nif_thread_env_atom_identity_crosses_by_name",
            "an atom a NIF-created thread interned on its own message env reads back by length and text and is the SAME atom in the receiving module (a match against its literal, not a printed name)",
            &[6],
            // 1 ({0,1,1,1,0}: env, length, text, send code, thread kind) +
            // 2 (the receiver matched the atom against its own literal).
            "3",
        ),
        case(
            "nif_a3/nif_thread_env_send_wakes_a_parked_receiver",
            "the same NIF-thread send to a receiver verified to be parked in receive first: the message {parked_got,{from_thread_to_target,42}} reaches its mailbox",
            &[7],
            // 1 (the receiver was {status,waiting} before the call) + 2 (the send tuple) +
            // 4 (the parked receiver got the exact message).
            "7",
        ),
        case(
            "nif_a3/resource_stop_runs_once_with_the_descriptor",
            "arm READ then STOP answers 17 (STOP_CALLED | READ_CANCELLED), the stop callback ran exactly once, inline (is_direct_call = 1) and saw the stopped object",
            &[8],
            // 1 (arm 0, stop 17) + 2 (one call, direct, payload tag 2) +
            // 4 (the report agrees and both the event and the fd are real descriptors).
            "7",
        ),
        case(
            "nif_a3/resource_stop_on_an_unarmed_descriptor_takes_the_fast_track",
            "a STOP on a descriptor nothing ever armed answers STOP_CALLED (1) and still runs the stop callback once, inline, with a real descriptor",
            &[9],
            // 1 (code 1) + 2 (one call) + 4 (direct) + 8 (a real descriptor).
            "15",
        ),
        case(
            "nif_a3/resource_stop_twice_runs_the_callback_twice",
            "arm READ, STOP, STOP: the first STOP is 17, the second is the fast-track 1, and the stop callback has run twice",
            &[10],
            // 1 (arm 0, first stop 17) + 2 (second stop 1, two calls) + 4 (direct, payload tag 3).
            "7",
        ),
        case(
            "nif_a3/resource_cancel_then_stop_cancels_the_read",
            "arm READ, CANCEL (16 = READ_CANCELLED), then STOP: the STOP is the plain STOP_CALLED 1, because there is no read interest left to cancel",
            &[11],
            // 1 (arm 0, cancel 16) + 2 (stop 1, one call) + 4 (direct, payload tag 4).
            "7",
        ),
        case(
            "nif_a3/resource_dtor_runs_when_the_last_reference_is_released",
            "dropping a resource object's only reference in a NIF reaches exactly one destructor call carrying that object's payload (bounded wait: ERTS runs a destructor as aux work)",
            &[12],
            // 1 (a clean slate before the release) + 2 (the destructor ran once, on the right object).
            "3",
        ),
    ]
}

#[cfg(test)]
mod tests {
    //! Invariants of the `nif_a3` group.

    use super::{group, CaseKind, Contract, NifFixture, NIF_A3_C, NIF_A3_MODULE_SOURCE};
    use std::collections::HashSet;

    /// The probe functions the module must export a stub for and call **externally**, each with a
    /// matching `ErlNifFunc` row in the library.
    const PROBES: &[&str] = &[
        "sched_sum",
        "sched_loop",
        "sched_raise",
        "sched_bad_flags",
        "sched_too_many_args",
        "sched_dirty",
        "sched_dirty_type",
        "thread_send",
        "worker_atoms",
        "thread_send_to",
        "stop_reset",
        "stop_report",
        "stop_unknown_fd",
        "arm_then_stop",
        "double_stop",
        "cancel_then_stop",
        "dtor_report",
        "dtor_release",
    ];

    /// The continuations: entered **by pointer** from the trampoline, exported only so the load's
    /// table check accepts the C rows.
    const CONTINUATIONS: &[(&str, u8)] = &[
        ("cont_sum", 2),
        ("cont_loop", 1),
        ("cont_raise", 0),
        ("cont_dirty", 1),
        ("cont_ttype", 1),
    ];

    #[test]
    fn every_case_is_a_nif_module_run_over_the_same_compiled_library() {
        let mut selectors = HashSet::new();
        for case in group() {
            assert!(
                case.id.starts_with("nif_a3/"),
                "case {} is outside the nif_a3 group",
                case.id
            );
            assert!(
                !case.description.is_empty(),
                "case {} has no description",
                case.id
            );
            assert_eq!(
                case.contracts,
                &[Contract::NifAbi, Contract::BeamAsm],
                "case {}: an end-to-end NIF case evidences A.3 and the compiled .beam (A.2)",
                case.id
            );
            let CaseKind::Module(run) = case.kind else {
                panic!("case {}: an A.3 case must be a module-run case", case.id);
            };
            assert_eq!(run.module, "nif_a3", "case {}", case.id);
            assert_eq!(run.function, "classify", "case {}", case.id);
            assert_eq!(run.arity, 1, "case {}", case.id);
            assert_eq!(
                run.args.len(),
                run.arity as usize,
                "case {}: one selector per arity slot",
                case.id
            );
            assert!(
                selectors.insert(run.args[0]),
                "case {}: selector reused",
                case.id
            );
            assert!(
                matches!(run.nif, Some(NifFixture::CompiledLibrary { .. })),
                "case {}: the library must be C-compiled",
                case.id
            );
            assert!(
                case.golden
                    .stdout
                    .expect("every case pins a golden")
                    .chars()
                    .all(|c| c.is_ascii_digit()),
                "case {}: the golden must be a plain integer line (a path-bearing \
                 {{error, {{Reason, Text}}}} or a per-VM value would drift), got {:?}",
                case.id,
                case.golden.stdout
            );
            assert_eq!(case.golden.exit_code, 0, "case {}", case.id);
        }
        // Twelve cases, one selector each: four for the trampoline, three for the NIF-thread
        // environment, five for the resource callbacks.
        assert_eq!(
            selectors.len(),
            12,
            "the group covers twelve distinct selectors"
        );
        assert_eq!(group().len(), 12, "the group carries twelve cases");
    }

    #[test]
    fn every_case_names_the_c_library() {
        // The `.so` name the module loads is the fixture's own name plus whatever the platform
        // appends, so a case whose fixture is unnamed (or named differently) would never load —
        // and `ERL_NIF_INIT` seeds the library's module name, so the two must agree.
        for case in group() {
            let CaseKind::Module(run) = case.kind else {
                panic!("case {}: an A.3 case must be a module-run case", case.id);
            };
            assert_eq!(
                run.nif,
                Some(NifFixture::CompiledLibrary {
                    name: "nif_a3",
                    c_source: NIF_A3_C,
                }),
                "case {}: the fixture must be the group's own compiled library",
                case.id
            );
            assert!(
                NIF_A3_MODULE_SOURCE.contains("Dir ++ \"/nif_a3\""),
                "case {}: the module must load the library the fixture builds",
                case.id
            );
            assert!(
                NIF_A3_C.contains("ERL_NIF_INIT(nif_a3,"),
                "case {}: the library must register itself as nif_a3 (load_nif/2 requires the \
                 loading module's name)",
                case.id
            );
        }
    }

    #[test]
    fn every_call_into_the_library_is_external() {
        // A *local* call reaches the `erlang:nif_error/1` stub and raises `not_loaded` on the Rust
        // runtime only — a guaranteed false mismatch, so the shape is asserted here.
        for probe in PROBES {
            let stub = format!("\n{probe}(");
            assert!(
                NIF_A3_MODULE_SOURCE.contains(&stub),
                "the module must export a stub for {probe}"
            );
            let external = format!("nif_a3:{probe}(");
            assert!(
                NIF_A3_MODULE_SOURCE.contains(&external),
                "the module must call {probe} through `nif_a3:` (external dispatch)"
            );
            assert!(
                NIF_A3_C.contains(&format!("\"{probe}\"")),
                "the library must register {probe}"
            );
        }
        for (continuation, arity) in CONTINUATIONS {
            assert!(
                NIF_A3_MODULE_SOURCE.contains(&format!("\n{continuation}(")),
                "the module must export a stub for {continuation} (entered by pointer, but the \
                 load validates every ErlNifFunc row)"
            );
            assert!(
                NIF_A3_C.contains(&format!("\"{continuation}\", {arity},")),
                "the library must register {continuation}/{arity}"
            );
        }
        // ...and no probe name may ever be *invoked* locally: every occurrence of `name(` in the
        // module is either the stub's own head (start of line) or a `nif_a3:` external call.
        for line in NIF_A3_MODULE_SOURCE.lines() {
            for name in PROBES
                .iter()
                .chain(CONTINUATIONS.iter().map(|(name, _)| name))
            {
                let Some(at) = line.find(&format!("{name}(")) else {
                    continue;
                };
                let before = line[..at].trim_end();
                assert!(
                    before.is_empty() || before.ends_with("nif_a3:"),
                    "{name} is called locally here, which only the oracle can dispatch: {line:?}"
                );
            }
        }
    }

    #[test]
    fn the_library_references_the_three_seams_it_pins() {
        // What makes the group the *closed gaps* group: the scheduling trampoline (plain and
        // dirty), the thread-side message env, and the resource stop/dtor pair. A fixture that
        // dropped one would pin less.
        for symbol in [
            "enif_schedule_nif",
            "ERL_NIF_DIRTY_JOB_CPU_BOUND",
            "enif_thread_create",
            "enif_thread_join",
            "enif_alloc_env",
            "enif_free_env",
            "enif_send(NULL",
            "enif_init_resource_type",
            "enif_alloc_resource",
            "enif_release_resource",
            "ERL_NIF_SELECT_STOP",
            "ERL_NIF_SELECT_READ",
            "ERL_NIF_SELECT_CANCEL",
        ] {
            assert!(
                NIF_A3_C.contains(symbol),
                "the library must reference {symbol}"
            );
        }
        // The `stop` and `dtor` callbacks exist and are both wired into the type's init struct —
        // that wiring IS the seam under test.
        assert!(
            NIF_A3_C.contains("init.stop = stop_cb;"),
            "the type must carry a stop callback"
        );
        assert!(
            NIF_A3_C.contains("init.dtor = dtor_cb;"),
            "the type must carry a dtor callback"
        );
        assert!(
            NIF_A3_C.contains("static void stop_cb(ErlNifEnv* env, void* obj, ErlNifEvent event, int is_direct_call)"),
            "the stop callback must observe its event, its object and is_direct_call"
        );
    }

    #[test]
    fn every_wait_is_bounded_and_every_thread_is_joined() {
        // No native wait at all beyond `enif_thread_join`: the fixture cannot hang.
        assert!(
            !NIF_A3_C.contains("nanosleep"),
            "the fixture must not sleep (a bounded Erlang poll is the only wait it needs)"
        );
        assert!(
            !NIF_A3_C.contains("while (1)"),
            "the fixture must not carry an unbounded loop"
        );
        assert_eq!(
            NIF_A3_C.matches("enif_thread_create(").count(),
            NIF_A3_C.matches("enif_thread_join(").count(),
            "every created thread must be joined"
        );
        assert_eq!(
            NIF_A3_C.matches("enif_thread_create(").count(),
            3,
            "the three NIF-thread probes are the only thread creators"
        );
        // The case-level waits are deadlined receives (or bounded polls that yield).
        for wait in [
            "after 5000 -> timeout",
            "await_waiting(Pid, 4000)",
            "receive after 1 -> ok end",
            "await_dtor(2000)",
            "await_dtor(0) -> timeout",
        ] {
            assert!(
                NIF_A3_MODULE_SOURCE.contains(wait),
                "the module must bound its waits with {wait:?}"
            );
        }
    }

    #[test]
    fn no_volatile_value_reaches_the_golden() {
        // A golden is a bit aggregate: a small integer, never a path, a pid or a timing figure.
        for case in group() {
            let golden: u32 = case
                .golden
                .stdout
                .expect("every case pins a golden")
                .parse()
                .expect("a decimal integer");
            assert!(
                golden < 16,
                "case {}: the golden packs more than four bits",
                case.id
            );
        }
        // The module prints nothing of its own: the only stdout is the result the `run` action
        // prints, so nothing can smuggle a host-dependent figure into the compared line.
        assert!(
            !NIF_A3_MODULE_SOURCE.contains("io:format"),
            "the module must not print: the golden is its return value alone"
        );
        // The library builds no per-VM term...
        for banned in [
            "enif_make_pid(",
            "enif_whereis_pid(",
            "enif_make_unique_integer(",
            "enif_make_monitor_term(",
            "enif_system_info(",
            "enif_dlopen(",
        ] {
            assert!(
                !NIF_A3_C.contains(banned),
                "the library must not build a per-VM value ({banned})"
            );
        }
        // ...the ONE per-VM value it creates is `enif_select`'s reference, which is handed to the
        // call and never placed into a returned tuple...
        assert_eq!(
            NIF_A3_C.matches("enif_make_ref(").count(),
            4,
            "each select probe creates exactly one reference"
        );
        for line in NIF_A3_C
            .lines()
            .filter(|line| line.contains("enif_make_ref"))
        {
            assert!(
                line.trim() == "ERL_NIF_TERM ref = enif_make_ref(env);",
                "a select reference must be created on its own line, never inside a returned \
                 tuple: {line:?}"
            );
        }
        // ...and `enif_self/2` is only ever used to name a send TARGET.
        assert_eq!(
            NIF_A3_C.matches("enif_self(").count(),
            2,
            "the two self-sends are the only callers of enif_self/2"
        );
        for line in NIF_A3_C.lines().filter(|line| line.contains("enif_self")) {
            assert!(
                line.trim().starts_with("(void)enif_self(env, &"),
                "enif_self/2 must only name a send target: {line:?}"
            );
        }
    }
}
