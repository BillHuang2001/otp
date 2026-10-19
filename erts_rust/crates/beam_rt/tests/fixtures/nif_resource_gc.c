/* %CopyrightBegin%
 *
 * SPDX-License-Identifier: Apache-2.0
 *
 * Copyright Ericsson AB 2025-2026. All Rights Reserved.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 *
 * %CopyrightEnd%
 */
/*
 * tests/fixtures/nif_resource_gc.c -- a real C NIF (contract A.3) whose resource
 * type's destructor counts how many times it ran, so the test can observe the
 * runtime's **NIF-resource reclamation** end to end
 * (crates/beam_rt/src/context/gc/resources.rs).
 *
 * Ground truth (`erts/emulator/beam/erl_nif.c`): `enif_make_resource` (`:3197`)
 * builds an `ERTS_MAGIC_REF` that owns one reference on the resource
 * (`erts_bld_resource_ref` -> `erts_mk_magic_ref` -> `erts_bin_inc_refc`), and the
 * term GC drops that reference when the term becomes unreachable -- the release that
 * reaches zero runs the type's `dtor` (`:2961-3030`). So after
 *
 *     alloc (refcount 1) -> enif_make_resource (2) -> enif_release_resource (1)
 *
 * the object is held by the term ALONE, and a runtime that never notices the term
 * dying never runs the destructor: the measured pre-fix divergence was oracle `1`,
 * beam_rt `0`.
 *
 * Entry points:
 *   * `probe/0`            -- mint a term that holds the object's only reference;
 *   * `probe_unexported/0` -- alloc + release, never exported as a term, so the
 *                             allocator's reference is the last one and the release
 *                             destroys the object immediately (no GC involved);
 *   * `dtor_count/0`       -- how many times the type's destructor has run;
 *   * `is_resource/1`      -- whether a term is still a term-bound resource, i.e. whether
 *                             `enif_get_resource` resolves its reference to the object
 *                             (`1`/`0`). This is the *independent* readability witness a
 *                             destructor count cannot give: a term whose binding the
 *                             resource prune released answers `0` from then on, so a
 *                             survivor reports `1` and a dangling copy reports `0`.
 *
 * The destructor's *timing* differs by design (the C defers it as misc aux work, this
 * tree runs it synchronously), so only COUNTS are comparable -- which is exactly what
 * these entry points expose. The oracle half of the test settles each step with
 * `erlang:garbage_collect/0` for that reason.
 *
 * Only `enif_*` slots this tree populates are referenced, and none of them is
 * C-variadic, so `dlopen(RTLD_NOW)` succeeds.
 */
#include <erl_nif.h>

static ErlNifResourceType *gc_type;
static int dtor_runs;

static void gc_dtor(ErlNifEnv *env, void *obj) { (void)env; (void)obj; dtor_runs++; }

static ERL_NIF_TERM probe(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{
    void *obj;
    ERL_NIF_TERM term;
    (void)argc; (void)argv;
    obj = enif_alloc_resource(gc_type, 8);
    if (obj == NULL) return enif_make_badarg(env);
    *(unsigned char *)obj = 42;
    term = enif_make_resource(env, obj);
    /* Drop the allocator's reference: only the term's is left. */
    enif_release_resource(obj);
    return term;
}

static ERL_NIF_TERM probe_unexported(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{
    void *obj;
    (void)argc; (void)argv;
    obj = enif_alloc_resource(gc_type, 8);
    if (obj == NULL) return enif_make_badarg(env);
    *(unsigned char *)obj = 7;
    /* Never exported: this release takes the count to zero and destroys the object. */
    enif_release_resource(obj);
    return enif_make_int(env, 1);
}

static ERL_NIF_TERM dtor_count(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{ (void)argc; (void)argv; return enif_make_int(env, dtor_runs); }

static ERL_NIF_TERM is_resource(ErlNifEnv *env, int argc, const ERL_NIF_TERM argv[])
{
    void *obj;
    if (argc != 1) return enif_make_badarg(env);
    return enif_make_int(env, enif_get_resource(env, argv[0], gc_type, &obj) ? 1 : 0);
}
static int load(ErlNifEnv *env, void **priv_data, ERL_NIF_TERM load_info)
{
    ErlNifResourceFlags tried;
    (void)priv_data; (void)load_info;
    gc_type = enif_open_resource_type(env, NULL, "gc_type", gc_dtor,
                                      ERL_NIF_RT_CREATE|ERL_NIF_RT_TAKEOVER, &tried);
    return gc_type == NULL ? 1 : 0;
}

static ErlNifFunc funcs[] = {
    {"probe", 0, probe}, {"probe_unexported", 0, probe_unexported}, {"dtor_count", 0, dtor_count},
    {"is_resource", 1, is_resource}
};
ERL_NIF_INIT(nif_resource_gc, funcs, load, NULL, NULL, NULL)
