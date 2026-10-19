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
 * tests/fixtures/nif_monitor_codes.c -- a real C NIF (contract A.3) pinning the
 * **three return codes** of `enif_monitor_process` (`src/nif/enif/process.rs`).
 *
 * Ground truth `erts/emulator/beam/erl_nif.c:3954-4011`, in the C's own order:
 *
 *   `-1`  the resource's type was opened WITHOUT a `down` callback ("cannot hold
 *         monitors"), tested BEFORE the pid is read (`:3968-3972`) -- so it wins
 *         over the undefined-pid answer;
 *   `1`   a null / `enif_set_pid_undefined` target (`:3975-3976`) or a target
 *         already dead (`:4004`); `*monitor` is left untouched;
 *   `0`   the monitor was armed and `*monitor` was filled with the reference
 *         block (`:4007-4010`); a null holder pointer is legal there.
 *
 * Each entry answers `{ReturnCode, WasHolderUntouched}` where the second element
 * is `1` when the holder still holds its zero bytes (untouched) and `0` when the
 * C wrote the reference block -- so one printed tuple distinguishes every path.
 *
 * There is deliberately NO `enif_demonitor_process` entry: probing it with a
 * zeroed holder crashes the oracle VM, which would make this fixture unusable as
 * a cross-check.
 *
 * Only `enif_*` slots this crate populates are referenced, and none of them is
 * C-variadic (`enif_make_tuple_from_array`, not `enif_make_tuple2`), so
 * `dlopen(RTLD_NOW)` succeeds whether or not the variadic shim could be built.
 */
#include <erl_nif.h>
#include <string.h>

static ErlNifResourceType* with_down_type = NULL;
static ErlNifResourceType* no_down_type = NULL;

static void down_cb(ErlNifEnv* env, void* obj, ErlNifPid* pid, ErlNifMonitor* mon) {
    (void)env; (void)obj; (void)pid; (void)mon;
}
static void dtor_cb(ErlNifEnv* env, void* obj) { (void)env; (void)obj; }

static ERL_NIF_TERM pair(ErlNifEnv* env, ERL_NIF_TERM a, ERL_NIF_TERM b) {
    ERL_NIF_TERM arr[2];
    arr[0] = a; arr[1] = b;
    return enif_make_tuple_from_array(env, arr, 2);
}

static int is_zero(const ErlNifMonitor* mon) {
    ErlNifMonitor zero;
    memset(&zero, 0, sizeof(zero));
    return memcmp(mon->data, zero.data, sizeof(zero.data)) == 0;
}

static int load_cb(ErlNifEnv* env, void** priv, ERL_NIF_TERM load_info) {
    ErlNifResourceFlags tried;
    ErlNifResourceTypeInit init;
    (void)priv; (void)load_info;
    memset(&init, 0, sizeof(init));
    init.dtor = dtor_cb; init.down = down_cb; init.members = 3;
    with_down_type = enif_init_resource_type(env, "nif_monitor_codes_with_down", &init,
                                             ERL_NIF_RT_CREATE | ERL_NIF_RT_TAKEOVER, &tried);
    no_down_type = enif_open_resource_type(env, NULL, "nif_monitor_codes_no_down", dtor_cb,
                                          ERL_NIF_RT_CREATE | ERL_NIF_RT_TAKEOVER, &tried);
    if (with_down_type == NULL || no_down_type == NULL) return 1;
    return 0;
}

static ERL_NIF_TERM arm_live(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifPid me; ErlNifMonitor mon; void* res; int code;
    (void)argc; (void)argv;
    if (!with_down_type || enif_self(env, &me) == NULL) return enif_make_badarg(env);
    res = enif_alloc_resource(with_down_type, sizeof(int));
    if (!res) return enif_make_badarg(env);
    memset(&mon, 0, sizeof(mon));
    code = enif_monitor_process(env, res, &me, &mon);
    return pair(env, enif_make_int(env, code), enif_make_int(env, is_zero(&mon)));
}

static ERL_NIF_TERM arm_undefined(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifPid pid; ErlNifMonitor mon; void* res; int code;
    (void)argc; (void)argv;
    if (!with_down_type) return enif_make_badarg(env);
    res = enif_alloc_resource(with_down_type, sizeof(int));
    if (!res) return enif_make_badarg(env);
    enif_set_pid_undefined(&pid);
    memset(&mon, 0, sizeof(mon));
    code = enif_monitor_process(env, res, &pid, &mon);
    return pair(env, enif_make_int(env, code), enif_make_int(env, is_zero(&mon)));
}

static ERL_NIF_TERM arm_no_down(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifPid me; ErlNifMonitor mon; void* res; int code;
    (void)argc; (void)argv;
    if (!no_down_type || enif_self(env, &me) == NULL) return enif_make_badarg(env);
    res = enif_alloc_resource(no_down_type, sizeof(int));
    if (!res) return enif_make_badarg(env);
    memset(&mon, 0, sizeof(mon));
    code = enif_monitor_process(env, res, &me, &mon);
    return pair(env, enif_make_int(env, code), enif_make_int(env, is_zero(&mon)));
}

static ERL_NIF_TERM arm_no_down_undefined(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifPid pid; ErlNifMonitor mon; void* res; int code;
    (void)argc; (void)argv;
    if (!no_down_type) return enif_make_badarg(env);
    res = enif_alloc_resource(no_down_type, sizeof(int));
    if (!res) return enif_make_badarg(env);
    enif_set_pid_undefined(&pid);
    memset(&mon, 0, sizeof(mon));
    code = enif_monitor_process(env, res, &pid, &mon);
    return pair(env, enif_make_int(env, code), enif_make_int(env, is_zero(&mon)));
}

static ERL_NIF_TERM arm_null_holder(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifPid me; void* res; int code;
    (void)argc; (void)argv;
    if (!with_down_type || enif_self(env, &me) == NULL) return enif_make_badarg(env);
    res = enif_alloc_resource(with_down_type, sizeof(int));
    if (!res) return enif_make_badarg(env);
    code = enif_monitor_process(env, res, &me, NULL);
    return enif_make_int(env, code);
}

static ErlNifFunc nif_monitor_codes_funcs[] = {
    {"arm_live", 0, arm_live, 0},
    {"arm_undefined", 0, arm_undefined, 0},
    {"arm_no_down", 0, arm_no_down, 0},
    {"arm_no_down_undefined", 0, arm_no_down_undefined, 0},
    {"arm_null_holder", 0, arm_null_holder, 0}
};

ERL_NIF_INIT(nif_monitor_codes, nif_monitor_codes_funcs, load_cb, NULL, NULL, NULL)
