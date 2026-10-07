/*
 * A REAL driver shared object — the end-to-end fixture of the `beam_abi` driver C ABI
 * (workspace contract A.4).
 *
 * It is compiled by the integration test `tests/driver_end_to_end.rs` with the system `cc`
 * against the system `erl_driver.h` and loaded through `beam_abi::driver::loader::load`. Its
 * `driver_*` / `erl_drv_*` imports are deliberately left undefined by the linker: they are
 * resolved (`RTLD_NOW`) out of the *loading process*'s dynamic symbol table, which the test
 * binary publishes via `-Wl,--export-dynamic` (`build.rs`). A successful load is therefore the
 * empirical proof that the Rust `driver_*` host functions are exported as dynamic symbols and
 * bind a real C driver.
 *
 * `DRIVER_INIT(t)` exports `driver_init` returning the `ErlDrvEntry` below, whose `driver_name`
 * is `"t"`, so the only name this library may be loaded as is `t`.
 *
 * Portability note: this fixture references ONLY `driver_*` symbols whose `ErlDrvApi` slots are
 * populated on stable Rust (alloc/free/output/timers/select/async/term-data/system-info). A
 * reference to an unimplemented slot would make `dlopen(RTLD_NOW)` fail.
 */
#include <erl_driver.h>
#include <string.h>

/* Private per-port state: the port handle (echoed later by `output`) plus a `timeout` counter. */
typedef struct {
    ErlDrvPort port;
    int timeouts;
} t_state;

static ErlDrvData t_start(ErlDrvPort port, char *command) {
    t_state *st;
    (void) command;
    st = (t_state *) driver_alloc(sizeof(t_state));
    if (st == NULL)
        return (ErlDrvData) -1;
    st->port = port;
    st->timeouts = 0;
    return (ErlDrvData) st;
}

static void t_stop(ErlDrvData drv_data) {
    if (drv_data != NULL)
        driver_free(drv_data);
}

/* Echo whatever Erlang sends to the port straight back to the port owner. `output` has no port
 * argument, so the port stored at `start` is used. */
static void t_output(ErlDrvData drv_data, char *buf, ErlDrvSizeT len) {
    t_state *st = (t_state *) drv_data;
    if (st != NULL)
        (void) driver_output(st->port, buf, len);
}

/* The driver "ioctl" behind `port_control/3`. */
static ErlDrvSSizeT t_control(ErlDrvData drv_data, unsigned int command,
                              char *buf, ErlDrvSizeT len,
                              char **rbuf, ErlDrvSizeT rlen) {
    t_state *st = (t_state *) drv_data;
    switch (command) {
    case 0:
        return 0;
    case 1: {
        /* Echo up to `rlen` received bytes back through the reply buffer. */
        ErlDrvSizeT n = (len < rlen) ? len : rlen;
        if (n > 0 && rbuf != NULL && *rbuf != NULL)
            memcpy(*rbuf, buf, (size_t) n);
        return (ErlDrvSSizeT) n;
    }
    case 2:
        /* Schedule the `timeout` callback: the host's code (0 when it has timers). */
        return (ErlDrvSSizeT) driver_set_timer(st->port, 100);
    case 3:
        /* Register an event: the host's code (0 when it has a poller). */
        return (ErlDrvSSizeT) driver_select(st->port, (ErlDrvEvent) 0,
                                            ERL_DRV_READ | ERL_DRV_USE, 1);
    case 4:
        /* Cancel the timer: the host's code. */
        return (ErlDrvSSizeT) driver_cancel_timer(st->port);
    case 5: {
        /* Output the 2-tuple {t, 1} through the term-data path. */
        ErlDrvTermData spec[] = {
            ERL_DRV_ATOM, driver_mk_atom("t"),
            ERL_DRV_INT, 1,
            ERL_DRV_TUPLE, 2
        };
        return (ErlDrvSSizeT) erl_drv_output_term(driver_mk_port(st->port), spec, 6);
    }
    case 6: {
        /* Fill a local ErlDrvSysInfo and report the driver ABI major version it carries. */
        ErlDrvSysInfo si;
        memset(&si, 0, sizeof(si));
        driver_system_info(&si, sizeof(si));
        return (ErlDrvSSizeT) si.driver_major_version;
    }
    case 7:
        /* Schedule an async job: the host's code (negative when it has no async pool). */
        return (ErlDrvSSizeT) driver_async(st->port, NULL, NULL, NULL, NULL);
    default:
        return 0;
    }
}

/* A fired timeout: bump the counter and emit a marker so the callback is observable. */
static void t_timeout(ErlDrvData drv_data) {
    t_state *st = (t_state *) drv_data;
    if (st != NULL) {
        st->timeouts++;
        (void) driver_output(st->port, "T", 1);
    }
}

static ErlDrvEntry t_driver_entry = {
    .init = NULL,
    .start = t_start,
    .stop = t_stop,
    .output = t_output,
    .ready_input = NULL,
    .ready_output = NULL,
    .driver_name = "t",
    .finish = NULL,
    .handle = NULL,
    .control = t_control,
    .timeout = t_timeout,
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

DRIVER_INIT(t) {
    return &t_driver_entry;
}
