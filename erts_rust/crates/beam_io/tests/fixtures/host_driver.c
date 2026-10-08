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
 * implemented today (alloc/free, output, output-term, select, timers, async). Referencing an
 * unimplemented slot would make `dlopen(RTLD_NOW)` fail, so the fixture deliberately keeps to
 * that subset.
 *
 * ## The start command
 *
 * `start(port, command)` parses a simple, whitespace/comma separated command string:
 *
 *   * `fd=<n>`   — the descriptor the port will `driver_select` on (default: none, `-1`);
 *   * `log=<path>` — a file every callback appends a line to (`start`, `output`, `stop`),
 *     so a host can observe the *order* in which the driver's callbacks ran; when absent no
 *     file is ever touched (the pre-existing tests are unaffected);
 *   * `upper`     — a mode token: `output` upper-cases the bytes it echoes back.
 *
 * Anything else is ignored. The parsed state is stored in the per-port `host_state`.
 *
 * ## The callback log
 *
 * When `log=<path>` is given, `start`, `output` and `stop` each append one line (`"start"`,
 * `"output"`, `"stop"`) to that file, in the order they ran. That is the observable evidence
 * a port created from the process-global driver table really ran the driver's `start`/`stop`
 * callbacks, in order, around its data path.
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
 *
 * Any other command returns `-1`, the C's negative `cres` (`port_control/3` answers `badarg`,
 * `io.c:4074`).
 *
 * A fired timer (`timeout`) and readable input (`ready_input`) are each turned into
 * `driver_output` traffic so a hosted port can observe them.
 *
 * ## `stop_select`
 *
 * The driver implements `stop_select`, the `ERL_DRV_USE` release callback, as `host_stop_select`.
 * Its signature (`void(ErlDrvEvent, void*)`) carries neither `ErlDrvData` nor the port, so it
 * cannot reach the per-port `log=` path from its arguments; the fixture keeps the most recently
 * started port's path in the file-scope `g_stop_select_log` instead, and appends `"stop_select"`
 * to it. A test therefore drives one port at a time.
 */
#include <erl_driver.h>

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

/* Private per-port state, allocated in `start` and freed in `stop`. */
typedef struct {
    ErlDrvPort port;   /* the port handle `output`/`timeout` need (they get no port argument) */
    int fd;            /* the descriptor `driver_select` is armed on, or -1 when none given */
    int timeouts;      /* how many times `timeout` fired */
    int reads;         /* how many times `ready_input` fired */
    char mode[16];     /* the mode token from the command string ("" when none) */
    char log[512];     /* path every callback appends an event line to ("" when none) */
} host_state;

/* The `log=` path `stop_select` logs to. `stop_select` gets no `ErlDrvData` (see the file
 * header), so the path is published process-wide by the most recent `start` instead. */
static char g_stop_select_log[512] = "";

/* Append `what` as one line to the per-port log file, if the command asked for one. */
static void log_event(const host_state *st, const char *what) {
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

/* Parse `fd=<n>`, `log=<path>` and a bare mode token out of the whitespace/comma separated
 * command string. */
static void parse_command(host_state *st, const char *command) {
    const char *p = command;
    st->fd = -1;
    st->mode[0] = '\0';
    st->log[0] = '\0';
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
        } else {
            const char *s = p;
            size_t n = 0;
            while (s[n] != '\0' && s[n] != ' ' && s[n] != '\t' && s[n] != ',')
                n++;
            copy_token(st->mode, sizeof(st->mode), s, n);
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

static ErlDrvData host_start(ErlDrvPort port, char *command) {
    host_state *st = (host_state *) driver_alloc(sizeof(host_state));
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
    log_event(st, "start");
    return (ErlDrvData) st;
}

static void host_stop(ErlDrvData drv_data) {
    host_state *st = (host_state *) drv_data;
    if (st == NULL)
        return;
    log_event(st, "stop");
    driver_free(drv_data);
}

/* The `ERL_DRV_USE` release callback (`stop_select`): the driver is told a `driver_select`ed
 * handle is really gone, so it may close or recycle it. The callback receives neither
 * `ErlDrvData` nor the port, so it logs through the path the most recent `start` published (see
 * `g_stop_select_log` and the file header). */
static void host_stop_select(ErlDrvEvent event, void *reserved) {
    FILE *f;
    (void) event;
    (void) reserved;
    if (g_stop_select_log[0] == '\0')
        return;
    f = fopen(g_stop_select_log, "a");
    if (f == NULL)
        return;
    fprintf(f, "stop_select\n");
    fclose(f);
}

/* Echo whatever Erlang sends to the port back to its owner. `output` receives no port argument,
 * so the handle stored at `start` is used. In `upper` mode the echoed bytes are upper-cased, so
 * the parsed command token is observable. */
static void host_output(ErlDrvData drv_data, char *buf, ErlDrvSizeT len) {
    host_state *st = (host_state *) drv_data;
    if (st == NULL)
        return;
    log_event(st, "output");
    if (strcmp(st->mode, "upper") == 0 && buf != NULL) {
        ErlDrvSizeT i;
        for (i = 0; i < len; i++)
            if (buf[i] >= 'a' && buf[i] <= 'z')
                buf[i] = (char) (buf[i] - 'a' + 'A');
    }
    (void) driver_output(st->port, buf, len);
}

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

/* The async job (runs on a worker thread): emit a marker through the port handle. */
static void host_async_invoke(void *data) {
    host_state *st = (host_state *) data;
    if (st != NULL)
        (void) driver_output(st->port, "A", 1);
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
    .stop_select = host_stop_select,
    .emergency_close = NULL,
};

DRIVER_INIT(host_driver) {
    return &host_driver_entry;
}
