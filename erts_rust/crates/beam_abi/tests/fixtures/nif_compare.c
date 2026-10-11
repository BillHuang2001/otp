/*
 * tests/fixtures/nif_compare.c — a real C NIF (contract A.3) exercising the
 * `enif_compare/2` term-ordering slot populated by `src/nif/enif/pred.rs`.
 *
 * Compiled at test time with the system `cc` against the system `erl_nif.h` and
 * loaded through `src/nif/loader.rs`; the `enif_*` calls below stay UNDEFINED in
 * the `.so` and are bound at `dlopen(RTLD_NOW)` against the loading process's
 * dynamic symbol table (`build.rs` adds `-Wl,--export-dynamic`).
 *
 * Only `enif_*` slots this crate actually defines (`Some` in the `ErlNifApi`
 * table) are referenced, and none of them is C-variadic — so `dlopen(RTLD_NOW)`
 * succeeds whether or not the variadic shim could be compiled.
 *
 * Every function returns a small integer or a container of small integers, so
 * the SAME file can be loaded by the system Erlang and its printed result
 * compared with the Rust runtime's (tests/nif_compare.rs drives both).
 *
 * What each NIF proves:
 *
 *   compare/2       `enif_compare(lhs, rhs)` verbatim — the parametric probe, so
 *                   the caller picks the term pair. `enif_compare` is the C's
 *                   `CMP(lhs,rhs)` = `erts_cmp(lhs,rhs,0,0)`: the *arithmetic*
 *                   mode (`1` compares equal to `1.0`) with a top-level atom
 *                   pair ordered by NAME (`erts_cmp_atoms`).
 *   order_pairs/0   a fixed twelve-point slice of the Erlang term order lattice,
 *                   built inside the NIF so both runtimes compare byte-for-byte
 *                   the same terms. Its expected values are `ORDER_PAIRS` in
 *                   tests/nif_compare.rs.
 *   nested_atoms/0  `{alpha}` vs `{beta}` and `[alpha]` vs `[beta]`. The atom
 *                   pair is *nested* inside a compound, which is the documented
 *                   remaining gap: this runtime orders it by raw atom index
 *                   where the oracle orders it by name (see the module docs of
 *                   src/nif/enif/pred.rs).
 *   fresh_atoms/0   `[enif_compare(alpha, beta), enif_compare(beta, alpha)]` where
 *                   BOTH atoms are created inside this very call by
 *                   `enif_make_atom` (and `beta` is created first, so the atom
 *                   indices run opposite to the names). This is the case a NIF
 *                   host whose `intern_atom` only *predicts* an index -- and
 *                   therefore cannot resolve the names through `atom_name` --
 *                   can only get right through the env's per-call atom record.
 *                   Oracle: `{-1,1}`.
 */

#include <erl_nif.h>
#include <string.h>

/* A binary from raw bytes — `enif_make_new_binary` is the non-variadic
 * constructor (the C-variadic `enif_make_tuple`/`enif_make_list` are not used
 * here, so the fixture does not depend on the optional C shim). */
static ERL_NIF_TERM make_bytes(ErlNifEnv* env, const char* bytes, size_t len)
{
    ERL_NIF_TERM term;
    unsigned char* p = enif_make_new_binary(env, len, &term);
    if (p != NULL && bytes != NULL && len > 0)
        memcpy(p, bytes, len);
    return term;
}

/* ---------------------------------------------------------------------------
 * the parametric probe: `enif_compare(lhs, rhs)`
 * ------------------------------------------------------------------------- */

static ERL_NIF_TERM compare_2(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    if (argc != 2)
        return enif_make_badarg(env);
    return enif_make_int(env, enif_compare(argv[0], argv[1]));
}

/* ---------------------------------------------------------------------------
 * the fixed order lattice
 * ------------------------------------------------------------------------- */

#define ORDER_PAIR_COUNT 12

static ERL_NIF_TERM order_pairs(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM one[1];
    ERL_NIF_TERM results[ORDER_PAIR_COUNT];
    ERL_NIF_TERM i1, i2, f10, f15, alpha, beta, a_tuple, a_map, a_list, a_binary;
    int n = 0;

    (void)argc;
    (void)argv;

    i1 = enif_make_int(env, 1);
    i2 = enif_make_int(env, 2);
    f10 = enif_make_double(env, 1.0);
    f15 = enif_make_double(env, 1.5);
    alpha = enif_make_atom(env, "alpha");
    beta = enif_make_atom(env, "beta");

    one[0] = i1;
    a_tuple = enif_make_tuple_from_array(env, one, 1);
    a_map = enif_make_new_map(env);
    a_list = enif_make_list_from_array(env, one, 1);
    a_binary = make_bytes(env, "ab", 2);

    /*                                            expected */
    results[n++] = enif_make_int(env, enif_compare(i1, f10));       /*  0 1 == 1.0 */
    results[n++] = enif_make_int(env, enif_compare(f10, i1));       /*  0 1.0 == 1 */
    results[n++] = enif_make_int(env, enif_compare(f15, i1));       /*  1 1.5 > 1  */
    results[n++] = enif_make_int(env, enif_compare(i1, i2));        /* -1 1 < 2    */
    results[n++] = enif_make_int(env, enif_compare(alpha, beta));   /* -1 by name  */
    results[n++] = enif_make_int(env, enif_compare(beta, alpha));   /*  1 by name  */
    results[n++] = enif_make_int(env, enif_compare(alpha, alpha));  /*  0 same atom*/
    results[n++] = enif_make_int(env, enif_compare(i1, alpha));     /* -1 number<atom */
    results[n++] = enif_make_int(env, enif_compare(alpha, a_tuple));/* -1 atom<tuple  */
    results[n++] = enif_make_int(env, enif_compare(a_tuple, a_map));/* -1 tuple<map   */
    results[n++] = enif_make_int(env, enif_compare(a_list, a_binary)); /* -1 list<bitstring */
    results[n++] = enif_make_int(env, enif_compare(a_binary, i1));  /*  1 bitstring>number */

    return enif_make_tuple_from_array(env, results, ORDER_PAIR_COUNT);
}

/* ---------------------------------------------------------------------------
 * the nested-atom case (the documented remaining gap)
 * ------------------------------------------------------------------------- */

#define NESTED_COUNT 2

static ERL_NIF_TERM nested_atoms(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM a[1];
    ERL_NIF_TERM b[1];
    ERL_NIF_TERM results[NESTED_COUNT];
    ERL_NIF_TERM alpha = enif_make_atom(env, "alpha");
    ERL_NIF_TERM beta = enif_make_atom(env, "beta");

    (void)argc;
    (void)argv;

    a[0] = alpha;
    b[0] = beta;

    /*                                            oracle says -1 (both) */
    results[0] = enif_make_int(env, enif_compare(enif_make_tuple_from_array(env, a, 1),
                                                  enif_make_tuple_from_array(env, b, 1)));
    results[1] = enif_make_int(env, enif_compare(enif_make_list_from_array(env, a, 1),
                                                  enif_make_list_from_array(env, b, 1)));

    return enif_make_tuple_from_array(env, results, NESTED_COUNT);
}

/* ---------------------------------------------------------------------------
 * the fresh-atom case: both operands are interned INSIDE this call
 * ------------------------------------------------------------------------- */

#define FRESH_COUNT 2

static ERL_NIF_TERM fresh_atoms(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM results[FRESH_COUNT];
    ERL_NIF_TERM alpha, beta;

    (void)argc;
    (void)argv;

    /* `beta` is interned FIRST on purpose: the atom indices then run *opposite*
     * to the names, so a comparison that fell back to the index would answer the
     * opposite value. */
    beta = enif_make_atom(env, "beta");
    alpha = enif_make_atom(env, "alpha");

    /*                                            oracle says -1 / 1 */
    results[0] = enif_make_int(env, enif_compare(alpha, beta));
    results[1] = enif_make_int(env, enif_compare(beta, alpha));

    return enif_make_tuple_from_array(env, results, FRESH_COUNT);
}

static ErlNifFunc nif_funcs[] = {
    {"compare", 2, compare_2, 0},
    {"order_pairs", 0, order_pairs, 0},
    {"nested_atoms", 0, nested_atoms, 0},
    {"fresh_atoms", 0, fresh_atoms, 0},
};

ERL_NIF_INIT(test_nif_compare, nif_funcs, NULL, NULL, NULL, NULL)
