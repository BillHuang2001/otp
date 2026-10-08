/*
 * The BINARY REFCOUNT TRIO of the driver C ABI (workspace contract A.4):
 * `driver_binary_get_refc` / `driver_binary_inc_refc` / `driver_binary_dec_refc`.
 *
 * These three functions are the only `ErlDrvApi` slots that need state the public `ErlDrvBinary`
 * does not declare: the emulator keeps a reference count in a hidden `Binary` header that
 * *precedes* the pointer a driver ever sees
 * (`Binary2ErlDrvBinary(B) = &((ErtsBinary *)B)->driver.binary`,
 * `erts/emulator/beam/erl_binary.h:173-176`). A driver therefore cannot observe the header through
 * the frozen public struct, so exercising the trio through real C is the only honest evidence that
 * this crate reproduces the layout and the arithmetic.
 *
 * # Two entry points, one sequence
 *
 * The same `.so` exports BOTH
 *
 *  * `driver_init` — the `binrefc` driver entry, driven by this crate's `driver::loader`; and
 *  * `nif_init`    — the `binrefc` NIF entry, driven by a REAL system ERTS.
 *
 * That duality exists for one empirical reason: the Nix-packaged OTP 29.1.1 / erts-17.1 this repo
 * is developed against does NOT export `erlang:load_driver/2` (a build-time emulator option), so
 * the oracle cannot load a `.so` as a *driver* at all. It CAN load it as a NIF, and the `driver_*`
 * symbols a NIF leaves undefined bind against the *running emulator's* dynamic symbols exactly as
 * a driver's do (`nm -D beam.smp` lists `driver_alloc_binary`, the reference-count trio,
 * `driver_realloc_binary` and `driver_free_binary` as defined `T` entries). Every oracle value this
 * file produces therefore comes from the real ERTS implementation of the trio, exercised by the
 * identical C code path the driver loader drives on the Rust side.
 *
 * Both entry points funnel into `refc_report()`, so a report can only differ between the two
 * runtimes if the *runtime's* implementation of the trio differs.
 *
 * # What each command reports (the sequence is the whole point)
 *
 *  * `1` — `driver_alloc_binary(8)`, then get/inc/get/inc/dec/get/dec/get, then
 *    `driver_free_binary`. Pins the initial reference count, that `inc_refc`/`dec_refc` return the
 *    NEW value, and that `driver_free_binary` releases the driver's own reference.
 *  * `2` — the same around `driver_realloc_binary`: does a move preserve the reference count and
 *    does the new block report the new `orig_size`?
 *  * `3` — `driver_alloc_binary(0)`: a payload-less binary is still a valid block.
 *  * `4` — `driver_binary_dec_refc` driven down to zero: does the trio free at zero, or only
 *    `driver_free_binary`? (It only decrements — the block is released by `driver_free_binary`
 *    alone, so this command deliberately leaks one driver binary on both runtimes.)
 *
 * The sequences are chosen so that neither runtime can be made to fault or double-free: every
 * block is released exactly once, at a reference count of 1, and command 4 leaks rather than
 * releasing a block whose count has already reached zero.
 *
 * # Portability note
 *
 * The file references ONLY `driver_*` symbols whose `ErlDrvApi` slots are populated on stable Rust
 * (`driver_alloc_binary`, `driver_realloc_binary`, `driver_free_binary` and the reference-count
 * trio) plus `driver_output`. A reference to an unimplemented slot would make `dlopen(RTLD_NOW)`
 * fail on the Rust side.
 */
#include <erl_driver.h>
#include <erl_nif.h>

#include <stdio.h>
#include <string.h>

/* The longest report is far below this; the buffer keeps `snprintf` bounded either way. */
#define REFC_REPORT_CAP 256

/* ------------------------------------------------------------------------------------------- */
/* The one sequence both entry points drive                                                     */
/* ------------------------------------------------------------------------------------------- */

/* Command 1: the initial count and the inc/dec arithmetic, around one allocation. */
static size_t refc_case_basic(char *buf, size_t cap) {
    ErlDrvBinary *bin = driver_alloc_binary(8);
    ErlDrvSInt r_get0, r_inc1, r_get1, r_inc2, r_dec1, r_get2, r_dec2, r_get3;

    if (bin == NULL)
        return (size_t) snprintf(buf, cap, "alloc_binary failed");

    r_get0 = driver_binary_get_refc(bin);   /* the count the allocator handed out */
    r_inc1 = driver_binary_inc_refc(bin);   /* NEW value or the previous one? */
    r_get1 = driver_binary_get_refc(bin);
    r_inc2 = driver_binary_inc_refc(bin);
    r_dec1 = driver_binary_dec_refc(bin);
    r_get2 = driver_binary_get_refc(bin);
    r_dec2 = driver_binary_dec_refc(bin);
    r_get3 = driver_binary_get_refc(bin);

    /* Back at the driver's own single reference, so this releases the block on both runtimes. */
    driver_free_binary(bin);

    return (size_t) snprintf(buf, cap,
                             "get=%ld inc=%ld get=%ld inc=%ld dec=%ld get=%ld dec=%ld get=%ld",
                             (long) r_get0, (long) r_inc1, (long) r_get1, (long) r_inc2,
                             (long) r_dec1, (long) r_get2, (long) r_dec2, (long) r_get3);
}

/* Command 2: does `driver_realloc_binary` carry the reference count across the move? */
static size_t refc_case_realloc(char *buf, size_t cap) {
    ErlDrvBinary *bin = driver_alloc_binary(4);
    ErlDrvBinary *grown;
    ErlDrvSInt r_get0, r_inc1, r_after, r_dec1;
    long size_before, size_after;

    if (bin == NULL)
        return (size_t) snprintf(buf, cap, "alloc_binary failed");

    size_before = (long) bin->orig_size;
    r_get0 = driver_binary_get_refc(bin);
    r_inc1 = driver_binary_inc_refc(bin);

    grown = driver_realloc_binary(bin, 64);
    if (grown == NULL)
        return (size_t) snprintf(buf, cap, "realloc_binary failed");

    /* Read the count and the size through the NEW pointer: a move must preserve the reference
     * count and report the updated `orig_size`. Then drop the extra reference this command took
     * (`inc` above) and release the driver's own one. */
    r_after = driver_binary_get_refc(grown);
    size_after = (long) grown->orig_size;
    r_dec1 = driver_binary_dec_refc(grown);
    driver_free_binary(grown);

    return (size_t) snprintf(buf, cap,
                             "size_before=%ld get=%ld inc=%ld size_after=%ld refc_after=%ld "
                             "dec=%ld",
                             size_before, (long) r_get0, (long) r_inc1, size_after,
                             (long) r_after, (long) r_dec1);
}

/* Command 3: a zero-size driver binary. */
static size_t refc_case_zero(char *buf, size_t cap) {
    ErlDrvBinary *bin = driver_alloc_binary(0);
    ErlDrvSInt r_get0, r_inc1, r_dec1;
    long size;

    if (bin == NULL)
        return (size_t) snprintf(buf, cap, "alloc_binary failed");

    size = (long) bin->orig_size;
    r_get0 = driver_binary_get_refc(bin);
    r_inc1 = driver_binary_inc_refc(bin);
    r_dec1 = driver_binary_dec_refc(bin);
    driver_free_binary(bin);

    return (size_t) snprintf(buf, cap, "nonnull=%d size=%ld get=%ld inc=%ld dec=%ld", 1, size,
                             (long) r_get0, (long) r_inc1, (long) r_dec1);
}

/*
 * Command 4: `driver_binary_dec_refc` at the last reference.
 *
 * It only decrements (`erts_refc_dectest`); only `driver_free_binary` (`erts_bin_release`) frees,
 * and it frees exactly when its own decrement reaches zero. So the block allocated here is left
 * with a count that has gone to zero and then to -1, and the final `driver_free_binary` does NOT
 * release it. That is a deliberate one-block leak on both runtimes: it is the only way to observe
 * the rule without a use-after-free.
 */
static size_t refc_case_dec_to_zero(char *buf, size_t cap) {
    ErlDrvBinary *bin = driver_alloc_binary(8);
    ErlDrvSInt r_get0, r_dec1, r_get1, r_dec2;

    if (bin == NULL)
        return (size_t) snprintf(buf, cap, "alloc_binary failed");

    r_get0 = driver_binary_get_refc(bin);
    r_dec1 = driver_binary_dec_refc(bin);   /* 1 -> 0; NOT freed by the trio */
    r_get1 = driver_binary_get_refc(bin);
    r_dec2 = driver_binary_dec_refc(bin);   /* 0 -> -1 */
    driver_free_binary(bin);                /* -1 -> -2: no release (the block leaks by design) */

    return (size_t) snprintf(buf, cap, "get=%ld dec=%ld get=%ld dec=%ld", (long) r_get0,
                             (long) r_dec1, (long) r_get1, (long) r_dec2);
}

/*
 * The commands, in the order every test asserts them. `command` is the `port_control/3` command
 * (driver side) and, minus one, the index of the NIF function (NIF side) - kept explicit so both
 * sides name the same sequence.
 */
static size_t refc_report(unsigned int command, char *buf, size_t cap) {
    switch (command) {
    case 1:
        return refc_case_basic(buf, cap);
    case 2:
        return refc_case_realloc(buf, cap);
    case 3:
        return refc_case_zero(buf, cap);
    case 4:
        return refc_case_dec_to_zero(buf, cap);
    default:
        return (size_t) snprintf(buf, cap, "unknown command");
    }
}

/* The number of commands the fixture knows - the tests iterate `1 .. REFC_COMMANDS`. */
#define REFC_COMMANDS 4

/* ------------------------------------------------------------------------------------------- */
/* Entry point 1: the driver (`driver_init`), driven by this crate's driver loader              */
/* ------------------------------------------------------------------------------------------- */

/* Private per-port state: the port the reports are published to. */
typedef struct {
    ErlDrvPort port;
} refc_state;

static ErlDrvData refc_start(ErlDrvPort port, char *command) {
    refc_state *st;
    (void) command;
    st = (refc_state *) driver_alloc(sizeof(refc_state));
    if (st == NULL)
        return (ErlDrvData) -1;
    st->port = port;
    return (ErlDrvData) st;
}

static void refc_stop(ErlDrvData drv_data) {
    if (drv_data != NULL)
        driver_free(drv_data);
}

static ErlDrvSSizeT refc_control(ErlDrvData drv_data, unsigned int command, char *buf,
                                 ErlDrvSizeT len, char **rbuf, ErlDrvSizeT rlen) {
    refc_state *st = (refc_state *) drv_data;
    char report[REFC_REPORT_CAP];
    size_t n;
    (void) buf;
    (void) len;
    (void) rbuf;
    (void) rlen;

    n = refc_report(command, report, sizeof(report));
    /* Publish through `driver_output` rather than the control reply buffer: the emulator's reply
     * buffer truncates at ~63 bytes, while `driver_output` reaches the port owner verbatim. */
    (void) driver_output(st->port, report, n);
    return (ErlDrvSSizeT) n;
}

static ErlDrvEntry refc_driver_entry = {
    .init = NULL,
    .start = refc_start,
    .stop = refc_stop,
    .output = NULL,
    .ready_input = NULL,
    .ready_output = NULL,
    .driver_name = "binrefc",
    .finish = NULL,
    .handle = NULL,
    .control = refc_control,
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

DRIVER_INIT(binrefc) {
    return &refc_driver_entry;
}

/* ------------------------------------------------------------------------------------------- */
/* Entry point 2: the NIF (`nif_init`), driven by a real ERTS whose driver loader is disabled    */
/* ------------------------------------------------------------------------------------------- */

/* `command` is the driver-side command number, so both halves name one sequence. */
static ERL_NIF_TERM refc_nif_report(ErlNifEnv *env, unsigned int command) {
    char report[REFC_REPORT_CAP];
    size_t n = refc_report(command, report, sizeof(report));
    return enif_make_string_len(env, report, n, ERL_NIF_LATIN1);
}

static ERL_NIF_TERM refc_nif_basic(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc;
    (void) argv;
    return refc_nif_report(env, 1);
}

static ERL_NIF_TERM refc_nif_realloc(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc;
    (void) argv;
    return refc_nif_report(env, 2);
}

static ERL_NIF_TERM refc_nif_zero(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc;
    (void) argv;
    return refc_nif_report(env, 3);
}

static ERL_NIF_TERM refc_nif_dec_to_zero(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[]) {
    (void) argc;
    (void) argv;
    return refc_nif_report(env, 4);
}

static ErlNifFunc refc_nif_funcs[] = {
    {"seq_basic", 0, refc_nif_basic, 0},
    {"seq_realloc", 0, refc_nif_realloc, 0},
    {"seq_zero", 0, refc_nif_zero, 0},
    {"seq_dec_to_zero", 0, refc_nif_dec_to_zero, 0},
};

/* The entry's `name` must equal the calling module, so the oracle module is named `binrefc`. */
ERL_NIF_INIT(binrefc, refc_nif_funcs, NULL, NULL, NULL, NULL)
