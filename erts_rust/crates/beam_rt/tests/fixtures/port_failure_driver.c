/*
 * A driver that closes its **own** port from inside its `output` callback — the fixture for
 * `beam_rt/tests/port_failure.rs` (contract A.4's `driver_failure*`/`driver_exit` family).
 *
 * Each one-byte command selects one row. Row 1 is the ordering row: it writes a read with
 * `driver_output/3` and *then* fails the port, so the owner is owed that data **before** the
 * exit signal. The oracle's order for row 1 is `{Port, {data, <<"hi">>}}` then
 * `{'EXIT', Port, 9}` (`erts/emulator/beam/io.c`'s `driver_failure_term` flushes the port's
 * output queue before the signal is built).
 *
 *  1  driver_output(port, "hi", 2) ; driver_failure(port, 9)      %% data, then exit 9
 *  2  driver_failure_eof(port)                                   %% exit normal
 *  3  driver_failure_atom(port, "boom")                          %% exit boom
 *  4  driver_failure_posix(port, EACCES)                         %% exit eacces
 *  5  driver_failure(port, 7)                                    %% exit 7 (a bare integer)
 *  6  driver_failure(port, 0)                                    %% exit normal
 *  7  driver_exit(port, 0)                                       %% no exit signal at all
 *  8  driver_exit(port, EACCES)                                  %% no exit signal, just 'DOWN'
 *
 * `driver_exit/2` is the `signals_owner == false` arm: it removes the port without telling the
 * owner (only a monitor is told), which is why rows 7 and 8 must never produce an `'EXIT'`.
 */
#include <erl_driver.h>
#include <errno.h>

typedef struct {
    ErlDrvPort port;
} pf_state;

static ErlDrvData pf_start(ErlDrvPort port, char *command) {
    pf_state *st;
    (void) command;
    st = (pf_state *) driver_alloc(sizeof(pf_state));
    if (st == NULL) {
        return ERL_DRV_ERROR_GENERAL;
    }
    /* The port handle the driver's own `driver_output`/`driver_failure*` calls need. */
    st->port = port;
    return (ErlDrvData) st;
}

static void pf_stop(ErlDrvData d) {
    driver_free((void *) d);
}

static void pf_output(ErlDrvData d, char *buf, ErlDrvSizeT len) {
    pf_state *st = (pf_state *) d;
    if (st == NULL || len == 0) {
        return;
    }
    switch ((unsigned char) buf[0]) {
    case 1:
        driver_output(st->port, "hi", 2);
        (void) driver_failure(st->port, 9);
        break;
    case 2:
        (void) driver_failure_eof(st->port);
        break;
    case 3:
        (void) driver_failure_atom(st->port, "boom");
        break;
    case 4:
        (void) driver_failure_posix(st->port, EACCES);
        break;
    case 5:
        (void) driver_failure(st->port, 7);
        break;
    case 6:
        (void) driver_failure(st->port, 0);
        break;
    case 7:
        (void) driver_exit(st->port, 0);
        break;
    case 8:
        (void) driver_exit(st->port, EACCES);
        break;
    default:
        break;
    }
}

static ErlDrvEntry pf_entry = {
    .init = NULL,
    .start = pf_start,
    .stop = pf_stop,
    .output = pf_output,
    .ready_input = NULL,
    .ready_output = NULL,
    .driver_name = "port_failure_driver",
    .finish = NULL,
    .handle = NULL,
    .control = NULL,
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

DRIVER_INIT(port_failure_driver_init) {
    return &pf_entry;
}
