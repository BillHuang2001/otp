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

//! Contract **A.3** — the **threading / scheduling / cross-process** half of the `enif_*` surface.
//!
//! [`super::nif_state`] pins the *stateful* families (monitors, `enif_select`, resource identity),
//! [`super::nif_res`] the resource / ioq / sync families, [`super::nif_so`] the load/call surface and
//! [`super::nif_abi`] / [`super::nif_abi_extra`] the frozen structs plus every `enif_*` *shape*. This
//! group pins what the **threading, blocking and cross-process** items actually **do**:
//!
//! * `threads_share_a_mutex_guarded_counter` — two NIF-created threads (`enif_thread_create`) walk
//!   one `enif_mutex`-guarded counter 500 times each; every critical section sees `inside == 1`
//!   (no overlap), `enif_thread_join` reports each entry point's **own** return value, and the
//!   counter is exactly `1000`. Aggregate `counter * 10 + overlaps` = `10000`.
//! * `thread_identity_and_kind_differ_from_the_caller` — inside the new thread
//!   `enif_thread_self` does **not** equal the caller's `ErlNifTid` (`enif_equal_tids` ⇒ `0`) and
//!   `enif_thread_type` answers `ERL_NIF_THR_UNDEFINED` (a NIF-created thread is no scheduler
//!   thread, `erl_nif.c:3607-3612`); the entry point's value reaches the joiner. Aggregate `7`.
//! * `mutex_trylock_from_the_owning_thread_is_busy` — `enif_mutex_trylock` succeeds on a free
//!   mutex and answers the C's `EBUSY` when the **owning** thread asks again (the mutex is not
//!   recursive); `3`.
//! * `mutex_lock_blocks_until_the_owner_unlocks` — the worker's `enif_mutex_trylock` answers
//!   `EBUSY` (the proof the caller still holds the mutex), its following blocking
//!   `enif_mutex_lock` only completes after the caller's `enif_mutex_unlock`, and it then observes
//!   the value the caller wrote *while holding* the mutex: `101`.
//! * `thread_name_reports_the_name_it_was_created_with` — `enif_thread_name` of a NIF-created
//!   thread reports the name `enif_thread_create` was handed, and the C's no-name form
//!   (`"unknown"`, `erl_drv_thread.c:135`) for an unnamed one; both joins report `5`. `15`.
//! * `getenv_success_path_copies_the_value` — `enif_getenv`'s **success** path: `0`, the value
//!   copied NUL-terminated to the caller's buffer, NUL-terminated, and `*value_size` the value's
//!   length **excluding** the NUL — cross-checked against the module's own `os:getenv/1`. `7`.
//! * `send_to_a_registered_name_reaches_the_worker` — `enif_whereis_pid` resolves a name a live
//!   process is registered under and the following `enif_send` (`msg_env == NULL`, the message
//!   term from the calling env) answers the C's `1` and the term really arrives in that mailbox:
//!   `10 * (whereis|send) + payload` = `37`.
//! * `new_binary_write_window_and_realloc` — `enif_make_new_binary`'s writable pointer produces a
//!   binary of the requested bytes, `enif_make_sub_binary` windows it, `enif_alloc_binary` +
//!   `enif_realloc_binary` grows an off-heap binary keeping its prefix, and a window of the grown
//!   binary reads the appended bytes: `27`.
//!
//! # Mechanics (identical to [`super::nif_state`])
//!
//! The C library is compiled by the harness inside the case's own temp dir against the host
//! `erl_nif.h`, and the embedded module `erlang:load_nif/2`s it from
//! `filename:dirname(code:which(?MODULE))`, so both runtimes load *the same* `.so` and run *the
//! same* compiled module; only stdout + exit status are compared. Two rules the fixture honours and
//! a future case must keep honouring:
//!
//! * every call into the library is **external** (`nif_thread:thr_counter()`), because the Rust
//!   runtime intercepts a NIF at external dispatch — a *local* call reaches the
//!   `erlang:nif_error/1` stub and raises `not_loaded` on the Rust side only (a guaranteed false
//!   mismatch);
//! * every `ErlNifFunc` entry has an exported stub of the same arity in the module (or the oracle
//!   refuses the load with `bad_lib`).
//!
//! # Bounded waits, no possible hang
//!
//! A fixture that can block forever is a hang in CI, so every wait is bounded and every thread is
//! joined: the case-level waits are `receive … after N` (and `os:getenv/1`, which cannot block), and
//! the one **native** wait — the caller waiting for the worker to be about to block, in
//! `mutex_handoff` — is a bounded `nanosleep` poll (500 × 1 ms) that reports a distinct negative
//! code instead of waiting forever. Every `ErlNifTid` created is joined on every path, and every
//! `enif_mutex`/`enif_cond` created is destroyed by its case.
//!
//! # Measured and deliberately NOT pinned
//!
//! * **`enif_schedule_nif`'s success path** — the yield seam. Measured with a NIF that re-schedules
//!   itself onto the same (or a dirty) scheduler: the oracle resumes the call and the module
//!   receives the resumed call's value (`2` for both the plain `flags = 0` form and
//!   `ERL_NIF_DIRTY_JOB_CPU_BOUND`), while the Rust runtime leaves `NifHost::schedule_nif` at its
//!   default and the C therefore raises `badarg` (the module observes `{raised, error, badarg}`).
//!   A case pinning it would be a mismatch; it is recorded in `divergences.md` §A.3. Owner:
//!   `beam_rt` (the `NifHost::schedule_nif` hook) / `beam_abi`.
//! * **`enif_send` from a NIF-created thread**, the documented thread-safe form (`env == NULL` plus
//!   a message env): the oracle delivers (`enif_send` ⇒ `1`, the message arrives), the Rust runtime
//!   answers `enif_alloc_env() == NULL` on a thread that is not inside a NIF call (so there is no
//!   message env to send from and nothing is delivered). Recorded in `divergences.md` §A.3. Owner:
//!   `beam_abi` (`nif/enif/envs.rs`) / the runtime's per-call host publication.
//! * **A sub-binary window of an *off-heap* parent.** Measured by retaining `enif_alloc_binary`'s
//!   data pointer, making the binary term, windowing it and then writing through the retained
//!   pointer: both runtimes answer alike (the window does not observe the later write). It is still
//!   not pinned, because the write happens through a pointer whose ownership the term now holds —
//!   asserting that is not something a NIF contract promises, so a runtime is free to copy the
//!   block into term-owned memory. The **heap** form of the same observation *is* pinned by
//!   `new_binary_write_window_and_realloc` (there the pointer is valid for the call by contract).
//! * **`enif_thread_type()` of the *calling* thread** is the already-recorded divergence (oracle
//!   `1` = normal scheduler, Rust runtime `0`), so this group asserts the **created** thread's kind
//!   (`ERL_NIF_THR_UNDEFINED`, `0`, on both) and never the caller's value.
//! * **Already pinned elsewhere, not duplicated here**: `enif_send`'s both forms and its dead-pid /
//!   self answers ([`super::nif_so`]'s `nif_rt` block), `enif_inspect_iolist_as_binary` over nested
//!   iodata ([`super::nif_so`] `nif_rt/iolist_deep_flatten_bytes`, [`super::nif_abi_extra`]'s byte
//!   rules), `enif_tsd_*` per-thread independence and `enif_cond_*` / `enif_rwlock_*`
//!   ([`super::nif_res`]), `enif_open_resource_type_x` with a full callback set
//!   ([`super::nif_res`]'s `xtype`/flag matrix and [`super::nif_state`]'s `init_resource_type`
//!   types), `enif_getenv`'s refusal codes ([`super::nif_res`]
//!   `getenv_reports_present_and_missing`) and `enif_thread_opts_create`/`enif_thread_exit`
//!   ([`super::nif_res`]).
//! * Everything already listed as unpinnable in `divergences.md` §A.3 (`enif_make_unique_integer`
//!   values, `enif_system_info`'s release/thread counts, `enif_max_atom_cache_index`,
//!   `enif_get_atom_cache_index` of a fresh atom, the `ERL_NIF_INTERNAL_HASH` value,
//!   `enif_getenv`'s "too small" `*value_size`, a zeroed `ErlNifMonitor`, `enif_term_size` of a
//!   reference or an off-heap binary, a resource type's `stop`-callback count) is not pinned here
//!   either.
use super::{Case, CaseKind, Contract, Golden, ModuleRun};
use crate::module_build::NifFixture;

/// The embedded module: one stub per `ErlNifFunc` entry plus the `classify/1` selector table.
///
/// Every call into the library is **external** and every wait is bounded. `send_case/0` spawns the
/// receiver the send case delivers to and waits for the delivery with a deadlined `receive`, so a
/// lost message shows up as the visible `timeout` answer.
const NIF_THREAD_MODULE_SOURCE: &str = r#"-module(nif_thread).
-export([classify/1, thr_counter/0, thr_kind/0, trylock_owner/0, mutex_handoff/0, thread_name/0,
         getenv_path/1, send_registered/0, new_binary/0]).

%% One stub per ErlNifFunc entry: `erlang:load_nif/2` refuses a library whose functions do not
%% exist AND are exported in the loading module, and every call into the library must be
%% external (the Rust runtime intercepts a NIF at external dispatch).
thr_counter() -> erlang:nif_error(not_loaded).
thr_kind() -> erlang:nif_error(not_loaded).
trylock_owner() -> erlang:nif_error(not_loaded).
mutex_handoff() -> erlang:nif_error(not_loaded).
thread_name() -> erlang:nif_error(not_loaded).
getenv_path(_Path) -> erlang:nif_error(not_loaded).
send_registered() -> erlang:nif_error(not_loaded).
new_binary() -> erlang:nif_error(not_loaded).

classify(N) ->
    Dir = filename:dirname(code:which(?MODULE)),
    case erlang:load_nif(Dir ++ "/nif_thread", 0) of
        ok ->
            case N of
                1 -> nif_thread:thr_counter();
                2 -> nif_thread:thr_kind();
                3 -> nif_thread:trylock_owner();
                4 -> nif_thread:mutex_handoff();
                5 -> nif_thread:thread_name();
                6 -> env_case();
                7 -> send_case();
                8 -> nif_thread:new_binary()
            end;
        {error, {Reason, _Text}} ->
            %% Only the reason *tag* leaves this function: the Text of a failure embeds the
            %% absolute temp-dir path, which would make the golden drift on every run.
            reason_code(Reason)
    end.

reason_code(ok) -> 0;
reason_code(load_failed) -> 1;
reason_code(bad_lib) -> 2;
reason_code(reload) -> 3;
reason_code(_) -> 9.

%% `enif_getenv`'s success path is cross-checked against the module's OWN view of the variable, so
%% the case never depends on a particular value: the C's copy must be byte-identical to it.
env_case() ->
    Path = os:getenv("PATH"),
    case is_list(Path) of
        true -> nif_thread:getenv_path(list_to_binary(Path));
        false -> {no_path, Path}
    end.

%% A worker registers under a name, the NIF resolves the name and sends to it, and the delivery is
%% waited for (bounded): a lost message would surface as the `timeout` answer.
send_case() ->
    Self = self(),
    W = spawn(fun() -> register(nif_thread_worker, self()), wait_for(Self) end),
    Ready = receive {ready, W} -> ok after 5000 -> no_ready end,
    Code = nif_thread:send_registered(),
    Got = receive {delivered, N} -> N after 5000 -> timeout end,
    _ = W,
    sd_code({Ready, Code, Got}).

wait_for(Parent) ->
    Parent ! {ready, self()},
    receive {from_nif, N} -> Parent ! {delivered, N} after 5000 -> ok end.

sd_code({ok, Code, Got}) -> Code * 10 + Got;
sd_code(_) -> 9.
"#;

/// The C library: the threading (create/join/self/equal_tids/type/name), mutex, environment and
/// cross-process-send families plus the binary write-through/window/realloc family.
///
/// Every `enif_*` referenced here is a symbol the `beam_rt` release binary exports (verified with
/// `nm -D`), and nothing C-variadic is used. `enif_schedule_nif` is deliberately **absent**: its
/// success path is a recorded divergence (`divergences.md` §A.3).
const NIF_THREAD_C: &str = r#"/* A real NIF exercising the THREADING / BLOCKING / CROSS-PROCESS half of the NIF C ABI:
   enif_thread_create / enif_thread_join / enif_thread_self / enif_equal_tids / enif_thread_type /
   enif_thread_name, the enif_mutex_lock / enif_mutex_trylock hand-off a case can observe, the
   enif_whereis_pid + enif_send cross-process send, enif_getenv's success path and the
   enif_make_new_binary / enif_make_sub_binary / enif_realloc_binary byte families. */
#include <erl_nif.h>
#include <errno.h>
#include <string.h>
#include <stdint.h>
#include <time.h>

/* ---- a shared, mutex-guarded counter, walked by two NIF-created threads ---------------------- */

static ErlNifMutex* cnt_mutex = NULL;
static int cnt = 0;
static int inside = 0;
static volatile int sink = 0;
static int overlaps = 0;

static void* counter_worker(void* arg) {
    int n = (int)(intptr_t) arg;
    int i, s;
    for (i = 0; i < n; i++) {
        enif_mutex_lock(cnt_mutex);
        inside++;
        if (inside != 1) overlaps++;
        /* Widen the critical section so a missing mutual exclusion would be observed. */
        for (s = 0; s < 200; s++) sink = sink + s;
        inside--;
        cnt = cnt + 1;
        enif_mutex_unlock(cnt_mutex);
    }
    return (void*)(intptr_t) n;
}

/* 500 increments per thread: the counter must read exactly 1000 and no overlap may ever be seen.
   The aggregate is `counter * 10 + overlaps`, so both facts stay on one integer line. */
static ERL_NIF_TERM thr_counter(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifTid t1 = NULL, t2 = NULL;
    void* r1 = NULL;
    void* r2 = NULL;
    int c1, c2, j1, j2, ok;
    (void) argc; (void) argv;
    if (cnt_mutex == NULL) cnt_mutex = enif_mutex_create("nif_thread_cnt");
    if (cnt_mutex == NULL) return enif_make_int(env, -1);
    cnt = 0;
    inside = 0;
    overlaps = 0;
    c1 = enif_thread_create("nif_thread_c1", &t1, counter_worker, (void*)(intptr_t) 500, NULL);
    if (c1 != 0) t1 = NULL;
    c2 = enif_thread_create("nif_thread_c2", &t2, counter_worker, (void*)(intptr_t) 500, NULL);
    if (c2 != 0) t2 = NULL;
    j1 = t1 ? enif_thread_join(t1, &r1) : c1;
    j2 = t2 ? enif_thread_join(t2, &r2) : c2;
    ok = (j1 == 0 && j2 == 0 && (intptr_t) r1 == 500 && (intptr_t) r2 == 500);
    if (!ok) return enif_make_int(env, -1);
    return enif_make_int(env, cnt * 10 + overlaps);
}

/* ---- thread identity and thread kind --------------------------------------------------------- */

static ErlNifTid kind_caller_tid;
static int kind_same_as_caller = 1;
static int kind_type = -1;

static void* kind_worker(void* arg) {
    (void) arg;
    kind_type = (int) enif_thread_type();
    kind_same_as_caller = enif_equal_tids(enif_thread_self(), kind_caller_tid) ? 1 : 0;
    return (void*)(intptr_t) 3;
}

/* 1: the entry point's value reached the joiner; 2: the new thread is NOT the caller;
   4: its kind is ERL_NIF_THR_UNDEFINED (a NIF thread is no scheduler thread). */
static ERL_NIF_TERM thr_kind(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifTid tid = NULL;
    void* resp = NULL;
    int create_ret, join_ret, code = 0;
    (void) argc; (void) argv;
    kind_same_as_caller = 1;
    kind_type = -1;
    kind_caller_tid = enif_thread_self();
    create_ret = enif_thread_create("nif_thread_kind", &tid, kind_worker, NULL, NULL);
    if (create_ret != 0) tid = NULL;
    join_ret = tid ? enif_thread_join(tid, &resp) : create_ret;
    if (join_ret == 0 && (intptr_t) resp == 3) code += 1;
    if (kind_same_as_caller == 0) code += 2;
    if (kind_type == ERL_NIF_THR_UNDEFINED) code += 4;
    return enif_make_int(env, code);
}

/* ---- enif_mutex_trylock: free, then from the owning thread ----------------------------------- */

/* 1: the trylock on a free mutex succeeded; 2: the owner asking again is EBUSY (not recursive). */
static ERL_NIF_TERM trylock_owner(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifMutex* m = enif_mutex_create("nif_thread_owner");
    int code = 0, second;
    (void) argc; (void) argv;
    if (m == NULL) return enif_make_int(env, -1);
    if (enif_mutex_trylock(m) == 0) {
        code += 1;
        second = enif_mutex_trylock(m);
        if (second == EBUSY) code += 2;
        /* Unlock as often as this thread managed to lock, so the destroy below is safe even if a
           runtime were to hand the owner a second, recursive lock. */
        if (second == 0) enif_mutex_unlock(m);
        enif_mutex_unlock(m);
    }
    enif_mutex_destroy(m);
    return enif_make_int(env, code);
}

/* ---- enif_mutex_lock blocks until the owner unlocks, and the hand-off is ordered -------------- */

static ErlNifMutex* ho_mutex = NULL;
static volatile int ho_tried = 0;
static int ho_flag = 0;

static void* handoff_worker(void* arg) {
    int busy, seen;
    (void) arg;
    /* EBUSY here is the proof the caller still holds the mutex: the caller only unlocks after this
       thread has announced `ho_tried`. */
    busy = enif_mutex_trylock(ho_mutex);
    if (busy == 0) enif_mutex_unlock(ho_mutex);
    ho_tried = 1;
    enif_mutex_lock(ho_mutex);          /* blocks until the caller's enif_mutex_unlock */
    seen = ho_flag;                     /* what the caller wrote while holding the mutex */
    enif_mutex_unlock(ho_mutex);
    return (void*)(intptr_t) ((busy == EBUSY ? 100 : 0) + seen);
}

/* `100` (the caller held the mutex when the worker tried it) + `1` (the worker's blocking lock
   completed only after the caller's unlock, and it sees the caller's write). The caller's wait for
   the worker is a BOUNDED poll (500 x 1 ms), so a stuck worker can never hang the case. */
static ERL_NIF_TERM mutex_handoff(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifTid tid = NULL;
    void* resp = NULL;
    struct timespec tick;
    int i, code = 0;
    (void) argc; (void) argv;
    if (ho_mutex == NULL) ho_mutex = enif_mutex_create("nif_thread_ho");
    if (ho_mutex == NULL) return enif_make_int(env, -1);
    ho_tried = 0;
    ho_flag = 0;
    enif_mutex_lock(ho_mutex);
    if (enif_thread_create("nif_thread_ho_w", &tid, handoff_worker, NULL, NULL) != 0) tid = NULL;
    if (tid == NULL) {
        enif_mutex_unlock(ho_mutex);
        return enif_make_int(env, -2);
    }
    tick.tv_sec = 0;
    tick.tv_nsec = 1000000L;
    for (i = 0; i < 500 && !ho_tried; i++) nanosleep(&tick, NULL);
    if (!ho_tried) code = -3;           /* the worker never announced itself: no hand-off to pin */
    else ho_flag = 1;                   /* the caller's write, made while it holds the mutex */
    enif_mutex_unlock(ho_mutex);
    if (enif_thread_join(tid, &resp) != 0) return enif_make_int(env, -4);
    if (code == -3) return enif_make_int(env, -3);
    if ((intptr_t) resp == 101) code += 1;
    return enif_make_int(env, code);
}

/* ---- enif_thread_name of a NIF-created thread ------------------------------------------------ */

static ErlNifMutex* name_mutex = NULL;
static ErlNifCond* name_cond = NULL;
static int name_go = 0;

static void* name_worker(void* arg) {
    (void) arg;
    enif_mutex_lock(name_mutex);
    while (!name_go) enif_cond_wait(name_cond, name_mutex);
    enif_mutex_unlock(name_mutex);
    return (void*)(intptr_t) 5;
}

/* 1: the created thread reports the name it was given; 2: its join reached the entry point's
   value; 4: an unnamed thread reports the C's no-name form ("unknown", `erl_drv_thread.c:135`);
   8: its join reached the entry point's value. The name must be read while the thread is alive
   (the C frees the thread record on join), hence the flag/cond hand-off. */
static ERL_NIF_TERM thread_name(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifTid tid = NULL, unnamed = NULL;
    void* resp = NULL;
    char* got;
    int code = 0;
    (void) argc; (void) argv;
    if (name_mutex == NULL) {
        name_mutex = enif_mutex_create("nif_thread_nm");
        name_cond = enif_cond_create("nif_thread_nc");
    }
    if (name_mutex == NULL || name_cond == NULL) return enif_make_int(env, -1);
    name_go = 0;
    if (enif_thread_create("nif_thread_named", &tid, name_worker, NULL, NULL) != 0) tid = NULL;
    if (tid != NULL) {
        got = enif_thread_name(tid);
        if (got != NULL && strcmp(got, "nif_thread_named") == 0) code += 1;
    }
    enif_mutex_lock(name_mutex);
    name_go = 1;
    enif_cond_broadcast(name_cond);
    enif_mutex_unlock(name_mutex);
    if (tid != NULL && enif_thread_join(tid, &resp) == 0 && (intptr_t) resp == 5) code += 2;
    name_go = 0;
    if (enif_thread_create(NULL, &unnamed, name_worker, NULL, NULL) != 0) unnamed = NULL;
    if (unnamed != NULL) {
        got = enif_thread_name(unnamed);
        if (got != NULL && strcmp(got, "unknown") == 0) code += 4;
    }
    enif_mutex_lock(name_mutex);
    name_go = 1;
    enif_cond_broadcast(name_cond);
    enif_mutex_unlock(name_mutex);
    if (unnamed != NULL && enif_thread_join(unnamed, &resp) == 0 && (intptr_t) resp == 5) code += 8;
    return enif_make_int(env, code);
}

/* ---- enif_getenv: the success path ---------------------------------------------------------- */

/* 1: the code is 0 (found and copied); 2: `*value_size` is the value's length WITHOUT the NUL;
   4: the bytes are the caller's own view of the variable, NUL-terminated. */
static ERL_NIF_TERM getenv_path(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    char buf[16384];
    size_t sz = sizeof(buf);
    ErlNifBinary want;
    int code, bits = 0;
    if (argc != 1) return enif_make_badarg(env);
    if (!enif_inspect_binary(env, argv[0], &want)) return enif_make_int(env, -1);
    code = enif_getenv("PATH", buf, &sz);
    if (code != 0) return enif_make_int(env, code);
    bits += 1;
    if (sz == want.size) bits += 2;
    if (sz == want.size && memcmp(buf, want.data, want.size) == 0 && buf[want.size] == '\0') bits += 4;
    return enif_make_int(env, bits);
}

/* ---- enif_whereis_pid + enif_send to a registered name -------------------------------------- */

/* 1: the name resolved to a live pid; 2: `enif_send` (msg_env == NULL, the message term built on
   the calling env) answered the C's 1. */
static ERL_NIF_TERM send_registered(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifPid pid;
    ERL_NIF_TERM name, msg, parts[2];
    int code = 0;
    (void) argc; (void) argv;
    name = enif_make_atom(env, "nif_thread_worker");
    memset(&pid, 0, sizeof pid);
    if (!enif_whereis_pid(env, name, &pid)) return enif_make_int(env, -1);
    code += 1;
    parts[0] = enif_make_atom(env, "from_nif");
    parts[1] = enif_make_int(env, 7);
    msg = enif_make_tuple_from_array(env, parts, 2);
    if (enif_send(env, &pid, NULL, msg) == 1) code += 2;
    return enif_make_int(env, code);
}

/* ---- the binary write-through / window / realloc family ------------------------------------- */

/* 1: `enif_make_new_binary`'s writable pointer produced the requested bytes; 2: an
   `enif_make_sub_binary` window reads them; 4: the window is a COPY (a write through the parent's
   pointer afterwards is not visible through it, as the oracle also answers); 8: an
   `enif_alloc_binary` grown by `enif_realloc_binary` keeps its prefix and reports the new size;
   16: a window of the grown binary reads the appended bytes. */
static ERL_NIF_TERM new_binary(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifBinary bin, sub;
    ERL_NIF_TERM term, subterm;
    unsigned char* p;
    int code = 0;
    (void) argc; (void) argv;
    p = enif_make_new_binary(env, 8, &term);
    if (p == NULL) return enif_make_int(env, -1);
    memcpy(p, "abcdefgh", 8);
    if (enif_inspect_binary(env, term, &bin) && bin.size == 8 && memcmp(bin.data, "abcdefgh", 8) == 0)
        code += 1;
    subterm = enif_make_sub_binary(env, term, 2, 3);
    if (enif_inspect_binary(env, subterm, &sub) && sub.size == 3 && memcmp(sub.data, "cde", 3) == 0)
        code += 2;
    p[2] = 'X';
    if (enif_inspect_binary(env, subterm, &sub) && sub.size == 3 && memcmp(sub.data, "cXe", 3) == 0)
        code += 4;
    if (enif_alloc_binary(4, &bin)) {
        memcpy(bin.data, "abcd", 4);
        if (enif_realloc_binary(&bin, 8) && bin.size == 8 && memcmp(bin.data, "abcd", 4) == 0) {
            memcpy(bin.data + 4, "efgh", 4);
            term = enif_make_binary(env, &bin);
            if (enif_inspect_binary(env, term, &sub) && sub.size == 8
                && memcmp(sub.data, "abcdefgh", 8) == 0)
                code += 8;
            subterm = enif_make_sub_binary(env, term, 2, 4);
            if (enif_inspect_binary(env, subterm, &sub) && sub.size == 4
                && memcmp(sub.data, "cdef", 4) == 0)
                code += 16;
        }
    }
    return enif_make_int(env, code);
}

static ErlNifFunc nif_funcs[] = {
    {"thr_counter", 0, thr_counter, 0},
    {"thr_kind", 0, thr_kind, 0},
    {"trylock_owner", 0, trylock_owner, 0},
    {"mutex_handoff", 0, mutex_handoff, 0},
    {"thread_name", 0, thread_name, 0},
    {"getenv_path", 1, getenv_path, 0},
    {"send_registered", 0, send_registered, 0},
    {"new_binary", 0, new_binary, 0}
};

/* No resource type is opened, so the library needs no `load` callback (`erlang:load_nif/2`
   accepts `NULL` for all four callbacks). `ERL_NIF_INIT` seeds `ErlNifEntry.name` with the
   stringified module name, so this library may only be loaded as `nif_thread`. */
ERL_NIF_INIT(nif_thread, nif_funcs, NULL, NULL, NULL, NULL)
"#;

/// The library every case loads.
const NIF_THREAD: NifFixture = NifFixture::CompiledLibrary {
    name: "nif_thread",
    c_source: NIF_THREAD_C,
};

/// Build one module-run case over [`NIF_THREAD_MODULE_SOURCE`].
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
            source: NIF_THREAD_MODULE_SOURCE,
            module: "nif_thread",
            function: "classify",
            arity: 1,
            args: selector,
            nif: Some(NIF_THREAD),
        }),
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// The A.3 threading / blocking / cross-process group. Each golden is the integer the **oracle**
/// prints (re-verified per case with `differential run <id>`).
pub(crate) fn group() -> Vec<Case> {
    vec![
        case(
            "nif_thread/threads_share_a_mutex_guarded_counter",
            "two NIF-created threads walk one mutex-guarded counter 500 times each: no critical section is ever entered twice, both joins report their own value, the counter is exactly 1000",
            &[1],
            // 1000 (increments) * 10 + 0 (no overlap observed).
            "10000",
        ),
        case(
            "nif_thread/thread_identity_and_kind_differ_from_the_caller",
            "the entry point's value reaches the joiner, enif_thread_self in the new thread is not the caller's tid, and enif_thread_type there is ERL_NIF_THR_UNDEFINED",
            &[2],
            // 1 (join value 3) + 2 (a different tid) + 4 (ERL_NIF_THR_UNDEFINED).
            "7",
        ),
        case(
            "nif_thread/mutex_trylock_from_the_owning_thread_is_busy",
            "enif_mutex_trylock succeeds on a free mutex and answers EBUSY when the owning thread asks again",
            &[3],
            // 1 (free trylock) + 2 (EBUSY from the owner).
            "3",
        ),
        case(
            "nif_thread/mutex_lock_blocks_until_the_owner_unlocks",
            "a worker's trylock answers EBUSY while the caller holds the mutex, its blocking enif_mutex_lock completes only after the caller's unlock, and it then sees the caller's write",
            &[4],
            // The worker reported 101 (100: the caller still held the mutex when it tried, 1: the
            // caller's write was visible after the hand-off), so the case's single bit is set.
            "1",
        ),
        case(
            "nif_thread/thread_name_reports_the_name_it_was_created_with",
            "enif_thread_name reports the name enif_thread_create was handed, and the C's no-name form for an unnamed thread",
            &[5],
            // 1 (name) + 2 (join value) + 4 (no-name form) + 8 (join value).
            "15",
        ),
        case(
            "nif_thread/getenv_success_path_copies_the_value",
            "enif_getenv's success path answers 0, copies the NUL-terminated value and reports its length without the NUL, matching the module's own os:getenv/1",
            &[6],
            // 1 (code 0) + 2 (size without the NUL) + 4 (the caller's own bytes).
            "7",
        ),
        case(
            "nif_thread/send_to_a_registered_name_reaches_the_worker",
            "enif_whereis_pid resolves a registered name and enif_send's msg_env == NULL form delivers the message to that mailbox",
            &[7],
            // 3 (whereis answered and enif_send returned 1) * 10 + 7 (the payload the worker received).
            "37",
        ),
        case(
            "nif_thread/new_binary_write_window_and_realloc",
            "enif_make_new_binary's pointer writes the bytes, enif_make_sub_binary windows them, and an enif_realloc_binary-grown binary keeps its prefix and reads back with its appended bytes",
            &[8],
            // 1 + 2 (the window reads the parent's bytes) + 8 + 16 (realloc preserved the prefix and
            // the grown binary's window reads the appended bytes); 4 is NOT set: the window is a copy.
            "27",
        ),
    ]
}

#[cfg(test)]
mod tests {
    //! Invariants of the A.3 threading / blocking / cross-process group.

    use super::{group, CaseKind, Contract, NifFixture, NIF_THREAD_C, NIF_THREAD_MODULE_SOURCE};
    use std::collections::HashSet;

    /// The probe functions the module exports and calls **externally**; each must be a stub of the
    /// same arity and every call into the library must go through `nif_thread:` (a local call would
    /// reach the stub and raise `not_loaded` on the Rust side only).
    const PROBES: &[&str] = &[
        "thr_counter",
        "thr_kind",
        "trylock_owner",
        "mutex_handoff",
        "thread_name",
        "getenv_path",
        "send_registered",
        "new_binary",
    ];

    #[test]
    fn every_case_is_a_nif_module_run_over_the_same_compiled_library() {
        let mut selectors = HashSet::new();
        for case in group() {
            assert!(
                case.id.starts_with("nif_thread/"),
                "case {} is outside the nif_thread group",
                case.id
            );
            assert!(!case.description.is_empty(), "case {} has no description", case.id);
            assert_eq!(
                case.contracts,
                &[Contract::NifAbi, Contract::BeamAsm],
                "case {}: an end-to-end NIF case evidences A.3 and the compiled .beam (A.2)",
                case.id
            );
            let CaseKind::Module(run) = case.kind else {
                panic!("case {}: an A.3 case must be a module-run case", case.id);
            };
            assert_eq!(run.module, "nif_thread", "case {}", case.id);
            assert_eq!(run.function, "classify", "case {}", case.id);
            assert_eq!(run.arity, 1, "case {}", case.id);
            assert_eq!(
                run.args.len(),
                run.arity as usize,
                "case {}: one selector per arity slot",
                case.id
            );
            assert!(selectors.insert(run.args[0]), "case {}: selector reused", case.id);
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
                 {{error, {{Reason, Text}}}} would drift), got {:?}",
                case.id,
                case.golden.stdout
            );
            assert_eq!(case.golden.exit_code, 0, "case {}", case.id);
        }
        assert_eq!(selectors.len(), 8, "the group covers eight distinct selectors");
        assert_eq!(group().len(), 8, "the group carries eight cases");
    }

    #[test]
    fn every_case_names_the_c_library() {
        // The `.so` name the module loads is the fixture's own name plus whatever the platform
        // appends, so a case whose fixture is unnamed (or named differently) would never load.
        for case in group() {
            let CaseKind::Module(run) = case.kind else {
                panic!("case {}: an A.3 case must be a module-run case", case.id);
            };
            assert_eq!(
                run.nif,
                Some(NifFixture::CompiledLibrary {
                    name: "nif_thread",
                    c_source: NIF_THREAD_C,
                }),
                "case {}: the fixture must be the group's own compiled library",
                case.id
            );
            assert!(
                NIF_THREAD_MODULE_SOURCE.contains("Dir ++ \"/nif_thread\""),
                "case {}: the module must load the library the fixture builds",
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
                NIF_THREAD_MODULE_SOURCE.contains(&stub),
                "the module must export a stub for {probe}"
            );
            let external = format!("nif_thread:{probe}(");
            assert!(
                NIF_THREAD_MODULE_SOURCE.contains(&external),
                "the module must call {probe} through `nif_thread:` (external dispatch)"
            );
            assert!(
                NIF_THREAD_C.contains(&format!("\"{probe}\"")),
                "the library must register {probe}"
            );
        }
    }

    #[test]
    fn the_library_references_the_threading_slots_it_pins() {
        // What makes the group a *threading* group: the thread family, the mutex family, the
        // cross-process send and the environment read. A fixture that dropped one would pin less.
        for symbol in [
            "enif_thread_create",
            "enif_thread_join",
            "enif_thread_self",
            "enif_equal_tids",
            "enif_thread_type",
            "enif_thread_name",
            "enif_mutex_create",
            "enif_mutex_lock",
            "enif_mutex_trylock",
            "enif_mutex_unlock",
            "enif_mutex_destroy",
            "enif_whereis_pid",
            "enif_send",
            "enif_getenv",
            "enif_make_new_binary",
            "enif_make_sub_binary",
            "enif_alloc_binary",
            "enif_realloc_binary",
            "enif_make_binary",
            "enif_inspect_binary",
        ] {
            assert!(NIF_THREAD_C.contains(symbol), "the library must reference {symbol}");
        }
    }

    #[test]
    fn the_divergent_scheduling_seam_is_not_pinned() {
        // `enif_schedule_nif`'s success path is a recorded divergence (`divergences.md` §A.3): the
        // oracle resumes the call, the Rust runtime raises badarg. A library referencing it would
        // pin a mismatch, so its absence is asserted — a future replacement of that hook may land a
        // case here, and must delete this guard at the same time.
        assert!(
            !NIF_THREAD_C.contains("enif_schedule_nif"),
            "the schedule_nif yield seam is a recorded divergence and must not be pinned"
        );
        // Same for the `env == NULL` + message-env send from a NIF-created thread: the oracle
        // delivers, the Rust runtime answers `enif_alloc_env() == NULL` off the NIF call path.
        assert!(
            !NIF_THREAD_C.contains("enif_send(NULL"),
            "enif_send from a created thread is a recorded divergence and must not be pinned"
        );
    }

    #[test]
    fn every_native_wait_is_bounded() {
        // A fixture that can block forever is a CI hang. The one native wait (the caller waiting
        // for the worker to be about to block) is a bounded nanosleep poll with a negative-code
        // escape; the case-level waits are deadlined receives.
        assert!(
            NIF_THREAD_C.contains("for (i = 0; i < 500 && !ho_tried; i++) nanosleep(&tick, NULL)"),
            "the hand-off handshake must be a bounded poll, never an unbounded wait"
        );
        assert!(
            NIF_THREAD_C.contains("if (!ho_tried)"),
            "the bounded poll must report a distinct code when it gives up"
        );
        for wait in [
            "after 5000 -> no_ready",
            "after 5000 -> timeout",
            "after 5000 -> ok",
        ] {
            assert!(
                NIF_THREAD_MODULE_SOURCE.contains(wait),
                "the module must bound its waits with {wait:?}"
            );
        }
        // Every thread a case creates is joined, and no mutex/cond is left behind.
        assert_eq!(
            NIF_THREAD_C.matches("enif_thread_create(").count(),
            NIF_THREAD_C.matches("enif_thread_join(").count(),
            "every created thread must be joined"
        );
        // No unbounded loop may exist in the fixture: the only loop with a live bound is the
        // hand-off's poll, and the counter's inner loop is a fixed 200 iterations.
        assert!(
            !NIF_THREAD_C.contains("while (1)"),
            "the fixture must not carry an unbounded loop"
        );
    }

    #[test]
    fn no_case_lets_a_volatile_value_reach_the_golden() {
        // Only integers leave the library, and nothing that differs per VM start (a pid, a ref, a
        // thread id, a pointer) is ever turned into a term.
        for banned in [
            "enif_make_pid(",
            "enif_make_ref(",
            "enif_make_monitor_term(",
            "enif_make_unique_integer(",
            "enif_system_info(",
            "enif_dlopen(",
        ] {
            assert!(
                !NIF_THREAD_C.contains(banned),
                "the library must not build a per-VM value ({banned})"
            );
        }
    }
}
