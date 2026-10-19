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
 * tests/fixtures/nif_map_arrays.c -- a real C NIF (contract A.3) pinning the
 * **duplicate-key refusal** of `enif_make_map_from_arrays`
 * (`src/nif/enif/map.rs`).
 *
 * The C ground truth:
 *
 *   `erts_map_from_ks_and_vs` (`erts/emulator/beam/erl_map.c:671-714`) is
 *   documented "Returns THE_NON_VALUE if duplicate keys found." -- the flat arm
 *   refuses as soon as `erts_cmp_flatmap_keys(..) == 0` inside
 *   `erts_validate_and_sort_flatmap`, the hash arm passes `reject_dupkeys = 1`.
 *   `enif_make_map_from_arrays` (`erts/emulator/beam/erl_nif.c:3664-3697`)
 *   stores that term in `*map_out` and answers `succeeded` -- an `int`, 1 or 0 --
 *   so a duplicate-key call answers 0 and leaves THE_NON_VALUE behind.
 *
 * The basis is the *exact* (`=:=`) key identity, which is why an integer and a
 * float of the same value are NOT duplicates.
 *
 * Every NIF below returns `{ReturnCode, Detail}`, built so one printed tuple
 * distinguishes "accepted, map of size N" from "refused":
 *
 *   ReturnCode  the `int` `enif_make_map_from_arrays` answered (1 or 0)
 *   Detail      on acceptance: `maps:size/1` of the out term (a small integer)
 *               on refusal:    the atom `overwritten` when the out slot no
 *                              longer holds the sentinel pre-written into it
 *                              (the C does write THE_NON_VALUE there), or
 *                              `unchanged` when the slot was left alone
 *
 * So the SAME `.so` can be loaded by the system Erlang (`erlang:load_nif/2`)
 * and by `beam_abi`'s own loader (`tests/nif_map_arrays.rs` drives both), and
 * the measured oracle tuples are the expected constants.
 *
 * Only `enif_*` slots this crate populates are referenced, and none of them is
 * C-variadic (no `enif_make_tuple`), so `dlopen(RTLD_NOW)` succeeds whether or
 * not the variadic shim could be compiled.
 */
#include <erl_nif.h>

/* The sentinel pre-written into the out slot: any refusal must replace it. */
#define SENTINEL -999

/* `{ReturnCode, Detail}` -- see the header comment. */
static ERL_NIF_TERM report(ErlNifEnv* env, int rc, ERL_NIF_TERM map, ERL_NIF_TERM sentinel)
{
    ERL_NIF_TERM out[2];
    out[0] = enif_make_int(env, rc);
    if (rc) {
        size_t size = 0;
        if (!enif_get_map_size(env, map, &size))
            out[1] = enif_make_atom(env, "not_a_map");
        else
            out[1] = enif_make_int(env, (int)size);
    } else {
        out[1] = enif_make_atom(env, enif_is_identical(map, sentinel)
                                         ? "unchanged" : "overwritten");
    }
    return enif_make_tuple_from_array(env, out, 2);
}

/* Run one `enif_make_map_from_arrays` over `cnt` key/value pairs, from a
 * sentinel-initialised out slot, and report it. */
static ERL_NIF_TERM build(ErlNifEnv* env, ERL_NIF_TERM* keys, ERL_NIF_TERM* values, size_t cnt)
{
    ERL_NIF_TERM sentinel = enif_make_int(env, SENTINEL);
    ERL_NIF_TERM map = sentinel;
    int rc = enif_make_map_from_arrays(env, keys, values, cnt, &map);
    return report(env, rc, map, sentinel);
}

/* ---------------------------------------------------------------------------
 * accepted: distinct keys, on both sides of the flat/hash threshold
 * ------------------------------------------------------------------------- */

/* Three distinct keys -> the flat arm. Oracle: {1,3}. */
static ERL_NIF_TERM unique_0(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM keys[3], values[3];
    int i;
    (void)argc;
    (void)argv;
    for (i = 0; i < 3; i++) {
        keys[i] = enif_make_int(env, i + 1);
        values[i] = enif_make_int(env, (i + 1) * 100);
    }
    return build(env, keys, values, 3);
}

/* Forty distinct keys -> the hash arm, which must NOT refuse a real map.
 * Oracle: {1,40}. */
static ERL_NIF_TERM unique_hash_0(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM keys[40], values[40];
    int i;
    (void)argc;
    (void)argv;
    for (i = 0; i < 40; i++) {
        keys[i] = enif_make_int(env, i);
        values[i] = enif_make_int(env, i * 10);
    }
    return build(env, keys, values, 40);
}

/* `1` and `1.0` are DIFFERENT map keys (`=:=`, not `==`), so this is accepted.
 * Oracle: {1,2}. */
static ERL_NIF_TERM int_float_0(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM keys[2], values[2];
    (void)argc;
    (void)argv;
    keys[0] = enif_make_int(env, 1);
    keys[1] = enif_make_double(env, 1.0);
    values[0] = enif_make_int(env, 100);
    values[1] = enif_make_int(env, 200);
    return build(env, keys, values, 2);
}

/* ---------------------------------------------------------------------------
 * refused: one key repeated, on both sides of the threshold
 * ------------------------------------------------------------------------- */

/* `7` twice inside the flat arm. Oracle: {0,overwritten}. */
static ERL_NIF_TERM dup_flat_0(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM keys[3], values[3];
    (void)argc;
    (void)argv;
    keys[0] = enif_make_int(env, 7); values[0] = enif_make_int(env, 10);
    keys[1] = enif_make_int(env, 8); values[1] = enif_make_int(env, 20);
    keys[2] = enif_make_int(env, 7); values[2] = enif_make_int(env, 30);
    return build(env, keys, values, 3);
}

/* 34 pairs -- `0`..`32` plus a second `5` -- so the hash arm's `reject_dupkeys`
 * does the refusing. Oracle: {0,overwritten}. */
static ERL_NIF_TERM dup_hash_0(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM keys[34], values[34];
    int i;
    (void)argc;
    (void)argv;
    for (i = 0; i < 33; i++) {
        keys[i] = enif_make_int(env, i);
        values[i] = enif_make_int(env, i);
    }
    keys[33] = enif_make_int(env, 5);
    values[33] = enif_make_int(env, 999);
    return build(env, keys, values, 34);
}

/* Two tuples with equal CONTENTS at distinct addresses: `=:=`-equal, so the
 * exact comparison refuses. Oracle: {0,overwritten}. */
static ERL_NIF_TERM dup_tuple_0(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM pair[2], keys[2], values[2];
    (void)argc;
    (void)argv;
    pair[0] = enif_make_int(env, 1);
    pair[1] = enif_make_int(env, 2);
    keys[0] = enif_make_tuple_from_array(env, pair, 2);
    keys[1] = enif_make_tuple_from_array(env, pair, 2);
    values[0] = enif_make_int(env, 10);
    values[1] = enif_make_int(env, 20);
    return build(env, keys, values, 2);
}

static ErlNifFunc nif_funcs[] = {
    {"unique", 0, unique_0, 0},
    {"unique_hash", 0, unique_hash_0, 0},
    {"int_float", 0, int_float_0, 0},
    {"dup_flat", 0, dup_flat_0, 0},
    {"dup_hash", 0, dup_hash_0, 0},
    {"dup_tuple", 0, dup_tuple_0, 0},
};

ERL_NIF_INIT(test_nif_map_arrays, nif_funcs, NULL, NULL, NULL, NULL)
