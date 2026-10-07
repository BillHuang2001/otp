/*
 * A REAL driver shared object — the end-to-end fixture of the `beam_abi` port-data-lock (PDL) /
 * driver-monitor / deprecated-wall-clock family of the driver C ABI (workspace contract A.4).
 *
 * It is compiled by the integration test `tests/driver_pdl.rs` with the system `cc` against the
 * system `erl_driver.h` and loaded through `beam_abi::driver::loader::load`. Its `driver_*`
 * imports are deliberately left undefined by the linker: they are resolved (`RTLD_NOW`) out of the
 * *loading process*'s dynamic symbol table, which the test binary publishes via
 * `-Wl,--export-dynamic` (`build.rs`). A successful load is therefore the empirical proof that the
 * Rust `driver_pdl_*` / `driver_monitor_*` / `driver_get_now` host functions are exported as
 * dynamic symbols and bind a real C driver.
 *
 * `DRIVER_INIT(pdl)` exports `driver_init` returning the `ErlDrvEntry` below, whose `driver_name`
 * is `"pdl"`, so the only name this library may be loaded as is `pdl`.
 *
 * # The commands
 *
 * `control` returns a number per command — the value the family produced — so the Rust test can
 * assert the *exact* C-level observable. Where a structured value is needed (the
 * `ErlDrvNowData` breakdown) the reply buffer is used, exactly as the header documents for
 * `port_control/3`.
 *
 * Portability note: this fixture references ONLY `driver_*` symbols whose `ErlDrvApi` slots are
 * populated on stable Rust (alloc/free/output/caller/term-nil/PDL/monitors/now). A reference to an
 * unimplemented slot would make `dlopen(RTLD_NOW)` fail.
 *
 * Oracle note: every expected value below is the literal the legacy ERTS returns
 * (`erts/emulator/beam/io.c`): `driver_pdl_create` → the lock or `NULL` (`io.c:6717`),
 * `driver_pdl_get_refc` → `pdl_read_refc` (`io.c:6759`; a fresh PDL is 1, `io.c:6726`, and `lock`
 * raises it before locking, `io.c:6736`), `driver_monitor_process` → `0` filled / `> 0` not alive /
 * `< 0` no callback or invalid port (`io.c:7020-7055`), `driver_demonitor_process` → `0` removed /
 * `1` no longer exists / `-1` invalid port (`io.c:7074-7080`), `driver_get_monitored_process` →
 * the term data or `driver_term_nil` (`io.c:7113`), `driver_compare_monitors` → the `memcmp` of
 * two reference things (`io.c:7142`), and `driver_get_now` → `0` filled / `-1` for a `NULL`
 * pointer (`io.c:6957`).
 */
#include <erl_driver.h>
#include <string.h>

/* Private per-port state: the port handle and the port's single data lock + monitors. */
typedef struct {
    ErlDrvPort port;
    ErlDrvPDL pdl;
    ErlDrvMonitor mon1;
    ErlDrvMonitor mon2;
} pdl_state;

static ErlDrvData pdl_start(ErlDrvPort port, char *command) {
    pdl_state *st;
    (void) command;
    st = (pdl_state *) driver_alloc(sizeof(pdl_state));
    if (st == NULL)
        return (ErlDrvData) -1;
    st->port = port;
    st->pdl = NULL;
    memset(&st->mon1, 0, sizeof(st->mon1));
    memset(&st->mon2, 0, sizeof(st->mon2));
    return (ErlDrvData) st;
}

static void pdl_stop(ErlDrvData drv_data) {
    if (drv_data != NULL)
        driver_free(drv_data);
}

/* Echo whatever Erlang sends to the port straight back (the standard driver idiom). */
static void pdl_output(ErlDrvData drv_data, char *buf, ErlDrvSizeT len) {
    pdl_state *st = (pdl_state *) drv_data;
    if (st != NULL)
        (void) driver_output(st->port, buf, len);
}

static ErlDrvSSizeT pdl_control(ErlDrvData drv_data, unsigned int command,
                                char *buf, ErlDrvSizeT len,
                                char **rbuf, ErlDrvSizeT rlen) {
    pdl_state *st = (pdl_state *) drv_data;
    (void) buf;
    (void) len;

    switch (command) {
    case 0:
        /* The documented no-op. */
        return 0;

    case 1:
        /* Create the port's data lock; report whether the handle is non-NULL. */
        st->pdl = driver_pdl_create(st->port);
        return (ErlDrvSSizeT) (st->pdl != NULL);

    case 2: {
        /* A second create for the same port must be refused (the C's `pp->port_data_lock`). */
        ErlDrvPDL again = driver_pdl_create(st->port);
        return (ErlDrvSSizeT) (again == NULL);
    }

    case 3:
        /* An invalid port cannot have a data lock: `driver_pdl_create` returns NULL. */
        return (ErlDrvSSizeT) (driver_pdl_create(NULL) == NULL);

    case 4:
        /* A fresh PDL has exactly one reference. */
        return (ErlDrvSSizeT) driver_pdl_get_refc(st->pdl);

    case 5:
        /* `inc_refc` reports the value reached. */
        return (ErlDrvSSizeT) driver_pdl_inc_refc(st->pdl);

    case 6:
        /* `dec_refc` reports the value reached. */
        return (ErlDrvSSizeT) driver_pdl_dec_refc(st->pdl);

    case 7:
        /* `driver_pdl_lock` raises the count *before* locking, so the count reads one higher for
         * the duration of the critical section. */
        driver_pdl_lock(st->pdl);
        return (ErlDrvSSizeT) driver_pdl_get_refc(st->pdl);

    case 8:
        /* `driver_pdl_unlock` unlocks *then* lowers the count. */
        driver_pdl_unlock(st->pdl);
        return (ErlDrvSSizeT) driver_pdl_get_refc(st->pdl);

    case 9:
        /* `NULL` handles are no-ops for lock/unlock and report the crate's "not one of mine"
         * reference count rather than dereferencing the handle. */
        driver_pdl_lock(NULL);
        driver_pdl_unlock(NULL);
        return (ErlDrvSSizeT) driver_pdl_get_refc(NULL);

    case 10:
        /* A handle this runtime never issued is refused the same way — never dereferenced. */
        return (ErlDrvSSizeT) driver_pdl_get_refc((ErlDrvPDL) (unsigned long) 0xdeadbeefUL);

    case 11:
        memset(&st->mon1, 0, sizeof(st->mon1));
        return (ErlDrvSSizeT) driver_monitor_process(st->port, driver_caller(st->port),
                                                     &st->mon1);

    case 12:
        memset(&st->mon2, 0, sizeof(st->mon2));
        return (ErlDrvSSizeT) driver_monitor_process(st->port, driver_caller(st->port),
                                                     &st->mon2);

    case 13: {
        /* An operand that cannot name a process is the positive "no longer alive" code. */
        ErlDrvMonitor mon;
        memset(&mon, 0, sizeof(mon));
        return (ErlDrvSSizeT) driver_monitor_process(st->port, driver_term_nil, &mon);
    }

    case 14:
        /* A missing out-parameter is the negative failure code. */
        return (ErlDrvSSizeT) driver_monitor_process(st->port, driver_caller(st->port), NULL);

    case 15: {
        ErlDrvMonitor mon;
        memset(&mon, 0, sizeof(mon));
        /* An invalid port is the negative failure code as well. */
        return (ErlDrvSSizeT) driver_monitor_process(NULL, driver_caller(st->port), &mon);
    }

    case 16:
        /* The monitor reports the process it was created for. */
        return (ErlDrvSSizeT) (driver_get_monitored_process(st->port, &st->mon1)
                               == driver_caller(st->port));

    case 17: {
        /* A monitor this runtime never issued reports `driver_term_nil`. */
        ErlDrvMonitor junk;
        memset(&junk, 0, sizeof(junk));
        return (ErlDrvSSizeT) (driver_get_monitored_process(st->port, &junk)
                               == driver_term_nil);
    }

    case 18:
        /* A monitor compares equal to itself (the C's `memcmp` of the reference thing). */
        return (ErlDrvSSizeT) driver_compare_monitors(&st->mon1, &st->mon1);

    case 19:
        /* Two monitors of the same process carry distinct reference things, so they differ. */
        return (ErlDrvSSizeT) (driver_compare_monitors(&st->mon1, &st->mon2) != 0);

    case 20:
        /* Removing a live monitor reports success. */
        return (ErlDrvSSizeT) driver_demonitor_process(st->port, &st->mon1);

    case 21:
        /* Removing it twice reports "no longer exists" (a positive code). */
        return (ErlDrvSSizeT) driver_demonitor_process(st->port, &st->mon1);

    case 22: {
        /* An invalid port and a missing monitor are both the negative failure code. */
        int a = driver_demonitor_process(NULL, &st->mon2);
        int b = driver_demonitor_process(st->port, NULL);
        return (ErlDrvSSizeT) ((a == -1 && b == -1) ? 1 : 0);
    }

    case 23: {
        /* The deprecated wall clock: fill a local `ErlDrvNowData` and copy its three fields into
         * the reply buffer (the `port_control/3` way to return a structure). */
        ErlDrvNowData now;
        int rc;
        memset(&now, 0, sizeof(now));
        rc = driver_get_now(&now);
        if (rc != 0)
            return (ErlDrvSSizeT) rc;
        if (rbuf == NULL || *rbuf == NULL || rlen < sizeof(now))
            return -2;
        memcpy(*rbuf, &now, sizeof(now));
        return (ErlDrvSSizeT) sizeof(now);
    }

    case 24:
        /* A NULL out-parameter is the C's `-1`. */
        return (ErlDrvSSizeT) driver_get_now(NULL);

    default:
        /* An unknown command is visible as a distinct negative marker. */
        return -100;
    }
}

static ErlDrvEntry pdl_driver_entry = {
    .init = NULL,
    .start = pdl_start,
    .stop = pdl_stop,
    .output = pdl_output,
    .ready_input = NULL,
    .ready_output = NULL,
    .driver_name = "pdl",
    .finish = NULL,
    .handle = NULL,
    .control = pdl_control,
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

DRIVER_INIT(pdl) {
    return &pdl_driver_entry;
}
