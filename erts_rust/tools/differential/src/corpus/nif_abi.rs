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
//! The families whose host hook the runtime may leave at its trait default (`NifHost` in
//! `beam_abi`, implemented by `beam_rt`'s `NifHostImpl`) are covered **only** by the shapes both
//! runtimes answer alike, because a *plain differing value* is a mismatch and a mismatch is
//! forbidden here:
//!
//! * pinned — `enif_make_existing_atom` / `_len` **refusing** a name no one holds (both answer
//!   `0` and leave `*atom` untouched, `atom.c:431`); `enif_whereis_pid` on an unregistered atom
//!   and on a non-atom name (both `0`); `enif_binary_to_term` **refusing** data that is not a
//!   `VERSION_MAGIC`-prefixed encoding (both `0`); `enif_clear_env` on a clean `enif_alloc_env`
//!   env leaving no pending exception (both `0`); the **properties** of `enif_make_unique_integer`
//!   (positive / monotonic / default-negative — the crate-internal counters are real, distinct
//!   and monotone per flag set); the frozen fields of `enif_system_info` (the driver/NIF version
//!   macros it was compiled with, `thread_support`, `smp_support`, the `"17.1"` erts version);
//!   and a name first interned *during* the call still being one atom with one identity.
//! * **not** pinned (a real, documented divergence — recorded as a gap, never as a corpus case) —
//!   `enif_term_to_binary` / `enif_binary_to_term` on a **valid** term (`0` where the oracle
//!   answers `1`: no `term_to_binary`/`binary_to_term` host hook); `enif_make_ref` (the Rust
//!   runtime raises `badarg` where the oracle yields a ref: no `make_ref` host hook);
//!   `enif_make_existing_atom`/`enif_whereis_pid` on a name that **is** held/registered (those
//!   hooks answer `None` unconditionally); and an `enif_make_atom` → `enif_get_atom` round trip
//!   on a brand-new name (the host's `atom_name` consults the **committed** atom table only, so a
//!   name interned during this call is not yet resolvable). Owning crates:
//!   `crates/beam_rt/src/nif.rs` (`NifHostImpl`) + `crates/beam_abi/src/nif/`. `enif_system_info`'s
//!   `otp_release` / `async_threads` / `scheduler_threads` / `dirty_scheduler_support` values are
//!   likewise documented to differ and are never asserted.
//!
//! The time family is exercised only through the deterministic `enif_convert_time_unit`
//! (`enif_monotonic_time`'s absolute value is documented to differ) and through the **shape** of
//! `enif_now_time` / `enif_cpu_time` (their `{MegaSecs,Secs,MicroSecs}` 3-tuple, whose values are
//! clock readings and are therefore never asserted).
//!
//! The **atom** read accessors are exercised over a name the runtime already holds (`ok`) and over
//! a name first interned during the call (identity only). The exception slot is driven on an
//! **`enif_alloc_env` env** because `enif_raise_exception` on a process env leaks into the caller;
//! `enif_clear_env` follows the same rule (on a clean allocated env it is a no-op in both — the
//! oracle leaves the pending flag set where the Rust runtime clears it, so the *raisable* shape is
//! the one that stays out).
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
       The families below are the ones whose `NifHost` hook the runtime may leave at its trait
       default. Each branch pins **one** shape whose answer is the same under both runtimes a
       real C ABI contract fixes: a miss both refuse, a malformed input both refuse, a clean env
       both leave clean, or a *property* of a value the crate-internal implementation really
       produces. A shape that would answer differently is not in this group (see the module
       docs for the ones that are not). */
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

    /* Term <-> binary: only the *refusal* shapes are pinned; the success paths are a documented
       host-hook gap (see the module docs). */
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
        // The slots whose host hook the runtime may leave at its trait default: only the shapes
        // both runtimes answer alike are referenced (see the module docs).
        for symbol in [
            "enif_make_existing_atom(",
            "enif_make_existing_atom_len(",
            "enif_whereis_pid(",
            "enif_make_unique_integer(",
            "enif_system_info(",
            "enif_binary_to_term(",
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
        // A call site for one of these would be a plain differing value (a mismatch), so the
        // library must never carry one:
        //   * enif_make_ref          -> the Rust runtime raises badarg (no host `make_ref`);
        //   * enif_term_to_binary    -> the Rust runtime answers 0 (no host `term_to_binary`);
        //   * a *valid* enif_binary_to_term input is exercised nowhere (no host hook either).
        for absent in ["enif_make_ref(", "enif_term_to_binary("] {
            assert!(
                !super::NIF_ABI_C.contains(absent),
                "the library must not reference {absent} (a guaranteed mismatch)"
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
    fn the_module_exports_and_calls_the_probe() {
        for needle in ["-export([classify/1, probe/1])", "nif_abi:probe(N)", "erlang:load_nif(Dir ++ \"/nif_abi\", 0)"] {
            assert!(
                super::NIF_ABI_MODULE_SOURCE.contains(needle),
                "the module must contain {needle:?}"
            );
        }
    }
}
