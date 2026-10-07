/*
 * A REAL NIF shared object that exercises the **I/O queue / I/O vector** `enif_*`
 * family (workspace contract A.3).
 *
 * It is compiled by the integration test `tests/nif_ioq.rs` with the system `cc`
 * against the system `erl_nif.h` and loaded through `beam_abi::nif::loader::load`.
 * Every `enif_*` it calls is deliberately left undefined by the linker and is bound
 * at `dlopen(RTLD_NOW)` out of the *loading process*'s dynamic symbol table, which
 * the test binary publishes via `-Wl,--export-dynamic` (`build.rs`). A successful
 * load therefore already proves the ten `enif_ioq_*`/`enif_inspect_iovec`/
 * `enif_free_iovec` slots exist as exported dynamic symbols and bind a real C NIF;
 * the NIFs below then pin their *observable behaviour*.
 *
 * `ERL_NIF_INIT` seeds `ErlNifEntry.name` with the stringified module name, so the
 * only module this library may be loaded as is `test_nif_ioq`.
 *
 * Portability note: this fixture uses ONLY `enif_*` functions whose table slots are
 * populated on stable Rust — in particular no C-variadic `enif_*` (`enif_make_tuple`,
 * `enif_make_list`, `enif_fprintf`, …) whose slots are `None`. `enif_make_tuple_from_array`
 * / `enif_make_list_from_array` stand in for the variadic builders.
 */
#include <erl_nif.h>
#include <stddef.h>
#include <string.h>

/* ------------------------------------------------------------------------- */
/* Helpers                                                                   */
/* ------------------------------------------------------------------------- */

/* A heap binary holding `src[0..n)` (`NULL` is allowed when `n == 0`). */
static ERL_NIF_TERM mkbin(ErlNifEnv* env, const void* src, size_t n) {
    ERL_NIF_TERM t;
    unsigned char* p = enif_make_new_binary(env, n, &t);
    if (p == NULL)
        return enif_make_badarg(env);
    if (n > 0 && src != NULL)
        memcpy(p, src, n);
    return t;
}

static ERL_NIF_TERM tuple(ErlNifEnv* env, ERL_NIF_TERM* elems, size_t n) {
    return enif_make_tuple_from_array(env, elems, (unsigned)n);
}

/* The `enif_ioq_peek` view flattened into one binary; `*iovlen` receives the vector
 * count. The queue's view is owned by the queue (a Rust `Vec<SysIOVec>`), so it stays
 * valid across the `enif_make_new_binary` call below. */
static ERL_NIF_TERM peek_flat(ErlNifEnv* env, ErlNifIOQueue* q, int* iovlen) {
    int n = 0, i;
    SysIOVec* v = enif_ioq_peek(q, &n);
    size_t total = 0, off = 0;
    unsigned char* p;
    ERL_NIF_TERM bin;

    if (iovlen != NULL)
        *iovlen = n;
    if (v == NULL)
        total = 0;
    else
        for (i = 0; i < n; i++)
            total += v[i].iov_len;

    p = enif_make_new_binary(env, total, &bin);
    if (p == NULL)
        return enif_make_badarg(env);
    if (v != NULL) {
        for (i = 0; i < n; i++) {
            if (v[i].iov_len > 0)
                memcpy(p + off, v[i].iov_base, v[i].iov_len);
            off += v[i].iov_len;
        }
    }
    return bin;
}

/* The `enif_ioq_peek` view as *one binary per vector* — the C's rule is one vector per
 * non-empty enqueued buffer (`erl_io_queue.c:259-280`), so the list length is the
 * vector count. `*iovlen` receives that count. */
static ERL_NIF_TERM peek_chunks(ErlNifEnv* env, ErlNifIOQueue* q, int* iovlen) {
    ERL_NIF_TERM chunks[ERL_NIF_IOVEC_SIZE];
    int n = 0, i, m;
    SysIOVec* v = enif_ioq_peek(q, &n);

    if (iovlen != NULL)
        *iovlen = n;
    m = n;
    if (m > ERL_NIF_IOVEC_SIZE)
        m = ERL_NIF_IOVEC_SIZE;
    for (i = 0; i < m; i++)
        chunks[i] = mkbin(env, v[i].iov_base, v[i].iov_len);
    return enif_make_list_from_array(env, chunks, (unsigned)m);
}

/* Enqueue `s` (a NUL-terminated literal) through `enif_ioq_enq_binary` with `skip`. */
static int enq_literal(ErlNifEnv* env, ErlNifIOQueue* q, const char* s, size_t skip, int* ret) {
    ErlNifBinary bin;
    ERL_NIF_TERM t = mkbin(env, s, strlen(s));
    if (!enif_inspect_binary(env, t, &bin))
        return 0;
    *ret = enif_ioq_enq_binary(q, &bin, skip);
    return 1;
}

/* ------------------------------------------------------------------------- */
/* NIFs                                                                      */
/* ------------------------------------------------------------------------- */

/* empty/0 -> {Size, IovLen, PeekIsNull, PeekHeadRet, HeadSentinelKept, HeadSizeSentinelKept}
 *
 * A freshly created queue is empty: `enif_ioq_size` is 0, `enif_ioq_peek` answers
 * `NULL` with `*iovlen == 0`, and `enif_ioq_peek_head` returns 0 *without* touching
 * either out-parameter (`erl_nif.c:4402`). */
static ERL_NIF_TERM ioq_empty(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifIOQueue* q;
    SysIOVec* v;
    int iovlen = 0x7fff, head_ret, head_kept, got = 0;
    size_t size0, head_size = 0x7fff;
    ERL_NIF_TERM head = enif_make_int(env, 0x7fff);
    ERL_NIF_TERM out[6];

    (void)argc;
    (void)argv;
    q = enif_ioq_create(ERL_NIF_IOQ_NORMAL);
    if (q == NULL)
        return enif_make_badarg(env);

    size0 = enif_ioq_size(q);
    v = enif_ioq_peek(q, &iovlen);
    head_ret = enif_ioq_peek_head(env, q, &head_size, &head);
    head_kept = enif_get_int(env, head, &got) && got == 0x7fff;
    enif_ioq_destroy(q);

    out[0] = enif_make_int(env, (int)size0);
    out[1] = enif_make_int(env, iovlen);
    out[2] = enif_make_int(env, v == NULL ? 1 : 0);
    out[3] = enif_make_int(env, head_ret);
    out[4] = enif_make_int(env, head_kept);
    out[5] = enif_make_int(env, head_size == 0x7fff ? 1 : 0);
    return tuple(env, out, 6);
}

/* null_handles/0 -> {Size, PeekIsNull, PeekIovLen, PeekHeadRet, DeqRet, DeqSizeKept,
 *                     EnqRet, BinSizeAfterFailedEnq}
 *
 * Every slot's *null-handle* refusal: the C dereferences (or, for `enif_ioq_deq`, faults
 * on) a null queue in several of these; this implementation returns the C's documented
 * failure code instead (see `src/nif/enif/ioq.rs`, "Refusals where the C would fault").
 * `enif_ioq_peek(NULL, &iovlen)` is the C's own `-1` (`erl_io_queue.c:401-403`). */
static ERL_NIF_TERM ioq_null(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    int iovlen = 0x7fff, head_ret, deq_ret, enq_ret;
    size_t size_null, deq_size = 0x7fff, head_size = 0x7fff, bin_size;
    SysIOVec* v;
    ErlNifBinary bin;
    ERL_NIF_TERM head = enif_make_int(env, 0x7fff);
    ERL_NIF_TERM bterm, out[8];

    (void)argc;
    (void)argv;

    size_null = enif_ioq_size(NULL);
    v = enif_ioq_peek(NULL, &iovlen);
    head_ret = enif_ioq_peek_head(env, NULL, &head_size, &head);
    deq_ret = enif_ioq_deq(NULL, 0, &deq_size);

    bterm = mkbin(env, "abc", 3);
    if (!enif_inspect_binary(env, bterm, &bin))
        return enif_make_badarg(env);
    enq_ret = enif_ioq_enq_binary(NULL, &bin, 0);
    /* The queue handle is null, so nothing was enqueued and the early return happens
     * *before* the consume-the-binary step: the caller's struct is untouched. (The C
     * reaches `enif_release_binary` and zeroes it.) */
    bin_size = bin.size;

    enif_ioq_destroy(NULL); /* the C faults; here it is a no-op */

    out[0] = enif_make_int(env, (int)size_null);
    out[1] = enif_make_int(env, v == NULL ? 1 : 0);
    out[2] = enif_make_int(env, iovlen);
    out[3] = enif_make_int(env, head_ret);
    out[4] = enif_make_int(env, deq_ret);
    out[5] = enif_make_int(env, deq_size == 0x7fff ? 1 : 0);
    out[6] = enif_make_int(env, enq_ret);
    out[7] = enif_make_int(env, (int)bin_size);
    return tuple(env, out, 8);
}

/* enq_binary/1 (Skip) -> {Ret, Size, IovLen, Bytes}
 *
 * Enqueue the six bytes "abcdef" with `enif_ioq_enq_binary` after dropping `Skip` leading
 * bytes, then report the queue size, the `enif_ioq_peek` vector count and the queued bytes
 * (the first vector's payload, flattened). `Skip` past the end is the C's `0`
 * (`erl_io_queue.c:227`). */
static ERL_NIF_TERM ioq_enq_binary(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    int skip = 0, ret = 0, iovlen = 0;
    size_t size;
    ErlNifIOQueue* q;
    ERL_NIF_TERM flat;
    ERL_NIF_TERM out[4];

    if (argc != 1 || !enif_get_int(env, argv[0], &skip) || skip < 0)
        return enif_make_badarg(env);
    q = enif_ioq_create(ERL_NIF_IOQ_NORMAL);
    if (q == NULL)
        return enif_make_badarg(env);
    if (!enq_literal(env, q, "abcdef", (size_t)skip, &ret)) {
        enif_ioq_destroy(q);
        return enif_make_badarg(env);
    }
    size = enif_ioq_size(q);
    flat = peek_flat(env, q, &iovlen);
    enif_ioq_destroy(q);

    out[0] = enif_make_int(env, ret);
    out[1] = enif_make_int(env, (int)size);
    out[2] = enif_make_int(env, iovlen);
    out[3] = flat;
    return tuple(env, out, 4);
}

/* enqv/1 (Skip) -> {Ret, Size, IovLen, Bytes}
 *
 * The `enif_ioq_enqv` sibling: the vector is obtained through `enif_inspect_iovec` over
 * the two-binary iolist `["ab","cde"]`, so both slots are exercised. The skip walks the
 * vector entry by entry. */
static ERL_NIF_TERM ioq_enqv(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    int skip = 0, ret, iovlen = 0;
    size_t size;
    ErlNifIOQueue* q;
    ErlNifIOVec* vec = NULL;
    ERL_NIF_TERM parts[2], list, tail, flat;
    ERL_NIF_TERM out[4];

    if (argc != 1 || !enif_get_int(env, argv[0], &skip) || skip < 0)
        return enif_make_badarg(env);
    parts[0] = mkbin(env, "ab", 2);
    parts[1] = mkbin(env, "cde", 3);
    list = enif_make_list_from_array(env, parts, 2);

    q = enif_ioq_create(ERL_NIF_IOQ_NORMAL);
    if (q == NULL)
        return enif_make_badarg(env);
    /* A non-null env: the vector lives in the env's term heap and is marked preallocated. */
    if (!enif_inspect_iovec(env, 16, list, &tail, &vec) || vec == NULL) {
        enif_ioq_destroy(q);
        return enif_make_badarg(env);
    }
    ret = enif_ioq_enqv(q, vec, (size_t)skip);
    size = enif_ioq_size(q);
    flat = peek_flat(env, q, &iovlen);
    enif_free_iovec(vec); /* preallocated: a no-op */
    enif_ioq_destroy(q);

    out[0] = enif_make_int(env, ret);
    out[1] = enif_make_int(env, (int)size);
    out[2] = enif_make_int(env, iovlen);
    out[3] = flat;
    return tuple(env, out, 4);
}

/* mixed/0 -> {Size, IovLen, Chunks, HeadRet, Head, HeadLen}
 *
 * Three enqueues through two different slots ("abc" via `enq_binary`, ["de","f"] via `enqv`,
 * "ghij" via `enq_binary`) then both peeks: `Chunks` is one binary per *enqueued buffer*
 * (four of them), `Head` is `enif_ioq_peek_head`'s binary for the head buffer. */
static ERL_NIF_TERM ioq_mixed(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifIOQueue* q;
    ErlNifIOVec* vec = NULL;
    ERL_NIF_TERM parts[2], list, tail, chunks, head;
    ERL_NIF_TERM out[6];
    int iovlen = 0, head_ret, ret;
    size_t size, head_len = 0;

    (void)argc;
    (void)argv;
    q = enif_ioq_create(ERL_NIF_IOQ_NORMAL);
    if (q == NULL)
        return enif_make_badarg(env);

    /* 1. "abc" through enq_binary. */
    if (!enq_literal(env, q, "abc", 0, &ret) || !ret)
        goto fail;
    /* 2. ["de","f"] through enqv. */
    parts[0] = mkbin(env, "de", 2);
    parts[1] = mkbin(env, "f", 1);
    list = enif_make_list_from_array(env, parts, 2);
    if (!enif_inspect_iovec(env, 16, list, &tail, &vec) || vec == NULL)
        goto fail;
    if (!enif_ioq_enqv(q, vec, 0))
        goto fail;
    enif_free_iovec(vec); /* preallocated: a no-op */
    vec = NULL;
    /* 3. "ghij" through enq_binary. */
    if (!enq_literal(env, q, "ghij", 0, &ret) || !ret)
        goto fail;

    size = enif_ioq_size(q);
    chunks = peek_chunks(env, q, &iovlen);
    head = enif_make_int(env, -1);
    head_ret = enif_ioq_peek_head(env, q, &head_len, &head);
    enif_ioq_destroy(q);

    out[0] = enif_make_int(env, (int)size);
    out[1] = enif_make_int(env, iovlen);
    out[2] = chunks;
    out[3] = enif_make_int(env, head_ret);
    out[4] = head;
    out[5] = enif_make_int(env, (int)head_len);
    return tuple(env, out, 6);

fail:
    if (vec != NULL)
        enif_free_iovec(vec);
    enif_ioq_destroy(q);
    return enif_make_badarg(env);
}

/* deq/1 (Count) -> {Ret, Remaining, Size, IovLen, Bytes, HeadRet, Head}
 *
 * Enqueue "abc" then "defgh" (eight bytes in two buffers), remove `Count` bytes from the
 * head, and report what the C reports: `Remaining` is the *post*-dequeue byte count written
 * through `*size` (`erl_nif.c:4394`), `Size`/`Bytes` the queue's state afterwards. A `Count`
 * larger than the queue is the C's `0`, with `*size` left untouched. */
static ERL_NIF_TERM ioq_deq(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    int count = 0, ret, iovlen = 0, head_ret, ok;
    size_t rem = 4242, size, head_len = 0;
    ErlNifIOQueue* q;
    ERL_NIF_TERM flat, head, out[7];

    if (argc != 1 || !enif_get_int(env, argv[0], &count) || count < 0)
        return enif_make_badarg(env);
    q = enif_ioq_create(ERL_NIF_IOQ_NORMAL);
    if (q == NULL)
        return enif_make_badarg(env);
    ok = enq_literal(env, q, "abc", 0, &ret) && ret
         && enq_literal(env, q, "defgh", 0, &ret) && ret;
    if (!ok) {
        enif_ioq_destroy(q);
        return enif_make_badarg(env);
    }

    ret = enif_ioq_deq(q, (size_t)count, &rem);
    size = enif_ioq_size(q);
    flat = peek_flat(env, q, &iovlen);
    head = enif_make_int(env, -1);
    head_ret = enif_ioq_peek_head(env, q, &head_len, &head);
    enif_ioq_destroy(q);

    out[0] = enif_make_int(env, ret);
    out[1] = enif_make_int(env, (int)rem);
    out[2] = enif_make_int(env, (int)size);
    out[3] = enif_make_int(env, iovlen);
    out[4] = flat;
    out[5] = enif_make_int(env, head_ret);
    out[6] = head;
    return tuple(env, out, 7);
}

/* inspect/1 (MaxLen) -> {Ret, TailLen, IovCnt, VecSize, Bytes}
 *
 * `enif_inspect_iovec` over the three-binary list ["aa","bbb","c"], stopping after `MaxLen`
 * list *elements* (`erl_nif.c:4161`). `TailLen` is the length of the remainder list the call
 * reports, `VecSize` the vector's declared byte size. The vector is allocated in the env's
 * term heap (`MaxLen`-independent) and released with a no-op `enif_free_iovec`. */
static ERL_NIF_TERM ioq_inspect(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    int max_len = 0, ret, i, tail_len = 0;
    unsigned tail_len_u = 0;
    size_t total = 0, off = 0, vec_size;
    int iovcnt;
    ERL_NIF_TERM parts[3], list, tail, bin, out[5];
    ErlNifIOVec* vec = NULL;
    unsigned char* p;

    if (argc != 1 || !enif_get_int(env, argv[0], &max_len) || max_len < 0)
        return enif_make_badarg(env);
    parts[0] = mkbin(env, "aa", 2);
    parts[1] = mkbin(env, "bbb", 3);
    parts[2] = mkbin(env, "c", 1);
    list = enif_make_list_from_array(env, parts, 3);

    ret = enif_inspect_iovec(env, (size_t)max_len, list, &tail, &vec);
    if (!ret || vec == NULL)
        return enif_make_badarg(env);
    if (enif_get_list_length(env, tail, &tail_len_u))
        tail_len = (int)tail_len_u;
    else
        tail_len = -1;
    iovcnt = vec->iovcnt;
    vec_size = vec->size;
    for (i = 0; i < iovcnt; i++)
        total += vec->iov[i].iov_len;
    p = enif_make_new_binary(env, total, &bin);
    if (p == NULL) {
        enif_free_iovec(vec);
        return enif_make_badarg(env);
    }
    for (i = 0; i < iovcnt; i++) {
        if (vec->iov[i].iov_len > 0)
            memcpy(p + off, vec->iov[i].iov_base, vec->iov[i].iov_len);
        off += vec->iov[i].iov_len;
    }

    out[0] = enif_make_int(env, ret);
    out[1] = enif_make_int(env, tail_len);
    out[2] = enif_make_int(env, iovcnt);
    out[3] = enif_make_int(env, (int)vec_size);
    out[4] = bin;
    enif_free_iovec(vec); /* preallocated: a no-op */
    return tuple(env, out, 5);
}

/* inspect_null_env/0 -> {Ret, TailLen, IovCnt, Bytes}
 *
 * The *other* `enif_inspect_iovec` ownership mode: with a `NULL` environment the returned
 * vector owns its storage (`flags == 0`) and the caller must hand it back to
 * `enif_free_iovec` — the one path where that function really frees (`erl_nif.c:4097`). */
static ERL_NIF_TERM ioq_inspect_null_env(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    int ret, i, tail_len = 0;
    unsigned tail_len_u = 0;
    size_t total = 0, off = 0;
    int iovcnt;
    ERL_NIF_TERM parts[2], list, tail, bin, out[4];
    ErlNifIOVec* vec = NULL;
    unsigned char* p;

    (void)argc;
    (void)argv;
    parts[0] = mkbin(env, "xy", 2);
    parts[1] = mkbin(env, "z", 1);
    list = enif_make_list_from_array(env, parts, 2);

    ret = enif_inspect_iovec(NULL, 16, list, &tail, &vec);
    if (!ret || vec == NULL)
        return enif_make_badarg(env);
    if (enif_get_list_length(env, tail, &tail_len_u))
        tail_len = (int)tail_len_u;
    else
        tail_len = -1;
    iovcnt = vec->iovcnt;
    for (i = 0; i < iovcnt; i++)
        total += vec->iov[i].iov_len;
    p = enif_make_new_binary(env, total, &bin);
    if (p == NULL) {
        enif_free_iovec(vec);
        return enif_make_badarg(env);
    }
    for (i = 0; i < iovcnt; i++) {
        if (vec->iov[i].iov_len > 0)
            memcpy(p + off, vec->iov[i].iov_base, vec->iov[i].iov_len);
        off += vec->iov[i].iov_len;
    }

    out[0] = enif_make_int(env, ret);
    out[1] = enif_make_int(env, tail_len);
    out[2] = enif_make_int(env, iovcnt);
    out[3] = bin;
    enif_free_iovec(vec); /* flags == 0: frees the malloc'd block */
    return tuple(env, out, 4);
}

/* inspect_provided/0 -> {Ret, UsedCallerStruct, InlineIovUsed, IovCnt, Bytes}
 *
 * For fewer than `ERL_NIF_IOVEC_SIZE` (16) vectors the C uses the caller's own
 * preallocated structure and its inline `small_iov`/`small_ref_bin` arrays
 * (`erl_nif.c:4296-4299`); the vector is then marked preallocated so a spurious
 * `enif_free_iovec` on it cannot free caller storage. */
static ERL_NIF_TERM ioq_inspect_provided(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    int ret, i, used, inline_iov;
    size_t total = 0, off = 0;
    int iovcnt;
    ERL_NIF_TERM parts[2], list, tail, bin, out[5];
    ErlNifIOVec stack_vec;
    ErlNifIOVec* vec;
    unsigned char* p;

    (void)argc;
    (void)argv;
    parts[0] = mkbin(env, "pq", 2);
    parts[1] = mkbin(env, "rst", 3);
    list = enif_make_list_from_array(env, parts, 2);

    memset(&stack_vec, 0, sizeof(stack_vec));
    vec = &stack_vec;
    /* The in-value of `iovec` is this caller-owned structure. */
    ret = enif_inspect_iovec(env, 16, list, &tail, &vec);
    if (!ret || vec == NULL)
        return enif_make_badarg(env);
    used = (vec == &stack_vec) ? 1 : 0;
    inline_iov = (vec->iov == vec->small_iov) ? 1 : 0;
    iovcnt = vec->iovcnt;
    for (i = 0; i < iovcnt; i++)
        total += vec->iov[i].iov_len;
    p = enif_make_new_binary(env, total, &bin);
    if (p == NULL)
        return enif_make_badarg(env);
    for (i = 0; i < iovcnt; i++) {
        if (vec->iov[i].iov_len > 0)
            memcpy(p + off, vec->iov[i].iov_base, vec->iov[i].iov_len);
        off += vec->iov[i].iov_len;
    }

    out[0] = enif_make_int(env, ret);
    out[1] = enif_make_int(env, used);
    out[2] = enif_make_int(env, inline_iov);
    out[3] = enif_make_int(env, iovcnt);
    out[4] = bin;
    enif_free_iovec(vec); /* preallocated: a no-op on caller storage */
    return tuple(env, out, 5);
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

static ErlNifFunc ioq_funcs[] = {
    {"empty", 0, ioq_empty, 0},
    {"null_handles", 0, ioq_null, 0},
    {"enq_binary", 1, ioq_enq_binary, 0},
    {"enqv", 1, ioq_enqv, 0},
    {"mixed", 0, ioq_mixed, 0},
    {"deq", 1, ioq_deq, 0},
    {"inspect", 1, ioq_inspect, 0},
    {"inspect_null_env", 0, ioq_inspect_null_env, 0},
    {"inspect_provided", 0, ioq_inspect_provided, 0}
};

ERL_NIF_INIT(test_nif_ioq, ioq_funcs, load, NULL, NULL, unload)
