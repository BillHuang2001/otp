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
 * `enif_schedule_nif`'s **success path** — a real C NIF (contract A.3) that pins every observable
 * of a scheduled (rescheduled / "yielded") NIF call, for the `beam_bifs`/`beam_abi` A.3 work.
 *
 * It is an INVESTIGATION ARTIFACT: compiled with the host `cc` against the host `erl_nif.h` and
 * run against the live system `erl` oracle, so the transcript it prints is ground truth for a Rust
 * implementer (see the sibling `a3_schedule.erl` driver and the report that accompanies this file).
 * A later step turns it into an automated differential case.
 *
 * Ground truth `erts/emulator/beam/erl_nif.c`:
 *
 *   `enif_schedule_nif` (`:3561-3604`) refuses `argc > MAX_ARG` (`:3569`, `MAX_ARG` = 255,
 *   `erl_vm.h:48`) and a `flags` carrying any bit outside `ERL_NIF_DIRTY_JOB_{CPU,IO}_BOUND`
 *   (`:3584-3598`) — both by RAISING `badarg` through `enif_make_badarg` (`:1791-1794`) rather than
 *   returning an error term. Otherwise it calls `schedule()` (`:367-400`) and returns
 *   `(ERL_NIF_TERM)THE_NON_VALUE` (`:399`) — i.e. the word `0` (`erl_term.h:376-381`), which is
 *   NOT a term: the value the Erlang caller will see comes from the continuation instead.
 *
 *   `schedule()` (`:367-400`) → `erts_nfunc_schedule` (`erl_nfunc_sched.c:66-146`): on the FIRST
 *   schedule it saves the ORIGINAL call's `argv`/pc/mfa/current (`nep->argv`, `:106-118`), then
 *   copies the NEW `argv` into the x-register array (`:126-130`), sets the trampoline's mfa to
 *   `{Module, fun_name_atom, argc}` (`:131-134`, the name is informational — it is interned, not
 *   looked up) and puts the process back at the trampoline with `freason = TRAP` (`:137-138`).
 *   So a continuation runs with the *new* argv on the *same* env, and the process keeps its
 *   position for when the chain finally produces a value.
 *
 *   `execute_nif` (`:3512-3558`) is the `direct_fp` for a `flags == 0` schedule: it calls the
 *   continuation pointer and, when the result is a value or the process is no longer TRAPping,
 *   restores the original NIF MFA (`nfunc_restore`) — that is where the continuation's value becomes
 *   the call's result — while a RAISED continuation takes the `nfunc_cleanup_nif_mod` +
 *   `EXF_RESTORE_NFUNC` branch instead (`:3534-3541`). A dirty-flagged schedule passes the
 *   continuation pointer itself as `direct_fp` with `indirect_fp = NULL` (`:3587-3590`), so it is
 *   entered straight from the dirty scheduler (`erts_call_dirty_nif`, `:426-`);
 *   `enif_thread_type()` (`:3606`) is what tells the two apart.
 *
 * Entries (each is a `{name, arity}` row of `nif_funcs` below):
 *   sched_sum/2          A. schedule `cont_sum`, which builds `{scheduled, N+M}` from ITS argv.
 *   sched_loop/1         B. a self-rescheduling chain `cont_loop` counting down argv[0]; the
 *                           observable is that a large N completes with a constant stack and reports
 *                           `{done, Yields}`.
 *   sched_raise/0        C. schedule `cont_raise`, which raises `boom`; the chain must stop and the
 *                           caller must see the raise.
 *   sched_bad_flags/0    D1. `flags = 0x1000` → `enif_make_badarg`.
 *   sched_too_many_args/0 D2. `argc = 300 > MAX_ARG` → `enif_make_badarg`.
 *   sched_dirty/1        E. `ERL_NIF_DIRTY_JOB_CPU_BOUND` schedule of `cont_dirty` → `{dirty, N}`.
 *   sched_last_ret/0     F1. The RAW word the most recent scheduling entry received from
 *                            `enif_schedule_nif` (0 == THE_NON_VALUE), as an `unsigned long`.
 *   sched_thread_types/1 F2. `{threads, CallerType, ContType, FlagsArg}` — `enif_thread_type()` in
 *                            the scheduling call and in the continuation, so a normal schedule vs a
 *                            dirty one can be told apart.
 *   sched_ghost_name/1   F3. schedule with a `fun_name` that is NOT an entry of this module, to show
 *                            the name is only interned (`:3571`), never looked up.
 *   sched_reargv/1       F4. a continuation that reschedules with a DIFFERENT argv and then raises,
 *                            pinning whose argument list a raise from the chain is rendered with.
 *
 * LOAD-TIME CONSTRAINT (found the hard way, see the report): `erlang:load_nif/2` validates EVERY
 * row of `nif_funcs` against the module's own function table — `erts_atom_get(f->name …)` plus
 * `get_func_ix(this_mi->code_hdr, f_atom, f->arity)` (`erl_nif.c:4874-4878`), failing the whole load
 * with `{error,{bad_lib,"Function not found M:F/A"}}` — so a continuation listed here must also be
 * a real (exported) function of the `.erl` module. `fun_name` in `enif_schedule_nif` is a different
 * thing: it is interned only and is never resolved.
 *
 * State: `last_sched_ret` / `loop_yields` / `caller_thread_type` are module-global and the probes
 * are meant to be driven one at a time from a single process (this is a probe, not a library).
 */
#include <erl_nif.h>

/* `argc` past `MAX_ARG` (255, `erl_vm.h:48`) is the D2 refusal. */
#define TOO_MANY_ARGS 300
/* A flag word outside `ERL_NIF_DIRTY_JOB_CPU_BOUND | ERL_NIF_DIRTY_JOB_IO_BOUND` is the D1 refusal
 * (`erl_nif.c:3584-3598`); `erl_nif.h` has no name for such a bit. */
#define BAD_FLAGS 0x1000

/* The raw word the last scheduling entry got back from `enif_schedule_nif()` (`sched_last_ret/0`).
 * The C returns THE_NON_VALUE == 0 there (`erl_nif.c:399`), NEVER the continuation's term. */
static ERL_NIF_TERM last_sched_ret;
/* Continuation entries in the current `sched_loop/1` chain (reset by the scheduling entry). */
static int loop_yields;
/* `enif_thread_type()` inside the scheduling call of `sched_thread_types/1`. */
static int caller_thread_type;

static ERL_NIF_TERM tuple2(ErlNifEnv* env, ERL_NIF_TERM a, ERL_NIF_TERM b)
{
    ERL_NIF_TERM arr[2];
    arr[0] = a;
    arr[1] = b;
    return enif_make_tuple_from_array(env, arr, 2);
}

/* ---- A: the continuation runs with the real env and the real argv ------------------------------ */

/* `{scheduled, N+M}` built from the arguments the CONTINUATION receives — they must be the ones the
 * scheduling call passed to `enif_schedule_nif` (`erl_nfunc_sched.c:126-130` copies them into the
 * x-register array verbatim, so the words — not copies of the terms — are what arrive here). */
static ERL_NIF_TERM cont_sum(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    int n = 0;
    int m = 0;
    if (argc != 2)
        return enif_make_badarg(env);
    if (!enif_get_int(env, argv[0], &n) || !enif_get_int(env, argv[1], &m))
        return enif_make_badarg(env);
    return tuple2(env, enif_make_atom(env, "scheduled"), enif_make_int(env, n + m));
}

/* `flags = 0` is the header's spelling of "normal scheduler": `erl_nif.h` has no
 * `ERL_NIF_SCHEDULER_NORMAL` constant, and the C only tests `flags == 0` (`:3584`). */
static ERL_NIF_TERM nif_sched_sum(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    last_sched_ret = enif_schedule_nif(env, "cont_sum", 0, cont_sum, argc, argv);
    return last_sched_ret;
}

/* ---- B: a long self-rescheduling chain -------------------------------------------------------- */

/* Decrement argv[0] and reschedule until it reaches 0. On the oracle this is a constant-stack
 * trampoline: every entry returns `THE_NON_VALUE` and `execute_nif` (`:3512-3558`) sees the process
 * still TRAPping, so it just loops in the emulator instead of nesting. */
static ERL_NIF_TERM cont_loop(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    int n = 0;
    ERL_NIF_TERM next[1];
    if (argc != 1)
        return enif_make_badarg(env);
    if (!enif_get_int(env, argv[0], &n))
        return enif_make_badarg(env);
    loop_yields++;
    if (n <= 0)
        return tuple2(env, enif_make_atom(env, "done"), enif_make_int(env, loop_yields));
    next[0] = enif_make_int(env, n - 1);
    return enif_schedule_nif(env, "cont_loop", 0, cont_loop, 1, next);
}

static ERL_NIF_TERM nif_sched_loop(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    loop_yields = 0;
    last_sched_ret = enif_schedule_nif(env, "cont_loop", 0, cont_loop, argc, argv);
    return last_sched_ret;
}

/* ---- C: a raising continuation --------------------------------------------------------------- */

/* `enif_raise_exception` (`:1796-1801`) sets `fvalue`/`EXC_ERROR` and RETURNS THE_NON_VALUE (via
 * `BIF_ERROR`, `bif.h:230-233`), so "raise" and "reschedule" are both expressed as the non-value;
 * only `proc->freason` tells them apart (`execute_nif`, `:3527-3541`). */
static ERL_NIF_TERM cont_raise(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    (void)argc;
    (void)argv;
    return enif_raise_exception(env, enif_make_atom(env, "boom"));
}

static ERL_NIF_TERM nif_sched_raise(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    last_sched_ret = enif_schedule_nif(env, "cont_raise", 0, cont_raise, argc, argv);
    return last_sched_ret;
}

/* ---- D: the refusals ------------------------------------------------------------------------ */

/* `flags = 0x1000` → `enif_make_badarg` (`:3597`), i.e. a RAISE of `badarg`, never a term result. */
static ERL_NIF_TERM nif_sched_bad_flags(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    (void)argc;
    last_sched_ret = enif_schedule_nif(env, "cont_sum", BAD_FLAGS, cont_sum, 0, argv);
    return last_sched_ret;
}

/* `argc = 300 > MAX_ARG` → `enif_make_badarg` (`:3569`), checked BEFORE `function`/`flags`, so the
 * bogus flag-free continuation is never reached. */
static ERL_NIF_TERM nif_sched_too_many_args(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM big[TOO_MANY_ARGS];
    int i;
    (void)argc;
    (void)argv;
    for (i = 0; i < TOO_MANY_ARGS; i++)
        big[i] = enif_make_int(env, i);
    last_sched_ret = enif_schedule_nif(env, "cont_sum", 0, cont_sum, TOO_MANY_ARGS, big);
    return last_sched_ret;
}

/* ---- E: a dirty-flagged schedule ------------------------------------------------------------ */

static ERL_NIF_TERM cont_dirty(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    int n = 0;
    if (argc != 1)
        return enif_make_badarg(env);
    if (!enif_get_int(env, argv[0], &n))
        return enif_make_badarg(env);
    return tuple2(env, enif_make_atom(env, "dirty"), enif_make_int(env, n));
}

/* `ERL_NIF_DIRTY_JOB_CPU_BOUND` makes the continuation itself the trampoline's `direct_fp`
 * (`:3582-3590`), so it runs ON a dirty CPU scheduler instead of via `execute_nif`. */
static ERL_NIF_TERM nif_sched_dirty(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    last_sched_ret = enif_schedule_nif(env, "cont_dirty",
                                       ERL_NIF_DIRTY_JOB_CPU_BOUND, cont_dirty, argc, argv);
    return last_sched_ret;
}

/* ---- F: what the SCHEDULING NIF itself sees -------------------------------------------------- */

/* The raw word `enif_schedule_nif` returned (`0` == THE_NON_VALUE). */
static ERL_NIF_TERM nif_sched_last_ret(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    (void)argc;
    (void)argv;
    return enif_make_ulong(env, (unsigned long)last_sched_ret);
}

/* `{threads, CallerType, ContType, FlagsArg}`: `enif_thread_type()` (`:3606`) in the scheduling call
 * (always a normal scheduler) and in the continuation (normal for `flags = 0`, dirty CPU for the
 * dirty flag), plus the flag word the continuation was handed. */
static ERL_NIF_TERM cont_ttype(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM arr[4];
    int flags = 0;
    if (argc != 1)
        return enif_make_badarg(env);
    if (!enif_get_int(env, argv[0], &flags))
        return enif_make_badarg(env);
    arr[0] = enif_make_atom(env, "threads");
    arr[1] = enif_make_int(env, caller_thread_type);
    arr[2] = enif_make_int(env, enif_thread_type());
    arr[3] = enif_make_int(env, flags);
    return enif_make_tuple_from_array(env, arr, 4);
}

static ERL_NIF_TERM nif_sched_ttype(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    int dirty = 0;
    if (argc != 1)
        return enif_make_badarg(env);
    if (!enif_get_int(env, argv[0], &dirty))
        return enif_make_badarg(env);
    caller_thread_type = enif_thread_type();
    last_sched_ret = enif_schedule_nif(env, "cont_ttype",
                                       dirty ? ERL_NIF_DIRTY_JOB_CPU_BOUND : 0,
                                       cont_ttype, argc, argv);
    return last_sched_ret;
}

/* ---- F4: whose argv does a raise from the SECOND continuation entry render? ------------------ */

/* `sched_reargv(4242)` schedules with the caller's argv; the continuation replaces it with the atom
 * `not_an_int` and reschedules ONCE, so the next entry fails `enif_get_int` and raises `badarg`.
 * `schedule()` saved the original argv on the FIRST schedule only (`erl_nfunc_sched.c:106-118`), so
 * the stacktrace frame tells us which argument list the raise is rendered with. */
static ERL_NIF_TERM cont_reargv(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    ERL_NIF_TERM next[1];
    int n = 0;
    if (argc != 1)
        return enif_make_badarg(env);
    if (!enif_get_int(env, argv[0], &n)) /* second entry: the atom arrives here */
        return enif_make_badarg(env);
    next[0] = enif_make_atom(env, "not_an_int");
    return enif_schedule_nif(env, "cont_reargv", 0, cont_reargv, 1, next);
}

static ERL_NIF_TERM nif_sched_reargv(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    last_sched_ret = enif_schedule_nif(env, "cont_reargv", 0, cont_reargv, argc, argv);
    return last_sched_ret;
}

/* ---- F3: the `fun_name` argument is informational only -------------------------------------- */
static ERL_NIF_TERM cont_ghost(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    int n = 0;
    if (argc != 1)
        return enif_make_badarg(env);
    if (!enif_get_int(env, argv[0], &n))
        return enif_make_badarg(env);
    return tuple2(env, enif_make_atom(env, "ghost"), enif_make_int(env, n));
}

/* The C interns `fun_name` (`:3571-3573`) and stores it as the trampoline's mfa function
 * (`erl_nfunc_sched.c:131-134`); it never resolves it against the module's function table, so a name
 * that exists nowhere must still work. (The load-time table check at `:4874-4878` is about the
 * `nif_funcs` rows, not about this string.) */
static ERL_NIF_TERM nif_sched_ghost_name(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[])
{
    last_sched_ret = enif_schedule_nif(env, "no_such_nif_xyz", 0, cont_ghost, argc, argv);
    return last_sched_ret;
}

static ErlNifFunc nif_funcs[] = {
    {"sched_sum", 2, nif_sched_sum, 0},
    {"sched_loop", 1, nif_sched_loop, 0},
    {"sched_raise", 0, nif_sched_raise, 0},
    {"sched_bad_flags", 0, nif_sched_bad_flags, 0},
    {"sched_too_many_args", 0, nif_sched_too_many_args, 0},
    {"sched_dirty", 1, nif_sched_dirty, 0},
    {"sched_last_ret", 0, nif_sched_last_ret, 0},
    {"sched_thread_types", 1, nif_sched_ttype, 0},
    {"sched_ghost_name", 1, nif_sched_ghost_name, 0},
    {"sched_reargv", 1, nif_sched_reargv, 0},
    /* The continuations are entered by POINTER, but they still need a row here: `-on_load`'s
     * `erlang:load_nif/2` refuses any row that is not a real function of the module
     * (`erl_nif.c:4874-4878`), which is why `a3_schedule.erl` exports a stub for each of them. */
    {"cont_sum", 2, cont_sum, 0},
    {"cont_loop", 1, cont_loop, 0},
    {"cont_raise", 0, cont_raise, 0},
    {"cont_dirty", 1, cont_dirty, 0},
    {"cont_ttype", 1, cont_ttype, 0},
    {"cont_ghost", 1, cont_ghost, 0},
    {"cont_reargv", 1, cont_reargv, 0},
};

ERL_NIF_INIT(a3_schedule, nif_funcs, NULL, NULL, NULL, NULL)
