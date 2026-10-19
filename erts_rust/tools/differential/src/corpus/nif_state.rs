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

//! Contract **A.3** — the *stateful* `enif_*` family's observable **behaviour**.
//!
//! [`super::nif_so`] pins the load/call surface (and, in its `nif_rt` block, one selector per
//! stateful item), [`super::nif_res`] the resource / ioq / sync families, and [`super::nif_abi`] /
//! [`super::nif_abi_extra`] the frozen structs plus every `enif_*` *shape*. This group pins what the
//! stateful items actually **do** end to end, on state the library keeps across calls:
//!
//! * `monitor_down_then_demonitor_is_not_identified` — `enif_monitor_process` arms on a live pid
//!   (the C's `0`), the target is killed, the resource's `down` callback runs **exactly once**, and
//!   the `enif_demonitor_process` that follows answers the C's `1` (nothing left to identify — the
//!   DOWN is already delivered). Aggregate `downs * 10 + demonitor_code` = `11`.
//! * `monitor_of_an_undefined_pid_is_refused` — `enif_set_pid_undefined` then
//!   `enif_monitor_process` is the C's `1` (no live process) and fabricates **no** `down`
//!   (`0`), so the aggregate is `10`.
//! * `select_stop_releases_an_armed_read` — `enif_select(…, READ, …)` on a real `pipe(2)` read end
//!   answers `0`; `ERL_NIF_SELECT_STOP` releases it with `STOP_CALLED | READ_CANCELLED` (`17`); the
//!   byte written afterwards is never delivered; a second STOP answers `STOP_CALLED` alone (`1`).
//!   Aggregate `stop * 10 + stop2` = `171`.
//! * `select_cancel_clears_only_the_armed_direction` — `SELECT_CANCEL | SELECT_READ` reports exactly
//!   `READ_CANCELLED` (`16`), nothing is delivered by a later write, and a second CANCEL cancels
//!   nothing (`0`): `160`.
//! * `select_custom_msg_delivers_the_callers_term` — `enif_select_x(…, READ | CUSTOM_MSG, …)` with
//!   `msg_env == NULL` delivers the caller's own `{custom, Ref, payload}` **verbatim** (no `select`
//!   tuple, no event atom), with the arming reference inside it.
//! * `select_write_end_delivers_ready_output` — WRITE interest on a pipe **write** end (writable as
//!   soon as it exists) delivers `{select, Res, Ref, ready_output}` unpoked: the write direction,
//!   which no other corpus case pins.
//! * `select_interest_holds_the_object_until_stop` — after `enif_release_resource` drops the
//!   library's own reference, the select interest's reference keeps the object alive (the in-call
//!   destructor count stays `0`); `ERL_NIF_SELECT_STOP` then answers `17` and gives that reference
//!   back exactly once, so the destructor runs exactly once (read through the settle loop).
//! * `resource_identity_survives_a_collection` — a resource term minted by `enif_make_resource`
//!   goes through Erlang's heap, a forced `erlang:garbage_collect/0` and a heap-churning allocation,
//!   and is still the **same** object: `enif_compare` of the two terms is `0`, `enif_is_identical`
//!   holds, `enif_get_resource` resolves it back to the recorded object pointer, and the payload is
//!   unchanged. The aggregate is the payload itself (`7`).
//! * `keep_resource_outlives_the_term_across_a_collection` — `enif_keep_resource` really is a
//!   reference: the object survives its term being collected (destructor count `0` after the
//!   collection), and the following `enif_release_resource` destroys it exactly once (`1`).
//!
//! # Mechanics (identical to [`super::nif_res`])
//!
//! The C library is compiled by the harness inside the case's own temp dir against the host
//! `erl_nif.h`, and the embedded module `erlang:load_nif/2`s it from
//! `filename:dirname(code:which(?MODULE))`, so both runtimes load *the same* `.so` and run *the
//! same* compiled module; only stdout + exit status are compared. Two rules the fixture honours and
//! a future case must keep honouring:
//!
//! * every call into the library is **external** (`nif_state:probe()`), because the Rust runtime
//!   intercepts a NIF at external dispatch — a *local* call reaches the `erlang:nif_error/1` stub
//!   and raises `not_loaded` on the Rust side only (a guaranteed false mismatch);
//! * every `ErlNifFunc` entry has an exported stub of the same arity in the module (or the oracle
//!   refuses the load with `bad_lib`).
//!
//! Every wait is bounded (`receive … after`, a bounded destructor-settle poll, a bounded `down`
//! poll), and no golden is a volatile value: the per-VM reference, pid and temp path are reduced to
//! a shape before they reach the integer line.
//!
//! # Measured and deliberately NOT pinned
//!
//! * **The resource type's `stop` callback invocation count.** Measured directly (arm a READ
//!   interest on a pipe, then `ERL_NIF_SELECT_STOP` twice, reading the callback's own counter
//!   between the steps): the oracle answers `{0, 0, 17, 1, 1, 2}` — the C's STOP path calls the
//!   type's `stop` callback on **every** `enif_select(STOP)`, armed or not (`erl_check_io.c:1530`)
//!   — while the Rust runtime answers `{0, 0, 17, 0, 1, 0}`, i.e. it never invokes it. The *codes*
//!   agree; only the callback count differs, so it is a divergence, recorded in
//!   `divergences.md` §A.3 (owner: `beam_abi` / the `beam_rt` select hook). `ERL_NIF_SELECT_STOP_SCHEDULED`
//!   is unobservable for the same reason.
//! * **The in-call destructor count** right after a release or a STOP. The oracle schedules a
//!   resource destructor as deferred aux work (`nif_resource_dtor_prologue` →
//!   `erts_schedule_misc_aux_work`, `erl_nif.c`) while the Rust runtime runs it synchronously, so
//!   *when* it ran is not comparable. Both cases that read it therefore pin the **settled** count
//!   (`settle_sel/1`, `settle_id/1` — the same shape [`super::nif_res`] uses with `settle_dtor/1`):
//!   *whether* it ran is the observable, *when* is not.
//! * **The `down` pid and the demonitor-while-armed code** are already pinned by
//!   [`super::nif_so`]'s stateful block (`nif_rt/monitor_down_fires_once_on_the_armed_pid`,
//!   `nif_rt/demonitor_cancels_an_armed_monitor`) and are not duplicated here; this group adds the
//!   **complementary** arm (a demonitor *after* the DOWN, which the C cannot identify).
//! * **`ERL_NIF_SELECT_ERROR` / `ready_error`** needs a TCP socket reset through `SO_LINGER`, and the
//!   socket data path belongs to the `udp` / `inet_control` groups — it is out of scope here rather
//!   than measured and rejected.
//! * **A resource garbage-collected *while* selected** is not pinned: proving that the runtime
//!   prunes the registration needs the object's address after its destructor ran, which is a
//!   use-after-free in the fixture (the C zeroes its own pointer only when it wins the race).
//! * Everything already listed as unpinnable in `divergences.md` §A.3 (`enif_make_unique_integer`
//!   values, `enif_system_info`'s release/thread counts, `enif_max_atom_cache_index`,
//!   `enif_get_atom_cache_index` of a fresh atom, the `ERL_NIF_INTERNAL_HASH` value,
//!   `enif_getenv`'s "too small" `*value_size`, a zeroed `ErlNifMonitor`, `enif_term_size` of a
//!   reference or an off-heap binary) is not pinned here either — none of it is stateful behaviour.
use super::{Case, CaseKind, Contract, Golden, ModuleRun};
use crate::module_build::NifFixture;

/// The embedded module: one stub per `ErlNifFunc` entry plus the `classify/1` selector table.
///
/// Every call into the library is **external** and every wait is bounded. `settle_sel/1` and
/// `settle_id/1` wait for a deferred destructor (the oracle's aux-work timing) so the two runtimes
/// agree on the settled count.
const NIF_STATE_MODULE_SOURCE: &str = r#"-module(nif_state).
-export([classify/1, mon_make/0, mon_arm/1, mon_arm_undefined/0, mon_demonitor/0,
         mon_downs/0, sel_arm/0, sel_arm_custom/0, sel_arm_write/0, sel_poke/0, sel_stop/0,
         sel_cancel/0, sel_release/0, sel_dtors/0, id_make/0, id_check/1, id_keep/0,
         id_release/0, id_count/0]).

%% One stub per ErlNifFunc entry: `erlang:load_nif/2` refuses a library whose functions do not
%% exist AND are exported in the loading module, and every call into the library must be
%% external (the Rust runtime intercepts a NIF at external dispatch).
mon_make() -> erlang:nif_error(not_loaded).
mon_arm(_P) -> erlang:nif_error(not_loaded).
mon_arm_undefined() -> erlang:nif_error(not_loaded).
mon_demonitor() -> erlang:nif_error(not_loaded).
mon_downs() -> erlang:nif_error(not_loaded).
sel_arm() -> erlang:nif_error(not_loaded).
sel_arm_custom() -> erlang:nif_error(not_loaded).
sel_arm_write() -> erlang:nif_error(not_loaded).
sel_poke() -> erlang:nif_error(not_loaded).
sel_stop() -> erlang:nif_error(not_loaded).
sel_cancel() -> erlang:nif_error(not_loaded).
sel_release() -> erlang:nif_error(not_loaded).
sel_dtors() -> erlang:nif_error(not_loaded).
id_make() -> erlang:nif_error(not_loaded).
id_check(_T) -> erlang:nif_error(not_loaded).
id_keep() -> erlang:nif_error(not_loaded).
id_release() -> erlang:nif_error(not_loaded).
id_count() -> erlang:nif_error(not_loaded).

classify(N) ->
    Dir = filename:dirname(code:which(?MODULE)),
    case erlang:load_nif(Dir ++ "/nif_state", 0) of
        ok ->
            case N of
                1 -> mon_down_then_demonitor();
                2 -> mon_undefined_case();
                3 -> stop_case();
                4 -> cancel_case();
                5 -> custom_case();
                6 -> identity_case();
                7 -> keep_gc_case();
                8 -> write_case();
                9 -> stop_after_release_case()
            end;
        {error, {Reason, _Text}} ->
            reason_code(Reason)
    end.

%% Only the reason *tag* leaves this function: the Text of a failure embeds the absolute temp-dir
%% path, which would make the golden drift on every run.
reason_code(ok) -> 0;
reason_code(load_failed) -> 1;
reason_code(bad_lib) -> 2;
reason_code(reload) -> 3;
reason_code(_) -> 9.

%% ---- monitors ----

victim() -> spawn(fun() -> receive stop -> ok end end).

%% Poll the `down` count (up to 200 rounds of 5 ms): the callback is delivered asynchronously, so
%% it is waited for, never assumed.
wait_down(0) -> nif_state:mon_downs();
wait_down(N) ->
    case nif_state:mon_downs() of
        0 -> receive after 5 -> ok end, wait_down(N - 1);
        Count -> Count
    end.

%% Armed, the target dies (one DOWN), then the demonitor finds nothing left to cancel.
mon_down_then_demonitor() ->
    _Res = nif_state:mon_make(),
    P = victim(),
    Armed = nif_state:mon_arm(P),
    exit(P, kill),
    Downs = wait_down(200),
    Cancel = nif_state:mon_demonitor(),
    md_code({Armed, Downs, Cancel}).

md_code({0, Downs, Cancel}) -> Downs * 10 + Cancel;
md_code(_) -> 9.

%% No process to watch: the C refuses the monitor and no `down` is fabricated.
mon_undefined_case() ->
    _Res = nif_state:mon_make(),
    Code = nif_state:mon_arm_undefined(),
    Downs = nif_state:mon_downs(),
    mu_code({Code, Downs}).

mu_code({Code, Downs}) -> Code * 10 + Downs;
mu_code(_) -> 9.

%% ---- enif_select ----

stop_case() ->
    Code = nif_state:sel_arm(),
    Stop = nif_state:sel_stop(),
    Written = nif_state:sel_poke(),
    Late = receive M -> {early, M} after 300 -> timeout end,
    Stop2 = nif_state:sel_stop(),
    st_code({Code, Stop, Written, Late, Stop2}).

st_code({0, Stop, 1, timeout, Stop2}) -> Stop * 10 + Stop2;
st_code(_) -> 9.

cancel_case() ->
    Code = nif_state:sel_arm(),
    Cancel = nif_state:sel_cancel(),
    Written = nif_state:sel_poke(),
    Late = receive M -> {early, M} after 300 -> timeout end,
    Cancel2 = nif_state:sel_cancel(),
    cn_code({Code, Cancel, Written, Late, Cancel2}).

cn_code({0, Cancel, 1, timeout, Cancel2}) -> Cancel * 10 + Cancel2;
cn_code(_) -> 9.

%% CUSTOM_MSG: the caller's own term arrives verbatim, with the arming reference in it.
custom_case() ->
    Code = nif_state:sel_arm_custom(),
    Early = receive M0 -> {early, M0} after 100 -> timeout end,
    Written = nif_state:sel_poke(),
    Late = receive M -> M after 5000 -> timeout end,
    cm_code({Code, Early, Written, Late}).

cm_code({0, timeout, 1, {custom, R, payload}}) when is_reference(R) -> 1;
cm_code(_) -> 9.

%% WRITE interest on a pipe write end: writable at once, so readiness arrives unpoked.
write_case() ->
    Code = nif_state:sel_arm_write(),
    Late = receive
               {select, _Res, _Ref, Ev} -> {delivered, Ev};
               Other -> {other, Other}
           after 5000 -> timeout
           end,
    ws_code({Code, Late}).

ws_code({0, {delivered, ready_output}}) -> 1;
ws_code(_) -> 9.

%% Arm, drop the library's own reference (the interest keeps the object alive), then STOP: the
%% interest's reference is given back exactly once, so the destructor runs exactly once.
stop_after_release_case() ->
    Code = nif_state:sel_arm(),
    AfterRelease = nif_state:sel_release(),
    Stop = nif_state:sel_stop(),
    Settled = settle_sel(50),
    sr_code({Code, AfterRelease, Stop, Settled}).

%% The oracle runs a resource destructor on a misc aux-work thread and the Rust runtime runs it
%% inline, so *when* it ran is not comparable — the settled count is.
settle_sel(0) -> nif_state:sel_dtors();
settle_sel(N) ->
    case nif_state:sel_dtors() of
        0 -> receive after 20 -> settle_sel(N - 1) end;
        Count -> Count
    end.

sr_code({0, 0, 17, Settled}) -> Settled;
sr_code(_) -> 9.

%% ---- resource identity and refcounting across collections ----

%% One cons cell per iteration, far above one slice's reduction budget, so the process is
%% pre-empted at slice boundaries (where the runtime's own prune runs and the oracle's deferred
%% destructor has settled).
park(0, Acc) -> Acc;
park(N, Acc) -> park(N - 1, [N | Acc]).

%% The term goes through Erlang's heap, a forced collection and a heap-churning allocation, and is
%% still the SAME object: it compares equal to itself, resolves back through the same type to the
%% recorded object pointer, and still carries its payload.
identity_case() ->
    T = nif_state:id_make(),
    _ = park(100000, 0),
    _ = erlang:garbage_collect(),
    _ = park(100000, 0),
    A = nif_state:id_check(T),
    B = nif_state:id_check(T),
    ic_code({A, B}).

ic_code({{1, 1, 1, 1, V}, {1, 1, 1, 1, V}}) -> V;
ic_code(_) -> 9.

%% A native `enif_keep_resource` holds the object after its term has died, and the final release
%% destroys it exactly once.
keep_gc_case() ->
    Fresh = nif_state:id_count(),
    _T = nif_state:id_keep(),
    _ = park(100000, 0),
    _ = erlang:garbage_collect(),
    _ = park(100000, 0),
    Held = nif_state:id_count(),
    _Released = nif_state:id_release(),
    Settled = settle_id(50),
    kg_code({Fresh, Held, Settled}).

settle_id(0) -> nif_state:id_count();
settle_id(N) ->
    case nif_state:id_count() of
        0 -> receive after 20 -> settle_id(N - 1) end;
        Count -> Count
    end.

kg_code({0, 0, Settled}) -> Settled;
kg_code(_) -> 9.
"#;

/// The C library: the monitor, `enif_select` and resource-identity/refcount families.
///
/// Every `enif_*` referenced here is a symbol the `beam_rt` release binary exports (verified with
/// `nm -D`), and nothing C-variadic is used. The resource types are opened by the **load** callback
/// because the C only allows a type to be opened while the module is loading
/// (`erl_nif.c:2704`), and the select type carries the `stop` callback the oracle's STOP path calls
/// unconditionally.
const NIF_STATE_C: &str = r#"/* A real NIF exercising the STATEFUL enif_* families of the NIF C ABI: enif_monitor_process /
   enif_demonitor_process with the resource's `down` callback, enif_select / enif_select_x on real
   pipe descriptors (READ, WRITE, STOP, CANCEL, CUSTOM_MSG), and resource identity + reference
   counting across a garbage collection. */
#include <erl_nif.h>
#include <string.h>
#include <stdint.h>
#include <unistd.h>

typedef struct { int value; } res_t;

static ErlNifResourceType* mon_type = NULL;   /* carries a `down` callback */
static ErlNifResourceType* sel_type = NULL;   /* carries a `stop` callback + a destructor */
static ErlNifResourceType* id_type = NULL;    /* carries a destructor */

/* --- monitors: the object, its armed handle and the `down` count --- */
static void* mon_obj = NULL;
static ErlNifMonitor mon_handle;
static int mon_down_count = 0;

/* --- enif_select: the descriptors, the registered object and the destructor count --- */
static int sel_rd = -1, sel_wr = -1;
static void* sel_obj = NULL;
static int sel_dtor_runs = 0;
static int sel_released = 0;

/* --- resource identity / refcounting --- */
static int id_dtor_runs = 0;
static void* id_last_obj = NULL;
static void* id_keep_obj = NULL;

static void mon_dtor(ErlNifEnv* env, void* obj) { (void) env; (void) obj; }

/* The resource's `down` callback: one per delivered monitor. */
static void mon_down(ErlNifEnv* env, void* obj, ErlNifPid* pid, ErlNifMonitor* mon) {
    (void) env; (void) obj; (void) pid; (void) mon;
    mon_down_count++;
}

static void sel_dtor(ErlNifEnv* env, void* obj) {
    (void) env;
    sel_dtor_runs++;
    if (obj == sel_obj) sel_obj = NULL;
}

/* The ERL_NIF_SELECT_STOP callback: the C's STOP path calls it unconditionally
   (`erl_check_io.c:1530`), so a library without one would crash the oracle. */
static void sel_stop(ErlNifEnv* env, void* obj, ErlNifEvent ev, int is_direct_call) {
    (void) env; (void) obj; (void) ev; (void) is_direct_call;
}

static void id_dtor(ErlNifEnv* env, void* obj) { (void) env; (void) obj; id_dtor_runs++; }

/* The load callback: opens the three resource types (the C may only open one while the module is
   loading, `erl_nif.c:2704`, so a probe cannot do it). */
static int load(ErlNifEnv* env, void** priv, ERL_NIF_TERM load_info) {
    ErlNifResourceFlags tried;
    ErlNifResourceTypeInit init;
    (void) load_info;
    *priv = NULL;
    memset(&init, 0, sizeof init);
    init.dtor = mon_dtor;
    init.down = mon_down;
    init.members = 3;
    mon_type = enif_init_resource_type(env, "nif_state_mon", &init,
                                       ERL_NIF_RT_CREATE | ERL_NIF_RT_TAKEOVER, &tried);
    if (mon_type == NULL) return 1;
    memset(&init, 0, sizeof init);
    init.dtor = sel_dtor;
    init.stop = sel_stop;
    init.members = 3;
    sel_type = enif_init_resource_type(env, "nif_state_sel", &init,
                                       ERL_NIF_RT_CREATE | ERL_NIF_RT_TAKEOVER, &tried);
    if (sel_type == NULL) return 2;
    id_type = enif_open_resource_type(env, NULL, "nif_state_id", id_dtor,
                                      ERL_NIF_RT_CREATE | ERL_NIF_RT_TAKEOVER, &tried);
    if (id_type == NULL) return 3;
    return 0;
}

/* ---- monitors ------------------------------------------------------------------------------- */

/* The resource object the monitors are armed through, as a term: the module holds the term for the
   whole case, so the object (and its monitors) cannot be destroyed under the probe's feet. */
static ERL_NIF_TERM mon_make(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    if (mon_type == NULL) return enif_make_badarg(env);
    if (mon_obj == NULL) {
        mon_obj = enif_alloc_resource(mon_type, sizeof(int));
        if (mon_obj == NULL) return enif_make_badarg(env);
        *(int*) mon_obj = 0;
        mon_down_count = 0;
    }
    return enif_make_resource(env, mon_obj);
}

/* Arm the monitor on `Pid`: the C's 0 established, 1 no live process. */
static ERL_NIF_TERM mon_arm(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifPid pid;
    int code;
    (void) argc;
    if (mon_obj == NULL) return enif_make_atom(env, "no_monitor_object");
    if (!enif_get_local_pid(env, argv[0], &pid)) return enif_make_atom(env, "not_a_pid");
    memset(&mon_handle, 0, sizeof mon_handle);
    code = enif_monitor_process(env, mon_obj, &pid, &mon_handle);
    return enif_make_int(env, code);
}

/* Arm a monitor on an UNDEFINED pid (enif_set_pid_undefined): there is no process to watch.
   The handle goes into a live local, never back into the runtime, so the probe stays safe even
   when the C refuses the monitor. */
static ERL_NIF_TERM mon_arm_undefined(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifPid pid;
    ErlNifMonitor mon;
    int code;
    (void) argc; (void) argv;
    if (mon_obj == NULL) return enif_make_atom(env, "no_monitor_object");
    enif_set_pid_undefined(&pid);
    memset(&mon, 0, sizeof mon);
    code = enif_monitor_process(env, mon_obj, &pid, &mon);
    return enif_make_int(env, code);
}

/* Cancels the armed monitor: the C's 0 identified and removed it, 1 could not identify it (the
   DOWN was already delivered, which is exactly what the monitor round-trip case observes). */
static ERL_NIF_TERM mon_demonitor(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    int code;
    (void) argc; (void) argv;
    if (mon_obj == NULL) return enif_make_atom(env, "no_monitor_object");
    code = enif_demonitor_process(env, mon_obj, &mon_handle);
    return enif_make_int(env, code);
}

/* How many times the `down` callback has run. */
static ERL_NIF_TERM mon_downs(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    return enif_make_int(env, mon_down_count);
}

/* ---- enif_select on real descriptors -------------------------------------------------------- */

/* A fresh pipe, a resource of the library's own type for it, and READ interest on the read end for
   the calling process (`pid == NULL`). Answers the raw `enif_select` code. */
static ERL_NIF_TERM sel_arm(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    int fds[2];
    ERL_NIF_TERM ref;
    int code;
    (void) argc; (void) argv;
    if (pipe(fds) != 0) return enif_make_int(env, -99);
    sel_rd = fds[0];
    sel_wr = fds[1];
    sel_released = 0;
    sel_obj = enif_alloc_resource(sel_type, sizeof(int));
    if (sel_obj == NULL) return enif_make_int(env, -98);
    *(int*) sel_obj = 1;
    ref = enif_make_ref(env);
    code = enif_select(env, (ErlNifEvent) sel_rd, ERL_NIF_SELECT_READ, sel_obj, NULL, ref);
    return enif_make_int(env, code);
}

/* READ | CUSTOM_MSG: the caller's own term (`{custom, Ref, payload}`) replaces the {select,…}
   notification. `msg_env` is NULL, so the message is the call env's own term. */
static ERL_NIF_TERM sel_arm_custom(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    int fds[2];
    ERL_NIF_TERM ref, parts[3], msg;
    int code;
    (void) argc; (void) argv;
    if (pipe(fds) != 0) return enif_make_int(env, -99);
    sel_rd = fds[0];
    sel_wr = fds[1];
    sel_released = 0;
    sel_obj = enif_alloc_resource(sel_type, sizeof(int));
    if (sel_obj == NULL) return enif_make_int(env, -98);
    *(int*) sel_obj = 1;
    ref = enif_make_ref(env);
    parts[0] = enif_make_atom(env, "custom");
    parts[1] = ref;
    parts[2] = enif_make_atom(env, "payload");
    msg = enif_make_tuple_from_array(env, parts, 3);
    code = enif_select_x(env, (ErlNifEvent) sel_rd,
                         (enum ErlNifSelectFlags) (ERL_NIF_SELECT_READ | ERL_NIF_SELECT_CUSTOM_MSG),
                         sel_obj, NULL, msg, NULL);
    return enif_make_int(env, code);
}

/* WRITE interest on the write end of a fresh pipe: a pipe write end is writable as soon as it
   exists, so readiness arrives without any poke. */
static ERL_NIF_TERM sel_arm_write(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    int fds[2];
    ERL_NIF_TERM ref;
    int code;
    (void) argc; (void) argv;
    if (pipe(fds) != 0) return enif_make_int(env, -99);
    sel_rd = -1;
    sel_wr = fds[1];
    sel_released = 0;
    sel_obj = enif_alloc_resource(sel_type, sizeof(int));
    if (sel_obj == NULL) return enif_make_int(env, -98);
    *(int*) sel_obj = 1;
    ref = enif_make_ref(env);
    code = enif_select(env, (ErlNifEvent) fds[1], ERL_NIF_SELECT_WRITE, sel_obj, NULL, ref);
    return enif_make_int(env, code);
}

/* Write the one byte that makes the read end readable; answers the byte count. */
static ERL_NIF_TERM sel_poke(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ssize_t n;
    (void) argc; (void) argv;
    if (sel_wr < 0) return enif_make_int(env, -1);
    n = write(sel_wr, "x", 1);
    return enif_make_int(env, (int) n);
}

/* ERL_NIF_SELECT_STOP on the descriptor the last arm registered: it releases the interest. */
static ERL_NIF_TERM sel_stop_probe(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ERL_NIF_TERM ref;
    void* obj;
    (void) argc; (void) argv;
    if (sel_rd < 0 || sel_obj == NULL) return enif_make_int(env, -1);
    obj = sel_obj;
    ref = enif_make_ref(env);
    return enif_make_int(env, enif_select(env, (ErlNifEvent) sel_rd, ERL_NIF_SELECT_STOP,
                                          obj, NULL, ref));
}

/* ERL_NIF_SELECT_CANCEL | ERL_NIF_SELECT_READ: clears exactly the direction armed. */
static ERL_NIF_TERM sel_cancel(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ERL_NIF_TERM ref;
    void* obj;
    (void) argc; (void) argv;
    if (sel_rd < 0 || sel_obj == NULL) return enif_make_int(env, -1);
    obj = sel_obj;
    ref = enif_make_ref(env);
    return enif_make_int(env, enif_select(env, (ErlNifEvent) sel_rd,
                                          (enum ErlNifSelectFlags) (ERL_NIF_SELECT_CANCEL
                                                                    | ERL_NIF_SELECT_READ),
                                          obj, NULL, ref));
}

/* Drop the library's own reference while the select interest still holds one; answers the
   destructor count reached in-call (the object is not destroyed: the interest owns it). */
static ERL_NIF_TERM sel_release(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    if (sel_obj == NULL || sel_released) return enif_make_int(env, -1);
    sel_released = 1;
    enif_release_resource(sel_obj);
    return enif_make_int(env, sel_dtor_runs);
}

/* How many times the select object's destructor has run. */
static ERL_NIF_TERM sel_dtors(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    return enif_make_int(env, sel_dtor_runs);
}

/* ---- resource identity and refcounting across collections ------------------------------------ */

/* Mint a resource term with payload 7 and remember the object pointer: `alloc` (1) -> term (2) ->
   `release` (1), so the term holds the object's only reference from here on. */
static ERL_NIF_TERM id_make(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    res_t* r;
    ERL_NIF_TERM term;
    (void) argc; (void) argv;
    r = (res_t*) enif_alloc_resource(id_type, sizeof(res_t));
    if (r == NULL) return enif_make_badarg(env);
    r->value = 7;
    id_last_obj = (void*) r;
    term = enif_make_resource(env, r);
    enif_release_resource(r);
    return term;
}

/* `{cmp_eq, identical, resolved, same_object, value}` for a resource term that has been through
   Erlang's own heap (and therefore through at least one collection). */
static ERL_NIF_TERM id_check(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    res_t* got = NULL;
    ERL_NIF_TERM terms[5];
    int ok;
    (void) argc;
    if (argc != 1) return enif_make_badarg(env);
    ok = enif_get_resource(env, argv[0], id_type, (void**) &got) && got != NULL;
    terms[0] = enif_make_int(env, enif_compare(argv[0], argv[0]) == 0 ? 1 : 0);
    terms[1] = enif_make_int(env, enif_is_identical(argv[0], argv[0]) ? 1 : 0);
    terms[2] = enif_make_int(env, ok ? 1 : 0);
    terms[3] = enif_make_int(env, (ok && (void*) got == id_last_obj) ? 1 : 0);
    terms[4] = enif_make_int(env, ok ? got->value : -1);
    return enif_make_tuple_from_array(env, terms, 5);
}

/* alloc (1) -> make_resource (2) -> keep (3) -> release (2): one native hold plus the term. */
static ERL_NIF_TERM id_keep(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    res_t* r;
    ERL_NIF_TERM term;
    (void) argc; (void) argv;
    r = (res_t*) enif_alloc_resource(id_type, sizeof(res_t));
    if (r == NULL) return enif_make_badarg(env);
    r->value = 5;
    id_keep_obj = (void*) r;
    term = enif_make_resource(env, r);
    enif_keep_resource(r);
    enif_release_resource(r);
    return term;
}

/* Give the kept reference back; answers the destructor count reached in-call. */
static ERL_NIF_TERM id_release(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    if (id_keep_obj != NULL) {
        enif_release_resource(id_keep_obj);
        id_keep_obj = NULL;
    }
    return enif_make_int(env, id_dtor_runs);
}

static ERL_NIF_TERM id_count(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    return enif_make_int(env, id_dtor_runs);
}

static ErlNifFunc nif_funcs[] = {
    {"mon_make", 0, mon_make, 0},
    {"mon_arm", 1, mon_arm, 0},
    {"mon_arm_undefined", 0, mon_arm_undefined, 0},
    {"mon_demonitor", 0, mon_demonitor, 0},
    {"mon_downs", 0, mon_downs, 0},
    {"sel_arm", 0, sel_arm, 0},
    {"sel_arm_custom", 0, sel_arm_custom, 0},
    {"sel_arm_write", 0, sel_arm_write, 0},
    {"sel_poke", 0, sel_poke, 0},
    {"sel_stop", 0, sel_stop_probe, 0},
    {"sel_cancel", 0, sel_cancel, 0},
    {"sel_release", 0, sel_release, 0},
    {"sel_dtors", 0, sel_dtors, 0},
    {"id_make", 0, id_make, 0},
    {"id_check", 1, id_check, 0},
    {"id_keep", 0, id_keep, 0},
    {"id_release", 0, id_release, 0},
    {"id_count", 0, id_count, 0}
};

ERL_NIF_INIT(nif_state, nif_funcs, load, NULL, NULL, NULL)
"#;

/// The library every case loads.
const NIF_STATE: NifFixture = NifFixture::CompiledLibrary {
    name: "nif_state",
    c_source: NIF_STATE_C,
};

/// Build one module-run case over [`NIF_STATE_MODULE_SOURCE`].
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
            source: NIF_STATE_MODULE_SOURCE,
            module: "nif_state",
            function: "classify",
            arity: 1,
            args: selector,
            nif: Some(NIF_STATE),
        }),
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// The A.3 stateful-family group. Each golden is the integer the **oracle** prints (re-verified
/// per case with `differential run <id>`).
pub(crate) fn group() -> Vec<Case> {
    vec![
        case(
            "nif_state/monitor_down_then_demonitor_is_not_identified",
            "a monitor armed on a live pid fires one down on its death, and the demonitor that follows cannot identify it",
            &[1],
            // 10 * (the one delivered down) + 1 (the C's "could not identify" demonitor code).
            "11",
        ),
        case(
            "nif_state/monitor_of_an_undefined_pid_is_refused",
            "enif_monitor_process of an enif_set_pid_undefined target is the C's 1 and fabricates no down",
            &[2],
            // 10 * (the refused monitor code) + 0 (downs fired).
            "10",
        ),
        case(
            "nif_state/select_stop_releases_an_armed_read",
            "ERL_NIF_SELECT_STOP on an armed pipe read answers STOP_CALLED|READ_CANCELLED, nothing is delivered, a second STOP answers STOP_CALLED alone",
            &[3],
            // 17 (STOP_CALLED|READ_CANCELLED) * 10 + 1 (STOP_CALLED alone).
            "171",
        ),
        case(
            "nif_state/select_cancel_clears_only_the_armed_direction",
            "ERL_NIF_SELECT_CANCEL|SELECT_READ reports READ_CANCELLED, nothing is delivered, a second CANCEL cancels nothing",
            &[4],
            // 16 (READ_CANCELLED) * 10 + 0 (a second CANCEL clears nothing).
            "160",
        ),
        case(
            "nif_state/select_custom_msg_delivers_the_callers_term",
            "enif_select_x with READ|CUSTOM_MSG delivers the caller's {custom, Ref, payload} term verbatim, with the arming reference inside it",
            &[5],
            // The message arrived verbatim (shape reduced to an integer: the ref's identity is not).
            "1",
        ),
        case(
            "nif_state/resource_identity_survives_a_collection",
            "a resource term that went through a forced garbage_collect is the same object: enif_compare 0, identical, same payload pointer",
            &[6],
            // The payload both checks resolved the surviving term to.
            "7",
        ),
        case(
            "nif_state/keep_resource_outlives_the_term_across_a_collection",
            "enif_keep_resource holds an object past its term's collection, and the final enif_release_resource destroys it exactly once",
            &[7],
            // The settled destructor count: 0 while the native reference was held, 1 after the release.
            "1",
        ),
        case(
            "nif_state/select_write_end_delivers_ready_output",
            "WRITE interest on a pipe write end answers 0 and delivers {select,…} with ready_output unpoked",
            &[8],
            // The write direction really became ready.
            "1",
        ),
        case(
            "nif_state/select_interest_holds_the_object_until_stop",
            "a select interest's own reference keeps a released object alive, and the STOP gives it back exactly once",
            &[9],
            // 0 in-call after the release (the interest holds it), then the settled destructor count 1.
            "1",
        ),
    ]
}

#[cfg(test)]
mod tests {
    //! Invariants of the A.3 stateful-family group.

    use super::{group, CaseKind, Contract, NifFixture, NIF_STATE_C, NIF_STATE_MODULE_SOURCE};
    use std::collections::HashSet;

    /// The probe functions the module exports and calls **externally**; each must be a stub of the
    /// same arity and every call into the library must go through `nif_state:` (a local call would
    /// reach the stub and raise `not_loaded` on the Rust side only).
    const PROBES: &[&str] = &[
        "mon_make",
        "mon_arm",
        "mon_arm_undefined",
        "mon_demonitor",
        "mon_downs",
        "sel_arm",
        "sel_arm_custom",
        "sel_arm_write",
        "sel_poke",
        "sel_stop",
        "sel_cancel",
        "sel_release",
        "sel_dtors",
        "id_make",
        "id_check",
        "id_keep",
        "id_release",
        "id_count",
    ];

    #[test]
    fn every_case_is_a_nif_module_run_over_the_same_compiled_library() {
        let mut selectors = HashSet::new();
        for case in group() {
            assert!(
                case.id.starts_with("nif_state/"),
                "case {} is outside the nif_state group",
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
            assert_eq!(run.module, "nif_state", "case {}", case.id);
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
        assert_eq!(selectors.len(), 9, "the group covers nine distinct selectors");
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
                    name: "nif_state",
                    c_source: NIF_STATE_C,
                }),
                "case {}: the fixture must be the group's own compiled library",
                case.id
            );
            assert!(
                NIF_STATE_MODULE_SOURCE.contains("Dir ++ \"/nif_state\""),
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
                NIF_STATE_MODULE_SOURCE.contains(&stub),
                "the module must export a stub for {probe}"
            );
            let external = format!("nif_state:{probe}(");
            assert!(
                NIF_STATE_MODULE_SOURCE.contains(&external),
                "the module must call {probe} through `nif_state:` (external dispatch)"
            );
            assert!(
                NIF_STATE_C.contains(&format!("\"{probe}\"")),
                "the library must register {probe}"
            );
        }
    }

    #[test]
    fn the_library_references_the_stateful_slots_it_pins() {
        // What makes the group "stateful": the monitor pair, the select family and the resource
        // identity/refcount calls. A fixture that dropped one would silently pin less.
        for symbol in [
            "enif_monitor_process",
            "enif_demonitor_process",
            "enif_set_pid_undefined",
            "enif_select",
            "enif_select_x",
            "enif_get_resource",
            "enif_keep_resource",
            "enif_release_resource",
            "enif_compare",
            "enif_is_identical",
        ] {
            assert!(NIF_STATE_C.contains(symbol), "the library must reference {symbol}");
        }
        // The divergent probe (the type's `stop` callback count) is deliberately absent: the oracle
        // invokes it once per STOP and the Rust runtime never does (`divergences.md` §A.3).
        assert!(
            !NIF_STATE_C.contains("stop_calls"),
            "the stop-callback count is a recorded divergence and must not be pinned"
        );
    }

    #[test]
    fn every_case_level_wait_is_bounded() {
        // An unbounded `receive` in the *case* flow would hang the harness. (The `victim/0`
        // process blocks in an unbounded `receive` on purpose: the case kills it.)
        for wait in [
            "after 100 -> timeout",
            "after 300 -> timeout",
            "after 5000 -> timeout",
            "receive after 5 -> ok",
            "receive after 20 -> settle",
        ] {
            assert!(
                NIF_STATE_MODULE_SOURCE.contains(wait),
                "the module must bound its waits with {wait:?}"
            );
        }
        assert!(
            !NIF_STATE_MODULE_SOURCE.contains("= self()"),
            "no case may pin a per-VM pid"
        );
    }
}
