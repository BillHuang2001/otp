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
 * src/nif/enif/fixtures/nif_resource_down.c -- a real C NIF (contract A.3) that
 * pins what a resource type's `down` callback OBSERVES when a process monitor
 * fires, and what `enif_demonitor_process` answers around it.
 *
 * Ground truth `erts/emulator/beam/erl_nif.c`:
 *
 *   `erts_fire_nif_monitor` (`:3058-3115`) deletes the monitor from the
 *   resource's tree, takes a protective reference (`:3092`), builds the operands
 *   `nif_pid.pid = omon->other.item` (the RECORDED target pid, `:3106`) and
 *   `erts_ref_to_driver_monitor(mdp->ref, &nif_monitor)` (the very 24 bytes the
 *   arm stored, `:3105`) and calls
 *   `type->fn.down(&msg_env.env, resource->data, &nif_pid, &nif_monitor)`
 *   (`:3107-3108`). A second fire cannot happen: the tree entry is gone.
 *   `enif_demonitor_process` (`:4019-4052`) answers `0` when it found the entry
 *   in the resource's own tree and `1` otherwise.
 *
 * The 24 monitor bytes are opaque (a pointer plus a header word), so the fixture
 * reports their RELATION to the copy the arm stored -- the only
 * runtime-independent, printable fact -- rather than the bytes themselves.
 *
 * Entries:
 *   arm/1       allocate the resource (once) and arm a monitor on the given pid;
 *               returns `enif_monitor_process`'s code. Resets the counters.
 *   state/0     `{Count, Pid, Mon}`: how many times `down` ran, whether its pid
 *               operand was the pid word the arm recorded (`same` | `other` |
 *               `none` when nothing fired) and whether its 24 monitor bytes
 *               equal the arm's (`1` | `0` | `-1` when nothing fired).
 *   obj/0       `1` when `down`'s `obj` operand was the armed resource pointer,
 *               `0` when it was not, `-1` while no `down` has run.
 *   demonitor/0 `enif_demonitor_process`'s code (`0` found+removed, `1` not).
 *   release/0   drop the fixture's own reference (a dying resource cancels its
 *               monitors and runs NO `down`); answers `ok`.
 *
 * Only `enif_*` slots this crate populates are referenced and none is
 * C-variadic (`enif_make_tuple_from_array`, not `enif_make_tuple3`).
 */
#include <erl_nif.h>
#include <string.h>

/* `ERTS_REF_THING_SIZE * sizeof(Eterm)` -- the monitor bytes the C copies
 * (`io.c:6994-7010`) and compares (`erl_nif.c:4057`). */
#define MON_BYTES 24

static ErlNifResourceType* down_type = NULL;

/* The fixture's own reference on the (single) resource object. */
static void* g_obj = NULL;
/* The holder the arm stored, and the pid word the arm recorded. */
static ErlNifMonitor g_mon;
static unsigned char g_arm_bytes[MON_BYTES];
static ERL_NIF_TERM g_arm_pid = 0;

/* What the `down` callback observed. */
static int g_count = 0;
static int g_pid_match = -1;
static int g_mon_match = -1;
static int g_obj_match = -1;

static void dtor_cb(ErlNifEnv* env, void* obj) {
    (void)env; (void)obj;
}

static void down_cb(ErlNifEnv* env, void* obj, ErlNifPid* pid, ErlNifMonitor* mon) {
    (void)env;
    g_count++;
    g_obj_match = (obj == g_obj) ? 1 : 0;
    /* Word comparison only: the pid operand is the recorded target pid, and
     * dereferencing a dead process's pid term is not safe from here. */
    g_pid_match = (pid && pid->pid == g_arm_pid) ? 1 : 0;
    g_mon_match = (mon && memcmp(mon->data, g_arm_bytes, MON_BYTES) == 0) ? 1 : 0;
}

static ERL_NIF_TERM triple(ErlNifEnv* env, ERL_NIF_TERM a, ERL_NIF_TERM b, ERL_NIF_TERM c) {
    ERL_NIF_TERM arr[3];
    arr[0] = a; arr[1] = b; arr[2] = c;
    return enif_make_tuple_from_array(env, arr, 3);
}

static int load_cb(ErlNifEnv* env, void** priv, ERL_NIF_TERM load_info) {
    ErlNifResourceFlags tried;
    ErlNifResourceTypeInit init;
    (void)priv; (void)load_info;
    memset(&init, 0, sizeof(init));
    init.dtor = dtor_cb;
    init.down = down_cb;
    init.members = 3;
    down_type = enif_init_resource_type(env, "nif_resource_down_type", &init,
                                       ERL_NIF_RT_CREATE | ERL_NIF_RT_TAKEOVER, &tried);
    if (down_type == NULL) return 1;
    return 0;
}

static ERL_NIF_TERM arm(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifPid pid;
    int code;
    (void)argc;
    if (!down_type) return enif_make_badarg(env);
    if (!enif_get_local_pid(env, argv[0], &pid)) return enif_make_badarg(env);
    if (!g_obj) {
        g_obj = enif_alloc_resource(down_type, sizeof(int));
        if (!g_obj) return enif_make_badarg(env);
    }
    memset(&g_mon, 0, sizeof(g_mon));
    code = enif_monitor_process(env, g_obj, &pid, &g_mon);
    if (code == 0) {
        memcpy(g_arm_bytes, g_mon.data, MON_BYTES);
        g_arm_pid = pid.pid;
        g_count = 0;
        g_pid_match = -1;
        g_mon_match = -1;
        g_obj_match = -1;
    }
    return enif_make_int(env, code);
}

static ERL_NIF_TERM state(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ERL_NIF_TERM pid_atom;
    (void)argc; (void)argv;
    if (g_pid_match < 0) pid_atom = enif_make_atom(env, "none");
    else pid_atom = enif_make_atom(env, g_pid_match ? "same" : "other");
    return triple(env, enif_make_int(env, g_count), pid_atom, enif_make_int(env, g_mon_match));
}

static ERL_NIF_TERM obj(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void)argc; (void)argv;
    return enif_make_int(env, g_obj_match);
}

static ERL_NIF_TERM demonitor(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void)argc; (void)argv;
    if (!g_obj) return enif_make_int(env, 1);
    return enif_make_int(env, enif_demonitor_process(env, g_obj, &g_mon));
}

static ERL_NIF_TERM release(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void)argc; (void)argv;
    if (g_obj) {
        enif_release_resource(g_obj);
        g_obj = NULL;
    }
    return enif_make_atom(env, "ok");
}

static ErlNifFunc nif_resource_down_funcs[] = {
    {"arm", 1, arm, 0},
    {"state", 0, state, 0},
    {"obj", 0, obj, 0},
    {"demonitor", 0, demonitor, 0},
    {"release", 0, release, 0}
};

ERL_NIF_INIT(nif_resource_down, nif_resource_down_funcs, load_cb, NULL, NULL, NULL)
