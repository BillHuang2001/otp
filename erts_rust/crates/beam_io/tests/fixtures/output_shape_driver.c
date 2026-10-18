/*
 * A REAL driver shared object — the C fixture for the **driver-output shapes** and the
 * **driver-failure channel** of the `beam_io` port/driver seam (workspace contract A.4: the
 * frozen driver C ABI).
 *
 * It is compiled by the integration test binary `tests/abi_failure.rs` with the system `cc`
 * against the system `erl_driver.h` and then `dlopen`ed, exactly like `host_driver.c` (whose
 * conventions this file follows). Every `driver_*` symbol is deliberately left *undefined* by
 * the linker: those calls resolve out of the *loading process*'s dynamic symbol table, so a
 * `dlopen(RTLD_NOW)` only succeeds once the host functions are exported.
 *
 * `DRIVER_INIT(output_shape_driver)` exports `driver_init` returning the `ErlDrvEntry` below,
 * whose `driver_name` is `"output_shape_driver"`, so the only name this library may be loaded
 * as is `output_shape_driver`. The entry declares the C's version fields
 * (`.extended_marker`/`.major_version`/`.minor_version`) because the loader refuses an entry
 * without them (`driver_incorrect_version`).
 *
 * ## Why a fixture of its own
 *
 * `host_driver.c` is the general-purpose fixture (select/timers/async/queues/`outputv`); it
 * echoes whatever it is given. The two tables this file pins are about the **shape** of a
 * single `driver_output*` call and about a driver **closing its own port**, so the observable
 * has to be scripted per command byte rather than derived from the input. Keeping it in a
 * separate object also keeps `host_driver.c`'s entry free of slots that exist only here.
 *
 * ## The command vocabulary
 *
 * The `output` callback reads the **first byte** of the command as an opcode and then makes
 * the `driver_output*` / `driver_failure*` calls the opcode names. It logs one
 * `output op=<n>` line per command and one `call=<name> rc=<n>` line per C call it makes
 * (the C's own return code, so the invalid-handle `-1` stays distinguishable from the
 * closing-port `0`):
 *
 *   0  `driver_output(port, "", 0)`                    — an empty, present body
 *   1  `driver_output(port, NULL, 0)`                  — the C's `buf == NULL`: no body at all
 *   2  `driver_output2(port, "hdr", 3, NULL, 0)`       — header only
 *   3  `driver_output2(port, "hdr", 3, "", 0)`         — header + present, empty body
 *   4  `driver_output2(port, "hdr", 3, "body", 4)`     — header + body
 *   5  `driver_output(port, "hello", 5)` **and** `driver_output(port, "W", 1)` in ONE callback —
 *      two calls, which the C delivers as two `{Port,{data,_}}` messages
 *   6  `driver_failure_posix(port, EINVAL)`
 *   7  `driver_failure_atom(port, "enospc")`
 *   8  `driver_failure(port, 42)`
 *   9  `driver_failure_eof(port)`
 *  10  `driver_exit(port, 0)`
 *  11  `driver_exit(port, ENOSPC)`
 *  12  `driver_output(port, "hello", 5)` **then** `driver_failure_posix(port, EINVAL)` — the
 *      ordering row: the bytes handed up before the failure still reach the owner, ahead of
 *      the close
 *  13  `driver_failure_posix(port, EINVAL)` then `driver_output2(port, "h", 1, "b", 1)` — a
 *      delivery attempted **after** the failure: dropped, and it reports the C's closing-port
 *      code `0` (`io.c:6400-6401`), never the invalid-handle `-1`
 *  14  `driver_failure_atom(port, "enospc")` then `driver_output(port, "x", 1)` — the same
 *      drop-and-report-success through the plain `driver_output` entry point
 *  15  `driver_output(port, "abcdef", 6)` — one read of six bytes (the `{line,4}` column's
 *      over-limit single-read row)
 *
 * Any other opcode, and an empty command, makes no call at all.
 *
 * ## The callback log
 *
 * `start(port, command)` parses `log=<path>` out of the whitespace/comma separated command
 * string; when it is an **absolute** path every callback appends its lines to that file (append
 * mode, one `fopen`/`fprintf`/`fclose` per line so the lines never interleave). With no `log=`
 * token, or a relative one, no file is ever touched — the only path this fixture may ever write is
 * the one its caller named, never one relative to the process's working directory.
 *
 * ## Why the per-port state is never freed
 *
 * `driver_failure*`/`driver_exit` tear the port down **synchronously, from inside the call**
 * (`driver_failure_term`, `io.c:7196-7280`, runs the driver's `stop` itself), and the post-failure
 * rows (`13`, `14`) make one more `driver_output*` and log its return code *after* that. A `stop`
 * that freed the per-port state would therefore have the next `log_call` read freed memory, and
 * since the log path is the first thing it reads, the write would name a file built from whatever
 * bytes the allocator left in that block — created relative to the process's working directory
 * (measured: a `HX\025l\216W`-style name, one per leaked row). The state is consequently allocated
 * in `start` and deliberately **never freed**: the process this fixture runs in ends with the test,
 * and a well-defined write through a live path is worth 600 bytes of test-process memory.
 *
 * ## The failure rows and the port owner
 *
 * A `driver_failure*` call closes the port from inside the callback: the owner observes the
 * reason (`einval`, `enospc`, `42`, `normal`) as the port's exit reason, and `driver_exit`
 * unlinks the connected process first, so its owner observes **nothing**. Both distinctions
 * are values on the Rust side (`PortFailure.failure.reason` / `.signals_owner`); the C
 * behaviour is recorded in `io.c` (`driver_failure_term`, `io.c:7196-7280`).
 */
/* The fixture uses the C99 `snprintf`; ask for POSIX.1-2008 before any header is read so it
 * builds under an explicit `-std=c99` as well as the compiler's default mode. */
#define _POSIX_C_SOURCE 200809L

#include <erl_driver.h>

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* Private per-port state, allocated in `start` and intentionally **never freed** (see the file
 * header: a post-failure callback still logs through it after `stop` has run). */
typedef struct {
    ErlDrvPort port; /* the handle `output` needs (it gets no port argument) */
    char log[512];   /* absolute path every callback appends its lines to ("" when none) */
} shape_state;

/* Append `text` as one line to the per-port log file, if the command asked for one.
 *
 * Only an absolute `log=` path is honoured, so nothing this fixture ever writes can land relative
 * to the process's working directory — the integration test runs `escript` from its own cwd, which
 * is where a stray write would otherwise appear. */
static void log_line(const shape_state *st, const char *text) {
    FILE *f;

    if (st == NULL || st->log[0] != '/')
        return;
    f = fopen(st->log, "a");
    if (f == NULL)
        return;
    fprintf(f, "%s\n", text);
    fclose(f);
}

/* Append `call=<name> rc=<code>`: the C entry point's own return code, so the two answers a
 * `driver_output*` can give — `0` on a closing port, `-1` for an unresolvable handle — are
 * observable from the fixture's side of the ABI. */
static void log_call(const shape_state *st, const char *name, int rc) {
    char line[128];

    snprintf(line, sizeof(line), "call=%s rc=%d", name, rc);
    log_line(st, line);
}

/* Parse `log=<path>` out of the whitespace/comma separated command string. */
static void parse_command(shape_state *st, const char *command) {
    const char *p = command;

    st->log[0] = '\0';
    if (p == NULL)
        return;
    while (*p != '\0') {
        while (*p == ' ' || *p == '\t' || *p == ',')
            p++;
        if (*p == '\0')
            break;
        if (strncmp(p, "log=", 4) == 0) {
            const char *s = p + 4;
            size_t n = 0;
            size_t i;
            while (s[n] != '\0' && s[n] != ' ' && s[n] != '\t' && s[n] != ',')
                n++;
            for (i = 0; i < n && i + 1 < sizeof(st->log); i++)
                st->log[i] = s[i];
            st->log[i] = '\0';
        }
        while (*p != '\0' && *p != ' ' && *p != '\t' && *p != ',')
            p++;
    }
}

static ErlDrvData shape_start(ErlDrvPort port, char *command) {
    shape_state *st = (shape_state *) driver_alloc(sizeof(shape_state));

    if (st == NULL)
        return (ErlDrvData) -1;
    st->port = port;
    parse_command(st, command);
    log_line(st, "start");
    return (ErlDrvData) st;
}

static void shape_stop(ErlDrvData drv_data) {
    shape_state *st = (shape_state *) drv_data;

    if (st == NULL)
        return;
    log_line(st, "stop");
    /* No `driver_free` here, on purpose: the C runs this `stop` from **inside**
     * `driver_failure*`/`driver_exit` (`io.c:7196-7280`), and the post-failure rows make one more
     * `driver_output*` — with its `log_call` — after it returns. Freeing would hand that write a
     * dangling log path. See the file header. */
}

/* The scripted `driver_output*`/`driver_failure*` calls the command byte names — see the file
 * header for the opcode table. */
static void shape_output(ErlDrvData drv_data, char *buf, ErlDrvSizeT len) {
    shape_state *st = (shape_state *) drv_data;
    char line[64];
    int op;

    if (st == NULL || buf == NULL || len < 1)
        return;
    op = (unsigned char) buf[0];
    snprintf(line, sizeof(line), "output op=%d", op);
    log_line(st, line);
    switch (op) {
    case 0:
        log_call(st, "driver_output_empty", driver_output(st->port, "", 0));
        break;
    case 1:
        log_call(st, "driver_output_null", driver_output(st->port, NULL, 0));
        break;
    case 2:
        log_call(st, "driver_output2_null_body",
                 driver_output2(st->port, "hdr", 3, NULL, 0));
        break;
    case 3:
        log_call(st, "driver_output2_empty_body",
                 driver_output2(st->port, "hdr", 3, "", 0));
        break;
    case 4:
        log_call(st, "driver_output2_body",
                 driver_output2(st->port, "hdr", 3, "body", 4));
        break;
    case 5:
        log_call(st, "driver_output_hello", driver_output(st->port, "hello", 5));
        log_call(st, "driver_output_W", driver_output(st->port, "W", 1));
        break;
    case 6:
        log_call(st, "driver_failure_posix", driver_failure_posix(st->port, EINVAL));
        break;
    case 7:
        log_call(st, "driver_failure_atom", driver_failure_atom(st->port, "enospc"));
        break;
    case 8:
        log_call(st, "driver_failure", driver_failure(st->port, 42));
        break;
    case 9:
        log_call(st, "driver_failure_eof", driver_failure_eof(st->port));
        break;
    case 10:
        log_call(st, "driver_exit_zero", driver_exit(st->port, 0));
        break;
    case 11:
        log_call(st, "driver_exit_enospc", driver_exit(st->port, ENOSPC));
        break;
    case 12:
        log_call(st, "driver_output_hello", driver_output(st->port, "hello", 5));
        log_call(st, "driver_failure_posix", driver_failure_posix(st->port, EINVAL));
        break;
    case 13:
        log_call(st, "driver_failure_posix", driver_failure_posix(st->port, EINVAL));
        log_call(st, "driver_output2_after_failure",
                 driver_output2(st->port, "h", 1, "b", 1));
        break;
    case 14:
        log_call(st, "driver_failure_atom", driver_failure_atom(st->port, "enospc"));
        log_call(st, "driver_output_after_failure", driver_output(st->port, "x", 1));
        break;
    case 15:
        log_call(st, "driver_output_six", driver_output(st->port, "abcdef", 6));
        break;
    default:
        break;
    }
}

static ErlDrvEntry output_shape_driver_entry = {
    .init = NULL,
    .start = shape_start,
    .stop = shape_stop,
    .output = shape_output,
    .ready_input = NULL,
    .ready_output = NULL,
    .driver_name = "output_shape_driver",
    .finish = NULL,
    .handle = NULL,
    .control = NULL,
    .timeout = NULL,
    .outputv = NULL,
    .ready_async = NULL,
    .flush = NULL,
    .call = NULL,
    .unused_event_callback = NULL,
    /* The loader refuses an entry that does not declare the extended version fields
     * (`driver_incorrect_version`), so they are set explicitly. */
    .extended_marker = ERL_DRV_EXTENDED_MARKER,
    .major_version = ERL_DRV_EXTENDED_MAJOR_VERSION,
    .minor_version = ERL_DRV_EXTENDED_MINOR_VERSION,
    .driver_flags = 0,
    .handle2 = NULL,
    .process_exit = NULL,
    .stop_select = NULL,
    .emergency_close = NULL,
};

DRIVER_INIT(output_shape_driver) {
    return &output_shape_driver_entry;
}
