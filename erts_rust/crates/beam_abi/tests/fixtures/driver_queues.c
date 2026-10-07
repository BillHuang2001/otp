/*
 * A REAL driver shared object exercising the PORT QUEUE family of the driver C ABI
 * (workspace contract A.4).
 *
 * It is compiled by the integration test `tests/driver_queues.rs` with the system `cc` against the
 * system `erl_driver.h` and loaded through `beam_abi::driver::loader::load`. Its queue imports
 * (`driver_enq`, `driver_pushq`, `driver_deq`, `driver_sizeq`, `driver_peekq`, `driver_peekqv`,
 * `driver_enq_bin`, `driver_pushq_bin`, `driver_enqv`, `driver_pushqv`, `driver_vec_to_buf`) are
 * deliberately left UNDEFINED by the linker: `dlopen(RTLD_NOW)` can only succeed if the *loading
 * process* exports them as dynamic symbols (the test binary does, via `-Wl,--export-dynamic` from
 * `build.rs` + the `#[used] DRIVER_API` static). A successful load is therefore the empirical proof
 * that the Rust queue functions bind a real C driver.
 *
 * `DRIVER_INIT(qd)` exports `driver_init` returning the `ErlDrvEntry` below, whose `driver_name` is
 * `"qd"`, so the only name this library may be loaded as is `qd`.
 *
 * # Portability note
 *
 * This fixture references ONLY `driver_*` symbols whose `ErlDrvApi` slots are populated on stable
 * Rust (`driver_alloc`/`driver_free`, `driver_alloc_binary`/`driver_free_binary` and the eleven
 * queue functions). A reference to an unimplemented slot would make `dlopen(RTLD_NOW)` fail.
 *
 * # What each control command does (it publishes a one-line report through `driver_output`)
 *
 * The report is delivered with `driver_output(st->port, text, strlen(text))` rather than written
 * into the `control` reply buffer: the emulator's reply buffer truncates at ~63 bytes, which cut
 * the longer reports off, whereas `driver_output` reaches the port owner verbatim.
 *
 * * `1` — append two 5-byte buffers, then report `driver_sizeq`, the `driver_peekq` entry count and
 *   per-entry byte count, `driver_peekqv`'s byte count and filled vector, a partial `driver_deq`,
 *   an over-large `driver_deq` (the C's `(ErlDrvSizeT)-1`), an exact `driver_deq`, and the empty
 *   queue readings.
 * * `2` — `driver_pushq` ×2, `driver_enq_bin`/`driver_pushq_bin` with an offset, a zero-length
 *   `driver_enq`, `driver_enqv`/`driver_pushqv` with a non-zero `skip`, and `driver_vec_to_buf`
 *   truncation/saturation/zero.
 * * `3` — the same `ErlIOVec` handed to `driver_enqv`: reports whether `skip` advanced the caller's
 *   vector *in place* (it does on the oracle, it does not in Rust — a documented deviation) plus the
 *   queue's own readings, which agree.
 * * `4` — the empty-queue readings on a fresh port (`driver_sizeq`, `driver_peekq`,
 *   `driver_peekqv`'s zeroing, `driver_deq(0)` and an over-large `driver_deq`).
 *
 * Every command **drains** the port queue before returning: a port whose queue is non-empty cannot
 * be cleanly closed by the emulator, so a leak here would make the oracle probe dump core.
 */
#include <erl_driver.h>
#include <stdio.h>
#include <string.h>

/* Private per-port state (the port handle; the queue itself lives in the emulator). */
typedef struct {
    ErlDrvPort port;
} qd_state;

static ErlDrvData qd_start(ErlDrvPort port, char *command) {
    qd_state *st;
    (void) command;
    st = (qd_state *) driver_alloc(sizeof(qd_state));
    if (st == NULL)
        return (ErlDrvData) -1;
    st->port = port;
    return (ErlDrvData) st;
}

static void qd_stop(ErlDrvData drv_data) {
    if (drv_data != NULL)
        driver_free(drv_data);
}

/* Publish `text` to the port owner through `driver_output` and return its length. */
static ErlDrvSSizeT publish(qd_state *st, const char *text) {
    size_t n = strlen(text);
    (void) driver_output(st->port, (char *) text, n);
    return (ErlDrvSSizeT) n;
}

/* Command 1: the enq / size / peek / deq semantics. */
static ErlDrvSSizeT qd_case1(qd_state *st, char **rbuf, ErlDrvSizeT rlen) {
    char report[512];
    ErlDrvPort p = st->port;
    SysIOVec *v, *ve;
    ErlIOVec ev, ev2;
    int vlen = -1, vlen_e = -1;
    int i, a, b;
    ErlDrvSizeT size1, first, sum, pk, dq3, size2, dqover, size3, dqexact, size4, pk_e;

    (void) rbuf;
    (void) rlen;
    memset(&ev, 0, sizeof(ev));
    memset(&ev2, 0, sizeof(ev2));

    a = driver_enq(p, "hello", 5);
    b = driver_enq(p, "world", 5);
    size1 = driver_sizeq(p);

    /* `driver_peekq` views the queue without removing anything. Read it *before* the next peek:
     * both spellings publish one per-thread snapshot in Rust (and share the queue's array in C). */
    v = driver_peekq(p, &vlen);
    first = 0;
    sum = 0;
    if (v != NULL) {
        first = v[0].iov_len;
        for (i = 0; i < vlen; i++)
            sum += v[i].iov_len;
    }

    /* `driver_peekqv` fills the caller's ErlIOVec and returns the queued byte count. */
    pk = driver_peekqv(p, &ev);

    dq3 = driver_deq(p, 3);              /* partial: returns the REMAINING size */
    size2 = driver_sizeq(p);
    dqover = driver_deq(p, 100);         /* larger than the queue: (ErlDrvSizeT)-1, queue untouched */
    size3 = driver_sizeq(p);
    dqexact = driver_deq(p, 7);          /* exact: drains the queue */
    size4 = driver_sizeq(p);

    ve = driver_peekq(p, &vlen_e);       /* empty: NULL, *vlen = 0 */
    pk_e = driver_peekqv(p, &ev2);       /* empty: 0, vector zeroed */

    snprintf(report, sizeof(report),
             "enq=%d,%d size=%zu peekq_null=%d vlen=%d first=%zu sum=%zu peekqv=%zu ev.vsize=%d "
             "ev.size=%zu deq3=%zu size2=%zu deq_over=%zu size3=%zu deq_exact=%zu size4=%zu "
             "peekq_empty_null=%d vlen_empty=%d peekqv_empty=%zu ev2.vsize=%d",
             a, b, (size_t) size1, (v == NULL), vlen, (size_t) first, (size_t) sum, (size_t) pk,
             ev.vsize, (size_t) ev.size, (size_t) dq3, (size_t) size2, (size_t) dqover,
             (size_t) size3, (size_t) dqexact, (size_t) size4, (ve == NULL), vlen_e,
             (size_t) pk_e, ev2.vsize);
    return publish(st, report);
}

/* Command 2: the head/tail variants, the binary forms, `skip`, and vec_to_buf. */
static ErlDrvSSizeT qd_case2(qd_state *st, char **rbuf, ErlDrvSizeT rlen) {
    char report[512];
    char bytes4[8], bytesbig[8];
    char buf[64];
    ErlDrvPort p = st->port;
    ErlDrvBinary *bin;
    ErlDrvBinary *binv[1];
    SysIOVec iov2[1], iov3[1], iov4[1];
    ErlIOVec ev, ev2;
    int c1, c2, eb, pb, zero, enqv, pushqv;
    ErlDrvSizeT size1, size2, size_mid, size3, r4, rbig, rzero;

    (void) rbuf;
    (void) rlen;
    c1 = driver_pushq(p, "cd", 2);
    c2 = driver_pushq(p, "ab", 2);
    size1 = driver_sizeq(p);

    /* The `*_bin` forms: a window `[offs .. offs+len)` of a driver binary. The emulator's own
     * reference keeps the binary alive, so freeing it here is legal in C too (the Rust seam has
     * already copied the bytes). */
    bin = driver_alloc_binary(4);
    if (bin == NULL)
        return publish(st, "driver_alloc_binary failed");
    memcpy(bin->orig_bytes, "wxyz", 4);
    eb = driver_enq_bin(p, bin, 1, 2);      /* "xy" ("wxyz"[1..3]) appended */
    pb = driver_pushq_bin(p, bin, 0, 2);    /* "wx" prepended */
    zero = driver_enq(p, "x", 0);           /* the C's early return: 0, nothing queued */
    size2 = driver_sizeq(p);
    driver_free_binary(bin);

    /* `driver_enqv` with skip = 2 over a 6-byte vector: "cdef" (4 bytes) appended. `binv[0]` is
     * NULL, the C's encoding for "plain memory, copy it" (it also dereferences `binv`, so a NULL
     * array would fault on the oracle). */
    binv[0] = NULL;
    iov2[0].iov_base = (char *) "abcdef";
    iov2[0].iov_len = 6;
    ev.vsize = 1;
    ev.size = 6;
    ev.iov = iov2;
    ev.binv = binv;
    enqv = driver_enqv(p, &ev, 2);
    size_mid = driver_sizeq(p);

    /* `driver_pushqv` with skip = 4 over a FRESH vector (`iov3`, never handed to a `*_v` call
     * before): "ef" (2 bytes) prepended. */
    iov3[0].iov_base = (char *) "abcdef";
    iov3[0].iov_len = 6;
    ev2.vsize = 1;
    ev2.size = 6;
    ev2.iov = iov3;
    ev2.binv = binv;
    pushqv = driver_pushqv(p, &ev2, 4);
    size3 = driver_sizeq(p);

    /* `driver_vec_to_buf`: truncation (`len` < the vector), saturation (the vector runs out first)
     * and `len == 0`. A vector that no `*_v` call ever saw (`iov4`), so the C's in-place `skip`
     * cannot turn it into a shorter vector first. */
    iov4[0].iov_base = (char *) "abcdef";
    iov4[0].iov_len = 6;
    memset(&ev2, 0, sizeof(ev2));
    ev2.vsize = 1;
    ev2.size = 6;
    ev2.iov = iov4;
    ev2.binv = binv;

    memset(buf, 0, sizeof(buf));
    r4 = driver_vec_to_buf(&ev2, buf, 4);
    memcpy(bytes4, buf, 4);
    bytes4[4] = '\0';

    memset(buf, 0, sizeof(buf));
    rbig = driver_vec_to_buf(&ev2, buf, 100);
    memcpy(bytesbig, buf, 6);
    bytesbig[6] = '\0';

    memset(buf, 0, sizeof(buf));
    rzero = driver_vec_to_buf(&ev2, buf, 0);

    /* Drain: a non-empty queue prevents a clean port close. */
    (void) driver_deq(p, driver_sizeq(p));

    snprintf(report, sizeof(report),
             "push=%d,%d size=%zu enqbin=%d pushbin=%d zero=%d size2=%zu enqv=%d size_mid=%zu "
             "pushqv=%d size3=%zu vec4=%zu bytes4=%s vecbig=%zu bytesbig=%s veczero=%zu",
             c1, c2, (size_t) size1, eb, pb, zero, (size_t) size2, enqv, (size_t) size_mid,
             pushqv, (size_t) size3, (size_t) r4, bytes4, (size_t) rbig, bytesbig,
             (size_t) rzero);
    return publish(st, report);
}

/* Command 3: does `driver_enqv` advance the caller's vector in place? */
static ErlDrvSSizeT qd_case3(qd_state *st, char **rbuf, ErlDrvSizeT rlen) {
    char report[512];
    ErlDrvPort p = st->port;
    ErlDrvBinary *binv[1];
    SysIOVec iov[1];
    ErlIOVec ev;
    SysIOVec *v;
    int enqv, vlen = -1, moved;
    ErlDrvSizeT size, entry, iov0_len, ev_size;

    (void) rbuf;
    (void) rlen;
    binv[0] = NULL;
    iov[0].iov_base = (char *) "abcdef";
    iov[0].iov_len = 6;
    ev.vsize = 1;
    ev.size = 6;
    ev.iov = iov;
    ev.binv = binv;

    enqv = driver_enqv(p, &ev, 2);          /* queues "cdef" (4 bytes) */
    size = driver_sizeq(p);
    entry = 0;
    v = driver_peekq(p, &vlen);
    if (v != NULL && vlen > 0)
        entry = v[0].iov_len;

    /* The oracle's `skip()` rewrites `iov[0]` in place: base += 2 ("cdef"), len -= 2 (4). This
     * implementation only reads the vector, so `iov[0].iov_len` stays 6 and the base still points
     * at "abcdef". `ev.size` is left alone by both. */
    iov0_len = ev.iov[0].iov_len;
    ev_size = ev.size;
    moved = (memcmp(ev.iov[0].iov_base, "cdef", 4) == 0);

    /* 4 queued bytes (the declared 6 - skip 2), so this drains cleanly on both runtimes. */
    (void) driver_deq(p, size);

    snprintf(report, sizeof(report),
             "enqv=%d size=%zu vlen=%d entry=%zu ev_iov0_len=%zu ev_size=%zu vector_moved=%d",
             enqv, (size_t) size, vlen, (size_t) entry, (size_t) iov0_len, (size_t) ev_size, moved);
    return publish(st, report);
}

/* Command 4: the empty-queue readings on a fresh port. */
static ErlDrvSSizeT qd_case4(qd_state *st, char **rbuf, ErlDrvSizeT rlen) {
    char report[512];
    ErlDrvPort p = st->port;
    SysIOVec *v;
    ErlIOVec ev;
    int vlen = -1;
    ErlDrvSizeT size, pk, dq0, dq1;

    (void) rbuf;
    (void) rlen;
    /* Poison the vector so `driver_peekqv`'s zeroing is observable. */
    memset(&ev, 0xAA, sizeof(ev));

    size = driver_sizeq(p);
    v = driver_peekq(p, &vlen);
    pk = driver_peekqv(p, &ev);
    dq0 = driver_deq(p, 0);                 /* the no-op: returns the current size */
    dq1 = driver_deq(p, 1);                 /* 1 byte on an empty queue: the C's failure */

    snprintf(report, sizeof(report),
             "size=%zu peekq_null=%d vlen=%d peekqv=%zu ev.vsize=%d ev.size=%zu ev.iov_null=%d "
             "ev.binv_null=%d deq0=%zu deq1=%zu",
             (size_t) size, (v == NULL), vlen, (size_t) pk, ev.vsize, (size_t) ev.size,
             (ev.iov == NULL), (ev.binv == NULL), (size_t) dq0, (size_t) dq1);
    return publish(st, report);
}

/* The driver "ioctl" behind `port_control/3`. */
static ErlDrvSSizeT qd_control(ErlDrvData drv_data, unsigned int command,
                               char *buf, ErlDrvSizeT len,
                               char **rbuf, ErlDrvSizeT rlen) {
    qd_state *st = (qd_state *) drv_data;
    (void) buf;
    (void) len;
    (void) rbuf;
    (void) rlen;
    switch (command) {
    case 1:
        return qd_case1(st, rbuf, rlen);
    case 2:
        return qd_case2(st, rbuf, rlen);
    case 3:
        return qd_case3(st, rbuf, rlen);
    case 4:
        return qd_case4(st, rbuf, rlen);
    default:
        return publish(st, "unknown command");
    }
}

static ErlDrvEntry qd_driver_entry = {
    .init = NULL,
    .start = qd_start,
    .stop = qd_stop,
    .output = NULL,
    .ready_input = NULL,
    .ready_output = NULL,
    .driver_name = "qd",
    .finish = NULL,
    .handle = NULL,
    .control = qd_control,
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

DRIVER_INIT(qd) {
    return &qd_driver_entry;
}
