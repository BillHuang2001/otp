/*
 * tests/fixtures/a3_thread_env.c — a real C NIF (contract A.3) probing the
 * PROCESS-INDEPENDENT environment ("message env") of `enif_alloc_env()` /
 * `enif_free_env()` when it is created on a thread that the NIF itself
 * created, plus the `enif_send(NULL, &pid, msg_env, msg)` entry point that
 * consumes such an env.
 *
 * The question this fixture measures: in real ERTS an `ErlNifEnv` handed out by
 * `enif_alloc_env(void)` needs no live NIF call at all — it is a standalone
 * message env with its own heap and the ONE global atom table
 * (`../../../../erts/emulator/beam/erl_nif.c:669-680` and `setup_nif_env`,
 * `:636-666`: `msg_env->env.proc = &msg_env->phony_proc`, whose
 * `common.id == ERTS_INVALID_PID`), so the message-env idiom
 *
 *     env = enif_alloc_env(); term = enif_make_*(env, …);
 *     enif_send(NULL, &pid, env, term); enif_free_env(env);
 *
 * works verbatim from any thread a NIF created. A Rust host that reaches the
 * runtime through a thread-local stack of *live NIF call* environments cannot
 * answer `enif_alloc_env` there, so this fixture pins what the oracle really
 * answers on both sides of the seam.
 *
 * It is compiled (never committed as a `.so`) with the system `cc` against the
 * system `erl_nif.h` and driven by the sibling `a3_thread_env.erl`, which is
 * loaded by the system `erl` — the oracle. Every `enif_*` below is a real
 * exported symbol of the emulator, so the fixture needs no wrapper.
 *
 * Determinism: each worker is `enif_thread_join`ed from the call path before
 * the call-path NIF reads the worker's statics, so every reported value belongs
 * to a worker that has already exited (pthread_join is the barrier; the fields
 * are `volatile` so the stores cannot be elided). There is exactly one worker
 * in flight per probe, so the per-probe statics need no lock.
 *
 * Probes (the `.erl` driver asserts/prints every one of them):
 *
 *   probe_thread_send/0   worker thread: enif_alloc_env + build
 *                         {from_thread, 42, <<"abc">>} + enif_send(NULL, …) +
 *                         enif_free_env; call path returns
 *                         {EnvIsNull, SendCode, ThreadType, ThreadCreated}
 *   probe_main_env/0      the same idiom on the CALL PATH (the control, where
 *                         enif_alloc_env does work): build {from_call, 42} and
 *                         send it to itself; returns {EnvIsNull, SendCode}
 *   probe_worker_atoms/0  worker thread: enif_make_atom("worker_atom") on the
 *                         message env, then read it back with
 *                         enif_get_atom_length + enif_get_atom, and send the
 *                         atom itself so the receiver proves atom-table
 *                         identity; returns
 *                         {EnvIsNull, AtomLenOk, AtomTextOk, SendCode,
 *                          ThreadType}
 *   probe_null_env/0      a scheduler-thread send with `env == NULL` (the
 *                         worker's exact call, minus the thread) — isolates the
 *                         `env` argument from the thread question; returns
 *                         {EnvIsNull, SendCode, ThreadType}
 *   probe_thread_send_to/1  probe_thread_send/0, but the worker sends to the
 *                         PID PASSED IN (empty mailbox / parked receiver) and
 *                         the message is {from_thread_to_target, 42}; returns
 *                         {EnvIsNull, SendCode, ThreadType}. The driver parks a
 *                         process in `receive` and polls `process_info(P,
 *                         status)` until it answers `{status,waiting}` BEFORE
 *                         calling this, so "a send from a NIF-created thread
 *                         wakes a parked receiver" is a verified precondition,
 *                         not a race.
 */

#include <erl_nif.h>
#include <string.h>

/* A code no real `enif_send`/`enif_thread_type` can return (they answer 0/1 and
 * ERL_NIF_THR_*), meaning "this step never ran". */
#define PROBE_NOT_RUN (-1)

/* The name the worker interns on its own message env and reads back. */
static const char worker_atom_name[] = "worker_atom";
#define WORKER_ATOM_LEN (sizeof(worker_atom_name) - 1)

/* ---- probe_thread_send/0 ------------------------------------------------ */

static ErlNifPid thread_send_caller; /* the pid the worker sends to */

static struct {
    volatile int done;
    volatile int env_is_null;
    volatile int send_code;
    volatile int thread_type;
} ts;

static void *thread_send_worker(void *arg)
{
    ERL_NIF_TERM msg, bin, arr[3];
    unsigned char *bytes;
    ErlNifEnv *msg_env;

    (void)arg;

    /* Pessimistic until proven otherwise: a NULL env cannot be built on, so the
     * send is then recorded as "not run" rather than crashing. */
    ts.env_is_null = 1;
    ts.send_code = PROBE_NOT_RUN;
    ts.thread_type = enif_thread_type();

    msg_env = enif_alloc_env();
    if (msg_env != NULL) {
        ts.env_is_null = 0;

        /* <<"abc">> — a binary needs enif_make_new_binary: enif_make_string_len
         * builds a *list* of characters in this API generation. */
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

        /* `env == NULL`: the documented form from a thread the NIF created. */
        ts.send_code = enif_send(NULL, &thread_send_caller, msg_env, msg);
        enif_free_env(msg_env);
    }

    ts.done = 1;
    return NULL;
}

static ERL_NIF_TERM triple(ErlNifEnv *env, ERL_NIF_TERM a, ERL_NIF_TERM b,
                           ERL_NIF_TERM c)
{
    ERL_NIF_TERM arr[3];

    arr[0] = a;
    arr[1] = b;
    arr[2] = c;
    return enif_make_tuple_from_array(env, arr, 3);
}

static ERL_NIF_TERM pair(ErlNifEnv *env, ERL_NIF_TERM a, ERL_NIF_TERM b)
{
    ERL_NIF_TERM arr[2];

    arr[0] = a;
    arr[1] = b;
    return enif_make_tuple_from_array(env, arr, 2);
}

static ERL_NIF_TERM probe_thread_send(ErlNifEnv *env, int argc,
                                      const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM ret;
    ErlNifTid tid;
    int created;

    (void)argc;
    (void)argv;

    (void)enif_self(env, &thread_send_caller);

    ts.done = 0;
    ts.env_is_null = 1;
    ts.send_code = PROBE_NOT_RUN;
    ts.thread_type = PROBE_NOT_RUN;

    created = enif_thread_create("a3_thread_send", &tid, thread_send_worker,
                                 NULL, NULL);
    if (created == 0) {
        /* Join from the call path: the reported values are the worker's final
         * ones, and the message is in the mailbox before this NIF returns. */
        (void)enif_thread_join(tid, NULL);
        if (ts.done != 1)
            return enif_make_badarg(env);
    }

    ret = triple(env,
                 enif_make_int(env, ts.env_is_null),
                 enif_make_int(env, ts.send_code),
                 enif_make_int(env, ts.thread_type));
    return pair(env, ret, enif_make_int(env, created));
}

/* ---- probe_main_env/0 (the call-path control) --------------------------- */

static ERL_NIF_TERM probe_main_env(ErlNifEnv *env, int argc,
                                   const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM msg, arr[2];
    ErlNifPid self;
    ErlNifEnv *msg_env;
    int env_is_null, code;

    (void)argc;
    (void)argv;

    env_is_null = 1;
    code = PROBE_NOT_RUN;

    msg_env = enif_alloc_env();
    if (msg_env != NULL) {
        env_is_null = 0;
        arr[0] = enif_make_atom(msg_env, "from_call");
        arr[1] = enif_make_int(msg_env, 42);
        msg = enif_make_tuple_from_array(msg_env, arr, 2);

        (void)enif_self(env, &self);
        /* The call-path idiom passes the CALL env here (not NULL). */
        code = enif_send(env, &self, msg_env, msg);
        enif_free_env(msg_env);
    }

    return pair(env, enif_make_int(env, env_is_null), enif_make_int(env, code));
}

/* ---- probe_worker_atoms/0 ----------------------------------------------- */

static ErlNifPid worker_atoms_caller;

static struct {
    volatile int done;
    volatile int env_is_null;
    volatile int atom_len_ok;
    volatile int atom_text_ok;
    volatile int send_code;
    volatile int thread_type;
} wa;

static void *worker_atoms_worker(void *arg)
{
    ERL_NIF_TERM atom, msg, arr[2];
    unsigned len = 0;
    char buf[32];
    int get_ret = 0;
    ErlNifEnv *msg_env;

    (void)arg;

    wa.env_is_null = 1;
    wa.atom_len_ok = -1;
    wa.atom_text_ok = -1;
    wa.send_code = PROBE_NOT_RUN;
    wa.thread_type = enif_thread_type();

    msg_env = enif_alloc_env();
    if (msg_env != NULL) {
        wa.env_is_null = 0;

        /* A name this driver module does not necessarily know, interned on a
         * NIF-created thread: it must land in the ONE global atom table. */
        atom = enif_make_atom(msg_env, worker_atom_name);

        wa.atom_len_ok = enif_get_atom_length(msg_env, atom, &len,
                                              ERL_NIF_LATIN1);
        memset(buf, 0, sizeof(buf));
        get_ret = enif_get_atom(msg_env, atom, buf, (unsigned)sizeof(buf),
                                ERL_NIF_LATIN1);
        wa.atom_text_ok = (get_ret != 0
                           && len == (unsigned)WORKER_ATOM_LEN
                           && memcmp(buf, worker_atom_name,
                                     WORKER_ATOM_LEN) == 0);

        arr[0] = enif_make_atom(msg_env, "worker_atom_check");
        arr[1] = atom;
        msg = enif_make_tuple_from_array(msg_env, arr, 2);
        wa.send_code = enif_send(NULL, &worker_atoms_caller, msg_env, msg);
        enif_free_env(msg_env);
    }

    wa.done = 1;
    return NULL;
}

static ERL_NIF_TERM probe_worker_atoms(ErlNifEnv *env, int argc,
                                       const ERL_NIF_TERM argv[])
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
    wa.send_code = PROBE_NOT_RUN;
    wa.thread_type = PROBE_NOT_RUN;

    created = enif_thread_create("a3_thread_atoms", &tid, worker_atoms_worker,
                                 NULL, NULL);
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

/* ---- probe_null_env/0 --------------------------------------------------- */

/* The worker's `enif_send(NULL, …)` call, but made from the scheduler thread:
 * separates "the env argument is NULL" from "the calling thread is foreign". */
static ERL_NIF_TERM probe_null_env(ErlNifEnv *env, int argc,
                                   const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM msg, arr[2];
    ErlNifPid self;
    ErlNifEnv *msg_env;
    int env_is_null, code;

    (void)argc;
    (void)argv;

    env_is_null = 1;
    code = PROBE_NOT_RUN;

    msg_env = enif_alloc_env();
    if (msg_env != NULL) {
        env_is_null = 0;
        arr[0] = enif_make_atom(msg_env, "from_call_null");
        arr[1] = enif_make_int(msg_env, 42);
        msg = enif_make_tuple_from_array(msg_env, arr, 2);

        (void)enif_self(env, &self);
        code = enif_send(NULL, &self, msg_env, msg);
        enif_free_env(msg_env);
    }

    return triple(env,
                  enif_make_int(env, env_is_null),
                  enif_make_int(env, code),
                  enif_make_int(env, enif_thread_type()));
}

/* ---- probe_thread_send_to/1 -------------------------------------------- */

/* The same worker as probe_thread_send/0, but the receiver is the PID argument
 * (so the caller can be a process parked in `receive`) and the message is a
 * distinguishable one. */
static ErlNifPid to_target;

static struct {
    volatile int done;
    volatile int env_is_null;
    volatile int send_code;
    volatile int thread_type;
} tt;

static void *thread_send_to_worker(void *arg)
{
    ERL_NIF_TERM msg, arr[2];
    ErlNifEnv *msg_env;

    (void)arg;

    tt.env_is_null = 1;
    tt.send_code = PROBE_NOT_RUN;
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

static ERL_NIF_TERM probe_thread_send_to(ErlNifEnv *env, int argc,
                                        const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM ret;
    ErlNifTid tid;
    int created;

    (void)argc;

    if (!enif_get_local_pid(env, argv[0], &to_target))
        return enif_make_badarg(env);

    tt.done = 0;
    tt.env_is_null = 1;
    tt.send_code = PROBE_NOT_RUN;
    tt.thread_type = PROBE_NOT_RUN;

    created = enif_thread_create("a3_thread_to", &tid, thread_send_to_worker,
                                 NULL, NULL);
    if (created == 0) {
        (void)enif_thread_join(tid, NULL);
        if (tt.done != 1)
            return enif_make_badarg(env);
    }

    ret = triple(env,
                 enif_make_int(env, tt.env_is_null),
                 enif_make_int(env, tt.send_code),
                 enif_make_int(env, tt.thread_type));
    return pair(env, ret, enif_make_int(env, created));
}

static ErlNifFunc nif_funcs[] = {
    {"probe_thread_send", 0, probe_thread_send, 0},
    {"probe_main_env", 0, probe_main_env, 0},
    {"probe_worker_atoms", 0, probe_worker_atoms, 0},
    {"probe_null_env", 0, probe_null_env, 0},
    {"probe_thread_send_to", 1, probe_thread_send_to, 0},
};

ERL_NIF_INIT(a3_thread_env, nif_funcs, NULL, NULL, NULL, NULL)
