/*
 * A REAL NIF shared object — the end-to-end fixture of the `beam_abi` NIF C ABI (contract A.3).
 *
 * It is compiled by the integration test `tests/nif_end_to_end.rs` with the system `cc` against
 * the system `erl_nif.h` and loaded through `beam_abi::nif::loader::load`. Its `enif_*` imports are
 * deliberately left undefined by the linker: they are resolved (`RTLD_NOW`) out of the *loading
 * process*'s dynamic symbol table, which the test binary publishes via `-Wl,--export-dynamic`
 * (`build.rs`). A successful load is therefore the empirical proof that the Rust `enif_*` host
 * functions are exported as dynamic symbols and bind a real C NIF.
 *
 * `ERL_NIF_INIT` seeds `ErlNifEntry.name` with the stringified module name, so the only module this
 * library may be loaded as is `test_nif`.
 *
 * Portability note: this fixture uses ONLY `enif_*` functions whose table slots are populated on
 * stable Rust. In particular it never calls a C-variadic `enif_*` (`enif_make_tuple`,
 * `enif_make_list`, `enif_set_option`, `enif_fprintf`) — those table slots are `None` and their
 * symbols are not exported. `enif_make_tuple_from_array` stands in for `enif_make_tuple`.
 */
#include <erl_nif.h>

/* `load` succeeds and stores no private state. */
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

/* add/2 -> A + B; badarg on a wrong arity/type. */
static ERL_NIF_TERM add(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    int a, b;
    if (argc != 2 || !enif_get_int(env, argv[0], &a) || !enif_get_int(env, argv[1], &b))
        return enif_make_badarg(env);
    return enif_make_int(env, a + b);
}

/* hello/0 -> the atom `hello`. */
static ERL_NIF_TERM hello(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void)argc;
    (void)argv;
    return enif_make_atom(env, "hello");
}

/* reverse/1 -> the reversal of a proper list; badarg otherwise. */
static ERL_NIF_TERM reverse(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ERL_NIF_TERM out = 0;
    if (argc != 1 || !enif_make_reverse_list(env, argv[0], &out))
        return enif_make_badarg(env);
    return out;
}

/* raise/0 -> raises `badarg` (the Erlang name is `raise`, `raise` being a libc symbol). */
static ERL_NIF_TERM raise_nif(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void)argc;
    (void)argv;
    return enif_make_badarg(env);
}

/* make_tuple/2 -> the 2-tuple {A, B}. */
static ERL_NIF_TERM make_tuple(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ERL_NIF_TERM arr[2];
    if (argc != 2)
        return enif_make_badarg(env);
    arr[0] = argv[0];
    arr[1] = argv[1];
    return enif_make_tuple_from_array(env, arr, 2);
}

static ErlNifFunc test_nif_funcs[] = {
    {"add", 2, add, 0},
    {"hello", 0, hello, 0},
    {"reverse", 1, reverse, 0},
    {"raise", 0, raise_nif, 0},
    {"make_tuple", 2, make_tuple, 0}
};

ERL_NIF_INIT(test_nif, test_nif_funcs, load, NULL, NULL, unload)
