/*
 * A real C NIF exercising the **resource family** of the NIF C ABI (workspace contract A.3).
 *
 * Compiled at test time with the system `cc` against the system OTP `erl_nif.h` (see
 * `tests/common/mod.rs::compile_so`) and driven through `beam_abi`'s own `dlopen` loader
 * (`tests/nif_resource.rs`). The `.so` leaves every `enif_*` call it makes **undefined**; the
 * loading test binary binds them out of its dynamic symbol table (the `build.rs`
 * `-Wl,--export-dynamic` mechanism), so `dlopen(RTLD_NOW)` only succeeds when this crate really
 * defines the symbols used below.
 *
 * Every `enif_*` function referenced here is a slot `beam_abi` populates (`Some`); `RTLD_NOW` would
 * fail on any unresolved import. The referenced set is exactly the eleven resource slots plus the
 * term-building/term-inspecting helpers this fixture needs.
 *
 * What it proves (asserted from Rust in `tests/nif_resource.rs`):
 *   - `enif_open_resource_type` / `_x` / `enif_init_resource_type` open a type and report the
 *     oracle's `*tried` values (`ERL_NIF_RT_CREATE` on create, `ERL_NIF_RT_TAKEOVER` on a failed
 *     takeover of a fresh name, the caller's `flags` otherwise);
 *   - `enif_alloc_resource` + `enif_sizeof_resource` round-trip the payload size;
 *   - `enif_make_resource` yields a term that `enif_get_resource` resolves to the same payload
 *     **only** for the exact type (0 for another type, a plain int, a plain binary);
 *   - **a resource TERM keeps its object alive across a release** (`post_release/0` runs the exact
 *     measured sequence `alloc -> enif_make_resource -> enif_release_resource -> enif_get_resource`
 *     and reads 42 back through the pointer; `roundtrip/0`'s `stale_get` slot reports the same after
 *     the final release) — the oracle holds the same reference the magic ref does;
 *   - **the round trip hands back the payload a NIF wrote** (`roundtrip_value/0` writes 42 into the
 *     resource and reads it back through the pointer `enif_get_resource` returned — the oracle
 *     prints `42` there);
 *   - **the destructor's count and ordering for an UNEXPORTED object** (`dtor_probe/0` releases two
 *     tagged objects that were never made into terms, and `dtor_state/0` reports how many destructors
 *     ran, in which order, with which payload byte); and
 *   - **process liveness** (`liveness/1` — `enif_is_process_alive` on the calling process, on an
 *     undefined pid, and on a caller-supplied "other" pid, plus `enif_is_current_process_alive`);
 *   - `enif_make_resource_binary` returns a binary whose bytes are the given bytes and which is
 *     itself a resource handle;
 *   - `enif_keep_resource`/`enif_release_resource` balance, and the destructor runs on the release
 *     that reaches zero, receiving the payload (its first byte is recorded);
 *   - `enif_dynamic_resource_call` returns 0 and invokes the type's `dyncall`, and returns non-zero
 *     for a non-resource term, an unknown type name, a type without a `dyncall`, and a name/term
 *     mismatch.
 *
 * The module state is plain C statics with a `dtor`-runs counter, read back through NIFs so the
 * Rust test can assert on them.
 *
 * NOTE (documented substitutions): the module is keyed by type *name* (the Rust env carries no
 * module identity), and a created type is visible immediately (there is no `load`-session
 * boundary). The two cases where that is observable are returned explicitly by `reopen_main/0` and
 * called out in the Rust test.
 */

#include <erl_nif.h>
#include <string.h>
#include <stdint.h>

/* ---------------------------------------------------------------------------------------------
 * Observations recorded by the `load` callback
 * ------------------------------------------------------------------------------------------- */

static int tried_main = -1;      /* *tried for res_main (CREATE|TAKEOVER on a fresh name) */
static int tried_absent = -1;    /* *tried for res_absent (TAKEOVER only on a fresh name) */
static int absent_is_null = -1;  /* 1 iff the res_absent open returned NULL */
static int tried_x = -1;         /* *tried for res_x (open_resource_type_x) */
static int tried_dy = -1;        /* *tried for res_dy (init_resource_type) */

static ErlNifResourceType *res_main = NULL; /* dtor only */
static ErlNifResourceType *res_x = NULL;    /* dtor + stop + down */
static ErlNifResourceType *res_dy = NULL;   /* dtor + dyncall */

/* ---------------------------------------------------------------------------------------------
 * Callback and destructor counters
 * ------------------------------------------------------------------------------------------- */

static int main_dtor_runs = 0;
static int main_dtor_last_byte = -1;
/* The payload tags of every `main_dtor` run, most recent last (one byte each). `dtor_probe/0`
 * resets it so `dtor_state/0` reports the probe's own destructor ORDER (`0x1122` = the object
 * tagged `0x11` was destroyed before the one tagged `0x22`). */
static unsigned main_dtor_order = 0;
static int dtor_base = 0; /* `main_dtor_runs` when the last `dtor_probe/0` started */
static int x_dtor_runs = 0;
static int x_stop_runs = 0;
static int x_down_runs = 0;
static int dy_dyncall_runs = 0;

static void main_dtor(ErlNifEnv *env, void *obj) {
    (void)env;
    main_dtor_runs++;
    if (obj != NULL) {
        unsigned b = (unsigned)((const unsigned char *)obj)[0];
        main_dtor_last_byte = (int)b;
        main_dtor_order = (main_dtor_order << 8) | b;
    }
}

static void x_dtor(ErlNifEnv *env, void *obj) {
    (void)env;
    (void)obj;
    x_dtor_runs++;
}

static void x_stop(ErlNifEnv *env, void *obj, ErlNifEvent event, int is_direct_call) {
    (void)env;
    (void)obj;
    (void)event;
    (void)is_direct_call;
    x_stop_runs++;
}

static void x_down(ErlNifEnv *env, void *obj, ErlNifPid *pid, ErlNifMonitor *mon) {
    (void)env;
    (void)obj;
    (void)pid;
    (void)mon;
    x_down_runs++;
}

static void dy_dtor(ErlNifEnv *env, void *obj) {
    (void)env;
    (void)obj;
}

static void dy_dyncall(ErlNifEnv *env, void *obj, void *call_data) {
    (void)env;
    dy_dyncall_runs++;
    if (obj != NULL && call_data != NULL) {
        *(int *)call_data = 4242;
    }
}

/* ---------------------------------------------------------------------------------------------
 * Load / unload
 * ------------------------------------------------------------------------------------------- */

static int load(ErlNifEnv *env, void **priv_data, ERL_NIF_TERM load_info) {
    ErlNifResourceFlags tried;
    ErlNifResourceType *absent;
    ErlNifResourceTypeInit init;

    (void)priv_data;
    (void)load_info;

    /* A fresh name with CREATE|TAKEOVER: created, and `*tried == ERL_NIF_RT_CREATE`. */
    tried = (ErlNifResourceFlags)0;
    res_main = enif_open_resource_type(env, NULL, "res_main", main_dtor,
                                       (ErlNifResourceFlags)(ERL_NIF_RT_CREATE | ERL_NIF_RT_TAKEOVER),
                                       &tried);
    tried_main = (int)tried;

    /* A fresh name with TAKEOVER only: no type, and `*tried` stays == the caller's flags. */
    tried = (ErlNifResourceFlags)0;
    absent = enif_open_resource_type(env, NULL, "res_absent", main_dtor,
                                     ERL_NIF_RT_TAKEOVER, &tried);
    absent_is_null = (absent == NULL) ? 1 : 0;
    tried_absent = (int)tried;

    /* open_resource_type_x reads dtor/stop/down (members == 3). */
    memset(&init, 0, sizeof(init));
    init.dtor = x_dtor;
    init.stop = x_stop;
    init.down = x_down;
    init.members = 3;
    tried = (ErlNifResourceFlags)0;
    res_x = enif_open_resource_type_x(env, "res_x", &init,
                                      (ErlNifResourceFlags)(ERL_NIF_RT_CREATE | ERL_NIF_RT_TAKEOVER),
                                      &tried);
    tried_x = (int)tried;

    /* init_resource_type honours `members`, so `dyncall` can be installed (members == 4). */
    memset(&init, 0, sizeof(init));
    init.dtor = dy_dtor;
    init.members = 4;
    init.dyncall = dy_dyncall;
    tried = (ErlNifResourceFlags)0;
    res_dy = enif_init_resource_type(env, "res_dy", &init,
                                     (ErlNifResourceFlags)(ERL_NIF_RT_CREATE | ERL_NIF_RT_TAKEOVER),
                                     &tried);
    tried_dy = (int)tried;

    if (res_main == NULL || res_x == NULL || res_dy == NULL) {
        return 1; /* veto the load, like a NIF that cannot set up its types */
    }
    return 0;
}

static void unload(ErlNifEnv *env, void *priv_data) {
    (void)env;
    (void)priv_data;
}

/* ---------------------------------------------------------------------------------------------
 * NIFs
 * ------------------------------------------------------------------------------------------- */

/* Small-int width used by every element below (32-bit `int` -> `enif_make_int`). */
static ERL_NIF_TERM small(ErlNifEnv *env, int value) {
    return enif_make_int(env, value);
}

/*
 * load_status/0 -> {tried_main, tried_absent, absent_is_null, tried_x, tried_dy,
 *                   main_nonnull, x_nonnull, dy_nonnull}
 */
static ERL_NIF_TERM load_status(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[]) {
    ERL_NIF_TERM out[8];
    (void)argc;
    (void)argv;
    out[0] = small(env, tried_main);
    out[1] = small(env, tried_absent);
    out[2] = small(env, absent_is_null);
    out[3] = small(env, tried_x);
    out[4] = small(env, tried_dy);
    out[5] = small(env, res_main != NULL);
    out[6] = small(env, res_x != NULL);
    out[7] = small(env, res_dy != NULL);
    return enif_make_tuple_from_array(env, out, 8);
}

/*
 * reopen_main/0 -> {nonnull, tried} for opening the already-created "res_main" with TAKEOVER only.
 *
 * The `*tried` value agrees with the oracle (`ERL_NIF_RT_TAKEOVER`); the non-null-ness does not —
 * the oracle refuses any open outside a `load` callback, while this runtime has no `load`-session
 * boundary (documented substitution).
 */
static ERL_NIF_TERM reopen_main(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[]) {
    ERL_NIF_TERM out[2];
    ErlNifResourceFlags tried = (ErlNifResourceFlags)0;
    ErlNifResourceType *type;
    (void)argc;
    (void)argv;
    type = enif_open_resource_type(env, NULL, "res_main", main_dtor, ERL_NIF_RT_TAKEOVER, &tried);
    out[0] = small(env, type == NULL ? 0 : 1);
    out[1] = small(env, (int)tried);
    return enif_make_tuple_from_array(env, out, 2);
}

/*
 * create_only_existing/0 -> {nonnull, tried} for opening the already-created "res_main" with CREATE
 * only: not taken over, so no type and `*tried` stays == the caller's flags. This agrees with the
 * oracle.
 */
static ERL_NIF_TERM create_only_existing(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[]) {
    ERL_NIF_TERM out[2];
    ErlNifResourceFlags tried = (ErlNifResourceFlags)0;
    ErlNifResourceType *type;
    (void)argc;
    (void)argv;
    type = enif_open_resource_type(env, NULL, "res_main", main_dtor, ERL_NIF_RT_CREATE, &tried);
    out[0] = small(env, type == NULL ? 0 : 1);
    out[1] = small(env, (int)tried);
    return enif_make_tuple_from_array(env, out, 2);
}

/*
 * roundtrip/0 -> {sizeof, same_ptr, wrongtype_untouched, resbin_same_ptr, resbin_bytes_ok,
 *                 dtor_after_balance, dtor_after_final, dtor_first_byte, stale_get,
 *                 r_same, r_wrongtype, r_int, r_binary, r_resbin,
 *                 sizeof_x, x_dtor_runs}
 *
 * `obj` is exported as a resource term (`enif_make_resource`) and as a resource binary
 * (`enif_make_resource_binary`), so it holds three references (alloc + term + resbin) plus the
 * explicit `keep`; neither release below reaches zero, so no destructor runs here
 * (`dtor_after_final == 0`, `dtor_first_byte == -1`) and the term keeps resolving
 * (`stale_get == 1`). Oracle values: `{13,1,1,1,1,0,0,-1,1,1,0,0,0,1,9,0}` — the last element
 * differs (the oracle's `x_dtor_runs` is 0 there because the C schedules the destructor on an aux
 * thread; this runtime runs it synchronously — see the fixture header and the module docs).
 */
static ERL_NIF_TERM roundtrip(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[]) {
    ERL_NIF_TERM out[16];
    ERL_NIF_TERM term;
    ERL_NIF_TERM resbin;
    ErlNifBinary bin;
    void *obj;
    void *got;
    void *sentinel = (void *)(uintptr_t)0xdeadbeef;
    unsigned char *p;
    size_t sz;
    int i;
    int r_same;
    int r_wrongtype;
    int r_int;
    int r_binary;
    int r_resbin;
    int dtor_base;
    int x_dtor_base;
    (void)argc;
    (void)argv;

    /* Every destructor count below is a DELTA from this call's start, so the result does not depend
     * on which NIF the test called first. */
    dtor_base = main_dtor_runs;
    x_dtor_base = x_dtor_runs;
    /* --- allocate a 13-byte payload and report its size ---------------------------------- */
    obj = enif_alloc_resource(res_main, 13);
    if (obj == NULL) {
        return enif_make_badarg(env);
    }
    p = (unsigned char *)obj;
    for (i = 0; i < 13; i++) {
        p[i] = (unsigned char)(0x40 + i);
    }
    sz = enif_sizeof_resource(obj);

    /* --- make a resource term and resolve it -------------------------------------------- */
    term = enif_make_resource(env, obj);

    got = sentinel;
    r_same = enif_get_resource(env, term, res_main, &got);
    out[1] = small(env, got == obj); /* writes the same payload pointer */

    got = sentinel;
    r_wrongtype = enif_get_resource(env, term, res_x, &got);
    out[2] = small(env, got == sentinel); /* *objp untouched on a type mismatch */

    got = sentinel;
    r_int = enif_get_resource(env, enif_make_int(env, 42), res_main, &got);

    {
        ERL_NIF_TERM plain;
        unsigned char *d = enif_make_new_binary(env, 3, &plain);
        d[0] = 1;
        d[1] = 2;
        d[2] = 3;
        got = sentinel;
        r_binary = enif_get_resource(env, plain, res_main, &got);
    }

    /* --- the resource binary: same bytes, and also a resource handle -------------------- */
    resbin = enif_make_resource_binary(env, obj, (const void *)"abcd", 4);
    got = sentinel;
    r_resbin = enif_get_resource(env, resbin, res_main, &got);
    out[3] = small(env, got == obj);
    memset(&bin, 0, sizeof(bin));
    out[4] = small(env,
                   enif_inspect_binary(env, resbin, &bin) && bin.size == 4 &&
                       memcmp(bin.data, "abcd", 4) == 0);

    /* --- keep / release balance --------------------------------------------------------- */
    enif_keep_resource(obj);
    enif_release_resource(obj);
    out[5] = small(env, main_dtor_runs - dtor_base); /* still alive: 0 */

    /* --- the final release: the term's reference keeps the object alive ----------------- */
    enif_release_resource(obj);
    /* The term and the resource binary each hold a reference, so this release only takes the count
     * to 2 — no destructor, exactly as on the oracle. */
    out[6] = small(env, main_dtor_runs - dtor_base);
    out[7] = small(env, main_dtor_last_byte); /* -1: no destructor has run */

    /* --- the term still resolves: it holds the object's own reference ------------------- */
    got = sentinel;
    out[8] = small(env, enif_get_resource(env, term, res_main, &got));

    out[0] = small(env, (int)sz);
    out[9] = small(env, r_same);
    out[10] = small(env, r_wrongtype);
    out[11] = small(env, r_int);
    out[12] = small(env, r_binary);
    out[13] = small(env, r_resbin);

    /* --- a type carrying a `down` callback reports its payload size too ------------------ */
    {
        void *xo = enif_alloc_resource(res_x, 9);
        out[14] = small(env, xo == NULL ? -1 : (int)enif_sizeof_resource(xo));
        enif_release_resource(xo); /* runs x_dtor */
        out[15] = small(env, x_dtor_runs - x_dtor_base);
    }

    return enif_make_tuple_from_array(env, out, 16);
}

/*
 * dyn_call/0 -> {c_ok, call_data, c_bad_name, c_nonresource, c_no_dyncall, c_wrong_name,
 *                c_wrong_module, dyncall_runs}
 *
 * `c_ok` must be 0 (the call was made) and must have written `call_data`; every failure case must
 * be non-zero. `c_wrong_module` is 0 here because the runtime compares only the type *name* (the
 * env carries no module identity — documented substitution); the oracle would return non-zero.
 */
static ERL_NIF_TERM dyn_call(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[]) {
    ERL_NIF_TERM out[8];
    ERL_NIF_TERM mod_atom;
    ERL_NIF_TERM other_mod;
    ERL_NIF_TERM name_dy;
    ERL_NIF_TERM name_main;
    ERL_NIF_TERM name_missing;
    ERL_NIF_TERM t_dy;
    ERL_NIF_TERM t_main;
    void *o_dy;
    void *o_main;
    int call_data;
    int c_ok;
    int c_bad_name;
    int c_nonresource;
    int c_no_dyncall;
    int c_wrong_name;
    int c_wrong_module;
    (void)argc;
    (void)argv;

    mod_atom = enif_make_atom(env, "nif_resource");
    other_mod = enif_make_atom(env, "not_nif_resource");
    name_dy = enif_make_atom(env, "res_dy");
    name_main = enif_make_atom(env, "res_main");
    name_missing = enif_make_atom(env, "res_missing");

    o_dy = enif_alloc_resource(res_dy, 4);
    t_dy = enif_make_resource(env, o_dy);
    o_main = enif_alloc_resource(res_main, 4);
    t_main = enif_make_resource(env, o_main);

    call_data = -1;
    c_ok = enif_dynamic_resource_call(env, mod_atom, name_dy, t_dy, &call_data);
    out[1] = small(env, call_data); /* the dyncall wrote 4242 */

    call_data = -1;
    c_bad_name = enif_dynamic_resource_call(env, mod_atom, name_missing, t_dy, &call_data);
    c_nonresource =
        enif_dynamic_resource_call(env, mod_atom, name_dy, enif_make_int(env, 7), &call_data);
    /* matching name but the type has no dyncall */
    c_no_dyncall = enif_dynamic_resource_call(env, mod_atom, name_main, t_main, &call_data);
    /* matching dyncall-bearing name but a resource of another type */
    c_wrong_name = enif_dynamic_resource_call(env, mod_atom, name_dy, t_main, &call_data);
    /* wrong module atom, right name: this runtime ignores the module (documented) */
    c_wrong_module = enif_dynamic_resource_call(env, other_mod, name_dy, t_dy, &call_data);

    enif_release_resource(o_dy);
    enif_release_resource(o_main);

    out[0] = small(env, c_ok);
    out[2] = small(env, c_bad_name);
    out[3] = small(env, c_nonresource);
    out[4] = small(env, c_no_dyncall);
    out[5] = small(env, c_wrong_name);
    out[6] = small(env, c_wrong_module);
    out[7] = small(env, dy_dyncall_runs); /* c_ok + c_wrong_module */
    return enif_make_tuple_from_array(env, out, 8);
}

/*
 * roundtrip_value/0 -> {get_ok, value, wrongtype_get, wrongtype_untouched, int_get}
 *
 * The value round trip this pins: `enif_make_resource` + `enif_get_resource` must round-trip, so the
 * `int` a NIF wrote into the resource is readable back through the pointer `enif_get_resource` handed
 * out. The oracle prints `{1,42,0,1,0}` there, and so does this runtime.
 */
static ERL_NIF_TERM roundtrip_value(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[]) {
    ERL_NIF_TERM out[5];
    ERL_NIF_TERM term;
    void *obj;
    void *got;
    void *sentinel = (void *)(uintptr_t)0xdeadbeef;
    int ok;
    int value = -1;
    int wrong;
    int wrong_untouched;
    int int_get;
    (void)argc;
    (void)argv;

    obj = enif_alloc_resource(res_main, sizeof(int));
    if (obj == NULL) {
        return enif_make_badarg(env);
    }
    *(int *)obj = 42;
    term = enif_make_resource(env, obj);

    got = sentinel;
    ok = enif_get_resource(env, term, res_main, &got);
    if (ok && got != NULL && got != sentinel) {
        value = *(int *)got;
    }

    got = sentinel;
    wrong = enif_get_resource(env, term, res_x, &got);
    wrong_untouched = (got == sentinel) ? 1 : 0;

    got = sentinel;
    int_get = enif_get_resource(env, enif_make_int(env, 42), res_main, &got);

    /* The single reference `enif_alloc_resource` took. The resource *term* holds its own reference,
     * so the object stays alive after this release — see `post_release/0` below, which reads the
     * payload back after a release. */
    enif_release_resource(obj);

    out[0] = small(env, ok);
    out[1] = small(env, value);
    out[2] = small(env, wrong);
    out[3] = small(env, wrong_untouched);
    out[4] = small(env, int_get);
    return enif_make_tuple_from_array(env, out, 5);
}

/*
 * post_release/0 -> {get_ok, value, dtor_runs_since_alloc}
 *
 * The exact sequence of the measured A.3 divergence:
 *
 *   alloc -> enif_make_resource -> enif_release_resource -> enif_get_resource
 *
 * The resource TERM holds its own reference (the reference the C's magic ref holds), so the release
 * only drops the allocator's reference and the object — and its payload — stays alive: the oracle
 * reads `42` back. A runtime that freed the block at reference-count zero would hand this code a
 * dangling pointer. Measured oracle value: `{1,42,0}` (no destructor ran, because the term still
 * holds a reference that only GC would release).
 */
static ERL_NIF_TERM post_release(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[]) {
    ERL_NIF_TERM out[3];
    ERL_NIF_TERM term;
    void *obj;
    void *got;
    void *sentinel = (void *)(uintptr_t)0xdeadbeef;
    int ok;
    int value = -1;
    int base = main_dtor_runs;
    (void)argc;
    (void)argv;

    obj = enif_alloc_resource(res_main, sizeof(int));
    if (obj == NULL) {
        return enif_make_badarg(env);
    }
    *(int *)obj = 42;
    term = enif_make_resource(env, obj);
    enif_release_resource(obj); /* drop the allocator's reference; the term keeps its own */

    got = sentinel;
    ok = enif_get_resource(env, term, res_main, &got);
    if (ok && got != NULL && got != sentinel) {
        value = *(int *)got;
    }

    out[0] = small(env, ok);
    out[1] = small(env, value);
    out[2] = small(env, main_dtor_runs - base);
    return enif_make_tuple_from_array(env, out, 3);
}

/*
 * liveness/1 -> {self_ok, self_alive, current_alive, undefined_flag, undefined_alive,
 *                other_get_ok, other_alive}
 *
 * `enif_is_process_alive` on the *calling* process must be `1` — the oracle's `erts_proc_lookup`
 * always finds it (`erl_nif.c:2330`). `enif_is_current_process_alive` is `1` too, an undefined pid
 * is never alive, and the caller-supplied pid (`argv[0]`) is the "other" process: a live one must
 * answer `1`, a dead one `0`.
 */
static ERL_NIF_TERM liveness(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[]) {
    ERL_NIF_TERM out[7];
    ErlNifPid self;
    ErlNifPid undefined;
    ErlNifPid other;
    int self_ok;
    int other_get_ok;
    (void)argc;

    self_ok = (enif_self(env, &self) != NULL);

    enif_set_pid_undefined(&undefined);

    other_get_ok = enif_get_local_pid(env, argv[0], &other);
    if (!other_get_ok) {
        enif_set_pid_undefined(&other);
    }

    out[0] = small(env, self_ok);
    out[1] = small(env, self_ok ? enif_is_process_alive(env, &self) : -1);
    out[2] = small(env, enif_is_current_process_alive(env));
    out[3] = small(env, enif_is_pid_undefined(&undefined));
    out[4] = small(env, enif_is_process_alive(env, &undefined));
    out[5] = small(env, other_get_ok);
    out[6] = small(env, other_get_ok ? enif_is_process_alive(env, &other) : -1);
    return enif_make_tuple_from_array(env, out, 7);
}

/*
 * dtor_probe/0 -> {allocs_ok, released_to_zero}
 *
 * Releases two tagged objects (`0x11`, `0x22`) to reference count zero: one of them only after a
 * `keep`, and with a `NULL` release and a `NULL` keep in between. Neither object is exported as a
 * term, so each really reaches zero and its destructor runs. Nothing about the destructor is read
 * here, because the C runs destructors on a *different* (misc aux-work) thread — the counts and the
 * order are read by `dtor_state/0` afterwards. This runtime runs them synchronously, so the counts
 * are already complete when `dtor_state/0` runs.
 *
 * Rust-only, and deliberately so: the oracle **cannot** run this probe, because the C's
 * `enif_release_resource(NULL)`/`enif_keep_resource(NULL)` dereference their operand (`erl_nif.c:3164`
 * / `3177`) and segfault the VM (measured). This runtime treats a `NULL` keep/release as a no-op
 * (documented in `src/nif/enif/resources.rs`) and continues.
 */
static ERL_NIF_TERM dtor_probe(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[]) {
    ERL_NIF_TERM out[2];
    void *a;
    void *b;
    (void)argc;
    (void)argv;

    dtor_base = main_dtor_runs;
    main_dtor_order = 0;

    a = enif_alloc_resource(res_main, 2);
    b = enif_alloc_resource(res_main, 2);
    if (a == NULL || b == NULL) {
        return enif_make_badarg(env);
    }
    ((unsigned char *)a)[0] = 0x11;
    ((unsigned char *)b)[0] = 0x22;

    enif_keep_resource(a);       /* a: 2 */
    enif_release_resource(a);    /* a: 1 — a live keep, so no destructor */
    enif_release_resource(NULL); /* a NULL release is a no-op */
    enif_keep_resource(NULL);    /* ...and so is a NULL keep */
    enif_release_resource(a);    /* a: 0 — destructor #1 (tag 0x11) */
    enif_release_resource(b);    /* b: 0 — destructor #2 (tag 0x22) */

    out[0] = small(env, 1); /* both allocations succeeded */
    out[1] = small(env, 2); /* two objects were released to zero */
    return enif_make_tuple_from_array(env, out, 2);
}

/*
 * dtor_state/0 -> {runs_since_probe, order_code, last_byte}
 *
 * `runs_since_probe` is how many `res_main` destructors ran since `dtor_probe/0` started (the C
 * schedules these off-thread, so its count would lag; this runtime runs them synchronously).
 * `order_code` is the tags of those destructors, most recent last: `0x1122` = 4386 means the object
 * tagged `0x11` was destroyed before the one tagged `0x22`.
 */
static ERL_NIF_TERM dtor_state(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[]) {
    ERL_NIF_TERM out[3];
    (void)argc;
    (void)argv;

    out[0] = small(env, main_dtor_runs - dtor_base);
    out[1] = small(env, (int)main_dtor_order);
    out[2] = small(env, main_dtor_last_byte);
    return enif_make_tuple_from_array(env, out, 3);
}

static ErlNifFunc nif_funcs[] = {
    {"load_status", 0, load_status, 0},
    {"reopen_main", 0, reopen_main, 0},
    {"create_only_existing", 0, create_only_existing, 0},
    {"roundtrip", 0, roundtrip, 0},
    {"roundtrip_value", 0, roundtrip_value, 0},
    {"liveness", 1, liveness, 0},
    {"dtor_probe", 0, dtor_probe, 0},
    {"dtor_state", 0, dtor_state, 0},
    {"dyn_call", 0, dyn_call, 0},
    {"post_release", 0, post_release, 0},
};

ERL_NIF_INIT(nif_resource, nif_funcs, load, NULL, NULL, unload)
