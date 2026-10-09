/*
 * The **event and timer** driver fixture for `beam_rt`'s A.4 end-to-end test
 * (`tests/driver.rs`). It is a real driver shared object, compiled with the host `cc` against
 * the host `erl_driver.h`, and `dlopen`ed like any other driver.
 *
 * It exercises the two `DriverHost` services that are served by the runtime's poller rather than
 * by a value the host can capture:
 *
 *   1. `driver_select(port, (ErlDrvEvent) read_end, ERL_DRV_READ, 1)` in `start` — the read end of
 *      an internal `pipe(2)`. When Erlang sends anything through `port_command`, `output` writes
 *      one byte to the write end, which makes the selected descriptor readable; the runtime's
 *      poller then schedules the port's INPUT task, which runs `ready_input` **with the descriptor
 *      the driver selected** (`erl_check_io.c`: `(ErlDrvEvent) state->fd`). `ready_input` drains
 *      that one byte (the poller is edge-triggered — a byte left unread would never be re-notified)
 *      and answers with `driver_output(port, "R", 1)`.
 *
 *   2. `driver_set_timer(port, EVENT_TIMER_MS)` in `start` — a real port timer. The runtime mirrors
 *      the deadline into the port's TIMEOUT task and clamps its poll round by it, so the driver's
 *      `timeout` callback runs **at** that deadline and answers with `driver_output(port, "T", 1)`.
 *
 * Both bytes therefore reach the port's owner as `{Port, {data, …}}` messages: "R" only if the
 * readiness path really ran the driver's `ready_input`, and "T" only if the timer really fired.
 *
 * The select is armed **before** anything is written, so the edge the write produces cannot be
 * missed, and `start` fails the port if either request is refused by the host (`0` = accepted,
 * the C's failure code otherwise) — a port whose arms are only half-applied must not look healthy.
 *
 * `DRIVER_INIT(EVENT_DRIVER_INIT)` exports `driver_init` returning the entry below, whose
 * `driver_name` is "event" (in the committed mode), so the library is loadable only under that name
 * — the file stem `{spawn_driver, Path}` derives it from (see `beam_rt::port::driver_lookup`). The
 * mode macros below select two further entry shapes from the same source.
 *
 * Portability note: this fixture references ONLY `driver_*` / `erl_drv_*` symbols whose `ErlDrvApi`
 * slots are populated on stable Rust (alloc/free/output/timers/select/term-data/mk-atom/mk-port/
 * connected/system-info). A reference to an unimplemented slot would make `dlopen(RTLD_NOW)` fail.
 */
#include <erl_driver.h>

#include <stdint.h>
#include <unistd.h>

/* ---------------------------------------------------------------------------------------------
 * The compile-time modes
 * ---------------------------------------------------------------------------------------------
 *
 * `tests/driver.rs` compiles this one source in four modes, so the three `port_command`-driven
 * entry-point shapes the A.4 boundary supports are each pinned by a real driver object without
 * duplicating the fixture text. The committed source (no `#define` prepended) is **mode 0** — the
 * event/timer driver documented above — and every other mode only adds an `output`/`outputv`
 * variant; `start`/`stop`/`ready_input`/`timeout` are shared verbatim.
 *
 *   0  `output` set, `outputv` NULL: the committed event/timer driver.
 *   1  `outputv` set, `output` NULL: the same `port_command`, answered through the *vector*
 *      callback, which echoes the data block straight back through `driver_output`.
 *   2  `output` answers `erl_drv_output_term` — a term-data spec containing an `ERL_DRV_EXT2TERM`
 *      operand — which is delivered to the port's **owner**.
 *   3  the same term, delivered with `erl_drv_send_term` to `driver_connected`'s pid — the
 *      **explicit** destination.
 *
 * The name a mode declares must equal the stem it is compiled under: the runtime derives the
 * driver name from the loaded path's stem (`beam_rt::port::driver_lookup`), and the loader rejects
 * an entry whose `driver_name` differs (`bad_driver_name`). Mode 0's stem is therefore `event`,
 * mode 1's `eventv`, mode 2's `eventt` and mode 3's `eventp`.
 */
#ifndef EVENT_DRIVER_MODE
#define EVENT_DRIVER_MODE 0
#endif

#ifndef EVENT_DRIVER_NAME
#if EVENT_DRIVER_MODE == 1
#define EVENT_DRIVER_NAME "eventv"
#elif EVENT_DRIVER_MODE == 2
#define EVENT_DRIVER_NAME "eventt"
#elif EVENT_DRIVER_MODE == 3
#define EVENT_DRIVER_NAME "eventp"
#else
#define EVENT_DRIVER_NAME "event"
#endif
#endif

/* `DRIVER_INIT`'s argument is cosmetic in a dynamically loaded driver (`erl_driver.h:319` exports
 * the generic symbol `driver_init` whatever the argument is), but it must still name an
 * identifier. */
#define EVENT_DRIVER_INIT event

/* The driver's port timer, in milliseconds. Small enough to keep the test quick, large enough that
 * the readiness byte (produced by the very next `port_command`) is always delivered first. */
#define EVENT_TIMER_MS 50

/* Private per-port state: the port handle, the two ends of the internal pipe, and the callback
 * counters the test's two observed bytes stand for. */
typedef struct {
    ErlDrvPort port;
    int read_end;
    int write_end;
    int reads;
    int timeouts;
} event_state;

static ErlDrvData event_start(ErlDrvPort port, char *command) {
    int fds[2];
    event_state *st;
    (void) command;
    if (pipe(fds) != 0)
        return (ErlDrvData) -1;
    st = (event_state *) driver_alloc(sizeof(event_state));
    if (st == NULL) {
        close(fds[0]);
        close(fds[1]);
        return (ErlDrvData) -1;
    }
    st->port = port;
    st->read_end = fds[0];
    st->write_end = fds[1];
    st->reads = 0;
    st->timeouts = 0;
    /* Arm the select BEFORE any byte is written: the poller is edge-triggered, so a readiness the
     * port was not watching yet would be lost. */
    if (driver_select(port, (ErlDrvEvent) (intptr_t) st->read_end, ERL_DRV_READ, 1) != 0) {
        driver_free(st);
        close(fds[0]);
        close(fds[1]);
        return (ErlDrvData) -1;
    }
    /* The timer. `driver_set_timer` answers `-1` when the host has no timer (or its entry declares
     * no `timeout` callback), so refusing the port here keeps a half-armed port from looking
     * healthy. */
    if (driver_set_timer(port, EVENT_TIMER_MS) != 0) {
        driver_free(st);
        close(fds[0]);
        close(fds[1]);
        return (ErlDrvData) -1;
    }
    return (ErlDrvData) st;
}

static void event_stop(ErlDrvData drv_data) {
    event_state *st = (event_state *) drv_data;
    if (st == NULL)
        return;
    close(st->read_end);
    close(st->write_end);
    driver_free(st);
}

/* `port_command` reaches this callback: write one byte to the pipe's write end, which makes the
 * selected read end readable. `output` has no port argument, so the state's port is used. Only mode
 * 0 declares this callback (the other modes answer through `outputv` or the term-data path). */
#if EVENT_DRIVER_MODE == 0
static void event_output(ErlDrvData drv_data, char *buf, ErlDrvSizeT len) {
    event_state *st = (event_state *) drv_data;
    ssize_t written;
    if (st == NULL || len == 0)
        return;
    written = write(st->write_end, buf, 1);
    (void) written;
}
#endif

/* Read readiness: drain exactly the byte the readiness was reported for (edge-triggered), then
 * answer through `driver_output`, which is what reaches the port's owner. */
static void event_ready_input(ErlDrvData drv_data, ErlDrvEvent event) {
    event_state *st = (event_state *) drv_data;
    char byte;
    (void) event;
    if (st == NULL)
        return;
    if (read(st->read_end, &byte, 1) != 1)
        return;
    st->reads++;
    (void) driver_output(st->port, "R", 1);
}

/* The port timer fired (at its deadline): answer with the marker byte the test asserts on. */
static void event_timeout(ErlDrvData drv_data) {
    event_state *st = (event_state *) drv_data;
    if (st == NULL)
        return;
    st->timeouts++;
    (void) driver_output(st->port, "T", 1);
}

/* ---------------------------------------------------------------------------------------------
 * Mode 1: the *vector* callback
 * ---------------------------------------------------------------------------------------------
 *
 * The C's own preference when a driver declares both callbacks is the vector one
 * (`if (drv->outputv) …` else `… drv->output`, `io.c:1782`), and an entry that declares **only**
 * `outputv` is not left without an output path at all. The vector mirrors the C's build: `iov[0]`
 * is the (empty) header block, `iov[1]` the data, `size` the data's byte count.
 *
 * Echoing the data block back through `driver_output` makes the byte the owner receives evidence of
 * *both* halves: the callback ran (only `port_command` invokes it) and it really saw the payload it
 * echoed.
 */
#if EVENT_DRIVER_MODE == 1
static void event_outputv(ErlDrvData drv_data, ErlIOVec *ev) {
    event_state *st = (event_state *) drv_data;
    if (st == NULL || ev == NULL || ev->iov == NULL || ev->vsize < 2)
        return;
    if (ev->iov[1].iov_base == NULL || ev->iov[1].iov_len == 0)
        return;
    st->reads++;
    (void) driver_output(st->port, ev->iov[1].iov_base, ev->iov[1].iov_len);
}
#endif

/* ---------------------------------------------------------------------------------------------
 * Modes 2 and 3: the term-data path
 * ---------------------------------------------------------------------------------------------
 *
 * `port_command` reaches `event_output_term`, which decodes a term-data spec and delivers the term
 * itself — never a `{Port, Term}` wrapper (`io.c:6174`). The spec mixes both operand kinds this
 * proof needs: a plain `ERL_DRV_INT` and an `ERL_DRV_EXT2TERM` whose operand is an **External Term
 * Format** image, so the received `{7, 9}` shows the ETF decoder (contract A.3's driver side,
 * `DriverHost::binary_to_term`) was reached *through* the spec decoder.
 *
 * The image is deliberately atom-free (`VERSION_MAGIC` + a `SMALL_INTEGER_EXT`): a *new* atom can
 * only be committed into the runtime's shared table by a port path that does not exist yet, so an
 * image naming one would be decoded into an index the runtime cannot name.
 *
 * Mode 2 targets the port's **owner** (`erl_drv_output_term`, `to = None`); mode 3 names the
 * connected process explicitly (`erl_drv_send_term` with `driver_connected`'s pid term data) — the
 * destination the runtime resolves through `ProcessId::from_eterm`. Either way a **refused**
 * delivery is made visible: the driver answers `"!"` through `driver_output`, which the test can
 * tell apart from the term it expects. Only the failure side is tested (`rc < 0`): the C answers
 * `-1` for a refused delivery and `1` for a delivered one (`io.c`), while this runtime's
 * `DriverHost::send_term` seam answers `0` for a delivered one — both are non-negative, so
 * `rc < 0` is the portable failure test.
 */
#if EVENT_DRIVER_MODE == 2 || EVENT_DRIVER_MODE == 3
/* VERSION_MAGIC (131), SMALL_INTEGER_EXT (97), the value 9. */
static const unsigned char event_term_image[] = { 131, 97, 9 };

static void event_output_term(ErlDrvData drv_data, char *buf, ErlDrvSizeT len) {
    event_state *st = (event_state *) drv_data;
    ErlDrvTermData spec[] = {
        ERL_DRV_INT, 7,
        ERL_DRV_EXT2TERM, (ErlDrvTermData) event_term_image,
        (ErlDrvTermData) sizeof(event_term_image),
        ERL_DRV_TUPLE, 2,
    };
    int rc;
    (void) buf;
    (void) len;
    if (st == NULL)
        return;
#if EVENT_DRIVER_MODE == 3
    rc = erl_drv_send_term(driver_mk_port(st->port), driver_connected(st->port), spec,
                           (int) (sizeof(spec) / sizeof(spec[0])));
#else
    rc = erl_drv_output_term(driver_mk_port(st->port), spec,
                             (int) (sizeof(spec) / sizeof(spec[0])));
#endif
    if (rc < 0)
        (void) driver_output(st->port, "!", 1);
}
#endif

/* The entry's `output`/`outputv` pair per mode (mode 0 keeps the committed event/timer callbacks). */
#if EVENT_DRIVER_MODE == 1
#define EVENT_OUTPUT NULL
#define EVENT_OUTPUTV event_outputv
#elif EVENT_DRIVER_MODE == 2 || EVENT_DRIVER_MODE == 3
#define EVENT_OUTPUT event_output_term
#define EVENT_OUTPUTV NULL
#else
#define EVENT_OUTPUT event_output
#define EVENT_OUTPUTV NULL
#endif

static ErlDrvEntry event_driver_entry = {
    .init = NULL,
    .start = event_start,
    .stop = event_stop,
    .output = EVENT_OUTPUT,
    .ready_input = event_ready_input,
    .ready_output = NULL,
    .driver_name = EVENT_DRIVER_NAME,
    .finish = NULL,
    .handle = NULL,
    .control = NULL,
    .timeout = event_timeout,
    .outputv = EVENT_OUTPUTV,
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

DRIVER_INIT(EVENT_DRIVER_INIT) {
    return &event_driver_entry;
}
