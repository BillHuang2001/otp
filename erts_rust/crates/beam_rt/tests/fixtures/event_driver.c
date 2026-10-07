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
 * `DRIVER_INIT(event)` exports `driver_init` returning the entry below, whose `driver_name` is
 * "event", so the library is loadable only under that name — the file stem `{spawn_driver, Path}`
 * derives it from (see `beam_rt::port::driver_lookup`).
 *
 * Portability note: this fixture references ONLY `driver_*` symbols whose `ErlDrvApi` slots are
 * populated on stable Rust (alloc/free/output/timers/select). A reference to an unimplemented slot
 * would make `dlopen(RTLD_NOW)` fail.
 */
#include <erl_driver.h>

#include <stdint.h>
#include <unistd.h>

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
 * selected read end readable. `output` has no port argument, so the state's port is used. */
static void event_output(ErlDrvData drv_data, char *buf, ErlDrvSizeT len) {
    event_state *st = (event_state *) drv_data;
    ssize_t written;
    if (st == NULL || len == 0)
        return;
    written = write(st->write_end, buf, 1);
    (void) written;
}

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

static ErlDrvEntry event_driver_entry = {
    .init = NULL,
    .start = event_start,
    .stop = event_stop,
    .output = event_output,
    .ready_input = event_ready_input,
    .ready_output = NULL,
    .driver_name = "event",
    .finish = NULL,
    .handle = NULL,
    .control = NULL,
    .timeout = event_timeout,
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

DRIVER_INIT(event) {
    return &event_driver_entry;
}
