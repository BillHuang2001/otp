/*
 * %CopyrightBegin%
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
 * The **`enif_monitor_process` / `down` end-to-end** fixture for `beam_rt`'s A.3 suite
 * (`tests/nif_monitor.rs`). A real C NIF, compiled at test time with the host `cc` against the
 * host `erl_nif.h` and loaded by both this runtime and the system `erl` oracle.
 *
 * Ground truth `erts/emulator/beam/erl_nif.c`:
 *
 *   `enif_monitor_process` (`:3954-4011`) refuses a resource type that has no `down`
 *   callback (`rm = rsrc->monitors; if (!rm) return -1;`, `:3968-3972`), answers `1` for an
 *   `enif_set_pid_undefined` target (`:3975-3976`) or a target with no live process
 *   (`erts_proc_lookup` fails, `:4004`), and otherwise inserts the monitor into the RESOURCE's
 *   own monitor tree (`:3983-3986`) and answers `0`.
 *   `erts_fire_nif_monitor` (`:3058-3115`) deletes that tree entry and calls
 *   `type->fn.down(&msg_env.env, resource->data, &nif_pid, &nif_monitor)` (`:3107-3108`) with
 *   `nif_pid.pid = omon->other.item`, the RECORDED target pid (`:3106`). A second fire cannot
 *   happen — the entry is gone.
 *   `enif_demonitor_process` (`:4019-4052`) answers `0` when it found the entry in the
 *   resource's tree and removed it, `1` when it did not (already fired, already cancelled).
 *
 * Entries:
 *   make_res/0          the resource object carrying the `down` callback, as an Erlang term.
 *   arm/2               `enif_monitor_process` on `Pid` for `Res`; the code as an integer.
 *                       The holder is kept for `demonitor/1`.
 *   demonitor/1         `enif_demonitor_process` for the last armed holder; the code. When no
 *                       arm ever succeeded there is no holder to hand over — the C's `1`
 *                       ("could not be identified") without touching a zeroed holder, which
 *                       would crash the oracle VM.
 *   downs/0             how many times the `down` callback ran.
 *   down_pid_matches/1  `true` when the pid `down` observed is the one given, else `false`
 *                       (the C hands the callback the recorded target pid word; `enif_make_pid`
 *                       is not among the `enif_*` slots the Rust host exports, so the identity is
 *                       reported as a comparison the caller supplies the pid for).
 *   alive/1             `enif_is_process_alive` for `Pid`.
 *   arm_undefined/0     `enif_monitor_process` on an `enif_set_pid_undefined` target.
 */
#include <erl_nif.h>
#include <string.h>

static ErlNifResourceType* mon_type = NULL;

/* The fixture's own reference on its (single, process-wide) resource object. It is deliberately
 * never released: a destroyed resource cancels its monitors without firing `down`
 * (`run_resource_dtor`, `erl_nif.c:2974-3030`), which would silently disarm a later phase. */
static void* g_obj = NULL;

/* The holder the last successful `arm/2` stored, and whether there is one. */
static ErlNifMonitor g_mon;
static int g_armed = 0;

/* What the `down` callback observed: how many times it ran and the pid WORD it was handed. */
static int g_count = 0;
static ERL_NIF_TERM g_down_pid = 0;
static int g_down_pid_seen = 0;

static void dtor_cb(ErlNifEnv* env, void* obj) {
    (void)env; (void)obj;
}

static void down_cb(ErlNifEnv* env, void* obj, ErlNifPid* pid, ErlNifMonitor* mon) {
    (void)env; (void)obj; (void)mon;
    g_count++;
    /* A word copy only (the C hands over the recorded pid term), never a dereference: the
     * target is dead by definition here. */
    if (pid) { g_down_pid = pid->pid; g_down_pid_seen = 1; }
}

static int load_cb(ErlNifEnv* env, void** priv, ERL_NIF_TERM load_info) {
    ErlNifResourceFlags tried;
    ErlNifResourceTypeInit init;
    (void)priv; (void)load_info;
    memset(&init, 0, sizeof(init));
    init.dtor = dtor_cb;
    init.down = down_cb;
    init.members = 3;
    mon_type = enif_init_resource_type(env, "nif_monitor_type", &init,
                                       ERL_NIF_RT_CREATE | ERL_NIF_RT_TAKEOVER, &tried);
    if (mon_type == NULL) return 1;
    return 0;
}

static ERL_NIF_TERM make_res(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void)argc; (void)argv;
    if (!mon_type) return enif_make_badarg(env);
    if (!g_obj) {
        g_obj = enif_alloc_resource(mon_type, sizeof(int));
        if (!g_obj) return enif_make_badarg(env);
        *(int*)g_obj = 0;
        g_count = 0;
        g_down_pid_seen = 0;
        g_down_pid = 0;
        g_armed = 0;
    }
    /* The term takes its own reference; the fixture keeps its own too (see `g_obj`). */
    return enif_make_resource(env, g_obj);
}

static ERL_NIF_TERM arm(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    void* obj = NULL;
    ErlNifPid pid;
    int code;
    (void)argc;
    if (!mon_type) return enif_make_badarg(env);
    if (!enif_get_resource(env, argv[0], mon_type, &obj)) return enif_make_atom(env, "not_a_resource");
    if (!enif_get_local_pid(env, argv[1], &pid)) return enif_make_atom(env, "not_a_pid");
    memset(&g_mon, 0, sizeof(g_mon));
    g_armed = 0;
    code = enif_monitor_process(env, obj, &pid, &g_mon);
    if (code == 0) g_armed = 1;
    return enif_make_int(env, code);
}

static ERL_NIF_TERM demonitor(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    void* obj = NULL;
    int code;
    (void)argc;
    if (!mon_type) return enif_make_badarg(env);
    if (!enif_get_resource(env, argv[0], mon_type, &obj)) return enif_make_atom(env, "not_a_resource");
    if (!g_armed) return enif_make_int(env, 1);
    code = enif_demonitor_process(env, obj, &g_mon);
    g_armed = 0;
    return enif_make_int(env, code);
}

static ERL_NIF_TERM downs(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    (void)argc; (void)argv;
    return enif_make_int(env, g_count);
}

static ERL_NIF_TERM down_pid_matches(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifPid pid;
    int same;
    (void)argc;
    if (!enif_get_local_pid(env, argv[0], &pid)) return enif_make_atom(env, "not_a_pid");
    same = (g_down_pid_seen && pid.pid == g_down_pid) ? 1 : 0;
    return enif_make_atom(env, same ? "true" : "false");
}

static ERL_NIF_TERM alive(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifPid pid;
    (void)argc;
    if (!enif_get_local_pid(env, argv[0], &pid)) return enif_make_atom(env, "not_a_pid");
    return enif_make_int(env, enif_is_process_alive(env, &pid));
}

static ERL_NIF_TERM arm_undefined(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    ErlNifPid pid;
    int code;
    (void)argc; (void)argv;
    if (!mon_type || !g_obj) return enif_make_atom(env, "no_resource");
    enif_set_pid_undefined(&pid);
    memset(&g_mon, 0, sizeof(g_mon));
    g_armed = 0;
    code = enif_monitor_process(env, g_obj, &pid, &g_mon);
    if (code == 0) g_armed = 1;
    return enif_make_int(env, code);
}

static ErlNifFunc nif_monitor_funcs[] = {
    {"make_res", 0, make_res, 0},
    {"arm", 2, arm, 0},
    {"demonitor", 1, demonitor, 0},
    {"downs", 0, downs, 0},
    {"down_pid_matches", 1, down_pid_matches, 0},
    {"alive", 1, alive, 0},
    {"arm_undefined", 0, arm_undefined, 0}
};

ERL_NIF_INIT(nif_monitor, nif_monitor_funcs, load_cb, NULL, NULL, NULL)
