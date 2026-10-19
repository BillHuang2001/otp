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
/*
 * tests/fixtures/nif_bin2term.c -- a real C NIF (contract A.3) exercising the
 * `enif_binary_to_term/5` slot populated by `src/nif/enif/mem.rs`.
 *
 * Only `enif_*` slots this crate defines (`Some` in the `ErlNifApi` table) are
 * referenced, and none of them is C-variadic: the tuple is built with
 * `enif_make_tuple_from_array`, NOT with `enif_make_tuple2`, because on GCC that
 * form is an inline wrapper around the C-variadic `enif_make_tuple` (the
 * optional C shim).
 *
 * The C's `size_t enif_binary_to_term(env, data, data_sz, &term, opts)` ends in
 * `return bp - data` (`erts/emulator/beam/erl_nif.c:1507-1545`): the number of
 * leading source bytes the FIRST term occupied. Trailing bytes are tolerated and
 * not counted; a truncated payload is `0`; an `opts` outside
 * {0, ERL_NIF_BIN2TERM_SAFE} is `0` before any decoding.
 */
#include <erl_nif.h>

static ERL_NIF_TERM decode_used(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{
    ErlNifBinary bin;
    ERL_NIF_TERM term;
    ERL_NIF_TERM result[2];
    size_t used;

    (void)argc;
    if (!enif_inspect_binary(env, argv[0], &bin))
        return enif_make_badarg(env);

    term = enif_make_atom(env, "undefined");
    used = enif_binary_to_term(env, bin.data, bin.size, &term, 0);
    /* On the C's `0` the term operand is NOT a term (ERTS stores its internal
     * non-value there), so replace it rather than read it. */
    if (used == 0)
        term = enif_make_atom(env, "undefined");

    result[0] = enif_make_ulong(env, (unsigned long)used);
    result[1] = term;
    return enif_make_tuple_from_array(env, result, 2);
}

static ERL_NIF_TERM used_truncated(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{
    ErlNifBinary bin;
    ERL_NIF_TERM term;
    size_t used;

    (void)argc;
    if (!enif_inspect_binary(env, argv[0], &bin) || bin.size == 0)
        return enif_make_badarg(env);

    term = enif_make_atom(env, "undefined");
    used = enif_binary_to_term(env, bin.data, bin.size - 1, &term, 0);
    return enif_make_ulong(env, (unsigned long)used);
}

static ERL_NIF_TERM used_bad_opts(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{
    ErlNifBinary bin;
    ERL_NIF_TERM term;
    size_t used;

    (void)argc;
    if (!enif_inspect_binary(env, argv[0], &bin))
        return enif_make_badarg(env);

    term = enif_make_atom(env, "undefined");
    used = enif_binary_to_term(env, bin.data, bin.size, &term, 42);
    return enif_make_ulong(env, (unsigned long)used);
}

static ErlNifFunc nif_funcs[] = {
    {"decode_used", 1, decode_used, 0},
    {"used_truncated", 1, used_truncated, 0},
    {"used_bad_opts", 1, used_bad_opts, 0},
};

ERL_NIF_INIT(nif_bin2term, nif_funcs, NULL, NULL, NULL, NULL)
