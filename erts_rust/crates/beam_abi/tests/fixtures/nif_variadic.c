/*
 * A REAL NIF shared object that calls the seven **C-variadic** `enif_*` functions.
 *
 * `tests/nif_variadic.rs` compiles this with the system `cc` against the system `erl_nif.h` and
 * loads it through `beam_abi::nif::loader::load`; its `enif_*` imports (the variadic seven
 * included) resolve out of the *loading process*'s dynamic symbol table, which `build.rs` publishes
 * with `-Wl,--export-dynamic` and links the C-variadic shim into (`csrc/nif_variadic.c`).
 *
 * Every function is a *fixed-arity* NIF so the varargs handed to `enif_make_tuple` /
 * `enif_make_list` / `enif_*printf` are known at compile time — the point is to prove the variadic
 * *plumbing*, not to re-test argument marshalling.
 *
 * `ERL_NIF_INIT` seeds `ErlNifEntry.name` from the module name, so this library may only be loaded
 * as `nif_variadic`.
 *
 * The `*printf` results are returned as `{ByteList, Ret}` — a list of small integers so the Rust
 * test can read them back with `beam_term::prelude::{car, cdr, signed_value}` without needing an
 * off-heap binary accessor. `Ret` is the `enif_*printf` return value, which (per `erts_vsnprintf`)
 * is the **full rendered length**, not the truncated one.
 */
#include <erl_nif.h>
#include <stdio.h>

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

/* `{ByteList, Ret}` — a list of small ints plus the number of characters rendered. */
static ERL_NIF_TERM result_pair(ErlNifEnv* env, const char* text, int len, int ret) {
    ERL_NIF_TERM arr[2];
    ERL_NIF_TERM list = enif_make_list(env, 0); /* the 0-arity variadic form => [] */
    int i;

    for (i = len - 1; i >= 0; i--)
        list = enif_make_list_cell(env, enif_make_int(env, (unsigned char)text[i]), list);
    arr[0] = list;
    arr[1] = enif_make_int(env, ret);
    return enif_make_tuple_from_array(env, arr, 2);
}

/* --- the two term-building variadic slots ---------------------------------------------------- */

/* tuple3/3 -> a 3-tuple of its arguments, built with the variadic `enif_make_tuple`. */
static ERL_NIF_TERM tuple3(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void)argc;
    return enif_make_tuple(env, 3, argv[0], argv[1], argv[2]);
}

/* list3/3 -> the proper list [A, B, C] via the variadic `enif_make_list` (declared order). */
static ERL_NIF_TERM list3(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void)argc;
    return enif_make_list(env, 3, argv[0], argv[1], argv[2]);
}

/* --- `enif_snprintf` ------------------------------------------------------------------------- */

/* snprintf_mixed/0 -> the ERTS rendering of a mixed-format string.
 *
 * `%p` of NULL is the giveaway that this is ERTS's formatter and not libc's: the oracle defaults
 * the width and precision to `2*sizeof(void*)` and always sets `#`, so NULL renders as `0x` plus 16
 * zeros, where libc prints `(nil)`. */
static ERL_NIF_TERM snprintf_mixed(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    char buf[128];
    int ret;

    (void)argc;
    (void)argv;
    ret = enif_snprintf(buf, sizeof(buf), "a=%d s=%s x=%#x p=%p", -7, "hi", 255, (void*)0);
    if (ret < 0)
        return enif_make_int(env, ret);
    return result_pair(env, buf, (int)strlen(buf), ret);
}

/* snprintf_trunc/0 -> a 4-byte buffer with a 6-byte rendering: ERTS writes `abc\0` but *returns 6*
 * (`erts_vsnprintf` counts what `erts_printf_format` produced, not what `write_sn` copied). */
static ERL_NIF_TERM snprintf_trunc(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    char buf[4];
    int ret;

    (void)argc;
    (void)argv;
    ret = enif_snprintf(buf, sizeof(buf), "%s", "abcdef");
    if (ret < 0)
        return enif_make_int(env, ret);
    return result_pair(env, buf, (int)strlen(buf), ret);
}

/* --- `enif_vsnprintf` (the va_list form) ---------------------------------------------------- */

static int vsn_to(char* buf, size_t size, const char* format, ...) {
    va_list ap;
    int ret;
    va_start(ap, format);
    ret = enif_vsnprintf(buf, size, format, ap);
    va_end(ap);
    return ret;
}

/* vsnprintf_len/0 -> `vsn_to` must behave exactly like `enif_snprintf`. */
static ERL_NIF_TERM vsnprintf_len(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    char buf[64];
    int ret;

    (void)argc;
    (void)argv;
    ret = vsn_to(buf, sizeof(buf), "%s-%d", "file", 42);
    if (ret < 0)
        return enif_make_int(env, ret);
    return result_pair(env, buf, (int)strlen(buf), ret);
}

/* --- `enif_fprintf` / `enif_vfprintf` (the FILE* forms) ------------------------------------- */

/* Read back what was written to `f`. */
static ERL_NIF_TERM drain(ErlNifEnv* env, FILE* f, int ret) {
    char buf[128];
    size_t n;

    if (ret < 0)
        return enif_make_int(env, ret);
    fflush(f);
    rewind(f);
    n = fread(buf, 1, sizeof(buf) - 1, f);
    buf[n] = '\0';
    return result_pair(env, buf, (int)n, ret);
}

/* fprintf_file/0 -> `enif_fprintf` onto a temporary stream. */
static ERL_NIF_TERM fprintf_file(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    FILE* f;
    int ret;
    ERL_NIF_TERM out;

    (void)argc;
    (void)argv;
    f = tmpfile();
    if (!f)
        return enif_make_badarg(env);
    ret = enif_fprintf(f, "%s-%d", "file", 42);
    out = drain(env, f, ret);
    fclose(f);
    return out;
}

static int vf_to(FILE* f, const char* format, ...) {
    va_list ap;
    int ret;
    va_start(ap, format);
    ret = enif_vfprintf(f, format, ap);
    va_end(ap);
    return ret;
}

/* vfprintf_file/0 -> `enif_vfprintf` onto a temporary stream. */
static ERL_NIF_TERM vfprintf_file(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    FILE* f;
    int ret;
    ERL_NIF_TERM out;

    (void)argc;
    (void)argv;
    f = tmpfile();
    if (!f)
        return enif_make_badarg(env);
    ret = vf_to(f, "%c%c", 'a', 'b');
    out = drain(env, f, ret);
    fclose(f);
    return out;
}

/* --- `enif_set_option` ---------------------------------------------------------------------- */

/* set_option/1 -> the option number is passed through and the return code handed back.
 *
 * Every option in the oracle is tied to the *loading* module instance (`env->mod_nif`), which this
 * runtime's env does not have, so `EINVAL` is the oracle's own answer here. */
static ERL_NIF_TERM set_option(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    int opt;
    int ret;

    if (argc != 1 || !enif_get_int(env, argv[0], &opt))
        return enif_make_badarg(env);
    switch (opt) {
    case 1: /* ERL_NIF_OPT_DELAY_HALT — takes no extra argument */
        ret = enif_set_option(env, ERL_NIF_OPT_DELAY_HALT);
        break;
    default:
        ret = enif_set_option(env, (ErlNifOption)opt);
        break;
    }
    return enif_make_int(env, ret);
}

static ErlNifFunc nif_funcs[] = {
    {"tuple3", 3, tuple3, 0},
    {"list3", 3, list3, 0},
    {"snprintf_mixed", 0, snprintf_mixed, 0},
    {"snprintf_trunc", 0, snprintf_trunc, 0},
    {"vsnprintf_len", 0, vsnprintf_len, 0},
    {"fprintf_file", 0, fprintf_file, 0},
    {"vfprintf_file", 0, vfprintf_file, 0},
    {"set_option", 1, set_option, 0},
};

ERL_NIF_INIT(nif_variadic, nif_funcs, load, NULL, NULL, unload)
