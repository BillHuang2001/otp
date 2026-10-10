// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

//! Contract **A.3** — the NIF C ABI, widened: a second block of live `enif_*` cases.
//!
//! A continuation of [`super::nif_abi`]: each case drives one `enif_*` behaviour end to end
//! through a real C `.so` loaded by `erlang:load_nif/2` and compares both runtimes. Case ids
//! use the `nif_extra/…` prefix. See the node `CONTEXT.md` ("How to add a corpus case").
//!
//! # Shape (identical to [`super::nif_abi`])
//!
//! A **single** C library whose only `ErlNifFunc` entry is `probe/1`; the embedded module
//! `nif_extra` loads it next to the compiled `.beam` (the self-locating trick the other A.3
//! groups use) and calls `nif_extra:classify(N)`, which forwards the integer selector `N` to
//! `nif_extra:probe(N)` — an **external** call, so the Rust runtime intercepts it at external
//! dispatch (a bare local call would keep running the `erlang:nif_error/1` stub). Each C branch
//! returns a **plain integer** (`1` matched, `0` not, `-1` unknown selector), so every golden is
//! a VM-stable integer line. Two selectors deliberately raise from C; the module maps the raise
//! class to an integer.
//!
//! # Why these selectors, and no others
//!
//! Both runtimes `dlopen(RTLD_NOW)` the **same** `.so`, so every `enif_*` the C references must be
//! a symbol the release `beam_rt` really exports (verified against `nm -D`); a symbol it does not
//! export fails the *Rust* load while the oracle loads it — a **mismatch**. Nothing C-variadic
//! (`enif_make_tuple`, `enif_make_list`, `enif_fprintf`, …) is referenced; the non-variadic
//! `*_from_array` / `enif_make_list_cell` forms stand in.
//!
//! The families whose host hook the runtime may leave at its trait default (`NifHost` in
//! `beam_abi`, implemented by `beam_rt`) are covered **only** by the shapes both runtimes answer
//! alike — a *plain differing value* is a mismatch, and a mismatch is forbidden here:
//!
//! * the get/make **boundary** shapes (`enif_get_int64` rejecting the `2^63` written by
//!   `enif_make_uint64`, `enif_get_uint`/`enif_get_uint64` rejecting an out-of-range / negative
//!   value, `INT64_MIN` round-tripping, `enif_get_double` on `1.0e308`);
//! * the list / tuple / binary **edges** (`enif_get_list_length` refusing an improper list,
//!   `enif_get_list_cell` refusing `[]`, the `[]` tail, the empty tuple / empty binary, a
//!   full-range `enif_make_sub_binary`, `enif_inspect_iolist_as_binary` over a flat binary and
//!   over `[]`);
//! * the **map** refusals (`enif_get_map_value` on a missing key and on a non-map,
//!   `enif_get_map_size` on a non-map, `enif_make_map_put`/`_remove` on a non-map,
//!   `enif_make_map_remove` of an absent key, `enif_make_map_from_arrays` with zero pairs) and
//!   the empty-map iterator head/tail;
//! * **`enif_term_type`** across the float / pid / `[]` lattice, **`enif_compare`** across the
//!   type rank (atom < tuple, list < bitstring), **`enif_is_identical`** on structurally equal
//!   containers, and **`enif_is_number`** on a bignum;
//! * the **string** edges (a list-of-codes input, the empty string, `enif_get_string_length([])`,
//!   an improper list and a non-byte element) and the atom read buffer bound;
//! * **`enif_term_size`** of a tuple / `[]` / an atom, and **`enif_hash`**`(ERL_NIF_PHASH2)`'s
//!   documented properties on a binary;
//! * the **exception** slot lifting a non-atom reason into Erlang, plus a clean process env;
//! * the deterministic **`enif_convert_time_unit`** pair and **`enif_monotonic_time`**'s
//!   monotonicity (its absolute value is documented to differ and is never asserted).
//!
//! Not pinned anywhere (a real, documented divergence — recorded as a gap, never a corpus case):
//! the atom-cache slots, `enif_term_to_binary` on a valid term, `enif_binary_to_term` on a valid
//! encoding, `enif_make_ref` on a valid env, `enif_make_existing_atom` on a *committed* atom and
//! `enif_whereis_pid` on a name a process **is** registered under (see [`super::nif_abi`] for the
//! full list and owners). `enif_inspect_iolist_as_binary` on a **bare byte integer** is a further
//! divergence and is likewise unpinned: the oracle refuses it (`erts_iolist_size` accepts only a
//! list or a binary at the top level, so the NIF answers `0`) while the Rust runtime flattens it
//! (`flatten_iolist` accepts a small integer in `0..=255`) and answers `1`. The same byte inside
//! a **list** agrees and is pinned by [`super::nif_abi`].
//!
//! # Skip policy (inherited from [`super::nif_so`])
//!
//! A C-compiled fixture needs a C compiler *and* the host `erl_nif.h`; when either is missing the
//! harness reports `Verdict::Skipped` with a loud reason
//! ([`crate::module_build::MISSING_CC_REASON`] / `MISSING_ERL_NIF_REASON`).

use super::{Case, CaseKind, Contract, Golden, ModuleRun};
use crate::module_build::NifFixture;

/// The embedded module: the library's single `ErlNifFunc` stub (`probe/1`, exported so the oracle
/// accepts the library) plus the `classify/1` driver.
///
/// `classify/1` loads the library next to this module's `.beam` and, on a successful load,
/// forwards the selector to `probe/1` through an **external** call. Only an integer leaves
/// `run/1`: a NIF raise is caught and mapped to a class code, and any non-integer return is
/// mapped to `90`, so the module's observable is always one integer line.
const NIF_EXTRA_MODULE_SOURCE: &str = r#"-module(nif_extra).
-export([classify/1, probe/1]).

%% The library's single ErlNifFunc entry: stubbed and exported so erlang:load_nif/2 accepts the
%% library, and dead code once the load succeeds (the real implementation is in the .so).
probe(_) -> erlang:nif_error(not_loaded).

classify(N) ->
    Dir = filename:dirname(code:which(?MODULE)),
    case erlang:load_nif(Dir ++ "/nif_extra", 0) of
        ok -> run(N);
        {error, {Reason, _Text}} -> reason_code(Reason)
    end.

%% Every branch answers an integer, so the golden is a VM-stable integer line. The raising
%% selectors land in the explicit class clauses; the wildcard keeps a surprising failure a
%% diffable integer rather than an escaping raise.
run(N) ->
    try nif_extra:probe(N) of
        I when is_integer(I) -> I;
        _ -> 90
    catch
        error:boom -> 80;
        error:badarg -> 81;
        error:_Other -> 82;
        exit:_ -> 83;
        throw:_ -> 84
    end.

%% Only the reason *tag* leaves this function: the `Text` of a failure embeds the absolute
%% temp-dir path, which would make the golden drift on every run.
reason_code(ok) -> 0;
reason_code(load_failed) -> 1;
reason_code(bad_lib) -> 2;
reason_code(reload) -> 3;
reason_code(_) -> 9.
"#;

/// The C library: one `probe/1` entry point that dispatches on the integer selector.
///
/// Every `enif_*` here is an exported symbol of the release `beam_rt`; nothing C-variadic is
/// referenced. Each branch returns an integer: `1` when the C ABI contract held, `0` otherwise.
const NIF_EXTRA_C: &str = r#"/* A second real NIF exercising the enif_* C ABI end to end (contract A.3). */
#include <erl_nif.h>
#include <string.h>
#include <stdint.h>

/* The load callback: succeeds and stores no private state. */
static int load(ErlNifEnv* env, void** priv, ERL_NIF_TERM load_info) {
    (void) env;
    (void) load_info;
    *priv = NULL;
    return 0;
}

/* A small on-heap binary built from `s[0..n)`, or the empty binary when `s` is NULL. */
static ERL_NIF_TERM make_bytes(ErlNifEnv* env, const char* s, size_t n) {
    ERL_NIF_TERM term;
    unsigned char* p = enif_make_new_binary(env, n, &term);
    if (p != NULL && s != NULL && n > 0)
        memcpy(p, s, n);
    return term;
}

/* The empty list, built through the non-variadic constructor. */
static ERL_NIF_TERM make_nil(ErlNifEnv* env) {
    ERL_NIF_TERM none[1] = {0};
    return enif_make_list_from_array(env, none, 0);
}

static ERL_NIF_TERM probe(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    int sel = 0;
    if (argc != 1 || !enif_get_int(env, argv[0], &sel))
        return enif_make_badarg(env);

    switch (sel) {
    /* --------------------------------------------------------------- integer boundaries */
    case 1: {                                  /* get_int64 refuses the positive 2^63 */
        ERL_NIF_TERM t = enif_make_uint64(env, (ErlNifUInt64) 1 << 63);
        ErlNifSInt64 s = 0;
        ErlNifUInt64 u = 0;
        int bad = enif_get_int64(env, t, &s);
        int ok = enif_get_uint64(env, t, &u) && u == ((ErlNifUInt64) 1 << 63);
        return enif_make_int(env, (!bad && ok) ? 1 : 0);
    }
    case 2: {                                  /* get_uint64 refuses a negative integer */
        ErlNifUInt64 u = 0;
        return enif_make_int(env, enif_get_uint64(env, enif_make_int(env, -1), &u) ? 0 : 1);
    }
    case 3: {                                  /* get_uint refuses a value above UINT_MAX */
        ERL_NIF_TERM t = enif_make_int64(env, (ErlNifSInt64) 1 << 35);
        unsigned v = 0;
        return enif_make_int(env, enif_get_uint(env, t, &v) ? 0 : 1);
    }
    case 4: {                                  /* int64 INT64_MIN round trips */
        ErlNifSInt64 want = (ErlNifSInt64) (-9223372036854775807LL - 1);
        ERL_NIF_TERM t = enif_make_int64(env, want);
        ErlNifSInt64 v = 0;
        return enif_make_int(env, (enif_get_int64(env, t, &v) && v == want) ? 1 : 0);
    }
    case 5: {                                  /* get_double round trips a large magnitude */
        ERL_NIF_TERM t = enif_make_double(env, 1.0e308);
        double v = 0.0;
        return enif_make_int(env, (enif_get_double(env, t, &v) && v == 1.0e308) ? 1 : 0);
    }
    case 6: {                                  /* get_uint refuses a float */
        unsigned v = 0;
        return enif_make_int(env, enif_get_uint(env, enif_make_double(env, 1.0), &v) ? 0 : 1);
    }
    case 7: {                                  /* get_long refuses a non-number */
        long v = 0;
        return enif_make_int(env, enif_get_long(env, enif_make_atom(env, "not_a_long"), &v) ? 0 : 1);
    }
    case 8: {                                  /* get_ulong refuses a negative integer */
        unsigned long v = 0;
        return enif_make_int(env, enif_get_ulong(env, enif_make_int(env, -7), &v) ? 0 : 1);
    }

    /* -------------------------------------------------------------------------- lists */
    case 9: {                                  /* get_list_length refuses an improper list */
        ERL_NIF_TERM cell = enif_make_list_cell(env, enif_make_int(env, 1),
                                               enif_make_int(env, 2));   /* [1|2] */
        unsigned n = 0;
        return enif_make_int(env, enif_get_list_length(env, cell, &n) ? 0 : 1);
    }
    case 10: {                                 /* get_list_cell refuses the empty list */
        ERL_NIF_TERM h = 0, t = 0;
        return enif_make_int(env, enif_get_list_cell(env, make_nil(env), &h, &t) ? 0 : 1);
    }
    case 11: {                                 /* the tail of a 1-element list is [] */
        ERL_NIF_TERM arr[1];
        ERL_NIF_TERM h = 0, t = 0;
        arr[0] = enif_make_int(env, 5);
        if (!enif_get_list_cell(env, enif_make_list_from_array(env, arr, 1), &h, &t))
            return enif_make_int(env, 0);
        return enif_make_int(env, enif_is_empty_list(env, t) ? 1 : 0);
    }
    case 12: {                                 /* is_list([]) true; is_empty_list([1]) false */
        ERL_NIF_TERM arr[1];
        arr[0] = enif_make_int(env, 1);
        return enif_make_int(env,
            (enif_is_list(env, make_nil(env)) &&
             !enif_is_empty_list(env, enif_make_list_from_array(env, arr, 1))) ? 1 : 0);
    }
    case 13: {                                 /* make_reverse_list of [] is [] */
        ERL_NIF_TERM rev = 0;
        if (!enif_make_reverse_list(env, make_nil(env), &rev))
            return enif_make_int(env, 0);
        return enif_make_int(env, enif_is_empty_list(env, rev) ? 1 : 0);
    }
    case 14: {                                 /* get_list_length of a 3-element list */
        ERL_NIF_TERM arr[3];
        unsigned n = 0;
        arr[0] = enif_make_int(env, 1);
        arr[1] = enif_make_int(env, 2);
        arr[2] = enif_make_int(env, 3);
        return enif_make_int(env,
            (enif_get_list_length(env, enif_make_list_from_array(env, arr, 3), &n) && n == 3)
                ? 1 : 0);
    }

    /* ------------------------------------------------------------------------- tuples */
    case 15: {                                 /* a 1-tuple: arity and element */
        ERL_NIF_TERM arr[1];
        int arity = -1, v = 0;
        const ERL_NIF_TERM* got = NULL;
        arr[0] = enif_make_int(env, 5);
        if (!enif_get_tuple(env, enif_make_tuple_from_array(env, arr, 1), &arity, &got))
            return enif_make_int(env, 0);
        if (!enif_get_int(env, got[0], &v))
            return enif_make_int(env, 0);
        return enif_make_int(env, (arity == 1 && v == 5) ? 1 : 0);
    }
    case 16: {                                 /* is_tuple({}) true and its arity 0 */
        ERL_NIF_TERM none[1] = {0};
        int arity = -1;
        const ERL_NIF_TERM* got = NULL;
        ERL_NIF_TERM tup = enif_make_tuple_from_array(env, none, 0);
        if (!enif_is_tuple(env, tup))
            return enif_make_int(env, 0);
        if (!enif_get_tuple(env, tup, &arity, &got))
            return enif_make_int(env, 0);
        return enif_make_int(env, arity == 0 ? 1 : 0);
    }
    case 17: {                                 /* get_tuple arity of a 2-tuple */
        ERL_NIF_TERM arr[2];
        int arity = -1;
        const ERL_NIF_TERM* got = NULL;
        arr[0] = enif_make_int(env, 1);
        arr[1] = enif_make_int(env, 2);
        if (!enif_get_tuple(env, enif_make_tuple_from_array(env, arr, 2), &arity, &got))
            return enif_make_int(env, 0);
        return enif_make_int(env, arity);
    }

    /* ----------------------------------------------------------------------- binaries */
    case 18: {                                 /* inspect the empty binary */
        ErlNifBinary v;
        ERL_NIF_TERM t = make_bytes(env, NULL, 0);
        if (!enif_inspect_binary(env, t, &v))
            return enif_make_int(env, 0);
        return enif_make_int(env, (enif_is_binary(env, t) && v.size == 0) ? 1 : 0);
    }
    case 19: {                                 /* a full-range sub-binary equals the whole */
        ERL_NIF_TERM whole = make_bytes(env, "abcdef", 6);
        ERL_NIF_TERM s = enif_make_sub_binary(env, whole, 0, 6);
        ErlNifBinary v;
        if (!enif_inspect_binary(env, s, &v))
            return enif_make_int(env, 0);
        return enif_make_int(env,
            (v.size == 6 && memcmp(v.data, "abcdef", 6) == 0 &&
             enif_is_identical(whole, s)) ? 1 : 0);
    }
    case 20: {                                 /* inspect_iolist_as_binary on a plain binary */
        ErlNifBinary v;
        if (!enif_inspect_iolist_as_binary(env, make_bytes(env, "ab", 2), &v))
            return enif_make_int(env, 0);
        return enif_make_int(env, (v.size == 2 && memcmp(v.data, "ab", 2) == 0) ? 1 : 0);
    }
    case 21: {                                 /* get_string UTF-8 encodes a codepoint list */
        ERL_NIF_TERM arr[1];
        char buf[16];
        int r;
        arr[0] = enif_make_int(env, 0xE5);     /* U+00E5 encodes to the two bytes c3 a5 */
        r = enif_get_string(env, enif_make_list_from_array(env, arr, 1), buf, sizeof(buf),
                            ERL_NIF_UTF8);
        return enif_make_int(env,
            (r == 3 && (unsigned char) buf[0] == 0xC3 && (unsigned char) buf[1] == 0xA5)
                ? 1 : 0);
    }
    case 22: {                                 /* inspect_iolist_as_binary on the empty list */
        ErlNifBinary v;
        if (!enif_inspect_iolist_as_binary(env, make_nil(env), &v))
            return enif_make_int(env, 0);
        return enif_make_int(env, v.size == 0 ? 1 : 0);
    }
    case 23: {                                 /* realloc_binary shrinking keeps the prefix */
        ErlNifBinary b;
        int ok;
        if (!enif_alloc_binary(4, &b))
            return enif_make_int(env, 0);
        memcpy(b.data, "abcd", 4);
        if (!enif_realloc_binary(&b, 2)) {
            enif_release_binary(&b);
            return enif_make_int(env, 0);
        }
        ok = (b.size == 2 && memcmp(b.data, "ab", 2) == 0);
        enif_release_binary(&b);
        return enif_make_int(env, ok ? 1 : 0);
    }
    case 24: {                                 /* a 0-byte owned binary round trips */
        ErlNifBinary b;
        ERL_NIF_TERM t;
        ErlNifBinary v;
        if (!enif_alloc_binary(0, &b))
            return enif_make_int(env, 0);
        t = enif_make_binary(env, &b);
        if (!enif_inspect_binary(env, t, &v))
            return enif_make_int(env, 0);
        return enif_make_int(env, (enif_is_binary(env, t) && v.size == 0) ? 1 : 0);
    }
    case 25: {                                 /* inspect_iolist_as_binary refuses a non-iolist */
        ErlNifBinary v;
        return enif_make_int(env,
            enif_inspect_iolist_as_binary(env, enif_make_atom(env, "nope"), &v) ? 0 : 1);
    }
    case 26: {                                 /* a 3-byte binary reports its term size */
        return enif_make_int(env,
            (int) (enif_term_size(make_bytes(env, "abc", 3)) / sizeof(ERL_NIF_TERM)));
    }

    /* --------------------------------------------------------------------------- maps */
    case 27: {                                 /* get_map_value refuses a missing key */
        ERL_NIF_TERM v = 0;
        ERL_NIF_TERM m;
        ERL_NIF_TERM empty = enif_make_new_map(env);
        if (!enif_make_map_put(env, empty, enif_make_atom(env, "a"), enif_make_int(env, 1), &m))
            return enif_make_int(env, 0);
        return enif_make_int(env,
            enif_get_map_value(env, m, enif_make_atom(env, "b"), &v) ? 0 : 1);
    }
    case 28: {                                 /* get_map_size refuses a non-map */
        size_t sz = 99;
        return enif_make_int(env,
            enif_get_map_size(env, enif_make_int(env, 1), &sz) ? 0 : 1);
    }
    case 29: {                                 /* get_map_value refuses a non-map */
        ERL_NIF_TERM v = 0;
        return enif_make_int(env,
            enif_get_map_value(env, enif_make_int(env, 1), enif_make_int(env, 1), &v) ? 0 : 1);
    }
    case 30: {                                 /* make_map_remove of an absent key is a no-op + 1 */
        ERL_NIF_TERM m1, m2;
        size_t sz = 99;
        ERL_NIF_TERM empty = enif_make_new_map(env);
        if (!enif_make_map_put(env, empty, enif_make_atom(env, "a"), enif_make_int(env, 1), &m1))
            return enif_make_int(env, 0);
        if (!enif_make_map_remove(env, m1, enif_make_atom(env, "zz"), &m2))
            return enif_make_int(env, 0);
        if (!enif_get_map_size(env, m2, &sz))
            return enif_make_int(env, 0);
        return enif_make_int(env, (sz == 1 && enif_is_identical(m1, m2)) ? 1 : 0);
    }
    case 31: {                                 /* make_map_remove refuses a non-map */
        ERL_NIF_TERM m2;
        return enif_make_int(env,
            enif_make_map_remove(env, enif_make_int(env, 1), enif_make_int(env, 1), &m2) ? 0 : 1);
    }
    case 32: {                                 /* make_map_put refuses a non-map */
        ERL_NIF_TERM m2;
        return enif_make_int(env,
            enif_make_map_put(env, enif_make_int(env, 1), enif_make_int(env, 1),
                              enif_make_int(env, 1), &m2) ? 0 : 1);
    }
    case 33: {                                 /* make_map_from_arrays with zero pairs */
        ERL_NIF_TERM keys[1] = {0}, vals[1] = {0};
        ERL_NIF_TERM m;
        size_t sz = 99;
        if (!enif_make_map_from_arrays(env, keys, vals, 0, &m))
            return enif_make_int(env, 0);
        if (!enif_get_map_size(env, m, &sz))
            return enif_make_int(env, 0);
        return enif_make_int(env, sz == 0 ? 1 : 0);
    }
    case 34: {                                 /* the empty-map iterator is at head and tail */
        ErlNifMapIterator it;
        ERL_NIF_TERM k = 0, v = 0;
        int head, tail, stepped, pair;
        if (!enif_map_iterator_create(env, enif_make_new_map(env), &it,
                                      ERL_NIF_MAP_ITERATOR_FIRST))
            return enif_make_int(env, 0);
        head = enif_map_iterator_is_head(env, &it);
        tail = enif_map_iterator_is_tail(env, &it);
        stepped = enif_map_iterator_next(env, &it);
        pair = enif_map_iterator_get_pair(env, &it, &k, &v);
        enif_map_iterator_destroy(env, &it);
        return enif_make_int(env, (head && tail && !stepped && !pair) ? 1 : 0);
    }
    case 35: {                                 /* a FIRST iterator of a 3-map walks all pairs */
        ERL_NIF_TERM m1, m2, m3, k = 0, v = 0;
        int count = 0;
        ErlNifMapIterator it;
        ERL_NIF_TERM empty = enif_make_new_map(env);
        if (!enif_make_map_put(env, empty, enif_make_int(env, 1), enif_make_int(env, 10), &m1))
            return enif_make_int(env, 0);
        if (!enif_make_map_put(env, m1, enif_make_int(env, 2), enif_make_int(env, 20), &m2))
            return enif_make_int(env, 0);
        if (!enif_make_map_put(env, m2, enif_make_int(env, 3), enif_make_int(env, 30), &m3))
            return enif_make_int(env, 0);
        if (!enif_map_iterator_create(env, m3, &it, ERL_NIF_MAP_ITERATOR_FIRST))
            return enif_make_int(env, 0);
        while (enif_map_iterator_get_pair(env, &it, &k, &v)) {
            count++;
            enif_map_iterator_next(env, &it);
        }
        enif_map_iterator_destroy(env, &it);
        return enif_make_int(env, count);
    }

    /* -------------------------------------------------- term type / compare / identity */
    case 36: {                                 /* term_type of a float */
        return enif_make_int(env,
            enif_term_type(env, enif_make_double(env, 1.5)) == ERL_NIF_TERM_TYPE_FLOAT ? 1 : 0);
    }
    case 37: {                                 /* term_type of the calling pid */
        ErlNifPid pid;
        if (enif_self(env, &pid) == NULL)
            return enif_make_int(env, 0);
        return enif_make_int(env,
            enif_term_type(env, enif_make_pid(env, &pid)) == ERL_NIF_TERM_TYPE_PID ? 1 : 0);
    }
    case 38: {                                 /* term_type of [] is List */
        return enif_make_int(env,
            enif_term_type(env, make_nil(env)) == ERL_NIF_TERM_TYPE_LIST ? 1 : 0);
    }
    case 39: {                                 /* an atom orders before a tuple */
        ERL_NIF_TERM arr[1];
        arr[0] = enif_make_int(env, 1);
        return enif_make_int(env,
            enif_compare(enif_make_atom(env, "an_atom"),
                         enif_make_tuple_from_array(env, arr, 1)) == -1 ? 1 : 0);
    }
    case 40: {                                 /* a list orders before a bitstring */
        ERL_NIF_TERM arr[1];
        arr[0] = enif_make_int(env, 1);
        return enif_make_int(env,
            enif_compare(enif_make_list_from_array(env, arr, 1), make_bytes(env, "ab", 2)) == -1
                ? 1 : 0);
    }
    case 41: {                                 /* is_identical on structurally equal tuples */
        ERL_NIF_TERM a[2], b[2], c[2];
        int same, diff;
        a[0] = enif_make_int(env, 1); a[1] = enif_make_int(env, 2);
        b[0] = enif_make_int(env, 1); b[1] = enif_make_int(env, 2);
        c[0] = enif_make_int(env, 1); c[1] = enif_make_int(env, 3);
        same = enif_is_identical(enif_make_tuple_from_array(env, a, 2),
                                 enif_make_tuple_from_array(env, b, 2));
        diff = enif_is_identical(enif_make_tuple_from_array(env, a, 2),
                                 enif_make_tuple_from_array(env, c, 2));
        return enif_make_int(env, (same && !diff) ? 1 : 0);
    }
    case 42: {                                 /* is_identical on equal binaries */
        return enif_make_int(env,
            (enif_is_identical(make_bytes(env, "xy", 2), make_bytes(env, "xy", 2)) &&
             !enif_is_identical(make_bytes(env, "xy", 2), make_bytes(env, "xz", 2))) ? 1 : 0);
    }
    case 43: {                                 /* is_identical on equal lists */
        ERL_NIF_TERM a[2], b[2], c[2];
        int same, diff;
        a[0] = enif_make_int(env, 1); a[1] = enif_make_int(env, 2);
        b[0] = enif_make_int(env, 1); b[1] = enif_make_int(env, 2);
        c[0] = enif_make_int(env, 2); c[1] = enif_make_int(env, 1);
        same = enif_is_identical(enif_make_list_from_array(env, a, 2),
                                 enif_make_list_from_array(env, b, 2));
        diff = enif_is_identical(enif_make_list_from_array(env, a, 2),
                                 enif_make_list_from_array(env, c, 2));
        return enif_make_int(env, (same && !diff) ? 1 : 0);
    }
    case 44: {                                 /* is_number of a bignum */
        return enif_make_int(env,
            enif_is_number(env, enif_make_uint64(env, (ErlNifUInt64) 1 << 63)) ? 1 : 0);
    }
    case 45: {                                 /* is_atom([]) is false */
        return enif_make_int(env, enif_is_atom(env, make_nil(env)) ? 0 : 1);
    }

    /* ------------------------------------------------------------------------ strings */
    case 46: {                                 /* get_string over a list of char codes */
        ERL_NIF_TERM arr[2];
        char buf[16];
        int r;
        arr[0] = enif_make_int(env, 'h');
        arr[1] = enif_make_int(env, 'i');
        r = enif_get_string(env, enif_make_list_from_array(env, arr, 2), buf, sizeof(buf),
                            ERL_NIF_LATIN1);
        return enif_make_int(env, (r == 3 && strcmp(buf, "hi") == 0) ? 1 : 0);
    }
    case 47: {                                 /* the empty string round trips */
        char buf[16];
        int r = enif_get_string(env, enif_make_string(env, "", ERL_NIF_LATIN1), buf, sizeof(buf),
                                ERL_NIF_LATIN1);
        return enif_make_int(env, (r == 1 && buf[0] == '\0') ? 1 : 0);
    }
    case 48: {                                 /* get_string_length of [] is 0 */
        unsigned n = 99;
        int r = enif_get_string_length(env, make_nil(env), &n, ERL_NIF_LATIN1);
        return enif_make_int(env, (r == 1 && n == 0) ? 1 : 0);
    }
    case 49: {                                 /* get_string refuses an improper list */
        ERL_NIF_TERM cell = enif_make_list_cell(env, enif_make_int(env, 'a'),
                                                enif_make_int(env, 2));
        char buf[16];
        return enif_make_int(env,
            enif_get_string(env, cell, buf, sizeof(buf), ERL_NIF_LATIN1) ? 0 : 1);
    }
    case 50: {                                 /* get_string refuses a non-byte element */
        ERL_NIF_TERM arr[1];
        char buf[16];
        arr[0] = enif_make_int(env, 256);
        return enif_make_int(env,
            enif_get_string(env, enif_make_list_from_array(env, arr, 1), buf, sizeof(buf),
                            ERL_NIF_LATIN1) ? 0 : 1);
    }
    case 51: {                                 /* get_atom refuses a too-small buffer */
        char buf[2];
        return enif_make_int(env,
            enif_get_atom(env, enif_make_atom(env, "ok"), buf, sizeof(buf), ERL_NIF_LATIN1)
                ? 0 : 1);
    }
    case 52: {                                 /* get_atom_length of an existing name */
        unsigned n = 0;
        int r = enif_get_atom_length(env, enif_make_atom(env, "ok"), &n, ERL_NIF_LATIN1);
        return enif_make_int(env, (r == 1 && n == 2) ? 1 : 0);
    }

    /* ---------------------------------------------------------------------- term size */
    case 53: {                                 /* term_size of a 2-tuple is 3 words */
        ERL_NIF_TERM arr[2];
        arr[0] = enif_make_int(env, 1);
        arr[1] = enif_make_int(env, 2);
        return enif_make_int(env,
            (int) (enif_term_size(enif_make_tuple_from_array(env, arr, 2))
                   / sizeof(ERL_NIF_TERM)));
    }
    case 54: {                                 /* term_size of [] is 0 */
        return enif_make_int(env,
            (int) (enif_term_size(make_nil(env)) / sizeof(ERL_NIF_TERM)));
    }
    case 55: {                                 /* term_size of an atom is 0 */
        return enif_make_int(env,
            (int) (enif_term_size(enif_make_atom(env, "an_atom")) / sizeof(ERL_NIF_TERM)));
    }

    /* --------------------------------------------------------------------------- hash */
    case 56: {                                 /* phash2 of a binary: salt-independent, < 2^27 */
        ERL_NIF_TERM t = make_bytes(env, "hello", 5);
        ErlNifUInt64 h0 = enif_hash(ERL_NIF_PHASH2, t, 0);
        ErlNifUInt64 h1 = enif_hash(ERL_NIF_PHASH2, t, 424242);
        return enif_make_int(env,
            (h0 == h1 && h0 < ((ErlNifUInt64) 1 << 27)) ? 1 : 0);
    }

    /* --------------------------------------------------------------------- exceptions */
    case 57: {                                 /* a tuple raise reason reaches Erlang */
        ERL_NIF_TERM arr[2];
        arr[0] = enif_make_atom(env, "reason");
        arr[1] = enif_make_int(env, 7);
        return enif_raise_exception(env, enif_make_tuple_from_array(env, arr, 2));
    }
    case 58: {                                 /* an integer raise reason reaches Erlang */
        return enif_raise_exception(env, enif_make_int(env, 42));
    }
    case 59: {                                 /* the process env has no pending exception */
        ERL_NIF_TERM reason = 0;
        return enif_make_int(env, enif_has_pending_exception(env, &reason) == 0 ? 1 : 0);
    }

    /* --------------------------------------------------------------------------- time */
    case 60: {                                 /* convert_time_unit 1 s -> ns */
        ErlNifTime ns = enif_convert_time_unit((ErlNifTime) 1, ERL_NIF_SEC, ERL_NIF_NSEC);
        return enif_make_int(env, ns == 1000000000 ? 1 : 0);
    }
    case 61: {                                 /* convert_time_unit 1 s -> ms */
        ErlNifTime ms = enif_convert_time_unit((ErlNifTime) 1, ERL_NIF_SEC, ERL_NIF_MSEC);
        return enif_make_int(env, ms == 1000 ? 1 : 0);
    }
    case 62: {                                 /* monotonic_time never runs backwards */
        ErlNifTime a = enif_monotonic_time(ERL_NIF_MSEC);
        ErlNifTime b = enif_monotonic_time(ERL_NIF_MSEC);
        return enif_make_int(env, b >= a ? 1 : 0);
    }

    default:
        break;
    }
    return enif_make_int(env, -1);
}

static ErlNifFunc nif_extra_funcs[] = {
    {"probe", 1, probe, 0}
};

/* `ERL_NIF_INIT` seeds ErlNifEntry.name with the stringified module name (`nif_extra`), so this
   library may only be loaded as `nif_extra`. */
ERL_NIF_INIT(nif_extra, nif_extra_funcs, load, NULL, NULL, NULL)
"#;

/// The real library every case loads.
const NIF_EXTRA: NifFixture = NifFixture::CompiledLibrary {
    name: "nif_extra",
    c_source: NIF_EXTRA_C,
};

/// Build one module-run case over [`NIF_EXTRA_MODULE_SOURCE`].
///
/// `selectors` is the single integer argument (`classify(N)`); `golden` is the integer line the
/// oracle prints.
fn case(
    id: &'static str,
    description: &'static str,
    selectors: &'static [i64],
    golden: &'static str,
) -> Case {
    Case {
        id,
        description,
        contracts: &[Contract::NifAbi, Contract::BeamAsm],
        kind: CaseKind::Module(ModuleRun {
            source: NIF_EXTRA_MODULE_SOURCE,
            module: "nif_extra",
            function: "classify",
            arity: 1,
            args: selectors,
            nif: Some(NIF_EXTRA),
        }),
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// The widened A.3 group: a second `enif_*` selector set, one per C ABI behaviour.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // --- integer boundaries ------------------------------------------------------------
        case(
            "nif_extra/int_get_int64_rejects_positive_2_63",
            "enif_get_int64 refuses the 2^63 written by enif_make_uint64 (enif_get_uint64 takes it)",
            &[1],
            "1",
        ),
        case(
            "nif_extra/int_get_uint64_rejects_negative",
            "enif_get_uint64 refuses a negative integer",
            &[2],
            "1",
        ),
        case(
            "nif_extra/int_get_uint_rejects_above_uint_max",
            "enif_get_uint refuses a value above UINT_MAX",
            &[3],
            "1",
        ),
        case(
            "nif_extra/int_int64_min_roundtrip",
            "INT64_MIN survives an enif_make_int64/enif_get_int64 round trip",
            &[4],
            "1",
        ),
        case(
            "nif_extra/float_get_double_large_magnitude",
            "enif_make_double/enif_get_double round trip a 1.0e308 magnitude",
            &[5],
            "1",
        ),
        case("nif_extra/int_get_uint_rejects_float", "enif_get_uint refuses a float", &[6], "1"),
        case("nif_extra/int_get_long_rejects_atom", "enif_get_long refuses a non-number", &[7], "1"),
        case(
            "nif_extra/int_get_ulong_rejects_negative",
            "enif_get_ulong refuses a negative integer",
            &[8],
            "1",
        ),
        // --- lists ------------------------------------------------------------------------
        case(
            "nif_extra/list_get_list_length_rejects_improper",
            "enif_get_list_length refuses an improper list",
            &[9],
            "1",
        ),
        case(
            "nif_extra/list_get_list_cell_rejects_nil",
            "enif_get_list_cell refuses the empty list",
            &[10],
            "1",
        ),
        case(
            "nif_extra/list_tail_of_singleton_is_nil",
            "the tail of a 1-element list is the empty list",
            &[11],
            "1",
        ),
        case(
            "nif_extra/list_is_list_nil_true_empty_list_cons_false",
            "enif_is_list accepts [] while enif_is_empty_list refuses a cons cell",
            &[12],
            "1",
        ),
        case(
            "nif_extra/list_make_reverse_list_empty",
            "enif_make_reverse_list of [] yields []",
            &[13],
            "1",
        ),
        case(
            "nif_extra/list_get_list_length_three",
            "enif_get_list_length reports 3 for a 3-element list",
            &[14],
            "1",
        ),
        // --- tuples -----------------------------------------------------------------------
        case(
            "nif_extra/tuple_one_element",
            "a 1-tuple: enif_get_tuple reports arity 1 and the element",
            &[15],
            "1",
        ),
        case(
            "nif_extra/tuple_empty_is_tuple_arity_zero",
            "enif_is_tuple accepts {} and enif_get_tuple reports arity 0",
            &[16],
            "1",
        ),
        case("nif_extra/tuple_get_tuple_arity_two", "enif_get_tuple reports the arity (2)", &[17], "2"),
        // --- binaries ---------------------------------------------------------------------
        case(
            "nif_extra/binary_inspect_empty",
            "enif_inspect_binary accepts the empty binary with size 0",
            &[18],
            "1",
        ),
        case(
            "nif_extra/binary_make_sub_binary_full_range",
            "a full-range enif_make_sub_binary equals the whole binary",
            &[19],
            "1",
        ),
        case(
            "nif_extra/binary_inspect_iolist_as_binary_flat",
            "enif_inspect_iolist_as_binary on a plain binary",
            &[20],
            "1",
        ),
        case(
            "nif_extra/string_get_utf8_from_codepoint_list",
            "enif_get_string under ERL_NIF_UTF8 encodes a codepoint list (U+00E5 -> c3 a5)",
            &[21],
            "1",
        ),
        case(
            "nif_extra/binary_inspect_iolist_empty_list",
            "enif_inspect_iolist_as_binary of [] yields a 0-byte view",
            &[22],
            "1",
        ),
        case(
            "nif_extra/binary_realloc_binary_shrink",
            "enif_realloc_binary shrinks and keeps the prefix",
            &[23],
            "1",
        ),
        case(
            "nif_extra/binary_zero_byte_owned_roundtrip",
            "a 0-byte enif_alloc_binary/enif_make_binary round trip",
            &[24],
            "1",
        ),
        case(
            "nif_extra/binary_inspect_iolist_rejects_atom",
            "enif_inspect_iolist_as_binary refuses a non-iolist",
            &[25],
            "1",
        ),
        case(
            "nif_extra/term_size_three_byte_binary",
            "enif_term_size of a 3-byte binary (in words)",
            &[26],
            "3",
        ),
        // --- maps -------------------------------------------------------------------------
        case("nif_extra/map_get_map_value_missing_key", "enif_get_map_value refuses a missing key", &[27], "1"),
        case("nif_extra/map_get_map_size_rejects_non_map", "enif_get_map_size refuses a non-map", &[28], "1"),
        case("nif_extra/map_get_map_value_rejects_non_map", "enif_get_map_value refuses a non-map", &[29], "1"),
        case(
            "nif_extra/map_make_map_remove_absent_key",
            "enif_make_map_remove of an absent key answers 1 and leaves the map unchanged",
            &[30],
            "1",
        ),
        case("nif_extra/map_make_map_remove_rejects_non_map", "enif_make_map_remove refuses a non-map", &[31], "1"),
        case("nif_extra/map_make_map_put_rejects_non_map", "enif_make_map_put refuses a non-map", &[32], "1"),
        case(
            "nif_extra/map_make_map_from_arrays_zero",
            "enif_make_map_from_arrays with zero pairs yields an empty map",
            &[33],
            "1",
        ),
        case(
            "nif_extra/map_iterator_empty_map_head_tail",
            "an empty-map iterator is at head and tail and yields no pair",
            &[34],
            "1",
        ),
        case(
            "nif_extra/map_iterator_first_walks_three",
            "a FIRST iterator walks all three pairs of a 3-entry map",
            &[35],
            "3",
        ),
        // --- term type / compare / identity -----------------------------------------------
        case("nif_extra/term_type_float", "enif_term_type reports Float for a double", &[36], "1"),
        case("nif_extra/term_type_pid", "enif_term_type reports Pid for the calling process", &[37], "1"),
        case("nif_extra/term_type_nil_is_list", "enif_term_type reports List for []", &[38], "1"),
        case("nif_extra/compare_atom_before_tuple", "enif_compare orders an atom before a tuple", &[39], "1"),
        case("nif_extra/compare_list_before_bitstring", "enif_compare orders a list before a bitstring", &[40], "1"),
        case(
            "nif_extra/identity_equal_tuples",
            "enif_is_identical holds for two structurally equal tuples and differs otherwise",
            &[41],
            "1",
        ),
        case(
            "nif_extra/identity_equal_binaries",
            "enif_is_identical holds for two binaries with the same bytes",
            &[42],
            "1",
        ),
        case("nif_extra/identity_equal_lists", "enif_is_identical holds for two structurally equal lists", &[43], "1"),
        case("nif_extra/pred_is_number_bignum", "enif_is_number accepts a bignum term", &[44], "1"),
        case("nif_extra/pred_is_atom_nil_false", "enif_is_atom is false for []", &[45], "1"),
        // --- strings / atoms --------------------------------------------------------------
        case("nif_extra/string_get_from_char_list", "enif_get_string reads a list of char codes", &[46], "1"),
        case("nif_extra/string_empty_roundtrip", "enif_make_string(\"\") round trips through enif_get_string", &[47], "1"),
        case("nif_extra/string_get_length_nil", "enif_get_string_length of [] reports 0", &[48], "1"),
        case("nif_extra/string_get_rejects_improper", "enif_get_string refuses an improper list", &[49], "1"),
        case("nif_extra/string_get_rejects_non_byte", "enif_get_string refuses a non-byte element", &[50], "1"),
        case("nif_extra/atom_get_atom_small_buffer", "enif_get_atom refuses a too-small buffer", &[51], "1"),
        case("nif_extra/atom_get_atom_length", "enif_get_atom_length reports the byte count (2)", &[52], "1"),
        // --- term size / hash -------------------------------------------------------------
        case("nif_extra/term_size_two_tuple", "enif_term_size of a 2-tuple is three words", &[53], "3"),
        case("nif_extra/term_size_nil", "enif_term_size of [] is zero words", &[54], "0"),
        case("nif_extra/term_size_atom", "enif_term_size of an atom is zero words", &[55], "0"),
        case(
            "nif_extra/hash_phash2_binary_salt_independent",
            "enif_hash(ERL_NIF_PHASH2, <<binary>>) ignores the salt and stays below 2^27",
            &[56],
            "1",
        ),
        // --- exceptions -------------------------------------------------------------------
        case(
            "nif_extra/exception_tuple_reason",
            "enif_raise_exception lifts a tuple reason into Erlang as error:Reason",
            &[57],
            "82",
        ),
        case(
            "nif_extra/exception_integer_reason",
            "enif_raise_exception lifts an integer reason into Erlang as error:Reason",
            &[58],
            "82",
        ),
        case(
            "nif_extra/exception_no_pending_on_process_env",
            "enif_has_pending_exception is 0 on the process env with no raise",
            &[59],
            "1",
        ),
        // --- time -------------------------------------------------------------------------
        case("nif_extra/time_convert_sec_to_nsec", "enif_convert_time_unit maps 1 s to 1e9 ns", &[60], "1"),
        case("nif_extra/time_convert_sec_to_msec", "enif_convert_time_unit maps 1 s to 1000 ms", &[61], "1"),
        case("nif_extra/time_monotonic_non_decreasing", "enif_monotonic_time never runs backwards", &[62], "1"),
    ]
}

#[cfg(test)]
mod tests {
    //! Invariants of the widened A.3 group: every case is a module-run case claiming
    //! [`Contract::NifAbi`], it runs the shared module through `classify/1`, and its golden is
    //! the integer the oracle prints for that selector.

    use super::{group, CaseKind, Contract, NifFixture};
    use std::collections::HashSet;

    #[test]
    fn every_case_is_a_nif_module_run_classifying_one_selector() {
        let mut selectors = HashSet::new();
        let mut previous = 0i64;
        let cases = group();
        assert!(
            cases.len() >= 40,
            "the widened A.3 group must carry a broad selector set (got {})",
            cases.len()
        );
        for case in cases {
            assert!(
                case.id.starts_with("nif_extra/"),
                "case {} is outside the nif_extra group",
                case.id
            );
            assert!(!case.description.is_empty(), "case {} has no description", case.id);
            assert_eq!(
                case.contracts,
                &[Contract::NifAbi, Contract::BeamAsm],
                "case {}: an end-to-end NIF case evidences A.3 and the compiled .beam (A.2)",
                case.id
            );
            let CaseKind::Module(run) = case.kind else {
                panic!("case {}: an A.3 case must be a module-run case", case.id);
            };
            assert_eq!(run.module, "nif_extra", "case {}", case.id);
            assert_eq!(run.function, "classify", "case {}", case.id);
            assert_eq!(run.arity, 1, "case {}", case.id);
            assert_eq!(run.args.len(), 1, "case {}", case.id);
            assert!(
                selectors.insert(run.args[0]),
                "case {}: selector {} is used twice",
                case.id,
                run.args[0]
            );
            assert!(
                matches!(run.nif, Some(NifFixture::CompiledLibrary { name: "nif_extra", .. })),
                "case {}: the library must be the shared C-compiled nif_extra fixture",
                case.id
            );
            assert!(
                case.golden
                    .stdout
                    .expect("every case pins a golden")
                    .trim_start_matches('-')
                    .chars()
                    .all(|c| c.is_ascii_digit()),
                "case {}: the golden must be a plain integer line (a path-bearing \
                 {{error, {{Reason, Text}}}} would drift), got {:?}",
                case.id,
                case.golden.stdout
            );
            assert_eq!(case.golden.exit_code, 0, "case {}", case.id);
            // Selectors are listed in ascending order, which keeps the table readable.
            assert!(
                run.args[0] > previous,
                "case {}: selectors must be listed in ascending order ({} after {})",
                case.id,
                run.args[0],
                previous
            );
            previous = run.args[0];
        }
    }

    #[test]
    fn the_c_library_covers_the_new_accessor_and_constructor_families() {
        // Each family this group exists to add must really be referenced, or the case does not
        // exercise it.
        for symbol in [
            "enif_get_int64",
            "enif_get_uint64",
            "enif_make_uint64",
            "enif_make_int64",
            "enif_get_long",
            "enif_get_ulong",
            "enif_get_list_length",
            "enif_get_list_cell",
            "enif_is_empty_list",
            "enif_is_list",
            "enif_make_reverse_list",
            "enif_make_list_from_array",
            "enif_make_list_cell",
            "enif_get_tuple",
            "enif_is_tuple",
            "enif_make_tuple_from_array",
            "enif_inspect_binary",
            "enif_inspect_iolist_as_binary",
            "enif_make_sub_binary",
            "enif_realloc_binary",
            "enif_alloc_binary",
            "enif_make_binary",
            "enif_get_map_size",
            "enif_get_map_value",
            "enif_make_map_put",
            "enif_make_map_remove",
            "enif_make_map_from_arrays",
            "enif_map_iterator_is_head",
            "enif_map_iterator_is_tail",
            "enif_term_type",
            "enif_compare",
            "enif_is_identical",
            "enif_is_number",
            "enif_is_atom",
            "enif_get_string",
            "enif_get_string_length",
            "enif_make_string",
            "enif_get_atom",
            "enif_get_atom_length",
            "enif_term_size",
            "enif_hash",
            "enif_raise_exception",
            "enif_has_pending_exception",
            "enif_convert_time_unit",
            "enif_monotonic_time",
        ] {
            assert!(
                super::NIF_EXTRA_C.contains(symbol),
                "the library must reference {symbol}"
            );
        }
    }

    #[test]
    fn the_c_library_avoids_the_divergent_and_variadic_slots() {
        // A call site for one of these would be a plain differing value (a mismatch), so the
        // library must never carry one:
        //   * enif_make_ref          -> the Rust runtime raises badarg (no host `make_ref`);
        //   * enif_term_to_binary    -> the Rust runtime answers 0 (no host `term_to_binary`);
        //   * the atom-cache slots   -> implementation-defined indices (always 0 here);
        //   * a *valid* enif_binary_to_term / a committed-atom lookup is exercised nowhere.
        for absent in [
            "enif_make_ref(",
            "enif_term_to_binary(",
            "enif_binary_to_term(",
            "enif_max_atom_cache_index(",
            "enif_get_atom_cache_index(",
            "ERL_NIF_INTERNAL_HASH",
        ] {
            assert!(
                !super::NIF_EXTRA_C.contains(absent),
                "the library must not reference {absent} (a guaranteed mismatch)"
            );
        }
        // A C-variadic slot must never be referenced: both runtimes dlopen the same `.so`, so an
        // unresolved slot would fail the Rust load where the oracle succeeds.
        for banned in ["enif_make_tuple(", "enif_make_list(", "enif_fprintf", "enif_set_option"] {
            assert!(
                !super::NIF_EXTRA_C.contains(banned),
                "the library must not reference the variadic {banned}"
            );
        }
    }

    #[test]
    fn the_module_exports_and_calls_the_probe() {
        for needle in [
            "-export([classify/1, probe/1])",
            "nif_extra:probe(N)",
            "erlang:load_nif(Dir ++ \"/nif_extra\", 0)",
        ] {
            assert!(
                super::NIF_EXTRA_MODULE_SOURCE.contains(needle),
                "the module must contain {needle:?}"
            );
        }
    }
}
