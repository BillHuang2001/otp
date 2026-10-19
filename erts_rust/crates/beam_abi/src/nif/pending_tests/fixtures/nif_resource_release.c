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
 * tests/fixtures/nif_resource_release.c -- a real C NIF (contract A.3) pinning the
 * **reference-count semantics of a resource TERM** that
 * `src/nif/enif/resources_release.rs` reproduces through
 * `release_resource_terms/1`.
 *
 * Ground truth (`erts/emulator/beam/erl_nif.c`): `enif_make_resource` (`:3197`)
 * builds an `ERTS_MAGIC_REF` that owns one reference on the resource
 * (`erts_bld_resource_ref` -> `erts_mk_magic_ref` -> `erts_bin_inc_refc`), so
 *   * alloc (1) + make_resource (2) + release (1) leaves the object alive,
 *     held by the term alone -- and its destructor does NOT run;
 *   * when the term becomes unreachable the term GC drops that reference, and
 *     the release that reaches zero runs the type's `dtor` (`:2961-3030`);
 *   * an object that is never exported as a term has only the allocator's
 *     reference, so `enif_release_resource` destroys it immediately.
 *
 * The three entry points are exactly the steps the Rust seam can drive without a
 * GC: mint a term (`probe`), mint-and-drop without ever exposing a term
 * (`probe_unexported`), and read the destructor count (`dtor_count`).
 *
 * The destructor's *timing* differs by design: the C schedules it as misc aux
 * work (deferred), this crate runs it synchronously. Only COUNTS are therefore
 * comparable, never timing -- the oracle half of `tests/nif_resource_release.rs`
 * settles with `erlang:garbage_collect/0` for that reason.
 *
 * Only `enif_*` slots this crate populates are referenced, and none of them is
 * C-variadic, so `dlopen(RTLD_NOW)` succeeds whether or not the variadic shim
 * could be compiled.
 */
#include <erl_nif.h>

static ErlNifResourceType *release_type;
static int dtor_runs;

static void release_dtor(ErlNifEnv *env, void *obj) { (void)env; (void)obj; dtor_runs++; }

static ERL_NIF_TERM probe(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{
    void *obj; ERL_NIF_TERM term;
    (void)argc; (void)argv;
    obj = enif_alloc_resource(release_type, 4);
    if (obj == NULL) return enif_make_badarg(env);
    *(unsigned char *)obj = 42;
    term = enif_make_resource(env, obj);
    enif_release_resource(obj);
    return term;
}

static ERL_NIF_TERM probe_unexported(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{
    void *obj;
    (void)argc; (void)argv;
    obj = enif_alloc_resource(release_type, 4);
    if (obj == NULL) return enif_make_badarg(env);
    *(unsigned char *)obj = 7;
    enif_release_resource(obj);
    return enif_make_int(env, 1);
}

static ERL_NIF_TERM dtor_count(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{ (void)argc; (void)argv; return enif_make_int(env, dtor_runs); }

static int load(ErlNifEnv *env, void **priv_data, ERL_NIF_TERM load_info)
{
    ErlNifResourceFlags tried;
    (void)priv_data; (void)load_info;
    release_type = enif_open_resource_type(env, NULL, "release_type", release_dtor,
                                          ERL_NIF_RT_CREATE|ERL_NIF_RT_TAKEOVER, &tried);
    return release_type == NULL ? 1 : 0;
}

static ErlNifFunc funcs[] = {
    {"probe", 0, probe}, {"probe_unexported", 0, probe_unexported}, {"dtor_count", 0, dtor_count}
};

ERL_NIF_INIT(nif_resource_release, funcs, load, NULL, NULL, NULL)
