/*
 * tests/fixtures/a3_resource_stop.c — a real C NIF (workspace contract A.3) that **measures what
 * the oracle's NIF resource `stop` callback and the `enif_select` STOP verdicts actually do**.
 *
 * This is an investigation artifact, not a differential case: the Erlang driver
 * (`tests/fixtures/a3_resource_stop.erl`) is run against the live system `erl` and the printed
 * transcript is the deliverable. `beam_abi` never invokes a resource type's `stop` callback, so
 * `ERL_NIF_SELECT_STOP_CALLED` is currently answered without the callback running and
 * `ERL_NIF_SELECT_STOP_SCHEDULED` is unobservable; the numbers below are what a faithful Rust
 * implementation has to reproduce.
 *
 * Ground truth read (never modified): `erts/emulator/sys/common/erl_check_io.c`
 *   :1278-1285   the STOP fast track — an fd with no event state answers
 *                `ERL_NIF_SELECT_STOP_CALLED` and calls the stop callback directly
 *                (`call_stop = CALL_STOP; ret = ERL_NIF_SELECT_STOP_CALLED; goto done_unknown;`)
 *   :1318-1337   a STOP that lands on an fd whose state is already `ERTS_EV_TYPE_STOP_NIF`
 *                (a stop is pending) steers through `steal_pending_stop_nif` and answers
 *                `ERL_NIF_SELECT_STOP_SCHEDULED`
 *   :1485-1517   the "off" arm: `!wake_poller` → `CALL_STOP_AND_RELEASE` + `|= STOP_CALLED`;
 *                `wake_poller` → `ERTS_EV_TYPE_STOP_NIF` + `|= STOP_SCHEDULED` after
 *                `enif_keep_resource`
 *   :1528-1535   the synchronous call: `erts_resource_stop(resource, fd, 1)` (is_direct_call = 1),
 *                then `enif_release_resource` for the `CALL_STOP_AND_RELEASE` arm
 *   :2241        the poller-thread call: `erts_resource_stop(resource, fd, 0)` (is_direct_call
 *                = 0) + `enif_release_resource` for a deferred (`STOP_NIF`) stop
 * `erts/emulator/beam/erl_nif.c:3032-3040` — `erts_resource_stop` runs the callback in a
 * **nooproc** message env: `pre_nif_noproc(&msg_env, type->owner, NULL); type->fn.stop(...)`.
 * `erts/emulator/beam/erl_drv_nif.h:54-64` — the flag values (`ERL_NIF_SELECT_STOP` = 1<<2,
 * `ERL_NIF_SELECT_CANCEL` = 1<<3) and `erts/emulator/beam/erl_nif.h:178-183` — the return bits
 * (`STOP_CALLED` = 1<<0, `STOP_SCHEDULED` = 1<<1, `READ_CANCELLED` = 1<<4).
 *
 * The `stop` callback records into plain C statics (call count, `last_event`, `last_is_direct_call`,
 * the payload tag, and `enif_thread_type()` of the calling thread — the one observation that tells
 * a synchronous scheduler-side call from a poller-thread one), and `probe_report/0` reads them back.
 * The callbacks reference only libc + `enif_*`; nothing from ERTS internals, so the fixture still
 * builds against a stock `erl_nif.h`.
 *
 * Every probe returns a flat tuple; the Erlang driver prints it. No probe raises on a verdict — the
 * verdict IS the measurement.
 *
 * Probe index (each returns one flat tuple; the tuple shapes are documented on each function):
 *   probe_reset/0                 zero the recorded statics, answer `ok`
 *   probe_report/0                the statics as they stand
 *   probe_stop_unknown_fd/0       STOP on a pipe fd nothing ever armed        (fast track)
 *   probe_arm_then_stop/0         arm READ then STOP on the same pipe fd      (`!wake_poller` arm)
 *   probe_double_stop/0           arm READ, STOP, STOP                        (repeated STOP)
 *   probe_cancel_then_stop/0      arm READ, CANCEL, STOP
 *   probe_stop_sleep_ms/1         arm READ, STOP, `nanosleep` MS in the NIF, then report
 *   probe_arm_then_stop_fallback/0  the same on an fd the kernel poll refuses (`wake_poller` arm)
 *   probe_stop_fallback_wait_ms/1   ... with a bounded wait, reporting the deferred latency
 *   probe_read_on_pending_stop/0    a second request on an fd whose stop is still pending
 *   probe_stop_nostop_type/0        STOP on a type opened with NO stop callback (crashes the VM)
 */
#include <erl_nif.h>
#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

/* ---------------------------------------------------------------------------------------------
 * What the resource type's `stop` callback observed (reset by probe_reset/0)
 * ------------------------------------------------------------------------------------------- */
static unsigned long stop_calls;   /* how many times stop ran */
static int last_is_direct;         /* the callback's `is_direct_call` (-1 = never ran) */
static long last_event;            /* the callback's `ErlNifEvent` (an fd; -1 = never ran) */
static int last_tag;               /* the payload tag of the object that was stopped */
static int last_thread_type;       /* enif_thread_type() inside the callback (-1 = never ran) */
static unsigned long dtor_calls;   /* how many times the destructor ran */
static int dtor_last_tag;          /* the payload tag the last destructor saw */
static long long last_stop_ns;     /* CLOCK_MONOTONIC ns when stop last ran (-1 = never ran) */

/* ---------------------------------------------------------------------------------------------
 * The two resource types: `a3_stop` carries a stop callback, `a3_nostop` carries none (the C
 * `ASSERT(resource->type->fn.stop)` at erl_check_io.c:1279 is a no-op in a release build).
 * ------------------------------------------------------------------------------------------- */
static ErlNifResourceType *res_stop;
static ErlNifResourceType *res_nostop;

typedef struct {
    int tag; /* identifies which probe's object a late (poller-thread) stop belongs to */
} probe_obj;

/* CLOCK_MONOTONIC in nanoseconds — the only clock this fixture needs (no ERTS internals). */
static long long now_ns(void)
{
    struct timespec ts;

    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0)
        return -1;
    return (long long)ts.tv_sec * 1000000000LL + (long long)ts.tv_nsec;
}

static void stop_cb(ErlNifEnv *env, void *obj, ErlNifEvent event, int is_direct_call)
{
    (void)env;
    stop_calls++;
    last_stop_ns = now_ns();
    last_is_direct = is_direct_call;
    last_event = (long)event;
    last_tag = ((probe_obj *)obj)->tag;
    last_thread_type = enif_thread_type();
}

static void dtor_cb(ErlNifEnv *env, void *obj)
{
    (void)env;
    dtor_calls++;
    dtor_last_tag = ((probe_obj *)obj)->tag;
}

static int nif_load(ErlNifEnv *env, void **priv_data, ERL_NIF_TERM load_info)
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

    res_stop = enif_init_resource_type(env, "a3_stop", &init,
                                       (ErlNifResourceFlags)(ERL_NIF_RT_CREATE
                                                             | ERL_NIF_RT_TAKEOVER),
                                       NULL);
    init.stop = NULL; /* the second type has NO stop callback */
    res_nostop = enif_init_resource_type(env, "a3_nostop", &init,
                                         (ErlNifResourceFlags)(ERL_NIF_RT_CREATE
                                                               | ERL_NIF_RT_TAKEOVER),
                                         NULL);
    if (res_stop == NULL || res_nostop == NULL)
        return 1;
    return 0;
}

/* ---------------------------------------------------------------------------------------------
 * Small helpers
 * ------------------------------------------------------------------------------------------- */
static probe_obj *alloc_obj(ErlNifResourceType *type, int tag)
{
    probe_obj *obj = (probe_obj *)enif_alloc_resource(type, sizeof(probe_obj));

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

static ERL_NIF_TERM pipe_error(ErlNifEnv *env, int err)
{
    ERL_NIF_TERM a[2];

    a[0] = enif_make_atom(env, "pipe_failed");
    a[1] = enif_make_int(env, err);
    return enif_make_tuple_from_array(env, a, 2);
}

static ERL_NIF_TERM alloc_error(ErlNifEnv *env)
{
    return enif_make_atom(env, "alloc_resource_failed");
}

/* The event state is keyed by FILE DESCRIPTOR, and a deferred stop (`ERTS_EV_TYPE_STOP_NIF`) keeps
 * that state alive until the poller thread services it. Two probes that happened to get the same
 * descriptor number would therefore steal each other's pending stop — `steal_pending_stop_nif`
 * (erl_check_io.c:1318-1337) answers the NEXT `enif_select` on that fd with STOP_SCHEDULED, even
 * when the new request is a READ. Every fallback probe below therefore moves its fd to its own
 * high, probe-private number so the probes stay independent. */
static int move_fd(int fd, int target)
{
    if (dup2(fd, target) < 0) {
        int err = errno;

        close(fd);
        errno = err;
        return -1;
    }
    close(fd);
    return target;
}

/* Bounded wait for a deferred stop to land: poll `stop_calls` in 100 us steps up to `ms`. */
static void wait_for_stop(unsigned long before, int ms)
{
    long long deadline = now_ns() + (long long)ms * 1000000LL;

    while (stop_calls == before) {
        struct timespec ts;

        if (now_ns() >= deadline)
            break;
        ts.tv_sec = 0;
        ts.tv_nsec = 100000L;
        while (nanosleep(&ts, &ts) != 0 && errno == EINTR) {
            /* resume the remaining time */
        }
    }
}

/* ---------------------------------------------------------------------------------------------
 * probe_reset/0 — zero every recorded static between probes (answers `ok`)
 * ------------------------------------------------------------------------------------------- */
static ERL_NIF_TERM probe_reset(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
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
    last_stop_ns = -1;
    return enif_make_atom(env, "ok");
}

/* ---------------------------------------------------------------------------------------------
 * probe_report/0 — what the statics say right now:
 *   {StopCalls, LastIsDirect, LastEvent, LastTag, LastThreadType, DtorCalls, DtorLastTag}
 * ------------------------------------------------------------------------------------------- */
static ERL_NIF_TERM probe_report(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
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

/* ---------------------------------------------------------------------------------------------
 * probe_stop_unknown_fd/0 — the C's STOP fast track (erl_check_io.c:1278-1285).
 * A pipe fd that no `enif_select` has ever armed has no event state, so the STOP is answered
 * without touching the poller and the callback runs inline (is_direct_call = 1).
 *   {Code, StopCalls, LastIsDirect, LastEvent, LastThreadType}
 * ------------------------------------------------------------------------------------------- */
static ERL_NIF_TERM probe_stop_unknown_fd(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM a[5];
    ERL_NIF_TERM ref = enif_make_ref(env);
    probe_obj *obj;
    int fds[2];
    int code;

    (void)argc;
    (void)argv;

    if (make_pipe(fds) != 0)
        return pipe_error(env, errno);
    obj = alloc_obj(res_stop, 1);
    if (obj == NULL) {
        close(fds[0]);
        close(fds[1]);
        return alloc_error(env);
    }
    code = enif_select(env, fds[0], ERL_NIF_SELECT_STOP, obj, NULL, ref);

    /* The callback (if it ran) has already updated the statics at this point. */
    a[0] = enif_make_int(env, code);
    a[1] = enif_make_ulong(env, stop_calls);
    a[2] = enif_make_int(env, last_is_direct);
    a[3] = enif_make_long(env, last_event);
    a[4] = enif_make_int(env, last_thread_type);

    close(fds[0]);
    close(fds[1]);
    enif_release_resource(obj); /* the fast track is CALL_STOP — no release of ours */
    return enif_make_tuple_from_array(env, a, 5);
}

/* ---------------------------------------------------------------------------------------------
 * probe_arm_then_stop/0 — the interesting case: the fd IS armed, so the STOP goes through the
 * "off" arm (erl_check_io.c:1485-1535). `!wake_poller` → CALL_STOP_AND_RELEASE + STOP_CALLED
 * (synchronous, is_direct_call = 1, the C releases the select's own reference); `wake_poller` →
 * ERTS_EV_TYPE_STOP_NIF + STOP_SCHEDULED (the stop runs later from the poller thread with
 * is_direct_call = 0).
 *   {ArmCode, StopCode, StopCalls, LastIsDirect, LastThreadType, LastTag, Fd}
 * (`Fd` is what the STOP was issued on, so the report proves `LastEvent` is that same descriptor.)
 * ------------------------------------------------------------------------------------------- */
static ERL_NIF_TERM probe_arm_then_stop(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM a[7];
    ERL_NIF_TERM ref = enif_make_ref(env);
    probe_obj *obj;
    int fds[2];
    int arm_code;
    int stop_code;

    (void)argc;
    (void)argv;

    if (make_pipe(fds) != 0)
        return pipe_error(env, errno);
    obj = alloc_obj(res_stop, 2);
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
    /* Balanced either way: CALL_STOP_AND_RELEASE already dropped the select's reference, a
     * deferred STOP_NIF keeps one until the poller releases it. */
    enif_release_resource(obj);
    return enif_make_tuple_from_array(env, a, 7);
}

/* ---------------------------------------------------------------------------------------------
 * probe_double_stop/0 — arm READ, STOP twice. The second STOP either hits the pending-stop arm
 * (erl_check_io.c:1318-1337, `steal_pending_stop_nif` → STOP_SCHEDULED) or, when the first STOP
 * already completed and left the fd with no event state, takes the fast track again — in which
 * case the callback runs a SECOND time.
 *   {ArmCode, StopCode1, StopCode2, StopCalls, LastIsDirect, LastThreadType, LastTag}
 * ------------------------------------------------------------------------------------------- */
static ERL_NIF_TERM probe_double_stop(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM a[7];
    ERL_NIF_TERM ref = enif_make_ref(env);
    probe_obj *obj;
    int fds[2];
    int arm_code;
    int first_code;
    int second_code;

    (void)argc;
    (void)argv;

    if (make_pipe(fds) != 0)
        return pipe_error(env, errno);
    obj = alloc_obj(res_stop, 3);
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

/* ---------------------------------------------------------------------------------------------
 * probe_cancel_then_stop/0 — arm READ, CANCEL the read, then STOP. The CANCEL leaves the event
 * state in ERTS_EV_TYPE_NIF with no events, so the STOP still has its own reference to release.
 *   {ArmCode, CancelCode, StopCode, StopCalls, LastIsDirect, LastThreadType, LastTag}
 * ------------------------------------------------------------------------------------------- */
static ERL_NIF_TERM probe_cancel_then_stop(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM a[7];
    ERL_NIF_TERM ref = enif_make_ref(env);
    probe_obj *obj;
    int fds[2];
    int arm_code;
    int cancel_code;
    int stop_code;

    (void)argc;
    (void)argv;

    if (make_pipe(fds) != 0)
        return pipe_error(env, errno);
    obj = alloc_obj(res_stop, 4);
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

/* ---------------------------------------------------------------------------------------------
 * probe_stop_sleep_ms/1 — arm READ, STOP, then sleep MS milliseconds INSIDE the NIF (a plain
 * `nanosleep`, no ERTS internals) before reading the statics back. A deferred stop runs on the
 * poller thread, which this NIF does not block, so it has the whole sleep to land:
 *   {ArmCode, StopCode, CallsBeforeSleep, CallsAfterSleep, LastIsDirect, LastThreadType, LastTag,
 *    SleptMs}
 * ------------------------------------------------------------------------------------------- */
static ERL_NIF_TERM probe_stop_sleep_ms(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM a[8];
    ERL_NIF_TERM ref = enif_make_ref(env);
    struct timespec ts;
    probe_obj *obj;
    unsigned long before;
    int fds[2];
    int arm_code;
    int stop_code;
    int ms;

    if (argc != 1 || !enif_get_int(env, argv[0], &ms) || ms < 0)
        return enif_make_badarg(env);
    if (make_pipe(fds) != 0)
        return pipe_error(env, errno);
    obj = alloc_obj(res_stop, 5);
    if (obj == NULL) {
        close(fds[0]);
        close(fds[1]);
        return alloc_error(env);
    }

    arm_code = enif_select(env, fds[0], ERL_NIF_SELECT_READ, obj, NULL, ref);
    stop_code = enif_select(env, fds[0], ERL_NIF_SELECT_STOP, obj, NULL, ref);
    before = stop_calls;

    ts.tv_sec = ms / 1000;
    ts.tv_nsec = (long)(ms % 1000) * 1000000L;
    while (nanosleep(&ts, &ts) != 0 && errno == EINTR) {
        /* resume the remaining time */
    }

    a[0] = enif_make_int(env, arm_code);
    a[1] = enif_make_int(env, stop_code);
    a[2] = enif_make_ulong(env, before);
    a[3] = enif_make_ulong(env, stop_calls);
    a[4] = enif_make_int(env, last_is_direct);
    a[5] = enif_make_int(env, last_thread_type);
    a[6] = enif_make_int(env, last_tag);
    a[7] = enif_make_int(env, ms);

    close(fds[0]);
    close(fds[1]);
    enif_release_resource(obj);
    return enif_make_tuple_from_array(env, a, 8);
}

/* ---------------------------------------------------------------------------------------------
 * probe_arm_then_stop_fallback/0 — the deferred (`ERL_NIF_SELECT_STOP_SCHEDULED`) arm, which on
 * Linux is ONLY reachable through the fallback pollset (erl_check_io.c:519-531): the "safe to close
 * now?" test at :1485 is `!wake_poller`, and the epoll/kqueue driver never sets it (a DEL goes
 * straight to `epoll_ctl`, erl_poll.c:761-877 `concurrent_update_pollset`), while the select/poll
 * fallback driver sets `*do_wake = 1` on every DEL (erl_poll.c:1399-1410). An fd the kernel poll
 * refuses drives the state onto that fallback pollset: a pipe is watchable by epoll, but
 * `/dev/null` (and any regular file or directory) answers `EPERM` from `epoll_ctl(ADD, …)`, which
 * `erts_io_control_wakeup` turns into `ERTS_EV_FLAG_FALLBACK`.
 *   {ArmCode, StopCode, StopCalls, LastIsDirect, LastThreadType, LastTag, Fd}
 * ------------------------------------------------------------------------------------------- */
static ERL_NIF_TERM probe_arm_then_stop_fallback(ErlNifEnv *env, int argc,
                                                 const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM a[7];
    ERL_NIF_TERM ref = enif_make_ref(env);
    probe_obj *obj;
    int fd;
    int arm_code;
    int stop_code;

    (void)argc;
    (void)argv;

    fd = move_fd(open("/dev/null", O_RDONLY), 900); /* pollable, but NOT by epoll (EPERM) */
    if (fd < 0)
        return pipe_error(env, errno);
    obj = alloc_obj(res_stop, 7);
    if (obj == NULL) {
        close(fd);
        return alloc_error(env);
    }

    arm_code = enif_select(env, fd, ERL_NIF_SELECT_READ, obj, NULL, ref);
    stop_code = enif_select(env, fd, ERL_NIF_SELECT_STOP, obj, NULL, ref);

    a[0] = enif_make_int(env, arm_code);
    a[1] = enif_make_int(env, stop_code);
    a[2] = enif_make_ulong(env, stop_calls);
    a[3] = enif_make_int(env, last_is_direct);
    a[4] = enif_make_int(env, last_thread_type);
    a[5] = enif_make_int(env, last_tag);
    a[6] = enif_make_int(env, fd);

    close(fd);
    enif_release_resource(obj);
    return enif_make_tuple_from_array(env, a, 7);
}

/* ---------------------------------------------------------------------------------------------
 * probe_stop_fallback_wait_ms/1 — the same fallback-pollset sequence (own fd, 901), then a BOUNDED
 * wait inside the NIF (100 us steps, `MS` max) until the deferred callback has run, so the report
 * carries the observed latency instead of a guess. This is the only probe that shows
 * `is_direct_call = 0` on a non-scheduler thread:
 *   {ArmCode, StopCode, CallsAtReturn, CallsAfterWait, LastIsDirect, LastThreadType, LastTag, Fd,
 *    WaitedNs, LatencyNs}
 * `CallsAtReturn` is read immediately after the STOP (whether the poller already beat us is a race
 * and is reported as such), `WaitedNs` is how long the wait took (-1 = timed out), `LatencyNs` is
 * the callback's own `CLOCK_MONOTONIC` stamp minus the clock read right after the STOP (-1 when the
 * callback never ran).
 * ------------------------------------------------------------------------------------------- */
static ERL_NIF_TERM probe_stop_fallback_wait_ms(ErlNifEnv *env, int argc,
                                                const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM a[10];
    ERL_NIF_TERM ref = enif_make_ref(env);
    probe_obj *obj;
    unsigned long at_return;
    long long armed_ns;
    long long waited_ns;
    long long latency_ns;
    int arm_code;
    int stop_code;
    int fd;
    int ms;

    if (argc != 1 || !enif_get_int(env, argv[0], &ms) || ms < 0)
        return enif_make_badarg(env);

    fd = move_fd(open("/dev/null", O_RDONLY), 901);
    if (fd < 0)
        return pipe_error(env, errno);
    obj = alloc_obj(res_stop, 8);
    if (obj == NULL) {
        close(fd);
        return alloc_error(env);
    }

    arm_code = enif_select(env, fd, ERL_NIF_SELECT_READ, obj, NULL, ref);
    stop_code = enif_select(env, fd, ERL_NIF_SELECT_STOP, obj, NULL, ref);
    at_return = stop_calls;
    armed_ns = now_ns();

    wait_for_stop(at_return, ms);

    latency_ns = -1;
    if (stop_calls > at_return && last_stop_ns >= 0 && armed_ns >= 0)
        latency_ns = last_stop_ns - armed_ns;
    waited_ns = -1;
    if (stop_calls > at_return && armed_ns >= 0)
        waited_ns = now_ns() - armed_ns;

    a[0] = enif_make_int(env, arm_code);
    a[1] = enif_make_int(env, stop_code);
    a[2] = enif_make_ulong(env, at_return);
    a[3] = enif_make_ulong(env, stop_calls);
    a[4] = enif_make_int(env, last_is_direct);
    a[5] = enif_make_int(env, last_thread_type);
    a[6] = enif_make_int(env, last_tag);
    a[7] = enif_make_int(env, fd);
    a[8] = enif_make_long(env, (long)waited_ns);
    a[9] = enif_make_long(env, (long)latency_ns);

    close(fd);
    enif_release_resource(obj);
    return enif_make_tuple_from_array(env, a, 10);
}

/* ---------------------------------------------------------------------------------------------
 * probe_read_on_pending_stop/0 — what a SECOND request on an fd whose stop is still pending
 * answers (erl_check_io.c:1318-1337): the state is `ERTS_EV_TYPE_STOP_NIF`, so the switch steers
 * through `steal_pending_stop_nif` and answers `ERL_NIF_SELECT_STOP_SCHEDULED` — for a READ
 * request too — and the fd is left with no pending stop, so a following STOP takes the fast track.
 * `ReadOnPending`/`StopAfter` are only meaningful while the poller has not yet drained the deferred
 * stop, so this probe is an explicit race (both shapes are reported, never asserted):
 *   {ArmCode, StopCode, ReadOnPending, StopAfter, CallsAtReport, LastIsDirect, LastThreadType, Fd}
 * ------------------------------------------------------------------------------------------- */
static ERL_NIF_TERM probe_read_on_pending_stop(ErlNifEnv *env, int argc,
                                               const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM a[8];
    ERL_NIF_TERM ref = enif_make_ref(env);
    probe_obj *obj;
    int fd;
    int arm_code;
    int stop_code;
    int read_code;
    int second_stop_code;

    (void)argc;
    (void)argv;

    fd = move_fd(open("/dev/null", O_RDONLY), 902);
    if (fd < 0)
        return pipe_error(env, errno);
    obj = alloc_obj(res_stop, 9);
    if (obj == NULL) {
        close(fd);
        return alloc_error(env);
    }

    arm_code = enif_select(env, fd, ERL_NIF_SELECT_READ, obj, NULL, ref);
    stop_code = enif_select(env, fd, ERL_NIF_SELECT_STOP, obj, NULL, ref);
    /* No yield between the STOP and these: the window is as tight as this probe can make it. */
    read_code = enif_select(env, fd, ERL_NIF_SELECT_READ, obj, NULL, ref);
    second_stop_code = enif_select(env, fd, ERL_NIF_SELECT_STOP, obj, NULL, ref);

    a[0] = enif_make_int(env, arm_code);
    a[1] = enif_make_int(env, stop_code);
    a[2] = enif_make_int(env, read_code);
    a[3] = enif_make_int(env, second_stop_code);
    a[4] = enif_make_ulong(env, stop_calls);
    a[5] = enif_make_int(env, last_is_direct);
    a[6] = enif_make_int(env, last_thread_type);
    a[7] = enif_make_int(env, fd);

    wait_for_stop(stop_calls, 500); /* let any pending stop land before the next probe */
    close(fd);
    enif_release_resource(obj);
    return enif_make_tuple_from_array(env, a, 8);
}

/* ---------------------------------------------------------------------------------------------
 * probe_stop_nostop_type/0 — the same STOP fast track on a resource type opened with NO stop * callback. The C only `ASSERT`s that the callback exists (erl_check_io.c:1279) and ASSERT is a
 * no-op in a release build, so `erts_resource_stop` (erl_nif.c:3032-3040) calls a NULL function
 * pointer. Run this probe in its OWN `erl` invocation: it is expected to take the VM down.
 *   {Code, ...} — only reachable if the oracle somehow refuses instead of calling NULL.
 * ------------------------------------------------------------------------------------------- */
static ERL_NIF_TERM probe_stop_nostop_type(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM a[3];
    ERL_NIF_TERM ref = enif_make_ref(env);
    probe_obj *obj;
    int fds[2];
    int code;

    (void)argc;
    (void)argv;

    if (make_pipe(fds) != 0)
        return pipe_error(env, errno);
    obj = alloc_obj(res_nostop, 6);
    if (obj == NULL) {
        close(fds[0]);
        close(fds[1]);
        return alloc_error(env);
    }

    code = enif_select(env, fds[0], ERL_NIF_SELECT_STOP, obj, NULL, ref);

    a[0] = enif_make_int(env, code);
    a[1] = enif_make_ulong(env, stop_calls);
    a[2] = enif_make_int(env, last_is_direct);

    close(fds[0]);
    close(fds[1]);
    enif_release_resource(obj);
    return enif_make_tuple_from_array(env, a, 3);
}

static ErlNifFunc funcs[] = {
    {"probe_reset", 0, probe_reset, 0},
    {"probe_report", 0, probe_report, 0},
    {"probe_stop_unknown_fd", 0, probe_stop_unknown_fd, 0},
    {"probe_arm_then_stop", 0, probe_arm_then_stop, 0},
    {"probe_double_stop", 0, probe_double_stop, 0},
    {"probe_cancel_then_stop", 0, probe_cancel_then_stop, 0},
    {"probe_stop_sleep_ms", 1, probe_stop_sleep_ms, 0},
    {"probe_arm_then_stop_fallback", 0, probe_arm_then_stop_fallback, 0},
    {"probe_stop_fallback_wait_ms", 1, probe_stop_fallback_wait_ms, 0},
    {"probe_read_on_pending_stop", 0, probe_read_on_pending_stop, 0},
    {"probe_stop_nostop_type", 0, probe_stop_nostop_type, 0}
};

ERL_NIF_INIT(a3_resource_stop, funcs, nif_load, NULL, NULL, NULL)
