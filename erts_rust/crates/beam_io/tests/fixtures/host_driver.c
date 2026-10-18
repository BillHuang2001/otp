/*
 * A REAL driver shared object — the C fixture for the `beam_io` port/driver seam
 * (workspace contract A.4: the frozen driver C ABI).
 *
 * It is compiled by the integration test `tests/fixture_builds.rs` with the system `cc`
 * against the system `erl_driver.h` and then `dlopen`ed. Every `driver_*` / `erl_drv_*`
 * symbol it references is deliberately left *undefined* by the linker: those calls are
 * resolved out of the *loading process*'s dynamic symbol table (the host runtime, e.g.
 * `beam_abi::driver::dle` + the `-Wl,--export-dynamic` test link). A `dlopen(RTLD_NOW)`
 * therefore only succeeds once the `driver_*` host functions are exported — the empirical
 * proof that a hosted port can bind a real C driver.
 *
 * `DRIVER_INIT(host_driver)` exports `driver_init` returning the `ErlDrvEntry` below, whose
 * `driver_name` is `"host_driver"`, so the only name this library may be loaded as is
 * `host_driver`.
 *
 * Portability note: this fixture references ONLY `driver_*` symbols whose host slots are
 * implemented today (alloc/free, output, output-term, select, timers, async, the port queues).
 * Referencing an unimplemented slot would make `dlopen(RTLD_NOW)` fail, so the fixture deliberately
 * keeps to that subset.
 *
 * ## The start command
 *
 * `start(port, command)` parses a simple, whitespace/comma separated command string:
 *
 *   * `fd=<n>`   — the descriptor the port will `driver_select` on (default: none, `-1`);
 *   * `log=<path>` — an **absolute** file path every callback appends a line to (`start`,
 *     `output`, `stop`), so a host can observe the *order* in which the driver's callbacks ran;
 *     when absent, or relative, no file is ever touched (nothing this fixture writes may land
 *     relative to the process's working directory);
 *   * `upper`     — a mode token: `output` upper-cases the bytes it echoes back.
 *   * `hdl`       — a mode token: every callback that runs while it is set additionally appends
 *     `handle=<%p>` (the `ErlDrvPort` the driver stashed at `start`), so a host can assert the
 *     handle is the *same value* the whole port's life;
 *   * `hdr=<n>`   — the async data a `driver_async` verb hands over (`0`, the default, is this
 *     port's own state pointer; `1` is `NULL`, the "no async data" case);
 *   * `fail-start=<kind>` — a **start failure** token: `start` returns the matching C sentinel
 *     (`ERL_DRV_ERROR_GENERAL` / `_ERRNO` / `_BADARG`) for the exact spellings
 *     `general`/`errno`/`badarg`, before allocating any state (see `start_failure_sentinel`); any
 *     other value is ignored, so the port starts normally.
 *
 * Anything else is ignored. The parsed state is stored in the per-port `host_state`.
 *
 * ## The callback log
 *
 * When an absolute `log=<path>` is given, `start`, `output` and `stop` each append one line
 * (`"start"`, `"output"`, `"stop"`) to that file, in the order they ran. That is the observable
 * evidence a port created from the process-global driver table really ran the driver's
 * `start`/`stop` callbacks, in order, around its data path. `async_invoke` (the worker-thread job
 * body), `ready_async` and `async_free` append
 * `"async_invoke"`/`"ready_async …"`/`"async_free"` through the same path, so both the job's
 * completion on its worker and the arm its completion was handed to are observable too. (An
 * `async_invoke`/`async_free` line is written from the async worker thread; every append opens,
 * writes and closes the file, so the lines never interleave.)
 *
 * ## The port-exit notifications
 *
 * The entry declares both port-exit slots, so a host can observe the adapter that maps
 * `Driver::port_exit` onto the closest slot the entry has: `process_exit` when the entry declares
 * it, else `emergency_close`. `host_process_exit` appends `"process_exit"` and `driver_output`s
 * `"P"`; `host_emergency_close` appends `"emergency_close"` and `driver_output`s `"E"`. Both get
 * their `ErlDrvData`, so both log through the port's own `log=` path. The
 * `host_driver_no_process_exit.c` variant forces the `process_exit` slot to `NULL` and so exercises
 * the `emergency_close` fallback.
 *
 * ## The control vocabulary (integer commands, `port_control/3`)
 *
 *   1  echo `min(len, rlen)` received bytes into `*rbuf`; return the count.
 *   2  `driver_set_timer(port, ms)` — `ms` is an `int` read from `buf` when present, else 100;
 *      return the host's code.
 *   3  `driver_cancel_timer(port)`; return the host's code.
 *   4  `driver_select(port, (ErlDrvEvent) fd, ERL_DRV_READ | ERL_DRV_USE, 1)` on the configured
 *      fd; return the host's code (or -1 when no fd was configured).
 *   5  `erl_drv_output_term(driver_mk_port(port), spec, 6)` with the 2-tuple
 *      `{host_driver, 1}`; return the host's code.
 *   6  `driver_async(port, NULL, host_async_invoke, state, NULL)` — the invoke callback runs on a
 *      worker thread and `driver_output`s `"A"`, so async completion is observable; return the
 *      host's code.
 *   7  `driver_select(port, (ErlDrvEvent) fd, ERL_DRV_READ, 0)` — deselect; return the host's code
 *      (or -1 when no fd was configured).
 *   8  `driver_read_timer(port, &ms)`; return `ms` when a timer is pending, else -1.
 *   9  `driver_select(port, (ErlDrvEvent) fd, ERL_DRV_READ | ERL_DRV_USE, 1)` — select for the
 *      `stop_select` release callback; return the host's code.
 *  10  `driver_select(port, (ErlDrvEvent) fd, ERL_DRV_READ | ERL_DRV_USE_NO_CALLBACK, 1)` — the
 *      same, declining `stop_select`; return the host's code.
 *  11  `driver_select(port, (ErlDrvEvent) fd, ERL_DRV_READ, 1)` — a plain select, no
 *      `ERL_DRV_USE`; return the host's code.
 *  12  `driver_select(port, (ErlDrvEvent) fd, ERL_DRV_READ, 0)` — deselect the configured fd
 *      (the same release verbs 7 and 4/9/10/11 pair with); return the host's code.
 *  13  answer a fixed reply (`"host-reply"`) by pointing `*rbuf` at the driver's own static
 *      buffer; return the byte count.
 *  14  `driver_async(port, NULL, host_async_invoke, data, host_async_free)` — the same job as verb
 *      6 but with an `async_free`; with `ready_async` present the completion goes there (the free
 *      never runs), and on a build whose entry leaves `ready_async` `NULL` the host must run the
 *      `async_free` instead; return the host's code.
 *  15  `driver_async(port, NULL, NULL, data, host_async_free)` — the **fire-and-forget** form: the
 *      `invoke` callback is `NULL`, so no `ready_async` can ever run and the host must release the
 *      data through the `async_free` right away; return the host's code.
 *  16  `driver_async(port, NULL, host_async_slow_invoke, data, host_async_free)` — the same as 14
 *      with an invoke that sleeps ~500 ms, so a host can drop the port **while the job runs**;
 *      return the host's code.
 *  17  `driver_select(port, fd, ERL_DRV_READ | ERL_DRV_USE, 1)` followed by
 *      `driver_select(port, fd, ERL_DRV_READ, 1)` — a re-selection that omits `ERL_DRV_USE`, which
 *      must **not** clear the release obligation the first select took on; return the second
 *      host's code.
 *  18  `driver_select(port, fd, ERL_DRV_READ | ERL_DRV_USE, 1)` followed by
 *      `driver_select(port, fd, ERL_DRV_READ | ERL_DRV_USE_NO_CALLBACK, 1)` — a re-selection that
 *      **declines** the callback, which must clear the obligation; return the second host's code.
 *  19  `driver_enq(port, buf, len)` — append the control argument to the port's **driver queue**
 *      (the C's `prt->ioq`); answer `enq=<rc>` (the host's return code, `0` = queued).
 *  20  `driver_sizeq(port)`; answer `size=<n>`.
 *  21  `driver_peekq(port, &vlen)`; answer `vlen=<v> bytes=<the queued head bytes>` (the whole
 *      queue when non-empty, `vlen == 1` — the host's one-contiguous-entry shape).
 *  22  `driver_deq(port, n)` with `n` an `int` from the argument (default `0`); answer
 *      `deq=<left> size=<n>`, where `left` is `driver_deq`'s return — the **new** queue size, or
 *      `(ErlDrvSizeT)-1` when `n` exceeds the queue (which then stays untouched, `io.c:6807`).
 *  23  `driver_peekqv(port, &ev)`; answer `qvsize=<v> qsize=<n> bytes=<the queued head bytes>`.
 *  24  `driver_select(port, (ErlDrvEvent) fd, ERL_DRV_WRITE, 1)` — arm the **write** direction on
 *      the configured fd, so a write-readiness event schedules an `OUTPUT` task; return the host's
 *      code (or -1 when no fd was configured).
 *  25  `set_busy_port(port, 1)` — mark the port **busy** (the C's per-port busy state); the C
 *      returns `void`, so answer the `cres == 0` reply (the empty list).
 *  26  `set_busy_port(port, 0)` — clear the port's busy state; the same empty reply.
 *  27  set `*rbuf = NULL` and return `5` — the C's documented `cres > 0`-with-a-`NULL`-reply case,
 *      which `port_control/3` reports as the **empty list** (`ertdoc › driver_entry.md › control`:
 *      "If `*rbuf` is set to `NULL`, an empty list is returned"), never `badarg`.
 *
 * Verbs 19–23 answer their observation as `key=value` text; the `bytes=` field is the trailing raw
 * payload and every other field is a decimal number. Verbs 19–23 are the driver-queue family
 * (`driver_enq`/`driver_sizeq`/`driver_peekq[v]`/`driver_deq`, `io.c:6790-6919`): the queue is the
 * host's driver-private byte queue (the C's `prt->ioq`, **not** the port's Erlang→driver output
 * queue), so it persists across these `control` calls for the port's whole life and a host can
 * drive the four steps in order. Verbs 25/26 flip the port's busy state (`set_busy_port`) and 27
 * exercises the `NULL`-`*rbuf` reply arm; none of them carries a `key=value` payload.
 *
 * Any other command returns `-1`, the C's negative `cres` (`port_control/3` answers `badarg`,
 * `io.c:4074`).
 *
 * A fired timer (`timeout`) and readable input (`ready_input`) are each turned into
 * `driver_output` traffic so a hosted port can observe them.
 *
 * ## `stop_select` and `async_free`
 *
 * The driver implements `stop_select`, the `ERL_DRV_USE` release callback, as `host_stop_select`.
 * Its signature (`void(ErlDrvEvent, void*)`) carries neither `ErlDrvData` nor the port, so it
 * cannot reach the per-port `log=` path from its arguments; the fixture keeps the most recently
 * started port's path in the file-scope `g_stop_select_log` instead, and appends `"stop_select"`
 * to it. A test therefore drives one port at a time.
 *
 * `host_async_free(void *data)` *is* handed the `driver_async` `async_data` (this port's state),
 * so it logs through that state's own path. It may run **after** the port's `stop` (the host drains
 * the job set at teardown), which is why the state is never freed — see `host_stop`.
 *
 * ## The variant builds
 *
 * `host_driver.c` builds an entry whose `ready_async` is `host_ready_async`, whose `process_exit` is
 * `host_process_exit` and whose `emergency_close` is `host_emergency_close`. Each slot has its own
 * `_SLOT` override macro, so a variant that `#include`s this file can force any of them to `NULL`
 * (`host_driver_no_ready_async.c`, the `NULL`-slot build the host's `async_free` arm is exercised
 * with, and `host_driver_no_process_exit.c`, the build the port-exit adapter's `emergency_close`
 * fallback is exercised with).
 *
 * The three write-side slots are `output`, `outputv` and `ready_output`. The base entry declares
 * `output` and `ready_output` but leaves `outputv` `NULL` (an `output`-only driver); the `outputv`
 * *callback* is compiled only when a variant asks for it by defining `HOST_DRIVER_OUTPUTV_SLOT`
 * (which also sets `HOST_DRIVER_HAS_OUTPUTV`). `host_driver_both.c` sets the slot to
 * `host_outputv`, so its entry is the one build that declares all three at once — the shape the
 * C's `outputv`-over-`output` preference (`io.c:1782`) is about.
 */
/* The fixture uses the POSIX `nanosleep` (the slow async job) and `read`/`close`, which a strict
 * `-std=c99` compilation does not expose without a feature-test macro; ask for POSIX.1-2008
 * before any header is read so the fixture builds under `-std=c99` as well as the default mode. */
#define _POSIX_C_SOURCE 200809L

#include <erl_driver.h>

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

/* The entry's `ready_async` slot: `host_ready_async` normally, `NULL` in the variant build
 * (`host_driver_no_ready_async.c`) that exists to exercise the host's other async arm. A variant
 * defines `HOST_DRIVER_READY_ASYNC_SLOT` before including this file; that also tells us the
 * `ready_async` *function* is unwanted there, so it is omitted (never an unused-function warning). */
#ifdef HOST_DRIVER_READY_ASYNC_SLOT
/* Defined by a variant — respect its choice. */
#else
#define HOST_DRIVER_READY_ASYNC_SLOT host_ready_async
#define HOST_DRIVER_HAS_READY_ASYNC 1
#endif

/* The entry's two port-exit slots: the fixture's own callbacks normally, `NULL` in a variant build
 * (`host_driver_no_process_exit.c`, the `emergency_close`-only build the host's port-exit adapter's
 * fallback arm is exercised with). Same pattern as `HOST_DRIVER_READY_ASYNC_SLOT` above: a variant
 * defines the `_SLOT` macro before including this file, and the `HAS_` macro then omits the
 * corresponding function so it is never an unused-function warning. */
#ifdef HOST_DRIVER_PROCESS_EXIT_SLOT
/* Defined by a variant — respect its choice. */
#else
#define HOST_DRIVER_PROCESS_EXIT_SLOT host_process_exit
#define HOST_DRIVER_HAS_PROCESS_EXIT 1
#endif
#ifdef HOST_DRIVER_EMERGENCY_CLOSE_SLOT
/* Defined by a variant — respect its choice. */
#else
#define HOST_DRIVER_EMERGENCY_CLOSE_SLOT host_emergency_close
#define HOST_DRIVER_HAS_EMERGENCY_CLOSE 1
#endif

/* The entry's `outputv` slot: `NULL` in the base fixture (the `output`-only shape) and the vector
 * callback in a variant that asks for it. This is the mirror of the `_SLOT` blocks above with the
 * default flipped to `NULL`: the base entry deliberately declares only `output`, and the variant
 * that wants both write callbacks (`host_driver_both.c`) defines `HOST_DRIVER_OUTPUTV_SLOT` to
 * `host_outputv`, which also tells this file to compile that callback (never an unused-function
 * warning in the base build). */
#ifdef HOST_DRIVER_OUTPUTV_SLOT
#define HOST_DRIVER_HAS_OUTPUTV 1
#else
#define HOST_DRIVER_OUTPUTV_SLOT NULL
#endif
/* Private per-port state, allocated in `start` and intentionally **never freed** (see `host_stop`). */
typedef struct {
    ErlDrvPort port;   /* the port handle `output`/`timeout` need (they get no port argument) */
    int fd;            /* the descriptor `driver_select` is armed on, or -1 when none given */
    int timeouts;      /* how many times `timeout` fired */
    int reads;         /* how many times `ready_input` fired */
    int log_handle;    /* the `hdl` token: log the stashed `ErlDrvPort` from every callback */
    int async_null;    /* the `hdr=1` token: hand the async verbs a NULL async_data */
    char mode[16];     /* the mode token from the command string ("" when none) */
    char log[512];     /* path every callback appends an event line to ("" when none) */
} host_state;

/* The `log=` path `stop_select` logs to. `stop_select` gets no `ErlDrvData` (see the file
 * header), so the path is published process-wide by the most recent `start` instead. */
static char g_stop_select_log[512] = "";

/* Append `what` as one line to the per-port log file, if the command asked for one.
 *
 * Only an absolute `log=` path is honoured, so nothing this fixture writes can land relative to the
 * process's working directory. */
static void log_event(const host_state *st, const char *what) {
    FILE *f;
    if (st == NULL || st->log[0] != '/')
        return;
    f = fopen(st->log, "a");
    if (f == NULL)
        return;
    fprintf(f, "%s\n", what);
    fclose(f);
}

/* Append `handle=<%p>` for the handle the driver stashed at `start`, when the `hdl` token asked
 * for it. A host asserts every such line carries the same value, the pointer-identity half of the
 * "the `ErlDrvPort` a driver sees never moves" contract (the other half being that the runtime
 * still accepts that pointer later, which the data paths prove). */
static void log_handle(const host_state *st) {
    FILE *f;
    if (st == NULL || st->log[0] != '/' || !st->log_handle)
        return;
    f = fopen(st->log, "a");
    if (f == NULL)
        return;
    fprintf(f, "handle=%p\n", (void *) st->port);
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

/* Parse `fd=<n>`, `log=<path>`, `hdr=<0|1>`, and the `hdl`/bare mode tokens out of the
 * whitespace/comma separated command string. */
static void parse_command(host_state *st, const char *command) {
    const char *p = command;
    st->fd = -1;
    st->mode[0] = '\0';
    st->log[0] = '\0';
    st->log_handle = 0;
    st->async_null = 0;
    if (p == NULL)
        return;
    while (*p != '\0') {
        while (*p == ' ' || *p == '\t' || *p == ',')
            p++;
        if (*p == '\0')
            break;
        if (strncmp(p, "fd=", 3) == 0) {
            st->fd = (int) strtol(p + 3, NULL, 10);
        } else if (strncmp(p, "log=", 4) == 0) {
            const char *s = p + 4;
            size_t n = 0;
            while (s[n] != '\0' && s[n] != ' ' && s[n] != '\t' && s[n] != ',')
                n++;
            copy_token(st->log, sizeof(st->log), s, n);
        } else if (strncmp(p, "hdr=", 4) == 0) {
            st->async_null = strtol(p + 4, NULL, 10) != 0;
        } else {
            const char *s = p;
            size_t n = 0;
            while (s[n] != '\0' && s[n] != ' ' && s[n] != '\t' && s[n] != ',')
                n++;
            copy_token(st->mode, sizeof(st->mode), s, n);
            if (strcmp(st->mode, "hdl") == 0)
                st->log_handle = 1;
        }
        while (*p != '\0' && *p != ' ' && *p != '\t' && *p != ',')
            p++;
    }
}

/* Read an `int` out of a control buffer, or `def` when the buffer is too short. */
static int buf_int(char *buf, ErlDrvSizeT len, int def) {
    int v = def;
    if (buf != NULL && len >= sizeof(int))
        memcpy(&v, buf, sizeof(int));
    return v;
}

/* Copy `prefix`, then `data[0 .. len]` (when non-null), into the caller's reply buffer and return
 * the total length. A reply that does not fit answers the C's negative `cres`, which never happens
 * for these reports: the host always hands in a 4096-byte buffer. */
static ErlDrvSSizeT reply_prefixed(char **rbuf, ErlDrvSizeT rlen,
                                   const char *prefix, const char *data, ErlDrvSizeT len) {
    size_t plen = strlen(prefix);
    if (rbuf == NULL || *rbuf == NULL || plen + (size_t) len > (size_t) rlen)
        return (ErlDrvSSizeT) -1;
    memcpy(*rbuf, prefix, plen);
    if (data != NULL && len > 0)
        memcpy(*rbuf + plen, data, (size_t) len);
    return (ErlDrvSSizeT) (plen + (size_t) len);
}

/* Gather `iov[0 .. vlen]` into `out` (bounded by `cap`) and return the byte count; an entry with a
 * null base or a zero length contributes nothing. Used by the peek verbs to linearize the iovec
 * view the host publishes into a byte payload the reply can carry. */
static ErlDrvSizeT gather_iov(char *out, ErlDrvSizeT cap, const SysIOVec *iov, int vlen) {
    ErlDrvSizeT n = 0;
    int i;
    if (iov == NULL || vlen <= 0)
        return 0;
    for (i = 0; i < vlen && n < cap; i++) {
        ErlDrvSizeT len = iov[i].iov_len;
        if (iov[i].iov_base == NULL || len == 0)
            continue;
        if (len > cap - n)
            len = cap - n;
        memcpy(out + n, iov[i].iov_base, (size_t) len);
        n += len;
    }
    return n;
}
/* The `fail-start=<kind>` token: the `start` sentinel the command string asks for, so a host can
 * observe the C's three `ERL_DRV_ERROR_*` `start` failures through a real shared object. Scans
 * `command` (the same whitespace/comma separated tokenization `parse_command` uses) and answers the
 * sentinel for an exact `general`/`errno`/`badarg` spelling, else `(ErlDrvData) 0`.
 *
 * `host_start` consults this **before** allocating its state and returns the sentinel without ever
 * building a `host_state`: a failed `start` must leave nothing behind (the host runs the driver's
 * `stop` with a null `ErlDrvData`, so a state allocated before the failure would leak). */
static ErlDrvData start_failure_sentinel(const char *command) {
    const char *p = command;
    char token[32];
    if (p == NULL)
        return (ErlDrvData) 0;
    while (*p != '\0') {
        const char *s;
        size_t n = 0;
        while (*p == ' ' || *p == '\t' || *p == ',')
            p++;
        if (*p == '\0')
            break;
        s = p;
        while (s[n] != '\0' && s[n] != ' ' && s[n] != '\t' && s[n] != ',')
            n++;
        copy_token(token, sizeof(token), s, n);
        if (strncmp(token, "fail-start=", 11) == 0) {
            const char *kind = token + 11;
            if (strcmp(kind, "general") == 0)
                return ERL_DRV_ERROR_GENERAL;
            if (strcmp(kind, "errno") == 0)
                return ERL_DRV_ERROR_ERRNO;
            if (strcmp(kind, "badarg") == 0)
                return ERL_DRV_ERROR_BADARG;
        }
        p = s + n;
    }
    return (ErlDrvData) 0;
}

static ErlDrvData host_start(ErlDrvPort port, char *command) {
    ErlDrvData failure = start_failure_sentinel(command);
    host_state *st;
    if (failure != (ErlDrvData) 0)
        return failure;
    st = (host_state *) driver_alloc(sizeof(host_state));
    if (st == NULL)
        return (ErlDrvData) -1;
    st->port = port;
    st->fd = -1;
    st->timeouts = 0;
    st->reads = 0;
    st->mode[0] = '\0';
    st->log[0] = '\0';
    parse_command(st, command);
    /* Publish this port's log path for `stop_select`, which receives no `ErlDrvData`. */
    copy_token(g_stop_select_log, sizeof(g_stop_select_log), st->log, strlen(st->log));
    /* The `busy` mode token: mark the port busy **at start time** through the C `set_busy_port`, so
     * a host can observe a driver that reports itself busy before it is handed any command (the
     * state a `port_command/2,3` on it must consult). `set_busy_port` returns `void`. */
    if (strcmp(st->mode, "busy") == 0)
        set_busy_port(port, 1);
    log_event(st, "start");
    log_handle(st);
    return (ErlDrvData) st;
}
static void host_stop(ErlDrvData drv_data) {
    host_state *st = (host_state *) drv_data;
    if (st == NULL)
        return;
    log_event(st, "stop");
    /* No `driver_free` here, on purpose: a `driver_async` job still in flight reaches
     * `host_async_free` from the host's teardown, i.e. **after** this `stop` has run, and that
     * callback logs through the same per-port path. Freeing would hand it a dangling log path, so
     * the state lives as long as the process (a test fixture's process ends with the test). */
}

/* The `ERL_DRV_USE` release callback (`stop_select`): the driver is told a `driver_select`ed
 * handle is really gone, so it may close or recycle it. The callback receives neither
 * `ErlDrvData` nor the port, so it logs through the path the most recent `start` published (see
 * `g_stop_select_log` and the file header). */
static void host_stop_select(ErlDrvEvent event, void *reserved) {
    FILE *f;
    (void) event;
    (void) reserved;
    if (g_stop_select_log[0] != '/')
        return;
    f = fopen(g_stop_select_log, "a");
    if (f == NULL)
        return;
    fprintf(f, "stop_select\n");
    fclose(f);
}

/* The `ErlDrvEntry.process_exit` slot: the host's port-exit adapter reaches it from the port-exit
 * seam (`PortRegistry::notify_port_exit` -> `DriverPort::port_exit`), the advisory notification a
 * port delivers **before** its teardown `stop`. The callback has its `ErlDrvData`, so it logs
 * through the port's own `log=` path and emits a `"P"` marker through `driver_output` — both
 * observable end to end (the log line, and the marker as port data on the next port-task round). */
#ifdef HOST_DRIVER_HAS_PROCESS_EXIT
static void host_process_exit(ErlDrvData drv_data, ErlDrvMonitor *monitor) {
    host_state *st = (host_state *) drv_data;
    (void) monitor; /* the adapter passes a null monitor — see the crate's driver-ABI note */
    if (st == NULL)
        return;
    log_event(st, "process_exit");
    log_handle(st);
    (void) driver_output(st->port, "P", 1);
}
#endif

/* The `ErlDrvEntry.emergency_close` slot: the host's port-exit adapter's **fallback** for an entry
 * with no `process_exit`, so a driver whose only exit hook is `emergency_close` still observes the
 * notification. Observable exactly like `host_process_exit`, with its own line and marker. */
#ifdef HOST_DRIVER_HAS_EMERGENCY_CLOSE
static void host_emergency_close(ErlDrvData drv_data) {
    host_state *st = (host_state *) drv_data;
    if (st == NULL)
        return;
    log_event(st, "emergency_close");
    log_handle(st);
    (void) driver_output(st->port, "E", 1);
}
#endif

/* Echo whatever Erlang sends to the port back to its owner. `output` receives no port argument,
 * so the handle stored at `start` is used. In `upper` mode the echoed bytes are upper-cased, so
 * the parsed command token is observable. */
static void host_output(ErlDrvData drv_data, char *buf, ErlDrvSizeT len) {
    host_state *st = (host_state *) drv_data;
    if (st == NULL)
        return;
    log_event(st, "output");
    log_handle(st);
    /* `silent`: consume the command and emit nothing (no `driver_output`). This is the driver shape
     * the C's `void` output callback allows — the command is counted as written and the port owner
     * gets no `{Port,{data,_}}` (`io.c:1405-1408` counts the whole command as out and frees it). It
     * pins that a driver which produces no bytes must not be reported as having produced any. */
    if (strcmp(st->mode, "silent") == 0)
        return;
    if (strcmp(st->mode, "upper") == 0 && buf != NULL) {
        ErlDrvSizeT i;
        for (i = 0; i < len; i++)
            if (buf[i] >= 'a' && buf[i] <= 'z')
                buf[i] = (char) (buf[i] - 'a' + 'A');
    }
    (void) driver_output(st->port, buf, len);
}
#ifdef HOST_DRIVER_HAS_OUTPUTV
/* The C's **vector** port-command callback (`io.c:1842-1864`): `iov[0]` is the empty header block a
 * driver may fill in, `iov[1]` the data. An entry that declares it is preferred over `output`
 * (`if (drv->outputv) { … } else { … drv->output … }`, `io.c:1782`) — the preference
 * `host_driver_both.c` exists to exercise. It logs `outputv` (so a host can prove the vector form
 * ran **and** the plain form did not: `output` logs `output`) and echoes the data block back
 * through `driver_output`. */
static void host_outputv(ErlDrvData drv_data, ErlIOVec *ev) {
    host_state *st = (host_state *) drv_data;
    if (st == NULL)
        return;
    log_event(st, "outputv");
    log_handle(st);
    if (ev != NULL && ev->vsize >= 2 && ev->iov != NULL && ev->iov[1].iov_base != NULL) {
        char *data = (char *) ev->iov[1].iov_base;
        ErlDrvSizeT len = ev->iov[1].iov_len;
        if (strcmp(st->mode, "upper") == 0) {
            ErlDrvSizeT i;
            for (i = 0; i < len; i++)
                if (data[i] >= 'a' && data[i] <= 'z')
                    data[i] = (char) (data[i] - 'a' + 'A');
        }
        (void) driver_output(st->port, data, len);
    }
}
#endif

/* A readable descriptor: read(2) to EAGAIN (or EOF) and forward every byte read. */
static void host_ready_input(ErlDrvData drv_data, ErlDrvEvent event) {
    host_state *st = (host_state *) drv_data;
    int fd = (int) (intptr_t) event;
    char buf[256];
    if (st == NULL)
        return;
    st->reads++;
    for (;;) {
        ssize_t n = read(fd, buf, sizeof(buf));
        if (n > 0)
            (void) driver_output(st->port, buf, (ErlDrvSizeT) n);
        else if (n == 0)
            break; /* EOF */
        else if (errno == EINTR)
            continue;
        else
            break; /* EAGAIN (drained) or a real error */
    }
}

/* A writable descriptor: emit a small marker so the callback is observable. */
static void host_ready_output(ErlDrvData drv_data, ErlDrvEvent event) {
    host_state *st = (host_state *) drv_data;
    (void) event;
    if (st != NULL)
        (void) driver_output(st->port, "W", 1);
}

/* A fired timer: count it and emit a marker. */
static void host_timeout(ErlDrvData drv_data) {
    host_state *st = (host_state *) drv_data;
    if (st != NULL) {
        st->timeouts++;
        (void) driver_output(st->port, "T", 1);
    }
}

/* The async job (runs on a worker thread): log the fact that the job body itself finished — the
 * completion is only drained on the port thread, so this line is how a host observes that a fast
 * job has completed — then emit a marker through the port handle. The `driver_output` reaches
 * nothing (no port context is installed on a worker thread; the crate's driver-ABI note), which is
 * fine: the log line is the observable. */
static void host_async_invoke(void *data) {
    host_state *st = (host_state *) data;
    if (st == NULL)
        return;
    log_event(st, "async_invoke");
    (void) driver_output(st->port, "A", 1);
}

/* A slow async job (runs on a worker thread): it sleeps so a host can drop the port while the job
 * is in flight. It touches neither the port handle nor the driver state (neither before nor after
 * the sleep) — a job that outlives its port must not call `driver_*` (the port's context is gone;
 * see the crate's driver-ABI note) nor read the freed port state, and it is the host's `async_free`
 * that releases the job's `async_data`. */
static void host_async_slow_invoke(void *data) {
    struct timespec ts;
    (void) data;
    ts.tv_sec = 0;
    ts.tv_nsec = 500 * 1000 * 1000;
    nanosleep(&ts, NULL);
}

/* The completion callback: the host runs it when the entry has a `ready_async` (the fixture's
 * default build does). The `drv_data` the host stored at `start` must be the same pointer the job
 * was handed as `thread_data`, so the port handle stashed behind either is the same value — the
 * `h=same` marker, which is what the `ErlDrvPort`-stability test asserts. */
#ifdef HOST_DRIVER_HAS_READY_ASYNC
static void host_ready_async(ErlDrvData drv_data, ErlDrvThreadData thread_data) {
    host_state *st = (host_state *) drv_data;
    host_state *tr = (host_state *) thread_data;
    if (st == NULL)
        return;
    log_event(st, (tr != NULL && tr->port == st->port) ? "ready_async h=same" : "ready_async h=diff");
    (void) driver_output(st->port, "H", 1);
}
#endif

/* The driver's `async_free`: releases `async_data` (this port's state) when no `ready_async` will
 * run for the job — the port-gone path and the no-`ready_async` entry. It logs through the async
 * data's own path, which stays valid even when the port was closed first: the state is never freed
 * (see `host_stop`), so this release can be delivered after `stop` without a dangling path. */
static void host_async_free(void *data) {
    host_state *st = (host_state *) data;
    if (st == NULL)
        return;
    log_event(st, "async_free");
}

/* The async data a verb hands over: `hdr=1` makes it `NULL` (no per-port state to log through),
 * so those tests observe the release through `g_stop_select_log` instead. */
static void *async_data(const host_state *st) {
    return st->async_null ? NULL : (void *) st;
}
/* The driver "ioctl" behind `port_control/3` (see the command vocabulary in the file header). */
static ErlDrvSSizeT host_control(ErlDrvData drv_data, unsigned int command,
                                 char *buf, ErlDrvSizeT len,
                                 char **rbuf, ErlDrvSizeT rlen) {
    host_state *st = (host_state *) drv_data;
    switch (command) {
    case 1: {
        ErlDrvSizeT n = (len < rlen) ? len : rlen;
        if (n > 0 && rbuf != NULL && *rbuf != NULL && buf != NULL)
            memcpy(*rbuf, buf, (size_t) n);
        return (ErlDrvSSizeT) n;
    }
    case 2:
        return (ErlDrvSSizeT) driver_set_timer(st->port, (unsigned long) buf_int(buf, len, 100));
    case 3:
        return (ErlDrvSSizeT) driver_cancel_timer(st->port);
    case 4:
        if (st->fd < 0)
            return (ErlDrvSSizeT) -1;
        return (ErlDrvSSizeT) driver_select(st->port, (ErlDrvEvent) (intptr_t) st->fd,
                                            ERL_DRV_READ | ERL_DRV_USE, 1);
    case 5: {
        ErlDrvTermData spec[] = {
            ERL_DRV_ATOM, driver_mk_atom("host_driver"),
            ERL_DRV_INT, 1,
            ERL_DRV_TUPLE, 2
        };
        return (ErlDrvSSizeT) erl_drv_output_term(driver_mk_port(st->port), spec, 6);
    }
    case 6:
        return (ErlDrvSSizeT) driver_async(st->port, NULL, host_async_invoke, st, NULL);
    case 7:
        if (st->fd < 0)
            return (ErlDrvSSizeT) -1;
        return (ErlDrvSSizeT) driver_select(st->port, (ErlDrvEvent) (intptr_t) st->fd,
                                            ERL_DRV_READ, 0);
    case 8: {
        unsigned long ms = 0;
        if (driver_read_timer(st->port, &ms) != 0)
            return (ErlDrvSSizeT) -1;
        return (ErlDrvSSizeT) ms;
    }
    case 9:
        if (st->fd < 0)
            return (ErlDrvSSizeT) -1;
        return (ErlDrvSSizeT) driver_select(st->port, (ErlDrvEvent) (intptr_t) st->fd,
                                            ERL_DRV_READ | ERL_DRV_USE, 1);
    case 10:
        if (st->fd < 0)
            return (ErlDrvSSizeT) -1;
        return (ErlDrvSSizeT) driver_select(st->port, (ErlDrvEvent) (intptr_t) st->fd,
                                            ERL_DRV_READ | ERL_DRV_USE_NO_CALLBACK, 1);
    case 11:
        if (st->fd < 0)
            return (ErlDrvSSizeT) -1;
        return (ErlDrvSSizeT) driver_select(st->port, (ErlDrvEvent) (intptr_t) st->fd,
                                            ERL_DRV_READ, 1);
    case 12:
        if (st->fd < 0)
            return (ErlDrvSSizeT) -1;
        return (ErlDrvSSizeT) driver_select(st->port, (ErlDrvEvent) (intptr_t) st->fd,
                                            ERL_DRV_READ, 0);
    case 13: {
        /* Answer a fixed reply by pointing `*rbuf` at the driver's own memory (the C contract:
         * the driver may keep the pointer valid until the next `control` call). */
        static const char reply[] = "host-reply";
        if (rbuf != NULL)
            *rbuf = (char *) (intptr_t) reply;
        return (ErlDrvSSizeT) (sizeof(reply) - 1);
    }
    case 14:
        /* A job WITH an `async_free`: on the default build the entry has a `ready_async`, so the
         * completion goes there and the free never runs; on a build whose entry leaves
         * `ready_async` `NULL` (`host_driver_no_ready_async.c`) the host runs the free instead. */
        return (ErlDrvSSizeT) driver_async(st->port, NULL, host_async_invoke,
                                           async_data(st), host_async_free);
    case 15:
        /* The fire-and-forget form: a `NULL` invoke, so no `ready_async` can ever run and the host
         * must release the data through the `async_free` right away. */
        return (ErlDrvSSizeT) driver_async(st->port, NULL, NULL, async_data(st), host_async_free);
    case 16:
        /* A slow job, so a host can drop the port while the job is in flight. */
        return (ErlDrvSSizeT) driver_async(st->port, NULL, host_async_slow_invoke,
                                           async_data(st), host_async_free);
    case 17:
        /* Re-selection that omits `ERL_DRV_USE`: it must not clear the release obligation the
         * first select took on. Returns the second select's host code. */
        if (st->fd < 0)
            return (ErlDrvSSizeT) -1;
        (void) driver_select(st->port, (ErlDrvEvent) (intptr_t) st->fd,
                             ERL_DRV_READ | ERL_DRV_USE, 1);
        return (ErlDrvSSizeT) driver_select(st->port, (ErlDrvEvent) (intptr_t) st->fd,
                                            ERL_DRV_READ, 1);
    case 18:
        /* Re-selection that declines the callback (`ERL_DRV_USE_NO_CALLBACK`): it clears the
         * obligation the first select took on. Returns the second select's host code. */
        if (st->fd < 0)
            return (ErlDrvSSizeT) -1;
        (void) driver_select(st->port, (ErlDrvEvent) (intptr_t) st->fd,
                             ERL_DRV_READ | ERL_DRV_USE, 1);
        return (ErlDrvSSizeT) driver_select(st->port, (ErlDrvEvent) (intptr_t) st->fd,
                                            ERL_DRV_READ | ERL_DRV_USE_NO_CALLBACK, 1);
    case 19: {
        /* `driver_enq`: append the control argument bytes to the driver queue. */
        char out[64];
        snprintf(out, sizeof(out), "enq=%d", driver_enq(st->port, buf, len));
        return reply_prefixed(rbuf, rlen, out, NULL, 0);
    }
    case 20: {
        /* `driver_sizeq`: how many bytes are on the driver queue. */
        char out[64];
        snprintf(out, sizeof(out), "size=%lu", (unsigned long) driver_sizeq(st->port));
        return reply_prefixed(rbuf, rlen, out, NULL, 0);
    }
    case 21: {
        /* `driver_peekq`: the head iovec view. Report the entry count the host publishes
         * (`vlen`) and then the bytes themselves. */
        char out[64];
        char head[512];
        int vlen = -2;
        ErlDrvSizeT n;
        SysIOVec *iov = driver_peekq(st->port, &vlen);
        n = gather_iov(head, sizeof(head), iov, vlen);
        snprintf(out, sizeof(out), "vlen=%d bytes=", vlen);
        return reply_prefixed(rbuf, rlen, out, head, n);
    }
    case 22: {
        /* `driver_deq`: drop `n` bytes (an `int` argument, default 0) from the head. Report the
         * C's return — the new queue size, or `(ErlDrvSizeT)-1` when `n` exceeds the queue. */
        char out[80];
        ErlDrvSizeT left = driver_deq(st->port, (ErlDrvSizeT) buf_int(buf, len, 0));
        snprintf(out, sizeof(out), "deq=%lu size=%lu",
                 (unsigned long) left, (unsigned long) driver_sizeq(st->port));
        return reply_prefixed(rbuf, rlen, out, NULL, 0);
    }
    case 23: {
        /* `driver_peekqv`: the vector form of the head view. Report `vsize`, the returned total
         * size and the bytes. */
        char out[64];
        char head[512];
        ErlIOVec ev = {0, 0, NULL, NULL};
        ErlDrvSizeT n = driver_peekqv(st->port, &ev);
        ErlDrvSizeT got = gather_iov(head, sizeof(head), ev.iov, (int) ev.vsize);
        snprintf(out, sizeof(out), "qvsize=%d qsize=%lu bytes=",
                 (int) ev.vsize, (unsigned long) n);
        return reply_prefixed(rbuf, rlen, out, head, got);
    }
    case 24:
        /* Arm the **write** direction on the configured fd, so a write-readiness event schedules
         * an `OUTPUT` task (the `PortArms` gate). Returns the host's code. */
        if (st->fd < 0)
            return (ErlDrvSSizeT) -1;
        return (ErlDrvSSizeT) driver_select(st->port, (ErlDrvEvent) (intptr_t) st->fd,
                                            ERL_DRV_WRITE, 1);
    case 25:
        /* `set_busy_port(port, 1)`: mark the port busy. The C returns `void`, so answer the
         * `cres == 0` reply (the empty list). */
        set_busy_port(st->port, 1);
        return (ErlDrvSSizeT) 0;
    case 26:
        /* `set_busy_port(port, 0)`: clear the port's busy state (the same empty reply). */
        set_busy_port(st->port, 0);
        return (ErlDrvSSizeT) 0;
    case 27:
        /* The `cres > 0` reply with `*rbuf` set to `NULL`: the C reports the **empty list** for it
         * (`erts/doc/references/driver_entry.md` › *control*), never `badarg`. */
        if (rbuf != NULL)
            *rbuf = NULL;
        return (ErlDrvSSizeT) 5;
    default:
        /* An unrecognised request: a negative `cres`, which `port_control/3` reports as `badarg`. */
        return (ErlDrvSSizeT) -1;
    }
}
static ErlDrvEntry host_driver_entry = {
    .init = NULL,
    .start = host_start,
    .stop = host_stop,
    .output = host_output,
    .ready_input = host_ready_input,
    .ready_output = host_ready_output,
    .driver_name = "host_driver",
    .finish = NULL,
    .handle = NULL,
    .control = host_control,
    .timeout = host_timeout,
    .outputv = HOST_DRIVER_OUTPUTV_SLOT,
    .ready_async = HOST_DRIVER_READY_ASYNC_SLOT,
    .flush = NULL,
    .call = NULL,
    .unused_event_callback = NULL,
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
    .driver_flags = 0,
    .handle2 = NULL,
    .process_exit = HOST_DRIVER_PROCESS_EXIT_SLOT,
    .stop_select = host_stop_select,
    .emergency_close = HOST_DRIVER_EMERGENCY_CLOSE_SLOT,
};

DRIVER_INIT(host_driver) {
    return &host_driver_entry;
}
