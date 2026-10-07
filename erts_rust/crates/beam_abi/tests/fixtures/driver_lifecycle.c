/*
 * A REAL driver shared object — the end-to-end fixture of the `beam_abi` port-lifecycle, failure
 * and attributes family of the driver C ABI (workspace contract A.4).
 *
 * It is compiled by the integration test `tests/driver_lifecycle.rs` with the system `cc` against
 * the system `erl_driver.h` and loaded through `beam_abi::driver::loader::load`. Its
 * `driver_*` / `erl_drv_*` imports are deliberately left undefined by the linker: they are resolved
 * (`RTLD_NOW`) out of the *loading process*'s dynamic symbol table, which the test binary publishes
 * via `-Wl,--export-dynamic` (`build.rs`). A successful load is therefore the empirical proof that
 * the Rust implementations of this family are exported as dynamic symbols and bind a real C driver.
 *
 * `DRIVER_INIT(lifecycle)` exports `driver_init` returning the `ErlDrvEntry` below, whose
 * `driver_name` is `"lifecycle"`, so the only name this library may be loaded as is `lifecycle`.
 *
 * Portability note: this fixture references ONLY `driver_*` / `erl_drv_*` names whose `ErlDrvApi`
 * slots this crate populates (`Some`). A reference to an unimplemented slot — e.g.
 * `add_driver_entry`, `remove_driver_entry`, `driver_lock_driver`, `driver_create_port` — would make
 * `dlopen(RTLD_NOW)` fail on the unresolved import. `driver_alloc`/`driver_free` are the only other
 * imports (they allocate the private state).
 *
 * Oracle note: every expected value below is what the legacy ERTS returns, with each command's
 * return value chosen so the Rust test can pin the *C-observable* result:
 *   - `driver_failure_eof` → `driver_failure_term(ix, NIL, 1)` → `0`, `-1` for an invalid port
 *     (`io.c:7278`, `io.c:7204`);
 *   - `driver_failure_atom` → `driver_failure_term(ix, erts_atom_put(string), 0)` → `0`/`-1`
 *     (`io.c:7263`);
 *   - `driver_failure_posix` → `driver_failure_atom(ix, erl_errno_id(err))` → `0`/`-1`
 *     (`io.c:7273`);
 *   - `driver_failure` → `driver_failure_term(ix, make_small(code), code == 0)` → `0`/`-1`
 *     (`io.c:7258`);
 *   - `driver_exit` → `-1` for an invalid port, else `driver_failure_term(ix, am_normal, 0)` → `0`
 *     (`io.c:7234-7253`);
 *   - `set_busy_port` / `set_port_control_flags` return `void`; `get_port_flags` returns `0` for an
 *     unresolvable port (`io.c:5208`, `io.c:5318`, `io.c:5325`);
 *   - `erl_drv_busy_msgq_limits` writes `ERL_DRV_BUSY_MSGQ_DISABLED` into both bounds for an
 *     invalid port (`erl_port_task.c:779-785`);
 *   - `erl_drv_set_os_pid` returns `void` (a no-op for an invalid port, `io.c:2970`);
 *   - `driver_async_port_key` returns `0` for a port it cannot resolve (`erl_async.c:518-524`).
 */
#include <erl_driver.h>
#include <errno.h>
#include <string.h>

/* Private per-port state: the port handle (these functions have no port argument) plus a marker so
 * the returned `ErlDrvData` is a real, non-sentinel pointer. */
typedef struct {
    ErlDrvPort port;
    unsigned long marker;
} lifecycle_state;

static ErlDrvData lifecycle_start(ErlDrvPort port, char *command) {
    lifecycle_state *st;
    (void) command;
    st = (lifecycle_state *) driver_alloc(sizeof(lifecycle_state));
    if (st == NULL)
        return (ErlDrvData) -1;
    st->port = port;
    st->marker = 0x1c0deUL;
    return (ErlDrvData) st;
}

static void lifecycle_stop(ErlDrvData drv_data) {
    if (drv_data != NULL)
        driver_free(drv_data);
}

static ErlDrvSSizeT lifecycle_control(ErlDrvData drv_data, unsigned int command,
                                      char *buf, ErlDrvSizeT len,
                                      char **rbuf, ErlDrvSizeT rlen) {
    lifecycle_state *st = (lifecycle_state *) drv_data;
    (void) buf;
    (void) len;
    (void) rbuf;
    (void) rlen;

    switch (command) {
    case 0:
        /* The documented no-op. */
        return 0;

    /* ------------------------------------------------------------------ attributes / flags ---- */
    case 1:
        /* `set_busy_port` returns `void`: the only observable is that nothing crashes and the host
         * records the two requests (busy, then idle). */
        set_busy_port(st->port, 1);
        set_busy_port(st->port, 0);
        return 0;

    case 2:
        /* The round trip: what `set_port_control_flags` set must come back from `get_port_flags`
         * (the seam routes both through the host, which keeps them consistent). */
        set_port_control_flags(st->port, PORT_CONTROL_FLAG_BINARY);
        return (ErlDrvSSizeT) (get_port_flags(st->port) == PORT_FLAG_BINARY);

    case 3:
        /* `PORT_CONTROL_FLAG_HEAVY` is the other bit (`erl_driver.h:99`). */
        set_port_control_flags(st->port, PORT_CONTROL_FLAG_HEAVY);
        return (ErlDrvSSizeT) (get_port_flags(st->port) == PORT_FLAG_LINE);

    case 4:
        /* Clearing the flags makes `get_port_flags` report none. */
        set_port_control_flags(st->port, 0);
        return (ErlDrvSSizeT) (get_port_flags(st->port) == 0);

    /* --------------------------------------------------------- busy-message-queue watermarks --- */
    case 5: {
        /* A write-read request (neither bound is zero): the applied bounds are echoed back. */
        ErlDrvSizeT low = 2, high = 8;
        erl_drv_busy_msgq_limits(st->port, &low, &high);
        return (ErlDrvSSizeT) (low == 2 && high == 8);
    }

    case 6: {
        /* `ERL_DRV_BUSY_MSGQ_DISABLED` in either bound disables the feature and is reported back
         * verbatim (`erl_port_task.c:790`). */
        ErlDrvSizeT low = ERL_DRV_BUSY_MSGQ_DISABLED;
        ErlDrvSizeT high = ERL_DRV_BUSY_MSGQ_DISABLED;
        erl_drv_busy_msgq_limits(st->port, &low, &high);
        return (ErlDrvSSizeT) (low == ERL_DRV_BUSY_MSGQ_DISABLED
                               && high == ERL_DRV_BUSY_MSGQ_DISABLED);
    }

    case 7: {
        /* An invalid port has no queue at all: both bounds become `ERL_DRV_BUSY_MSGQ_DISABLED`
         * (`erl_port_task.c:779-785`). */
        ErlDrvSizeT low = 1, high = 2;
        erl_drv_busy_msgq_limits(NULL, &low, &high);
        return (ErlDrvSSizeT) (low == ERL_DRV_BUSY_MSGQ_DISABLED
                               && high == ERL_DRV_BUSY_MSGQ_DISABLED);
    }

    case 8: {
        /* One absent bound pointer is "not supplied" and must not be written through. */
        ErlDrvSizeT high = 3;
        erl_drv_busy_msgq_limits(st->port, NULL, &high);
        return (ErlDrvSSizeT) (high == 3);
    }

    /* ---------------------------------------------------- async init ack / os pid / async key --- */
    case 9:
        /* A successful init result acks: `res` is the private state pointer (a real `ErlDrvData`,
         * not one of the three `ERL_DRV_ERROR_*` sentinels). */
        erl_drv_init_ack(st->port, (ErlDrvData) st);
        return 0;

    case 10:
        /* A failure result must NOT ack. */
        erl_drv_init_ack(st->port, ERL_DRV_ERROR_BADARG);
        return 0;

    case 11:
        /* `NULL` is a legal success result (the C replies with the port id). */
        erl_drv_init_ack(st->port, NULL);
        return 0;

    case 12:
        /* Record the OS pid seen in `port_info(_, os_pid)`. */
        erl_drv_set_os_pid(st->port, 4242);
        return 0;

    case 13:
        /* The async pool key: non-zero for a port this runtime can resolve
         * (`driver_async_port_key` → `internal_port_data` of the id term). */
        return (ErlDrvSSizeT) (driver_async_port_key(st->port) != 0);

    case 14:
        /* An invalid port has no key: the C's `driver_mk_port` → `NIL` → `0` path. */
        return (ErlDrvSSizeT) (driver_async_port_key(NULL) == 0);

    /* ---------------------------------------------------------------------- failures / exit ---- */
    case 15:
        /* Each of the four C spellings must reach the host's one `failure` hook and return `0`. */
        return (ErlDrvSSizeT) ((driver_failure_eof(st->port) == 0)
                               && (driver_failure_atom(st->port, "enoent") == 0)
                               && (driver_failure_posix(st->port, ENOENT) == 0)
                               && (driver_failure(st->port, 0) == 0)
                               && (driver_failure(st->port, 7) == 0));

    case 16:
        /* `driver_exit` reports `0` for a valid port, whatever the code. */
        return (ErlDrvSSizeT) ((driver_exit(st->port, 0) == 0)
                               && (driver_exit(st->port, 3) == 0));

    case 17:
        /* Every failure entry point reports `-1` for an invalid port, and the port queries report
         * the C's "cannot resolve" values — never a crash (`io.c:7204`, `io.c:5332`,
         * `erl_async.c:521`). */
        return (ErlDrvSSizeT) ((driver_failure_eof(NULL) == -1)
                               && (driver_failure_atom(NULL, "enoent") == -1)
                               && (driver_failure_posix(NULL, ENOENT) == -1)
                               && (driver_failure(NULL, 1) == -1)
                               && (driver_exit(NULL, 1) == -1)
                               && (get_port_flags(NULL) == 0)
                               && (driver_async_port_key(NULL) == 0));

    case 18:
        /* The `void` entry points are silent no-ops for an invalid port (`io.c:5214`,
         * `io.c:5319-5321`, `io.c:2975`), and `erl_drv_init_ack` must not fault either. */
        set_busy_port(NULL, 1);
        set_port_control_flags(NULL, PORT_CONTROL_FLAG_BINARY);
        erl_drv_set_os_pid(NULL, 1);
        erl_drv_init_ack(NULL, (ErlDrvData) st);
        return 1;

    default:
        /* An unknown command is visible as a distinct negative marker. */
        return -100;
    }
}

static ErlDrvEntry lifecycle_driver_entry = {
    .init = NULL,
    .start = lifecycle_start,
    .stop = lifecycle_stop,
    .output = NULL,
    .ready_input = NULL,
    .ready_output = NULL,
    .driver_name = "lifecycle",
    .finish = NULL,
    .handle = NULL,
    .control = lifecycle_control,
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

DRIVER_INIT(lifecycle) {
    return &lifecycle_driver_entry;
}
