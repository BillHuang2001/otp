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
 * `nif_get_string_utf8` — a probe NIF for `enif_get_string` / `enif_get_string_length` with
 * `ERL_NIF_UTF8` (NIF C ABI, contract A.3).
 *
 * `probe(List, Len)`:
 *   1. fills a 64-byte buffer with the sentinel `0x2A` (`'*'`),
 *   2. calls `enif_get_string(env, List, (char *)buf, Len, ERL_NIF_UTF8)`,
 *   3. returns `{ReturnValue, Window}` where `Window` is the first `min(Len, 8)` bytes of the buffer
 *      as a list of integers.
 *
 * Printing the whole *requested* window (not only the bytes the call claims to have written) pins
 * both the value returned and **where the terminating NUL landed** — plus that nothing past it was
 * clobbered. This is what makes the UTF-8 truncation contract observable: a run that emitted a
 * partial multi-byte sequence leaves the sentinel bytes displaced, even when the returned integer
 * happens to match.
 *
 * `len(List)` does the same for `enif_get_string_length(env, List, &Out, ERL_NIF_UTF8)` and returns
 * `{ReturnValue, Out}`.
 *
 * The fixture is deliberately I/O-free and returns only plain integer tuples/list so that the very
 * same `.so` can be loaded by the system Erlang and the printed observations compared line for line.
 */
#include <string.h>

#include "erl_nif.h"

/* The caller's buffer size, the untouched-byte sentinel and the printed window width. */
#define BUF_BYTES 64
#define SENTINEL  0x2A
#define WINDOW    8

/* The number of buffer bytes printed for a request of `len` bytes. */
static unsigned window_bytes(unsigned len)
{
    return len < WINDOW ? len : WINDOW;
}

/* The first `n` buffer bytes as a list of small integers. */
static ERL_NIF_TERM window_list(ErlNifEnv *env, const unsigned char *buf, unsigned n)
{
    ERL_NIF_TERM cells[WINDOW];
    unsigned i;

    for (i = 0; i < n; i++) {
        cells[i] = enif_make_int(env, (int)buf[i]);
    }
    return enif_make_list_from_array(env, cells, n);
}

static ERL_NIF_TERM probe(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{
    unsigned len;
    int ret;
    unsigned char buf[BUF_BYTES];
    ERL_NIF_TERM result[2];

    if (argc != 2 || !enif_get_uint(env, argv[1], &len)) {
        return enif_make_badarg(env);
    }
    if (len > BUF_BYTES) {
        len = BUF_BYTES;
    }
    memset(buf, SENTINEL, sizeof(buf));
    ret = enif_get_string(env, argv[0], (char *)buf, len, ERL_NIF_UTF8);
    result[0] = enif_make_int(env, ret);
    result[1] = window_list(env, buf, window_bytes(len));
    return enif_make_tuple_from_array(env, result, 2);
}

static ERL_NIF_TERM length(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{
    unsigned out = 0;
    int ret;
    ERL_NIF_TERM result[2];

    if (argc != 1) {
        return enif_make_badarg(env);
    }
    ret = enif_get_string_length(env, argv[0], &out, ERL_NIF_UTF8);
    result[0] = enif_make_int(env, ret);
    result[1] = enif_make_uint(env, out);
    return enif_make_tuple_from_array(env, result, 2);
}

static ErlNifFunc nif_funcs[] = {
    {"probe", 2, probe, 0},
    {"len", 1, length, 0}
};

ERL_NIF_INIT(nif_get_string_utf8, nif_funcs, NULL, NULL, NULL, NULL)
