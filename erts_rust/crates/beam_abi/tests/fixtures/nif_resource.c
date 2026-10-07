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
static int x_dtor_runs = 0;
static int x_stop_runs = 0;
static int x_down_runs = 0;
static int dy_dyncall_runs = 0;

static void main_dtor(ErlNifEnv *env, void *obj) {
    (void)env;
    main_dtor_runs++;
    if (obj != NULL) {
        main_dtor_last_byte = (int)((const unsigned char *)obj)[0];
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
    (void)argc;
    (void)argv;

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
    out[5] = small(env, main_dtor_runs); /* still alive: 0 */

    /* --- the release that reaches zero runs the destructor ------------------------------ */
    enif_release_resource(obj);
    out[6] = small(env, main_dtor_runs);
    out[7] = small(env, main_dtor_last_byte); /* 0x40 */

    /* --- the (now stale) term no longer resolves ---------------------------------------- */
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
        out[15] = small(env, x_dtor_runs);
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

static ErlNifFunc nif_funcs[] = {
    {"load_status", 0, load_status, 0},
    {"reopen_main", 0, reopen_main, 0},
    {"create_only_existing", 0, create_only_existing, 0},
    {"roundtrip", 0, roundtrip, 0},
    {"dyn_call", 0, dyn_call, 0},
};

ERL_NIF_INIT(nif_resource, nif_funcs, load, NULL, NULL, unload)
