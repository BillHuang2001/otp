/*
 * A REAL NIF shared object that pins the **shape rules of `enif_inspect_iolist_as_binary`**
 * (workspace contract A.3).
 *
 * It is compiled by the integration test `tests/enif.rs` with the system `cc` against the system
 * `erl_nif.h` and loaded through `beam_abi::nif::loader::load`; every `enif_*` it calls is left
 * undefined by the linker and is bound at `dlopen(RTLD_NOW)` out of the *loading process*'s dynamic
 * symbol table (which the test binary publishes via `-Wl,--export-dynamic`, `build.rs`). The very
 * same `.so` is loaded by the *system* Erlang as the oracle, so each NIF's result is comparable
 * between this runtime and the real one.
 *
 * Ground truth: `erts/emulator/beam/erl_nif.c:1385-1407` (the `is_bitstring` / `is_nil` /
 * `erts_iolist_size` / `erts_iolist_to_buf` sequence) over `erl_iolist.c:114-263`
 * (`generic_iolist_size`, called with `Unit = 8`). An integer is only ever an iolist *element in
 * head position* (a byte, `0..=255`); a bare integer, a non-byte integer, an improper-list tail and
 * a non-byte-aligned bitstring are all a type error, and `[]` is accepted with a non-NULL dummy
 * `data` and `size == 0`.
 *
 * `ERL_NIF_INIT` seeds `ErlNifEntry.name` with the stringified module name, so the only module this
 * library may be loaded as is `test_nif_iolist`.
 *
 * Portability note: this fixture uses ONLY `enif_*` functions whose table slots are populated on
 * stable Rust. In particular it never calls a C-variadic `enif_*` (`enif_make_tuple`,
 * `enif_make_list`, `enif_set_option`, `enif_fprintf`) — those slots are `None` and their symbols
 * are not exported. `enif_make_tuple_from_array` / `enif_make_list_from_array` /
 * `enif_make_list_cell` stand in for the variadic builders.
 */
#include <erl_nif.h>
#include <string.h>

/* ------------------------------------------------------------------------- */
/* Helpers                                                                   */
/* ------------------------------------------------------------------------- */

/* The empty list, through the non-variadic constructor (there is no enif_make_empty_list). */
static ERL_NIF_TERM nil(ErlNifEnv* env) {
    ERL_NIF_TERM none[1] = {0};
    return enif_make_list_from_array(env, none, 0);
}

/* A byte (`0..=255`) or any other small integer — `enif_inspect_iolist_as_binary` must tell them
 * apart by *position*, not by range alone. */
static ERL_NIF_TERM byte(ErlNifEnv* env, int b) {
    return enif_make_int(env, b);
}

/* The cons cell `h :: t`. */
static ERL_NIF_TERM cell(ErlNifEnv* env, ERL_NIF_TERM h, ERL_NIF_TERM t) {
    return enif_make_list_cell(env, h, t);
}

/* An on-heap binary of `n` bytes read from `s` (`n == 0` yields the empty binary). */
static ERL_NIF_TERM bytes(ErlNifEnv* env, const unsigned char* s, size_t n) {
    ERL_NIF_TERM term;
    unsigned char* p = enif_make_new_binary(env, n, &term);
    if (p != NULL && s != NULL && n > 0)
        memcpy(p, s, n);
    return term;
}

/* ------------------------------------------------------------------------- */
/* The observation                                                           */
/* ------------------------------------------------------------------------- */

/* `{Accepted, Size, DataNonNull, Checksum}` for `term`:
 *
 *   Accepted     1 when `enif_inspect_iolist_as_binary` returned 1, else 0;
 *   Size         `v.size` when accepted, `-1` when refused;
 *   DataNonNull  1 when the accepted view's `data` is non-NULL (the empty iolist's dummy), else 0;
 *   Checksum     the order-sensitive checksum `sum((i + 1) * byte[i])` of the flattened bytes
 *                (`0` for an accepted empty view, `-1` when refused).
 *
 * The accepted view is then released, so the release path runs for every accepted shape — the
 * empty one included. */
static ERL_NIF_TERM observe(ErlNifEnv* env, ERL_NIF_TERM term) {
    ErlNifBinary v;
    ERL_NIF_TERM out[4];
    unsigned long long checksum = 0;
    size_t i;
    int ok;

    v.size = 0;
    v.data = NULL;
    v.ref_bin = NULL;
    ok = enif_inspect_iolist_as_binary(env, term, &v);
    if (ok && v.data != NULL && v.size > 0) {
        for (i = 0; i < v.size; i++)
            checksum += (unsigned long long) (i + 1) * v.data[i];
    }
    out[0] = enif_make_int(env, ok ? 1 : 0);
    out[1] = enif_make_int(env, ok ? (int) v.size : -1);
    out[2] = enif_make_int(env, (ok && v.data != NULL) ? 1 : 0);
    out[3] = enif_make_int(env, ok ? (int) checksum : -1);
    if (ok)
        enif_release_binary(&v);
    return enif_make_tuple_from_array(env, out, 4);
}

/* ------------------------------------------------------------------------- */
/* The terms                                                                 */
/* ------------------------------------------------------------------------- */

/* The term selector 1..10 builds. Written as a switch so the oracle run passes the same selector
 * integers through the very same construction path. */
static ERL_NIF_TERM made_term(ErlNifEnv* env, int sel) {
    int i;

    switch (sel) {
    case 1:                                     /* the recorded reproducer: a BARE byte integer */
        return byte(env, 65);
    case 2:                                     /* the recorded reproducer: the improper [1|2] */
        return cell(env, byte(env, 1), byte(env, 2));
    case 3:                                     /* a non-byte integer head */
        return cell(env, byte(env, 256), nil(env));
    case 4:                                     /* a nested empty list: byte-less, but not nil */
        return cell(env, nil(env), nil(env));
    case 5:                                     /* the empty list itself */
        return nil(env);
    case 6:                                     /* a deeply nested iolist: [[<<"ab">>,[99]]] */
        return cell(env,
                    cell(env, bytes(env, (const unsigned char*) "ab", 2),
                         cell(env, byte(env, 99), nil(env))),
                    nil(env));
    case 7:                                     /* a byte-aligned binary in TAIL position: [1|<<"xy">>] */
        return cell(env, byte(env, 1), bytes(env, (const unsigned char*) "xy", 2));
    case 8:                                     /* the empty binary */
        return bytes(env, NULL, 0);
    case 9:                                     /* a plain binary */
        return bytes(env, (const unsigned char*) "xyz", 3);
    case 10: {                                  /* a large nested iolist */
        /* `[0,1,…,299 | <<1200 bytes, byte i = i % 251>>]`, wrapped three levels deep: 1500 bytes
         * over 303 cons cells and one heap binary. */
        unsigned char big[1200];
        ERL_NIF_TERM acc;

        for (i = 0; i < 1200; i++)
            big[i] = (unsigned char) (i % 251);
        acc = bytes(env, big, 1200);
        for (i = 299; i >= 0; i--)
            acc = cell(env, byte(env, i % 256), acc);
        for (i = 0; i < 3; i++)
            acc = cell(env, acc, nil(env));
        return acc;
    }
    default:
        return nil(env);
    }
}

/* ------------------------------------------------------------------------- */
/* NIFs                                                                      */
/* ------------------------------------------------------------------------- */

/* made/1 (Selector) -> the observation for the term selector 1..10 builds in C. */
static ERL_NIF_TERM made(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    int sel = 0;
    if (argc != 1 || !enif_get_int(env, argv[0], &sel) || sel < 1 || sel > 10)
        return enif_make_badarg(env);
    return observe(env, made_term(env, sel));
}

/* probe/1 (Term) -> the same observation for a term the *caller* built (the terms C cannot build
 * through the public `enif_*` API, e.g. a non-byte-aligned bitstring). */
static ERL_NIF_TERM probe(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    if (argc != 1)
        return enif_make_badarg(env);
    return observe(env, argv[0]);
}

/* ------------------------------------------------------------------------- */
/* The `ErlNifEntry`                                                         */
/* ------------------------------------------------------------------------- */

static int load(ErlNifEnv* env, void** priv, ERL_NIF_TERM load_info) {
    (void)env;
    (void)load_info;
    *priv = NULL;
    return 0;
}

static void unload(ErlNifEnv* env, void* priv) {
    (void)env;
    (void)priv;
}

static ErlNifFunc iolist_funcs[] = {
    {"made", 1, made, 0},
    {"probe", 1, probe, 0}
};

ERL_NIF_INIT(test_nif_iolist, iolist_funcs, load, NULL, NULL, unload)
