/*
 * A REAL NIF shared object that exercises the **map iterator** `enif_*` family
 * (workspace contract A.3).
 *
 * It is compiled by the integration test `tests/nif_mapiter.rs` with the system `cc`
 * against the system `erl_nif.h` and loaded through `beam_abi::nif::loader::load`. Every
 * `enif_map_iterator_*` it calls is deliberately left undefined by the linker and is bound
 * at `dlopen(RTLD_NOW)` out of the *loading process*'s dynamic symbol table, which the test
 * binary publishes via `-Wl,--export-dynamic` (`build.rs`). So a successful load already
 * proves all seven slots exist as exported dynamic symbols and bind a real C NIF; the NIFs
 * below then pin the *observable behaviour*, most importantly the order the iterator walks.
 *
 * `ERL_NIF_INIT` seeds `ErlNifEntry.name` with the stringified module name, so the only
 * module this library may be loaded as is `test_nif_mapiter`.
 *
 * Portability note: this fixture uses ONLY `enif_*` functions whose table slots are
 * populated on stable Rust — in particular no C-variadic `enif_*` (`enif_make_tuple`,
 * `enif_make_list`, …) whose slots are `None`. `enif_make_tuple_from_array` /
 * `enif_make_list_from_array` stand in for the variadic builders.
 */
#include <erl_nif.h>
#include <string.h>

/* The walk collects at most this many pairs (a test-scope bound, not an API limit). */
#define MAX_KEYS 64

/* ------------------------------------------------------------------------- */
/* Helpers                                                                   */
/* ------------------------------------------------------------------------- */

static ERL_NIF_TERM tuple(ErlNifEnv* env, ERL_NIF_TERM* elems, size_t n) {
    return enif_make_tuple_from_array(env, elems, (unsigned)n);
}

/* Build the map `K => K * 100` for every small integer `K` of the proper list `keylist`, in
 * one `enif_make_map_from_arrays` call (the same single-shot construction
 * `maps:from_list/1` performs, so the two produce the identical map). `*n_out` receives the
 * number of list elements; the value is `0` on a malformed list or a failed allocation. */
static int build_map(ErlNifEnv* env, ERL_NIF_TERM keylist, ERL_NIF_TERM* map_out, int* n_out) {
    ERL_NIF_TERM keys[MAX_KEYS], values[MAX_KEYS];
    ERL_NIF_TERM rest = keylist;
    int n = 0;

    /* `enif_is_list` is true for `[]` too (the C's `is_list || is_nil`), so the nil case has to
     * leave the loop here rather than through a failing `enif_get_list_cell`. */
    while (enif_is_list(env, rest) && !enif_is_empty_list(env, rest)) {
        ERL_NIF_TERM head, tail;
        int key;
        if (n >= MAX_KEYS)
            return 0;
        if (!enif_get_list_cell(env, rest, &head, &tail))
            return 0;
        if (!enif_get_int(env, head, &key))
            return 0;
        keys[n] = enif_make_int(env, key);
        values[n] = enif_make_int(env, key * 100);
        n++;
        rest = tail;
    }
    if (!enif_is_empty_list(env, rest))
        return 0; /* an improper list */
    *n_out = n;
    return enif_make_map_from_arrays(env, keys, values, (size_t)n, map_out);
}

/* Walk the map built from `argv[0]` starting at `entry` (`FIRST` walks forwards with
 * `enif_map_iterator_next`, `LAST` walks backwards from the last pair with
 * `enif_map_iterator_prev`) and report everything the family exposes:
 *
 *   {Created, SizeField, Head0, Tail0, HeadEnd, TailEnd, Keys, Values}
 *
 * `Head0`/`Tail0` are `enif_map_iterator_is_head`/`is_tail` immediately after `create`;
 * `HeadEnd`/`TailEnd` the same two *after* the walk ran off the end. */
static ERL_NIF_TERM walk(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[],
                         ErlNifMapIteratorEntry entry) {
    ERL_NIF_TERM map, keys[MAX_KEYS], values[MAX_KEYS];
    ERL_NIF_TERM out[8];
    ErlNifMapIterator it;
    int n = 0, cnt = 0, created, head0, tail0, head_end, tail_end;
    unsigned size_field = 0;

    if (argc != 1)
        return enif_make_badarg(env);
    if (!build_map(env, argv[0], &map, &n))
        return enif_make_badarg(env);

    /* A zeroed iterator, as a NIF's stack local is before `create`. */
    memset(&it, 0, sizeof(it));
    created = enif_map_iterator_create(env, map, &it, entry);
    if (!created)
        return enif_make_badarg(env);
    size_field = (unsigned)it.size;
    head0 = enif_map_iterator_is_head(env, &it);
    tail0 = enif_map_iterator_is_tail(env, &it);

    while (cnt < MAX_KEYS) {
        ERL_NIF_TERM key, value;
        if (!enif_map_iterator_get_pair(env, &it, &key, &value))
            break;
        keys[cnt] = key;
        values[cnt] = value;
        cnt++;
        if (entry == ERL_NIF_MAP_ITERATOR_FIRST) {
            if (!enif_map_iterator_next(env, &it))
                break;
        } else {
            if (!enif_map_iterator_prev(env, &it))
                break;
        }
    }
    head_end = enif_map_iterator_is_head(env, &it);
    tail_end = enif_map_iterator_is_tail(env, &it);
    enif_map_iterator_destroy(env, &it);

    out[0] = enif_make_int(env, created);
    out[1] = enif_make_int(env, (int)size_field);
    out[2] = enif_make_int(env, head0);
    out[3] = enif_make_int(env, tail0);
    out[4] = enif_make_int(env, head_end);
    out[5] = enif_make_int(env, tail_end);
    out[6] = enif_make_list_from_array(env, keys, (unsigned)cnt);
    out[7] = enif_make_list_from_array(env, values, (unsigned)cnt);
    return tuple(env, out, 8);
}

/* ------------------------------------------------------------------------- */
/* NIFs                                                                      */
/* ------------------------------------------------------------------------- */

/* forward/1 (KeyList) -> the `walk` tuple starting at the FIRST pair. */
static ERL_NIF_TERM mapiter_forward(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    return walk(env, argc, argv, ERL_NIF_MAP_ITERATOR_FIRST);
}

/* backward/1 (KeyList) -> the `walk` tuple starting at the LAST pair. */
static ERL_NIF_TERM mapiter_backward(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    return walk(env, argc, argv, ERL_NIF_MAP_ITERATOR_LAST);
}

/* empty/0 -> {Created1, Head1, Tail1, PairRet1, PairKeyKept1, Created2, Head2, Tail2,
 *             PairRet2, PairKeyKept2}
 *
 * The C's "empty maps are ok but will leave the iterator in bad shape" case
 * (`erl_nif.c:3788-3790`): both entries create successfully, both report head *and* tail,
 * and neither yields a pair (with the out-parameters left untouched). */
static ERL_NIF_TERM mapiter_empty(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ERL_NIF_TERM map, out[10];
    ErlNifMapIterator it;
    ERL_NIF_TERM pair_key, pair_value;
    int got = 0;
    int created1, head1, tail1, pair1, kept1;
    int created2, head2, tail2, pair2, kept2;

    (void)argc;
    (void)argv;
    if (!build_map(env, enif_make_list_from_array(env, NULL, 0), &map, &got))
        return enif_make_badarg(env);

    memset(&it, 0, sizeof(it));
    created1 = enif_map_iterator_create(env, map, &it, ERL_NIF_MAP_ITERATOR_FIRST);
    head1 = enif_map_iterator_is_head(env, &it);
    tail1 = enif_map_iterator_is_tail(env, &it);
    pair_key = enif_make_int(env, 0x7fff);
    pair_value = enif_make_int(env, 0x7fff);
    pair1 = enif_map_iterator_get_pair(env, &it, &pair_key, &pair_value);
    kept1 = enif_get_int(env, pair_key, &got) && got == 0x7fff;
    enif_map_iterator_destroy(env, &it);

    memset(&it, 0, sizeof(it));
    created2 = enif_map_iterator_create(env, map, &it, ERL_NIF_MAP_ITERATOR_LAST);
    head2 = enif_map_iterator_is_head(env, &it);
    tail2 = enif_map_iterator_is_tail(env, &it);
    pair_key = enif_make_int(env, 0x7fff);
    pair_value = enif_make_int(env, 0x7fff);
    pair2 = enif_map_iterator_get_pair(env, &it, &pair_key, &pair_value);
    kept2 = enif_get_int(env, pair_key, &got) && got == 0x7fff;
    enif_map_iterator_destroy(env, &it);

    out[0] = enif_make_int(env, created1);
    out[1] = enif_make_int(env, head1);
    out[2] = enif_make_int(env, tail1);
    out[3] = enif_make_int(env, pair1);
    out[4] = enif_make_int(env, kept1);
    out[5] = enif_make_int(env, created2);
    out[6] = enif_make_int(env, head2);
    out[7] = enif_make_int(env, tail2);
    out[8] = enif_make_int(env, pair2);
    out[9] = enif_make_int(env, kept2);
    return tuple(env, out, 10);
}

/* nonmap/0 -> {CreateRet, PairRet}
 *
 * `enif_map_iterator_create` on a term that is not a map is the C's `0`
 * (`erl_nif.c:3824`). The iterator stays zeroed, so the `destroy` calls below are the
 * documented no-op (in the C a `destroy` of a never-created iterator is a fault/UB). */
static ERL_NIF_TERM mapiter_nonmap(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ERL_NIF_TERM out[2];
    ErlNifMapIterator it;
    ERL_NIF_TERM key, value;
    int created, pair;

    (void)argc;
    (void)argv;
    memset(&it, 0, sizeof(it));
    created = enif_map_iterator_create(env, enif_make_int(env, 7), &it,
                                       ERL_NIF_MAP_ITERATOR_FIRST);
    key = enif_make_int(env, 0);
    value = enif_make_int(env, 0);
    pair = enif_map_iterator_get_pair(env, &it, &key, &value);
    /* Idempotent, and safe on a never-created iterator (`src/nif/enif/mapiter.rs`). */
    enif_map_iterator_destroy(env, &it);
    enif_map_iterator_destroy(env, &it);

    out[0] = enif_make_int(env, created);
    out[1] = enif_make_int(env, pair);
    return tuple(env, out, 2);
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

static ErlNifFunc mapiter_funcs[] = {
    {"forward", 1, mapiter_forward, 0},
    {"backward", 1, mapiter_backward, 0},
    {"empty", 0, mapiter_empty, 0},
    {"nonmap", 0, mapiter_nonmap, 0}
};

ERL_NIF_INIT(test_nif_mapiter, mapiter_funcs, load, NULL, NULL, unload)
