/*
 * A REAL driver shared object whose ONLY port-command callback is `outputv`, and whose entry has
 * no `ready_async` (workspace contract A.4: the frozen driver C ABI).
 *
 * `host_driver.c` declares `.output = host_output, .outputv = NULL`, so it cannot exercise the
 * C's preference for the *vector* callback (`if (drv->outputv) { … } else { … drv->output … }`,
 * `erts/emulator/beam/io.c:1782`): an entry that declares only `outputv` must still receive every
 * `port_command/2`, and one that declares both must be handed the vector form. Rather than mutate
 * that fixture's entry (its suites depend on it), this is a separate, self-contained sibling with
 * the opposite shape:
 *
 *   * `.output = NULL`, `.outputv = vector_outputv` — the `outputv`-only port-command callback;
 *   * `.ready_output = NULL` — the slot-absent shape: a `port_command` reaches the driver through
 *     the `output`/`outputv` write path (`io.c:1782`/`io.c:1517`), with no write-readiness
 *     notification to run; the C's `no_ready_output_callback` itself (`io.c:7531-7538`) writes
 *     nothing at all;
 *   * `.ready_async = NULL` — the "async-free" shape: with no `ready_async` slot the host must
 *     release a finished `driver_async` job through the driver's own `async_free` (the C's
 *     `need_free` path, `io.c:5399-5406`).
 *
 * Every `driver_*` symbol it references is left undefined, exactly like a real OTP driver, so a
 * `dlopen(RTLD_NOW)` only succeeds once the host's `driver_*` symbols are exported from the
 * loading process (see `tests/support/mod.rs` and `build.rs`). It keeps to the `driver_*` slots
 * that are implemented today (alloc/free, output, async), because referencing an unimplemented
 * slot would make that `dlopen` fail.
 *
 * ## The start command
 *
 * `start(port, command)` parses the same whitespace/comma separated string as `host_driver.c`,
 * keeping the two tokens it needs and ignoring anything else:
 *
 *   * `log=<path>` — a file every callback appends a line to; when absent no file is touched;
 *   * `upper`      — a mode token: the `outputv` callback upper-cases the bytes it echoes.
 *
 * ## The `outputv` callback
 *
 * `vector_outputv` appends one line describing the vector it was handed —
 * `outputv vsize=<n> size=<bytes> head=<null|set> head_len=<n> binv0=<null|set> binv1=<null|set>`
 * — so a host can assert the exact shape the C builds (`io.c:1842-1864`: `iov[0]` the empty
 * header block, `iov[1]` the data, `binv` all `NULL`), and then echoes the data block back
 * through `driver_output`, so the *bytes* the callback received are observable as port data.
 *
 * ## The async verbs (integer `control` commands, `port_control/3`)
 *
 *   1  `driver_async(port, NULL, vector_async_invoke, state, vector_async_free)` — a fast job
 *      whose completion must be released through `async_free` (the entry has no `ready_async`);
 *   2  the same with a slow invoke, so a host can drop the port while the job is in flight;
 *   3  the fire-and-forget form (`NULL` invoke), released through `async_free` right away.
 *
 * Any other command answers the negative `cres` that `port_control/3` reports as `badarg`.
 */

/* The fixture uses the POSIX `nanosleep` (the slow async job), which a strict `-std=c99`
 * compilation does not expose without a feature-test macro; ask for POSIX.1-2008 before any header
 * is read so the fixture builds under `-std=c99` as well as the default mode. */
#define _POSIX_C_SOURCE 200809L

#include <erl_driver.h>

#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <time.h>

/* Private per-port state, allocated in `start` and freed in `stop`. */
typedef struct {
    ErlDrvPort port; /* the handle `outputv` needs (the callback gets no port argument) */
    int upper;       /* the `upper` token: upper-case the bytes echoed back */
    char log[512];   /* path every callback appends an event line to ("" when none) */
} vector_state;

/* Append `what` as one line to the per-port log file, if the command asked for one. */
static void log_event(const vector_state *st, const char *what) {
    FILE *f;
    if (st == NULL || st->log[0] == '\0')
        return;
    f = fopen(st->log, "a");
    if (f == NULL)
        return;
    fprintf(f, "%s\n", what);
    fclose(f);
}

/* Copy at most `cap` bytes of `src` into `dst`, NUL-terminating: a tiny, dependency-free `strncpy`. */
static void copy_token(char *dst, size_t cap, const char *src, size_t n) {
    size_t i;
    if (cap == 0)
        return;
    for (i = 0; i < n && i + 1 < cap; i++)
        dst[i] = src[i];
    dst[i] = '\0';
}

/* Parse `log=<path>` and the bare `upper` token out of the whitespace/comma separated command. */
static void parse_command(vector_state *st, const char *command) {
    const char *p = command;
    st->upper = 0;
    st->log[0] = '\0';
    if (p == NULL)
        return;
    while (*p != '\0') {
        while (*p == ' ' || *p == '\t' || *p == ',')
            p++;
        if (*p == '\0')
            break;
        if (strncmp(p, "log=", 4) == 0) {
            const char *s = p + 4;
            size_t n = 0;
            while (s[n] != '\0' && s[n] != ' ' && s[n] != '\t' && s[n] != ',')
                n++;
            copy_token(st->log, sizeof(st->log), s, n);
        } else {
            const char *s = p;
            size_t n = 0;
            char token[16];
            while (s[n] != '\0' && s[n] != ' ' && s[n] != '\t' && s[n] != ',')
                n++;
            copy_token(token, sizeof(token), s, n);
            if (strcmp(token, "upper") == 0)
                st->upper = 1;
        }
        while (*p != '\0' && *p != ' ' && *p != '\t' && *p != ',')
            p++;
    }
}

static ErlDrvData vector_start(ErlDrvPort port, char *command) {
    vector_state *st = (vector_state *) driver_alloc(sizeof(vector_state));
    if (st == NULL)
        return (ErlDrvData) -1;
    st->port = port;
    st->upper = 0;
    st->log[0] = '\0';
    parse_command(st, command);
    log_event(st, "start");
    return (ErlDrvData) st;
}

static void vector_stop(ErlDrvData drv_data) {
    vector_state *st = (vector_state *) drv_data;
    if (st == NULL)
        return;
    log_event(st, "stop");
    driver_free(drv_data);
}

/* The async job (runs on a worker thread): log that the job body finished. The completion is only
 * released on the port thread, so this line is how a host observes that a job has completed. */
static void vector_async_invoke(void *data) {
    vector_state *st = (vector_state *) data;
    if (st == NULL)
        return;
    log_event(st, "async_invoke");
}

/* A slow async job (runs on a worker thread): it sleeps so a host can drop the port while the job
 * is in flight. It touches neither the port handle nor the driver state. */
static void vector_async_slow_invoke(void *data) {
    struct timespec ts;
    (void) data;
    ts.tv_sec = 0;
    ts.tv_nsec = 500 * 1000 * 1000;
    nanosleep(&ts, NULL);
}

/* `async_free`: releases `async_data` when no `ready_async` will run for the job — this entry's
 * only arm, and the port-gone path. It logs through the async data's own path, which is why the
 * tests that observe it drop the port instead of closing it (a closed port has already freed the
 * state, and the release still has to be delivered). */
static void vector_async_free(void *data) {
    vector_state *st = (vector_state *) data;
    if (st == NULL)
        return;
    log_event(st, "async_free");
}

/* The ONLY port-command callback of this entry (`output = NULL`). It records the exact vector
 * shape it was handed and echoes the data block back to the port owner through `driver_output`. */
static void vector_outputv(ErlDrvData drv_data, ErlIOVec *ev) {
    vector_state *st = (vector_state *) drv_data;
    char line[160];
    int vsize;
    ErlDrvSizeT size;
    int head_null;
    ErlDrvSizeT head_len;
    int binv0_null;
    int binv1_null;
    if (st == NULL || ev == NULL)
        return;

    vsize = (int) ev->vsize;
    size = ev->size;
    /* The C's shape: `iov[0]` is the empty header block a driver may fill in, `iov[1]` the data. */
    head_null = (vsize >= 1 && ev->iov != NULL && ev->iov[0].iov_base == NULL);
    head_len = (vsize >= 1 && ev->iov != NULL) ? ev->iov[0].iov_len : (ErlDrvSizeT) 0;
    binv0_null = (ev->binv != NULL && ev->binv[0] == NULL);
    binv1_null = (vsize >= 2 && ev->binv != NULL && ev->binv[1] == NULL);
    snprintf(line, sizeof(line),
             "outputv vsize=%d size=%lu head=%s head_len=%lu binv0=%s binv1=%s",
             vsize, (unsigned long) size, head_null ? "null" : "set",
             (unsigned long) head_len, binv0_null ? "null" : "set",
             binv1_null ? "null" : "set");
    log_event(st, line);

    if (vsize >= 2 && ev->iov != NULL && ev->iov[1].iov_base != NULL) {
        char *data = (char *) ev->iov[1].iov_base;
        ErlDrvSizeT len = ev->iov[1].iov_len;
        ErlDrvSizeT i;
        if (st->upper) {
            for (i = 0; i < len; i++)
                if (data[i] >= 'a' && data[i] <= 'z')
                    data[i] = (char) (data[i] - 'a' + 'A');
        }
        (void) driver_output(st->port, data, len);
    }
}

/* The driver "ioctl" behind `port_control/3` (see the async verbs in the file header). */
static ErlDrvSSizeT vector_control(ErlDrvData drv_data, unsigned int command,
                                   char *buf, ErlDrvSizeT len,
                                   char **rbuf, ErlDrvSizeT rlen) {
    vector_state *st = (vector_state *) drv_data;
    (void) buf;
    (void) len;
    (void) rbuf;
    (void) rlen;
    switch (command) {
    case 1:
        return (ErlDrvSSizeT) driver_async(st->port, NULL, vector_async_invoke,
                                           st, vector_async_free);
    case 2:
        return (ErlDrvSSizeT) driver_async(st->port, NULL, vector_async_slow_invoke,
                                           st, vector_async_free);
    case 3:
        return (ErlDrvSSizeT) driver_async(st->port, NULL, NULL, st, vector_async_free);
    default:
        /* An unrecognised request: a negative `cres`, which `port_control/3` reports as `badarg`. */
        return (ErlDrvSSizeT) -1;
    }
}

static ErlDrvEntry vector_driver_entry = {
    .init = NULL,
    .start = vector_start,
    .stop = vector_stop,
    .output = NULL,            /* the `outputv`-only shape */
    .ready_input = NULL,
    .ready_output = NULL,      /* no write-readiness notification; the write path is `outputv` */
    .driver_name = "vector_driver",
    .finish = NULL,
    .handle = NULL,
    .control = vector_control,
    .timeout = NULL,
    .outputv = vector_outputv, /* the ONLY port-command callback */
    .ready_async = NULL,       /* the "async-free" shape: the host runs `async_free` itself */
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

DRIVER_INIT(vector_driver) {
    return &vector_driver_entry;
}
