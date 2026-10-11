/*
 * tests/fixtures/nif_env.c — a real C NIF (contract A.3) exercising the
 * environment / select / existing-atom `enif_*` slots populated by
 * `src/nif/enif/{envs,select,make}.rs`.
 *
 * Compiled at test time with the system `cc` against the system `erl_nif.h`
 * and loaded through `src/nif/loader.rs`; every `enif_*` call below stays
 * UNDEFINED in the `.so` and is bound at `dlopen(RTLD_NOW)` against the loading
 * process's dynamic symbol table (`build.rs` adds `-Wl,--export-dynamic`).
 *
 * IMPORTANT: this fixture may reference only `enif_*` symbols the crate
 * actually defines (`Some` in the `ErlNifApi` table) — `dlopen(RTLD_NOW)` fails
 * on any unresolved import — and it must NOT reference the C-variadic
 * `enif_make_tuple`/`enif_make_list`/… (those bodies exist only in the C shim,
 * and only when `build.rs` could compile it).
 *
 * What each NIF proves (the Rust side asserts the observable result):
 *
 *   existing_hit/0        enif_make_existing_atom HIT: returns the atom and 1
 *   existing_miss/0       enif_make_existing_atom MISS: returns 0 and leaves
 *                         the out-pointer at the sentinel the caller wrote
 *   existing_len/0        enif_make_existing_atom_len honours the EXPLICIT
 *                         length ("helloX", len 5 => the existing atom `hello`)
 *   select_bad/0          enif_select with a negative event => the C's
 *                         INT_MIN | ERL_NIF_SELECT_INVALID_EVENT
 *   select_noservice/0    enif_select the host cannot service => -1
 *   select_x_bad/0        the same invalid-event path through enif_select_x
 *   select_x_ok/0         enif_select_x passes a serviced code through
 *   priv_data/0           enif_priv_data answers the pointer nif_load stored
 *                         (env->mod_nif->priv_data)
 *   make_copy/0           enif_make_copy with no copy engine => THE_NON_VALUE (0)
 *   alloc_env/0           enif_alloc_env reaches the host of the current NIF
 *                         call: the host's own pointer (or NULL without support)
 */

#include <erl_nif.h>

/* A non-null stand-in for the resource handle (the `obj` operand of enif_select).
 * This layer never dereferences it; the runtime would turn it into a resource. */
static int priv_marker;

/* The library's `load` callback: it stores the module's private data pointer,
 * which is what `enif_priv_data` reports in real ERTS. */
static void* nif_load(ErlNifEnv* env, void** priv_data, ERL_NIF_TERM load_info)
{
    (void)env;
    (void)load_info;
    *priv_data = &priv_marker;
    return NULL;
}

static ERL_NIF_TERM pair(ErlNifEnv* env, ERL_NIF_TERM a, ERL_NIF_TERM b)
{
    ERL_NIF_TERM arr[2];
    arr[0] = a;
    arr[1] = b;
    return enif_make_tuple_from_array(env, arr, 2);
}

/* {Ret, Atom} — `hello` is interned first, so the lookup must be a HIT. */
static ERL_NIF_TERM existing_hit(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM out = enif_make_int(env, -1);
    int ret;

    (void)argc;
    (void)argv;
    (void)enif_make_atom(env, "hello"); /* intern it */
    ret = enif_make_existing_atom(env, "hello", &out, ERL_NIF_LATIN1);
    return pair(env, enif_make_int(env, ret), out);
}

/* {Ret, Out} — the out-pointer is seeded with 12345 and must survive a MISS. */
static ERL_NIF_TERM existing_miss(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM out = enif_make_int(env, 12345); /* the sentinel */
    int ret;

    (void)argc;
    (void)argv;
    ret = enif_make_existing_atom(env, "beam_abi_absent_atom", &out, ERL_NIF_LATIN1);
    return pair(env, enif_make_int(env, ret), out);
}

/* {Ret, Atom} — an explicit length of 5 must look up `hello`, not `helloX`. */
static ERL_NIF_TERM existing_len(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    static const char name[] = "helloX"; /* NOT NUL-terminated within the first 5 bytes */
    ERL_NIF_TERM out = enif_make_int(env, -1);
    int ret;

    (void)argc;
    (void)argv;
    (void)enif_make_atom(env, "hello");
    ret = enif_make_existing_atom_len(env, name, 5, &out, ERL_NIF_LATIN1);
    return pair(env, enif_make_int(env, ret), out);
}

static ERL_NIF_TERM select_bad(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    (void)argc;
    (void)argv;
    return enif_make_int(env, enif_select(env, -1, ERL_NIF_SELECT_READ,
                                          (void*)&priv_marker, NULL,
                                          enif_make_int(env, 1)));
}

/* A valid event the host is asked to service (the host decides the code). */
static ERL_NIF_TERM select_noservice(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    (void)argc;
    (void)argv;
    return enif_make_int(env, enif_select(env, 3, ERL_NIF_SELECT_READ,
                                          (void*)&priv_marker, NULL,
                                          enif_make_int(env, 1)));
}

static ERL_NIF_TERM select_x_bad(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    (void)argc;
    (void)argv;
    return enif_make_int(env, enif_select_x(env, -1, ERL_NIF_SELECT_READ,
                                            (void*)&priv_marker, NULL,
                                            enif_make_int(env, 1), NULL));
}

static ERL_NIF_TERM select_x_ok(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    (void)argc;
    (void)argv;
    return enif_make_int(env, enif_select_x(env, 3, ERL_NIF_SELECT_READ,
                                            (void*)&priv_marker, NULL,
                                            enif_make_int(env, 1), NULL));
}

/* Whether enif_priv_data is non-NULL (the nif_load callback above stores a pointer). */
static ERL_NIF_TERM priv_data(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    (void)argc;
    (void)argv;
    return enif_make_int(env, enif_priv_data(env) != NULL);
}

/* enif_make_copy's raw result as an unsigned long (0 == THE_NON_VALUE). */
static ERL_NIF_TERM make_copy(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM copied;

    (void)argc;
    (void)argv;
    copied = enif_make_copy(env, enif_make_int(env, 7));
    return enif_make_ulong(env, (unsigned long)copied);
}

/* enif_alloc_env(void) driven from inside a real NIF call, as an unsigned long
 * (0 == NULL). The answer is the *host's* env, which this fixture neither
 * dereferences nor frees — it only reports whether the call reached the hook.
 * No free is needed: the test hosts below do not hand out a real allocation,
 * and `enif_alloc_env` itself owns nothing. */
static ERL_NIF_TERM alloc_env(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    (void)argc;
    (void)argv;
    return enif_make_ulong(env, (unsigned long)enif_alloc_env());
}

static ErlNifFunc nif_funcs[] = {
    {"existing_hit", 0, existing_hit, 0},
    {"existing_miss", 0, existing_miss, 0},
    {"existing_len", 0, existing_len, 0},
    {"select_bad", 0, select_bad, 0},
    {"select_noservice", 0, select_noservice, 0},
    {"select_x_bad", 0, select_x_bad, 0},
    {"select_x_ok", 0, select_x_ok, 0},
    {"priv_data", 0, priv_data, 0},
    {"make_copy", 0, make_copy, 0},
    {"alloc_env", 0, alloc_env, 0},
};

ERL_NIF_INIT(test_nif_env, nif_funcs, nif_load, NULL, NULL, NULL)
