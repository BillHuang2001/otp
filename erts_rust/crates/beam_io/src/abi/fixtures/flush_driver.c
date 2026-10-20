/*
 * The C driver fixture for `DriverPort`'s **`flush`** forwarding (`src/abi/port.rs`, contract A.4:
 * the frozen driver C ABI).
 *
 * Compiled by the in-module suite `src/abi/flush_tests.rs` with the system `cc` against the system
 * `erl_driver.h` and then `dlopen`ed through `beam_abi::driver::loader::load`.
 *
 * ## Why it exists
 *
 * The C splits a port's death in two: `flush` (`flush_port`,
 * `erts/emulator/beam/io.c:3659-3700`) gives the driver one chance to discard the output still
 * queued for it, and `stop` (`terminate_port`, `io.c:3703-3704`, whose `drv->stop` call is
 * `:3752`) releases the driver afterwards.
 * `flush` is only called when the entry's pointer is non-`NULL`
 * (`if (p->drv_ptr->flush != NULL)`, `io.c:3664`; the call itself is `:3687`), so both halves of
 * that rule need a fixture:
 *
 *   * this file's base entry declares `flush`, so a host can prove the callback ran (its log line)
 *     and in which order relative to the port's own write drain and to `stop`;
 *   * `flush_driver_no_flush.c` defines `FLUSH_DRIVER_FLUSH_SLOT` to `NULL` and includes this file,
 *     producing the entry a driver with nothing to discard has — the C's silent no-op;
 *   * `flush_driver_marker.c` defines `FLUSH_DRIVER_EMIT_MARKER` and includes this file, so `flush`
 *     additionally hands a `"F"` marker up through `driver_output` — the one thing this fixture
 *     cannot do unconditionally (see below).
 *
 * ## Why the base fixture references **no** `driver_*` function
 *
 * A driver `.so` leaves `driver_*` undefined and resolves them out of the loading process's dynamic
 * symbol table. That only works where the host symbols are exported, and `beam_io/build.rs`'s
 * `-Wl,--export-dynamic` covers the **integration** test targets, not the unit-test build of the
 * library — so the base fixture must load in either binary, which it does by using the C library's
 * own allocation and file I/O instead of `driver_alloc`/`driver_output`.
 *
 * The consequence is a real property, not a compromise: a fixture with no host calls **cannot**
 * discard the port's queued bytes (they live in the port, not the driver), so it is exactly the
 * "flush discards nothing, keep the bytes and try to write them" driver the ordering test needs.
 * The `driver_output` half (the callback's data reaching the port's sinks) is the marker build's,
 * and that build needs the exported host symbols; `flush_tests.rs` skips it loudly with a precise
 * reason where they are absent.
 *
 * ## The start command
 *
 * `start(port, command)` parses a whitespace/comma separated command string:
 *
 *   * `log=<path>` — an **absolute** file path every callback appends a line to, so a host can
 *     observe the *order* the driver's callbacks ran in (`start`, `output`, `flush`, `stop`). Only
 *     an absolute path is honoured, so nothing this fixture writes lands relative to the process's
 *     working directory.
 *
 * Anything else is ignored. The per-port state is intentionally never freed (see `flush_stop`).
 */
#define _POSIX_C_SOURCE 200809L
#include <erl_driver.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
/* The entry's `flush` slot: `flush_cb` normally, `NULL` in the variant build
 * (`flush_driver_no_flush.c`) that exists to pin the C's `flush == NULL` no-op. A variant defines
 * `FLUSH_DRIVER_FLUSH_SLOT` before including this file; that also tells this file the callback
 * function is unwanted there, so it is omitted (never an unused-function warning). Same pattern as
 * `tests/fixtures/host_driver.c`'s `_SLOT` macros. */
#ifdef FLUSH_DRIVER_FLUSH_SLOT
/* Defined by a variant — respect its choice. */
#else
#define FLUSH_DRIVER_FLUSH_SLOT flush_cb
#define FLUSH_DRIVER_HAS_FLUSH 1
#endif
/* Private per-port state, allocated in `start` and intentionally never freed (see `flush_stop`). */
typedef struct {
    ErlDrvPort port; /* the handle the marker build's `driver_output` needs */
    char log[512];   /* path every callback appends an event line to ("" when none) */
} flush_state;
/* Append `what` as one line to the per-port log file, if the command asked for one. */
static void log_event(const flush_state *st, const char *what) {
    FILE *f;
    if (st == NULL || st->log[0] != '/')
        return;
    f = fopen(st->log, "a");
    if (f == NULL)
        return;
    fprintf(f, "%s\n", what);
    fclose(f);
}
/* Copy at most `cap - 1` bytes of `src` into `dst`, NUL-terminating. */
static void copy_token(char *dst, size_t cap, const char *src, size_t n) {
    size_t i = 0;
    if (cap == 0)
        return;
    while (i + 1 < cap && i < n && src[i] != '\0') {
        dst[i] = src[i];
        i++;
    }
    dst[i] = '\0';
}
/* Parse the `log=<path>` token out of the `open_port/2` command string. */
static void parse_command(flush_state *st, const char *command) {
    const char *p = command;
    if (p == NULL)
        return;
    while (*p != '\0') {
        while (*p == ' ' || *p == ',' || *p == '\t' || *p == '\n')
            p++;
        if (strncmp(p, "log=", 4) == 0) {
            const char *start = p + 4;
            const char *end = start;
            while (*end != '\0' && *end != ' ' && *end != ',' && *end != '\t' && *end != '\n')
                end++;
            copy_token(st->log, sizeof(st->log), start, (size_t) (end - start));
            p = end;
            continue;
        }
        while (*p != '\0' && *p != ' ' && *p != ',' && *p != '\t' && *p != '\n')
            p++;
    }
}
static ErlDrvData flush_start(ErlDrvPort port, char *command) {
    /* The C library's allocator, deliberately not `driver_alloc` — see "Why the base fixture
     * references no `driver_*` function" above. */
    flush_state *st = (flush_state *) malloc(sizeof(flush_state));
    if (st == NULL)
        return ERL_DRV_ERROR_GENERAL;
    st->port = port;
    st->log[0] = '\0';
    parse_command(st, command);
    log_event(st, "start");
    return (ErlDrvData) st;
}
/* The `ErlDrvEntry.flush` slot: the driver's one chance, before `stop`, to discard the output still
 * queued for it. This callback discards nothing (the queued bytes live in the port, and this
 * fixture makes no host calls at all), so the host's drain after it still tries the write — the
 * shape that pins the ordering `flush` → try-the-write → `stop`. It logs `"flush"`; the marker
 * build additionally hands a `"F"` up through `driver_output` (one read message, the ordinary sink
 * path), which needs the exported host symbols. */
#ifdef FLUSH_DRIVER_HAS_FLUSH
static void flush_cb(ErlDrvData drv_data) {
    flush_state *st = (flush_state *) drv_data;
    if (st == NULL)
        return;
    log_event(st, "flush");
#ifdef FLUSH_DRIVER_EMIT_MARKER
    (void) driver_output(st->port, "F", 1);
#endif
}
#endif
/* Echo whatever Erlang sends to the port back to its owner, logging the callback so a host can
 * assert the close-time drain really reached the driver. The echo itself is the marker build's
 * (it needs `driver_output`); the base build logs only. */
static void flush_output(ErlDrvData drv_data, char *buf, ErlDrvSizeT len) {
    flush_state *st = (flush_state *) drv_data;
    if (st == NULL)
        return;
    log_event(st, "output");
#ifdef FLUSH_DRIVER_EMIT_MARKER
    (void) driver_output(st->port, buf, len);
#else
    (void) buf;
    (void) len;
#endif
}
/* `stop`: the C's `terminate_port` callback (`io.c:3703-3704`, called at `:3752`), the **only**
 * caller of this slot.
 * Logging it is how a host proves `flush` ran first. No `free` here on purpose: a host may still
 * read the port's teardown state after this returns, and the state lives as long as the test
 * process. */
static void flush_stop(ErlDrvData drv_data) {
    flush_state *st = (flush_state *) drv_data;
    if (st == NULL)
        return;
    log_event(st, "stop");
}
static ErlDrvEntry flush_driver_entry = {
    .init = NULL,
    .start = flush_start,
    .stop = flush_stop,
    .output = flush_output,
    .ready_input = NULL,
    .ready_output = NULL,
    .driver_name = "flush_driver",
    .finish = NULL,
    .handle = NULL,
    .control = NULL,
    .timeout = NULL,
    .outputv = NULL,
    .ready_async = NULL,
    .flush = FLUSH_DRIVER_FLUSH_SLOT,
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
DRIVER_INIT(flush_driver) {
    return &flush_driver_entry;
}
