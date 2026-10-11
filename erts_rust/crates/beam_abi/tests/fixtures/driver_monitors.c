/*
 * The DRIVER-MONITOR family of the driver C ABI (workspace contract A.4):
 *
 *     driver_monitor_process / driver_get_monitored_process /
 *     driver_demonitor_process / driver_compare_monitors
 *
 * It is compiled by the integration test `tests/driver_monitors.rs` with the system `cc` against
 * the system `erl_driver.h`, and loaded through `beam_abi::driver::loader::load`. Its `driver_*`
 * imports are deliberately left undefined by the linker: they are resolved (`RTLD_NOW`) out of the
 * *loading process*'s dynamic symbol table, which the test binary publishes via
 * `-Wl,--export-dynamic` (`build.rs`). A successful load is therefore empirical proof that the
 * Rust monitor family is exported as dynamic symbols and binds a real C driver.
 *
 * # What the fixture reports
 *
 * The family's whole point is the *identity* of a monitor: `driver_monitor_process` hands the
 * driver an opaque `ErlDrvMonitor` (`unsigned char data[sizeof(void*)*4]`, `erl_drv_nif.h:67-69`)
 * whose first `ERTS_REF_THING_SIZE * sizeof(Eterm)` bytes are the emulator's internal reference
 * thing (`erts_ref_to_driver_monitor`, `io.c:6994-7000`). Every later call is keyed on exactly
 * those 24 bytes: `driver_demonitor_process` rebuilds the ref from them
 * (`erts_driver_monitor_to_ref`, `io.c:7002-7010`, then `erts_monitor_tree_lookup`, `io.c:7069`),
 * `driver_get_monitored_process` does the same (`io.c:7108`), and `driver_compare_monitors` is a
 * pure `sys_memcmp` of exactly that range (`io.c:7142-7147`).
 *
 * The driver half therefore:
 *
 *  1. reports the **24 identity bytes** of the monitor it received (commands 10/11) into the
 *     control reply buffer, so the Rust test can compare them with the monitor its host issued;
 *  2. pins that the **tail** of the holder (`data[24..32]`, which the C never writes) is not part
 *     of the identity: a copy whose tail bytes are inverted still compares equal (command 12) and
 *     still resolves the monitored process (command 14) and still demonitors it (command 15);
 *  3. pins that a byte **inside** the identity does count (command 13);
 *  4. walks the documented reason codes of the three lifecycle calls (commands 1-9, 16-21).
 *
 * # Two entry points, one comparison
 *
 * The same `.so` exports BOTH
 *
 *  * `driver_init` — the `monitors` driver entry, driven by this crate's `driver::loader`; and
 *  * `nif_init`    — the `monitors` NIF entry, driven by a REAL system ERTS.
 *
 * That duality exists for one empirical reason: the Nix-packaged OTP 29.1.1 / erts-17.1 this repo
 * is developed against does NOT export `erlang:load_driver/2` (a build-time emulator option), so
 * the oracle cannot load a `.so` as a *driver* at all. It CAN load it as a NIF, and the `driver_*`
 * symbols a NIF leaves undefined bind against the *running emulator's* dynamic symbols exactly as
 * a driver's do (`nm -D beam.smp` lists `driver_compare_monitors` and the monitor trio as defined
 * `T` entries). `driver_compare_monitors` takes no port and no host state — it is a pure byte
 * comparison — so the NIF half can exercise the REAL ERTS implementation of it, and
 * `compare_report()` is shared verbatim by both entry points: its four reports therefore differ
 * between the two runtimes only if *the runtimes' comparisons differ*.
 *
 * # Portability note
 *
 * The file references ONLY `driver_*` symbols whose `ErlDrvApi` slots are populated on stable Rust
 * (`driver_alloc`/`driver_free`/`driver_output`/`driver_caller`/`driver_term_nil` + the monitor
 * family) and one `enif_*` slot (`enif_make_string_len`). A reference to an unimplemented slot
 * would make `dlopen(RTLD_NOW)` fail on the Rust side.
 */
#include <erl_driver.h>
#include <erl_nif.h>

#include <stdio.h>
#include <string.h>

/*
 * The reference-thing width: `ERTS_REF_THING_SIZE(3) * sizeof(Uint)(8)` on LP64 (`io.c:6996`).
 * `sizeof(Uint) == sizeof(void *)` on every platform ERTS builds for.
 */
#define IDENTITY_BYTES (3 * sizeof(void *))
/* `sizeof(ErlDrvMonitor)` — `sizeof(void *) * 4` (`erl_drv_nif.h:67-69`). */
#define MONITOR_BYTES (sizeof(ErlDrvMonitor))
/* No report is close to this; the bound keeps `snprintf` honest. */
#define REPORT_CAP 128

/* The sign of a comparison — the only part of `memcmp`'s result C guarantees. */
static int cmp_sign(int value) {
    return (value > 0) - (value < 0);
}

/* A monitor whose first `IDENTITY_BYTES` bytes are `byte` and whose tail is `tail`. */
static void fill_monitor(ErlDrvMonitor *mon, unsigned char byte, unsigned char tail) {
    memset(mon, tail, sizeof(*mon));
    memset(mon->data, byte, IDENTITY_BYTES);
}

/*
 * The four measurable facts about `driver_compare_monitors`, reported as `key=value` text. Both
 * entry points call this, so the two runtimes' strings must be identical:
 *
 *  * case 1 — identical identities, opposite tails -> `0` (the tail is not compared);
 *  * case 2 — a differing byte at index `IDENTITY_BYTES - 1` -> non-zero (the last identity byte
 *             counts);
 *  * case 3 — the comparison is **unsigned** byte order in both directions (`0xff` > `0x00`);
 *  * case 4 — a differing byte at index `IDENTITY_BYTES` (the first tail byte) -> `0`, which pins
 *             the width to exactly 24 bytes and not to the whole 32-byte holder.
 */
static size_t compare_report(unsigned int which, char *buf, size_t cap) {
    ErlDrvMonitor a, b;

    switch (which) {
    case 1:
        fill_monitor(&a, 0x11, 0x00);
        fill_monitor(&b, 0x11, 0xff);
        return (size_t) snprintf(buf, cap, "tail_ignored=%d",
                                 cmp_sign(driver_compare_monitors(&a, &b)));
    case 2:
        fill_monitor(&a, 0x11, 0x00);
        b = a;
        b.data[IDENTITY_BYTES - 1] ^= 0x01;
        return (size_t) snprintf(buf, cap, "last_identity_byte=%d",
                                 cmp_sign(driver_compare_monitors(&a, &b)));
    case 3:
        fill_monitor(&a, 0x00, 0x00);
        b = a;
        a.data[0] = 0xff;
        a.data[1] = 0x00;
        return (size_t) snprintf(buf, cap, "ff_vs_00=%d 00_vs_ff=%d",
                                 cmp_sign(driver_compare_monitors(&a, &b)),
                                 cmp_sign(driver_compare_monitors(&b, &a)));
    case 4:
        fill_monitor(&a, 0x22, 0x00);
        b = a;
        b.data[IDENTITY_BYTES] = 0x7f;
        return (size_t) snprintf(buf, cap, "first_tail_byte_only=%d",
                                 cmp_sign(driver_compare_monitors(&a, &b)));
    default:
        return (size_t) snprintf(buf, cap, "unknown compare case");
    }
}

/* ------------------------------------------------------------------------------------------- */
/* Entry point 1: the driver (`driver_init`), driven by this crate's driver loader               */
/* ------------------------------------------------------------------------------------------- */

/* Private per-port state: the port plus the two monitors the commands create. */
typedef struct {
    ErlDrvPort port;
    ErlDrvMonitor mon1;
    ErlDrvMonitor mon2;
} monitors_state;

static ErlDrvData monitors_start(ErlDrvPort port, char *command) {
    monitors_state *st;
    (void) command;
    st = (monitors_state *) driver_alloc(sizeof(monitors_state));
    if (st == NULL)
        return (ErlDrvData) -1;
    st->port = port;
    memset(&st->mon1, 0, sizeof(st->mon1));
    memset(&st->mon2, 0, sizeof(st->mon2));
    return (ErlDrvData) st;
}

static void monitors_stop(ErlDrvData drv_data) {
    if (drv_data != NULL)
        driver_free(drv_data);
}

/*
 * The callback the family exists for (`io.c:7150`): when a monitored process dies the emulator
 * hands the monitor back to the driver. `do_driver_monitor_process` REFUSES to arm a monitor
 * unless this callback is present (`io.c:3020-3022`), so the fixture must declare it. Under this
 * test's host the callback is never fired (there is no scheduler behind the seam - that half is
 * `beam_rt`/`beam_io`), so the body only clears the monitor so a second event cannot be mistaken
 * for the first.
 */
static void monitors_process_exit(ErlDrvData drv_data, ErlDrvMonitor *monitor) {
    monitors_state *st = (monitors_state *) drv_data;
    if (st != NULL && monitor != NULL && memcmp(monitor->data, st->mon1.data, MONITOR_BYTES) == 0)
        memset(&st->mon1, 0, sizeof(st->mon1));
    else if (st != NULL)
        memset(&st->mon2, 0, sizeof(st->mon2));
}

/* Copy the 24 identity bytes of `mon` into the control reply buffer. */
static ErlDrvSSizeT reply_identity(char **rbuf, ErlDrvSizeT rlen, const ErlDrvMonitor *mon) {
    if (rbuf == NULL || *rbuf == NULL || rlen < IDENTITY_BYTES)
        return -2;
    memcpy(*rbuf, mon->data, IDENTITY_BYTES);
    return (ErlDrvSSizeT) IDENTITY_BYTES;
}

/* Overwrite the tail (the bytes the emulator never writes) with `byte`. */
static void scatter_tail(ErlDrvMonitor *mon, unsigned char byte) {
    size_t i;
    for (i = IDENTITY_BYTES; i < MONITOR_BYTES; i++)
        mon->data[i] = byte;
}

/* Invert the tail, so a copy cannot accidentally equal the original's tail. */
static void invert_tail(ErlDrvMonitor *mon) {
    size_t i;
    for (i = IDENTITY_BYTES; i < MONITOR_BYTES; i++)
        mon->data[i] = (unsigned char) ~mon->data[i];
}

static ErlDrvSSizeT monitors_control(ErlDrvData drv_data, unsigned int command,
                                     char *buf, ErlDrvSizeT len,
                                     char **rbuf, ErlDrvSizeT rlen) {
    monitors_state *st = (monitors_state *) drv_data;
    (void) buf;
    (void) len;

    switch (command) {
    case 0:
        /* The documented no-op. */
        return 0;

    case 1:
        /* Arm a monitor on the port's connected process: `0` on success, monitor filled. */
        memset(&st->mon1, 0, sizeof(st->mon1));
        return (ErlDrvSSizeT) driver_monitor_process(st->port, driver_caller(st->port),
                                                     &st->mon1);

    case 2:
        /* A second monitor on the same process carries a DIFFERENT reference thing. */
        memset(&st->mon2, 0, sizeof(st->mon2));
        return (ErlDrvSSizeT) driver_monitor_process(st->port, driver_caller(st->port),
                                                     &st->mon2);

    case 3: {
        /* An operand that cannot name a process is the positive "no longer alive" code. */
        ErlDrvMonitor mon;
        memset(&mon, 0, sizeof(mon));
        return (ErlDrvSSizeT) driver_monitor_process(st->port, driver_term_nil, &mon);
    }

    case 4:
        /* A missing out-parameter is the negative failure code. */
        return (ErlDrvSSizeT) driver_monitor_process(st->port, driver_caller(st->port), NULL);

    case 5: {
        /* An invalid port is the negative failure code as well. */
        ErlDrvMonitor mon;
        memset(&mon, 0, sizeof(mon));
        return (ErlDrvSSizeT) driver_monitor_process(NULL, driver_caller(st->port), &mon);
    }

    case 6:
        /* The monitor reports the process it was created for. */
        return (ErlDrvSSizeT) (driver_get_monitored_process(st->port, &st->mon1)
                               == driver_caller(st->port));

    case 7: {
        /* A monitor nobody issued reports `driver_term_nil` (`io.c:7113`). */
        ErlDrvMonitor junk;
        memset(&junk, 0, sizeof(junk));
        return (ErlDrvSSizeT) (driver_get_monitored_process(st->port, &junk)
                               == driver_term_nil);
    }

    case 8:
        /* A monitor compares equal to itself. */
        return (ErlDrvSSizeT) driver_compare_monitors(&st->mon1, &st->mon1);

    case 9:
        /* Two monitors of the same process carry distinct reference things, so they differ. */
        return (ErlDrvSSizeT) (driver_compare_monitors(&st->mon1, &st->mon2) != 0);

    case 10:
        /* The 24 identity bytes of the first monitor, straight out of the holder. */
        return reply_identity(rbuf, rlen, &st->mon1);

    case 11:
        /* ... and of the second. */
        return reply_identity(rbuf, rlen, &st->mon2);

    case 12: {
        /* Same identity, inverted tail: still equal - the tail is not part of the identity. */
        ErlDrvMonitor copy = st->mon1;
        invert_tail(&copy);
        return (ErlDrvSSizeT) (driver_compare_monitors(&copy, &st->mon1) == 0);
    }

    case 13: {
        /* A byte INSIDE the identity is what makes two monitors differ. */
        ErlDrvMonitor copy = st->mon1;
        copy.data[IDENTITY_BYTES - 1] = (unsigned char) (copy.data[IDENTITY_BYTES - 1] ^ 0x01);
        return (ErlDrvSSizeT) (driver_compare_monitors(&copy, &st->mon1) != 0);
    }

    case 14: {
        /* The identity - not the tail - is what resolves the monitored process. */
        ErlDrvMonitor copy = st->mon1;
        scatter_tail(&copy, 0x5a);
        return (ErlDrvSSizeT) (driver_get_monitored_process(st->port, &copy)
                               == driver_caller(st->port));
    }

    case 15: {
        /* ... and what demonitors it: the tail-perturbed copy removes the original monitor. */
        ErlDrvMonitor copy = st->mon1;
        scatter_tail(&copy, 0x5a);
        return (ErlDrvSSizeT) driver_demonitor_process(st->port, &copy);
    }

    case 16:
        /* Removing it again reports "no longer exists" (a positive code). */
        return (ErlDrvSSizeT) driver_demonitor_process(st->port, &st->mon1);

    case 17: {
        /* An invalid port and a missing monitor are both the negative failure code. */
        int a = driver_demonitor_process(NULL, &st->mon2);
        int b = driver_demonitor_process(st->port, NULL);
        return (ErlDrvSSizeT) ((a == -1 && b == -1) ? 1 : 0);
    }

    case 18:
        /* A missing monitor out-parameter answers `driver_term_nil`. */
        return (ErlDrvSSizeT) (driver_get_monitored_process(st->port, NULL) == driver_term_nil);

    case 19:
        /* An invalid port answers `driver_term_nil` as well. */
        return (ErlDrvSSizeT) (driver_get_monitored_process(NULL, &st->mon2) == driver_term_nil);

    case 20:
        /* The second, still-live monitor is removed by its own holder. */
        return (ErlDrvSSizeT) driver_demonitor_process(st->port, &st->mon2);

    case 21:
        /* Once demonitored, the first monitor no longer names a process. */
        return (ErlDrvSSizeT) (driver_get_monitored_process(st->port, &st->mon1)
                               == driver_term_nil);

    case 30:
    case 31:
    case 32:
    case 33: {
        /* The shared comparison report, published verbatim through `driver_output` so the test can
         * compare it with the string the oracle's NIF entry produces for the same case. */
        char report[REPORT_CAP];
        size_t n = compare_report(command - 29, report, sizeof(report));
        (void) driver_output(st->port, report, n);
        return (ErlDrvSSizeT) n;
    }

    default:
        /* An unknown command is visible as a distinct negative marker. */
        return -100;
    }
}

static ErlDrvEntry monitors_driver_entry = {
    .init = NULL,
    .start = monitors_start,
    .stop = monitors_stop,
    .output = NULL,
    .ready_input = NULL,
    .ready_output = NULL,
    .driver_name = "monitors",
    .finish = NULL,
    .handle = NULL,
    .control = monitors_control,
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
    .process_exit = monitors_process_exit,
    .stop_select = NULL,
    .emergency_close = NULL,
};

DRIVER_INIT(monitors) {
    return &monitors_driver_entry;
}

/* ------------------------------------------------------------------------------------------- */
/* Entry point 2: the NIF (`nif_init`), driven by a real ERTS whose driver loader is disabled    */
/* ------------------------------------------------------------------------------------------- */

/* `which` is the driver-side compare case, so both halves name one report. */
static ERL_NIF_TERM monitors_nif_report(ErlNifEnv *env, unsigned int which) {
    char report[REPORT_CAP];
    size_t n = compare_report(which, report, sizeof(report));
    return enif_make_string_len(env, report, n, ERL_NIF_LATIN1);
}

static ERL_NIF_TERM monitors_nif_tail_ignored(ErlNifEnv *env, int argc,
                                              const ERL_NIF_TERM argv[]) {
    (void) argc;
    (void) argv;
    return monitors_nif_report(env, 1);
}

static ERL_NIF_TERM monitors_nif_last_identity_byte(ErlNifEnv *env, int argc,
                                                    const ERL_NIF_TERM argv[]) {
    (void) argc;
    (void) argv;
    return monitors_nif_report(env, 2);
}

static ERL_NIF_TERM monitors_nif_unsigned_order(ErlNifEnv *env, int argc,
                                                const ERL_NIF_TERM argv[]) {
    (void) argc;
    (void) argv;
    return monitors_nif_report(env, 3);
}

static ERL_NIF_TERM monitors_nif_tail_only_byte(ErlNifEnv *env, int argc,
                                                const ERL_NIF_TERM argv[]) {
    (void) argc;
    (void) argv;
    return monitors_nif_report(env, 4);
}

static ErlNifFunc monitors_nif_funcs[] = {
    {"cmp_tail_ignored", 0, monitors_nif_tail_ignored, 0},
    {"cmp_last_identity_byte", 0, monitors_nif_last_identity_byte, 0},
    {"cmp_unsigned_order", 0, monitors_nif_unsigned_order, 0},
    {"cmp_tail_only_byte", 0, monitors_nif_tail_only_byte, 0},
};

/* The entry's `name` must equal the calling module, so the oracle module is named `monitors`. */
ERL_NIF_INIT(monitors, monitors_nif_funcs, NULL, NULL, NULL, NULL)
