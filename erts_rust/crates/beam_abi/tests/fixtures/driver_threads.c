/*
 * A REAL driver shared object — the end-to-end fixture of the `beam_abi` **threads &
 * synchronisation** family of the driver C ABI (contract A.4): the `erl_drv_mutex_*`,
 * `erl_drv_cond_*`, `erl_drv_rwlock_*`, `erl_drv_tsd_*` and `erl_drv_thread_*` surface.
 *
 * It is compiled by the integration test `tests/driver_threads.rs` with the system `cc` against the
 * system `erl_driver.h` and loaded through `beam_abi::driver::loader::load`. Its `erl_drv_*` /
 * `driver_*` imports are deliberately left undefined by the linker: they are resolved
 * (`RTLD_NOW`) out of the *loading process*'s dynamic symbol table, which the test binary publishes
 * via `-Wl,--export-dynamic` (`build.rs`). A successful load is therefore the empirical proof that
 * the Rust implementations of this family are exported as dynamic symbols and bind a real C driver.
 *
 * `DRIVER_INIT(threads)` exports `driver_init` returning the `ErlDrvEntry` below, whose
 * `driver_name` is `"threads"`, so the only name this library may be loaded as is `threads`.
 *
 * Portability note: this fixture references ONLY `driver_*` / `erl_drv_*` names whose `ErlDrvApi`
 * slots this crate populates (`Some`). A reference to an unimplemented slot (e.g.
 * `erl_drv_cond_wait_timed`, `erl_drv_steal_main_thread`) would make `dlopen(RTLD_NOW)` fail on the
 * unresolved import. `driver_alloc`/`driver_free` allocate the private state.
 *
 * Because the fixture only calls the documented API, the *same* source is also loaded into a real
 * `erl` by the oracle half of `tests/driver_threads.rs` (`erlang:load_driver/2` + `open_port` +
 * `port_control/3`), where the identical assertions must hold and the identical values must come
 * back — that is the differential comparison.
 *
 * The `control` commands (each returns 1 when its assertions held, a negative "failure" marker
 * otherwise; ERTS turns a negative return into `badarg`, so a failure is never silent):
 *
 *   0   the documented no-op → 0
 *   1   mutex create/name/trylock/lock/unlock/destroy
 *   2   cond create/name/signal/broadcast/wait, driven as a real cross-thread handshake
 *   3   rwlock create/name/tryrlock/rlock/runlock/tryrwlock/rwlock/rwunlock/destroy
 *   4   tsd key create/get/set + per-thread isolation through a worker thread + key_destroy
 *   5   thread create/name/join returning a value, with thread_self + equal_tids checked from
 *       *inside* the worker (before the join, so no freed tid is ever touched)
 *   6   erl_drv_thread_exit delivers its response value to the joiner
 *   7   thread_opts create (default suggested_stack_size) / use for a thread / destroy
 *   8   THIS LAYER'S documented refusals for misuse (null handles / null out-slots / a null entry
 *       point). The C `erts_exit`s on every one of these (`erl_drv_thread.c:523,545,670,692,712`),
 *       so command 8 is exercised by the Rust side only and is deliberately NOT driven by the
 *       oracle run below; `A4_ORACLE_COMMANDS` in the test lists the oracle-safe commands.
 *   9   the private state `start` returned is live
 *   10  the four `*_name` accessors on handles created with a NULL name → "unknown"
 *   11  the four `*_name` accessors on named handles → the name
 *   12  the four `*_name` accessors on NULL handles → NULL
 *   99+ unknown command → -100
 *
 * Kernel-vs-header note: no assertion here depends on a constant whose value the Rust layer
 * redefines; `EBUSY`/`EINVAL` are used through the *system* header's `errno.h`, and the "lock is
 * held" cases are asserted as "a non-zero error code", which is the header's documented contract
 * (`erl_driver.h:461`: "returns 0 if successful, otherwise a non-zero value").
 */
#include <erl_driver.h>
#include <errno.h>
#include <string.h>

/* The marker values the worker threads hand back so a join can be checked. */
#define THREADS_JOIN_MAGIC ((void *) 0x5eed1234UL)
#define THREADS_TSD_MAGIC ((void *) 0x77aabbccUL)

/* Private per-port state. The port handle is kept so the fixture is a well-behaved driver (the
 * thread family itself takes no port). */
typedef struct {
    ErlDrvPort port;
    unsigned long marker;
} threads_state;

/* ---------------------------------------------------------------------------------------------
 * Command 2 — a real cross-thread condition-variable handshake
 * --------------------------------------------------------------------------------------------- */

typedef struct {
    ErlDrvMutex *mtx;
    ErlDrvCond *cnd;
    /* 0 = idle, 1 = the waiter is inside `erl_drv_cond_wait`, 2 = released. Always touched under
     * `mtx`, so the predicate can never be observed torn (the standard condvar discipline). */
    int phase;
} cond_handshake;

static void *cond_waiter(void *arg) {
    cond_handshake *hs = (cond_handshake *) arg;

    erl_drv_mutex_lock(hs->mtx);
    hs->phase = 1;
    erl_drv_cond_signal(hs->cnd);
    while (hs->phase != 2)
        erl_drv_cond_wait(hs->cnd, hs->mtx);
    erl_drv_mutex_unlock(hs->mtx);

    return THREADS_JOIN_MAGIC;
}

static int cmd_cond(void) {
    ErlDrvMutex *mtx;
    ErlDrvCond *cnd;
    cond_handshake hs;
    ErlDrvTid tid = NULL;
    void *resp = NULL;
    int ok;

    mtx = erl_drv_mutex_create("drv_c_cond_mtx");
    cnd = erl_drv_cond_create("drv_c_cond");
    if (!mtx || !cnd)
        return -1; /* out of memory; the C would fault on the null handle too */

    /* The debug name round-trips (`erl_drv_thread.c:283-287`). */
    ok = (strcmp(erl_drv_cond_name(cnd), "drv_c_cond") == 0);

    /* Signalling with no waiter is harmless (`erl_drv_thread.c:289-302`). */
    erl_drv_cond_signal(cnd);
    erl_drv_cond_broadcast(cnd);

    hs.mtx = mtx;
    hs.cnd = cnd;
    hs.phase = 0;

    if (erl_drv_thread_create("drv_c_cond_worker", &tid, cond_waiter, &hs, NULL) != 0)
        return -1;

    /* The signaller half: publish the release while holding the lock, then wake the waiter. */
    erl_drv_mutex_lock(mtx);
    while (hs.phase != 1)
        erl_drv_cond_wait(cnd, mtx);
    hs.phase = 2;
    erl_drv_cond_broadcast(cnd);
    erl_drv_mutex_unlock(mtx);

    ok = ok
         && (erl_drv_thread_join(tid, &resp) == 0)
         && (resp == THREADS_JOIN_MAGIC);

    erl_drv_cond_destroy(cnd);
    erl_drv_mutex_destroy(mtx);
    return ok ? 1 : -1;
}

/* ---------------------------------------------------------------------------------------------
 * Command 4 — thread-specific data, per thread
 * --------------------------------------------------------------------------------------------- */

typedef struct {
    ErlDrvTSDKey key;
    void *set_value;
    void *seen_before; /* what the worker saw before setting its own value */
    void *seen_after;  /* what the worker read back after setting it */
} tsd_probe;

static void *tsd_worker(void *arg) {
    tsd_probe *p = (tsd_probe *) arg;

    p->seen_before = erl_drv_tsd_get(p->key);
    erl_drv_tsd_set(p->key, p->set_value);
    p->seen_after = erl_drv_tsd_get(p->key);
    return p->seen_after;
}

static int cmd_tsd(void) {
    ErlDrvTSDKey key = -1;
    tsd_probe probe;
    ErlDrvTid tid = NULL;
    void *resp = NULL;
    int ok;

    if (erl_drv_tsd_key_create("drv_c_tsd", &key) != 0)
        return -1;
    if (key < 0)
        return -1;

    /* Unset for this thread, then set: the key addresses one `void *` slot per thread. */
    ok = (erl_drv_tsd_get(key) == NULL);
    erl_drv_tsd_set(key, THREADS_TSD_MAGIC);
    ok = ok && (erl_drv_tsd_get(key) == THREADS_TSD_MAGIC);

    probe.key = key;
    probe.set_value = (void *) 0x1234UL;
    probe.seen_before = (void *) 1; /* sentinel: the worker must overwrite it */
    probe.seen_after = NULL;

    ok = ok
         && (erl_drv_thread_create("drv_c_tsd_worker", &tid, tsd_worker, &probe, NULL) == 0)
         && (erl_drv_thread_join(tid, &resp) == 0)
         && (resp == probe.set_value)
         && (probe.seen_before == NULL)                       /* per-thread: unset in the worker */
         && (probe.seen_after == probe.set_value)
         && (erl_drv_tsd_get(key) == THREADS_TSD_MAGIC);      /* the caller's slot is untouched */

    erl_drv_tsd_key_destroy(key);
    return ok ? 1 : -1;
}

/* ---------------------------------------------------------------------------------------------
 * Command 5 — the thread lifecycle, with `thread_self`/`equal_tids` checked inside the worker
 * --------------------------------------------------------------------------------------------- */

typedef struct {
    ErlDrvMutex *mtx;
    ErlDrvCond *cnd;
    int phase;               /* 1 = the creator published the tid, 2 = the worker compared it */
    ErlDrvTid published;     /* what `erl_drv_thread_create` handed the creator */
    int equal;               /* what `erl_drv_equal_tids` answered inside the worker */
    void *resp;              /* what the worker returns */
} self_probe;

static void *self_worker(void *arg) {
    self_probe *p = (self_probe *) arg;

    /* Wait until the creator has published the tid it got: comparing it *before* the join is the
     * only contract-legal order — `erl_drv_thread_join` frees the creator's tid record
     * (`erl_drv_thread.c:721-723`), so reading it afterwards would be a use-after-free. */
    erl_drv_mutex_lock(p->mtx);
    while (p->phase != 1)
        erl_drv_cond_wait(p->cnd, p->mtx);
    p->equal = erl_drv_equal_tids(erl_drv_thread_self(), p->published);
    p->phase = 2;
    erl_drv_cond_broadcast(p->cnd);
    erl_drv_mutex_unlock(p->mtx);

    return p->resp;
}

static int cmd_threads(void) {
    self_probe probe;
    ErlDrvTid tid = NULL;
    void *resp = NULL;
    int ok;

    probe.mtx = erl_drv_mutex_create("drv_c_self_mtx");
    probe.cnd = erl_drv_cond_create("drv_c_self_cond");
    if (!probe.mtx || !probe.cnd)
        return -1;
    probe.phase = 0;
    probe.published = NULL;
    probe.equal = 0;
    probe.resp = THREADS_JOIN_MAGIC;

    /* A thread handle is stable for the calling thread and equal to itself; a null operand is not
     * compared (`erl_drv_thread.c:687-695` would abort on one). */
    ok = (erl_drv_equal_tids(erl_drv_thread_self(), erl_drv_thread_self()) == 1);

    ok = ok && (erl_drv_thread_create("drv_c_worker", &tid, self_worker, &probe, NULL) == 0);

    /* The debug name is readable as soon as the create returns (`erl_drv_thread.c:656-659`). */
    ok = ok
         && (tid != NULL)
         && (strcmp(erl_drv_thread_name(tid), "drv_c_worker") == 0)
         && (erl_drv_equal_tids(erl_drv_thread_self(), tid) == 0); /* a worker is another thread */

    /* Publish the tid, let the worker compare it, then join. */
    erl_drv_mutex_lock(probe.mtx);
    probe.published = tid;
    probe.phase = 1;
    erl_drv_cond_broadcast(probe.cnd);
    while (probe.phase != 2)
        erl_drv_cond_wait(probe.cnd, probe.mtx);
    erl_drv_mutex_unlock(probe.mtx);

    ok = ok
         && (probe.equal == 1)   /* the tid the worker reports for itself is the created tid */
         && (erl_drv_thread_join(tid, &resp) == 0)
         && (resp == THREADS_JOIN_MAGIC);

    erl_drv_cond_destroy(probe.cnd);
    erl_drv_mutex_destroy(probe.mtx);
    return ok ? 1 : -1;
}

/* ---------------------------------------------------------------------------------------------
 * Command 6 — `erl_drv_thread_exit` delivers its response to the joiner
 * --------------------------------------------------------------------------------------------- */

static void *exit_worker(void *arg) {
    erl_drv_thread_exit(arg);
    return NULL; /* not reached */
}

static int cmd_thread_exit(void) {
    ErlDrvTid tid = NULL;
    void *resp = NULL;

    if (erl_drv_thread_create("drv_c_exiting_worker", &tid, exit_worker, THREADS_JOIN_MAGIC,
                              NULL) != 0)
        return -1;
    return ((erl_drv_thread_join(tid, &resp) == 0) && (resp == THREADS_JOIN_MAGIC)) ? 1 : -1;
}

/* ---------------------------------------------------------------------------------------------
 * Command 7 — thread options
 * --------------------------------------------------------------------------------------------- */

static void *value_worker(void *arg) {
    return arg;
}

static int cmd_thread_opts(void) {
    ErlDrvThreadOpts *opts;
    ErlDrvTid tid = NULL;
    void *resp = NULL;
    int ok;

    opts = erl_drv_thread_opts_create("drv_c_opts");
    if (!opts)
        return -1;

    /* The C's default is `-1` = "use the platform's default stack" (`erl_drv_thread.c:591`). */
    ok = (opts->suggested_stack_size == -1);

    /* A small-but-legal explicit request, in kilowords (64 = 512 KiB on this target). */
    opts->suggested_stack_size = 64;
    ok = ok
         && (erl_drv_thread_create("drv_c_opts_worker", &tid, value_worker, THREADS_JOIN_MAGIC,
                                   opts) == 0)
         && (erl_drv_thread_join(tid, &resp) == 0)
         && (resp == THREADS_JOIN_MAGIC);

    erl_drv_thread_opts_destroy(opts);
    return ok ? 1 : -1;
}

/* ---------------------------------------------------------------------------------------------
 * Commands 1 and 3 — mutexes and read/write locks
 * --------------------------------------------------------------------------------------------- */

static int cmd_mutex(void) {
    ErlDrvMutex *mtx = erl_drv_mutex_create("drv_c_mutex");
    int ok;

    if (!mtx)
        return -1;

    ok = (strcmp(erl_drv_mutex_name(mtx), "drv_c_mutex") == 0)
         && (erl_drv_mutex_trylock(mtx) == 0)      /* acquired */
         && (erl_drv_mutex_trylock(mtx) != 0);     /* already held by this thread */
    erl_drv_mutex_unlock(mtx);

    erl_drv_mutex_lock(mtx);
    ok = ok && (erl_drv_mutex_trylock(mtx) != 0);
    erl_drv_mutex_unlock(mtx);

    erl_drv_mutex_destroy(mtx);
    return ok ? 1 : -1;
}

static int cmd_rwlock(void) {
    ErlDrvRWLock *lck = erl_drv_rwlock_create("drv_c_rwlock");
    int ok;

    if (!lck)
        return -1;

    ok = (strcmp(erl_drv_rwlock_name(lck), "drv_c_rwlock") == 0)
         && (erl_drv_rwlock_tryrlock(lck) == 0);
    erl_drv_rwlock_runlock(lck);

    erl_drv_rwlock_rlock(lck);
    erl_drv_rwlock_runlock(lck);

    erl_drv_rwlock_rwlock(lck);
    /* A held write lock refuses the read lock, and refuses a second writer. */
    ok = ok
         && (erl_drv_rwlock_tryrlock(lck) != 0)
         && (erl_drv_rwlock_tryrwlock(lck) != 0);
    erl_drv_rwlock_rwunlock(lck);

    ok = ok && (erl_drv_rwlock_tryrwlock(lck) == 0);
    erl_drv_rwlock_rwunlock(lck);

    erl_drv_rwlock_destroy(lck);
    return ok ? 1 : -1;
}

/* ---------------------------------------------------------------------------------------------
 * Command 8 — this layer's documented refusals (the C aborts on every one of them)
 * --------------------------------------------------------------------------------------------- */

static int cmd_refusals(void) {
    ErlDrvTid tid = NULL;
    void *resp = NULL;
    int ok;

    /* The `int`-returning entry points answer the C's failure code instead of aborting… */
    ok = (erl_drv_thread_join(NULL, &resp) == EINVAL)
         && (erl_drv_tsd_key_create("drv_c_bad", NULL) == EINVAL)
         && (erl_drv_thread_create("drv_c_bad", NULL, value_worker, NULL, NULL) == EINVAL)
         && (erl_drv_thread_create(NULL, &tid, NULL, NULL, NULL) == EINVAL)
         && (tid == NULL)                                   /* a refused create writes no tid */
         && (erl_drv_mutex_trylock(NULL) == EINVAL)
         && (erl_drv_rwlock_tryrlock(NULL) == EINVAL)
         && (erl_drv_rwlock_tryrwlock(NULL) == EINVAL)
         && (erl_drv_mutex_name(NULL) == NULL)
         && (erl_drv_cond_name(NULL) == NULL)
         && (erl_drv_rwlock_name(NULL) == NULL)
         && (erl_drv_thread_name(NULL) == NULL);

    /* …and the `void` entry points are inert for a null handle. */
    erl_drv_mutex_lock(NULL);
    erl_drv_mutex_unlock(NULL);
    erl_drv_mutex_destroy(NULL);
    erl_drv_cond_signal(NULL);
    erl_drv_cond_broadcast(NULL);
    erl_drv_cond_wait(NULL, NULL);
    erl_drv_cond_destroy(NULL);
    erl_drv_rwlock_rlock(NULL);
    erl_drv_rwlock_runlock(NULL);
    erl_drv_rwlock_rwlock(NULL);
    erl_drv_rwlock_rwunlock(NULL);
    erl_drv_rwlock_destroy(NULL);
    erl_drv_tsd_key_destroy(-1);
    erl_drv_tsd_set(-1, NULL);
    erl_drv_thread_opts_destroy(NULL);

    return ok ? 1 : -1;
}

/* ---------------------------------------------------------------------------------------------
 * Commands 10-12 — every `*_name` accessor in all three states
 *
 * Commands 1/2/3/5 already check a *named* handle's name in passing; these three make the whole
 * contract explicit for all four accessors at once:
 *
 *   10  a handle created with a NULL name → the C's static `"unknown"` (`no_name`,
 *       `erl_drv_thread.c:99,135`, stored in the handle at `:170`/`:264`/`:341`/`:639`);
 *   11  a named handle                   → the name it was created with;
 *   12  a NULL handle                    → NULL (`erl_drv_thread.c:198`/`:283`/`:365`/`:656`).
 *
 * `"unknown"` is spelled out here rather than taken from the emulator's private header (the macro
 * `no_name` is not part of `erl_driver.h`); the *same* literal is asserted by
 * `tests/driver_threads.rs` against the Rust implementation, and by the oracle run for the C one.
 * --------------------------------------------------------------------------------------------- */

#define THREADS_UNKNOWN_NAME "unknown"

/* `name != NULL && strcmp(name, want) == 0` — the accessors' documented contract. */
static int name_is(char *name, const char *want) {
    return (name != NULL) && (strcmp(name, want) == 0);
}

/* Command 10 — the four accessors on handles created with a NULL name. */
static int cmd_names_unnamed(void) {
    ErlDrvMutex *mtx = erl_drv_mutex_create(NULL);
    ErlDrvCond *cnd = erl_drv_cond_create(NULL);
    ErlDrvRWLock *lck = erl_drv_rwlock_create(NULL);
    ErlDrvTid tid = NULL;
    void *resp = NULL;
    int ok;

    if (!mtx || !cnd || !lck)
        return -1;
    /* A NULL name with no options: the C's "unnamed, platform default stack" case
       (`erl_drv_thread.c:613-621`). */
    if (erl_drv_thread_create(NULL, &tid, value_worker, THREADS_JOIN_MAGIC, NULL) != 0) {
        erl_drv_mutex_destroy(mtx);
        erl_drv_cond_destroy(cnd);
        erl_drv_rwlock_destroy(lck);
        return -1;
    }

    ok = name_is(erl_drv_mutex_name(mtx), THREADS_UNKNOWN_NAME)
         && name_is(erl_drv_cond_name(cnd), THREADS_UNKNOWN_NAME)
         && name_is(erl_drv_rwlock_name(lck), THREADS_UNKNOWN_NAME)
         && name_is(erl_drv_thread_name(tid), THREADS_UNKNOWN_NAME);

    /* The join retires the tid (the C frees its record), so nothing is read from it afterwards. */
    ok = ok
         && (erl_drv_thread_join(tid, &resp) == 0)
         && (resp == THREADS_JOIN_MAGIC);

    erl_drv_mutex_destroy(mtx);
    erl_drv_cond_destroy(cnd);
    erl_drv_rwlock_destroy(lck);
    return ok ? 1 : -1;
}

/* Command 11 — the four accessors on named handles. */
static int cmd_names_named(void) {
    ErlDrvMutex *mtx = erl_drv_mutex_create("drv_c_names_mtx");
    ErlDrvCond *cnd = erl_drv_cond_create("drv_c_names_cnd");
    ErlDrvRWLock *lck = erl_drv_rwlock_create("drv_c_names_lck");
    ErlDrvTid tid = NULL;
    void *resp = NULL;
    int ok;

    if (!mtx || !cnd || !lck)
        return -1;
    if (erl_drv_thread_create("drv_c_names_tid", &tid, value_worker, THREADS_JOIN_MAGIC, NULL) != 0) {
        erl_drv_mutex_destroy(mtx);
        erl_drv_cond_destroy(cnd);
        erl_drv_rwlock_destroy(lck);
        return -1;
    }

    ok = name_is(erl_drv_mutex_name(mtx), "drv_c_names_mtx")
         && name_is(erl_drv_cond_name(cnd), "drv_c_names_cnd")
         && name_is(erl_drv_rwlock_name(lck), "drv_c_names_lck")
         && name_is(erl_drv_thread_name(tid), "drv_c_names_tid");

    ok = ok
         && (erl_drv_thread_join(tid, &resp) == 0)
         && (resp == THREADS_JOIN_MAGIC);

    erl_drv_mutex_destroy(mtx);
    erl_drv_cond_destroy(cnd);
    erl_drv_rwlock_destroy(lck);
    return ok ? 1 : -1;
}

/* Command 12 — the four accessors on a NULL handle: `NULL` is reserved for exactly this case. */
static int cmd_names_null(void) {
    int ok = (erl_drv_mutex_name(NULL) == NULL)
             && (erl_drv_cond_name(NULL) == NULL)
             && (erl_drv_rwlock_name(NULL) == NULL)
             && (erl_drv_thread_name(NULL) == NULL);
    return ok ? 1 : -1;
}

/* ---------------------------------------------------------------------------------------------
 * The driver entry points
 * --------------------------------------------------------------------------------------------- */

static ErlDrvData threads_start(ErlDrvPort port, char *command) {
    threads_state *st;

    (void) command;
    st = (threads_state *) driver_alloc(sizeof(threads_state));
    if (st == NULL)
        return (ErlDrvData) -1;
    st->port = port;
    st->marker = 0x7e44UL;
    return (ErlDrvData) st;
}

static void threads_stop(ErlDrvData drv_data) {
    if (drv_data != NULL)
        driver_free(drv_data);
}

static ErlDrvSSizeT threads_control(ErlDrvData drv_data, unsigned int command,
                                    char *buf, ErlDrvSizeT len,
                                    char **rbuf, ErlDrvSizeT rlen) {
    threads_state *st = (threads_state *) drv_data;
    (void) buf;
    (void) len;
    (void) rbuf;
    (void) rlen;

    switch (command) {
    case 0:
        /* The documented no-op: 0 bytes of reply. */
        return 0;

    case 1:
        return (ErlDrvSSizeT) cmd_mutex();

    case 2:
        return (ErlDrvSSizeT) cmd_cond();

    case 3:
        return (ErlDrvSSizeT) cmd_rwlock();

    case 4:
        return (ErlDrvSSizeT) cmd_tsd();

    case 5:
        return (ErlDrvSSizeT) cmd_threads();

    case 6:
        return (ErlDrvSSizeT) cmd_thread_exit();

    case 7:
        return (ErlDrvSSizeT) cmd_thread_opts();

    case 8:
        return (ErlDrvSSizeT) cmd_refusals();

    case 9:
        /* The private state is what `start` produced (a real `ErlDrvData`, never a sentinel). */
        return (ErlDrvSSizeT) ((st != NULL) && (st->marker == 0x7e44UL));

    case 10:
        /* The four `*_name` accessors on handles created with a NULL name. */
        return (ErlDrvSSizeT) cmd_names_unnamed();

    case 11:
        /* …on named handles. */
        return (ErlDrvSSizeT) cmd_names_named();

    case 12:
        /* …and on NULL handles. */
        return (ErlDrvSSizeT) cmd_names_null();

    default:
        /* An unknown command is visible as a distinct negative marker (→ `badarg`). */
        return -100;
    }
}

static ErlDrvEntry threads_driver_entry = {
    .init = NULL,
    .start = threads_start,
    .stop = threads_stop,
    .output = NULL,
    .ready_input = NULL,
    .ready_output = NULL,
    .driver_name = "threads",
    .finish = NULL,
    .handle = NULL,
    .control = threads_control,
    .timeout = NULL,
    .outputv = NULL,
    .ready_async = NULL,
    .flush = NULL,
    .call = NULL,
    .unused_event_callback = NULL,
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
    .driver_flags = 0,
    .handle2 = NULL,
    .process_exit = NULL,
    .stop_select = NULL,
    .emergency_close = NULL,
};

DRIVER_INIT(threads) {
    return &threads_driver_entry;
}
