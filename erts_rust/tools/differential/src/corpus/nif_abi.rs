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

//! Contract **A.3** — the NIF C ABI, expanded: the `enif_*` *term* surface driven end to end
//! through a real C `.so` loaded by `erlang:load_nif/2`.
//!
//! [`super::nif_so`] pins the *call* path of a real NIF library (a load, an `add/2`, a
//! `badarg`, the `load_nif/2` failure shapes) and [`super::nif_res`] pins the resource /
//! IO-queue / sync / extra-environment families. This group widens the coverage to the bulk of
//! the C ABI a real NIF actually uses — the **get/make accessors**, the **type predicates**,
//! **comparison**, the **list / tuple / binary** constructors, the **map** family, the
//! **exception** slot and a few **process / time / allocation** slots — all exercised *not* as
//! a structural declaration but as a live call under both runtimes.
//!
//! # Shape: one library, one entry point
//!
//! Unlike `nif_so`/`nif_res` (one NIF function per case), this group compiles a **single** C
//! library whose only `ErlNifFunc` entry is `probe/1`. The embedded module loads it next to the
//! compiled `.beam` (the same self-locating trick as the other A.3 groups) and calls
//! `nif_abi:classify(N)`, which forwards the integer selector `N` to
//! `nif_abi:probe(N)` — an **external** call, so the Rust runtime intercepts it at external
//! dispatch (a bare local call would keep running the `erlang:nif_error/1` stub).
//!
//! The C `probe/1` `switch`es on `N` and each branch drives one `enif_*` behaviour, returning a
//! **plain integer** (`1` when the observed behaviour matched the C ABI contract, `0` when it
//! did not, `-1` for an unknown selector). Two selectors deliberately raise from C
//! (`enif_raise_exception` / `enif_make_badarg`); the module catches them and maps the class to
//! an integer, so *every* golden is a VM-stable integer line — never a value that embeds the
//! temp-dir path, a pid or a clock reading.
//!
//! # Why these selectors, and no others
//!
//! Both runtimes `dlopen(RTLD_NOW)` the **same** `.so`, so every `enif_*` the C references must
//! be a symbol the Rust runtime really defines; a missing one fails the *Rust* load while the
//! oracle loads it — a **mismatch**. Every symbol used here is exported by the release
//! `beam_rt` (verified against `nm -D`), and no C-variadic slot (`enif_make_tuple`,
//! `enif_make_list`, `enif_fprintf`, `enif_set_option`, …) is referenced — the non-variadic
//! `enif_make_*_from_array` / `enif_make_list_cell` forms stand in.
//!
//! The families whose host hook the runtime serves through `NifHost` (`beam_abi`, implemented by
//! `beam_rt`'s `NifHostImpl`) are covered end to end, because a *plain differing value* is a
//! mismatch and a mismatch is forbidden here — so every shape below is one both runtimes answer
//! alike:
//!
//! * pinned — `enif_make_existing_atom` / `_len` **refusing** a name no one holds (both answer
//!   `0` and leave `*atom` untouched, `atom.c:431`) **and** *finding* a name, whether interned
//!   earlier in the same call (the env's per-call atom record makes a freshly interned name
//!   visible to the lookup-only constructor, which is exactly the C's `erts_atom_get` answer) or
//!   held in the **committed** table (`enif_make_existing_atom(env, "ok", …)` resolves through the
//!   `existing_atom` host hook); `enif_get_atom` / `enif_get_atom_length` **reading back** a name
//!   first interned during the call; `enif_whereis_pid` on an unregistered atom and on a non-atom
//!   name (both `0`) **and** on a name a live process **is** registered under (`init`, registered
//!   in both — answered through the `whereis_pid` hook); `enif_binary_to_term` **refusing** data
//!   that is not a `VERSION_MAGIC`-prefixed encoding (both `0`) **and** decoding a **valid** image
//!   (the consumed byte count); `enif_term_to_binary` on a **valid** term (its canonical ETF
//!   image); `enif_make_ref` on a valid env (the `make_ref` hook mints a fresh reference — only its
//!   *ref-ness* is asserted, never the volatile id); `enif_clear_env` on a clean `enif_alloc_env`
//!   env leaving no pending exception (both `0`); the **properties** of `enif_make_unique_integer`
//!   (positive / monotonic / default-negative — the crate-internal counters are real, distinct and
//!   monotone per flag set); the frozen fields of `enif_system_info` (the driver/NIF version macros
//!   it was compiled with, `thread_support`, `smp_support`, the `"17.1"` erts version); and a name
//!   first interned *during* the call still being one atom with one identity.
//!
//! The five success paths (`enif_term_to_binary`, `enif_binary_to_term`, `enif_make_ref`,
//! `enif_make_existing_atom` on a committed name, `enif_whereis_pid` on a registered name) are
//! pinned as cases `nif_abi/term_to_binary_valid_term`, `nif_abi/binary_to_term_valid_encoding`,
//! `nif_abi/ref_make_ref_is_a_ref`, `nif_abi/atom_make_existing_atom_committed_hit` and
//! `nif_abi/process_whereis_pid_registered_hit`. The hooks live in `crates/beam_rt/src/nif.rs`
//! (`NifHostImpl`) over `crates/beam_abi/src/nif/`. `enif_system_info`'s `otp_release` /
//! `async_threads` / `scheduler_threads` / `dirty_scheduler_support` values, and
//! `enif_make_unique_integer`'s actual values, are documented to differ and are never asserted.
//!
//! The time family is exercised only through the deterministic `enif_convert_time_unit`
//! (`enif_monotonic_time`'s absolute value is documented to differ) and through the **shape** of
//! `enif_now_time` / `enif_cpu_time` (their `{MegaSecs,Secs,MicroSecs}` 3-tuple, whose values are
//! clock readings and are therefore never asserted).
//!
//! The **atom** read accessors are exercised over a name the runtime already holds (`ok`), over a
//! name first interned during the call (identity), and over the **lookup/read-back** side of that
//! same fresh name — `enif_make_existing_atom` / `_len` finding it, `enif_get_atom` /
//! `enif_get_atom_length` reading it — which resolves through the env's per-call atom record here
//! and through the global atom table in the oracle. The exception slot is driven on an
//! **`enif_alloc_env` env** because `enif_raise_exception` on a process env leaks into the caller;
//! `enif_clear_env` follows the same rule (on a clean allocated env it is a no-op in both — the
//! oracle leaves the pending flag set where the Rust runtime clears it, so the *raisable* shape is
//! the one that stays out).
//!
//! # The local-id / liveness / hash / size / width extensions
//!
//! A second, later block widens the same library with families the first pass did not reach; each
//! is pinned only through a shape both runtimes answer alike:
//!
//! * **local ids & liveness** — `enif_get_local_pid` resolving the pid from `enif_self` back to the
//!   same live process (and `enif_get_local_pid` / `enif_get_local_port` **refusing** a non-id),
//!   and `enif_is_current_process_alive` on the calling process (both `1`).
//! * **`enif_hash`** — only the `ERL_NIF_PHASH2` **properties** are pinned (the salt is ignored and
//!   the value stays below 2^27). `ERL_NIF_INTERNAL_HASH`'s *value* is implementation-defined and
//!   differs by design, so no case pins it.
//! * **`enif_term_size`** — a small integer occupies no heap words (`0`) and a 3-element proper
//!   list occupies six (`6`); the count is divided by `sizeof(ERL_NIF_TERM)` so the golden is
//!   word-size independent.
//! * **`enif_compare` / `enif_term_type` across the type lattice** — a number orders before an
//!   atom, and tuple / map / list report their own `ErlNifTermType`.
//! * **`enif_getenv`** — both the missing-key refusal (`-1`) and the oversized buffer that finds a
//!   key the environment carries (`0`).
//! * **`enif_consume_timeslice`** — charging a slice without forcing a yield (`0`).
//! * **`enif_is_identical` on floats / mixed numbers**, the **UTF-8** `enif_make_string` /
//!   `enif_get_string` / `enif_get_string_length` round trip (the length is the *encoded byte*
//!   count), and the **map** edges (`enif_get_map_size` on an empty map, `enif_make_map_update`
//!   refusing a missing key, a `ERL_NIF_MAP_ITERATOR_LAST` iterator walking back over its
//!   entries).
//!
//! The **atom-cache** slots are deliberately **not** pinned: the oracle answers a real
//! `enif_max_atom_cache_index` (`2038`) and a per-atom cache index (`399` for a fresh name), while
//! the Rust runtime exposes no cached indices (always `0` / a refusal) — a plain differing value,
//! i.e. a mismatch, not a corpus case. Owners: `crates/beam_abi/src/nif/enif/proc.rs`.
//!
//! # Skip policy (inherited from [`super::nif_so`])
//!
//! A C-compiled fixture needs a C compiler *and* the host `erl_nif.h`; when either is missing
//! the harness reports `Verdict::Skipped` with a loud reason
//! ([`crate::module_build::MISSING_CC_REASON`] / `MISSING_ERL_NIF_REASON`).

use super::{Case, CaseKind, Contract, Golden, ModuleRun};
use crate::module_build::NifFixture;

/// The embedded module: the library's single `ErlNifFunc` stub (`probe/1`, exported so the
/// oracle accepts the library) plus the `classify/1` driver.
///
/// `classify/1` loads the library next to this module's `.beam` and, on a successful load,
/// forwards the selector to `probe/1` through an **external** call. Only an integer leaves
/// `run/1`: a NIF raise is caught and mapped to a class code, and any non-integer return is
/// mapped to `90`, so the module's observable is always one integer line.
const NIF_ABI_MODULE_SOURCE: &str = r#"-module(nif_abi).
-export([classify/1, probe/1]).

%% The library's single ErlNifFunc entry: stubbed and exported so erlang:load_nif/2 accepts the
%% library, and dead code once the load succeeds (the real implementation is in the .so).
probe(_) -> erlang:nif_error(not_loaded).

classify(N) ->
    Dir = filename:dirname(code:which(?MODULE)),
    case erlang:load_nif(Dir ++ "/nif_abi", 0) of
        ok -> run(N);
        {error, {Reason, _Text}} -> reason_code(Reason)
    end.

%% Every branch answers an integer, so the golden is a VM-stable integer line. The two raising
%% selectors land in the explicit class clauses; the wildcard keeps a surprising failure a
%% diffable integer rather than an escaping raise.
run(N) ->
    try nif_abi:probe(N) of
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
const NIF_ABI_C: &str = r#"/* A real NIF exercising the enif_* C ABI end to end (contract A.3). */
#include <erl_nif.h>
#include <erl_driver.h>   /* ERlNifSysInfo is an ErlDrvSysInfo: its version macros live here */
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

/* The error handler `enif_dlopen`/`enif_dlsym` invoke; it only counts the calls, so the probe can
   assert "the handler ran exactly once" without depending on the message text (which embeds the
   path). */
static int err_calls = 0;
static void on_err(void* arg, const char* msg) {
    (void) arg;
    (void) msg;
    err_calls++;
}

static ERL_NIF_TERM probe(ErlNifEnv* env, int argc, const ERL_NIF_TERM argv[]) {
    int sel = 0;
    if (argc != 1 || !enif_get_int(env, argv[0], &sel))
        return enif_make_badarg(env);

    switch (sel) {
    /* ------------------------------------------------------------------ integers */
    case 1: {                                  /* get_make_int */
        ERL_NIF_TERM t = enif_make_int(env, -1234);
        int v = 0;
        return enif_make_int(env, (enif_get_int(env, t, &v) && v == -1234) ? 1 : 0);
    }
    case 2: {                                  /* get_make_uint */
        ERL_NIF_TERM t = enif_make_uint(env, 4000000000u);
        unsigned v = 0;
        return enif_make_int(env, (enif_get_uint(env, t, &v) && v == 4000000000u) ? 1 : 0);
    }
    case 3: {                                  /* get_make_long */
        ERL_NIF_TERM t = enif_make_long(env, -5000000000L);
        long v = 0;
        return enif_make_int(env, (enif_get_long(env, t, &v) && v == -5000000000L) ? 1 : 0);
    }
    case 4: {                                  /* get_make_ulong */
        ERL_NIF_TERM t = enif_make_ulong(env, 18000000000UL);
        unsigned long v = 0;
        return enif_make_int(env, (enif_get_ulong(env, t, &v) && v == 18000000000UL) ? 1 : 0);
    }
    case 5: {                                  /* get_make_int64 */
        ErlNifSInt64 want = -9000000000000LL;
        ERL_NIF_TERM t = enif_make_int64(env, want);
        ErlNifSInt64 v = 0;
        return enif_make_int(env, (enif_get_int64(env, t, &v) && v == want) ? 1 : 0);
    }
    case 6: {                                  /* get_make_uint64 */
        ErlNifUInt64 want = 18000000000000000000ULL;
        ERL_NIF_TERM t = enif_make_uint64(env, want);
        ErlNifUInt64 v = 0;
        return enif_make_int(env, (enif_get_uint64(env, t, &v) && v == want) ? 1 : 0);
    }
    case 7: {                                  /* get_int rejects an out-of-range value */
        ERL_NIF_TERM t = enif_make_int64(env, 1099511627776LL);   /* 1 << 40 */
        int v = 0;
        return enif_make_int(env, enif_get_int(env, t, &v) ? 0 : 1);
    }
    case 8: {                                  /* get_uint rejects a negative value */
        ERL_NIF_TERM t = enif_make_int(env, -1);
        unsigned v = 0;
        return enif_make_int(env, enif_get_uint(env, t, &v) ? 0 : 1);
    }
    case 9: {                                  /* get_int rejects a non-number */
        ERL_NIF_TERM t = enif_make_atom(env, "not_a_number");
        int v = 0;
        return enif_make_int(env, enif_get_int(env, t, &v) ? 0 : 1);
    }
    case 10: {                                 /* a wide int64 survives the round trip */
        ErlNifSInt64 want = 1099511627776LL;   /* 1 << 40 */
        ERL_NIF_TERM t = enif_make_int64(env, want);
        ErlNifSInt64 v = 0;
        return enif_make_int(env, (enif_get_int64(env, t, &v) && v == want) ? 1 : 0);
    }

    /* -------------------------------------------------------------------- floats */
    case 11: {                                 /* get_make_double */
        ERL_NIF_TERM t = enif_make_double(env, 3.5);
        double v = 0.0;
        return enif_make_int(env, (enif_get_double(env, t, &v) && v == 3.5) ? 1 : 0);
    }
    case 12: {                                 /* get_make_double, negative */
        ERL_NIF_TERM t = enif_make_double(env, -2.25);
        double v = 0.0;
        return enif_make_int(env, (enif_get_double(env, t, &v) && v == -2.25) ? 1 : 0);
    }
    case 13: {                                 /* get_double rejects a non-number */
        ERL_NIF_TERM t = enif_make_atom(env, "not_a_float");
        double v = 0.0;
        return enif_make_int(env, enif_get_double(env, t, &v) ? 0 : 1);
    }
    case 14: {                                 /* get_double on an integer */
        ERL_NIF_TERM t = enif_make_int(env, 7);
        double v = 0.0;
        return enif_make_int(env, enif_get_double(env, t, &v));
    }

    /* --------------------------------------------------------------------- atoms
       The name used is one the runtime already holds (`ok`): a name it interns for the FIRST
       time during this call is not yet resolvable by enif_get_atom in the Rust runtime, whose
       host atom_name consults only the committed atom table (see the module docs). */
    case 15: {                                 /* make_get_atom */
        const char* name = "ok";
        ERL_NIF_TERM t = enif_make_atom(env, name);
        char buf[16];
        int r = enif_get_atom(env, t, buf, sizeof(buf), ERL_NIF_LATIN1);
        return enif_make_int(env,
            (r == (int) strlen(name) + 1 && strcmp(buf, name) == 0) ? 1 : 0);
    }
    case 16: {                                 /* make_atom_len honours the length */
        ERL_NIF_TERM t = enif_make_atom_len(env, "okX", 2);
        char buf[16];
        int r = enif_get_atom(env, t, buf, sizeof(buf), ERL_NIF_LATIN1);
        return enif_make_int(env, (r == 3 && strcmp(buf, "ok") == 0) ? 1 : 0);
    }
    case 17: {                                 /* get_atom_length */
        ERL_NIF_TERM t = enif_make_atom(env, "ok");
        unsigned n = 0;
        return enif_make_int(env,
            (enif_get_atom_length(env, t, &n, ERL_NIF_LATIN1) && n == 2) ? 1 : 0);
    }
    case 18: {                                 /* make_new_atom */
        const char* name = "ok";
        ERL_NIF_TERM t = 0;
        if (!enif_make_new_atom(env, name, &t, ERL_NIF_LATIN1))
            return enif_make_int(env, 0);
        char buf[16];
        int r = enif_get_atom(env, t, buf, sizeof(buf), ERL_NIF_LATIN1);
        return enif_make_int(env,
            (r == (int) strlen(name) + 1 && strcmp(buf, name) == 0) ? 1 : 0);
    }
    case 19: {                                 /* make_new_atom_len */
        ERL_NIF_TERM t = 0;
        if (!enif_make_new_atom_len(env, "okYY", 2, &t, ERL_NIF_LATIN1))
            return enif_make_int(env, 0);
        char buf[16];
        int r = enif_get_atom(env, t, buf, sizeof(buf), ERL_NIF_LATIN1);
        return enif_make_int(env, (r == 3 && strcmp(buf, "ok") == 0) ? 1 : 0);
    }

    /* ------------------------------------------------------------------ strings */
    case 20: {                                 /* make_get_string */
        ERL_NIF_TERM t = enif_make_string(env, "hello", ERL_NIF_LATIN1);
        char buf[16];
        int r = enif_get_string(env, t, buf, sizeof(buf), ERL_NIF_LATIN1);
        return enif_make_int(env, (r == 6 && strcmp(buf, "hello") == 0) ? 1 : 0);
    }
    case 21: {                                 /* make_string_len */
        ERL_NIF_TERM t = enif_make_string_len(env, "helloX", 5, ERL_NIF_LATIN1);
        char buf[16];
        int r = enif_get_string(env, t, buf, sizeof(buf), ERL_NIF_LATIN1);
        return enif_make_int(env, (r == 6 && strcmp(buf, "hello") == 0) ? 1 : 0);
    }
    case 22: {                                 /* get_string_length */
        ERL_NIF_TERM t = enif_make_string(env, "abcdef", ERL_NIF_LATIN1);
        unsigned n = 0;
        return enif_make_int(env,
            (enif_get_string_length(env, t, &n, ERL_NIF_LATIN1) && n == 6) ? 1 : 0);
    }
    case 23: {                                 /* a short buffer yields the C's negative length */
        ERL_NIF_TERM t = enif_make_string(env, "hello", ERL_NIF_LATIN1);
        char buf[3];
        int r = enif_get_string(env, t, buf, sizeof(buf), ERL_NIF_LATIN1);
        return enif_make_int(env, (r == -3 && buf[2] == '\0') ? 1 : 0);
    }

    /* --------------------------------------------------------------------- lists */
    case 24: {                                 /* make_list_from_array */
        ERL_NIF_TERM arr[3];
        unsigned n = 0;
        arr[0] = enif_make_int(env, 1);
        arr[1] = enif_make_int(env, 2);
        arr[2] = enif_make_int(env, 3);
        return enif_make_int(env,
            (enif_get_list_length(env, enif_make_list_from_array(env, arr, 3), &n) && n == 3)
                ? 1 : 0);
    }
    case 25: {                                 /* make_list_cell builds [1,2] */
        ERL_NIF_TERM none[1] = {0};
        ERL_NIF_TERM nil = enif_make_list_from_array(env, none, 0);
        ERL_NIF_TERM l2 = enif_make_list_cell(env, enif_make_int(env, 2), nil);
        ERL_NIF_TERM l1 = enif_make_list_cell(env, enif_make_int(env, 1), l2);
        unsigned n = 0;
        return enif_make_int(env,
            (enif_get_list_length(env, l1, &n) && n == 2) ? 1 : 0);
    }
    case 26: {                                 /* get_list_cell head of [7,8] */
        ERL_NIF_TERM arr[2];
        ERL_NIF_TERM head = 0, tail = 0;
        int v = 0;
        arr[0] = enif_make_int(env, 7);
        arr[1] = enif_make_int(env, 8);
        if (!enif_get_list_cell(env, enif_make_list_from_array(env, arr, 2), &head, &tail))
            return enif_make_int(env, 0);
        return enif_make_int(env, enif_get_int(env, head, &v) ? v : 0);
    }
    case 27: {                                 /* make_reverse_list head of [1,2,3] */
        ERL_NIF_TERM arr[3];
        ERL_NIF_TERM rev = 0, head = 0, tail = 0;
        int v = 0;
        arr[0] = enif_make_int(env, 1);
        arr[1] = enif_make_int(env, 2);
        arr[2] = enif_make_int(env, 3);
        if (!enif_make_reverse_list(env, enif_make_list_from_array(env, arr, 3), &rev))
            return enif_make_int(env, 0);
        if (!enif_get_list_cell(env, rev, &head, &tail))
            return enif_make_int(env, 0);
        return enif_make_int(env, enif_get_int(env, head, &v) ? v : 0);
    }
    case 28: {                                 /* get_list_length rejects a non-list */
        ERL_NIF_TERM t = enif_make_int(env, 42);
        unsigned n = 0;
        return enif_make_int(env, enif_get_list_length(env, t, &n) ? 0 : 1);
    }

    /* -------------------------------------------------------------------- tuples */
    case 29: {                                 /* make_tuple_from_array */
        ERL_NIF_TERM arr[4];
        int arity = -1;
        const ERL_NIF_TERM* got = NULL;
        int v3 = 0;
        arr[0] = enif_make_int(env, 1);
        arr[1] = enif_make_int(env, 2);
        arr[2] = enif_make_int(env, 3);
        arr[3] = enif_make_int(env, 4);
        if (!enif_get_tuple(env, enif_make_tuple_from_array(env, arr, 4), &arity, &got))
            return enif_make_int(env, 0);
        if (!enif_get_int(env, got[3], &v3))
            return enif_make_int(env, 0);
        return enif_make_int(env, (arity == 4 && v3 == 4) ? 1 : 0);
    }
    case 30: {                                 /* get_tuple arity of {1,2,3,4} */
        ERL_NIF_TERM arr[4];
        int arity = -1;
        const ERL_NIF_TERM* got = NULL;
        arr[0] = enif_make_int(env, 1);
        arr[1] = enif_make_int(env, 2);
        arr[2] = enif_make_int(env, 3);
        arr[3] = enif_make_int(env, 4);
        if (!enif_get_tuple(env, enif_make_tuple_from_array(env, arr, 4), &arity, &got))
            return enif_make_int(env, 0);
        return enif_make_int(env, arity);
    }
    case 31: {                                 /* get_tuple rejects a non-tuple */
        int arity = -1;
        const ERL_NIF_TERM* got = NULL;
        ERL_NIF_TERM t = enif_make_int(env, 42);
        return enif_make_int(env, enif_get_tuple(env, t, &arity, &got) ? 0 : 1);
    }

    /* ----------------------------------------------------------------- binaries */
    case 32: {                                 /* alloc_binary -> make_binary -> inspect */
        ErlNifBinary b;
        ErlNifBinary v;
        ERL_NIF_TERM t;
        if (!enif_alloc_binary(4, &b))
            return enif_make_int(env, 0);
        memcpy(b.data, "abcd", 4);
        t = enif_make_binary(env, &b);          /* consumes b */
        if (!enif_inspect_binary(env, t, &v))
            return enif_make_int(env, 0);
        return enif_make_int(env,
            (v.size == 4 && memcmp(v.data, "abcd", 4) == 0) ? 1 : 0);
    }
    case 33: {                                 /* make_new_binary writes in place */
        ERL_NIF_TERM t;
        ErlNifBinary v;
        unsigned char* p = enif_make_new_binary(env, 3, &t);
        if (p == NULL)
            return enif_make_int(env, 0);
        memcpy(p, "xyz", 3);
        if (!enif_inspect_binary(env, t, &v))
            return enif_make_int(env, 0);
        return enif_make_int(env,
            (v.size == 3 && memcmp(v.data, "xyz", 3) == 0) ? 1 : 0);
    }
    case 34: {                                 /* make_sub_binary */
        ERL_NIF_TERM s = enif_make_sub_binary(env, make_bytes(env, "abcdef", 6), 1, 3);
        ErlNifBinary v;
        if (!enif_inspect_binary(env, s, &v))
            return enif_make_int(env, 0);
        return enif_make_int(env,
            (v.size == 3 && memcmp(v.data, "bcd", 3) == 0) ? 1 : 0);
    }
    case 35: {                                 /* inspect_binary rejects a non-binary */
        ErlNifBinary v;
        ERL_NIF_TERM t = enif_make_int(env, 42);
        return enif_make_int(env, enif_inspect_binary(env, t, &v) ? 0 : 1);
    }
    case 36: {                                 /* inspect_iolist_as_binary flattens */
        ERL_NIF_TERM none[1] = {0};
        ERL_NIF_TERM nil = enif_make_list_from_array(env, none, 0);
        ERL_NIF_TERM e = enif_make_int(env, (int) 'e');
        ERL_NIF_TERM l2 = enif_make_list_cell(env, e, nil);
        ERL_NIF_TERM l1 = enif_make_list_cell(env, make_bytes(env, "cd", 2), l2);
        ERL_NIF_TERM l0 = enif_make_list_cell(env, make_bytes(env, "ab", 2), l1);
        ErlNifBinary v;
        int ok;
        if (!enif_inspect_iolist_as_binary(env, l0, &v))
            return enif_make_int(env, 0);
        ok = (v.size == 5 && memcmp(v.data, "abcde", 5) == 0);
        enif_release_binary(&v);
        return enif_make_int(env, ok ? 1 : 0);
    }
    case 37: {                                 /* realloc_binary preserves the prefix */
        ErlNifBinary b;
        int ok;
        if (!enif_alloc_binary(2, &b))
            return enif_make_int(env, 0);
        memcpy(b.data, "hi", 2);
        if (!enif_realloc_binary(&b, 4)) {
            enif_release_binary(&b);
            return enif_make_int(env, 0);
        }
        memcpy(b.data + 2, "!!", 2);
        ok = (b.size == 4 && memcmp(b.data, "hi!!", 4) == 0);
        enif_release_binary(&b);
        return enif_make_int(env, ok ? 1 : 0);
    }

    /* --------------------------------------------------------------- predicates */
    case 38: {                                 /* is_atom */
        int a = enif_is_atom(env, enif_make_atom(env, "an_atom"));
        int b = enif_is_atom(env, enif_make_int(env, 1));
        return enif_make_int(env, (a && !b) ? 1 : 0);
    }
    case 39: {                                 /* is_number */
        int a = enif_is_number(env, enif_make_int(env, 1));
        int b = enif_is_number(env, enif_make_double(env, 1.5));
        int c = enif_is_number(env, enif_make_atom(env, "not_a_number"));
        return enif_make_int(env, (a && b && !c) ? 1 : 0);
    }
    case 40: {                                 /* is_list on a non-empty list / non-list */
        ERL_NIF_TERM arr[2];
        int a, b;
        arr[0] = enif_make_int(env, 1);
        arr[1] = enif_make_int(env, 2);
        a = enif_is_list(env, enif_make_list_from_array(env, arr, 2));
        b = enif_is_list(env, enif_make_atom(env, "not_a_list"));
        return enif_make_int(env, (a && !b) ? 1 : 0);
    }
    case 41: {                                 /* is_empty_list */
        ERL_NIF_TERM none[1] = {0};
        int a = enif_is_empty_list(env, enif_make_list_from_array(env, none, 0));
        int b = enif_is_empty_list(env, enif_make_atom(env, "not_empty"));
        return enif_make_int(env, (a && !b) ? 1 : 0);
    }
    case 42: {                                 /* is_map */
        int a = enif_is_map(env, enif_make_new_map(env));
        int b = enif_is_map(env, enif_make_int(env, 1));
        return enif_make_int(env, (a && !b) ? 1 : 0);
    }
    case 43: {                                 /* is_tuple */
        ERL_NIF_TERM arr[1];
        int a, b;
        arr[0] = enif_make_int(env, 1);
        a = enif_is_tuple(env, enif_make_tuple_from_array(env, arr, 1));
        b = enif_is_tuple(env, enif_make_int(env, 1));
        return enif_make_int(env, (a && !b) ? 1 : 0);
    }
    case 44: {                                 /* is_binary */
        int a = enif_is_binary(env, make_bytes(env, "ab", 2));
        int b = enif_is_binary(env, enif_make_int(env, 1));
        return enif_make_int(env, (a && !b) ? 1 : 0);
    }
    case 45: {                                 /* is_pid is false for a non-pid */
        return enif_make_int(env, enif_is_pid(env, enif_make_int(env, 1)) ? 0 : 1);
    }
    case 46: {                                 /* is_ref is false for a non-ref */
        return enif_make_int(env, enif_is_ref(env, enif_make_int(env, 1)) ? 0 : 1);
    }
    case 47: {                                 /* is_port is false for a non-port */
        return enif_make_int(env, enif_is_port(env, enif_make_int(env, 1)) ? 0 : 1);
    }
    case 48: {                                 /* is_fun is false for a non-fun */
        return enif_make_int(env, enif_is_fun(env, enif_make_int(env, 1)) ? 0 : 1);
    }
    case 49: {                                 /* is_identical */
        int same = enif_is_identical(enif_make_int(env, 5), enif_make_int(env, 5));
        int diff = enif_is_identical(enif_make_int(env, 5), enif_make_int(env, 6));
        return enif_make_int(env, (same && !diff) ? 1 : 0);
    }
    case 50: {                                 /* compare ordering of integers */
        int lt = enif_compare(enif_make_int(env, 1), enif_make_int(env, 2));
        int eq = enif_compare(enif_make_int(env, 2), enif_make_int(env, 2));
        int gt = enif_compare(enif_make_int(env, 3), enif_make_int(env, 2));
        return enif_make_int(env, (lt == -1 && eq == 0 && gt == 1) ? 1 : 0);
    }
    case 51: {                                 /* term_type */
        int i = enif_term_type(env, enif_make_int(env, 1)) == ERL_NIF_TERM_TYPE_INTEGER;
        int a = enif_term_type(env, enif_make_atom(env, "an_atom")) == ERL_NIF_TERM_TYPE_ATOM;
        int l = enif_term_type(env, make_bytes(env, "ab", 2)) == ERL_NIF_TERM_TYPE_BITSTRING;
        return enif_make_int(env, (i && a && l) ? 1 : 0);
    }

    /* ---------------------------------------------------------------------- maps */
    case 52: {                                 /* make_map_put then get_map_value */
        ERL_NIF_TERM m1, m2, v = 0;
        int got = 0;
        ERL_NIF_TERM empty = enif_make_new_map(env);
        if (!enif_make_map_put(env, empty, enif_make_atom(env, "a"), enif_make_int(env, 1), &m1))
            return enif_make_int(env, 0);
        if (!enif_make_map_put(env, m1, enif_make_atom(env, "b"), enif_make_int(env, 2), &m2))
            return enif_make_int(env, 0);
        if (!enif_get_map_value(env, m2, enif_make_atom(env, "a"), &v))
            return enif_make_int(env, 0);
        return enif_make_int(env, (enif_get_int(env, v, &got) && got == 1) ? 1 : 0);
    }
    case 53: {                                 /* make_map_update */
        ERL_NIF_TERM m1, m2, v = 0;
        int got = 0;
        ERL_NIF_TERM empty = enif_make_new_map(env);
        if (!enif_make_map_put(env, empty, enif_make_atom(env, "k"), enif_make_int(env, 1), &m1))
            return enif_make_int(env, 0);
        if (!enif_make_map_update(env, m1, enif_make_atom(env, "k"), enif_make_int(env, 9), &m2))
            return enif_make_int(env, 0);
        if (!enif_get_map_value(env, m2, enif_make_atom(env, "k"), &v))
            return enif_make_int(env, 0);
        return enif_make_int(env, (enif_get_int(env, v, &got) && got == 9) ? 1 : 0);
    }
    case 54: {                                 /* make_map_remove */
        ERL_NIF_TERM m1, m2, v = 0;
        size_t sz = 99;
        ERL_NIF_TERM empty = enif_make_new_map(env);
        if (!enif_make_map_put(env, empty, enif_make_atom(env, "k"), enif_make_int(env, 1), &m1))
            return enif_make_int(env, 0);
        if (!enif_make_map_remove(env, m1, enif_make_atom(env, "k"), &m2))
            return enif_make_int(env, 0);
        if (!enif_get_map_size(env, m2, &sz))
            return enif_make_int(env, 0);
        return enif_make_int(env, (sz == 0 && !enif_get_map_value(env, m2, enif_make_atom(env, "k"), &v))
                                    ? 1 : 0);
    }
    case 55: {                                 /* get_map_size */
        ERL_NIF_TERM m1, m2;
        size_t sz = 0;
        ERL_NIF_TERM empty = enif_make_new_map(env);
        if (!enif_make_map_put(env, empty, enif_make_atom(env, "a"), enif_make_int(env, 1), &m1))
            return enif_make_int(env, 0);
        if (!enif_make_map_put(env, m1, enif_make_atom(env, "b"), enif_make_int(env, 2), &m2))
            return enif_make_int(env, 0);
        if (!enif_get_map_size(env, m2, &sz))
            return enif_make_int(env, 0);
        return enif_make_int(env, (int) sz);
    }
    case 56: {                                 /* make_map_from_arrays */
        ERL_NIF_TERM keys[2], vals[2], m;
        ERL_NIF_TERM got = 0;
        int v = 0;
        keys[0] = enif_make_atom(env, "a");
        keys[1] = enif_make_atom(env, "b");
        vals[0] = enif_make_int(env, 10);
        vals[1] = enif_make_int(env, 20);
        if (!enif_make_map_from_arrays(env, keys, vals, 2, &m))
            return enif_make_int(env, 0);
        if (!enif_get_map_value(env, m, keys[1], &got))
            return enif_make_int(env, 0);
        return enif_make_int(env, (enif_get_int(env, got, &v) && v == 20) ? 1 : 0);
    }
    case 57: {                                 /* map_iterator sums the values */
        ERL_NIF_TERM m1, m2, m3, k = 0, v = 0;
        int sum = 0;
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
            int vi = 0;
            if (enif_get_int(env, v, &vi))
                sum += vi;
            enif_map_iterator_next(env, &it);
        }
        enif_map_iterator_destroy(env, &it);
        return enif_make_int(env, sum);
    }

    /* ---------------------------------------------------------------- exceptions */
    case 58: {                                 /* enif_raise_exception reaches Erlang */
        return enif_raise_exception(env, enif_make_atom(env, "boom"));
    }
    case 59: {                                 /* enif_make_badarg reaches Erlang */
        return enif_make_badarg(env);
    }
    case 60: {                                 /* the pending-exception slot, on an allocated env */
        /* enif_raise_exception is only meaningful on a fresh env here: clearing/PENDING on the
           PROCESS env leaks into the caller, so the slot is driven on an enif_alloc_env env.
           Only the raise->has_pending_exception->identical part is asserted; enif_clear_env is
           omitted because the oracle leaves the flag set where the Rust runtime clears it (a
           divergence, see the module docs). */
        ErlNifEnv* e2 = enif_alloc_env();
        ERL_NIF_TERM boom, reason = 0;
        int pending, is_boom;
        if (e2 == NULL)
            return enif_make_int(env, 0);
        boom = enif_make_atom(e2, "boom");
        (void) enif_raise_exception(e2, boom);
        pending = enif_has_pending_exception(e2, &reason);
        is_boom = enif_is_identical(reason, boom);
        enif_free_env(e2);
        return enif_make_int(env, (pending == 1 && is_boom) ? 1 : 0);
    }

    /* -------------------------------------------------------- process / time / mem */
    case 61: {                                 /* enif_self is a pid */
        ErlNifPid pid;
        if (enif_self(env, &pid) == NULL)
            return enif_make_int(env, 0);
        return enif_make_int(env, enif_is_pid(env, enif_make_pid(env, &pid)) ? 1 : 0);
    }
    case 62: {                                 /* enif_self is alive */
        ErlNifPid pid;
        if (enif_self(env, &pid) == NULL)
            return enif_make_int(env, 0);
        return enif_make_int(env, enif_is_process_alive(env, &pid) ? 1 : 0);
    }
    case 63: {                                 /* pid-undefined round trip */
        ErlNifPid pid;
        int undefined_first, defined_second;
        enif_set_pid_undefined(&pid);
        undefined_first = enif_is_pid_undefined(&pid);
        if (enif_self(env, &pid) == NULL)
            return enif_make_int(env, 0);
        defined_second = enif_is_pid_undefined(&pid);
        return enif_make_int(env, (undefined_first == 1 && defined_second == 0) ? 1 : 0);
    }
    case 64: {                                 /* alloc / realloc / free */
        void* p = enif_alloc(64);
        void* q;
        int ok;
        if (p == NULL)
            return enif_make_int(env, 0);
        memset(p, 0x5a, 64);
        q = enif_realloc(p, 128);
        if (q == NULL) {
            enif_free(p);
            return enif_make_int(env, 0);
        }
        ok = (((unsigned char*) q)[0] == 0x5a);
        enif_free(q);
        return enif_make_int(env, ok ? 1 : 0);
    }
    case 65: {                                 /* enif_priv_data is the NULL load set */
        return enif_make_int(env, enif_priv_data(env) == NULL ? 1 : 0);
    }
    case 66: {                                 /* convert_time_unit ns -> ms */
        ErlNifTime ms = enif_convert_time_unit((ErlNifTime) 1000000000,
                                               ERL_NIF_NSEC, ERL_NIF_MSEC);
        return enif_make_int(env, ms == 1000 ? 1 : 0);
    }
    case 67: {                                 /* now_time is a {MegaSecs,Secs,MicroSecs} 3-tuple */
        int arity = -1;
        const ERL_NIF_TERM* got = NULL;
        if (!enif_get_tuple(env, enif_now_time(env), &arity, &got))
            return enif_make_int(env, 0);
        return enif_make_int(env, arity == 3 ? 1 : 0);
    }
    case 68: {                                 /* cpu_time is a {MegaSecs,Secs,MicroSecs} 3-tuple */
        int arity = -1;
        const ERL_NIF_TERM* got = NULL;
        if (!enif_get_tuple(env, enif_cpu_time(env), &arity, &got))
            return enif_make_int(env, 0);
        return enif_make_int(env, arity == 3 ? 1 : 0);
    }
    case 69: {                                 /* is_exception is false with no raise */
        return enif_make_int(env, enif_is_exception(env, enif_make_int(env, 7)) ? 0 : 1);
    }
    case 70: {                                 /* the empty tuple / empty list edge */
        ERL_NIF_TERM none[1] = {0};
        ERL_NIF_TERM tup = enif_make_tuple_from_array(env, none, 0);
        ERL_NIF_TERM lst = enif_make_list_from_array(env, none, 0);
        int arity = -1;
        const ERL_NIF_TERM* got = NULL;
        if (!enif_get_tuple(env, tup, &arity, &got))
            return enif_make_int(env, 0);
        return enif_make_int(env, (arity == 0 && enif_is_empty_list(env, lst)) ? 1 : 0);
    }

    /* ------------------------------------------- the host-hooked slots, shape by shape
    /* ------------------------------------------- the host-hooked slots, shape by shape
       The families below are the ones served through a `NifHost` hook. Each branch pins **one**
       shape whose answer is the same under both runtimes a real C ABI contract fixes: a miss both
       refuse, a malformed input both refuse, a clean env both leave clean, or a *property* of a
       value the crate-internal implementation really produces. The now-implemented success paths
       are pinned too (selectors 83/84, 108/109/110/111/112); a shape that would answer differently
       is not in this group (see the module docs). */
    case 71: {                                 /* make_existing_atom refuses an unheld name */
        ERL_NIF_TERM t = 0;
        int r = enif_make_existing_atom(env, "nif_abi_no_such_atom_zzz", &t, ERL_NIF_LATIN1);
        /* Lookup-only: an unheld name is refused and `*atom` is left untouched (atom.c:431). */
        return enif_make_int(env, (r == 0 && t == 0) ? 1 : 0);
    }
    case 72: {                                 /* make_existing_atom_len, same miss */
        const char* name = "nif_abi_no_such_atom_len_zzz";
        ERL_NIF_TERM t = 0;
        int r = enif_make_existing_atom_len(env, name, strlen(name) - 1, &t, ERL_NIF_LATIN1);
        return enif_make_int(env, (r == 0 && t == 0) ? 1 : 0);
    }
    case 73: {                                 /* whereis_pid on an unregistered atom */
        ErlNifPid pid;
        int r;
        memset(&pid, 0, sizeof(pid));
        r = enif_whereis_pid(env, enif_make_atom(env, "nif_abi_no_such_registered_name"), &pid);
        return enif_make_int(env, r == 0 ? 1 : 0);
    }
    case 74: {                                 /* whereis_pid refuses a non-atom name */
        ErlNifPid pid;
        int r;
        memset(&pid, 0, sizeof(pid));
        r = enif_whereis_pid(env, enif_make_int(env, 1), &pid);
        return enif_make_int(env, r == 0 ? 1 : 0);
    }

    /* Unique integers: the crate-internal counters are real, distinct and monotone per flag set,
       so the *properties* (not the values) are the pinnable contract. */
    case 75: {                                 /* unique_integer([positive]) is positive */
        ErlNifSInt64 v = 0;
        ERL_NIF_TERM t = enif_make_unique_integer(env, ERL_NIF_UNIQUE_POSITIVE);
        return enif_make_int(env, (enif_get_int64(env, t, &v) && v > 0) ? 1 : 0);
    }
    case 76: {                                 /* unique_integer([monotonic]) increases */
        ErlNifSInt64 a = 0, b = 0;
        ERL_NIF_TERM t1 = enif_make_unique_integer(env, ERL_NIF_UNIQUE_MONOTONIC);
        ERL_NIF_TERM t2 = enif_make_unique_integer(env, ERL_NIF_UNIQUE_MONOTONIC);
        if (!enif_get_int64(env, t1, &a) || !enif_get_int64(env, t2, &b))
            return enif_make_int(env, 0);
        return enif_make_int(env, b > a ? 1 : 0);
    }
    case 77: {                                 /* unique_integer() defaults to negative */
        ErlNifSInt64 v = 0;
        ERL_NIF_TERM t = enif_make_unique_integer(env, (ErlNifUniqueInteger) 0);
        /* The default counter is the "non-positive" one: its values are negative in both. */
        return enif_make_int(env, (enif_get_int64(env, t, &v) && v < 0) ? 1 : 0);
    }
    case 78: {                                 /* unique_integer([positive,monotonic]) is positive */
        ErlNifSInt64 v = 0;
        ErlNifUniqueInteger flags =
            (ErlNifUniqueInteger) (ERL_NIF_UNIQUE_POSITIVE | ERL_NIF_UNIQUE_MONOTONIC);
        ERL_NIF_TERM t = enif_make_unique_integer(env, flags);
        return enif_make_int(env, (enif_get_int64(env, t, &v) && v > 0) ? 1 : 0);
    }

    /* System information: the *frozen* ABI fields, compared against the very macros the C was
       compiled with (so the comparison is header-relative, not a hard-coded number). */
    case 79: {                                 /* system_info fills the frozen ABI fields */
        ErlNifSysInfo si;
        memset(&si, 0, sizeof(si));
        enif_system_info(&si, sizeof(si));
        return enif_make_int(env,
            (si.driver_major_version == ERL_DRV_EXTENDED_MAJOR_VERSION &&
             si.driver_minor_version == ERL_DRV_EXTENDED_MINOR_VERSION &&
             si.nif_major_version == ERL_NIF_MAJOR_VERSION &&
             si.nif_minor_version == ERL_NIF_MINOR_VERSION &&
             si.thread_support == 1 && si.smp_support == 1 &&
             si.erts_version != NULL && strcmp(si.erts_version, "17.1") == 0) ? 1 : 0);
    }

    /* Term <-> binary: both the *refusal* shape (below) and the *success* paths (selectors 108 /
       109, added with the term_to_binary/binary_to_term host hooks) are pinned. */
    case 80: {                                 /* binary_to_term refuses a malformed encoding */
        static const unsigned char bad[1] = {0};   /* not VERSION_MAGIC (131) */
        ERL_NIF_TERM t = 0;
        size_t n = enif_binary_to_term(env, bad, sizeof(bad), &t, 0);
        return enif_make_int(env, (n == 0 && t == 0) ? 1 : 0);
    }
    case 81: {                                 /* clear_env leaves a clean env clean */
        ErlNifEnv* e2 = enif_alloc_env();
        ERL_NIF_TERM reason = 0;
        int pending;
        if (e2 == NULL)
            return enif_make_int(env, 0);
        enif_clear_env(e2);
        pending = enif_has_pending_exception(e2, &reason);
        enif_free_env(e2);
        return enif_make_int(env, pending == 0 ? 1 : 0);
    }
    case 82: {                                 /* a fresh name is still one atom, same identity */
        const char* name = "nif_abi_fresh_atom_identity";
        ERL_NIF_TERM a = enif_make_atom(env, name);
        ERL_NIF_TERM b = enif_make_atom(env, name);
        return enif_make_int(env, (enif_is_atom(env, a) && enif_is_identical(a, b)) ? 1 : 0);
    }

    /* ------------------------------------------- the atom lookups: the success shapes
       The lookup/read side of the family above: a lookup that hits (through the env's per-call
       atom record here, through the global atom table in the oracle) and a name interned during
       the call staying readable. Each branch pins one integer the two runtimes answer alike. */
    case 83: {                                 /* make_existing_atom hits a name held in this call */
        const char* name = "nif_abi_held_atom";
        ERL_NIF_TERM made = enif_make_atom(env, name);
        ERL_NIF_TERM found = 0;
        int r = enif_make_existing_atom(env, name, &found, ERL_NIF_LATIN1);
        return enif_make_int(env, (r == 1 && enif_is_identical(made, found)) ? 1 : 0);
    }
    case 84: {                                 /* make_existing_atom_len hits, under its length */
        const char* buf = "nif_abi_held_atom_lenXX";
        size_t n = strlen(buf) - 2;            /* the trailing "XX" is not part of the name */
        ERL_NIF_TERM made = enif_make_atom_len(env, buf, n);
        ERL_NIF_TERM found = 0;
        char got[64];
        int r = enif_make_existing_atom_len(env, buf, n, &found, ERL_NIF_LATIN1);
        int len = enif_get_atom(env, found, got, sizeof(got), ERL_NIF_LATIN1);
        /* The explicit length is honoured on both sides: the atom is the truncated name, not the
           whole buffer (`got` is only read once `enif_get_atom` reported success). */
        return enif_make_int(env,
            (r == 1 && enif_is_identical(made, found) && len == (int) n + 1 &&
             strncmp(got, "nif_abi_held_atom_len", n) == 0) ? 1 : 0);
    }
    case 85: {                                 /* get_atom reads a name interned in this call */
        const char* name = "nif_abi_fresh_get_atom";
        ERL_NIF_TERM t = enif_make_atom(env, name);
        char buf[64];
        int r = enif_get_atom(env, t, buf, sizeof(buf), ERL_NIF_LATIN1);
        return enif_make_int(env,
            (r == (int) strlen(name) + 1 && strcmp(buf, name) == 0) ? 1 : 0);
    }
    case 86: {                                 /* get_atom_length counts that same fresh name */
        const char* name = "nif_abi_fresh_atom_length";
        ERL_NIF_TERM t = enif_make_atom(env, name);
        unsigned n = 0;
        int r = enif_get_atom_length(env, t, &n, ERL_NIF_LATIN1);
        return enif_make_int(env, (r == 1 && n == (unsigned) strlen(name)) ? 1 : 0);
    }

    /* ------------------------------------------- the local-id / liveness / hash family */
    case 87: {                                 /* get_local_pid round trips a self pid */
        ErlNifPid pid, back;
        ERL_NIF_TERM t;
        if (enif_self(env, &pid) == NULL)
            return enif_make_int(env, 0);
        t = enif_make_pid(env, &pid);
        if (!enif_get_local_pid(env, t, &back))
            return enif_make_int(env, 0);
        return enif_make_int(env,
            (enif_is_identical(t, enif_make_pid(env, &back)) &&
             enif_is_process_alive(env, &back)) ? 1 : 0);
    }
    case 88: {                                 /* get_local_pid refuses a non-pid */
        ErlNifPid pid;
        return enif_make_int(env,
            enif_get_local_pid(env, enif_make_int(env, 1), &pid) ? 0 : 1);
    }
    case 89: {                                 /* get_local_port refuses a non-port */
        ErlNifPort p;
        return enif_make_int(env,
            enif_get_local_port(env, enif_make_atom(env, "a_port"), &p) ? 0 : 1);
    }
    case 90: {                                 /* is_current_process_alive */
        return enif_make_int(env, enif_is_current_process_alive(env) ? 1 : 0);
    }
    case 91: {                                 /* phash2 is salt-independent and < 2^27 */
        ERL_NIF_TERM t = enif_make_int(env, 42);
        ErlNifUInt64 h0 = enif_hash(ERL_NIF_PHASH2, t, 0);
        ErlNifUInt64 h1 = enif_hash(ERL_NIF_PHASH2, t, 987654321);
        return enif_make_int(env,
            (h0 == h1 && h0 < ((ErlNifUInt64) 1 << 27)) ? 1 : 0);
    }

    /* The term-size slot: enif_term_size counts the bytes the term occupies in the term heap. A
       small integer is one word; a cons cell is two, so a 3-element proper list is three cons
       cells = six words. Both are word-size-independent *counts of words* times the word size,
       and the word size is the host ABI's, so the golden is the byte count. */
    case 92: {                                 /* term_size of a small integer */
        return enif_make_int(env, (int) enif_term_size(enif_make_int(env, 7)));
    }
    case 93: {                                 /* term_size of a 3-element list, in words */
        ERL_NIF_TERM arr[3];
        arr[0] = enif_make_int(env, 1);
        arr[1] = enif_make_int(env, 2);
        arr[2] = enif_make_int(env, 3);
        return enif_make_int(env,
            (int) (enif_term_size(enif_make_list_from_array(env, arr, 3))
                   / sizeof(ERL_NIF_TERM)));
    }
    /* comparison / term_type across the type lattice (not just integer-vs-integer) */
    case 94: {                                 /* compare follows the term order across types */
        int n_lt_a = enif_compare(enif_make_int(env, 1), enif_make_atom(env, "an_atom"));
        int a_gt_n = enif_compare(enif_make_atom(env, "an_atom"), enif_make_int(env, 1));
        return enif_make_int(env, (n_lt_a == -1 && a_gt_n == 1) ? 1 : 0);
    }
    case 95: {                                 /* term_type of tuple / map / proper list */
        ERL_NIF_TERM arr[1];
        int tup, mp, lst;
        arr[0] = enif_make_int(env, 1);
        tup = enif_term_type(env, enif_make_tuple_from_array(env, arr, 1))
                  == ERL_NIF_TERM_TYPE_TUPLE;
        mp = enif_term_type(env, enif_make_new_map(env)) == ERL_NIF_TERM_TYPE_MAP;
        lst = enif_term_type(env, enif_make_list_from_array(env, arr, 1))
                  == ERL_NIF_TERM_TYPE_LIST;
        return enif_make_int(env, (tup && mp && lst) ? 1 : 0);
    }

    /* The environment slot: a key no one set is refused by both (the C's -1). */
    case 96: {                                 /* getenv refuses a missing key */
        char buf[8];
        size_t n = sizeof(buf);
        return enif_make_int(env,
            enif_getenv("NIF_ABI_NO_SUCH_ENV_VAR_ZZZ", buf, &n) == -1 ? 1 : 0);
    }
    case 97: {                                 /* consume_timeslice charges without a forced yield */
        return enif_make_int(env, enif_consume_timeslice(env, 10));
    }

    /* ------------------------------------------- the identity / string-width / map edges */
    case 98: {                                 /* is_identical on floats and mixed numbers */
        int same = enif_is_identical(enif_make_double(env, 1.5), enif_make_double(env, 1.5));
        int mixed = enif_is_identical(enif_make_int(env, 1), enif_make_double(env, 1.0));
        return enif_make_int(env, (same && !mixed) ? 1 : 0);
    }
    case 99: {                                 /* UTF-8 string round trip */
        ERL_NIF_TERM t = enif_make_string(env, "hello", ERL_NIF_UTF8);
        char buf[16];
        int r = enif_get_string(env, t, buf, sizeof(buf), ERL_NIF_UTF8);
        return enif_make_int(env, (r == 6 && strcmp(buf, "hello") == 0) ? 1 : 0);
    }
    case 100: {                                /* UTF-8 string length is the encoded byte count */
        const char* s = "abc\xc3\xa5" "d";     /* "abc<U+00E5>d": 5 chars over 6 bytes */
        ERL_NIF_TERM t = enif_make_string(env, s, ERL_NIF_UTF8);
        unsigned n = 0;
        int r = enif_get_string_length(env, t, &n, ERL_NIF_UTF8);
        return enif_make_int(env, r == 1 ? (int) n : -1);
    }
    case 101: {                                /* get_map_size on an empty map */
        size_t sz = 99;
        return enif_make_int(env,
            (enif_get_map_size(env, enif_make_new_map(env), &sz) && sz == 0) ? 1 : 0);
    }
    case 102: {                                /* make_map_update refuses a missing key */
        ERL_NIF_TERM m2;
        int r = enif_make_map_update(env, enif_make_new_map(env), enif_make_atom(env, "k"),
                                     enif_make_int(env, 1), &m2);
        return enif_make_int(env, r ? 0 : 1);
    }
    case 103: {                                /* a LAST map iterator walks back over both entries */
        ERL_NIF_TERM m1, m2, k = 0, v = 0;
        int count = 0;
        ErlNifMapIterator it;
        ERL_NIF_TERM empty = enif_make_new_map(env);
        if (!enif_make_map_put(env, empty, enif_make_int(env, 1), enif_make_int(env, 10), &m1))
            return enif_make_int(env, 0);
        if (!enif_make_map_put(env, m1, enif_make_int(env, 2), enif_make_int(env, 20), &m2))
            return enif_make_int(env, 0);
        if (!enif_map_iterator_create(env, m2, &it, ERL_NIF_MAP_ITERATOR_LAST))
            return enif_make_int(env, 0);
        while (enif_map_iterator_get_pair(env, &it, &k, &v)) {
            count++;
            enif_map_iterator_prev(env, &it);
        }
        enif_map_iterator_destroy(env, &it);
        return enif_make_int(env, count);
    }

    /* The environment slot, positive half: a key both runtimes' environments carry (the very
       `PATH` the harness resolved them through). The probe asserts the documented *contract*
       rather than a length: with a generously sized buffer the copy path answers `0` and reports
       the copied string's own length (`strlen(value)`), and only a `PATH` longer than the buffer
       would make both answer the positive "too small" code with a larger capacity. The value's
       bytes are never compared (they are the host's `PATH`), so the answer is independent of how
       long `PATH` happens to be. */
    case 107: {                                /* getenv finds a key the environment carries */
        char* buf = (char*) enif_alloc(65536);
        size_t n = 65536;
        int r, ok;
        if (buf == NULL)
            return enif_make_int(env, 0);
        r = enif_getenv("PATH", buf, &n);
        ok = (r == 0) ? (strlen(buf) == n)      /* copied: the reported size is the length */
                      : (r == 1 && n > 65536);  /* too small: only if PATH exceeded the buffer */
        enif_free(buf);
        return enif_make_int(env, ok ? 1 : 0);
    }

    /* ------------------------------------------- the term<->binary / ref / registered-name
       success paths (the host hooks a real implementation now serves). Each returns a plain
       integer: the frozen ETF image check, the consumed byte count, the ref-ness / hit booleans. */
    case 108: {                                /* term_to_binary of a valid small integer */
        ErlNifBinary bin;
        int ok;
        if (!enif_term_to_binary(env, enif_make_int(env, 42), &bin))
            return enif_make_int(env, 0);
        /* The canonical image of 42 is <<131,97,42>>: VERSION_MAGIC + SMALL_INTEGER_EXT. */
        ok = (bin.size == 3 && bin.data[0] == 131 && bin.data[1] == 97 && bin.data[2] == 42);
        enif_release_binary(&bin);
        return enif_make_int(env, ok ? 1 : 0);
    }
    case 109: {                                /* binary_to_term reports the consumed byte count */
        static const unsigned char enc[3] = {131, 97, 42};   /* term_to_binary(42) */
        ERL_NIF_TERM t = 0;
        size_t n = enif_binary_to_term(env, enc, sizeof(enc), &t, 0);
        int v = 0;
        if (n == 0 || !enif_get_int(env, t, &v) || v != 42)
            return enif_make_int(env, 0);
        return enif_make_int(env, (int) n);
    }
    case 110: {                                /* make_ref yields a real reference */
        /* Only the *ref-ness* is pinned: the reference id is volatile per VM start. */
        return enif_make_int(env, enif_is_ref(env, enif_make_ref(env)) ? 1 : 0);
    }
    case 111: {                                /* make_existing_atom finds a committed name */
        ERL_NIF_TERM t = 0;
        int r = enif_make_existing_atom(env, "ok", &t, ERL_NIF_LATIN1);
        return enif_make_int(env, (r == 1 && enif_is_atom(env, t)) ? 1 : 0);
    }
    case 112: {                                /* whereis_pid resolves a registered name */
        ErlNifPid pid;
        int r;
        memset(&pid, 0, sizeof(pid));
        /* `init` is registered in both runtimes (the Rust runtime's own registered set is
           `[init, file_server_2]`), so the hit is the pinnable shape. The pid itself is never
           rendered. */
        r = enif_whereis_pid(env, enif_make_atom(env, "init"), &pid);
        return enif_make_int(env, r == 1 ? 1 : 0);
    }

    /* ------------------------------------------- the port / monitor / dynamic-loader / scheduler
       surface. Only the shapes both runtimes answer alike are probed: a name no port is
       registered under (and a non-atom name), a port id that is not a port, a monitor payload the
       byte compare orders equal, a loader/thread/liveness answer that carries no per-VM-start
       value. The exact `enif_thread_type()` value and `enif_demonitor_process(env, NULL, …)` are
       deliberately absent — the first differs between the runtimes, the second crashes the oracle
       (see `divergences.md` §A.3). */
    case 113: {                                /* whereis_port: an unregistered name */
        ErlNifPort p;
        memset(&p, 0, sizeof(p));
        return enif_make_int(env, enif_whereis_port(env, enif_make_atom(env, "no_such_reg_port_zzz"), &p) == 0 ? 1 : 0);
    }
    case 114: {                                /* whereis_port: a non-atom name */
        ErlNifPort p;
        memset(&p, 0, sizeof(p));
        return enif_make_int(env, enif_whereis_port(env, enif_make_int(env, 1), &p) == 0 ? 1 : 0);
    }
    case 115: {                                /* is_port_alive: an id that is not a port */
        ErlNifPort p;
        memset(&p, 0, sizeof(p));
        p.port_id = enif_make_int(env, 7);
        return enif_make_int(env, enif_is_port_alive(env, &p) == 0 ? 1 : 0);
    }
    case 116: {                                /* port_command: a non-port destination */
        ErlNifPort p;
        memset(&p, 0, sizeof(p));
        p.port_id = enif_make_atom(env, "not_a_port");
        return enif_make_int(env, enif_port_command(env, &p, NULL, enif_make_int(env, 1)) == 0 ? 1 : 0);
    }
    case 117: {                                /* schedule_nif: an argc above MAX_ARG (255) */
        /* The C refuses before reading `argv` (`erl_nif.c:3569`); the refused term is the raise
           the NIF returns, so the module observes `error:badarg`. */
        return enif_schedule_nif(env, "probe", 0, probe, 256, NULL);
    }
    case 118: {                                /* schedule_nif: a flag outside the dirty-job pair */
        return enif_schedule_nif(env, "probe", 1 << 10, probe, 1, NULL);
    }
    case 119: {                                /* compare_monitors: two equal payloads */
        ErlNifMonitor a, b;
        memset(&a, 0, sizeof(a));
        memset(&b, 0, sizeof(b));
        return enif_make_int(env, enif_compare_monitors(&a, &b));
    }
    case 120: {                                /* make_monitor_term of a zeroed payload */
        /* Its *ref-ness* is the pinnable shape: the value itself is per-monitor state. */
        ErlNifMonitor m;
        memset(&m, 0, sizeof(m));
        return enif_make_int(env, enif_is_ref(env, enif_make_monitor_term(env, &m)) ? 1 : 0);
    }
    case 122: {                                /* dlopen of a path that does not exist */
        void* h;
        err_calls = 0;
        h = enif_dlopen("/no/such/dir/no_such_nif_lib_zzz", on_err, NULL);
        return enif_make_int(env, (h == NULL && err_calls == 1) ? 1 : 0);
    }
    case 123: {                                /* dlsym with no handle */
        void* s;
        err_calls = 0;
        s = enif_dlsym(NULL, "no_such_symbol_zzz", on_err, NULL);
        return enif_make_int(env, s == NULL ? 1 : 0);
    }
    case 124: {                                /* thread_type: one of the documented kinds */
        int ty = enif_thread_type();
        return enif_make_int(env, (ty == ERL_NIF_THR_UNDEFINED || ty == ERL_NIF_THR_NORMAL_SCHEDULER
            || ty == ERL_NIF_THR_DIRTY_CPU_SCHEDULER || ty == ERL_NIF_THR_DIRTY_IO_SCHEDULER) ? 1 : 0);
    }
    case 126: {                                /* send: a receiver that is not a pid */
        ErlNifPid to;
        memset(&to, 0, sizeof(to));
        to.pid = enif_make_int(env, 3);
        return enif_make_int(env, enif_send(env, &to, NULL, enif_make_int(env, 1)));
    }
    case 127: {                                /* whereis_port on a name registered to a process */
        ErlNifPort p;
        memset(&p, 0, sizeof(p));
        /* `init` names a *process*, so no port is resolved. */
        return enif_make_int(env, enif_whereis_port(env, enif_make_atom(env, "init"), &p));
    }

    default:
        break;
    }
    return enif_make_int(env, -1);
}

static ErlNifFunc nif_abi_funcs[] = {
    {"probe", 1, probe, 0}
};

/* `ERL_NIF_INIT` seeds ErlNifEntry.name with the stringified module name (`nif_abi`), so this
   library may only be loaded as `nif_abi`. */
ERL_NIF_INIT(nif_abi, nif_abi_funcs, load, NULL, NULL, NULL)
"#;

/// The real library every case loads.
const NIF_ABI: NifFixture = NifFixture::CompiledLibrary {
    name: "nif_abi",
    c_source: NIF_ABI_C,
};

/// Build one module-run case over [`NIF_ABI_MODULE_SOURCE`].
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
            source: NIF_ABI_MODULE_SOURCE,
            module: "nif_abi",
            function: "classify",
            arity: 1,
            args: selectors,
            nif: Some(NIF_ABI),
        }),
        golden: Golden {
            stdout: Some(golden),
            exit_code: 0,
        },
    }
}

/// The expanded A.3 group: the `enif_*` term surface, one selector per C ABI behaviour.
pub(crate) fn group() -> Vec<Case> {
    vec![
        // --- get / make accessors: integers -----------------------------------------------
        case("nif_abi/int_make_get_int", "enif_make_int/enif_get_int round trip (-1234)", &[1], "1"),
        case("nif_abi/int_make_get_uint", "enif_make_uint/enif_get_uint round trip", &[2], "1"),
        case("nif_abi/int_make_get_long", "enif_make_long/enif_get_long round trip", &[3], "1"),
        case("nif_abi/int_make_get_ulong", "enif_make_ulong/enif_get_ulong round trip", &[4], "1"),
        case("nif_abi/int_make_get_int64", "enif_make_int64/enif_get_int64 round trip", &[5], "1"),
        case("nif_abi/int_make_get_uint64", "enif_make_uint64/enif_get_uint64 round trip", &[6], "1"),
        case(
            "nif_abi/int_get_int_rejects_out_of_range",
            "enif_get_int refuses a value that does not fit in an int",
            &[7],
            "1",
        ),
        case(
            "nif_abi/int_get_uint_rejects_negative",
            "enif_get_uint refuses a negative integer",
            &[8],
            "1",
        ),
        case(
            "nif_abi/int_get_int_rejects_non_number",
            "enif_get_int refuses a non-number term",
            &[9],
            "1",
        ),
        case(
            "nif_abi/int_wide_int64_roundtrip",
            "a wide int64 (1 << 40) survives make/get",
            &[10],
            "1",
        ),
        // --- get / make accessors: floats --------------------------------------------------
        case("nif_abi/float_make_get_double", "enif_make_double/enif_get_double round trip (3.5)", &[11], "1"),
        case("nif_abi/float_make_get_double_negative", "enif_make_double/get_double round trip (-2.25)", &[12], "1"),
        case(
            "nif_abi/float_get_double_rejects_non_number",
            "enif_get_double refuses a non-number term",
            &[13],
            "1",
        ),
        case(
            "nif_abi/float_get_double_from_integer",
            "enif_get_double on an integer (the C's own accept/reject answer)",
            &[14],
            "0",
        ),
        // --- atoms (names the runtime already holds) --------------------------------------
        case("nif_abi/atom_make_get", "enif_make_atom/enif_get_atom round trip", &[15], "1"),
        case("nif_abi/atom_make_atom_len", "enif_make_atom_len honours the explicit length", &[16], "1"),
        case("nif_abi/atom_get_atom_length", "enif_get_atom_length reports the byte count", &[17], "1"),
        case("nif_abi/atom_make_new_atom", "enif_make_new_atom round trips an existing atom", &[18], "1"),
        case("nif_abi/atom_make_new_atom_len", "enif_make_new_atom_len honours the length", &[19], "1"),
        // --- strings ----------------------------------------------------------------------
        case("nif_abi/string_make_get", "enif_make_string/enif_get_string round trip", &[20], "1"),
        case("nif_abi/string_make_string_len", "enif_make_string_len honours the length", &[21], "1"),
        case("nif_abi/string_get_string_length", "enif_get_string_length reports the char count", &[22], "1"),
        case(
            "nif_abi/string_get_string_short_buffer",
            "enif_get_string with a too-short buffer returns the negative length",
            &[23],
            "1",
        ),
        // --- lists ------------------------------------------------------------------------
        case("nif_abi/list_make_from_array", "enif_make_list_from_array builds a 3-element list", &[24], "1"),
        case("nif_abi/list_make_list_cell", "enif_make_list_cell builds a 2-element list", &[25], "1"),
        case("nif_abi/list_get_list_cell_head", "enif_get_list_cell reads the head (7)", &[26], "7"),
        case("nif_abi/list_make_reverse_list_head", "enif_make_reverse_list reverses [1,2,3] (head 3)", &[27], "3"),
        case(
            "nif_abi/list_get_list_length_rejects_nonlist",
            "enif_get_list_length refuses a non-list",
            &[28],
            "1",
        ),
        // --- tuples -----------------------------------------------------------------------
        case("nif_abi/tuple_make_from_array", "enif_make_tuple_from_array builds a 4-tuple", &[29], "1"),
        case("nif_abi/tuple_get_tuple_arity", "enif_get_tuple reports the arity (4)", &[30], "4"),
        case(
            "nif_abi/tuple_get_tuple_rejects_non_tuple",
            "enif_get_tuple refuses a non-tuple",
            &[31],
            "1",
        ),
        // --- binaries ---------------------------------------------------------------------
        case(
            "nif_abi/binary_alloc_make_inspect",
            "enif_alloc_binary/enif_make_binary/enif_inspect_binary round trip",
            &[32],
            "1",
        ),
        case("nif_abi/binary_make_new_binary", "enif_make_new_binary writes in place", &[33], "1"),
        case("nif_abi/binary_make_sub_binary", "enif_make_sub_binary slices a binary", &[34], "1"),
        case(
            "nif_abi/binary_inspect_rejects_non_binary",
            "enif_inspect_binary refuses a non-binary",
            &[35],
            "1",
        ),
        case(
            "nif_abi/binary_inspect_iolist_as_binary",
            "enif_inspect_iolist_as_binary flattens a nested iolist",
            &[36],
            "1",
        ),
        case("nif_abi/binary_realloc_binary", "enif_realloc_binary grows and preserves the prefix", &[37], "1"),
        // --- predicates / comparison ------------------------------------------------------
        case("nif_abi/pred_is_atom", "enif_is_atom distinguishes an atom from an int", &[38], "1"),
        case("nif_abi/pred_is_number", "enif_is_number accepts int and float, refuses an atom", &[39], "1"),
        case("nif_abi/pred_is_list", "enif_is_list accepts a list and refuses an atom", &[40], "1"),
        case("nif_abi/pred_is_empty_list", "enif_is_empty_list accepts [] and refuses an atom", &[41], "1"),
        case("nif_abi/pred_is_map", "enif_is_map accepts a map and refuses an int", &[42], "1"),
        case("nif_abi/pred_is_tuple", "enif_is_tuple accepts a tuple and refuses an int", &[43], "1"),
        case("nif_abi/pred_is_binary", "enif_is_binary accepts a binary and refuses an int", &[44], "1"),
        case("nif_abi/pred_is_pid_false", "enif_is_pid is false for a non-pid", &[45], "1"),
        case("nif_abi/pred_is_ref_false", "enif_is_ref is false for a non-ref", &[46], "1"),
        case("nif_abi/pred_is_port_false", "enif_is_port is false for a non-port", &[47], "1"),
        case("nif_abi/pred_is_fun_false", "enif_is_fun is false for a non-fun", &[48], "1"),
        case("nif_abi/pred_is_identical", "enif_is_identical holds for equal and differs otherwise", &[49], "1"),
        case("nif_abi/compare_ordering", "enif_compare orders two integers (-1/0/1)", &[50], "1"),
        case("nif_abi/pred_term_type", "enif_term_type reports integer/atom/bitstring", &[51], "1"),
        // --- maps -------------------------------------------------------------------------
        case("nif_abi/map_make_new_put_get", "enif_make_new_map/enif_make_map_put/enif_get_map_value", &[52], "1"),
        case("nif_abi/map_make_map_update", "enif_make_map_update replaces a value", &[53], "1"),
        case("nif_abi/map_make_map_remove", "enif_make_map_remove drops the entry", &[54], "1"),
        case("nif_abi/map_get_map_size", "enif_get_map_size reports 2", &[55], "2"),
        case("nif_abi/map_make_map_from_arrays", "enif_make_map_from_arrays builds a 2-entry map", &[56], "1"),
        case("nif_abi/map_iterator_sums_values", "enif_map_iterator_* walks the map (sum 60)", &[57], "60"),
        // --- the exception slot -----------------------------------------------------------
        case(
            "nif_abi/exception_raise_exception",
            "enif_raise_exception surfaces as a catchable error:boom",
            &[58],
            "80",
        ),
        case(
            "nif_abi/exception_make_badarg",
            "enif_make_badarg surfaces as a catchable error:badarg",
            &[59],
            "81",
        ),
        case(
            "nif_abi/exception_pending_slot",
            "enif_raise_exception/enif_has_pending_exception drive the env exception slot (on enif_alloc_env)",
            &[60],
            "1",
        ),
        // --- process / time / allocation --------------------------------------------------
        case("nif_abi/process_self_is_pid", "enif_self yields a pid term (enif_make_pid)", &[61], "1"),
        case("nif_abi/process_self_is_alive", "enif_is_process_alive on enif_self answers true", &[62], "1"),
        case("nif_abi/process_pid_undefined", "enif_set_pid_undefined/enif_is_pid_undefined round trip", &[63], "1"),
        case("nif_abi/mem_alloc_realloc_free", "enif_alloc/enif_realloc/enif_free round trip", &[64], "1"),
        case("nif_abi/mem_priv_data_is_null", "enif_priv_data is the NULL the load callback set", &[65], "1"),
        case(
            "nif_abi/time_convert_time_unit",
            "enif_convert_time_unit maps 1e9 ns to 1000 ms",
            &[66],
            "1",
        ),
        case(
            "nif_abi/time_now_time_is_tuple",
            "enif_now_time yields the {MegaSecs,Secs,MicroSecs} 3-tuple",
            &[67],
            "1",
        ),
        case(
            "nif_abi/time_cpu_time_is_tuple",
            "enif_cpu_time yields the {MegaSecs,Secs,MicroSecs} 3-tuple",
            &[68],
            "1",
        ),
        case(
            "nif_abi/exception_is_exception_false",
            "enif_is_exception is false for a plain term with no pending raise",
            &[69],
            "1",
        ),
        case(
            "nif_abi/tuple_empty_from_array",
            "the empty tuple and empty list edge through the *_from_array constructors",
            &[70],
            "1",
        ),
        // --- the host-hooked slots: the shapes both runtimes answer alike -----------------
        case(
            "nif_abi/atom_make_existing_atom_miss",
            "enif_make_existing_atom refuses a name the runtime does not hold (and leaves *atom untouched)",
            &[71],
            "1",
        ),
        case(
            "nif_abi/atom_make_existing_atom_len_miss",
            "enif_make_existing_atom_len refuses an unheld name under its explicit length",
            &[72],
            "1",
        ),
        case(
            "nif_abi/process_whereis_pid_unregistered",
            "enif_whereis_pid answers 0 for a name no process is registered under",
            &[73],
            "1",
        ),
        case(
            "nif_abi/process_whereis_pid_rejects_non_atom",
            "enif_whereis_pid answers 0 for a non-atom name",
            &[74],
            "1",
        ),
        case(
            "nif_abi/unique_integer_positive",
            "enif_make_unique_integer([positive]) yields a positive integer",
            &[75],
            "1",
        ),
        case(
            "nif_abi/unique_integer_monotonic_increases",
            "enif_make_unique_integer([monotonic]) strictly increases across two calls",
            &[76],
            "1",
        ),
        case(
            "nif_abi/unique_integer_default_is_negative",
            "enif_make_unique_integer with no properties yields a negative integer",
            &[77],
            "1",
        ),
        case(
            "nif_abi/unique_integer_positive_monotonic",
            "enif_make_unique_integer([positive,monotonic]) yields a positive integer",
            &[78],
            "1",
        ),
        case(
            "nif_abi/system_info_frozen_fields",
            "enif_system_info fills the frozen driver/NIF versions and the erts version",
            &[79],
            "1",
        ),
        case(
            "nif_abi/binary_to_term_rejects_malformed",
            "enif_binary_to_term refuses data that is not a VERSION_MAGIC-prefixed encoding",
            &[80],
            "1",
        ),
        case(
            "nif_abi/exception_clear_env_clean",
            "enif_clear_env on a clean allocated env leaves no pending exception",
            &[81],
            "1",
        ),
        case(
            "nif_abi/atom_fresh_name_identity",
            "a name interned during the call is one atom with one identity",
            &[82],
            "1",
        ),
        // --- the host-hooked slots: the lookup/read success paths -------------------------
        case(
            "nif_abi/atom_make_existing_atom_hit",
            "enif_make_existing_atom finds a name held in this call and yields the same atom",
            &[83],
            "1",
        ),
        case(
            "nif_abi/atom_make_existing_atom_len_hit",
            "enif_make_existing_atom_len finds that name under its explicit length",
            &[84],
            "1",
        ),
        case(
            "nif_abi/atom_get_atom_fresh_name",
            "enif_get_atom reads back a name first interned during the call",
            &[85],
            "1",
        ),
        case(
            "nif_abi/atom_get_atom_length_fresh_name",
            "enif_get_atom_length counts a name first interned during the call",
            &[86],
            "1",
        ),
        // --- the local-id / liveness / hash / term-size family ----------------------------
        case(
            "nif_abi/local_pid_from_self_roundtrip",
            "enif_get_local_pid resolves the pid from enif_self back to the same live process",
            &[87],
            "1",
        ),
        case(
            "nif_abi/local_pid_rejects_non_pid",
            "enif_get_local_pid refuses a non-pid term",
            &[88],
            "1",
        ),
        case(
            "nif_abi/local_port_rejects_non_port",
            "enif_get_local_port refuses a non-port term",
            &[89],
            "1",
        ),
        case(
            "nif_abi/current_process_alive",
            "enif_is_current_process_alive is true for the calling process",
            &[90],
            "1",
        ),
        case(
            "nif_abi/hash_phash2_salt_independent",
            "enif_hash(ERL_NIF_PHASH2, ..) ignores the salt and stays below 2^27",
            &[91],
            "1",
        ),
        case(
            "nif_abi/term_size_small_int",
            "enif_term_size reports no heap words for a small integer",
            &[92],
            "0",
        ),
        case(
            "nif_abi/term_size_three_element_list",
            "enif_term_size reports six words for a 3-element proper list",
            &[93],
            "6",
        ),
        case(
            "nif_abi/compare_across_types",
            "enif_compare orders a number before an atom (the term order)",
            &[94],
            "1",
        ),
        case(
            "nif_abi/term_type_container_terms",
            "enif_term_type reports tuple / map / list for the three container kinds",
            &[95],
            "1",
        ),
        case(
            "nif_abi/env_getenv_missing_key",
            "enif_getenv refuses a key no one set",
            &[96],
            "1",
        ),
        case(
            "nif_abi/sched_consume_timeslice",
            "enif_consume_timeslice charges a slice without forcing a yield",
            &[97],
            "0",
        ),
        // --- the identity / string-width / map edges ---------------------------------------
        case(
            "nif_abi/identity_float_vs_mixed_number",
            "enif_is_identical holds for two equal floats and differs for 1 vs 1.0",
            &[98],
            "1",
        ),
        case(
            "nif_abi/string_utf8_roundtrip",
            "enif_make_string/enif_get_string round trip under ERL_NIF_UTF8",
            &[99],
            "1",
        ),
        case(
            "nif_abi/string_utf8_length_counts_encoded_bytes",
            "enif_get_string_length under ERL_NIF_UTF8 reports the encoded byte count (6)",
            &[100],
            "6",
        ),
        case(
            "nif_abi/map_get_map_size_empty",
            "enif_get_map_size reports 0 for an empty map",
            &[101],
            "1",
        ),
        case(
            "nif_abi/map_make_map_update_missing_key",
            "enif_make_map_update refuses a key the map does not hold",
            &[102],
            "1",
        ),
        case(
            "nif_abi/map_iterator_last_walks_all",
            "a ERL_NIF_MAP_ITERATOR_LAST iterator walks back over both entries",
            &[103],
            "2",
        ),
        case(
            "nif_abi/env_getenv_present_key",
            "enif_getenv finds a key the environment carries and reports the copied string's own length",
            &[107],
            "1",
        ),
        // --- the now-implemented host hooks: the success paths ----------------------------
        case(
            "nif_abi/term_to_binary_valid_term",
            "enif_term_to_binary encodes a valid term to its canonical ETF image (<<131,97,42>>)",
            &[108],
            "1",
        ),
        case(
            "nif_abi/binary_to_term_valid_encoding",
            "enif_binary_to_term decodes a valid encoding and reports the consumed byte count (3)",
            &[109],
            "3",
        ),
        case(
            "nif_abi/ref_make_ref_is_a_ref",
            "enif_make_ref yields a real reference (only its ref-ness is asserted)",
            &[110],
            "1",
        ),
        case(
            "nif_abi/atom_make_existing_atom_committed_hit",
            "enif_make_existing_atom finds a name held in the committed atom table (\"ok\")",
            &[111],
            "1",
        ),
        case(
            "nif_abi/process_whereis_pid_registered_hit",
            "enif_whereis_pid resolves a name a live process is registered under (\"init\")",
            &[112],
            "1",
        ),
        // --- the port / monitor / dynamic-loader / scheduler family ------------------------
        case(
            "nif_abi/port_whereis_port_unregistered",
            "enif_whereis_port answers 0 for a name no port is registered under",
            &[113],
            "1",
        ),
        case(
            "nif_abi/port_whereis_port_rejects_non_atom",
            "enif_whereis_port answers 0 for a non-atom name",
            &[114],
            "1",
        ),
        case(
            "nif_abi/port_is_port_alive_rejects_non_port",
            "enif_is_port_alive answers 0 for an id that is not a port",
            &[115],
            "1",
        ),
        case(
            "nif_abi/port_command_rejects_non_port",
            "enif_port_command answers 0 for a non-port destination",
            &[116],
            "1",
        ),
        case(
            "nif_abi/schedule_nif_rejects_argc_above_max",
            "enif_schedule_nif raises badarg for an argc above MAX_ARG (255)",
            &[117],
            "81",
        ),
        case(
            "nif_abi/schedule_nif_rejects_unknown_flag",
            "enif_schedule_nif raises badarg for a flag bit outside the dirty-job pair",
            &[118],
            "81",
        ),
        case(
            "nif_abi/monitor_compare_equal_payloads",
            "enif_compare_monitors orders two equal monitor payloads as 0",
            &[119],
            "0",
        ),
        case(
            "nif_abi/monitor_make_monitor_term_zeroed_not_ref",
            "enif_make_monitor_term over a zeroed payload yields a non-reference",
            &[120],
            "0",
        ),
        case(
            "nif_abi/dlopen_missing_library_reports_error",
            "enif_dlopen of a missing path yields no handle and calls the error handler once",
            &[122],
            "1",
        ),
        case(
            "nif_abi/dlsym_without_handle_is_null",
            "enif_dlsym with no handle yields no symbol",
            &[123],
            "1",
        ),
        case(
            "nif_abi/thread_type_in_documented_set",
            "enif_thread_type reports one of the four documented thread kinds",
            &[124],
            "1",
        ),
        case(
            "nif_abi/send_rejects_non_pid_receiver",
            "enif_send answers 0 for a receiver that is not a pid",
            &[126],
            "0",
        ),
        case(
            "nif_abi/port_whereis_port_on_process_name",
            "enif_whereis_port answers 0 for a name registered to a process, not a port",
            &[127],
            "0",
        ),
    ]
}
#[cfg(test)]
mod tests {
    //! Invariants of the expanded A.3 group: every case is a module-run case claiming
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
            "the expanded A.3 group must carry a broad selector set (got {})",
            cases.len()
        );
        for case in cases {
            assert!(
                case.id.starts_with("nif_abi/"),
                "case {} is outside the nif_abi group",
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
            assert_eq!(run.module, "nif_abi", "case {}", case.id);
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
                matches!(run.nif, Some(NifFixture::CompiledLibrary { name: "nif_abi", .. })),
                "case {}: the library must be the shared C-compiled nif_abi fixture",
                case.id
            );
            assert!(
                case.golden
                    .stdout
                    .expect("every case pins a golden")
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
    fn the_c_library_covers_the_get_make_predicate_and_map_families() {
        // The families this group exists to add (each symbol must really be referenced, or the
        // case does not exercise it).
        for symbol in [
            "enif_get_int",
            "enif_make_int",
            "enif_get_double",
            "enif_make_double",
            "enif_make_atom",
            "enif_make_atom_len",
            "enif_get_atom",
            "enif_get_atom_length",
            "enif_make_new_atom",
            "enif_make_new_atom_len",
            "enif_get_string",
            "enif_make_string_len",
            "enif_make_list_from_array",
            "enif_make_reverse_list",
            "enif_make_tuple_from_array",
            "enif_get_tuple",
            "enif_alloc_binary",
            "enif_make_sub_binary",
            "enif_inspect_iolist_as_binary",
            "enif_is_identical",
            "enif_compare",
            "enif_term_type",
            "enif_make_new_map",
            "enif_make_map_put",
            "enif_make_map_update",
            "enif_make_map_remove",
            "enif_make_map_from_arrays",
            "enif_get_map_value",
            "enif_map_iterator_create",
            "enif_raise_exception",
            "enif_has_pending_exception",
            "enif_alloc_env",
            "enif_free_env",
            "enif_is_process_alive",
            "enif_now_time",
            "enif_cpu_time",
            "enif_convert_time_unit",
        ] {
            assert!(
                super::NIF_ABI_C.contains(symbol),
                "the library must reference {symbol}"
            );
        }
    }

    #[test]
    fn the_c_library_covers_the_host_hooked_slots() {
        // The slots whose host hook the runtime serves through `NifHost`: both the failure shapes
        // and the now-implemented success paths are referenced (see the module docs).
        for symbol in [
            "enif_make_existing_atom(",
            "enif_make_existing_atom_len(",
            "enif_whereis_pid(",
            "enif_make_unique_integer(",
            "enif_system_info(",
            "enif_binary_to_term(",
            "enif_term_to_binary(",
            "enif_make_ref(",
            "enif_clear_env(",
            "enif_has_pending_exception(",
            "enif_alloc_env(",
            "enif_free_env(",
        ] {
            assert!(
                super::NIF_ABI_C.contains(symbol),
                "the library must reference {symbol}"
            );
        }
        // The success paths the runtime now serves through the `NifHost` hooks each really drive a
        // call site (a valid image, a minted ref, a registered-name hit) — pinned, never a gap.
        for needle in [
            "131, 97, 42",
            "enif_whereis_pid(env, enif_make_atom(env, \"init\")",
        ] {
            assert!(
                super::NIF_ABI_C.contains(needle),
                "the library must reference {needle} (a pinned success shape)"
            );
        }
    }

    #[test]
    fn the_atom_lookup_success_paths_are_pinned() {
        // The *success* half of the lookup-only constructor and of the two atom read accessors: a
        // name first interned during the call is found / read back by both runtimes (the exact
        // rule is in the module docs). Each is a real corpus case — the selector really drives the
        // branch, and the golden is the oracle's own integer for it.
        let cases = group();
        for (id, selector) in [
            ("nif_abi/atom_make_existing_atom_hit", 83i64),
            ("nif_abi/atom_make_existing_atom_len_hit", 84),
            ("nif_abi/atom_get_atom_fresh_name", 85),
            ("nif_abi/atom_get_atom_length_fresh_name", 86),
        ] {
            let case = cases
                .iter()
                .find(|case| case.id == id)
                .unwrap_or_else(|| panic!("the group must carry {id}"));
            let CaseKind::Module(run) = case.kind else {
                panic!("{id}: an A.3 case must be a module-run case");
            };
            assert_eq!(run.args, &[selector][..], "case {id}");
            assert_eq!(case.golden.stdout, Some("1"), "case {id}");
            assert_eq!(case.golden.exit_code, 0, "case {id}");
        }
    }

    #[test]
    fn the_c_library_covers_the_local_id_liveness_hash_and_size_families() {
        // The families the second block adds (each symbol must really be referenced).
        for symbol in [
            "enif_get_local_pid(",
            "enif_get_local_port(",
            "enif_is_current_process_alive(",
            "enif_hash(",
            "ERL_NIF_PHASH2",
            "enif_term_size(",
            "enif_getenv(",
            "enif_consume_timeslice(",
            "enif_map_iterator_prev(",
            "ERL_NIF_MAP_ITERATOR_LAST",
        ] {
            assert!(
                super::NIF_ABI_C.contains(symbol),
                "the library must reference {symbol}"
            );
        }
        // The internal-hash *value* and the atom-cache slots are implementation-defined and differ
        // between the two runtimes, so no call site may pin them (a guaranteed mismatch):
        for banned in [
            "ERL_NIF_INTERNAL_HASH",
            "enif_max_atom_cache_index(",
            "enif_get_atom_cache_index(",
        ] {
            assert!(
                !super::NIF_ABI_C.contains(banned),
                "the library must not reference {banned} (a guaranteed mismatch)"
            );
        }
    }

    #[test]
    fn the_c_library_references_no_variadic_symbol() {
        // A C-variadic slot (`enif_make_tuple`, `enif_make_list`, `enif_fprintf`, …) must never
        // be referenced: both runtimes `dlopen(RTLD_NOW)` the same `.so`, so an unresolved slot
        // would fail the Rust load where the oracle succeeds (a mismatch, not a gap).
        for banned in ["enif_make_tuple(", "enif_make_list(", "enif_fprintf", "enif_set_option"] {
            assert!(
                !super::NIF_ABI_C.contains(banned),
                "the library must not reference the variadic {banned}"
            );
        }
    }

    #[test]
    fn the_c_library_covers_the_port_monitor_loader_and_scheduler_families() {
        // The families the third block adds (each symbol must really be referenced).
        for symbol in [
            "enif_whereis_port(",
            "enif_is_port_alive(",
            "enif_port_command(",
            "enif_schedule_nif(",
            "enif_compare_monitors(",
            "enif_make_monitor_term(",
            "enif_dlopen(",
            "enif_dlsym(",
            "enif_thread_type(",
            "enif_send(",
        ] {
            assert!(
                super::NIF_ABI_C.contains(symbol),
                "the library must reference {symbol}"
            );
        }
        // Two probes are deliberately absent: the *exact* `enif_thread_type()` value and
        // `enif_demonitor_process(env, NULL, …)` both differ from the oracle — the first returns a
        // different kind, the second crashes the oracle VM (see `divergences.md` §A.3). Pinning
        // either would be a mismatch, so no call site may exist.
        assert!(
            !super::NIF_ABI_C.contains("enif_demonitor_process("),
            "the library must not call enif_demonitor_process (a probe hazard)"
        );
    }

    #[test]
    fn the_module_exports_and_calls_the_probe() {
        for needle in ["-export([classify/1, probe/1])", "nif_abi:probe(N)", "erlang:load_nif(Dir ++ \"/nif_abi\", 0)"] {
            assert!(
                super::NIF_ABI_MODULE_SOURCE.contains(needle),
                "the module must contain {needle:?}"
            );
        }
    }
}
