/*
 * A real C NIF exercising `enif_priv_data/1` (workspace contract A.3).
 *
 * Compiled at test time with the system `cc` against the system OTP `erl_nif.h` (see
 * `tests/common/mod.rs::compile_so`) and driven through `beam_abi`'s own `dlopen` loader
 * (`tests/nif_priv_data.rs`). The `.so` leaves `enif_priv_data` (and the term helpers below)
 * **undefined**; the loading test binary binds them out of its dynamic symbol table (the `build.rs`
 * `-Wl,--export-dynamic` mechanism), so `dlopen(RTLD_NOW)` only succeeds when this crate really
 * defines the symbols used here.
 *
 * What it proves: `enif_priv_data(env)` answers the pointer this library's own `load` callback
 * stored in `*priv_data` — the C's `env->mod_nif->priv_data` (`erl_nif.c:609-612`), which ERTS keeps
 * in `lib->priv_data` (`erl_nif.c:5003`). The library records the address of a global holding 11:
 *
 *   priv_value/0    the int `enif_priv_data` points at (11), or -1 when the pointer is NULL
 *   priv_is_set/0   1 when `enif_priv_data` is non-NULL, 0 when it is NULL
 *
 * Both are small integers, so the same `.so` can be loaded by the system Erlang and its printed
 * result compared with this runtime's (`tests/nif_priv_data.rs`).
 */

#include <erl_nif.h>

/* The library's private data. Its ADDRESS is what the `load` callback stores; the value 11 is what
 * `priv_value/0` reads back through the pointer `enif_priv_data` returns. */
static int priv = 11;

/* The library's `load` callback: store the module's private data address, exactly as a real NIF
 * does (ERTS keeps what it writes in `lib->priv_data`). */
static int nif_load(ErlNifEnv *env, void **priv_data, ERL_NIF_TERM load_info)
{
    (void)env;
    (void)load_info;
    *priv_data = &priv;
    return 0;
}

/* priv_value/0 -> *(int *)enif_priv_data(env), or -1 when the pointer is NULL. */
static ERL_NIF_TERM priv_value(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{
    void *data = enif_priv_data(env);

    (void)argc;
    (void)argv;
    return enif_make_int(env, data == NULL ? -1 : *(int *)data);
}

/* priv_is_set/0 -> 1 when enif_priv_data is non-NULL, else 0. */
static ERL_NIF_TERM priv_is_set(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{
    (void)argc;
    (void)argv;
    return enif_make_int(env, enif_priv_data(env) == NULL ? 0 : 1);
}

static ErlNifFunc nif_funcs[] = {
    {"priv_value", 0, priv_value, 0},
    {"priv_is_set", 0, priv_is_set, 0},
};

ERL_NIF_INIT(nif_priv_data, nif_funcs, nif_load, NULL, NULL, NULL)
