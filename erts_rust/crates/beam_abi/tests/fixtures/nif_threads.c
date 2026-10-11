/*
 * tests/fixtures/nif_threads.c — a real C NIF (contract A.3) exercising the
 * thread / mutex / condition / rwlock / TSD `enif_*` slots populated by
 * `src/nif/enif/threads.rs`.
 *
 * Compiled at test time with the system `cc` against the system `erl_nif.h` and
 * loaded through `src/nif/loader.rs`; every `enif_*` call below stays UNDEFINED
 * in the `.so` and is bound at `dlopen(RTLD_NOW)` against the loading process's
 * dynamic symbol table (`build.rs` adds `-Wl,--export-dynamic`).
 *
 * IMPORTANT: this fixture may reference only `enif_*` symbols the crate actually
 * defines (`Some` in the `ErlNifApi` table) — `dlopen(RTLD_NOW)` fails on any
 * unresolved import. It also must not touch `enif_tsd_key_create`-failure shapes
 * whose observable value differs from the oracle (see src/nif/enif/threads.rs,
 * "Documented deviations"). `enif_thread_name` IS safe to use here for a tid this
 * fixture created (named or unnamed); only a *foreign* tid (`enif_thread_self`) or
 * a tid read *after* the join diverges from the oracle (the C mints a
 * `drv_thr = 0` record / reads freed memory), so those two are deliberately not
 * exercised.
 *
 * Every NIF returns a tuple of small integers, so the SAME file can be loaded by
 * the system Erlang and its printed result compared with the Rust runtime's
 * (tests/nif_threads.rs drives both).
 *
 * What each NIF proves:
 *
 *   mutex_basic/0          create/lock/trylock(EBUSY)/unlock/trylock(0)/destroy
 *   cond_wait_signal/0     a spawned thread blocks in enif_cond_wait and is
 *                          woken by enif_cond_signal (returns 42)
 *   cond_wait_broadcast/0  two spawned waiters are both woken by
 *                          enif_cond_broadcast (each returns 7)
 *   rwlock_basic/0         rlock excludes tryrwlock, rwlock/rwunlock round-trip
 *   tsd_roundtrip/0        enif_tsd_* is per-thread: the child sees an unset
 *                          slot and its write is invisible to the parent
 *   thread_create_join/0   enif_thread_create + join returns the entry point's
 *                          value; enif_thread_self/enif_equal_tids;
 *                          enif_thread_opts_create/destroy
 *   thread_exit_value/0    enif_thread_exit's argument is the join result
 *   thread_type/0          enif_thread_type() (the one value that legitimately
 *                          differs from the oracle — see the module docs)
 *   names/0                enif_{mutex,cond,rwlock}_name on NAMED handles
 *   names_unnamed/0        enif_{mutex,cond,rwlock}_name on handles created with
 *                          a NULL name report "unknown"; a NULL handle reports NULL
 *   thread_names/0         enif_thread_name on a thread created with a NULL name
 *                          reports "unknown", on a named thread reports the name,
 *                          and on a NULL tid reports NULL
 */

#include <erl_nif.h>
#include <stdint.h>
#include <string.h>

/* ---------------------------------------------------------------------------
 * mutex
 * ------------------------------------------------------------------------- */

static ERL_NIF_TERM mutex_basic(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ErlNifMutex* m = enif_mutex_create("mb");
    ERL_NIF_TERM arr[3];
    int created, held, freed;

    (void)argc;
    (void)argv;

    created = (m != NULL);

    enif_mutex_lock(m);
    held = enif_mutex_trylock(m);   /* busy: this thread already holds it */
    enif_mutex_unlock(m);

    freed = enif_mutex_trylock(m);  /* free again */
    if (freed == 0)
        enif_mutex_unlock(m);

    enif_mutex_destroy(m);

    arr[0] = enif_make_int(env, created);
    arr[1] = enif_make_int(env, held);
    arr[2] = enif_make_int(env, freed);
    return enif_make_tuple_from_array(env, arr, 3);
}

/* ---------------------------------------------------------------------------
 * condition variable
 * ------------------------------------------------------------------------- */

static ErlNifMutex* cond_mutex;
static ErlNifCond* cond_var;
static int cond_go;

static void* cond_worker(void* arg)
{
    (void)arg;
    enif_mutex_lock(cond_mutex);
    while (!cond_go)
        enif_cond_wait(cond_var, cond_mutex);
    enif_mutex_unlock(cond_mutex);
    return (void*)(intptr_t)42;
}

static void* bcast_worker(void* arg)
{
    (void)arg;
    enif_mutex_lock(cond_mutex);
    while (!cond_go)
        enif_cond_wait(cond_var, cond_mutex);
    enif_mutex_unlock(cond_mutex);
    return (void*)(intptr_t)7;
}

static ERL_NIF_TERM cond_wait_signal(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ErlNifTid tid;
    void* resp = NULL;
    ERL_NIF_TERM arr[3];
    int created, create_ret, join_ret;

    (void)argc;
    (void)argv;

    cond_mutex = enif_mutex_create("cond_mutex");
    cond_var = enif_cond_create("cond_var");
    cond_go = 0;
    created = (cond_mutex != NULL && cond_var != NULL);

    create_ret = enif_thread_create("cond_worker", &tid, cond_worker, NULL, NULL);
    if (create_ret != 0)
        tid = NULL;

    /* Acquiring the mutex here cannot happen before the worker has released it
     * inside enif_cond_wait, so the signal below cannot be lost. */
    enif_mutex_lock(cond_mutex);
    cond_go = 1;
    enif_cond_signal(cond_var);
    enif_mutex_unlock(cond_mutex);

    join_ret = tid ? enif_thread_join(tid, &resp) : create_ret;

    enif_cond_destroy(cond_var);
    enif_mutex_destroy(cond_mutex);

    arr[0] = enif_make_int(env, created);
    arr[1] = enif_make_int(env, join_ret);
    arr[2] = enif_make_int(env, (intptr_t)resp == 42);
    return enif_make_tuple_from_array(env, arr, 3);
}

static ERL_NIF_TERM cond_wait_broadcast(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ErlNifTid tid1 = NULL;
    ErlNifTid tid2 = NULL;
    void* resp1 = NULL;
    void* resp2 = NULL;
    ERL_NIF_TERM arr[4];
    int created, c1, c2, j1, j2;

    (void)argc;
    (void)argv;

    cond_mutex = enif_mutex_create("bc_mutex");
    cond_var = enif_cond_create("bc_var");
    cond_go = 0;
    created = (cond_mutex != NULL && cond_var != NULL);

    c1 = enif_thread_create("bc1", &tid1, bcast_worker, NULL, NULL);
    if (c1 != 0)
        tid1 = NULL;
    c2 = enif_thread_create("bc2", &tid2, bcast_worker, NULL, NULL);
    if (c2 != 0)
        tid2 = NULL;

    /* As above: the mutex hand-off makes the broadcast unlosable; a worker that
     * starts late simply observes cond_go and returns without waiting. */
    enif_mutex_lock(cond_mutex);
    cond_go = 1;
    enif_cond_broadcast(cond_var);
    enif_mutex_unlock(cond_mutex);

    j1 = tid1 ? enif_thread_join(tid1, &resp1) : c1;
    j2 = tid2 ? enif_thread_join(tid2, &resp2) : c2;

    enif_cond_destroy(cond_var);
    enif_mutex_destroy(cond_mutex);

    arr[0] = enif_make_int(env, created);
    arr[1] = enif_make_int(env, j1);
    arr[2] = enif_make_int(env, j2);
    arr[3] = enif_make_int(env, (intptr_t)resp1 + (intptr_t)resp2);
    return enif_make_tuple_from_array(env, arr, 4);
}

/* ---------------------------------------------------------------------------
 * reader/writer lock
 * ------------------------------------------------------------------------- */

static ERL_NIF_TERM rwlock_basic(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ErlNifRWLock* l = enif_rwlock_create("rw");
    ERL_NIF_TERM arr[3];
    int created, tryw_while_read = -1, rw_round_trip = -1;

    (void)argc;
    (void)argv;

    created = (l != NULL);

    if (l != NULL) {
        enif_rwlock_rlock(l);
        tryw_while_read = enif_rwlock_tryrwlock(l); /* busy: a reader (us) holds it */
        enif_rwlock_runlock(l);

        enif_rwlock_rwlock(l);
        enif_rwlock_rwunlock(l);
        rw_round_trip = 0;

        enif_rwlock_destroy(l);
    }

    arr[0] = enif_make_int(env, created);
    arr[1] = enif_make_int(env, tryw_while_read);
    arr[2] = enif_make_int(env, rw_round_trip);
    return enif_make_tuple_from_array(env, arr, 3);
}

/* ---------------------------------------------------------------------------
 * thread-specific data
 * ------------------------------------------------------------------------- */

static ErlNifTSDKey tsd_key;

static void* tsd_worker(void* arg)
{
    (void)arg;
    /* A fresh thread's slot must be unset. */
    return enif_tsd_get(tsd_key);
}

static ERL_NIF_TERM tsd_roundtrip(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ErlNifTSDKey key = -1;
    ErlNifTid tid = NULL;
    void* resp = (void*)(intptr_t)0xdead; /* a sentinel the join must overwrite */
    ERL_NIF_TERM arr[5];
    int key_ret, create_ret = -1, join_ret = -1;

    (void)argc;
    (void)argv;

    key_ret = enif_tsd_key_create("tsd", &key);
    tsd_key = key;

    if (key_ret == 0) {
        enif_tsd_set(key, (void*)(intptr_t)0x11);
        create_ret = enif_thread_create("tsd_worker", &tid, tsd_worker, NULL, NULL);
        if (create_ret != 0)
            tid = NULL;
        join_ret = tid ? enif_thread_join(tid, &resp) : create_ret;
    }

    arr[0] = enif_make_int(env, key_ret);
    arr[1] = enif_make_int(env, create_ret);
    arr[2] = enif_make_int(env, resp == NULL); /* the child saw an unset slot */
    arr[3] = enif_make_int(env, enif_tsd_get(key) == (void*)(intptr_t)0x11);
    arr[4] = enif_make_int(env, join_ret);

    if (key_ret == 0)
        enif_tsd_key_destroy(key);
    return enif_make_tuple_from_array(env, arr, 5);
}

/* ---------------------------------------------------------------------------
 * threads
 * ------------------------------------------------------------------------- */

static void* answer_42(void* arg)
{
    (void)arg;
    return (void*)(intptr_t)42;
}

static void* exit_worker(void* arg)
{
    (void)arg;
    enif_thread_exit((void*)(intptr_t)99);
    return NULL; /* not reached */
}

static ERL_NIF_TERM thread_create_join(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ErlNifThreadOpts* opts;
    ErlNifTid tid = NULL;
    void* resp = NULL;
    ERL_NIF_TERM arr[6];
    int opts_ok, create_ret, join_ret, self_ok, equal;

    (void)argc;
    (void)argv;

    opts = enif_thread_opts_create("opts");
    opts_ok = (opts != NULL);

    create_ret = enif_thread_create("t42", &tid, answer_42, NULL, opts);
    if (create_ret != 0)
        tid = NULL;
    join_ret = tid ? enif_thread_join(tid, &resp) : create_ret;

    enif_thread_opts_destroy(opts);

    self_ok = (enif_thread_self() != NULL);
    equal = enif_equal_tids(enif_thread_self(), enif_thread_self());

    arr[0] = enif_make_int(env, opts_ok);
    arr[1] = enif_make_int(env, create_ret);
    arr[2] = enif_make_int(env, join_ret);
    arr[3] = enif_make_long(env, (long)(intptr_t)resp);
    arr[4] = enif_make_int(env, self_ok);
    arr[5] = enif_make_int(env, equal);
    return enif_make_tuple_from_array(env, arr, 6);
}

static ERL_NIF_TERM thread_exit_value(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ErlNifTid tid = NULL;
    void* resp = NULL;
    ERL_NIF_TERM arr[3];
    int create_ret, join_ret;

    (void)argc;
    (void)argv;

    create_ret = enif_thread_create("exit", &tid, exit_worker, NULL, NULL);
    if (create_ret != 0)
        tid = NULL;
    join_ret = tid ? enif_thread_join(tid, &resp) : create_ret;

    arr[0] = enif_make_int(env, create_ret);
    arr[1] = enif_make_int(env, join_ret);
    arr[2] = enif_make_long(env, (long)(intptr_t)resp);
    return enif_make_tuple_from_array(env, arr, 3);
}

static ERL_NIF_TERM thread_type(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    (void)argc;
    (void)argv;
    return enif_make_int(env, enif_thread_type());
}

/* ---------------------------------------------------------------------------
 * names (named handles only: the unnamed case is a documented deviation)
 * ------------------------------------------------------------------------- */

static ERL_NIF_TERM names(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ErlNifMutex* m = enif_mutex_create("mname");
    ErlNifCond* c = enif_cond_create("cname");
    ErlNifRWLock* l = enif_rwlock_create("lname");
    ERL_NIF_TERM arr[3];
    char* mn;
    char* cn;
    char* ln;

    (void)argc;
    (void)argv;

    mn = enif_mutex_name(m);
    cn = enif_cond_name(c);
    ln = enif_rwlock_name(l);

    arr[0] = enif_make_int(env, mn != NULL && strcmp(mn, "mname") == 0);
    arr[1] = enif_make_int(env, cn != NULL && strcmp(cn, "cname") == 0);
    arr[2] = enif_make_int(env, ln != NULL && strcmp(ln, "lname") == 0);

    enif_mutex_destroy(m);
    enif_cond_destroy(c);
    enif_rwlock_destroy(l);
    return enif_make_tuple_from_array(env, arr, 3);
}

/* ---------------------------------------------------------------------------
 * names, unnamed/null cases (the C stores its static `no_name` = "unknown" for a
 * handle created with a NULL name; a NULL handle answers NULL)
 * ------------------------------------------------------------------------- */

static ERL_NIF_TERM names_unnamed(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ErlNifMutex* m = enif_mutex_create(NULL);
    ErlNifCond* c = enif_cond_create(NULL);
    ErlNifRWLock* l = enif_rwlock_create(NULL);
    ERL_NIF_TERM arr[6];
    char* mn;
    char* cn;
    char* ln;

    (void)argc;
    (void)argv;

    mn = enif_mutex_name(m);
    cn = enif_cond_name(c);
    ln = enif_rwlock_name(l);

    arr[0] = enif_make_int(env, mn != NULL && strcmp(mn, "unknown") == 0);
    arr[1] = enif_make_int(env, cn != NULL && strcmp(cn, "unknown") == 0);
    arr[2] = enif_make_int(env, ln != NULL && strcmp(ln, "unknown") == 0);
    /* A null handle answers NULL (`handle ? handle->name : NULL`). */
    arr[3] = enif_make_int(env, enif_mutex_name(NULL) == NULL);
    arr[4] = enif_make_int(env, enif_cond_name(NULL) == NULL);
    arr[5] = enif_make_int(env, enif_rwlock_name(NULL) == NULL);

    enif_mutex_destroy(m);
    enif_cond_destroy(c);
    enif_rwlock_destroy(l);
    return enif_make_tuple_from_array(env, arr, 6);
}

/* ---------------------------------------------------------------------------
 * thread names
 * ------------------------------------------------------------------------- */

static void* null_worker(void* arg)
{
    (void)arg;
    return NULL;
}

static ERL_NIF_TERM thread_names(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ErlNifTid unnamed = NULL;
    ErlNifTid named = NULL;
    void* r1 = NULL;
    void* r2 = NULL;
    ERL_NIF_TERM arr[4];
    char* un;
    char* nn;
    int c1, c2;

    (void)argc;
    (void)argv;

    c1 = enif_thread_create(NULL, &unnamed, null_worker, NULL, NULL);
    if (c1 != 0)
        unnamed = NULL;
    c2 = enif_thread_create("named-thread", &named, null_worker, NULL, NULL);
    if (c2 != 0)
        named = NULL;

    /* Read the names while both threads are live (a read after the join would be a
     * use-after-free in the C). */
    un = enif_thread_name(unnamed);
    nn = enif_thread_name(named);

    arr[0] = enif_make_int(env, c1 == 0 && c2 == 0);
    arr[1] = enif_make_int(env, un != NULL && strcmp(un, "unknown") == 0);
    arr[2] = enif_make_int(env, nn != NULL && strcmp(nn, "named-thread") == 0);
    arr[3] = enif_make_int(env, enif_thread_name(NULL) == NULL);

    if (unnamed)
        enif_thread_join(unnamed, &r1);
    if (named)
        enif_thread_join(named, &r2);
    return enif_make_tuple_from_array(env, arr, 4);
}

static ErlNifFunc nif_funcs[] = {
    {"mutex_basic", 0, mutex_basic, 0},
    {"cond_wait_signal", 0, cond_wait_signal, 0},
    {"cond_wait_broadcast", 0, cond_wait_broadcast, 0},
    {"rwlock_basic", 0, rwlock_basic, 0},
    {"tsd_roundtrip", 0, tsd_roundtrip, 0},
    {"thread_create_join", 0, thread_create_join, 0},
    {"thread_exit_value", 0, thread_exit_value, 0},
    {"thread_type", 0, thread_type, 0},
    {"names", 0, names, 0},
    {"names_unnamed", 0, names_unnamed, 0},
    {"thread_names", 0, thread_names, 0},
};

ERL_NIF_INIT(test_nif_threads, nif_funcs, NULL, NULL, NULL, NULL)
