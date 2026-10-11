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

//! Contract **A.3** — the **resource / ioq / sync** families of the NIF C ABI.
//!
//! [`super::nif_so`] pins the *call* surface of a real NIF library (`enif_get_int`,
//! `enif_make_atom`, `enif_make_reverse_list`, `enif_make_badarg`, the `load_nif/2` failure
//! shapes). This group adds the families that have no counterpart there: the **resource
//! types** (`enif_open_resource_type` + `enif_alloc_resource` + `enif_sizeof_resource` +
//! `enif_keep_resource`/`enif_release_resource`), the **IO queue** (`enif_ioq_create` /
//! `enif_ioq_enq_binary` / `enif_ioq_size` / `enif_ioq_peek` / `enif_ioq_deq` /
//! `enif_ioq_destroy`), the **thread/sync** slots (`enif_mutex_*`, `enif_cond_*`,
//! `enif_rwlock_*`, `enif_tsd_*`, `enif_thread_*` and the `enif_{mutex,cond,rwlock}_name`
//! accessors) and the **extra-environment** family (`enif_alloc_env` / `enif_make_copy` /
//! `enif_free_env`).
//!
//! The C library is compiled in the case's own temp dir against the host `erl_nif.h` and
//! `erlang:load_nif/2`ed by the embedded module exactly like `nif_so`, so both runtimes load
//! *the same* `.so` and run *the same* compiled module; only stdout + exit status are
//! compared. A case therefore agrees only if the Rust runtime really exports and implements
//! the `enif_*` slots the library binds.
//!
//! # Why only these selectors are pinned
//!
//! The library is `dlopen(RTLD_NOW)`ed by **both** runtimes, so every `enif_*` it references
//! must be a symbol the Rust runtime really defines; a missing one fails the *Rust* load
//! while the oracle loads it — a **mismatch**, not a capability gap. Only the families that
//! were verified to agree end to end under both runtimes are therefore pinned here.
//!
//! The resource **round trip** (`enif_make_resource` + `enif_get_resource`) and
//! **self-liveness** (`enif_is_process_alive` on `enif_self`) are pinned cases: the oracle and
//! the Rust runtime both answer `42` for the round trip and `1` for self-liveness (re-verified
//! against the live oracle, OTP 29.1.1 / erts-17.1). The **post-release read** is pinned too:
//! a term minted by `enif_make_resource` keeps the resource object alive after
//! `enif_release_resource` (the documented C contract: "the call to `enif_release_resource` can
//! occur immediately after obtaining the term … the resource object is deallocated when the term
//! is garbage collected"), so `enif_get_resource` on that term still answers `42` under both
//! runtimes (`nif_res/resource_term_keeps_object_alive_after_release`).
//!
//! The **extra-environment** family (`enif_alloc_env` / `enif_free_env` / `enif_make_copy`) is
//! split by what the runtime can do. `beam_rt` implements the `alloc_env`/`free_env` hooks, so
//! `enif_alloc_env` now answers a live, process-independent environment whose atom table is the
//! runtime's own — `nif_res/alloc_env_free_roundtrip` pins that end to end (**agree**). The copy
//! half is still a *capability gap*: the runtime leaves the `NifHost::make_copy` hook at its
//! unimplemented default, so `enif_make_copy` answers the C's `THE_NON_VALUE` (`0`, the ABI's
//! "no term"). `nif_res/alloc_env_roundtrip` therefore reports the module's
//! `unsupported_enif_alloc_env` raise (exit `3`, `Verdict::Unsupported`) — never a mismatch.
//! The probe in [`NIF_RES_C`] checks the copied term against the non-value itself, so the engine
//! is never handed a non-term; `NifHost::make_copy` gaining an implementation flips the case to
//! `agree` with no fixture change.
//!
//! One further verification is *not* pinned: a resource's **destructor** is **asynchronous** in
//! the oracle: `erts` schedules it on a misc aux-work thread (`nif_resource_dtor_prologue` →
//! `erts_schedule_misc_aux_work`, `erl_nif.c`), so a `dtor_count` read inside the same NIF call,
//! right after the final `enif_release_resource`, is still `0` there — while the Rust runtime runs
//! the destructor synchronously on the releasing thread and reports `1`. The count is not
//! observable within the NIF call on the oracle side, so no case pins it. (The related
//! `enif_monitor_process` follow-up is likewise unpinnable: both runtimes answer `0` because the
//! monitor is not established, so it would pin no behaviour.)
//!
//! # The sync family is completed (cond / rwlock / tsd / threads / names)
//!
//! The `mutex_*` + `enif_thread_self` pair above is joined by the rest of the `erl_drv_thread`
//! surface, each pinned through the shape both runtimes answer alike (the same shapes the
//! `beam_abi` fixture `tests/fixtures/nif_threads.c` already checks end to end):
//!
//! * `cond_roundtrip` — a spawned thread blocks in `enif_cond_wait` on a shared mutex and is woken
//!   by `enif_cond_signal`; the mutex hand-off makes the wake-up unlosable, and the joined value
//!   is `42` (`2`).
//! * `rwlock_roundtrip` — `enif_rwlock_tryrwlock` reports `EBUSY` while a reader holds the lock,
//!   and the write lock round-trips.
//! * `tsd_roundtrip` — `enif_tsd_*` is **per-thread**: a fresh thread's slot is unset, the parent's
//!   write is invisible to it and survives the join.
//! * `thread_roundtrip` — `enif_thread_opts_create` / `enif_thread_create` / `enif_thread_join`
//!   round-trip the entry point's value, and `enif_thread_self` / `enif_equal_tids` agree with
//!   themselves.
//! * `names` — the `enif_{mutex,cond,rwlock}_name` accessors report the **named** handle's own
//!   name. (The unnamed-handle shape is a documented deviation and is not exercised; neither is
//!   `enif_thread_name` or `enif_thread_type`.)
//!
//! # Skip policy (inherited from [`super::nif_so`])
//!
//! A C-compiled fixture needs a C compiler *and* the host `erl_nif.h`; when either is
//! missing the harness reports `Verdict::Skipped` with a loud reason
//! ([`crate::module_build::MISSING_CC_REASON`] / `MISSING_ERL_NIF_REASON`).
//!
//! # The second wave: the remaining pinnable resource / ioq / sync shapes
//!
//! * `itype_sizeof` / `xtype_sizeof` — the other two ways to open a resource type:
//!   `enif_init_resource_type` (init `members` = 1, dtor only) and `enif_open_resource_type_x`
//!   (which always reads dtor/stop/down and reports the `tried` flag). Both types are opened by
//!   the **load** callback, because the oracle only lets a resource type be opened while the
//!   module is loading (`open_resource_type` requires the module's LOADING flag, `erl_nif.c:2704`);
//!   a probe that opened one would get `NULL` there.
//! * `resource_binary_backed_by_resource` — `enif_make_resource_binary` mints a binary term whose
//!   bytes are the resource's own, still readable after the native reference is released. (Only
//!   the bytes and the size are pinned: on the oracle that term is deliberately *not* a resource
//!   handle any more - `erl_nif.c:3222` wraps it in a magic binary - so a `enif_get_resource` on
//!   it is not a shape the two runtimes share.)
//! * `ioq_enqv_skip_peek_deq` / `ioq_peek_head_first_chunk` / `ioq_deq_partial_leaves_remainder` /
//!   `ioq_empty_queue_probe` — the rest of the IO-queue surface: `enif_ioq_enqv` fed by an
//!   `enif_inspect_iovec` slice with a byte `skip`, `enif_ioq_peek_head` (which copies the head
//!   chunk without dequeuing), a partial `enif_ioq_deq` (its count is a **byte** count,
//!   `erl_io_queue.c:347`, and its out-parameter reports what *remains*), and the empty-queue
//!   answer (`NULL` view, vector count 0, refused `peek_head`). `enif_free_iovec` is deliberately
//!   not called on the env-allocated vector (the oracle attaches it to the env, `erl_nif.c:4315`).
//! * `mutex_trylock_reports_busy` / `rwlock_tryrlock_busy_under_writer` — the `try*` halves, taken
//!   from a **second thread** while the calling thread holds the lock, so the answer is the
//!   unambiguous `EBUSY` rather than a same-thread-specific code.
//! * `cond_broadcast_wakes_both_waiters` — one `enif_cond_broadcast` wakes two waiters (the
//!   mutex hand-off plus a flag make a lost wake-up impossible).
//! * `thread_exit_value_reaches_joiner` — `enif_thread_exit`'s value is what `enif_thread_join`
//!   reports.
//! * `getenv_reports_present_and_missing` — `enif_getenv`'s code surface without depending on a
//!   variable's contents: a 1-byte buffer cannot hold any non-empty value, so a present variable
//!   answers `1` and an absent one `-1`. (The oracle reads a snapshot of the environment taken at
//!   VM start, so a `setenv` performed inside the NIF is deliberately not exercised.)
//! * `env_clear_env_stays_usable` — `enif_clear_env` on a live message env: atoms built on that
//!   env afterwards are atoms, and they are the *global* atoms the call env also makes.
//!
//! One further slot is **not** pinned because the two runtimes disagree on it today:
//! `enif_priv_data` answers the pointer the load callback stored (`erl_nif.c:609`) under the
//! oracle, while the Rust runtime leaves the `NifHost::priv_data` hook at its default and answers
//! `NULL`. That is an observable difference, so no case pins it; it becomes pinnable the moment
//! the host hook is implemented.

use super::{Case, CaseKind, Contract, Golden, ModuleRun};
use crate::module_build::NifFixture;

/// The embedded module: one stub and one `classify/1` branch per `ErlNifFunc` entry.
///
/// `erlang:load_nif/2` refuses a library whose functions do not exist **and are exported**
/// in the loading module, and the calls into the library must be **external** (the Rust
/// runtime intercepts a NIF at external dispatch) — the same three rules `nif_so` pins.
const NIF_RES_MODULE_SOURCE: &str = r#"-module(nif_res).
-export([classify/1, res_size/0, ioq_size/0, sync_roundtrip/0, res_roundtrip/0, self_alive/0,
         alloc_env_probe/0, alloc_env_simple/0, res_read_after_release/0, cond_roundtrip/0,
         rwlock_roundtrip/0, tsd_roundtrip/0, thread_roundtrip/0, names/0,
         itype_sizeof/0, xtype_sizeof/0, resource_binary_probe/0,
         ioq_enqv_skip_probe/0, ioq_peek_head_probe/0, ioq_deq_partial_probe/0,
         ioq_empty_probe/0, mutex_trylock_probe/0, rwlock_tryrlock_probe/0,
         cond_broadcast_probe/0, thread_exit_probe/0, getenv_probe/0, env_clear_probe/0]).

res_size() -> erlang:nif_error(not_loaded).
ioq_size() -> erlang:nif_error(not_loaded).
sync_roundtrip() -> erlang:nif_error(not_loaded).
res_roundtrip() -> erlang:nif_error(not_loaded).
self_alive() -> erlang:nif_error(not_loaded).
alloc_env_probe() -> erlang:nif_error(not_loaded).
alloc_env_simple() -> erlang:nif_error(not_loaded).
res_read_after_release() -> erlang:nif_error(not_loaded).
cond_roundtrip() -> erlang:nif_error(not_loaded).
rwlock_roundtrip() -> erlang:nif_error(not_loaded).
tsd_roundtrip() -> erlang:nif_error(not_loaded).
thread_roundtrip() -> erlang:nif_error(not_loaded).
names() -> erlang:nif_error(not_loaded).
itype_sizeof() -> erlang:nif_error(not_loaded).
xtype_sizeof() -> erlang:nif_error(not_loaded).
resource_binary_probe() -> erlang:nif_error(not_loaded).
ioq_enqv_skip_probe() -> erlang:nif_error(not_loaded).
ioq_peek_head_probe() -> erlang:nif_error(not_loaded).
ioq_deq_partial_probe() -> erlang:nif_error(not_loaded).
ioq_empty_probe() -> erlang:nif_error(not_loaded).
mutex_trylock_probe() -> erlang:nif_error(not_loaded).
rwlock_tryrlock_probe() -> erlang:nif_error(not_loaded).
cond_broadcast_probe() -> erlang:nif_error(not_loaded).
thread_exit_probe() -> erlang:nif_error(not_loaded).
getenv_probe() -> erlang:nif_error(not_loaded).
env_clear_probe() -> erlang:nif_error(not_loaded).

classify(N) ->
    Dir = filename:dirname(code:which(?MODULE)),
    case erlang:load_nif(Dir ++ "/nif_res", 0) of
        ok ->
            case N of
                1 -> nif_res:res_size();
                2 -> nif_res:ioq_size();
                3 -> nif_res:sync_roundtrip();
                4 -> nif_res:res_roundtrip();
                5 -> nif_res:self_alive();
                6 -> simple_env(nif_res:alloc_env_probe());
                7 -> simple_env(nif_res:alloc_env_simple());
                8 -> nif_res:res_read_after_release();
                9 -> nif_res:cond_roundtrip();
                10 -> nif_res:rwlock_roundtrip();
                11 -> nif_res:tsd_roundtrip();
                12 -> nif_res:thread_roundtrip();
                13 -> nif_res:names();
                14 -> nif_res:itype_sizeof();
                15 -> nif_res:xtype_sizeof();
                16 -> nif_res:resource_binary_probe();
                17 -> nif_res:ioq_enqv_skip_probe();
                18 -> nif_res:ioq_peek_head_probe();
                19 -> nif_res:ioq_deq_partial_probe();
                20 -> nif_res:ioq_empty_probe();
                21 -> nif_res:mutex_trylock_probe();
                22 -> nif_res:rwlock_tryrlock_probe();
                23 -> nif_res:cond_broadcast_probe();
                24 -> nif_res:thread_exit_probe();
                25 -> nif_res:getenv_probe();
                26 -> nif_res:env_clear_probe()
            end;
        {error, {Reason, _Text}} ->
            reason_code(Reason)
    end.

%% The extra-environment probes answer the atom `ok` when the runtime does the whole idiom, and
%% the capability-gap atom `unsupported_enif_alloc_env` when it cannot (a `NULL` `enif_alloc_env`,
%% or an `enif_make_copy` that answers the ABI's non-value). Either gap atom raises the documented
%% `unsupported` reason (exit 3) rather than reaching the engine as a non-term.
simple_env(ok) -> 1;
simple_env(_) -> erlang:error(unsupported_enif_alloc_env).

%% Only the reason *tag* leaves this function: the `Text` of a failure embeds the absolute
%% temp-dir path, which would make the golden drift on every run.
reason_code(ok) -> 0;
reason_code(load_failed) -> 1;
reason_code(bad_lib) -> 2;
reason_code(reload) -> 3;
reason_code(_) -> 9.
"#;

/// The C library: the resource-type, IO-queue, sync and extra-environment families, one probe
/// function each.
///
/// Every `enif_*` here is a symbol the `beam_rt` release binary exports (verified with
/// `nm -D`); nothing C-variadic is referenced. The extra-environment family
/// (`enif_alloc_env`/`enif_free_env`/`enif_make_copy`) is referenced by [`NIF_RES_C`]'s
/// `alloc_env_probe`/`alloc_env_simple` because the runtime *does* export those slots: it
/// implements `alloc_env`/`free_env` (a live environment) but leaves `make_copy` at the trait
/// default, so the copy probe reports the `unsupported_enif_alloc_env` capability-gap atom
/// rather than returning the C's non-value to the engine.
const NIF_RES_C: &str = r#"/* A real NIF exercising the resource / ioq / sync families of the NIF C ABI. */
#include <erl_nif.h>
#include <string.h>
#include <stdint.h>
#include <errno.h>
typedef struct { int value; } res_t;
/* The payload of the second and third resource types: a byte buffer the resource-binary probe
   backs a binary with, plus a scalar field. */
typedef struct { unsigned char buf[8]; int value; } xres_t;

static int dtor_count = 0;
static ErlNifResourceType* res_type = NULL;
static ErlNifResourceType* itype = NULL;
static ErlNifResourceType* xtype = NULL;
static ErlNifResourceFlags itype_tried = 0;
static ErlNifResourceFlags xtype_tried = 0;

static void res_dtor(ErlNifEnv* env, void* obj) {
    (void) env;
    (void) obj;
    dtor_count++;
}

/* The second and third types are opened here, during the load callback, because the C may only
   open a resource type while the module is loading (`open_resource_type` requires the module's
   LOADING flag, `erl_nif.c:2704`): a probe that opened one would get NULL from the oracle. A
   failure to open either is not fatal to the load; the probe reports it as its own code. */
static int load(ErlNifEnv* env, void** priv, ERL_NIF_TERM load_info) {
    ErlNifResourceTypeInit itype_init, xtype_init;
    (void) load_info;
    res_type = enif_open_resource_type(env, NULL, "nif_res_type", res_dtor, ERL_NIF_RT_CREATE, NULL);
    if (res_type == NULL)
        return 1;
    memset(&itype_init, 0, sizeof itype_init);
    itype_init.dtor = res_dtor;
    itype_init.members = 1;
    itype = enif_init_resource_type(env, "nif_res_itype", &itype_init,
                                    ERL_NIF_RT_CREATE, &itype_tried);
    /* `enif_open_resource_type_x` always reads three callbacks (dtor/stop/down), whatever
       `members` says (`erl_nif.c:2788`); only `dtor` is set, which is the common idiom. */
    memset(&xtype_init, 0, sizeof xtype_init);
    xtype_init.dtor = res_dtor;
    xtype_init.members = 3;
    xtype = enif_open_resource_type_x(env, "nif_res_xtype", &xtype_init,
                                      ERL_NIF_RT_CREATE, &xtype_tried);
    *priv = (void*) res_type;
    return 0;
}

/* The resource-type family: allocate a resource, keep it (refcount 2), ask for its size,
   then release it twice. Returns sizeof(res_t) = 4. */
static ERL_NIF_TERM res_size(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    size_t n;
    res_t* r = (res_t*) enif_alloc_resource(res_type, sizeof(res_t));
    if (r == NULL)
        return enif_make_badarg(env);
    n = enif_sizeof_resource(r);
    enif_keep_resource(r);
    enif_release_resource(r);
    enif_release_resource(r);
    return enif_make_int(env, (int) n);
}

/* The ioq family: a NORMAL queue takes ownership of two heap binaries; size, peek and deq
   are all observable. Returns size*1000 + dequeued*100 + iovlen*10 + deq_ok. */
static ERL_NIF_TERM ioq_size(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    ErlNifIOQueue* q = enif_ioq_create(ERL_NIF_IOQ_NORMAL);
    ErlNifBinary b1, b2;
    size_t n, got = 0, after;
    int iovlen = 0, deq;
    SysIOVec* v;
    if (q == NULL || !enif_alloc_binary(3, &b1) || !enif_alloc_binary(2, &b2))
        return enif_make_badarg(env);
    memcpy(b1.data, "abc", 3);
    memcpy(b2.data, "de", 2);
    if (!enif_ioq_enq_binary(q, &b1, 0) || !enif_ioq_enq_binary(q, &b2, 0))
        return enif_make_badarg(env);
    n = enif_ioq_size(q);
    v = enif_ioq_peek(q, &iovlen);
    deq = enif_ioq_deq(q, n, &got);
    after = enif_ioq_size(q);
    enif_ioq_destroy(q);
    return enif_make_int(env, (int) (n * 1000 + got * 100 + (v != NULL ? iovlen : 0) * 10
                                    + (deq ? 1 : 0) + (after == 0 ? 0 : 1)));
}

/* The sync family: create a named mutex, lock/unlock it, ask for the calling thread's id and
   destroy it. Returns 2 when every call succeeded. */
static ERL_NIF_TERM sync_roundtrip(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    int thr;
    ErlNifMutex* m = enif_mutex_create("nif_res_mutex");
    if (m == NULL)
        return enif_make_int(env, 0);
    enif_mutex_lock(m);
    thr = (enif_thread_self() != 0);
    enif_mutex_unlock(m);
    enif_mutex_destroy(m);
    return enif_make_int(env, (thr ? 1 : 0) + 1);
}

/* The make/get round trip: allocate a resource, mint its term (enif_make_resource), resolve it
   back through the same type (enif_get_resource) and read the payload field. Returns 42 when
   the resolved pointer is the allocated one; -1 when enif_get_resource does not match. The read
   happens before the release, so the native reference is still held here;
   `res_read_after_release` pins the complementary order. */
static ERL_NIF_TERM res_roundtrip(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    res_t* r = (res_t*) enif_alloc_resource(res_type, sizeof(res_t));
    res_t* got = NULL;
    ERL_NIF_TERM term;
    int value;
    if (r == NULL)
        return enif_make_badarg(env);
    r->value = 42;
    term = enif_make_resource(env, r);
    if (!enif_get_resource(env, term, res_type, (void**) &got) || got == NULL) {
        enif_release_resource(r);
        return enif_make_int(env, -1);
    }
    value = got->value;
    enif_release_resource(r);
    return enif_make_int(env, value);
}

/* Self-liveness: the calling process is alive by definition, so enif_is_process_alive on the
   pid from enif_self is 1. */
static ERL_NIF_TERM self_alive(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    ErlNifPid pid;
    if (!enif_self(env, &pid))
        return enif_make_int(env, -1);
    return enif_make_int(env, enif_is_process_alive(env, &pid) ? 1 : 0);
}

/* The extra-environment family, copy half: allocate a process-independent environment
   (enif_alloc_env), build the atom `ok` on it, copy that term back into the call environment
   (enif_make_copy) and free the message env again (enif_free_env). Returns the atom `ok` when the
   runtime can do all of it. `enif_alloc_env` answering NULL — or `enif_make_copy` answering the
   ABI's `THE_NON_VALUE` (0, "no term"), which a runtime without the copy hook returns — is
   reported as the capability-gap atom `unsupported_enif_alloc_env` instead of handing the engine
   a non-term. */
static ERL_NIF_TERM alloc_env_probe(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    ErlNifEnv* e = enif_alloc_env();
    ERL_NIF_TERM t, c;
    if (e == NULL)
        return enif_make_atom(env, "unsupported_enif_alloc_env");
    t = enif_make_atom(e, "ok");
    c = enif_make_copy(env, t);
    enif_free_env(e);
    if (c == 0)
        return enif_make_atom(env, "unsupported_enif_alloc_env");
    return c;
}

/* The extra-environment family, allocation half: allocate a live, process-independent
   environment (enif_alloc_env), build the atom `ok` on it and free it again (enif_free_env).
   An atom is global, so the term built on the message env is still valid after that env is
   freed. Returns the atom `ok`, or the capability-gap atom when `enif_alloc_env` answers NULL. */
static ERL_NIF_TERM alloc_env_simple(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    ErlNifEnv* e = enif_alloc_env();
    ERL_NIF_TERM t;
    if (e == NULL)
        return enif_make_atom(env, "unsupported_enif_alloc_env");
    t = enif_make_atom(e, "ok");
    enif_free_env(e);
    return t;
}

/* The post-release read: the term minted by `enif_make_resource` keeps the resource object alive
   after `enif_release_resource` (the object is deallocated only when the term is garbage
   collected), so `enif_get_resource` on that term still resolves and the payload is 42. Returns 0
   when the term no longer resolves to the resource. */
static ERL_NIF_TERM res_read_after_release(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    res_t* r = (res_t*) enif_alloc_resource(res_type, sizeof(res_t));
    res_t* got = NULL;
    ERL_NIF_TERM term;
    if (r == NULL)
        return enif_make_badarg(env);
    r->value = 42;
    term = enif_make_resource(env, r);
    enif_release_resource(r);
    if (!enif_get_resource(env, term, res_type, (void**) &got) || got == NULL)
        return enif_make_int(env, 0);
    return enif_make_int(env, got->value);
}

/* The condition-variable family: a spawned thread blocks in enif_cond_wait on a shared mutex and
   is woken by enif_cond_signal. The mutex hand-off makes the wake-up unlosable (a worker that
   starts late simply observes the flag and returns without waiting). Returns 2 when the waiter
   joined cleanly AND returned 42, 0 otherwise. */
static ErlNifMutex* cond_mutex;
static ErlNifCond* cond_var;
static int cond_go;

static void* cond_worker(void* arg) {
    (void) arg;
    enif_mutex_lock(cond_mutex);
    while (!cond_go)
        enif_cond_wait(cond_var, cond_mutex);
    enif_mutex_unlock(cond_mutex);
    return (void*)(intptr_t) 42;
}

static ERL_NIF_TERM cond_roundtrip(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    ErlNifTid tid = NULL;
    void* resp = NULL;
    int create_ret, join_ret;
    cond_mutex = enif_mutex_create("nif_res_cond_mutex");
    cond_var = enif_cond_create("nif_res_cond");
    cond_go = 0;
    if (cond_mutex == NULL || cond_var == NULL)
        return enif_make_int(env, 0);
    create_ret = enif_thread_create("nif_res_cond_worker", &tid, cond_worker, NULL, NULL);
    if (create_ret != 0)
        tid = NULL;
    enif_mutex_lock(cond_mutex);
    cond_go = 1;
    enif_cond_signal(cond_var);
    enif_mutex_unlock(cond_mutex);
    join_ret = tid ? enif_thread_join(tid, &resp) : create_ret;
    enif_cond_destroy(cond_var);
    enif_mutex_destroy(cond_mutex);
    return enif_make_int(env, (join_ret == 0 && (intptr_t) resp == 42) ? 2 : 0);
}

/* The reader/writer lock family: a read lock excludes a write lock (tryrwlock reports EBUSY while
   a reader holds it), and the write lock round-trips. Returns EBUSY when the exclusion held, else
   0. */
static ERL_NIF_TERM rwlock_roundtrip(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    ErlNifRWLock* l = enif_rwlock_create("nif_res_rwlock");
    int busy;
    if (l == NULL)
        return enif_make_int(env, 0);
    enif_rwlock_rlock(l);
    busy = enif_rwlock_tryrwlock(l);
    enif_rwlock_runlock(l);
    enif_rwlock_rwlock(l);
    enif_rwlock_rwunlock(l);
    enif_rwlock_destroy(l);
    return enif_make_int(env, busy);
}

/* The thread-specific-data family: a fresh thread's slot is unset, the parent's write is
   invisible to it, and the parent's own read is unchanged. Returns 1 when the whole per-thread
   contract held. */
static ErlNifTSDKey tsd_key;

static void* tsd_worker(void* arg) {
    (void) arg;
    return enif_tsd_get(tsd_key);
}

static ERL_NIF_TERM tsd_roundtrip(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    ErlNifTSDKey key = -1;
    ErlNifTid tid = NULL;
    void* resp = (void*)(intptr_t) 0xdead;   /* a sentinel the join must overwrite */
    int key_ret, create_ret = -1, join_ret = -1, ok;
    key_ret = enif_tsd_key_create("nif_res_tsd", &key);
    tsd_key = key;
    if (key_ret == 0) {
        enif_tsd_set(key, (void*)(intptr_t) 0x11);
        create_ret = enif_thread_create("nif_res_tsd_worker", &tid, tsd_worker, NULL, NULL);
        if (create_ret != 0)
            tid = NULL;
        join_ret = tid ? enif_thread_join(tid, &resp) : create_ret;
    }
    ok = (key_ret == 0 && create_ret == 0 && join_ret == 0 && resp == NULL &&
          enif_tsd_get(key) == (void*)(intptr_t) 0x11);
    if (key_ret == 0)
        enif_tsd_key_destroy(key);
    return enif_make_int(env, ok ? 1 : 0);
}

/* The thread family: enif_thread_opts_create/enif_thread_create/enif_thread_join round-trip the
   entry point's return value (42), and enif_thread_self/enif_equal_tids agree with themselves.
   Returns 1 when every part of that contract held. */
static void* answer_42(void* arg) {
    (void) arg;
    return (void*)(intptr_t) 42;
}

static ERL_NIF_TERM thread_roundtrip(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    ErlNifThreadOpts* opts = enif_thread_opts_create("nif_res_opts");
    ErlNifTid tid = NULL;
    void* resp = NULL;
    int opts_ok = (opts != NULL);
    int create_ret, join_ret, ok;
    create_ret = enif_thread_create("nif_res_t42", &tid, answer_42, NULL, opts);
    if (create_ret != 0)
        tid = NULL;
    join_ret = tid ? enif_thread_join(tid, &resp) : create_ret;
    if (opts != NULL)
        enif_thread_opts_destroy(opts);
    ok = (opts_ok && create_ret == 0 && join_ret == 0 && (intptr_t) resp == 42 &&
          enif_thread_self() != NULL &&
          enif_equal_tids(enif_thread_self(), enif_thread_self()));
    return enif_make_int(env, ok ? 1 : 0);
}

/* The debug-name accessors on NAMED handles: each reports the name it was created with (the
   unnamed-handle shape is a documented deviation and is not exercised). Returns 1 when all three
   names matched. */
static ERL_NIF_TERM names(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    ErlNifMutex* m = enif_mutex_create("nif_res_mname");
    ErlNifCond* c = enif_cond_create("nif_res_cname");
    ErlNifRWLock* l = enif_rwlock_create("nif_res_lname");
    char *mn, *cn, *ln;
    int ok;
    if (m == NULL || c == NULL || l == NULL)
        return enif_make_int(env, 0);
    mn = enif_mutex_name(m);
    cn = enif_cond_name(c);
    ln = enif_rwlock_name(l);
    ok = (mn != NULL && strcmp(mn, "nif_res_mname") == 0 &&
          cn != NULL && strcmp(cn, "nif_res_cname") == 0 &&
          ln != NULL && strcmp(ln, "nif_res_lname") == 0);
    enif_mutex_destroy(m);
    enif_cond_destroy(c);
    enif_rwlock_destroy(l);
    return enif_make_int(env, ok ? 1 : 0);
}

/* ---- The widened resource-type surface (init_resource_type / open_resource_type_x) ------ */

/* The type opened with `enif_init_resource_type` (init members = 1: dtor only). Allocate a
   resource of it, ask for its size and read back the field that was written. Returns
   1 + 10 = 11 when both held; 0 when the type was not opened by the load callback. */
static ERL_NIF_TERM itype_sizeof(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    xres_t* r;
    int code = 0;
    if (itype == NULL)
        return enif_make_int(env, 0);
    r = (xres_t*) enif_alloc_resource(itype, sizeof(xres_t));
    if (r == NULL)
        return enif_make_int(env, -1);
    r->value = 7;
    if (enif_sizeof_resource(r) == sizeof(xres_t))
        code += 1;
    if (r->value == 7)
        code += 10;
    enif_release_resource(r);
    return enif_make_int(env, code);
}

/* The type opened with `enif_open_resource_type_x`: the same look, plus the `tried` flag it
   reports. Returns 1 + 10 + 100 = 111 when the flag answered ERL_NIF_RT_CREATE and the resource
   held. */
static ERL_NIF_TERM xtype_sizeof(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    xres_t* r;
    int code = 0;
    if (xtype == NULL)
        return enif_make_int(env, 0);
    if (xtype_tried == ERL_NIF_RT_CREATE)
        code += 1;
    r = (xres_t*) enif_alloc_resource(xtype, sizeof(xres_t));
    if (r == NULL)
        return enif_make_int(env, -1);
    r->value = 9;
    if (enif_sizeof_resource(r) == sizeof(xres_t))
        code += 10;
    if (r->value == 9)
        code += 100;
    enif_release_resource(r);
    return enif_make_int(env, code);
}

/* `enif_make_resource_binary` mints a binary term backed by the resource's own bytes; the term
   keeps the resource object alive after the native reference is released. Returns 1 + 10 = 11
   when the binary reads back as the three bytes `abc`. */
static ERL_NIF_TERM resource_binary_probe(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    xres_t* r;
    ERL_NIF_TERM term;
    ErlNifBinary b;
    int code = 0;
    if (xtype == NULL)
        return enif_make_int(env, 0);
    r = (xres_t*) enif_alloc_resource(xtype, sizeof(xres_t));
    if (r == NULL)
        return enif_make_int(env, -1);
    memcpy(r->buf, "abc", 3);
    r->value = 3;
    term = enif_make_resource_binary(env, r, r->buf, 3);
    enif_release_resource(r);
    if (enif_inspect_binary(env, term, &b)) {
        if (b.size == 3)
            code += 1;
        if (b.size == 3 && memcmp(b.data, "abc", 3) == 0)
            code += 10;
    }
    return enif_make_int(env, code);
}

/* ---- The widened ioq surface ------------------------------------------------------------ */

/* A queue filled through `enif_ioq_enqv` from an `enif_inspect_iovec` slice of a two-element
   list, with a one-byte `skip`: 4 of the 5 bytes are enqueued, the peek view has both remaining
   vectors and a partial dequeue of 2 bytes leaves 2. Returns 11111 when every part held. */
static ERL_NIF_TERM ioq_enqv_skip_probe(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    ErlNifIOQueue* q = enif_ioq_create(ERL_NIF_IOQ_NORMAL);
    ERL_NIF_TERM terms[2], list, tail = 0;
    ErlNifIOVec* iov = NULL;
    unsigned char* p;
    SysIOVec* v;
    size_t n, got = 0;
    int iovlen = -1, enq, deq, code = 0;
    if (q == NULL)
        return enif_make_int(env, 0);
    p = enif_make_new_binary(env, 3, &terms[0]);
    memcpy(p, "abc", 3);
    p = enif_make_new_binary(env, 2, &terms[1]);
    memcpy(p, "de", 2);
    list = enif_make_list_from_array(env, terms, 2);
    if (!enif_inspect_iovec(env, 5, list, &tail, &iov))
        return enif_make_int(env, -1);
    enq = enif_ioq_enqv(q, iov, 1);
    n = enif_ioq_size(q);
    v = enif_ioq_peek(q, &iovlen);
    if (enq == 1)
        code += 1;
    if (n == 4)
        code += 10;
    if (iovlen == 2)
        code += 100;
    if (iovlen == 2 && v != NULL && v[0].iov_len == 2 && v[0].iov_base != NULL &&
        ((const char*) v[0].iov_base)[0] == 'b')
        code += 1000;
    deq = enif_ioq_deq(q, 2, &got);
    if (deq == 1 && got == 2 && enif_ioq_size(q) == 2)
        code += 10000;
    enif_ioq_destroy(q);
    return enif_make_int(env, code);
}

/* `enif_ioq_peek_head` copies the queue's head chunk into a binary without dequeuing it: a
   3-byte head answers size 3 and the bytes `abc`, and the queue still holds all 5 bytes.
   Returns 1 + 10 + 100 + 1000 + 10000 = 11111 when every part held. */
static ERL_NIF_TERM ioq_peek_head_probe(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    ErlNifIOQueue* q = enif_ioq_create(ERL_NIF_IOQ_NORMAL);
    ErlNifBinary b1, b2, hb;
    ERL_NIF_TERM head = 0;
    size_t hsize = 0;
    int ok, code = 0;
    if (q == NULL)
        return enif_make_int(env, 0);
    if (!enif_alloc_binary(3, &b1) || !enif_alloc_binary(2, &b2))
        return enif_make_int(env, -1);
    memcpy(b1.data, "abc", 3);
    memcpy(b2.data, "de", 2);
    if (!enif_ioq_enq_binary(q, &b1, 0) || !enif_ioq_enq_binary(q, &b2, 0))
        return enif_make_int(env, -2);
    ok = enif_ioq_peek_head(env, q, &hsize, &head);
    if (ok == 1)
        code += 1;
    if (hsize == 3)
        code += 10;
    if (ok == 1 && enif_inspect_binary(env, head, &hb)) {
        if (hb.size == 3)
            code += 100;
        if (hb.size == 3 && memcmp(hb.data, "abc", 3) == 0)
            code += 1000;
    }
    if (enif_ioq_size(q) == 5)
        code += 10000;
    enif_ioq_destroy(q);
    return enif_make_int(env, code);
}

/* A partial dequeue (the byte count `erl_ioq_deq` takes, `erl_io_queue.c:347`): 3 of the 5
   queued bytes go, the out-parameter reports the 2 that remain, and the peek view is the single
   remaining vector. Returns 1 + 10 + 100 + 1000 = 1111 when every part held. */
static ERL_NIF_TERM ioq_deq_partial_probe(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    ErlNifIOQueue* q = enif_ioq_create(ERL_NIF_IOQ_NORMAL);
    ErlNifBinary b1, b2;
    SysIOVec* v;
    size_t before, got = 0;
    int iovlen = -1, deq, code = 0;
    if (q == NULL)
        return enif_make_int(env, 0);
    if (!enif_alloc_binary(3, &b1) || !enif_alloc_binary(2, &b2))
        return enif_make_int(env, -1);
    memcpy(b1.data, "abc", 3);
    memcpy(b2.data, "de", 2);
    if (!enif_ioq_enq_binary(q, &b1, 0) || !enif_ioq_enq_binary(q, &b2, 0))
        return enif_make_int(env, -2);
    before = enif_ioq_size(q);
    deq = enif_ioq_deq(q, 3, &got);
    v = enif_ioq_peek(q, &iovlen);
    if (before == 5)
        code += 1;
    if (deq == 1 && got == 2)
        code += 10;
    if (iovlen == 1)
        code += 100;
    if (iovlen == 1 && v != NULL && v[0].iov_len == 2 && v[0].iov_base != NULL &&
        ((const char*) v[0].iov_base)[0] == 'd')
        code += 1000;
    enif_ioq_destroy(q);
    return enif_make_int(env, code);
}

/* An empty queue: size 0, no peek vectors (and a reported vector count of 0), and a
   `peek_head` refusal. Returns 1 + 10 + 100 + 1000 = 1111. */
static ERL_NIF_TERM ioq_empty_probe(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    ErlNifIOQueue* q = enif_ioq_create(ERL_NIF_IOQ_NORMAL);
    ERL_NIF_TERM head = 0;
    SysIOVec* v;
    size_t n, hsize = 0;
    int iovlen = -1, ok, code = 0;
    if (q == NULL)
        return enif_make_int(env, 0);
    n = enif_ioq_size(q);
    v = enif_ioq_peek(q, &iovlen);
    ok = enif_ioq_peek_head(env, q, &hsize, &head);
    if (n == 0)
        code += 1;
    if (v == NULL)
        code += 10;
    if (iovlen == 0)
        code += 100;
    if (ok == 0)
        code += 1000;
    enif_ioq_destroy(q);
    return enif_make_int(env, code);
}

/* ---- The trylock halves of the sync family -------------------------------------------- */

static ErlNifMutex* trylock_mutex;

static void* trylock_worker(void* arg) {
    (void) arg;
    /* The parent holds the lock, so the trylock must fail with EBUSY. */
    return (void*)(intptr_t) enif_mutex_trylock(trylock_mutex);
}

/* A trylock from another thread while the lock is taken answers EBUSY, and the same trylock on
   the released lock succeeds. Returns 1 + 10 = 11. */
static ERL_NIF_TERM mutex_trylock_probe(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    ErlNifMutex* m = enif_mutex_create("nif_res_trylock");
    ErlNifTid tid = NULL;
    void* resp = NULL;
    int create_ret, join_ret, ret, code = 0;
    if (m == NULL)
        return enif_make_int(env, -1);
    trylock_mutex = m;
    enif_mutex_lock(m);
    create_ret = enif_thread_create("nif_res_trylock_worker", &tid, trylock_worker, NULL, NULL);
    if (create_ret != 0)
        tid = NULL;
    join_ret = tid ? enif_thread_join(tid, &resp) : create_ret;
    enif_mutex_unlock(m);
    if (join_ret == 0 && (intptr_t) resp == EBUSY)
        code += 1;
    ret = enif_mutex_trylock(m);
    if (ret == 0) {
        code += 10;
        enif_mutex_unlock(m);
    }
    enif_mutex_destroy(m);
    return enif_make_int(env, code);
}

static ErlNifRWLock* tryrlock_lock;

static void* tryrlock_worker(void* arg) {
    (void) arg;
    /* The parent holds the write lock, so a reader trylock must fail with EBUSY. */
    return (void*)(intptr_t) enif_rwlock_tryrlock(tryrlock_lock);
}

/* A reader trylock from another thread while a writer holds the lock answers EBUSY, and the same
   trylock after the write unlock succeeds. Returns 1 + 10 = 11. */
static ERL_NIF_TERM rwlock_tryrlock_probe(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    ErlNifRWLock* l = enif_rwlock_create("nif_res_tryrlock");
    ErlNifTid tid = NULL;
    void* resp = NULL;
    int create_ret, join_ret, ret, code = 0;
    if (l == NULL)
        return enif_make_int(env, -1);
    tryrlock_lock = l;
    enif_rwlock_rwlock(l);
    create_ret = enif_thread_create("nif_res_tryrlock_worker", &tid, tryrlock_worker, NULL, NULL);
    if (create_ret != 0)
        tid = NULL;
    join_ret = tid ? enif_thread_join(tid, &resp) : create_ret;
    enif_rwlock_rwunlock(l);
    if (join_ret == 0 && (intptr_t) resp == EBUSY)
        code += 1;
    ret = enif_rwlock_tryrlock(l);
    if (ret == 0) {
        code += 10;
        enif_rwlock_runlock(l);
    }
    enif_rwlock_destroy(l);
    return enif_make_int(env, code);
}

/* ---- The broadcast / thread-exit halves of the thread family --------------------------- */

static ErlNifMutex* bc_mutex;
static ErlNifCond* bc_cond;
static int bc_go;

static void* bc_worker(void* arg) {
    (void) arg;
    enif_mutex_lock(bc_mutex);
    while (!bc_go)
        enif_cond_wait(bc_cond, bc_mutex);
    enif_mutex_unlock(bc_mutex);
    return (void*)(intptr_t) 42;
}

/* Two waiters are woken by one `enif_cond_broadcast`. The mutex hand-off plus the `bc_go` flag
   make a lost wake-up impossible (a late starter observes the flag and returns), so both joins
   must succeed with 42: returns 1 + 2 = 3 when both did. */
static ERL_NIF_TERM cond_broadcast_probe(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    ErlNifTid t1 = NULL, t2 = NULL;
    void* r1 = NULL;
    void* r2 = NULL;
    int c1, c2, j1, j2, code = 0;
    bc_mutex = enif_mutex_create("nif_res_bc_mutex");
    bc_cond = enif_cond_create("nif_res_bc");
    bc_go = 0;
    if (bc_mutex == NULL || bc_cond == NULL)
        return enif_make_int(env, 0);
    c1 = enif_thread_create("nif_res_bc_w1", &t1, bc_worker, NULL, NULL);
    c2 = enif_thread_create("nif_res_bc_w2", &t2, bc_worker, NULL, NULL);
    if (c1 != 0)
        t1 = NULL;
    if (c2 != 0)
        t2 = NULL;
    enif_mutex_lock(bc_mutex);
    bc_go = 1;
    enif_cond_broadcast(bc_cond);
    enif_mutex_unlock(bc_mutex);
    j1 = t1 ? enif_thread_join(t1, &r1) : c1;
    j2 = t2 ? enif_thread_join(t2, &r2) : c2;
    if (j1 == 0 && (intptr_t) r1 == 42)
        code += 1;
    if (j2 == 0 && (intptr_t) r2 == 42)
        code += 2;
    enif_cond_destroy(bc_cond);
    enif_mutex_destroy(bc_mutex);
    return enif_make_int(env, code);
}

static void* thread_exit_worker(void* arg) {
    (void) arg;
    enif_thread_exit((void*)(intptr_t) 9);
    /* `enif_thread_exit` does not return; the compiler cannot know that. */
    return NULL;
}

/* `enif_thread_exit`'s response value reaches the joiner: returns 9. */
static ERL_NIF_TERM thread_exit_probe(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    ErlNifTid tid = NULL;
    void* resp = NULL;
    int create_ret, join_ret;
    create_ret = enif_thread_create("nif_res_exit", &tid, thread_exit_worker, NULL, NULL);
    if (create_ret != 0)
        tid = NULL;
    join_ret = tid ? enif_thread_join(tid, &resp) : create_ret;
    if (join_ret != 0)
        return enif_make_int(env, -1);
    return enif_make_int(env, (int)(intptr_t) resp);
}

/* ---- The environment half of the extra-environment family ------------------------------ */

/* `enif_getenv` on a 1-byte buffer: a present variable cannot fit, so it answers the
   "insufficient buffer" code (1), while an absent one answers the "not found" code (-1) —
   neither answer depends on the variable's contents. Returns 1 + 10 = 11. */
static ERL_NIF_TERM getenv_probe(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    char buf[8];
    size_t sz;
    int code = 0;
    sz = 1;
    if (enif_getenv("PATH", buf, &sz) == 1)
        code += 1;
    sz = sizeof(buf);
    if (enif_getenv("NIF_RES_UNSET_VAR_9F3C", buf, &sz) == -1)
        code += 10;
    return enif_make_int(env, code);
}

/* `enif_clear_env` on a live message env: the env stays usable (atoms built on it afterwards
   are atoms, and are the same global atoms as one built on the call env). Returns
   1 + 10 + 100 = 111. */
static ERL_NIF_TERM env_clear_probe(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc; (void) argv;
    ErlNifEnv* e = enif_alloc_env();
    ERL_NIF_TERM before, after, same;
    int code = 0;
    if (e == NULL)
        return enif_make_int(env, 0);
    before = enif_make_atom(e, "nif_res_before_clear");
    enif_clear_env(e);
    after = enif_make_atom(e, "nif_res_after_clear");
    same = enif_make_atom(env, "nif_res_after_clear");
    if (enif_is_atom(env, after))
        code += 1;
    if (enif_is_identical(after, same))
        code += 10;
    if (!enif_is_identical(before, after))
        code += 100;
    enif_free_env(e);
    return enif_make_int(env, code);
}

static ErlNifFunc nif_res_funcs[] = {
    {"res_size", 0, res_size, 0},
    {"ioq_size", 0, ioq_size, 0},
    {"sync_roundtrip", 0, sync_roundtrip, 0},
    {"res_roundtrip", 0, res_roundtrip, 0},
    {"self_alive", 0, self_alive, 0},
    {"alloc_env_probe", 0, alloc_env_probe, 0},
    {"alloc_env_simple", 0, alloc_env_simple, 0},
    {"res_read_after_release", 0, res_read_after_release, 0},
    {"cond_roundtrip", 0, cond_roundtrip, 0},
    {"rwlock_roundtrip", 0, rwlock_roundtrip, 0},
    {"tsd_roundtrip", 0, tsd_roundtrip, 0},
    {"thread_roundtrip", 0, thread_roundtrip, 0},
    {"names", 0, names, 0},
    {"itype_sizeof", 0, itype_sizeof, 0},
    {"xtype_sizeof", 0, xtype_sizeof, 0},
    {"resource_binary_probe", 0, resource_binary_probe, 0},
    {"ioq_enqv_skip_probe", 0, ioq_enqv_skip_probe, 0},
    {"ioq_peek_head_probe", 0, ioq_peek_head_probe, 0},
    {"ioq_deq_partial_probe", 0, ioq_deq_partial_probe, 0},
    {"ioq_empty_probe", 0, ioq_empty_probe, 0},
    {"mutex_trylock_probe", 0, mutex_trylock_probe, 0},
    {"rwlock_tryrlock_probe", 0, rwlock_tryrlock_probe, 0},
    {"cond_broadcast_probe", 0, cond_broadcast_probe, 0},
    {"thread_exit_probe", 0, thread_exit_probe, 0},
    {"getenv_probe", 0, getenv_probe, 0},
    {"env_clear_probe", 0, env_clear_probe, 0}
};

/* `ERL_NIF_INIT` seeds ErlNifEntry.name with the stringified module name (`nif_res`), so this
   library may only be loaded as `nif_res`. */
ERL_NIF_INIT(nif_res, nif_res_funcs, load, NULL, NULL, NULL)
"#;

/// The library every case loads.
const NIF_RES: NifFixture = NifFixture::CompiledLibrary {
    name: "nif_res",
    c_source: NIF_RES_C,
};

/// Build one module-run case over [`NIF_RES_MODULE_SOURCE`].
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
            source: NIF_RES_MODULE_SOURCE,
            module: "nif_res",
            function: "classify",
            arity: 1,
            args: selector,
            nif: Some(NIF_RES),
        }),
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// The A.3 resource / ioq / sync group. Each golden is the integer the **oracle** prints
/// (verified per case with `differential run <id>`).
pub(crate) fn group() -> Vec<Case> {
    vec![
        case(
            "nif_res/resource_type_sizeof",
            "a NIF resource type allocates a resource and enif_sizeof_resource reports its size",
            &[1],
            // `sizeof(res_t)` = one `int`.
            "4",
        ),
        case(
            "nif_res/ioq_enq_peek_deq",
            "a NIF IO queue takes two binaries, reports size 5, peeks one iovec and dequeues all 5",
            &[2],
            "5021",
        ),
        case(
            "nif_res/mutex_lock_unlock_roundtrip",
            "a NIF named mutex locks, unlocks and is destroyed, and enif_thread_self is non-zero",
            &[3],
            "2",
        ),
        case(
            "nif_res/resource_make_get_roundtrip",
            "enif_make_resource's term resolves back through enif_get_resource to the allocated resource (payload 42)",
            &[4],
            // The payload field the round trip wrote before minting the term.
            "42",
        ),
        case(
            "nif_res/self_process_is_alive",
            "enif_is_process_alive on the pid from enif_self is 1: the calling process is alive",
            &[5],
            "1",
        ),
        case(
            "nif_res/alloc_env_roundtrip",
            "enif_alloc_env builds an atom copied back through enif_make_copy and freed with enif_free_env",
            &[6],
            // The oracle's `1`: the copied term is the atom `ok`. `beam_rt` implements
            // `alloc_env`/`free_env` but not `make_copy`, so its probe reports the capability-gap
            // atom and the module raises `unsupported_enif_alloc_env` (exit 3) — never a mismatch.
            "1",
        ),
        case(
            "nif_res/alloc_env_free_roundtrip",
            "enif_alloc_env yields a live message env whose atom is usable, released with enif_free_env",
            &[7],
            // The oracle's `1`: the atom built on the message env is `ok`.
            "1",
        ),
        case(
            "nif_res/resource_term_keeps_object_alive_after_release",
            "after enif_release_resource the term still resolves through enif_get_resource (payload 42)",
            &[8],
            // The payload field the round trip wrote before minting the term.
            "42",
        ),
        case(
            "nif_res/cond_signal_wakes_waiter",
            "a worker blocked in enif_cond_wait is woken by enif_cond_signal (join 0, returns 42)",
            &[9],
            // The oracle's `2`: the join succeeded and the worker returned 42.
            "2",
        ),
        case(
            "nif_res/rwlock_read_excludes_write",
            "enif_rwlock_tryrwlock reports EBUSY while a reader holds the lock, then the write lock round-trips",
            &[10],
            // The oracle's `EBUSY` (16 on this host).
            "16",
        ),
        case(
            "nif_res/tsd_is_per_thread",
            "enif_tsd_* is per-thread: the child sees an unset slot and the parent's write survives",
            &[11],
            // The oracle's `1`: every part of the per-thread contract held.
            "1",
        ),
        case(
            "nif_res/thread_create_join_roundtrip",
            "enif_thread_create/enif_thread_join round-trip the entry point's value (42), self-tid agrees with itself",
            &[12],
            // The oracle's `1`: opts/thread/join/self/equal all held.
            "1",
        ),
        case(
            "nif_res/sync_handle_names",
            "enif_{mutex,cond,rwlock}_name report the names the handles were created with",
            &[13],
            // The oracle's `1`: all three NAMED handles reported their own name.
            "1",
        ),
        case(
            "nif_res/init_resource_type_alloc_sizeof",
            "enif_init_resource_type opens a dtor-only type whose resource allocates, reports its size and holds its field",
            &[14],
            // 1 (the size held) + 10 (the field held).
            "11",
        ),
        case(
            "nif_res/open_resource_type_x_tried_flag",
            "enif_open_resource_type_x opens a type, reports ERL_NIF_RT_CREATE as the tried flag and its resource holds",
            &[15],
            // 1 (the tried flag) + 10 (the size) + 100 (the field).
            "111",
        ),
        case(
            "nif_res/resource_binary_backed_by_resource",
            "enif_make_resource_binary mints a binary backed by the resource's own 3 bytes, alive after the release",
            &[16],
            // 1 (the size) + 10 (the bytes).
            "11",
        ),
        case(
            "nif_res/ioq_enqv_skip_peek_deq",
            "enif_ioq_enqv skips one byte of an enif_inspect_iovec slice: size 4, two peek vectors, a 2-byte deq leaves 2",
            &[17],
            // 1 (enq) + 10 (size) + 100 (two vectors) + 1000 (first byte 'b') + 10000 (partial deq).
            "11111",
        ),
        case(
            "nif_res/ioq_peek_head_first_chunk",
            "enif_ioq_peek_head copies the 3-byte head chunk into a binary without dequeuing (the queue still holds 5)",
            &[18],
            // 1 (ok) + 10 (head size) + 100 (binary size) + 1000 (bytes) + 10000 (size unchanged).
            "11111",
        ),
        case(
            "nif_res/ioq_deq_partial_leaves_remainder",
            "enif_ioq_deq of 3 bytes reports the 2 remaining and leaves one peek vector holding \"de\"",
            &[19],
            // 1 (size before) + 10 (deq + remaining) + 100 (one vector) + 1000 (first byte 'd').
            "1111",
        ),
        case(
            "nif_res/ioq_empty_queue_probe",
            "an empty queue reports size 0, no peek vectors with a vector count of 0, and refuses peek_head",
            &[20],
            // 1 (size) + 10 (NULL view) + 100 (count 0) + 1000 (peek_head refusal).
            "1111",
        ),
        case(
            "nif_res/mutex_trylock_reports_busy",
            "enif_mutex_trylock answers EBUSY from another thread while the mutex is held, then succeeds once released",
            &[21],
            // 1 (EBUSY) + 10 (the released trylock succeeded).
            "11",
        ),
        case(
            "nif_res/rwlock_tryrlock_busy_under_writer",
            "enif_rwlock_tryrlock answers EBUSY from another thread while a writer holds the lock, then succeeds",
            &[22],
            // 1 (EBUSY) + 10 (the released tryrlock succeeded).
            "11",
        ),
        case(
            "nif_res/cond_broadcast_wakes_both_waiters",
            "one enif_cond_broadcast wakes two threads blocked in enif_cond_wait, both joining with 42",
            &[23],
            // 1 (the first waiter) + 2 (the second).
            "3",
        ),
        case(
            "nif_res/thread_exit_value_reaches_joiner",
            "enif_thread_exit's response value (9) is what enif_thread_join reports",
            &[24],
            // The response value the exiting thread handed to the joiner.
            "9",
        ),
        case(
            "nif_res/getenv_reports_present_and_missing",
            "enif_getenv answers the insufficient-buffer code for a 1-byte buffer and -1 for an absent variable",
            &[25],
            // 1 (buffer too small, so PATH is present) + 10 (absent variable not found).
            "11",
        ),
        case(
            "nif_res/env_clear_env_stays_usable",
            "after enif_clear_env a message env still builds atoms, and they are the global ones the call env makes",
            &[26],
            // 1 (atom) + 10 (identical to the call env's atom) + 100 (distinct from the pre-clear one).
            "111",
        ),
    ]
}

#[cfg(test)]
mod tests {
    //! Invariants of the A.3 resource/ioq/sync group.

    use super::{group, CaseKind, Contract, NifFixture};
    use std::collections::HashSet;

    #[test]
    fn every_case_is_a_nif_module_run_over_the_same_compiled_library() {
        let mut selectors = HashSet::new();
        for case in group() {
            assert!(
                case.id.starts_with("nif_res/"),
                "case {} is outside the nif_res group",
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
            assert_eq!(run.module, "nif_res", "case {}", case.id);
            assert_eq!(run.function, "classify", "case {}", case.id);
            assert_eq!(run.arity, 1, "case {}", case.id);
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
        assert_eq!(selectors.len(), 26, "the group covers twenty-six distinct selectors");
    }

    #[test]
    fn the_c_library_references_the_extra_environment_probe() {
        // The extra-environment family is exercised on purpose: `beam_rt` *exports*
        // `enif_alloc_env`/`enif_free_env`/`enif_make_copy` (they resolve at `dlopen(RTLD_NOW)`). It
        // implements `alloc_env`/`free_env` (a live message env — `alloc_env_simple`) but leaves
        // `make_copy` at the trait default, so the copy probe reports the
        // `unsupported_enif_alloc_env` capability-gap atom and the module raises the documented
        // capability gap (exit 3) — never a mismatch, never a bogus load failure.
        for symbol in ["enif_alloc_env", "enif_free_env", "enif_make_copy"] {
            assert!(
                super::NIF_RES_C.contains(symbol),
                "the library must reference {symbol}"
            );
        }
        for probe in ["alloc_env_probe", "alloc_env_simple"] {
            assert!(
                super::NIF_RES_MODULE_SOURCE.contains(probe),
                "the module must export and call {probe}"
            );
        }
    }
}
